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
