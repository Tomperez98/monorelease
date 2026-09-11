//! Provider-neutral release artifact manifests and verification.
//!
//! This module handles files, hashes, and explicit release identity only. It
//! does not publish anything, execute artifacts, inspect registries, or assume
//! a language, package manager, or CI provider.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

pub const METADATA_FILE_NAME: &str = "BUILD-METADATA.json";
pub const CHECKSUMS_FILE_NAME: &str = "SHA256SUMS";
const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReleaseIdentity {
    pub repository: Option<String>,
    pub release_tag: Option<String>,
    pub source_commit: Option<String>,
    pub workflow_run: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Artifact {
    pub name: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseManifest {
    pub manifest_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_run: Option<String>,
    pub artifacts: Vec<Artifact>,
}

impl ReleaseManifest {
    fn identity(&self) -> ReleaseIdentity {
        ReleaseIdentity {
            repository: self.repository.clone(),
            release_tag: self.release_tag.clone(),
            source_commit: self.source_commit.clone(),
            workflow_run: self.workflow_run.clone(),
        }
    }
}

#[derive(Debug)]
pub enum ReleaseError {
    Read { path: PathBuf, source: io::Error },
    Write { path: PathBuf, source: io::Error },
    Command(String),
    Invalid(String),
}

impl fmt::Display for ReleaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(formatter, "failed to read {}: {source}", path.display())
            }
            Self::Write { path, source } => {
                write!(formatter, "failed to write {}: {source}", path.display())
            }
            Self::Command(message) | Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

impl StdError for ReleaseError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Read { source, .. } | Self::Write { source, .. } => Some(source),
            Self::Command(_) | Self::Invalid(_) => None,
        }
    }
}

/// Create metadata and checksum files from every regular file below `directory`.
/// The two generated files themselves are excluded from the inventory.
pub fn create_manifest(
    directory: &Path,
    identity: ReleaseIdentity,
) -> Result<ReleaseManifest, ReleaseError> {
    let artifacts = collect_artifacts(directory)?;
    if artifacts.is_empty() {
        return Err(ReleaseError::Invalid(format!(
            "artifact directory {} contains no release artifacts",
            directory.display()
        )));
    }
    let manifest = ReleaseManifest {
        manifest_version: MANIFEST_VERSION,
        repository: identity.repository,
        release_tag: identity.release_tag,
        source_commit: identity.source_commit,
        workflow_run: identity.workflow_run,
        artifacts,
    };
    write_outputs(directory, &manifest)?;
    Ok(manifest)
}

/// Verify metadata, checksums, and the exact regular-file inventory.
pub fn verify_source(
    repository: &Path,
    tag: &str,
    expected_commit: &str,
) -> Result<(), ReleaseError> {
    if tag.is_empty() || expected_commit.is_empty() {
        return Err(ReleaseError::Invalid(
            "release source verification requires a non-empty tag and commit".to_owned(),
        ));
    }
    if tag.starts_with('-') || tag.chars().any(char::is_whitespace) {
        return Err(ReleaseError::Invalid(format!(
            "release tag `{tag}` is not a safe Git ref"
        )));
    }

    let checkout_commit = git(repository, &["rev-parse", "HEAD"])?;
    let tag_ref = format!("{tag}^{{commit}}");
    let tag_commit = git(repository, &["rev-parse", "--verify", &tag_ref])?;
    if checkout_commit != tag_commit || checkout_commit != expected_commit {
        return Err(ReleaseError::Invalid(format!(
            "release source does not match: checkout {checkout_commit}, tag {tag_commit}, expected {expected_commit}"
        )));
    }
    Ok(())
}

