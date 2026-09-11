//! Crash-safe file publication.
//!
//! Both modes write a same-directory temporary file, flush it to disk, and
//! then publish it. `Replace` can overwrite an existing destination; `New`
//! refuses to, and reports `AlreadyExists` so the caller can name that
//! failure in its own vocabulary.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// How the temporary file is published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteMode {
    /// Atomically replace the destination, if it exists.
    Replace,
    /// Create the destination, failing if it already exists.
    New,
}

/// Publish `contents` at `path`.
pub(crate) fn write(path: &Path, contents: &[u8], mode: WriteMode) -> io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("mono");
    let temporary = path.with_file_name(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
    ));

    let result = write_temporary(&temporary, contents).and_then(|()| match mode {
        WriteMode::Replace => replace_file(&temporary, path),
        WriteMode::New => create_new(&temporary, path),
    });
    let _ = fs::remove_file(&temporary);
    result
}

fn write_temporary(temporary: &Path, contents: &[u8]) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temporary)?;
    file.write_all(contents)?;
    file.sync_all()
}

fn replace_file(temporary: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::rename(temporary, destination)
    }

    #[cfg(not(unix))]
    {
        match fs::rename(temporary, destination) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                fs::remove_file(destination)?;
                fs::rename(temporary, destination)
            }
            Err(error) => Err(error),
        }
    }
}

/// Publish without replacing.
///
/// A hard link is the atomic create-if-absent primitive on every platform
/// this crate targets; it fails with `AlreadyExists` when the destination is
/// taken.
fn create_new(temporary: &Path, destination: &Path) -> io::Result<()> {
    fs::hard_link(temporary, destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn replace_writes_a_new_file() {
        let temp = TempDir::new();
        let path = temp.path().join("artifact");

        write(&path, b"content", WriteMode::Replace).expect("replace writes");

        assert_eq!(fs::read_to_string(&path).unwrap(), "content");
    }

    #[test]
    fn replace_overwrites_an_existing_file() {
        let temp = TempDir::new();
        let path = temp.path().join("artifact");
        fs::write(&path, b"before").unwrap();

        write(&path, b"after", WriteMode::Replace).expect("replace overwrites");

        assert_eq!(fs::read_to_string(&path).unwrap(), "after");
    }

    #[test]
    fn new_refuses_to_replace_an_existing_file() {
        let temp = TempDir::new();
        let path = temp.path().join("artifact");
        fs::write(&path, b"before").unwrap();

        let error = write(&path, b"after", WriteMode::New).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&path).unwrap(), "before");
    }

    #[test]
    fn new_writes_when_the_destination_is_absent() {
        let temp = TempDir::new();
        let path = temp.path().join("artifact");

        write(&path, b"content", WriteMode::New).expect("new writes");

        assert_eq!(fs::read_to_string(&path).unwrap(), "content");
    }

    #[test]
    fn no_temporary_file_survives_a_success_or_a_refusal() {
        let temp = TempDir::new();
        let path = temp.path().join("artifact");

        write(&path, b"content", WriteMode::Replace).expect("replace writes");
        let _ = write(&path, b"content", WriteMode::New);

        assert!(
            fs::read_dir(temp.path())
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| !entry.file_name().to_string_lossy().ends_with(".tmp")),
            "a temporary file was left behind"
        );
    }
}
