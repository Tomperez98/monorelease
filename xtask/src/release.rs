//! Release coordination that must happen before publishing.
//!
//! The workflow provides credentials and GitHub Pages actions. This module owns
//! the repository-local release boundary: stamp the release version, build the
//! versioned documentation, verify its output, and restore the committed
//! placeholders before returning.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use flate2::Compression;
use flate2::write::GzEncoder;
use mono::Version;
use serde_json::json;
use tar::Builder;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

use crate::Error;
use crate::release_contract;
use crate::release_model::{
    ArchiveKind, DocsIdentity, ManifestState, POWERSHELL_INSTALLER_FILE, PublicationAction,
    PublicationIntent, PublicationState, ReleaseContext, ReleaseTarget, SHELL_INSTALLER_FILE,
    VersionContractInput, artifact_inventory, checksum_table, compose_release_notes,
    installer_digests, powershell_installer, powershell_installer_version_marker,
    publication_transition, release_context, release_plan, shell_installer,
    shell_installer_version_marker, version_contract,
};
use crate::stamp;

pub(crate) const COMPONENT: &str = "release-docs";
pub(crate) const PREPARE_COMPONENT: &str = "release-prepare";
pub(crate) const PUBLISH_COMPONENT: &str = "release-publish";
pub(crate) const BUILD_COMPONENT: &str = "release-build";
pub(crate) const CHECK_COMPONENT: &str = "release-version-check";
pub(crate) const STATE_COMPONENT: &str = "release-check";
pub(crate) const VALIDATE_COMPONENT: &str = "release-validate";
const DOCS_METADATA_FILE: &str = "release.json";
const DOCS_METADATA_SCHEMA: u32 = 1;
const ZENSICAL_VERSION: &str = "0.0.61";

/// Run the repository-local release gates from one stamped checkout. The
/// workflow supplies the toolchain; this coordinator owns source validation,
/// changelog notes, CI, release gates, packaging, and restoration.
pub(crate) fn prepare(root: &Path, tag: &str, version: Version) -> Result<(), Error> {
    check(root)?;
    let commit = git_output(root, &["rev-parse", "HEAD"])?;
    let context = release_context(tag, commit.clone(), None, None)?;
    if context.version != version {
        return Err(Error::Invalid(format!(
            "release tag {tag} does not match requested version {version}"
        )));
    }
    context.validate_changelog(root)?;
    run_command(
        root,
        "cargo",
        [
            "run", "--locked", "--quiet", "--", "release", "source", "--tag", tag, "--commit",
            &commit,
        ],
    )?;
    stamp::apply(root, version)?;

    let result = (|| {
        run_command(
            root,
            "cargo",
            [
                "run",
                "--locked",
                "--",
                "ci",
                "--no-cache",
                "--output",
                "text",
                "--ui",
                "stream",
            ],
        )?;
        run_command(
            root,
            "cargo",
            [
                "run",
                "--locked",
                "--",
                "run",
                "release",
                "--no-cache",
                "--output",
                "text",
                "--ui",
                "stream",
            ],
        )?;
        run_command(root, "cargo", ["package", "--locked", "--allow-dirty"])?;
        verify_packaged_source(root, version)?;
        if !root.join("RELEASE_NOTES.md").is_file() {
            return Err(Error::Invalid(
                "release gates completed without producing RELEASE_NOTES.md".to_owned(),
            ));
        }
        Ok(())
    })();

    let restore = stamp::restore(root);
    match (result, restore) {
        (Ok(()), Ok(())) => {
            println!("{PREPARE_COMPONENT}: release gates and package passed for {tag}");
            Ok(())
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(restore)) => Err(Error::Command(format!(
            "release preparation failed: {error}; restoring pinned files also failed: {restore}"
        ))),
    }
}

/// Build one canonical release artifact from the release target table.
pub(crate) fn build(
    root: &Path,
    tag: &crate::Tag,
    target_name: &str,
    directory: &Path,
) -> Result<(), Error> {
    let target = ReleaseTarget::find(target_name)?;
    let commit = git_output(root, &["rev-parse", "HEAD"])?;
    mono::verify_source(root, &tag.name, &commit)
        .map_err(|error| Error::Command(error.to_string()))?;
    let context = release_context(&tag.name, commit, None, None)?;
    context.validate_changelog(root)?;
    stamp::apply(root, tag.version)?;

    let result = (|| {
        verify_stamped_manifests(root, &context)?;
        run_command_slice(
            root,
            "cargo",
            &[
                "build",
                "--locked",
                "--release",
                "--target",
                target.rust_target,
            ],
        )?;
        let binary = root
            .join("target")
            .join(target.rust_target)
            .join("release")
            .join(target.binary_name());
        assert_binary_identity(&binary, tag.version)?;
        package_artifact(root, &binary, target, &context, directory)
    })();
    let restore = stamp::restore(root);

    match (result, restore) {
        (Ok(()), Ok(())) => {
            println!(
                "{BUILD_COMPONENT}: built {} for {}",
                target.artifact_name(&context.tag),
                target.rust_target
            );
            Ok(())
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(restore)) => Err(Error::Command(format!(
            "release build failed: {error}; restoring pinned files also failed: {restore}"
        ))),
    }
}

