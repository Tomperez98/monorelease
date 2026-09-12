//! Repository-specific release artifact contract.
//!
//! GitHub Actions remains responsible for moving artifacts and publishing the
//! release. This module owns the repeated contract: which platform artifacts
//! exist, how release identity is recorded, and how the assembled directory is
//! verified before or after publication.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use mono::{
    ReleaseError, ReleaseIdentity, create_manifest_with_expected, verify_manifest_with_expected,
};

use crate::Error;
use crate::release_model::artifact_inventory;

pub(crate) const COMPONENT: &str = "release-contract";
const DEFAULT_DIRECTORY: &str = "dist";

pub(crate) fn run(directory: &Path, verify_only: bool) -> Result<(), Error> {
    let identity = release_identity()?;
    run_with_identity(directory, verify_only, identity)
}

pub(crate) fn run_with_identity(
    directory: &Path,
    verify_only: bool,
    identity: ReleaseIdentity,
) -> Result<(), Error> {
    let tag = identity
        .release_tag
        .as_deref()
        .ok_or_else(|| Error::Invalid("RELEASE_TAG is not set".to_owned()))?;
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

fn release_identity() -> Result<ReleaseIdentity, Error> {
    let tag = required_env("RELEASE_TAG")?;
    let source_commit = match non_empty_env("RELEASE_COMMIT") {
        Some(commit) => commit,
        None => git_output(&["rev-parse", "HEAD"])?,
    };
    let tag_object = match non_empty_env("RELEASE_TAG_OBJECT") {
        Some(tag_object) => Some(tag_object),
        None => annotated_tag_object(&tag)?,
    };

    Ok(ReleaseIdentity {
        repository: non_empty_env("GITHUB_REPOSITORY"),
        release_tag: Some(tag),
        source_commit: Some(source_commit),
        tag_object,
        workflow_run: non_empty_env("RELEASE_RUN_URL"),
    })
}

fn annotated_tag_object(tag: &str) -> Result<Option<String>, Error> {
    match git_output(&["cat-file", "-t", tag])?.as_str() {
        "tag" => {
            let tag_ref = format!("{tag}^{{tag}}");
            Ok(Some(git_output(&["rev-parse", &tag_ref])?))
        }
        "commit" => Ok(None),
        object_type => Err(Error::Invalid(format!(
            "release tag {tag} resolves to unsupported Git object type {object_type}"
        ))),
    }
}

fn expected_inventory(tag: &str) -> Result<PathBuf, Error> {
    let expected = artifact_inventory(tag)?;
    let path = env::temp_dir().join(format!("mono-release-expected-{}", std::process::id()));
    fs::write(&path, expected.join("\n") + "\n").map_err(|error| {
        Error::Command(format!(
            "failed to write temporary release inventory {}: {error}",
            path.display()
        ))
    })?;
    Ok(path)
}

fn required_env(name: &str) -> Result<String, Error> {
    non_empty_env(name).ok_or_else(|| Error::Invalid(format!("{name} is not set")))
}

fn non_empty_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.is_empty())
}

fn git_output(args: &[&str]) -> Result<String, Error> {
    let output = Command::new("git")
        .args(args)
        .output()
        .map_err(|source| Error::Spawn {
            program: format!("git {}", args.join(" ")),
            source,
        })?;
    if !output.status.success() {
        return Err(Error::Command(format!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
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
