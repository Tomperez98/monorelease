//! `mono doctor` — validate the complete root-project task graph.

use std::error::Error as StdError;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::project::{Project, ProjectError};

/// Validate that `dir` resolves to a healthy root-project manifest and return
/// the root it resolved to.
///
/// The root is part of the answer because `mono check` reports it; a caller
/// that discarded it and re-loaded the manifest to learn it would be a second
/// authority on what a healthy project is.
pub fn doctor(dir: &Path) -> Result<PathBuf, DoctorError> {
    let project = Project::load(dir)?;
    Ok(project.root)
}

/// Expected failures of [`doctor`].
#[derive(Debug)]
pub enum DoctorError {
    Project(ProjectError),
}

impl fmt::Display for DoctorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Project(error) => error.fmt(f),
        }
    }
}

impl StdError for DoctorError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Project(error) => Some(error),
        }
    }
}

impl From<ProjectError> for DoctorError {
    fn from(error: ProjectError) -> Self {
        Self::Project(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::config_path;
    use crate::testing::TempDir;
    use std::fs;

    #[test]
    fn validates_a_root_project_manifest() {
        let temp = TempDir::new();
        fs::write(
            config_path(temp.path()),
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\", \"test\"]\n\n[tasks.build]\ncommand = [\"echo\", \"app\"]\n\n[tasks.test]\ncommand = [\"echo\", \"app\"]\n",
        )
        .expect("write project manifest");

        assert_eq!(
            doctor(temp.path()).expect("doctor succeeds"),
            fs::canonicalize(temp.path()).expect("temp path canonicalizes")
        );
    }
}
