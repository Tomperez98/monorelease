//! Repository-specific release artifact contract.
//!
//! GitHub Actions remains responsible for moving artifacts and publishing the
//! release. This module owns the repeated contract: which platform artifacts
//! exist, how release identity is recorded, and how the assembled directory is
//! verified before or after publication.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use mono::{
    ReleaseError, ReleaseIdentity, create_manifest_with_expected, verify_manifest_with_expected,
};

use crate::Error;
use crate::release_model::artifact_inventory;

pub(crate) const COMPONENT: &str = "release-contract";
const DEFAULT_DIRECTORY: &str = "dist";

// Tests and release steps can invoke the contract concurrently in one process.
// Include a monotonic suffix so one caller cannot remove another caller's
// temporary inventory file.
static EXPECTED_INVENTORY_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) fn run_with_identity(
    directory: &Path,
    verify_only: bool,
    identity: ReleaseIdentity,
) -> Result<(), Error> {
    let tag = identity
        .release_tag
        .as_deref()
        .ok_or_else(|| Error::Invalid("release contract identity has no tag".to_owned()))?;
    let expected_path = expected_inventory(tag)?;

    let result = if verify_only {
        verify_manifest_with_expected(directory, identity, Some(&expected_path))
            .map(|_| "verified")
            .map_err(release_error)
    } else {
        create_manifest_with_expected(directory, identity.clone(), Some(&expected_path))
            .map_err(release_error)
            .and_then(|_| {
                verify_manifest_with_expected(directory, identity, Some(&expected_path))
                    .map(|_| "wrote and verified")
                    .map_err(release_error)
            })
    };

    let cleanup = fs::remove_file(&expected_path);
    if let (Err(error), true) = (cleanup, result.is_ok()) {
        return Err(Error::Command(format!(
            "failed to remove temporary release inventory {}: {error}",
            expected_path.display()
        )));
    }

    let action = result?;
    println!(
        "{COMPONENT}: {action} release contract for {}",
        directory.display()
    );
    Ok(())
}

fn expected_inventory(tag: &str) -> Result<PathBuf, Error> {
    let expected = artifact_inventory(tag)?;
    let unique = EXPECTED_INVENTORY_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = env::temp_dir().join(format!(
        "mono-release-expected-{}-{unique}",
        std::process::id()
    ));
    fs::write(&path, expected.join("\n") + "\n").map_err(|error| {
        Error::Command(format!(
            "failed to write temporary release inventory {}: {error}",
            path.display()
        ))
    })?;
    Ok(path)
}

fn release_error(error: ReleaseError) -> Error {
    Error::Command(error.to_string())
}

pub(crate) fn default_directory() -> &'static str {
    DEFAULT_DIRECTORY
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expected_inventory_names_are_versioned_for_every_target() {
        let path = expected_inventory("v1.2.3").unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        fs::remove_file(path).unwrap();

        assert!(contents.contains("mono-v1.2.3-x86_64-unknown-linux-gnu.tar.gz"));
        assert!(contents.contains("mono-v1.2.3-aarch64-apple-darwin.tar.gz"));
        assert!(contents.contains("mono-v1.2.3-x86_64-apple-darwin.tar.gz"));
        assert!(contents.contains("mono-v1.2.3-x86_64-pc-windows-msvc.zip"));
        assert!(contents.lines().any(|line| line == "install.sh"));
        assert!(contents.lines().any(|line| line == "install.ps1"));
    }
}
