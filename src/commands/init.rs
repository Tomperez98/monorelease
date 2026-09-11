//! `monorelease init` — scaffold a fresh repository.

use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{CONFIG_FILE_NAME, MonorepoConfig, config_path, render_config};

/// Create `dir` if needed and write a fresh monorepo [`MonorepoConfig`] into it.
///
/// Returns the config file that was written. Refuses to touch an existing
/// config, so a mistyped `init` can never destroy a real one.
pub fn init(dir: &Path) -> Result<PathBuf, InitError> {
    write_config(dir, render_config(&MonorepoConfig::template()))
}

/// Create a standalone project config using one caller-supplied command.
pub fn init_standalone(dir: &Path, command: Vec<String>) -> Result<PathBuf, InitError> {
    if command.is_empty() || command[0].is_empty() {
        return Err(InitError::StandaloneCommandRequired);
    }
    write_config(
        dir,
        render_config(&MonorepoConfig::standalone_template(
            "project".to_owned(),
            command,
        )),
    )
}

fn write_config(dir: &Path, contents: String) -> Result<PathBuf, InitError> {
    fs::create_dir_all(dir).map_err(|source| InitError::CreateDir {
        dir: dir.to_path_buf(),
        source,
    })?;

    let path = config_path(dir);

    // Write to a unique sibling first. Hard-linking the completed file into
    // place gives us publication after the write while still refusing to
    // replace an existing destination on platforms supported by std::fs.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after the epoch")
        .as_nanos();
    let temporary_path = dir.join(format!(
        ".{CONFIG_FILE_NAME}.{}.{}.{}.tmp",
        std::process::id(),
        timestamp,
        COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary_path)
        .map_err(|source| InitError::WriteConfig {
            path: path.clone(),
            source,
        })?;

    if let Err(source) = file.write_all(contents.as_bytes()) {
        let _ = fs::remove_file(&temporary_path);
        return Err(InitError::WriteConfig { path, source });
    }
    if let Err(source) = file.sync_all() {
        let _ = fs::remove_file(&temporary_path);
        return Err(InitError::WriteConfig { path, source });
    }
    drop(file);

    let result = fs::hard_link(&temporary_path, &path);
    let _ = fs::remove_file(&temporary_path);
    result.map_err(|source| match source.kind() {
        io::ErrorKind::AlreadyExists => InitError::AlreadyInitialized(path.clone()),
        _ => InitError::WriteConfig {
            path: path.clone(),
            source,
        },
    })?;

    Ok(path)
}

/// Expected failures of [`init`].
#[derive(Debug)]
pub enum InitError {
    /// Standalone initialization requires at least one command argument.
    StandaloneCommandRequired,
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
            Self::StandaloneCommandRequired => {
                write!(f, "standalone init requires a non-empty --command")
            }
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
            Self::StandaloneCommandRequired | Self::AlreadyInitialized(_) => None,
            Self::CreateDir { source, .. } | Self::WriteConfig { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CONFIG_FILE_NAME;
    use crate::testing::TempDir;

    fn read_config(path: &Path) -> MonorepoConfig {
        let contents = fs::read_to_string(path).expect("config is readable");
        MonorepoConfig::parse(&contents).expect("config parses")
    }

    #[test]
    fn creates_nested_directories_and_writes_the_config() {
        let temp = TempDir::new();
        let target = temp.path().join("nested").join("repo");

        let written = init(&target).expect("init succeeds");

        assert_eq!(written, target.join(CONFIG_FILE_NAME));
        assert_eq!(read_config(&written), MonorepoConfig::template());
    }

    #[test]
    fn refuses_to_overwrite_an_existing_config() {
        let temp = TempDir::new();
        let existing = config_path(temp.path());
        fs::write(&existing, "existing = true\n").expect("seed an existing config");

        let error = init(temp.path()).expect_err("a second init must fail");

        assert!(matches!(error, InitError::AlreadyInitialized(path) if path == existing));
        assert_eq!(
            fs::read_to_string(&existing).expect("config is readable"),
            "existing = true\n",
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