/// Verify the committed repository state before any release stamping occurs.
pub(crate) fn check(root: &Path) -> Result<(), Error> {
    let cargo = fs::read_to_string(root.join("Cargo.toml")).map_err(|source| Error::Io {
        path: root.join("Cargo.toml"),
        source,
    })?;
    if !cargo.contains("version = \"0.0.0\"") {
        return Err(Error::Invalid(
            "Cargo.toml is not pinned at version 0.0.0".to_owned(),
        ));
    }
    let lock = fs::read_to_string(root.join("Cargo.lock")).map_err(|source| Error::Io {
        path: root.join("Cargo.lock"),
        source,
    })?;
    if lockfile_package_version(&lock, "mono").as_deref() != Some("0.0.0") {
        return Err(Error::Invalid(
            "Cargo.lock mono package is not pinned at version 0.0.0".to_owned(),
        ));
    }
    let zensical = fs::read_to_string(root.join("zensical.toml")).map_err(|source| Error::Io {
        path: root.join("zensical.toml"),
        source,
    })?;
    if !zensical.contains("Mono v0.0.0") {
        return Err(Error::Invalid(
            "zensical.toml is not pinned at version 0.0.0".to_owned(),
        ));
    }
    let pinned_context = release_context("v0.0.0", "pinned-check", None, None)?;
    let plan = release_plan(pinned_context)?;
    if plan.targets.is_empty() || plan.artifact_names.is_empty() {
        return Err(Error::Invalid(
            "release plan contains no targets or artifacts".to_owned(),
        ));
    }
    for relative in ["Cargo.toml", "Cargo.lock", "zensical.toml"] {
        if stamp_backup_exists(root, relative) {
            return Err(Error::Invalid(format!(
                "{relative}.backup exists; restore the incomplete release stamp"
            )));
        }
    }
    check_installers(root)?;
    println!("{STATE_COMPONENT}: pinned manifests, release targets, and installers are valid");
    Ok(())
}

/// Tag used to prove the checked-in installers still render. No release can
/// carry it, so a rendered marker built from it cannot be mistaken for a real one.
const PINNED_VERSION_TAG: &str = "v0.0.0";

/// Prove the checked-in installers still render before a release needs them.
///
/// `release-docs` rewrites the published-defaults region of each script a few
/// hundred lines into the release job, after the artifacts are already built, so
/// a script that lost its region would fail a release rather than a pull
/// request. Rendering here with placeholder values moves that failure to the
/// pre-tag check, which is where `release-check` already verifies that the
/// committed state is releasable.
fn check_installers(root: &Path) -> Result<(), Error> {
    let shell = read_installer(root, SHELL_INSTALLER_FILE)?;
    shell_installer(&shell, PINNED_VERSION_TAG, "target=digest")
        .map_err(|error| Error::Invalid(format!("{SHELL_INSTALLER_FILE}: {error}")))?;

    let powershell = read_installer(root, POWERSHELL_INSTALLER_FILE)?;
    let rendered = powershell_installer(&powershell, PINNED_VERSION_TAG, "target=digest")
        .map_err(|error| Error::Invalid(format!("{POWERSHELL_INSTALLER_FILE}: {error}")))?;
    // PowerShell rejects a `param()` block that is not the first statement, so a
    // defaults region placed above it would break every Windows install.
    let parameters = rendered.find("param(").ok_or_else(|| {
        Error::Invalid(format!("{POWERSHELL_INSTALLER_FILE} has no param() block"))
    })?;
    let defaults = rendered
        .find(&powershell_installer_version_marker(PINNED_VERSION_TAG))
        .expect("a successful render inserts the version marker");
    if defaults < parameters {
        return Err(Error::Invalid(format!(
            "{POWERSHELL_INSTALLER_FILE} generates its defaults above the param() block"
        )));
    }
    Ok(())
}

fn stamp_backup_exists(root: &Path, relative: &str) -> bool {
    root.join(format!("{relative}.backup")).exists()
}

/// Verify all user-visible release version sources against one tag.
pub(crate) fn version_check(
    root: &Path,
    tag: &crate::Tag,
    binary: Option<&Path>,
    site: &Path,
    notes: &Path,
) -> Result<(), Error> {
    let context = release_context(
        &tag.name,
        git_output(root, &["rev-parse", "HEAD"])?,
        None,
        None,
    )?;
    context.validate_changelog(root)?;

    let expected = context.version.to_string();
    let notes_text = fs::read_to_string(notes).map_err(|source| Error::Io {
        path: notes.to_path_buf(),
        source,
    })?;
    let cargo = fs::read_to_string(root.join("Cargo.toml")).map_err(|source| Error::Io {
        path: root.join("Cargo.toml"),
        source,
    })?;
    let lock = fs::read_to_string(root.join("Cargo.lock")).map_err(|source| Error::Io {
        path: root.join("Cargo.lock"),
        source,
    })?;
    let zensical = fs::read_to_string(root.join("zensical.toml")).map_err(|source| Error::Io {
        path: root.join("zensical.toml"),
        source,
    })?;
    let docs = read_docs_metadata(site)?;
    let cli_version = binary.map(read_binary_version).transpose()?;
    version_contract(
        &context,
        VersionContractInput {
            notes: &notes_text,
            cargo_toml: &cargo,
            cargo_lock: &lock,
            zensical: &zensical,
            cli_version: cli_version.as_deref(),
            docs: Some(&docs),
        },
        ManifestState::Pinned,
    )
    .map_err(|error| Error::Invalid(error.to_string()))?;
    verify_installers(site, &tag.name)?;
    println!("{CHECK_COMPONENT}: all release versions agree on {expected}");
    Ok(())
}

