//! Provider-neutral release artifact manifests and verification.
//!
//! This module handles files, hashes, and explicit release identity only. It
//! does not publish anything, execute artifacts, inspect registries, or assume
//! a language, package manager, or CI provider.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
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
    pub tag_object: Option<String>,
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
    /// The annotated tag object, when the release tag is annotated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag_object: Option<String>,
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
            tag_object: self.tag_object.clone(),
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
    create_manifest_with_expected(directory, identity, None)
}

pub fn create_manifest_with_expected(
    directory: &Path,
    identity: ReleaseIdentity,
    expected_path: Option<&Path>,
) -> Result<ReleaseManifest, ReleaseError> {
    let artifacts = collect_artifacts(directory)?;
    if artifacts.is_empty() {
        return Err(ReleaseError::Invalid(format!(
            "artifact directory {} contains no release artifacts",
            directory.display()
        )));
    }
    validate_expected_inventory(directory, &artifacts, expected_path)?;
    let manifest = ReleaseManifest {
        manifest_version: MANIFEST_VERSION,
        repository: identity.repository,
        release_tag: identity.release_tag,
        source_commit: identity.source_commit,
        tag_object: identity.tag_object,
        workflow_run: identity.workflow_run,
        artifacts,
    };
    write_outputs(directory, &manifest)?;
    Ok(manifest)
}

/// Verify that `repository` is checked out at exactly `tag` and `expected_commit`.
pub fn verify_source(
    repository: &Path,
    tag: &str,
    expected_commit: &str,
) -> Result<(), ReleaseError> {
    let mut run = |args: &[&str]| git(repository, args);
    verify_source_with(&mut run, tag, expected_commit)
}

/// The verification workflow, with `run_git` as the only side effect.
///
/// `run_git` receives the argument vector after `git` — for example
/// `["rev-parse", "HEAD"]` — so a test can script the answers without a
/// repository and without spawning a process.
///
/// It is `&mut dyn FnMut`, NOT `&dyn Fn`: the scripted stubs consume a queue of
/// answers, which makes them `FnMut`, and `&dyn Fn` rejects them with `E0525`
/// ("expected a closure that implements the `Fn` trait, but this closure only
/// implements `FnMut`"). Mutating closures also need a `mut` binding so the
/// reborrow as `&mut` is legal.
fn verify_source_with(
    run_git: &mut dyn FnMut(&[&str]) -> Result<String, ReleaseError>,
    tag: &str,
    expected_commit: &str,
) -> Result<(), ReleaseError> {
    if tag.is_empty() || expected_commit.is_empty() {
        return Err(ReleaseError::Invalid(
            "release source verification requires a non-empty tag and commit".to_owned(),
        ));
    }
    if tag.starts_with('-') || tag.chars().any(char::is_whitespace) || tag.contains('\0') {
        return Err(ReleaseError::Invalid(format!(
            "release tag `{tag}` is not a safe Git ref"
        )));
    }
    if expected_commit.chars().any(char::is_whitespace) || expected_commit.contains('\0') {
        return Err(ReleaseError::Invalid(
            "expected release commit is not a safe Git object name".to_owned(),
        ));
    }

    let tag_ref = format!("refs/tags/{tag}");
    run_git(&["check-ref-format", "--allow-onelevel", &tag_ref])?;
    let checkout_commit = run_git(&["rev-parse", "HEAD"])?;
    let tag_commit_ref = format!("{tag_ref}^{{commit}}");
    let tag_commit = run_git(&["rev-parse", "--verify", &tag_commit_ref])?;
    if checkout_commit != tag_commit || checkout_commit != expected_commit {
        return Err(ReleaseError::Invalid(format!(
            "release source does not match: checkout {checkout_commit}, tag {tag_commit}, expected {expected_commit}"
        )));
    }
    Ok(())
}

