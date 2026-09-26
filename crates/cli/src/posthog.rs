//! PostHog: discovering recordings (HogQL) and fetching their snapshots (`blob_v2`).

use crate::http::{Http, UpstreamError, credential};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use spoiler_core::{
    artifact::{Header, Kind, RecordingArtifact, RecordingSource},
    recording::{DecodeError, Limits, Recording},
};
use std::time::{Duration, SystemTime};

const TOKEN: &str = "POSTHOG_API_KEY";
/// The snapshot API allows at most 20 blob keys in each inclusive range.
const BLOB_KEYS_PER_REQUEST: usize = 20;
/// Per-fetch safety cap, including the source listing, not a rate-limit scheduler. Snapshot
/// throttles apply to both listing and ranges: free 12/min 60/h, paid 60/min 300/h,
/// enterprise 100/min 400/h per personal key, across concurrent fetches.
pub const MAX_SNAPSHOT_REQUESTS: usize = 59;
pub const MAX_PAGE_SIZE: usize = 1000;
/// Bound outlier ID probes to avoid another full retention scan. A recording with
/// segments farther outside the window can still appear complete incorrectly.
const OUTLIER_LOOKAROUND_DAYS: u8 = 7;

/// Which recordings discovery lists: one project on one host, within one time window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub host: String,
    pub project: u64,
    pub since: String,
    pub until: String,
}

/// A window as given on the command line, where a cursor may supply the rest.
pub struct PartialWindow {
    pub host: Option<String>,
    pub project: Option<u64>,
    pub since: Option<String>,
    pub until: Option<String>,
}

impl PartialWindow {
    /// Every given part equals the window's.
    pub fn agrees_with(&self, window: &Window) -> bool {
        self.host.as_ref().is_none_or(|host| *host == window.host)
            && self.project.is_none_or(|project| project == window.project)
            && self
                .since
                .as_ref()
                .is_none_or(|since| *since == window.since)
            && self
                .until
                .as_ref()
                .is_none_or(|until| *until == window.until)
    }

    pub fn complete(self, default_host: &str) -> Result<Window> {
        Ok(Window {
            host: self.host.unwrap_or_else(|| default_host.to_owned()),
            project: self
                .project
                .context("--project is required without --cursor")?,
            since: self.since.context("--since is required without --cursor")?,
            until: self.until.context("--until is required without --cursor")?,
        })
    }
}

/// The last recording of a page: the next page starts after it in (start time, id) order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub start_time: String,
    pub session_id: String,
}

/// Where to resume discovery, as an opaque token: a version prefix and base64url JSON.
///
/// Callers store and pass back the token; its contents follow this query's pagination and may
/// change with it, which the version prefix makes detectable. It is not secret or signed: an
/// edited token can only select a different window, which is checked against the flags given.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cursor {
    pub window: Window,
    pub after: Position,
}

const CURSOR_PREFIX: &str = "v1.";

impl Cursor {
    pub fn encode(&self) -> Result<String> {
        Ok(format!(
            "{CURSOR_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(self)?)
        ))
    }

