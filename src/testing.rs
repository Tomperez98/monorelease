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

/// Create a symbolic link, or report that this process is not allowed to.
///
/// Creating a symlink is a platform *capability*, not a platform fact: Windows
/// permits it only with Developer Mode or elevation. Tests call this first so
/// they exercise symlink behavior wherever it is available instead of being
/// compiled out on Windows.
#[allow(dead_code)]
pub fn create_symlink_or_skip(target: &Path, link: &Path) -> bool {
    match symlink(target, link) {
        Ok(()) => true,
        Err(error) => {
            eprintln!("skipped: cannot create a symlink here ({error})");
            false
        }
    }
}

/// Create a symbolic link using whichever API the platform exposes.
#[allow(dead_code)]
fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    {
        // Windows needs the link kind declared up front. Relative targets are
        // resolved against the link's directory, so `is_dir` is only consulted
        // for the absolute directory link the cache tests create.
        if target.is_dir() {
            std::os::windows::fs::symlink_dir(target, link)
        } else {
            std::os::windows::fs::symlink_file(target, link)
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "symbolic links are not supported",
        ))
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