/// Verify a legacy `SHA256SUMS` file without requiring release metadata.
/// Public for compatibility; no in-tree caller.
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
    verify_manifest_with_expected(directory, expected, None)
}

pub fn verify_manifest_with_expected(
    directory: &Path,
    expected: ReleaseIdentity,
    expected_path: Option<&Path>,
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
    validate_expected_inventory(directory, &actual_artifacts, expected_path)?;
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
        ("tag object", &actual.tag_object, &expected.tag_object),
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

fn validate_expected_inventory(
    directory: &Path,
    artifacts: &[Artifact],
    expected_path: Option<&Path>,
) -> Result<(), ReleaseError> {
    let Some(expected_path) = expected_path else {
        return Ok(());
    };
    let text = read_to_string(expected_path)?;
    let mut expected = BTreeSet::new();
    for (line_number, line) in text.lines().enumerate() {
        let name = line.trim();
        if name.is_empty() {
            continue;
        }
        let path = Path::new(name);
        if path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, Component::ParentDir))
            || name.contains(['\n', '\r'])
        {
            return Err(ReleaseError::Invalid(format!(
                "{}:{}: invalid expected artifact name",
                expected_path.display(),
                line_number + 1
            )));
        }
        if !expected.insert(name.to_owned()) {
            return Err(ReleaseError::Invalid(format!(
                "{}:{}: duplicate expected artifact `{name}`",
                expected_path.display(),
                line_number + 1
            )));
        }
    }
    let actual = artifacts
        .iter()
        .map(|artifact| artifact.name.as_str())
        .collect::<BTreeSet<_>>();
    let expected_refs = expected.iter().map(String::as_str).collect::<BTreeSet<_>>();
    if actual != expected_refs {
        return Err(ReleaseError::Invalid(format!(
            "expected artifact inventory in {} does not match {}",
            expected_path.display(),
            directory.display()
        )));
    }
    Ok(())
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

