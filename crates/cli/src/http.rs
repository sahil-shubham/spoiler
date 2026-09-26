//! One bounded HTTP client for every upstream. No retries here: the caller's scheduler owns
//! retry policy and aggregate rate admission across invocations; failures say whether retrying
//! can help.

use anyhow::{Context, Result, ensure};
use reqwest::blocking::Client;
use serde_json::Value;
use std::{fmt, io::Read, time::Duration};

/// Independent cap for HogQL listings and model JSON responses.
const MAX_JSON_RESPONSE_BYTES: u64 = 32 << 20;

/// An upstream answered with an error status.
#[derive(Debug)]
pub struct UpstreamError {
    pub status: u16,
    pub retry_after: Option<String>,
}

impl UpstreamError {
    /// Throttling and server errors are transient.
    pub fn is_retryable(&self) -> bool {
        self.status == 429 || self.status >= 500
    }
}

impl fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "upstream HTTP {}", self.status)?;
        if let Some(retry_after) = &self.retry_after {
            write!(f, " (retry after {retry_after})")?;
        }
        Ok(())
    }
}

impl std::error::Error for UpstreamError {}

pub struct Http {
    client: Client,
}

impl Http {
    pub fn new(timeout_seconds: u64) -> Result<Self> {
        ensure!(timeout_seconds > 0, "timeout must be positive");
        let client = Client::builder()
            .timeout(Duration::from_secs(timeout_seconds))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { client })
    }

    pub fn get(&self, url: &str, token: &str) -> Result<String> {
        self.send(url, token, None, MAX_JSON_RESPONSE_BYTES)
    }

    pub fn get_limited(&self, url: &str, token: &str, max_bytes: u64) -> Result<String> {
        self.send(url, token, None, max_bytes)
    }

    pub fn post_json(&self, url: &str, token: &str, body: &Value) -> Result<String> {
        self.send(url, token, Some(body), MAX_JSON_RESPONSE_BYTES)
    }

    fn send(&self, url: &str, token: &str, body: Option<&Value>, max_bytes: u64) -> Result<String> {
        require_secure(url)?;
        let request = match body {
            Some(body) => self.client.post(url).json(body),
            None => self.client.get(url),
        };
        let response = request
            .bearer_auth(token)
            .header("X-Title", "spoiler")
            .send()
            .context("upstream request failed")?;
        let status = response.status();
        if !status.is_success() {
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            return Err(UpstreamError {
                status: status.as_u16(),
                retry_after,
            }
            .into());
        }
        read_limited(response, max_bytes).context("reading upstream response")
    }
}

/// Read at most one byte beyond the cap, so the limit is checked before retaining a full body.
fn read_limited(reader: impl Read, max_bytes: u64) -> Result<String> {
    let mut bytes = Vec::new();
    reader
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    ensure!(
        (bytes.len() as u64) <= max_bytes,
        "upstream response exceeds {max_bytes} bytes"
    );
    String::from_utf8(bytes).context("upstream response is not UTF-8")
}

/// Credentials only travel over HTTPS; plain HTTP is allowed for loopback test servers.
fn require_secure(url: &str) -> Result<()> {
    let url = url::Url::parse(url).with_context(|| format!("invalid URL {url}"))?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && loopback),
        "refusing to send credentials over {}: HTTPS is required",
        url.scheme()
    );
    Ok(())
}

/// A credential from the environment. Never read from arguments, which leak into process lists.
pub fn credential(variable: &str) -> Result<String> {
    std::env::var(variable)
        .ok()
        .filter(|value| !value.is_empty())
        .with_context(|| format!("set {variable}"))
}

/// Whether an error chain says the failure is transient: an upstream 429/5xx, or no response.
pub fn is_retryable(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        if let Some(upstream) = cause.downcast_ref::<UpstreamError>() {
            return upstream.is_retryable();
        }
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|e| e.is_timeout() || e.is_connect() || e.is_request())
    })
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{Http, is_retryable, read_limited};
    use std::{
        io::{Cursor, Read, Write},
        net::TcpListener,
    };

    struct CountingReader {
        inner: Cursor<Vec<u8>>,
        bytes_read: usize,
    }

    impl Read for CountingReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buffer)?;
            self.bytes_read += n;
            Ok(n)
        }
    }

    #[test]
    fn response_limit_checks_before_reading_remainder() {
        let mut reader = CountingReader {
            inner: Cursor::new(vec![b'x'; 4096]),
            bytes_read: 0,
        };
        assert!(read_limited(&mut reader, 10).is_err());
        assert_eq!(reader.bytes_read, 11);
        assert_eq!(
            read_limited(Cursor::new(b"ten bytes!".to_vec()), 10).unwrap(),
            "ten bytes!"
        );
    }

    #[test]
    fn oversized_http_body_is_not_retryable() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Read the whole request first: answering and closing mid-request resets the
            // connection, which is a (retryable) transport error instead of an oversized body.
            let mut request = Vec::new();
            let mut byte = [0; 1];
            while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
                request.push(byte[0]);
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\n\r\nhello world!")
                .unwrap();
        });
        let error = Http::new(5)
            .unwrap()
            .get_limited(&format!("http://{address}/"), "token", 5)
            .unwrap_err();
        server.join().unwrap();
        assert!(!is_retryable(&error));
    }
}