/// Verify the installers published with the documentation serve this release.
///
/// A published installer that names another tag would pin its downloads to a
/// release the reader did not ask for, so this runs before the Pages deploy.
fn verify_installers(site: &Path, tag: &str) -> Result<(), Error> {
    for (name, marker) in [
        (SHELL_INSTALLER_FILE, shell_installer_version_marker(tag)),
        (
            POWERSHELL_INSTALLER_FILE,
            powershell_installer_version_marker(tag),
        ),
    ] {
        let path = site.join(name);
        let text = fs::read_to_string(&path).map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        if !text.contains(&marker) {
            return Err(Error::Invalid(format!(
                "{} does not name {tag}; it was not rendered for this release",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Build the release-tagged documentation without leaving release versions in
/// the checkout. The caller must provide a parsed release version; parsing
/// belongs at the CLI boundary.
pub(crate) fn docs(root: &Path, version: Version, directory: &Path) -> Result<(), Error> {
    stamp::apply(root, version)?;

    let tag = release_tag(version);
    let build_result = build_site(root, version)
        .and_then(|()| write_docs_metadata(root, &tag, version))
        .and_then(|()| write_installers(root, &tag, directory));
    let restore_result = stamp::restore(root);

    match (build_result, restore_result) {
        (Ok(()), Ok(())) => {
            println!("{COMPONENT}: built versioned documentation in site/");
            Ok(())
        }
        (Err(build), Ok(())) => Err(build),
        (Ok(()), Err(restore)) => Err(restore),
        (Err(build), Err(restore)) => Err(Error::Command(format!(
            "documentation build failed: {build}; restoring pinned files also failed: {restore}"
        ))),
    }
}

/// Create or reuse the draft GitHub release and upload the assembled release
/// directory. A separate `finalize` invocation publishes it after Pages has
/// deployed the documentation.
pub(crate) fn publish(
    tag: &str,
    directory: &Path,
    notes: &Path,
    finalize: bool,
) -> Result<(), Error> {
    let repository = required_env("GITHUB_REPOSITORY")?;
    // The draft and the final publication must carry the same body, so both are
    // composed from the same checked changelog notes.
    let composed = ComposedNotes::write(tag, notes)?;
    let notes = composed.path();
    println!("{PUBLISH_COMPONENT}: composed the {tag} release body with the install section");
    if finalize {
        let state = publication_state(tag, &repository)?;
        let transition = publication_transition(state, PublicationIntent::Finalize)
            .map_err(|error| Error::Invalid(error.to_string()))?;
        if transition
            .actions
            .contains(&PublicationAction::AlreadyPublished)
        {
            println!("{PUBLISH_COMPONENT}: {tag} is already published");
            return Ok(());
        }
        run_gh([
            "release",
            "edit",
            tag,
            "--repo",
            &repository,
            "--title",
            tag,
            "--notes-file",
            notes.to_str().ok_or_else(|| {
                Error::Invalid(format!(
                    "release notes path is not valid UTF-8: {}",
                    notes.display()
                ))
            })?,
            "--draft=false",
            "--latest",
        ])?;
        println!("{PUBLISH_COMPONENT}: published {tag}");
        return Ok(());
    }

    let state = create_or_reuse_draft(tag, &repository, notes)?;
    if state == PublicationState::Published {
        return Ok(());
    }
    let artifacts = artifact_paths(directory)?;
    let artifact_args = artifacts
        .iter()
        .map(|path| {
            path.to_str().ok_or_else(|| {
                Error::Invalid(format!(
                    "release artifact path is not valid UTF-8: {}",
                    path.display()
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut upload_args = vec!["release", "upload", tag];
    upload_args.extend(artifact_args.iter().copied());
    upload_args.extend(["--repo", &repository, "--clobber"]);
    run_gh(upload_args)?;
    println!(
        "{PUBLISH_COMPONENT}: uploaded {} artifacts to draft {tag}",
        artifacts.len()
    );
    Ok(())
}

/// The release body: checked changelog notes plus the generated install section.
///
/// `gh --notes-file` reads a path, so the composed body is written to a
/// temporary file instead of being passed as one enormous argument. The file is
/// removed when this value is dropped, including on the early returns above it.
struct ComposedNotes {
    directory: PathBuf,
    path: PathBuf,
}

impl ComposedNotes {
    fn write(tag: &str, notes: &Path) -> Result<Self, Error> {
        let text = fs::read_to_string(notes).map_err(|source| Error::Io {
            path: notes.to_path_buf(),
            source,
        })?;
        let composed = compose_release_notes(tag, &text)?;
        let directory = temporary_directory("notes")?;
        let path = directory.join("RELEASE_NOTES.md");
        fs::write(&path, composed).map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        Ok(Self { directory, path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ComposedNotes {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn verify_stamped_manifests(root: &Path, context: &ReleaseContext) -> Result<(), Error> {
    let cargo = fs::read_to_string(root.join("Cargo.toml")).map_err(|source| Error::Io {
        path: root.join("Cargo.toml"),
        source,
    })?;
    let lock = fs::read_to_string(root.join("Cargo.lock")).map_err(|source| Error::Io {
        path: root.join("Cargo.lock"),
        source,
    })?;
    let zensical = fs::read_to_string(root.join("zensical.toml")).map_err(|source| Error::Io {
        path: root.join("zensical.toml"),
        source,
    })?;
    let notes = format!("# {}\n", context.version);
    version_contract(
        context,
        VersionContractInput {
            notes: &notes,
            cargo_toml: &cargo,
            cargo_lock: &lock,
            zensical: &zensical,
            cli_version: None,
            docs: None,
        },
        ManifestState::Stamped,
    )
    .map_err(|error| Error::Invalid(error.to_string()))
}

fn verify_packaged_source(root: &Path, version: Version) -> Result<(), Error> {
    let package_root = root.join("target").join("package");
    let expected = format!("mono-{version}");
    let package = fs::read_dir(&package_root)
        .map_err(|source| Error::Io {
            path: package_root.clone(),
            source,
        })?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.file_name().and_then(|name| name.to_str()) == Some(expected.as_str()))
        .ok_or_else(|| {
            Error::Invalid(format!(
                "cargo package did not create target/package/{expected}"
            ))
        })?;
    let manifest = package.join("Cargo.toml");
    let text = fs::read_to_string(&manifest).map_err(|source| Error::Io {
        path: manifest.clone(),
        source,
    })?;
    if !text.contains(&format!("version = \"{version}\"")) {
        return Err(Error::Invalid(format!(
            "packaged Cargo.toml is not versioned {version}"
        )));
    }
    Ok(())
}

fn package_artifact(
    root: &Path,
    binary: &Path,
    target: ReleaseTarget,
    context: &ReleaseContext,
    directory: &Path,
) -> Result<(), Error> {
    fs::create_dir_all(directory).map_err(|source| Error::Io {
        path: directory.to_path_buf(),
        source,
    })?;
    let archive_path = directory.join(target.artifact_name(&context.tag));
    if archive_path.exists() {
        fs::remove_file(&archive_path).map_err(|source| Error::Io {
            path: archive_path.clone(),
            source,
        })?;
    }
    match target.archive {
        ArchiveKind::TarGz => {
            let file = fs::File::create(&archive_path).map_err(|source| Error::Io {
                path: archive_path.clone(),
                source,
            })?;
            let encoder = GzEncoder::new(file, Compression::default());
            let mut archive = Builder::new(encoder);
            archive
                .append_path_with_name(binary, target.binary_name())
                .map_err(|source| Error::Io {
                    path: binary.to_path_buf(),
                    source,
                })?;
            archive
                .append_path_with_name(root.join("LICENSE"), "LICENSE")
                .map_err(|source| Error::Io {
                    path: root.join("LICENSE"),
                    source,
                })?;
            archive
                .append_path_with_name(root.join("README.md"), "README.md")
                .map_err(|source| Error::Io {
                    path: root.join("README.md"),
                    source,
                })?;
            let encoder = archive.into_inner().map_err(|source| Error::Io {
                path: archive_path.clone(),
                source,
            })?;
            encoder.finish().map_err(|source| Error::Io {
                path: archive_path.clone(),
                source,
            })?;
        }
        ArchiveKind::Zip => {
            let file = fs::File::create(&archive_path).map_err(|source| Error::Io {
                path: archive_path.clone(),
                source,
            })?;
            let mut archive = ZipWriter::new(file);
            let options = SimpleFileOptions::default();
            for (path, name) in [
                (binary.to_path_buf(), target.binary_name()),
                (root.join("LICENSE"), "LICENSE"),
                (root.join("README.md"), "README.md"),
            ] {
                archive.start_file(name, options).map_err(|source| {
                    Error::Command(format!(
                        "failed to add {name} to {}: {source}",
                        archive_path.display()
                    ))
                })?;
                let mut input = fs::File::open(&path).map_err(|source| Error::Io {
                    path: path.clone(),
                    source,
                })?;
                std::io::copy(&mut input, &mut archive)
                    .map_err(|source| Error::Io { path, source })?;
            }
            archive.finish().map_err(|source| {
                Error::Command(format!(
                    "failed to finish {}: {source}",
                    archive_path.display()
                ))
            })?;
        }
    }
    Ok(())
}

fn read_binary_version(binary: &Path) -> Result<String, Error> {
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .map_err(|source| Error::Spawn {
            program: format!("{} --version", binary.display()),
            source,
        })?;
    let reported = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !output.status.success() {
        return Err(Error::Command(format!(
            "{} --version failed with {}{}",
            binary.display(),
            output.status,
            diagnostics(&output)
        )));
    }
    Ok(reported)
}

fn assert_binary_identity(binary: &Path, version: Version) -> Result<(), Error> {
    let reported = read_binary_version(binary)?;
    let expected = format!("mono {version}");
    if reported != expected {
        return Err(Error::Command(format!(
            "{} reports `{reported}`, expected `{expected}`",
            binary.display()
        )));
    }
    Ok(())
}

fn lockfile_package_version(text: &str, package: &str) -> Option<String> {
    let mut current = None;
    for line in text.lines() {
        if line == "[[package]]" {
            current = None;
        } else if let Some(name) = line.strip_prefix("name = ") {
            current = Some(name.trim_matches('"').to_owned());
        } else if current.as_deref() == Some(package)
            && let Some(version) = line.strip_prefix("version = ")
        {
            return Some(version.trim_matches('"').to_owned());
        }
    }
    None
}

/// The tag a documentation build belongs to: CI exports it, a local run derives it.
fn release_tag(version: Version) -> String {
    non_empty_env("RELEASE_TAG").unwrap_or_else(|| format!("v{version}"))
}

/// Publish the installers beside the documentation, pinned to this release.
///
/// The digests come from the release directory's `SHA256SUMS`, which the release
/// contract has already written and verified, so the published installer names
/// exactly the bytes that were uploaded. No release asset changes: the
/// documentation site is the only thing this touches.
fn write_installers(root: &Path, tag: &str, directory: &Path) -> Result<(), Error> {
    let checksums_path = directory.join("SHA256SUMS");
    let text = fs::read_to_string(&checksums_path).map_err(|source| Error::Io {
        path: checksums_path,
        source,
    })?;
    let digests = installer_digests(tag, &checksum_table(&text)?)?;

    let site = root.join("site");
    let source = read_installer(root, SHELL_INSTALLER_FILE)?;
    write_site_file(
        &site,
        SHELL_INSTALLER_FILE,
        shell_installer(&source, tag, &digests)?,
    )?;

    let source = read_installer(root, POWERSHELL_INSTALLER_FILE)?;
    write_site_file(
        &site,
        POWERSHELL_INSTALLER_FILE,
        powershell_installer(&source, tag, &digests)?,
    )?;
    Ok(())
}

fn read_installer(root: &Path, name: &str) -> Result<String, Error> {
    let path = root.join(name);
    fs::read_to_string(&path).map_err(|source| Error::Io { path, source })
}

fn write_site_file(site: &Path, name: &str, contents: String) -> Result<(), Error> {
    let path = site.join(name);
    fs::write(&path, contents).map_err(|source| Error::Io { path, source })
}

fn write_docs_metadata(root: &Path, tag: &str, version: Version) -> Result<(), Error> {
    let source_commit =
        non_empty_env("RELEASE_COMMIT").unwrap_or(git_output(root, &["rev-parse", "HEAD"])?);
    let metadata = json!({
        "schema_version": DOCS_METADATA_SCHEMA,
        "version": version.to_string(),
        "tag": tag,
        "source_commit": source_commit,
        "repository": non_empty_env("GITHUB_REPOSITORY"),
        "workflow_run": non_empty_env("RELEASE_RUN_URL"),
    });
    let path = root.join("site").join(DOCS_METADATA_FILE);
    fs::write(
        &path,
        serde_json::to_vec_pretty(&metadata).expect("release metadata is serializable"),
    )
    .map_err(|source| Error::Io { path, source })
}

fn read_docs_metadata(site: &Path) -> Result<DocsIdentity, Error> {
    let path = site.join(DOCS_METADATA_FILE);
    let text = fs::read_to_string(&path).map_err(|source| Error::Io {
        path: path.clone(),
        source,
    })?;
    parse_docs_metadata(&text, &path.display().to_string())
}

fn parse_docs_metadata(text: &str, label: &str) -> Result<DocsIdentity, Error> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|error| Error::Invalid(format!("{label} is invalid JSON: {error}")))?;
    let schema_version = value
        .get("schema_version")
        .and_then(|value| value.as_u64())
        .ok_or_else(|| Error::Invalid(format!("{label} has no schema_version")))?;
    let schema_version = u32::try_from(schema_version)
        .map_err(|_| Error::Invalid(format!("{label} has an invalid schema_version")))?;
    let version = value
        .get("version")
        .and_then(|value| value.as_str())
        .ok_or_else(|| Error::Invalid(format!("{label} has no version")))?;
    let tag = value
        .get("tag")
        .and_then(|value| value.as_str())
        .ok_or_else(|| Error::Invalid(format!("{label} has no tag")))?;
    let source_commit = value
        .get("source_commit")
        .and_then(|value| value.as_str())
        .ok_or_else(|| Error::Invalid(format!("{label} has no source_commit")))?;
    Ok(DocsIdentity {
        schema_version: schema_version as u32,
        version: version.to_owned(),
        tag: tag.to_owned(),
        source_commit: source_commit.to_owned(),
    })
}

pub(crate) fn validate_published(root: &Path) -> Result<(), Error> {
    let tag = required_env("RELEASE_TAG")?;
    let repository = required_env("GITHUB_REPOSITORY")?;
    let directory = temporary_directory("published")?;
    let source_root = temporary_directory("published-source")?;
    let result = (|| {
        let mut patterns = vec!["SHA256SUMS", "BUILD-METADATA.json"];
        let names = artifact_inventory(&tag)?;
        patterns.extend(names.iter().map(String::as_str));
        gh_download(&tag, &repository, &directory, &patterns)?;

        let metadata_path = directory.join("BUILD-METADATA.json");
        let metadata_text = fs::read_to_string(&metadata_path).map_err(|source| Error::Io {
            path: metadata_path.clone(),
            source,
        })?;
        let manifest: mono::ReleaseManifest =
            serde_json::from_str(&metadata_text).map_err(|error| {
                Error::Invalid(format!(
                    "{} is invalid JSON: {error}",
                    metadata_path.display()
                ))
            })?;
        let source_commit = manifest
            .source_commit
            .clone()
            .ok_or_else(|| Error::Invalid("BUILD-METADATA.json has no source_commit".to_owned()))?;
        if manifest.release_tag.as_deref() != Some(tag.as_str()) {
            return Err(Error::Invalid(format!(
                "published metadata tag does not match {tag}"
            )));
        }
        let context = release_context(&tag, source_commit.clone(), Some(&repository), None)?;

        let source_path = source_root
            .to_str()
            .ok_or_else(|| Error::Invalid("non-UTF8 source path".to_owned()))?;
        run_command_slice(
            root,
            "gh",
            &[
                "repo",
                "clone",
                &repository,
                source_path,
                "--",
                "--branch",
                &tag,
                "--depth",
                "1",
            ],
        )?;
        mono::verify_source(&source_root, &tag, &source_commit)
            .map_err(|error| Error::Command(error.to_string()))?;

        release_contract::run_with_identity(
            &directory,
            true,
            mono::ReleaseIdentity {
                repository: Some(repository.clone()),
                release_tag: Some(tag.clone()),
                source_commit: Some(source_commit),
                tag_object: None,
                workflow_run: None,
            },
        )?;
        for target in ReleaseTarget::all() {
            let artifact = directory.join(target.artifact_name(&tag));
            run_gh([
                "attestation",
                "verify",
                artifact
                    .to_str()
                    .ok_or_else(|| Error::Invalid("non-UTF8 artifact path".to_owned()))?,
                "--repo",
                &repository,
            ])?;
        }
        let linux = ReleaseTarget::find("x86_64-unknown-linux-gnu")?;
        let extracted = temporary_directory("published-linux")?;
        extract_archive(
            &directory.join(linux.artifact_name(&tag)),
            linux,
            &extracted,
        )?;
        rebuild_and_compare(&source_root, &context, &extracted.join("mono"))?;
        verify_published_docs(&context, &repository)?;
        let _ = fs::remove_dir_all(&extracted);
        Ok(())
    })();
    let _ = fs::remove_dir_all(&directory);
    let _ = fs::remove_dir_all(&source_root);
    result
}

pub(crate) fn validate_platform(root: &Path, target_name: &str) -> Result<(), Error> {
    let context = ReleaseContext::from_environment(root)?;
    let repository = context
        .repository
        .clone()
        .ok_or_else(|| Error::Invalid("GITHUB_REPOSITORY is not set".to_owned()))?;
    let target = ReleaseTarget::find(target_name)?;
    let directory = temporary_directory("platform")?;
    let result = (|| {
        let artifact_name = target.artifact_name(&context.tag);
        gh_download(&context.tag, &repository, &directory, &[&artifact_name])?;
        let extracted = directory.join("extracted");
        fs::create_dir_all(&extracted).map_err(|source| Error::Io {
            path: extracted.clone(),
            source,
        })?;
        extract_archive(&directory.join(&artifact_name), target, &extracted)?;
        for required in ["LICENSE", "README.md"] {
            if !extracted.join(required).is_file() {
                return Err(Error::Invalid(format!("missing extracted/{required}")));
            }
        }
        assert_binary_identity(&extracted.join(target.binary_name()), context.version)?;
        println!("{VALIDATE_COMPONENT}: validated {artifact_name}");
        Ok(())
    })();
    let _ = fs::remove_dir_all(&directory);
    result
}

fn rebuild_and_compare(
    root: &Path,
    context: &ReleaseContext,
    released: &Path,
) -> Result<(), Error> {
    let target = ReleaseTarget::find("x86_64-unknown-linux-gnu")?;
    stamp::apply(root, context.version)?;
    let result = (|| {
        run_command_slice(
            root,
            "cargo",
            &[
                "build",
                "--locked",
                "--release",
                "--target",
                target.rust_target,
            ],
        )?;
        let rebuilt = root
            .join("target")
            .join(target.rust_target)
            .join("release")
            .join("mono");
        assert_binary_identity(&rebuilt, context.version)?;
        let actual = fs::read(&rebuilt).map_err(|source| Error::Io {
            path: rebuilt.clone(),
            source,
        })?;
        let expected = fs::read(released).map_err(|source| Error::Io {
            path: released.to_path_buf(),
            source,
        })?;
        if actual != expected {
            return Err(Error::Invalid(
                "released Linux binary differs from a tagged-source rebuild".to_owned(),
            ));
        }
        Ok(())
    })();
    let restore = stamp::restore(root);
    match (result, restore) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(restore)) => Err(Error::Command(format!(
            "rebuild failed: {error}; restore failed: {restore}"
        ))),
    }
}

fn extract_archive(path: &Path, target: ReleaseTarget, destination: &Path) -> Result<(), Error> {
    match target.archive {
        ArchiveKind::TarGz => {
            let file = fs::File::open(path).map_err(|source| Error::Io {
                path: path.to_path_buf(),
                source,
            })?;
            let decoder = flate2::read::GzDecoder::new(file);
            let mut archive = tar::Archive::new(decoder);
            archive.unpack(destination).map_err(|source| Error::Io {
                path: destination.to_path_buf(),
                source,
            })?;
        }
        ArchiveKind::Zip => {
            let file = fs::File::open(path).map_err(|source| Error::Io {
                path: path.to_path_buf(),
                source,
            })?;
            let mut archive = zip::ZipArchive::new(file).map_err(|error| {
                Error::Invalid(format!("{} is invalid zip: {error}", path.display()))
            })?;
            for index in 0..archive.len() {
                let mut entry = archive
                    .by_index(index)
                    .map_err(|error| Error::Invalid(error.to_string()))?;
                let output = destination.join(entry.name());
                if !output.starts_with(destination) {
                    return Err(Error::Invalid(format!(
                        "archive entry escapes {}",
                        destination.display()
                    )));
                }
                if entry.is_dir() {
                    fs::create_dir_all(&output).map_err(|source| Error::Io {
                        path: output,
                        source,
                    })?;
                } else {
                    if let Some(parent) = output.parent() {
                        fs::create_dir_all(parent).map_err(|source| Error::Io {
                            path: parent.to_path_buf(),
                            source,
                        })?;
                    }
                    let mut output_file =
                        fs::File::create(&output).map_err(|source| Error::Io {
                            path: output.clone(),
                            source,
                        })?;
                    std::io::copy(&mut entry, &mut output_file).map_err(|source| Error::Io {
                        path: output,
                        source,
                    })?;
                }
            }
        }
    }
    Ok(())
}

fn verify_published_docs(context: &ReleaseContext, repository: &str) -> Result<(), Error> {
    let pages = gh_output([
        "api",
        &format!("repos/{repository}/pages"),
        "--jq",
        ".html_url",
    ])?;
    let base = pages.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err(Error::Invalid("GitHub Pages URL is empty".to_owned()));
    }
    let text = http_output(&format!("{base}/{DOCS_METADATA_FILE}"))?;
    let docs = parse_docs_metadata(&text, "published docs metadata")?;
    // The deployed installer is the artifact users pipe into a shell, so a
    // stale Pages deploy has to fail validation rather than serve anonymously.
    let installer = http_output(&format!("{base}/{SHELL_INSTALLER_FILE}"))?;
    let marker = shell_installer_version_marker(&context.tag);
    if !installer.contains(&marker) {
        return Err(Error::Invalid(format!(
            "published {SHELL_INSTALLER_FILE} does not name {}",
            context.tag
        )));
    }
    let notes = format!("# {}\n", context.version);
    let cargo_lock = "[[package]]\nname = \"mono\"\nversion = \"0.0.0\"\n";
    version_contract(
        context,
        VersionContractInput {
            notes: &notes,
            cargo_toml: "version = \"0.0.0\"\n",
            cargo_lock,
            zensical: "site_name = \"Mono v0.0.0\"",
            cli_version: None,
            docs: Some(&docs),
        },
        ManifestState::Pinned,
    )
    .map_err(|error| Error::Invalid(error.to_string()))
}

fn temporary_directory(prefix: &str) -> Result<PathBuf, Error> {
    let path = env::temp_dir().join(format!("mono-release-{prefix}-{}", std::process::id()));
    if path.exists() {
        fs::remove_dir_all(&path).map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
    }
    fs::create_dir_all(&path).map_err(|source| Error::Io {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

fn gh_download(
    tag: &str,
    repository: &str,
    directory: &Path,
    patterns: &[&str],
) -> Result<(), Error> {
    let mut args = vec!["release", "download", tag, "--repo", repository, "--dir"];
    let directory_text = directory
        .to_str()
        .ok_or_else(|| Error::Invalid("non-UTF8 download path".to_owned()))?;
    args.push(directory_text);
    args.push("--clobber");
    for pattern in patterns {
        args.push("--pattern");
        args.push(pattern);
    }
    run_gh(args)
}

fn http_output(url: &str) -> Result<String, Error> {
    let output = Command::new("curl")
        .args(["--fail", "--silent", "--show-error", url])
        .output()
        .map_err(|source| Error::Spawn {
            program: format!("curl {url}"),
            source,
        })?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(Error::Command(format!(
            "curl {url} failed with {}{}",
            output.status,
            diagnostics(&output)
        )))
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.is_empty())
}

fn run_command_slice(root: &Path, program: &str, args: &[&str]) -> Result<(), Error> {
    let status = Command::new(program)
        .args(args)
        .current_dir(root)
        .status()
        .map_err(|source| Error::Spawn {
            program: format!("{program} {}", args.join(" ")),
            source,
        })?;
    if status.success() {
        return Ok(());
    }
    Err(Error::Command(format!(
        "`{program} {}` failed with {status}",
        args.join(" ")
    )))
}

fn run_command<const N: usize>(root: &Path, program: &str, args: [&str; N]) -> Result<(), Error> {
    let status = Command::new(program)
        .args(args)
        .current_dir(root)
        .status()
        .map_err(|source| Error::Spawn {
            program: format!("{program} {}", args.join(" ")),
            source,
        })?;
    if status.success() {
        return Ok(());
    }
    Err(Error::Command(format!(
        "`{program} {}` failed with {status}",
        args.join(" ")
    )))
}

fn git_output(root: &Path, args: &[&str]) -> Result<String, Error> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|source| Error::Spawn {
            program: format!("git {}", args.join(" ")),
            source,
        })?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned());
    }
    Err(Error::Command(format!(
        "`git {}` failed with {}{}",
        args.join(" "),
        output.status,
        diagnostics(&output)
    )))
}

fn publication_state(tag: &str, repository: &str) -> Result<PublicationState, Error> {
    let view = gh_output([
        "release", "view", tag, "--repo", repository, "--json", "isDraft", "--jq", ".isDraft",
    ])?;
    match view.trim() {
        "" => Ok(PublicationState::Missing),
        "true" => Ok(PublicationState::Draft),
        "false" => Ok(PublicationState::Published),
        state => Err(Error::Invalid(format!(
            "unexpected `gh release view` state `{state}` for {tag}"
        ))),
    }
}

fn create_or_reuse_draft(
    tag: &str,
    repository: &str,
    notes: &Path,
) -> Result<PublicationState, Error> {
    let state = publication_state(tag, repository)?;
    let transition = publication_transition(state, PublicationIntent::Upload)
        .map_err(|error| Error::Invalid(error.to_string()))?;
    for action in transition.actions {
        match action {
            PublicationAction::CreateDraft => {
                let notes = notes.to_str().ok_or_else(|| {
                    Error::Invalid(format!(
                        "release notes path is not valid UTF-8: {}",
                        notes.display()
                    ))
                })?;
                run_gh([
                    "release",
                    "create",
                    tag,
                    "--repo",
                    repository,
                    "--verify-tag",
                    "--draft",
                    "--title",
                    tag,
                    "--notes-file",
                    notes,
                ])?;
                println!("{PUBLISH_COMPONENT}: created draft {tag}");
            }
            PublicationAction::ReuseDraft => {
                println!("{PUBLISH_COMPONENT}: reusing existing draft {tag}");
            }
            PublicationAction::AlreadyPublished => {
                println!(
                    "{PUBLISH_COMPONENT}: {tag} is already published; keeping its immutable assets"
                );
            }
            PublicationAction::UploadAssets | PublicationAction::Finalize => {}
        }
    }
    Ok(state)
}

fn artifact_paths(directory: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut paths = fs::read_dir(directory)
        .map_err(|source| Error::Io {
            path: directory.to_path_buf(),
            source,
        })?
        .map(|entry| {
            let entry = entry.map_err(|source| Error::Io {
                path: directory.to_path_buf(),
                source,
            })?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|source| Error::Io {
                path: path.clone(),
                source,
            })?;
            if file_type.is_file() {
                Ok(Some(path))
            } else {
                Ok(None)
            }
        })
        .collect::<Result<Vec<_>, Error>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    paths.sort();
    if paths.is_empty() {
        return Err(Error::Invalid(format!(
            "release artifact directory {} contains no files",
            directory.display()
        )));
    }
    Ok(paths)
}

fn required_env(name: &str) -> Result<String, Error> {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::Invalid(format!("{name} is not set")))
}

fn run_gh<'a>(args: impl IntoIterator<Item = &'a str>) -> Result<(), Error> {
    let args = args.into_iter().collect::<Vec<_>>();
    let output = Command::new("gh")
        .args(&args)
        .output()
        .map_err(|source| Error::Spawn {
            program: format!("gh {}", args.join(" ")),
            source,
        })?;
    if output.status.success() {
        return Ok(());
    }
    Err(Error::Command(format!(
        "`gh {}` failed with {}{}",
        args.join(" "),
        output.status,
        diagnostics(&output)
    )))
}

fn gh_output<'a>(args: impl IntoIterator<Item = &'a str>) -> Result<String, Error> {
    let args = args.into_iter().collect::<Vec<_>>();
    let output = Command::new("gh")
        .args(&args)
        .output()
        .map_err(|source| Error::Spawn {
            program: format!("gh {}", args.join(" ")),
            source,
        })?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned());
    }
    // A missing release is the normal first-run state. Treat it as an empty
    // state so the caller creates the draft; other failures are surfaced.
    if output.status.code() == Some(1) {
        return Ok(String::new());
    }
    Err(Error::Command(format!(
        "`gh {}` failed with {}{}",
        args.join(" "),
        output.status,
        diagnostics(&output)
    )))
}

fn diagnostics(output: &Output) -> String {
    let mut text = String::new();
    for stream in [&output.stdout, &output.stderr] {
        let stream = String::from_utf8_lossy(stream);
        for line in stream.lines().filter(|line| !line.trim().is_empty()) {
            text.push('\n');
            text.push_str("    ");
            text.push_str(line);
        }
    }
    text
}

fn build_site(root: &Path, version: Version) -> Result<(), Error> {
    let site = root.join("site");
    if site.exists() {
        fs::remove_dir_all(&site).map_err(|source| Error::Io { path: site, source })?;
    }
    let status = Command::new("uvx")
        .args([
            "--from",
            &format!("zensical=={ZENSICAL_VERSION}"),
            "zensical",
            "build",
            "--clean",
            "--config-file",
            "zensical.toml",
        ])
        .current_dir(root)
        .status()
        .map_err(|source| Error::Spawn {
            program: "uvx zensical build".to_owned(),
            source,
        })?;

    if !status.success() {
        return Err(Error::Command(format!(
            "`uvx --from zensical=={ZENSICAL_VERSION} zensical build` failed with {status} for {version}"
        )));
    }

    let index = root.join("site/index.html");
    if !index.is_file() {
        return Err(Error::Invalid(format!(
            "Zensical completed but did not produce {}",
            index.display()
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn artifact_paths_are_sorted_and_exclude_directories() {
        let temp = TempDir::new();
        fs::write(temp.path().join("z-last"), "last").unwrap();
        fs::write(temp.path().join("a-first"), "first").unwrap();
        fs::create_dir(temp.path().join("nested")).unwrap();

        let paths = artifact_paths(temp.path()).unwrap();
        let names = paths
            .iter()
            .map(|path| path.file_name().unwrap().to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, ["a-first", "z-last"]);
    }

    #[test]
    fn artifact_paths_reject_an_empty_directory() {
        let temp = TempDir::new();
        let error = artifact_paths(temp.path()).unwrap_err();
        assert!(error.to_string().contains("contains no files"), "{error}");
    }

    #[test]
    fn composed_notes_carry_the_install_section_and_clean_up_after_themselves() {
        let temp = TempDir::new();
        let notes = temp.path().join("RELEASE_NOTES.md");
        fs::write(&notes, "# 0.1.5\n\n## Changelog\n\nReleased: 2026-09-12\n").unwrap();

        let path = {
            let composed = ComposedNotes::write("v0.1.5", &notes).unwrap();
            let path = composed.path().to_path_buf();
            let text = fs::read_to_string(&path).unwrap();
            assert!(text.starts_with("# 0.1.5\n"), "{text}");
            assert!(text.contains("## Install"), "{text}");
            assert!(text.contains("## Changelog"), "{text}");
            path
        };

        assert!(!path.exists(), "the temporary release body is removed");
    }

    #[test]
    fn published_installers_bake_in_the_release_contract() {
        let temp = TempDir::new();
        let site = temp.path().join("site");
        fs::create_dir_all(&site).unwrap();
        let dist = temp.path().join("dist");
        fs::create_dir_all(&dist).unwrap();
        fs::write(
            temp.path().join(SHELL_INSTALLER_FILE),
            "#!/bin/sh\nset -eu\n\n#region published defaults\ndefault_version=\"\"\ndefault_checksums=\"\"\n#endregion\n",
        )
        .unwrap();
        fs::write(
            temp.path().join(POWERSHELL_INSTALLER_FILE),
            "#Requires -Version 5.1\nparam(\n    [string]$Version = \"\"\n)\n\n#region published defaults\n$DefaultVersion = \"\"\n$DefaultChecksums = \"\"\n#endregion\n",
        )
        .unwrap();

        let tag = "v0.1.5";
        let checksums = ReleaseTarget::all()
            .iter()
            .enumerate()
            .map(|(index, target)| format!("{:064x}  {}\n", index + 1, target.artifact_name(tag)))
            .collect::<String>();
        fs::write(dist.join("SHA256SUMS"), checksums).unwrap();

        write_installers(temp.path(), tag, &dist).unwrap();

        let shell = fs::read_to_string(site.join(SHELL_INSTALLER_FILE)).unwrap();
        assert!(
            shell.contains(&shell_installer_version_marker(tag)),
            "{shell}"
        );
        assert!(
            shell.contains(&format!("x86_64-unknown-linux-gnu={:064x}", 1)),
            "{shell}"
        );
        verify_installers(&site, tag).unwrap();

        let powershell = fs::read_to_string(site.join(POWERSHELL_INSTALLER_FILE)).unwrap();
        assert!(
            powershell.contains(&powershell_installer_version_marker(tag)),
            "{powershell}"
        );
        verify_installers(&site, "v0.1.6").expect_err("another tag is rejected");
    }

    #[test]
    fn publishing_installers_requires_the_verified_release_contract() {
        let temp = TempDir::new();
        fs::create_dir_all(temp.path().join("site")).unwrap();
        fs::write(
            temp.path().join(SHELL_INSTALLER_FILE),
            "#!/bin/sh\nset -eu\n",
        )
        .unwrap();

        let error = write_installers(temp.path(), "v0.1.5", &temp.path().join("absent"))
            .expect_err("an absent release directory fails");
        assert!(error.to_string().contains("failed to access"), "{error}");
    }

    #[test]
    fn composed_notes_refuse_a_release_notes_file_for_another_version() {
        let temp = TempDir::new();
        let notes = temp.path().join("RELEASE_NOTES.md");
        fs::write(&notes, "# 0.1.4\n\nbody\n").unwrap();

        let error = match ComposedNotes::write("v0.1.5", &notes) {
            Ok(_) => panic!("a mismatched heading must be rejected"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("must start with"), "{error}");
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "mono-xtask-release-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
