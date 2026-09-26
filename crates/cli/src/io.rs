//! Reading inputs and publishing outputs.

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use spoiler_core::{
    artifact::sha256_hex,
    recording::{DecodeError, Limits},
    vocab::Vocabulary,
};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// `-` in place of an input path reads standard input.
fn is_stdin(path: &Path) -> bool {
    path.as_os_str() == "-"
}

fn name(path: &Path) -> std::borrow::Cow<'_, str> {
    if is_stdin(path) {
        "standard input".into()
    } else {
        path.display().to_string().into()
    }
}

pub fn read(path: &Path) -> Result<Vec<u8>> {
    let bytes = if is_stdin(path) {
        let mut bytes = Vec::new();
        std::io::stdin()
            .lock()
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    } else {
        std::fs::read(path)
    };
    bytes.with_context(|| format!("reading {}", name(path)))
}

/// Check regular-file metadata first, then enforce the same cap while reading (files can grow,
/// and standard input has no size to check).
pub fn read_recording(path: &Path, limits: Limits) -> Result<Vec<u8>> {
    if is_stdin(path) {
        return read_limited(std::io::stdin().lock(), limits)
            .with_context(|| format!("reading {}", name(path)));
    }
    let file = std::fs::File::open(path).with_context(|| format!("reading {}", name(path)))?;
    if file
        .metadata()
        .with_context(|| format!("reading {}", name(path)))?
        .len()
        > limits.max_bytes
    {
        return Err(DecodeError::TooLarge {
            max_bytes: limits.max_bytes,
        }
        .into());
    }
    read_limited(file, limits).with_context(|| format!("reading {}", name(path)))
}

fn read_limited(reader: impl Read, limits: Limits) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(limits.max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if (bytes.len() as u64) > limits.max_bytes {
        return Err(DecodeError::TooLarge {
            max_bytes: limits.max_bytes,
        }
        .into());
    }
    Ok(bytes)
}

pub fn read_text(path: &Path) -> Result<String> {
    String::from_utf8(read(path)?).with_context(|| format!("{} is not UTF-8", name(path)))
}

/// A vocabulary and the digest of the exact file it came from (what traces pin).
pub struct PinnedVocabulary {
    pub vocabulary: Vocabulary,
    pub digest: String,
}

pub fn load_vocabulary(path: &Path) -> Result<PinnedVocabulary> {
    let bytes = read(path)?;
    let vocabulary =
        Vocabulary::parse(&bytes).with_context(|| format!("loading vocabulary {}", name(path)))?;
    Ok(PinnedVocabulary {
        vocabulary,
        digest: sha256_hex(&bytes),
    })
}

/// Write JSON to `path` atomically (temporary file, fsync, rename), or to stdout without one.
///
/// A failure before the rename leaves any existing file untouched. An interrupted process may
/// leave a `.spoiler-*.tmp` file behind, never a partially written destination.
pub fn publish(value: &impl Serialize, path: Option<&Path>) -> Result<()> {
    publish_digest(value, path).map(drop)
}

/// [`publish`], returning the SHA-256 of the bytes written: what a reader of the file hashes.
pub fn publish_digest(value: &impl Serialize, path: Option<&Path>) -> Result<String> {
    let Some(path) = path else {
        return write_json(value, std::io::stdout().lock());
    };
    let directory = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    ensure!(
        directory.is_dir(),
        "output directory does not exist: {}",
        directory.display()
    );
    let temporary = temporary_path(directory)?;
    let written = write_synced(value, &temporary).and_then(|digest| {
        std::fs::rename(&temporary, path)
            .with_context(|| format!("publishing {}", path.display()))?;
        Ok(digest)
    });
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

/// The SHA-256 [`publish`] would report for `value`, without writing it anywhere.
pub fn digest(value: &impl Serialize) -> Result<String> {
    write_json(value, std::io::sink())
}

fn temporary_path(directory: &Path) -> Result<PathBuf> {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    Ok(directory.join(format!(".spoiler-{}-{nanos}.tmp", std::process::id())))
}

fn write_synced(value: &impl Serialize, path: &Path) -> Result<String> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    let digest = write_json(value, &file)?;
    file.sync_all()?;
    Ok(digest)
}

/// Compact JSON and a newline: the one published form, hashed as it streams out.
fn write_json(value: &impl Serialize, writer: impl Write) -> Result<String> {
    let mut writer = std::io::BufWriter::new(Hashing {
        inner: writer,
        hasher: Sha256::new(),
    });
    serde_json::to_writer(&mut writer, value)?;
    writer.write_all(b"\n")?;
    let mut hashing = writer.into_inner().map_err(|error| error.into_error())?;
    hashing.inner.flush()?;
    Ok(format!("{:x}", hashing.hasher.finalize()))
}

struct Hashing<W> {
    inner: W,
    hasher: Sha256,
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.hasher.update(&buffer[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{read_limited, read_recording};
    use spoiler_core::recording::{DecodeError, Limits};
    use std::{
        io::{Cursor, Read},
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

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
    fn recording_file_metadata_rejects_oversized_input() {
        let path = std::env::temp_dir().join(format!(
            "spoiler-read-limit-{}-{}",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, b"12345").unwrap();
        let result = read_recording(&path, Limits { max_bytes: 4 });
        std::fs::remove_file(path).unwrap();
        assert!(matches!(
            result.unwrap_err().downcast_ref::<DecodeError>(),
            Some(DecodeError::TooLarge { max_bytes: 4 })
        ));
    }

    #[test]
    fn recording_stream_stops_one_byte_past_limit() {
        let mut reader = CountingReader {
            inner: Cursor::new(vec![b'x'; 4096]),
            bytes_read: 0,
        };
        let result = read_limited(&mut reader, Limits { max_bytes: 10 });
        assert!(matches!(
            result.unwrap_err().downcast_ref::<DecodeError>(),
            Some(DecodeError::TooLarge { max_bytes: 10 })
        ));
        assert_eq!(reader.bytes_read, 11);
        assert_eq!(
            read_limited(Cursor::new(b"yes"), Limits { max_bytes: 3 }).unwrap(),
            b"yes"
        );
    }
}
