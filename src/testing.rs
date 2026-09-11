//! Test-only helpers shared across modules.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// A unique directory that deletes itself when the test ends.
///
/// Hand-rolled to keep the crate dependency-free; `Drop` is what makes
/// cleanup happen even when an assertion panics.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        for _ in 0..100 {
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("mono-{}-{id}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create temp dir {}: {error}", path.display()),
            }
        }
        panic!("could not allocate a unique temporary directory after 100 attempts");
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::TempDir;

    #[test]
    fn temporary_directories_have_distinct_paths() {
        let first = TempDir::new();
        let second = TempDir::new();

        assert_ne!(first.path(), second.path());
        assert!(first.path().is_dir());
        assert!(second.path().is_dir());
    }
}
