//! Root manifest discovery for a single-project execution graph.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{MonoConfig, config_path};
use crate::workspace::{WorkspaceError, validate_schema};

#[derive(Debug)]
pub(crate) struct DiscoveredRoot {
    pub(crate) root: PathBuf,
    pub(crate) config: MonoConfig,
}

/// Find the nearest root `mono.toml`, walking upward from `start`.
pub(crate) fn find_root(start: &Path) -> Result<DiscoveredRoot, WorkspaceError> {
    let start = fs::canonicalize(start).map_err(|source| WorkspaceError::Io {
        path: start.to_path_buf(),
        source,
    })?;
    let start_for_error = start.clone();
    let mut current = if start.is_dir() {
        start
    } else {
        start.parent().unwrap_or(Path::new("/")).to_path_buf()
    };

    loop {
        let manifest_path = config_path(&current);
        if manifest_path.is_file() {
            let config = read_manifest(&manifest_path)?;
            validate_schema(&manifest_path, config.schema)?;
            return Ok(DiscoveredRoot {
                root: current,
                config,
            });
        }
        if !current.pop() {
            break;
        }
    }

    Err(WorkspaceError::MissingRoot {
        start: start_for_error,
    })
}

pub(crate) fn read_manifest(path: &Path) -> Result<MonoConfig, WorkspaceError> {
    let contents = fs::read_to_string(path).map_err(|source| WorkspaceError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    MonoConfig::parse(&contents).map_err(|source| WorkspaceError::Parse {
        path: path.to_path_buf(),
        source,
    })
}