/// Verify a legacy `SHA256SUMS` file without requiring release metadata.
pub fn verify_checksums(directory: &Path) -> Result<usize, ReleaseError> {
    let checksums_path = directory.join(CHECKSUMS_FILE_NAME);
    let entries = read_checksums(&checksums_path)?;
    for (name, expected) in &entries {
        let path = directory.join(name);
        if !path.starts_with(directory) {
            return Err(ReleaseError::Invalid(format!(
                "checksum entry `{name}` escapes the artifact directory"
            )));
        }
        let metadata = fs::symlink_metadata(&path).map_err(|source| ReleaseError::Read {
            path: path.clone(),
            source,
        })?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(ReleaseError::Invalid(format!(
                "checksum entry `{name}` is not a regular file"
            )));
        }
        let actual = digest(&path)?;
        if &actual != expected {
            return Err(ReleaseError::Invalid(format!(
                "{name}: checksum mismatch; expected {expected}, actual {actual}"
            )));
        }
    }
    Ok(entries.len())
}

pub fn verify_manifest(
    directory: &Path,
    expected: ReleaseIdentity,
) -> Result<ReleaseManifest, ReleaseError> {
    let metadata_path = directory.join(METADATA_FILE_NAME);
    let checksums_path = directory.join(CHECKSUMS_FILE_NAME);
    let manifest_text = read_to_string(&metadata_path)?;
    let manifest: ReleaseManifest = serde_json::from_str(&manifest_text).map_err(|error| {
        ReleaseError::Invalid(format!(
            "{} is invalid JSON: {error}",
            metadata_path.display()
        ))
    })?;

    if manifest.manifest_version != MANIFEST_VERSION {
        return Err(ReleaseError::Invalid(format!(
            "{} has unsupported manifest version {}; expected {}",
            metadata_path.display(),
            manifest.manifest_version,
            MANIFEST_VERSION
        )));
    }
    compare_identity(&manifest.identity(), &expected)?;

    let actual_artifacts = collect_artifacts(directory)?;
    let expected_artifacts = artifact_map(&manifest.artifacts, "metadata")?;
    let actual_artifacts_map = artifact_map(&actual_artifacts, "directory")?;
    if expected_artifacts.keys().collect::<Vec<_>>()
        != actual_artifacts_map.keys().collect::<Vec<_>>()
    {
        return Err(ReleaseError::Invalid(format!(
            "{} artifact inventory does not match the files in {}",
            metadata_path.display(),
            directory.display()
        )));
    }

    for artifact in &manifest.artifacts {
        let actual = actual_artifacts_map
            .get(&artifact.name)
            .expect("inventory key comparison guarantees every artifact exists");
        if actual.sha256 != artifact.sha256 || actual.size != artifact.size {
            return Err(ReleaseError::Invalid(format!(
                "artifact `{}` does not match its metadata",
                artifact.name
            )));
        }
    }

    let checksum_entries = read_checksums(&checksums_path)?;
    if checksum_entries
        != expected_artifacts
            .iter()
            .map(|(name, artifact)| (name.clone(), artifact.sha256.clone()))
            .collect()
    {
        return Err(ReleaseError::Invalid(format!(
            "{} does not match the release manifest",
            checksums_path.display()
        )));
    }

    Ok(manifest)
}

