//! Crash-safe file publication.
//!
//! Both modes write a same-directory temporary file, flush it to disk, and
//! then publish it. `Replace` can overwrite an existing destination; `New`
//! refuses to, and reports `AlreadyExists` so the caller can name that
//! failure in its own vocabulary.
//!
//! [`tempfile`] owns the temporary-file and publish mechanics, including the
//! platform's create-if-absent primitive, so this module carries no operating
//! system knowledge of its own.

use std::io::{self, Write};
use std::path::Path;

use crate::platform;

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
    // The temporary must share a directory with the destination so that
    // publishing is a same-filesystem rename.
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("mono");

    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(".{name}."))
        .suffix(".tmp")
        .tempfile_in(directory)?;
    temporary.write_all(contents)?;
    temporary.as_file().sync_all()?;
    // `tempfile` creates private files; restore the mode a published file
    // should carry before it becomes visible at the destination.
    platform::prepare_published_file(temporary.path(), path)?;

    match mode {
        WriteMode::Replace => temporary.persist(path).map(|_| ()),
        WriteMode::New => temporary.persist_noclobber(path).map(|_| ()),
    }
    .map_err(io::Error::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;
    use std::fs;

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

    #[test]
    fn replacing_a_file_keeps_its_permissions() {
        let temp = TempDir::new();
        let path = temp.path().join("artifact");
        fs::write(&path, b"before").unwrap();
        let Some(before) = platform::file_mode(&path).unwrap() else {
            return;
        };

        write(&path, b"after", WriteMode::Replace).expect("replace overwrites");

        assert_eq!(platform::file_mode(&path).unwrap(), Some(before));
    }

    /// `tempfile` creates private (`0600`) temporary files. Publishing renames
    /// that file into place, so without an explicit fix a new `mono.toml` would
    /// be readable only by its owner.
    #[test]
    fn a_published_file_is_not_owner_only() {
        let temp = TempDir::new();
        let path = temp.path().join("artifact");

        write(&path, b"content", WriteMode::New).expect("new writes");

        let Some(mode) = platform::file_mode(&path).unwrap() else {
            return;
        };
        assert_eq!(
            mode & 0o777,
            0o644,
            "temporary-file mode leaked to the destination"
        );
    }
}
