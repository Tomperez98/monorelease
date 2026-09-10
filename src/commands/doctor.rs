//! `monore doctor` — validate the complete workspace manifest graph.

use std::error::Error as StdError;
use std::fmt;
use std::path::Path;

use crate::workspace::{Workspace, WorkspaceError};

/// Validate that `dir` resolves to a healthy manifest-driven workspace.
pub fn doctor(dir: &Path) -> Result<(), DoctorError> {
    Workspace::load(dir)?;
    Ok(())
}

/// Expected failures of [`doctor`].
#[derive(Debug)]
pub enum DoctorError {
    Workspace(WorkspaceError),
}

impl fmt::Display for DoctorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Workspace(error) => error.fmt(f),
        }
    }
}

impl StdError for DoctorError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Workspace(error) => Some(error),
        }
    }
}

impl From<WorkspaceError> for DoctorError {
    fn from(error: WorkspaceError) -> Self {
        Self::Workspace(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::config_path;
    use crate::testing::TempDir;
    use std::fs;

    #[test]
    fn validates_a_root_and_package_manifest() {
        let temp = TempDir::new();
        fs::write(
            config_path(temp.path()),
            "[workspace]\nname = \"fixture\"\nmembers = [\"packages/*\"]\n\n[pipelines.ci]\ntasks = [\"build\", \"test\"]\n",
        )
        .expect("write root manifest");
        let package = temp.path().join("packages/app");
        fs::create_dir_all(&package).expect("create package");
        fs::write(
            config_path(&package),
            "[package]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"echo\", \"app\"]\n\n[tasks.test]\ncommand = [\"echo\", \"app\"]\n",
        )
        .expect("write package manifest");

        doctor(temp.path()).expect("doctor succeeds");
    }
}
