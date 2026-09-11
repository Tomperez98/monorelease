//! Provider-neutral release artifact commands.

use std::error::Error as StdError;
use std::fmt;
use std::path::Path;

use crate::release::{
    ReleaseError, ReleaseIdentity, create_manifest, verify_manifest, verify_source,
};

/// Default directory holding the release artifacts and their metadata.
pub const DEFAULT_DIRECTORY: &str = "dist";

pub fn manifest(
    directory: &Path,
    identity: ReleaseIdentity,
) -> Result<String, ReleaseCommandError> {
    let manifest = create_manifest(directory, identity)?;
    Ok(format!(
        "wrote release manifest for {} artifact(s) in {}",
        manifest.artifacts.len(),
        directory.display()
    ))
}

pub fn source(
    repository: &Path,
    tag: &str,
    expected_commit: &str,
) -> Result<String, ReleaseCommandError> {
    verify_source(repository, tag, expected_commit)?;
    Ok(format!(
        "verified release source {tag} at {expected_commit}"
    ))
}

pub fn verify(directory: &Path, expected: ReleaseIdentity) -> Result<String, ReleaseCommandError> {
    let manifest = verify_manifest(directory, expected)?;
    Ok(format!(
        "verified release manifest for {} artifact(s) in {}",
        manifest.artifacts.len(),
        directory.display()
    ))
}

#[derive(Debug)]
pub enum ReleaseCommandError {
    Release(ReleaseError),
}

impl fmt::Display for ReleaseCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Release(error) => error.fmt(formatter),
        }
    }
}

impl StdError for ReleaseCommandError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Release(error) => Some(error),
        }
    }
}

impl From<ReleaseError> for ReleaseCommandError {
    fn from(error: ReleaseError) -> Self {
        Self::Release(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;
    use std::fs;

    #[test]
    fn creates_then_verifies_a_release_directory() {
        let temp = TempDir::new();
        fs::write(temp.path().join("artifact.tar.gz"), b"artifact").unwrap();

        manifest(
            temp.path(),
            ReleaseIdentity {
                release_tag: Some("v1.0.0".to_owned()),
                ..ReleaseIdentity::default()
            },
        )
        .unwrap();
        let output = verify(
            temp.path(),
            ReleaseIdentity {
                release_tag: Some("v1.0.0".to_owned()),
                ..ReleaseIdentity::default()
            },
        )
        .unwrap();
        assert!(output.contains("verified release manifest"));
    }
}