    pub fn decode(token: &str) -> Result<Self> {
        let payload = token
            .strip_prefix(CURSOR_PREFIX)
            .context("not a spoiler v1 cursor")?;
        let json = URL_SAFE_NO_PAD
            .decode(payload)
            .context("the cursor is not valid base64url")?;
        serde_json::from_slice(&json).context("the cursor's contents are malformed")
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RecordingPage {
    #[serde(flatten)]
    pub header: Header,
    /// One object per recording, keyed by HogQL column.
    pub recordings: Vec<Map<String, Value>>,
    /// A [`Cursor`] token for the next page; absent when this page is the last in the window.
    pub next_cursor: Option<String>,
}

pub struct Discovery {
    pub window: Window,
    pub limit: usize,
}

/// Bound every caller-supplied value rather than interpolating it into HogQL.
fn query_body(query: &Discovery, after: Option<&Position>) -> Value {
    let mut values = json!({
        "since": query.window.since,
        "until": query.window.until,
    });
    let cursor = if let Some(after) = after {
        values["cursor_start"] = json!(after.start_time);
        values["cursor_id"] = json!(after.session_id);
        " AND (start_time > parseDateTimeBestEffort({cursor_start}) \
          OR (start_time = parseDateTimeBestEffort({cursor_start}) AND session_id > {cursor_id}))"
    } else {
        ""
    };
    // Normal PostHog sessions stop at 24h, but live session IDs can persist for days.
    // Probe a bounded look-around for disqualifying segments outside the aggregate.
    let hogql = format!(
        "SELECT session_id, any(distinct_id) AS distinct_id, \
         min(min_first_timestamp) AS start_time, max(max_last_timestamp) AS end_time, \
         sum(active_milliseconds) / 1000 AS active_s, sum(click_count) AS clicks, \
         sum(keypress_count) AS keypresses, sum(console_error_count) AS console_errors, \
         argMinMerge(first_url) AS first_url, sum(size) AS bytes, \
         argMinMerge(snapshot_source) AS snapshot_source, \
         argMinMerge(snapshot_library) AS snapshot_library, \
         any(retention_period_days) AS retention_period_days \
         FROM raw_session_replay_events \
         WHERE min_first_timestamp >= parseDateTimeBestEffort({{since}}) - INTERVAL 24 HOUR \
         AND min_first_timestamp <= parseDateTimeBestEffort({{until}}) + INTERVAL 24 HOUR \
         GROUP BY session_id \
         HAVING start_time >= parseDateTimeBestEffort({{since}}) \
         AND end_time <= parseDateTimeBestEffort({{until}}) \
         AND start_time < now() - INTERVAL 24 HOUR \
         AND max(is_deleted) = 0 \
         AND session_id NOT IN (SELECT session_id FROM raw_session_replay_events \
             WHERE (min_first_timestamp >= parseDateTimeBestEffort({{since}}) - INTERVAL {lookaround} DAY \
                 AND min_first_timestamp < parseDateTimeBestEffort({{since}}) - INTERVAL 24 HOUR) \
             OR (min_first_timestamp > parseDateTimeBestEffort({{until}}) + INTERVAL 24 HOUR \
                 AND min_first_timestamp <= parseDateTimeBestEffort({{until}}) + INTERVAL {lookaround} DAY)){cursor} \
         ORDER BY start_time ASC, session_id ASC \
         LIMIT {limit}",
        limit = query.limit + 1,
        lookaround = OUTLIER_LOOKAROUND_DAYS,
    );
    json!({
        "name": "spoiler recordings list",
        "query": { "kind": "HogQLQuery", "query": hogql, "values": values },
    })
}

/// Recordings that started at or after `since` and ended by `until`, oldest first.
///
/// Every product view counts: there is no host, route or activity filter. Keyset pagination
/// over (start time, id) keeps pages stable while newer recordings arrive. Recordings still
/// ingesting past `until` are excluded; choose a settled window and overlap successive syncs.
pub fn list(http: &Http, query: &Discovery, after: Option<&Position>) -> Result<RecordingPage> {
    ensure!(
        (1..=MAX_PAGE_SIZE).contains(&query.limit),
        "limit must be between 1 and {MAX_PAGE_SIZE}"
    );
    let window = &query.window;
    let url = format!(
        "{}/api/projects/{}/query/",
        window.host.trim_end_matches('/'),
        window.project
    );
    let body = query_body(query, after);
    let response: Value =
        serde_json::from_str(&http.post_json(&url, &credential(TOKEN)?, &body)?)
            .context("query response is not JSON")?;

    let mut recordings = rows(&response)?;
    let has_more = recordings.len() > query.limit;
    recordings.truncate(query.limit);
    let next_cursor = match recordings.last() {
        Some(last) if has_more => Some(
            Cursor {
                window: window.clone(),
                after: Position {
                    start_time: last["start_time"]
                        .as_str()
                        .context("row has no start_time")?
                        .to_owned(),
                    session_id: last["session_id"]
                        .as_str()
                        .context("row has no session_id")?
                        .to_owned(),
                },
            }
            .encode()?,
        ),
        _ => None,
    };
    Ok(RecordingPage {
        header: Header::new(Kind::RecordingPage),
        recordings,
        next_cursor,
    })
}

/// HogQL results as one object per row.
fn rows(response: &Value) -> Result<Vec<Map<String, Value>>> {
    let columns: Vec<&str> = response["columns"]
        .as_array()
        .context("query response has no columns")?
        .iter()
        .map(|c| c.as_str().context("query column is not a string"))
        .collect::<Result<_>>()?;
    let results = response["results"]
        .as_array()
        .context("query response has no results")?;
    results
        .iter()
        .map(|row| {
            let cells = row.as_array().context("query row is not an array")?;
            ensure!(
                cells.len() == columns.len(),
                "query row width does not match its columns"
            );
            Ok(columns
                .iter()
                .map(|c| (*c).to_owned())
                .zip(cells.iter().cloned())
                .collect())
        })
        .collect()
}

pub struct Snapshot<'a> {
    pub host: &'a str,
    pub project: u64,
    pub session: &'a str,
    pub max_requests: usize,
    pub max_wait: u64,
    pub limits: Limits,
}

/// Download a recording. Fails before downloading bodies if the listing plus ranges need more
/// than `max_requests`. Retries after 429 repeat only the failed request.
pub fn fetch(http: &Http, snapshot: &Snapshot<'_>) -> Result<RecordingArtifact> {
    let token = credential(TOKEN)?;
    fetch_with_token(http, snapshot, &token)
}

fn fetch_with_token(
    http: &Http,
    snapshot: &Snapshot<'_>,
    token: &str,
) -> Result<RecordingArtifact> {
    ensure!(
        (1..=MAX_SNAPSHOT_REQUESTS).contains(&snapshot.max_requests),
        "max requests must be between 1 and {MAX_SNAPSHOT_REQUESTS}"
    );
    let valid_id = !snapshot.session.is_empty()
        && snapshot
            .session
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    ensure!(valid_id, "invalid recording id");
    let mut waited = Duration::ZERO;
    let base = format!(
        "{}/api/environments/{}/session_recordings/{}/snapshots",
        snapshot.host.trim_end_matches('/'),
        snapshot.project,
        snapshot.session
    );

    let mut listing_url = url::Url::parse(&base)?;
    listing_url.query_pairs_mut().append_pair("blob_v2", "true");
    let listing: Value =
        serde_json::from_str(&get_with_retry(snapshot.max_wait, &mut waited, || {
            http.get(listing_url.as_str(), token)
        })?)
        .context("snapshot source listing is not JSON")?;
    let mut blob_keys = Vec::new();
    for source in listing["sources"]
        .as_array()
        .context("no snapshot sources")?
    {
        ensure!(
            source["source"] == "blob_v2",
            "unsupported snapshot source {}",
            source["source"]
        );
        let key = source["blob_key"]
            .as_str()
            .context("snapshot source has no blob key")?;
        blob_keys.push(key.to_owned());
    }
    let requests = 1 + blob_keys.len().div_ceil(BLOB_KEYS_PER_REQUEST);
    ensure!(
        requests <= snapshot.max_requests,
        "recording needs {requests} snapshot requests (listing and ranges); --max-requests allows {}",
        snapshot.max_requests
    );

    let mut bodies = Vec::with_capacity(requests - 1);
    let mut downloaded = 0_u64;
    let mut wire_downloaded = 0_u64;
    for chunk in blob_keys.chunks(BLOB_KEYS_PER_REQUEST) {
        let (Some(first), Some(last)) = (chunk.first(), chunk.last()) else {
            continue;
        };
        let mut url = url::Url::parse(&base)?;
        url.query_pairs_mut()
            .append_pair("source", "blob_v2")
            .append_pair("start_blob_key", first)
            .append_pair("end_blob_key", last)
            .append_pair("decompress", "false");
        let separator = u64::from(!bodies.is_empty());
        let remaining = snapshot
            .limits
            .max_bytes
            .checked_sub(downloaded)
            .and_then(|bytes| bytes.checked_sub(separator))
            .ok_or(DecodeError::TooLarge {
                max_bytes: snapshot.limits.max_bytes,
            })?;
        let wire_remaining = snapshot
            .limits
            .max_bytes
            .checked_sub(wire_downloaded)
            .ok_or(DecodeError::TooLarge {
                max_bytes: snapshot.limits.max_bytes,
            })?;
        let wire = get_with_retry(snapshot.max_wait, &mut waited, || {
            http.get_bytes_limited(url.as_str(), token, wire_remaining)
        })?;
        wire_downloaded += wire.len() as u64;
        let body = decode_snappy_blocks(&wire, remaining, snapshot.limits.max_bytes)?;
        downloaded += separator + body.len() as u64;
        bodies.push(body);
    }
    let events = Recording::from_snapshot_bodies(&bodies, snapshot.limits)?;
    Ok(RecordingArtifact::new(
        RecordingSource::Posthog {
            host: snapshot.host.to_owned(),
            project: snapshot.project,
            session_id: snapshot.session.to_owned(),
            blob_keys,
        },
        events,
    ))
}

fn get_with_retry<T>(
    max_wait: u64,
    waited: &mut Duration,
    mut request: impl FnMut() -> Result<T>,
) -> Result<T> {
    loop {
        match request() {
            Ok(response) => return Ok(response),
            Err(error) => {
                let Some(upstream) = error.downcast_ref::<UpstreamError>() else {
                    return Err(error);
                };
                let Some(delay) = upstream
                    .retry_after
                    .as_deref()
                    .filter(|_| upstream.status == 429 && max_wait > 0)
                    .and_then(retry_delay)
                else {
                    return Err(error);
                };
                // A zero-delay 429 must not spin without spending any allowance.
                let delay = delay.max(Duration::from_millis(100));
                if delay > Duration::from_secs(max_wait).saturating_sub(*waited) {
                    return Err(error);
                }
                std::thread::sleep(delay);
                *waited += delay;
            }
        }
    }
}

fn retry_delay(value: &str) -> Option<Duration> {
    value
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .or_else(|| {
            httpdate::parse_http_date(value)
                .ok()
                .map(|until| until.duration_since(SystemTime::now()).unwrap_or_default())
        })
}

/// PostHog's `decompress=false` is a sequence of big-endian length-prefixed raw Snappy blocks.
/// Reject other formats, rather than silently interpreting corrupt binary as snapshot lines.
fn decode_snappy_blocks(wire: &[u8], max_bytes: u64, recording_max_bytes: u64) -> Result<String> {
    ensure!(
        !wire.is_empty(),
        "snapshot body is not valid length-prefixed Snappy: empty body"
    );
    let mut rest = wire;
    let mut output = Vec::new();
    let mut decoder = snap::raw::Decoder::new();
    while !rest.is_empty() {
        ensure!(
            rest.len() >= 4,
            "snapshot body is not valid length-prefixed Snappy: truncated block header"
        );
        let length = u32::from_be_bytes(rest[..4].try_into()?) as usize;
        rest = &rest[4..];
        ensure!(
            length > 0 && length <= rest.len(),
            "snapshot body is not valid length-prefixed Snappy: invalid block length"
        );
        let (block, remaining) = rest.split_at(length);
        rest = remaining;
        let decoded = snap::raw::decompress_len(block)
            .context("snapshot body is not valid length-prefixed Snappy: invalid block")?;
        let separator = usize::from(!output.is_empty());
        let new_size = output
            .len()
            .checked_add(separator)
            .and_then(|bytes| bytes.checked_add(decoded))
            .ok_or(DecodeError::TooLarge {
                max_bytes: recording_max_bytes,
            })?;
        if new_size as u64 > max_bytes {
            return Err(DecodeError::TooLarge {
                max_bytes: recording_max_bytes,
            }
            .into());
        }
        output.reserve_exact(new_size - output.len());
        if separator != 0 {
            output.push(b'\n');
        }
        let start = output.len();
        output.resize(new_size, 0);
        let written = decoder
            .decompress(block, &mut output[start..])
            .context("snapshot body is not valid length-prefixed Snappy: corrupt block")?;
        ensure!(
            written == decoded,
            "snapshot body is not valid length-prefixed Snappy: inconsistent block size"
        );
    }
    String::from_utf8(output).context("snapshot body is not UTF-8 JSONL")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };

    fn framed(lines: &[&str]) -> Vec<u8> {
        let mut wire = Vec::new();
        for line in lines {
            let block = snap::raw::Encoder::new()
                .compress_vec(line.as_bytes())
                .unwrap();
            wire.extend_from_slice(&(block.len() as u32).to_be_bytes());
            wire.extend_from_slice(&block);
        }
        wire
    }

    #[test]
    fn framed_snappy_joins_blocks_and_rejects_oversized_or_invalid_frames() {
        let wire = framed(&["first", "second"]);
        assert_eq!(
            decode_snappy_blocks(&wire, 12, 12).unwrap(),
            "first\nsecond"
        );
        assert!(matches!(
            decode_snappy_blocks(&wire, 11, 11)
                .unwrap_err()
                .downcast_ref::<DecodeError>(),
            Some(DecodeError::TooLarge { max_bytes: 11 })
        ));

        let compressed = snap::raw::Encoder::new()
            .compress_vec(&vec![b'a'; 128 << 10])
            .unwrap();
        let prefix = (1..=5)
            .find(|&n| snap::raw::decompress_len(&compressed[..n]).ok() == Some(128 << 10))
            .unwrap();
        let mut incomplete = Vec::new();
        incomplete.extend_from_slice(&(prefix as u32).to_be_bytes());
        incomplete.extend_from_slice(&compressed[..prefix]);
        // Even an incomplete block advertises its size: reject before allocating or decoding.
        assert!(matches!(
            decode_snappy_blocks(&incomplete, 1024, 1024)
                .unwrap_err()
                .downcast_ref::<DecodeError>(),
            Some(DecodeError::TooLarge { max_bytes: 1024 })
        ));
        let error = decode_snappy_blocks(&[0, 0, 0, 4, 1], 1024, 1024).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("not valid length-prefixed Snappy")
        );
        assert!(decode_snappy_blocks(&[], 1024, 1024).is_err());
    }

    #[test]
    fn rate_limit_retry_keeps_downloaded_ranges_and_retries_only_the_throttled_request() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let listing = json!({ "sources": (0..21).map(|key| json!({
                "source": "blob_v2", "blob_key": format!("k{key:02}")
            })).collect::<Vec<_>>() })
            .to_string();
            let first = framed(&[
                &json!(["tab", { "type": 4, "timestamp": 1, "data": { "href": "https://example.test/one" } }]).to_string(),
                &json!(["tab", { "type": 4, "timestamp": 2, "data": { "href": "https://example.test/two" } }]).to_string(),
            ]);
            let second = framed(&[
                &json!(["tab", { "type": 4, "timestamp": 3, "data": { "href": "https://example.test/three" } }]).to_string(),
            ]);
            let mut starts = Vec::new();
            for attempt in 0..4 {
                let Some((mut stream, _)) = (0..400).find_map(|_| match listener.accept() {
                    Ok(connection) => Some(connection),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                        None
                    }
                    Err(error) => panic!("accepting loopback request: {error}"),
                }) else {
                    break;
                };
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                let path = request.split_whitespace().nth(1).unwrap();
                let url = url::Url::parse(&format!("http://localhost{path}")).unwrap();
                let pairs = url
                    .query_pairs()
                    .collect::<std::collections::BTreeMap<_, _>>();
                starts.push(pairs.get("start_blob_key").map(|key| key.to_string()));
                let (status, body, retry_after) = match attempt {
                    0 => {
                        assert_eq!(pairs.get("blob_v2").map(|v| v.as_ref()), Some("true"));
                        ("200 OK", listing.as_bytes(), "")
                    }
                    1 => {
                        assert_eq!(pairs.get("end_blob_key").map(|v| v.as_ref()), Some("k19"));
                        assert_eq!(pairs.get("decompress").map(|v| v.as_ref()), Some("false"));
                        ("200 OK", first.as_slice(), "")
                    }
                    2 | 3 => {
                        assert_eq!(pairs.get("end_blob_key").map(|v| v.as_ref()), Some("k20"));
                        assert_eq!(pairs.get("decompress").map(|v| v.as_ref()), Some("false"));
                        if attempt == 2 {
                            (
                                "429 Too Many Requests",
                                b"".as_slice(),
                                "Retry-After: 1\r\n",
                            )
                        } else {
                            ("200 OK", second.as_slice(), "")
                        }
                    }
                    _ => unreachable!(),
                };
                stream
                    .write_all(
                        format!(
                            // One request per connection: a pooled connection the server
                            // already closed would reset the client's next request.
                            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{retry_after}\r\n",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .unwrap();
                stream.write_all(body).unwrap();
            }
            starts
        });
        let artifact = fetch_with_token(
            &Http::new(3).unwrap(),
            &Snapshot {
                host: &host,
                project: 7,
                session: "session",
                max_requests: 3,
                max_wait: 1,
                limits: Limits { max_bytes: 1024 },
            },
            "token",
        );
        let starts = server.join().unwrap();
        let artifact = artifact.unwrap();
        assert_eq!(artifact.events.len(), 3);
        assert_eq!(
            artifact
                .events
                .events()
                .map(|event| event.timestamp.0)
                .collect::<Vec<_>>(),
            [1.0, 2.0, 3.0]
        );
        assert_eq!(
            starts,
            [
                None,
                Some("k00".into()),
                Some("k20".into()),
                Some("k20".into())
            ]
        );
    }

    #[test]
    fn retry_after_longer_than_wait_budget_preserves_retryable_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut byte = [0];
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
                request.push(byte[0]);
            }
            stream
                .write_all(b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 2\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
        });
        let error = fetch_with_token(
            &Http::new(3).unwrap(),
            &Snapshot {
                host: &format!("http://{address}"),
                project: 7,
                session: "session",
                max_requests: 3,
                max_wait: 1,
                limits: Limits::default(),
            },
            "token",
        )
        .err()
        .unwrap();
        server.join().unwrap();
        assert!(crate::http::is_retryable(&error));
        assert!(error.to_string().contains("retry after 2"));
    }

    fn cursor() -> Cursor {
        Cursor {
            window: Window {
                host: "https://eu.posthog.com".into(),
                project: 7,
                since: "2026-09-01".into(),
                until: "2026-09-02".into(),
            },
            after: Position {
                start_time: "2026-09-01 10:00:00".into(),
                session_id: "s-1".into(),
            },
        }
    }

    #[test]
    fn discovery_binds_window_and_cursor_without_scanning_other_segments() {
        let mut window = cursor().window;
        window.since = "2026-09-01T00:00:00Z' sentinel".into();
        window.until = "2026-09-02T00:00:00Z' endpoint".into();
        let after = Position {
            start_time: "2026-09-01T01:00:00Z' cursor".into(),
            session_id: "session' marker".into(),
        };
        let body = query_body(
            &Discovery {
                window: window.clone(),
                limit: 100,
            },
            Some(&after),
        );
        let sql = body["query"]["query"].as_str().unwrap();
        assert_eq!(body["name"], "spoiler recordings list");
        for value in [
            &window.since,
            &window.until,
            &after.start_time,
            &after.session_id,
        ] {
            assert!(!sql.contains(value), "user value leaked into SQL");
        }
        assert_eq!(body["query"]["values"]["since"], window.since);
        assert_eq!(body["query"]["values"]["until"], window.until);
        assert_eq!(body["query"]["values"]["cursor_start"], after.start_time);
        assert_eq!(body["query"]["values"]["cursor_id"], after.session_id);
        assert!(sql.contains(
            "min_first_timestamp >= parseDateTimeBestEffort({since}) - INTERVAL 24 HOUR"
        ));
        assert!(sql.contains(
            "min_first_timestamp <= parseDateTimeBestEffort({until}) + INTERVAL 24 HOUR"
        ));
        assert!(sql.contains("HAVING start_time >= parseDateTimeBestEffort({since})"));
        assert!(sql.contains("end_time <= parseDateTimeBestEffort({until})"));
        assert!(sql.contains("start_time < now() - INTERVAL 24 HOUR"));
        assert!(sql.contains("max(is_deleted) = 0"));
        assert!(sql.contains("argMinMerge(snapshot_source) AS snapshot_source"));
        assert!(sql.contains("argMinMerge(snapshot_library) AS snapshot_library"));
        assert!(sql.contains("retention_period_days"));
        assert!(sql.contains("session_id > {cursor_id}"));
        assert!(sql.contains("LIMIT 101"));
        // Outlier probes cover both sides without scanning the entire retention window.
        assert!(
            sql.contains("session_id NOT IN (SELECT session_id FROM raw_session_replay_events")
        );
        assert!(
            sql.contains(
                "min_first_timestamp >= parseDateTimeBestEffort({since}) - INTERVAL 7 DAY"
            )
        );
        assert!(
            sql.contains(
                "min_first_timestamp < parseDateTimeBestEffort({since}) - INTERVAL 24 HOUR"
            )
        );
        assert!(
            sql.contains(
                "min_first_timestamp > parseDateTimeBestEffort({until}) + INTERVAL 24 HOUR"
            )
        );
        assert!(
            sql.contains(
                "min_first_timestamp <= parseDateTimeBestEffort({until}) + INTERVAL 7 DAY"
            )
        );
    }

    #[test]
    fn discovery_without_cursor_omits_cursor_bindings() {
        let body = query_body(
            &Discovery {
                window: cursor().window,
                limit: 1,
            },
            None,
        );
        assert!(body["query"]["values"].get("cursor_start").is_none());
        assert!(body["query"]["values"].get("cursor_id").is_none());
        assert!(
            !body["query"]["query"]
                .as_str()
                .unwrap()
                .contains("{cursor_id}")
        );
    }

    #[test]
    fn a_cursor_token_resumes_the_same_window_and_position() {
        let token = cursor().encode().unwrap();
        assert!(
            !token.contains(['"', '{', ' ', '/', '+']),
            "shell- and column-safe: {token}"
        );
        assert_eq!(Cursor::decode(&token).unwrap(), cursor());
        // Another format version, or a token that is not one, is refused rather than guessed at.
        assert!(Cursor::decode(&token.replacen("v1.", "v2.", 1)).is_err());
        assert!(Cursor::decode(&token[..token.len() - 3]).is_err());
    }

    #[test]
    fn flags_beside_a_cursor_must_match_its_window() {
        let window = cursor().window;
        let given = |project, since: Option<&str>| PartialWindow {
            host: None,
            project,
            since: since.map(str::to_owned),
            until: None,
        };
        assert!(given(None, None).agrees_with(&window));
        assert!(given(Some(7), Some("2026-09-01")).agrees_with(&window));
        assert!(!given(Some(8), None).agrees_with(&window));
        assert!(!given(None, Some("2026-08-01")).agrees_with(&window));
    }
}
