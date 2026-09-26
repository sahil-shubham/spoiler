//! Reading inputs and publishing outputs.

use anyhow::{Context, Result, ensure};
use serde::Serialize;
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

pub fn read(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("reading {}", path.display()))
}

/// Check regular-file metadata first, then enforce the same cap while reading (files can grow).
pub fn read_recording(path: &Path, limits: Limits) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    if file
        .metadata()
        .with_context(|| format!("reading {}", path.display()))?
        .len()
        > limits.max_bytes
    {
        return Err(DecodeError::TooLarge {
            max_bytes: limits.max_bytes,
        }
        .into());
    }
    read_limited(file, limits).with_context(|| format!("reading {}", path.display()))
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
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

/// A vocabulary and the digest of the exact file it came from (what traces pin).
pub struct PinnedVocabulary {
    pub vocabulary: Vocabulary,
    pub digest: String,
}

pub fn load_vocabulary(path: &Path) -> Result<PinnedVocabulary> {
    let bytes = read(path)?;
    let vocabulary = Vocabulary::parse(&bytes)
        .with_context(|| format!("loading vocabulary {}", path.display()))?;
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
    let Some(path) = path else {
        let mut stdout = std::io::BufWriter::new(std::io::stdout().lock());
        serde_json::to_writer(&mut stdout, value)?;
        stdout.write_all(b"\n")?;
        return Ok(stdout.flush()?);
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
    let written = write_synced(value, &temporary).and_then(|()| {
        std::fs::rename(&temporary, path).with_context(|| format!("publishing {}", path.display()))
    });
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

fn temporary_path(directory: &Path) -> Result<PathBuf> {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    Ok(directory.join(format!(".spoiler-{}-{nanos}.tmp", std::process::id())))
}

fn write_synced(value: &impl Serialize, path: &Path) -> Result<()> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    let mut writer = std::io::BufWriter::new(file);
    serde_json::to_writer(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(())
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