/// Publish a generated release file through a same-directory temporary file.
///
/// Unix renames replace the destination atomically. Windows does not expose the
/// same replacement semantics through `std::fs::rename`, so the fallback removes
/// an existing destination before renaming; callers still get crash-safe
/// temporary-file cleanup, but not an atomic replacement guarantee on Windows.
fn write_file(path: &Path, contents: &str) -> Result<(), ReleaseError> {
    crate::atomic_file::write(
        path,
        contents.as_bytes(),
        crate::atomic_file::WriteMode::Replace,
    )
    .map_err(|source| ReleaseError::Write {
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
    fn replaces_existing_release_metadata_without_leaving_temporary_files() {
        let temp = TempDir::new();
        let artifact = temp.path().join("artifact");
        fs::write(&artifact, b"before").unwrap();
        create_manifest(temp.path(), ReleaseIdentity::default()).unwrap();
        let before = fs::read_to_string(temp.path().join(METADATA_FILE_NAME)).unwrap();

        fs::write(&artifact, b"after").unwrap();
        create_manifest(temp.path(), ReleaseIdentity::default()).unwrap();
        let after = fs::read_to_string(temp.path().join(METADATA_FILE_NAME)).unwrap();

        assert_ne!(before, after);
        let temporary_files = fs::read_dir(temp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
            .collect::<Vec<_>>();
        assert!(temporary_files.is_empty());
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

    /// The workflows read and write these field names, so they are a contract
    /// with `.github/workflows/release*.yml` rather than an implementation
    /// detail. A rename here has to be paired with one there.
    #[test]
    fn writes_the_field_names_the_release_workflows_read() {
        let temp = TempDir::new();
        fs::write(temp.path().join("artifact.tar.gz"), b"artifact").unwrap();
        create_manifest(
            temp.path(),
            ReleaseIdentity {
                repository: Some("owner/repo".to_owned()),
                release_tag: Some("v1.2.3".to_owned()),
                source_commit: Some("abc123".to_owned()),
                tag_object: Some("def456".to_owned()),
                workflow_run: Some("https://example.test/runs/1".to_owned()),
            },
        )
        .unwrap();

        let written: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(temp.path().join(METADATA_FILE_NAME)).unwrap(),
        )
        .unwrap();
        for field in [
            "manifest_version",
            "repository",
            "release_tag",
            "source_commit",
            "tag_object",
            "workflow_run",
            "artifacts",
        ] {
            assert!(written.get(field).is_some(), "missing {field}: {written}");
        }
        assert_eq!(
            written["manifest_version"],
            serde_json::json!(MANIFEST_VERSION)
        );
    }

    #[test]
    fn records_the_annotated_tag_object_and_verifies_it() {
        let temp = TempDir::new();
        fs::write(temp.path().join("artifact"), b"artifact").unwrap();
        let identity = ReleaseIdentity {
            release_tag: Some("v1.2.3".to_owned()),
            tag_object: Some("def456".to_owned()),
            ..ReleaseIdentity::default()
        };

        let manifest = create_manifest(temp.path(), identity.clone()).unwrap();
        assert_eq!(manifest.tag_object.as_deref(), Some("def456"));
        verify_manifest(temp.path(), identity).unwrap();

        let error = verify_manifest(
            temp.path(),
            ReleaseIdentity {
                tag_object: Some("other".to_owned()),
                ..ReleaseIdentity::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("tag object"));
    }

    /// A lightweight tag has no tag object. It must be omitted rather than
    /// written as an empty string, which the validator would compare literally.
    #[test]
    fn omits_the_tag_object_for_a_lightweight_tag() {
        let temp = TempDir::new();
        fs::write(temp.path().join("artifact"), b"artifact").unwrap();
        create_manifest(
            temp.path(),
            ReleaseIdentity {
                release_tag: Some("v1.2.3".to_owned()),
                ..ReleaseIdentity::default()
            },
        )
        .unwrap();

        let written: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(temp.path().join(METADATA_FILE_NAME)).unwrap(),
        )
        .unwrap();
        assert!(written.get("tag_object").is_none(), "{written}");

        let error = verify_manifest(
            temp.path(),
            ReleaseIdentity {
                tag_object: Some("def456".to_owned()),
                ..ReleaseIdentity::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("tag object"));
    }

    /// `verify_source_with` calls `rev-parse` twice, so a stub keyed only by
    /// subcommand is not enough; these stubs answer from a scripted queue.
    #[test]
    fn a_matching_checkout_tag_and_commit_verify() {
        let mut answers = vec!["abc123".to_owned(), "abc123".to_owned()];
        let mut git = move |args: &[&str]| -> Result<String, ReleaseError> {
            if args[0] == "check-ref-format" {
                return Ok(String::new());
            }
            Ok(answers.remove(0))
        };

        assert!(verify_source_with(&mut git, "v1.0.0", "abc123").is_ok());
    }

    #[test]
    fn a_checkout_that_does_not_match_the_tag_is_rejected() {
        let mut answers = vec!["abc123".to_owned(), "def456".to_owned()];
        let mut git = move |args: &[&str]| -> Result<String, ReleaseError> {
            if args[0] == "check-ref-format" {
                return Ok(String::new());
            }
            Ok(answers.remove(0))
        };

        let error = verify_source_with(&mut git, "v1.0.0", "abc123").unwrap_err();

        assert!(error.to_string().contains("does not match"), "{error}");
    }

    #[test]
    fn a_checkout_that_does_not_match_the_expected_commit_is_rejected() {
        let mut answers = vec!["abc123".to_owned(), "abc123".to_owned()];
        let mut git = move |args: &[&str]| -> Result<String, ReleaseError> {
            if args[0] == "check-ref-format" {
                return Ok(String::new());
            }
            Ok(answers.remove(0))
        };

        let error = verify_source_with(&mut git, "v1.0.0", "other").unwrap_err();

        assert!(error.to_string().contains("does not match"), "{error}");
    }

    #[test]
    fn a_git_failure_propagates_as_a_command_error() {
        let mut git = |args: &[&str]| -> Result<String, ReleaseError> {
            if args[0] == "check-ref-format" {
                return Ok(String::new());
            }
            Err(ReleaseError::Command("git exploded".to_owned()))
        };

        let error = verify_source_with(&mut git, "v1.0.0", "abc123").unwrap_err();

        assert!(matches!(error, ReleaseError::Command(_)), "{error}");
    }

    #[test]
    fn an_empty_or_unsafe_identity_is_rejected_before_running_git() {
        let mut git = |args: &[&str]| -> Result<String, ReleaseError> {
            panic!("git must not run: {args:?}")
        };

        for (tag, commit) in [
            ("", "abc123"),
            ("v1.0.0", ""),
            ("--force", "abc123"),
            ("v1 0 0", "abc123"),
            ("v1.0.0", "abc 123"),
        ] {
            let error = verify_source_with(&mut git, tag, commit).unwrap_err();
            assert!(
                matches!(error, ReleaseError::Invalid(_)),
                "{tag}/{commit}: {error}"
            );
        }
    }

    fn sha256_of(contents: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(contents);
        format!("{:x}", hasher.finalize())
    }

    #[test]
    fn verify_checksums_accepts_a_matching_sums_file() {
        let temp = TempDir::new();
        fs::write(temp.path().join("app.tar.gz"), b"app").unwrap();
        fs::write(
            temp.path().join(CHECKSUMS_FILE_NAME),
            format!("{}  app.tar.gz\n", sha256_of(b"app")),
        )
        .unwrap();

        let count = verify_checksums(temp.path()).expect("checksums verify");

        assert_eq!(count, 1);
    }

    #[test]
    fn verify_checksums_reports_a_mismatch() {
        let temp = TempDir::new();
        fs::write(temp.path().join("app.tar.gz"), b"after").unwrap();
        fs::write(
            temp.path().join(CHECKSUMS_FILE_NAME),
            format!("{}  app.tar.gz\n", sha256_of(b"before")),
        )
        .unwrap();

        let error = verify_checksums(temp.path()).expect_err("a mismatch fails");

        assert!(error.to_string().contains("checksum mismatch"), "{error}");
    }

    #[test]
    fn verify_checksums_rejects_a_non_regular_entry() {
        let temp = TempDir::new();
        fs::create_dir(temp.path().join("directory")).unwrap();
        fs::write(
            temp.path().join(CHECKSUMS_FILE_NAME),
            format!("{}  directory\n", sha256_of(b"")),
        )
        .unwrap();

        let error = verify_checksums(temp.path()).expect_err("a directory is not an artifact");

        assert!(error.to_string().contains("not a regular file"), "{error}");
    }

    #[test]
    fn malformed_sums_files_are_rejected_with_their_line_number() {
        let temp = TempDir::new();
        fs::write(temp.path().join("artifact"), b"data").unwrap();

        let cases = [
            ("no separator\n", "expected `<sha256>  <file>`"),
            ("abc  artifact\n", "invalid SHA-256 digest"),
            ("   \n", "lists no artifacts"),
            (
                "0000000000000000000000000000000000000000000000000000000000000000  ../escape\n",
                "invalid artifact name",
            ),
            (
                "0000000000000000000000000000000000000000000000000000000000000000  artifact\n0000000000000000000000000000000000000000000000000000000000000000  artifact\n",
                "duplicate artifact name",
            ),
        ];

        for (contents, expected_message) in cases {
            fs::write(temp.path().join(CHECKSUMS_FILE_NAME), contents).unwrap();
            let error = verify_checksums(temp.path()).expect_err(contents);
            assert!(
                error.to_string().contains(expected_message),
                "{contents:?}: {error}"
            );
        }
    }
}