fn git(repository: &Path, args: &[&str]) -> Result<String, ReleaseError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repository)
        .output()
        .map_err(|error| {
            ReleaseError::Command(format!("could not run `git {}`: {error}", args.join(" ")))
        })?;
    if !output.status.success() {
        return Err(ReleaseError::Command(format!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn compare_identity(
    actual: &ReleaseIdentity,
    expected: &ReleaseIdentity,
) -> Result<(), ReleaseError> {
    for (field, actual_value, expected_value) in [
        ("repository", &actual.repository, &expected.repository),
        ("release tag", &actual.release_tag, &expected.release_tag),
        (
            "source commit",
            &actual.source_commit,
            &expected.source_commit,
        ),
        ("workflow run", &actual.workflow_run, &expected.workflow_run),
    ] {
        if let Some(expected) = expected_value
            && actual_value.as_ref() != Some(expected)
        {
            return Err(ReleaseError::Invalid(format!(
                "release metadata {field} does not match expected value `{expected}`"
            )));
        }
    }
    Ok(())
}

fn collect_artifacts(directory: &Path) -> Result<Vec<Artifact>, ReleaseError> {
    if !directory.is_dir() {
        return Err(ReleaseError::Invalid(format!(
            "artifact directory {} does not exist or is not a directory",
            directory.display()
        )));
    }

    let mut files = Vec::new();
    collect_files(directory, directory, &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));

    files
        .into_iter()
        .map(|(name, path)| {
            let metadata = fs::metadata(&path).map_err(|source| ReleaseError::Read {
                path: path.clone(),
                source,
            })?;
            Ok(Artifact {
                name,
                sha256: digest(&path)?,
                size: metadata.len(),
            })
        })
        .collect()
}

fn collect_files(
    root: &Path,
    directory: &Path,
    files: &mut Vec<(String, PathBuf)>,
) -> Result<(), ReleaseError> {
    let mut entries = fs::read_dir(directory)
        .map_err(|source| ReleaseError::Read {
            path: directory.to_path_buf(),
            source,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| ReleaseError::Read {
            path: directory.to_path_buf(),
            source,
        })?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let file_type = fs::symlink_metadata(&path)
            .map_err(|source| ReleaseError::Read {
                path: path.clone(),
                source,
            })?
            .file_type();
        if file_type.is_symlink() {
            return Err(ReleaseError::Invalid(format!(
                "release artifact inventory cannot contain symlink {}",
                path.display()
            )));
        }
        if file_type.is_dir() {
            collect_files(root, &path, files)?;
        } else if file_type.is_file() {
            let relative = path.strip_prefix(root).expect("walked path is below root");
            if relative == Path::new(METADATA_FILE_NAME)
                || relative == Path::new(CHECKSUMS_FILE_NAME)
            {
                continue;
            }
            let name = relative_name(relative)?;
            files.push((name, path));
        } else {
            return Err(ReleaseError::Invalid(format!(
                "unsupported release artifact file type: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn relative_name(path: &Path) -> Result<String, ReleaseError> {
    let mut name = String::new();
    for component in path.components() {
        let Component::Normal(component) = component else {
            return Err(ReleaseError::Invalid(format!(
                "release artifact path `{}` is not a normal relative path",
                path.display()
            )));
        };
        if !name.is_empty() {
            name.push('/');
        }
        let component = component.to_str().ok_or_else(|| {
            ReleaseError::Invalid(format!(
                "release artifact path `{}` is not valid UTF-8",
                path.display()
            ))
        })?;
        if component.contains(['\n', '\r']) {
            return Err(ReleaseError::Invalid(format!(
                "release artifact path `{}` contains a newline",
                path.display()
            )));
        }
        name.push_str(component);
    }
    Ok(name)
}

fn artifact_map<'a>(
    artifacts: &'a [Artifact],
    source: &str,
) -> Result<BTreeMap<String, &'a Artifact>, ReleaseError> {
    let mut map = BTreeMap::new();
    for artifact in artifacts {
        if map.insert(artifact.name.clone(), artifact).is_some() {
            return Err(ReleaseError::Invalid(format!(
                "duplicate artifact `{}` in {source}",
                artifact.name
            )));
        }
    }
    Ok(map)
}

fn write_outputs(directory: &Path, manifest: &ReleaseManifest) -> Result<(), ReleaseError> {
    let metadata_path = directory.join(METADATA_FILE_NAME);
    let checksums_path = directory.join(CHECKSUMS_FILE_NAME);
    let json = serde_json::to_string_pretty(manifest)
        .expect("ReleaseManifest contains only serializable fields");
    write_file(&metadata_path, &format!("{json}\n"))?;

    let checksums = manifest
        .artifacts
        .iter()
        .map(|artifact| format!("{}  {}", artifact.sha256, artifact.name))
        .collect::<Vec<_>>()
        .join("\n");
    write_file(&checksums_path, &format!("{checksums}\n"))
}

fn read_checksums(path: &Path) -> Result<BTreeMap<String, String>, ReleaseError> {
    let text = read_to_string(path)?;
    let mut entries = BTreeMap::new();
    for (line_number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let Some((digest, name)) = line.split_once("  ") else {
            return Err(ReleaseError::Invalid(format!(
                "{}:{}: expected `<sha256>  <file>`",
                path.display(),
                line_number + 1
            )));
        };
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(ReleaseError::Invalid(format!(
                "{}:{}: invalid SHA-256 digest",
                path.display(),
                line_number + 1
            )));
        }
        let name = name.strip_prefix('*').unwrap_or(name);
        if name.is_empty()
            || name.contains(['\n', '\r'])
            || Path::new(name).is_absolute()
            || Path::new(name)
                .components()
                .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(ReleaseError::Invalid(format!(
                "{}:{}: invalid artifact name",
                path.display(),
                line_number + 1
            )));
        }
        if entries
            .insert(name.to_owned(), digest.to_ascii_lowercase())
            .is_some()
        {
            return Err(ReleaseError::Invalid(format!(
                "{}:{}: duplicate artifact name `{name}`",
                path.display(),
                line_number + 1
            )));
        }
    }
    if entries.is_empty() {
        return Err(ReleaseError::Invalid(format!(
            "{} lists no artifacts",
            path.display()
        )));
    }
    Ok(entries)
}

fn digest(path: &Path) -> Result<String, ReleaseError> {
    let mut file = fs::File::open(path).map_err(|source| ReleaseError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher).map_err(|source| ReleaseError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn read_to_string(path: &Path) -> Result<String, ReleaseError> {
    fs::read_to_string(path).map_err(|source| ReleaseError::Read {
        path: path.to_path_buf(),
        source,
    })
}

fn write_file(path: &Path, contents: &str) -> Result<(), ReleaseError> {
    fs::write(path, contents).map_err(|source| ReleaseError::Write {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn creates_and_verifies_a_deterministic_manifest() {
        let temp = TempDir::new();
        fs::create_dir_all(temp.path().join("nested")).unwrap();
        fs::write(temp.path().join("zeta.bin"), b"z").unwrap();
        fs::write(temp.path().join("nested/alpha.bin"), b"a").unwrap();

        let manifest = create_manifest(
            temp.path(),
            ReleaseIdentity {
                release_tag: Some("v1.2.3".to_owned()),
                ..ReleaseIdentity::default()
            },
        )
        .unwrap();

        assert_eq!(
            manifest
                .artifacts
                .iter()
                .map(|artifact| artifact.name.as_str())
                .collect::<Vec<_>>(),
            vec!["nested/alpha.bin", "zeta.bin"]
        );
        verify_manifest(
            temp.path(),
            ReleaseIdentity {
                release_tag: Some("v1.2.3".to_owned()),
                ..ReleaseIdentity::default()
            },
        )
        .unwrap();
    }

    #[test]
    fn rejects_an_empty_artifact_directory() {
        let temp = TempDir::new();
        let error = create_manifest(temp.path(), ReleaseIdentity::default()).unwrap_err();
        assert!(error.to_string().contains("contains no release artifacts"));
    }

    #[test]
    fn rejects_a_changed_artifact() {
        let temp = TempDir::new();
        fs::write(temp.path().join("artifact"), b"before").unwrap();
        create_manifest(temp.path(), ReleaseIdentity::default()).unwrap();
        fs::write(temp.path().join("artifact"), b"after").unwrap();

        let error = verify_manifest(temp.path(), ReleaseIdentity::default()).unwrap_err();
        assert!(error.to_string().contains("does not match its metadata"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks() {
        let temp = TempDir::new();
        fs::write(temp.path().join("artifact"), b"data").unwrap();
        std::os::unix::fs::symlink(temp.path().join("artifact"), temp.path().join("link")).unwrap();

        let error = create_manifest(temp.path(), ReleaseIdentity::default()).unwrap_err();
        assert!(error.to_string().contains("symlink"));
    }
}
