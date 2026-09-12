//! Filesystem helpers for assembling a GitHub release.
//!
//! These helpers do not publish anything. They collect release files and compose
//! the final release body before the GitHub adapter uploads it.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::Error;
use crate::release_model::compose_release_notes;

/// The release body is written to a temporary file because `gh --notes-file`
/// accepts a path rather than an in-memory string.
pub(crate) struct ComposedNotes {
    directory: PathBuf,
    path: PathBuf,
}

impl ComposedNotes {
    pub(crate) fn write(tag: &str, notes: &Path) -> Result<Self, Error> {
        let text = fs::read_to_string(notes).map_err(|source| Error::Io {
            path: notes.to_path_buf(),
            source,
        })?;
        let composed = compose_release_notes(tag, &text)?;
        let directory = temporary_directory("notes")?;
        let path = directory.join("RELEASE_NOTES.md");
        fs::write(&path, composed).map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        Ok(Self { directory, path })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ComposedNotes {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

/// Return sorted regular files from an assembled artifact directory.
pub(crate) fn artifact_paths(directory: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut paths = fs::read_dir(directory)
        .map_err(|source| Error::Io {
            path: directory.to_path_buf(),
            source,
        })?
        .map(|entry| {
            let entry = entry.map_err(|source| Error::Io {
                path: directory.to_path_buf(),
                source,
            })?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|source| Error::Io {
                path: path.clone(),
                source,
            })?;
            if file_type.is_file() {
                Ok(Some(path))
            } else {
                Ok(None)
            }
        })
        .collect::<Result<Vec<_>, Error>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    paths.sort();
    if paths.is_empty() {
        return Err(Error::Invalid(format!(
            "release artifact directory {} contains no files",
            directory.display()
        )));
    }
    Ok(paths)
}

fn temporary_directory(prefix: &str) -> Result<PathBuf, Error> {
    let path = env::temp_dir().join(format!(
        "mono-release-{prefix}-artifacts-{}",
        std::process::id()
    ));
    if path.exists() {
        fs::remove_dir_all(&path).map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
    }
    fs::create_dir_all(&path).map_err(|source| Error::Io {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn artifact_paths_are_sorted_and_exclude_directories() {
        let temp = TempDir::new();
        fs::write(temp.path().join("z-last"), "last").unwrap();
        fs::write(temp.path().join("a-first"), "first").unwrap();
        fs::create_dir(temp.path().join("nested")).unwrap();

        let paths = artifact_paths(temp.path()).unwrap();
        let names = paths
            .iter()
            .map(|path| path.file_name().unwrap().to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, ["a-first", "z-last"]);
    }

    #[test]
    fn artifact_paths_reject_an_empty_directory() {
        let temp = TempDir::new();
        let error = artifact_paths(temp.path()).unwrap_err();
        assert!(error.to_string().contains("contains no files"), "{error}");
    }

    #[test]
    fn composed_notes_carry_the_install_section_and_clean_up_after_themselves() {
        let temp = TempDir::new();
        let notes = temp.path().join("RELEASE_NOTES.md");
        fs::write(&notes, "# 0.1.5\n\n## Changelog\n\nReleased: 2026-09-12\n").unwrap();

        let path = {
            let composed = ComposedNotes::write("v0.1.5", &notes).unwrap();
            let path = composed.path().to_path_buf();
            let text = fs::read_to_string(&path).unwrap();
            assert!(text.starts_with("# 0.1.5\n"), "{text}");
            assert!(text.contains("## Install"), "{text}");
            assert!(text.contains("## Changelog"), "{text}");
            path
        };

        assert!(!path.exists(), "the temporary release body is removed");
    }

    #[test]
    fn composed_notes_refuse_a_release_notes_file_for_another_version() {
        let temp = TempDir::new();
        let notes = temp.path().join("RELEASE_NOTES.md");
        fs::write(&notes, "# 0.1.4\n\nbody\n").unwrap();

        let error = match ComposedNotes::write("v0.1.5", &notes) {
            Ok(_) => panic!("a mismatched heading must be rejected"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("must start with"), "{error}");
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "mono-xtask-release-artifacts-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
