//! `monore init` — scaffold a fresh repository.

use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::config::{MonorepoConfig, config_path, render_config};

/// Create `dir` if needed and write a fresh [`MonorepoConfig`] into it.
///
/// Returns the config file that was written. Refuses to touch an existing
/// config, so a mistyped `init` can never destroy a real one.
pub fn init(dir: &Path) -> Result<PathBuf, InitError> {
    fs::create_dir_all(dir).map_err(|source| InitError::CreateDir {
        dir: dir.to_path_buf(),
        source,
    })?;

    let path = config_path(dir);
    let contents = render_config(&MonorepoConfig::template());

    // `create_new` makes "already initialized" atomic with the create: the
    // check and the open are a single syscall, so an existing config is never
    // opened, truncated, or raced by a concurrent `init`.
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|source| match source.kind() {
            io::ErrorKind::AlreadyExists => InitError::AlreadyInitialized(path.clone()),
            _ => InitError::WriteConfig {
                path: path.clone(),
                source,
            },
        })?;

    if let Err(source) = file.write_all(contents.as_bytes()) {
        // A half-written config would block every later `init` with
        // `AlreadyInitialized`, so drop it. If the cleanup itself fails the
        // write error is still the one worth reporting.
        let _ = fs::remove_file(&path);
        return Err(InitError::WriteConfig { path, source });
    }

    Ok(path)
}

/// Expected failures of [`init`].
#[derive(Debug)]
pub enum InitError {
    /// A config already exists at this path; it was left untouched.
    AlreadyInitialized(PathBuf),
    /// The target directory could not be created.
    CreateDir { dir: PathBuf, source: io::Error },
    /// The config file could not be created or written.
    WriteConfig { path: PathBuf, source: io::Error },
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyInitialized(path) => {
                write!(
                    f,
                    "{} already exists — refusing to overwrite",
                    path.display()
                )
            }
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
            Self::AlreadyInitialized(_) => None,
            Self::CreateDir { source, .. } | Self::WriteConfig { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CONFIG_FILE_NAME, CONFIG_VERSION};
    use crate::testing::TempDir;

    fn read_config(path: &Path) -> MonorepoConfig {
        let contents = fs::read_to_string(path).expect("config is readable");
        toml::from_str(&contents).expect("config parses")
    }

    #[test]
    fn creates_nested_directories_and_writes_the_config() {
        let temp = TempDir::new();
        let target = temp.path().join("nested").join("repo");

        let written = init(&target).expect("init succeeds");

        assert_eq!(written, target.join(CONFIG_FILE_NAME));
        assert_eq!(read_config(&written).version, CONFIG_VERSION);
    }

    #[test]
    fn refuses_to_overwrite_an_existing_config() {
        let temp = TempDir::new();
        let existing = config_path(temp.path());
        fs::write(&existing, "version = 99\n").expect("seed an existing config");

        let error = init(temp.path()).expect_err("a second init must fail");

        assert!(matches!(error, InitError::AlreadyInitialized(path) if path == existing));
        assert_eq!(
            fs::read_to_string(&existing).expect("config is readable"),
            "version = 99\n",
        );
    }

    #[test]
    fn reports_a_directory_it_cannot_create() {
        let temp = TempDir::new();
        let blocker = temp.path().join("file");
        fs::write(&blocker, "not a directory").expect("seed a blocking file");

        let error = init(&blocker).expect_err("init inside a file must fail");

        assert!(matches!(error, InitError::CreateDir { .. }));
    }
}
