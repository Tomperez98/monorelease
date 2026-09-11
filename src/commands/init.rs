//! `mono init` — scaffold a root project.

use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::config::{MonoConfig, config_path, render_config};

/// Create `dir` if needed and write a valid root manifest.
pub fn init(dir: &Path) -> Result<PathBuf, InitError> {
    write_config(dir, render_config(&MonoConfig::template()))
}

fn write_config(dir: &Path, contents: String) -> Result<PathBuf, InitError> {
    fs::create_dir_all(dir).map_err(|source| InitError::CreateDir {
        dir: dir.to_path_buf(),
        source,
    })?;
    let path = config_path(dir);
    crate::atomic_file::write(
        &path,
        contents.as_bytes(),
        crate::atomic_file::WriteMode::New,
    )
    .map_err(|source| match source.kind() {
        io::ErrorKind::AlreadyExists => InitError::AlreadyInitialized(path.clone()),
        _ => InitError::WriteConfig {
            path: path.clone(),
            source,
        },
    })?;
    Ok(path)
}

#[derive(Debug)]
pub enum InitError {
    AlreadyInitialized(PathBuf),
    CreateDir { dir: PathBuf, source: io::Error },
    WriteConfig { path: PathBuf, source: io::Error },
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyInitialized(path) => write!(
                f,
                "{} already exists — refusing to overwrite",
                path.display()
            ),
            Self::CreateDir { dir, source } => {
                write!(f, "could not create directory {}: {source}", dir.display())
            }
            Self::WriteConfig { path, source } => {
                write!(f, "could not write {}: {source}", path.display())
            }
        }
    }
}

impl StdError for InitError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::CreateDir { source, .. } | Self::WriteConfig { source, .. } => Some(source),
            Self::AlreadyInitialized(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn init_writes_a_manifest_that_parses_back() {
        let temp = TempDir::new();

        let written = init(temp.path()).expect("init succeeds");

        assert_eq!(written, config_path(temp.path()));
        let contents = fs::read_to_string(&written).expect("manifest is written");
        let config = MonoConfig::parse(&contents).expect("written manifest parses");
        assert_eq!(config, MonoConfig::template());
    }

    #[test]
    fn init_creates_a_missing_directory() {
        let temp = TempDir::new();
        let nested = temp.path().join("a/b/c");

        let written = init(&nested).expect("init creates the directory");

        assert!(written.is_file(), "{}", written.display());
    }

    #[test]
    fn a_second_init_refuses_to_overwrite() {
        let temp = TempDir::new();
        init(temp.path()).expect("first init succeeds");

        let error = init(temp.path()).expect_err("second init refuses");

        assert!(
            matches!(error, InitError::AlreadyInitialized(ref path) if path == &config_path(temp.path())),
            "{error}"
        );
        assert!(
            error.to_string().contains("refusing to overwrite"),
            "{error}"
        );
        assert!(
            error.source().is_none(),
            "a refusal has no underlying cause"
        );
    }

    #[test]
    fn a_directory_that_cannot_be_created_is_a_tool_failure() {
        let temp = TempDir::new();
        // A file where the directory should be makes `create_dir_all` fail.
        let blocked = temp.path().join("blocked");
        fs::write(&blocked, "not a directory").expect("write blocker file");

        let error = init(&blocked.join("nested")).expect_err("init fails");

        assert!(matches!(error, InitError::CreateDir { .. }), "{error}");
        assert!(error.source().is_some(), "a filesystem cause is exposed");
        assert!(error.to_string().contains("could not create directory"));
    }
}
