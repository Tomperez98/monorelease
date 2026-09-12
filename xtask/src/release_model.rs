//! Pure release-domain rules.
//!
//! This module deliberately does not publish releases, run processes, read
//! artifacts, or talk to GitHub. Adapters in `release.rs` and
//! `release_contract.rs` translate side effects into these values and execute
//! the resulting decisions.

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::fs;
use std::path::Path;

use mono::{Changelog, Heading, Version};

use crate::Error;

pub const RELEASE_TAG_ENV: &str = "RELEASE_TAG";
const PINNED_VERSION: &str = "0.0.0";
const DOCS_METADATA_SCHEMA: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveKind {
    TarGz,
    Zip,
}

impl ArchiveKind {
    pub fn extension(self) -> &'static str {
        match self {
            Self::TarGz => "tar.gz",
            Self::Zip => "zip",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReleaseTarget {
    pub rust_target: &'static str,
    pub archive: ArchiveKind,
    pub runner: &'static str,
}

impl ReleaseTarget {
    pub const fn all() -> &'static [Self] {
        &[
            Self {
                rust_target: "x86_64-unknown-linux-gnu",
                archive: ArchiveKind::TarGz,
                runner: "ubuntu-latest",
            },
            Self {
                rust_target: "aarch64-apple-darwin",
                archive: ArchiveKind::TarGz,
                runner: "macos-latest",
            },
            Self {
                rust_target: "x86_64-apple-darwin",
                archive: ArchiveKind::TarGz,
                runner: "macos-15-intel",
            },
            Self {
                rust_target: "x86_64-pc-windows-msvc",
                archive: ArchiveKind::Zip,
                runner: "windows-latest",
            },
        ]
    }

    pub fn find(name: &str) -> Result<Self, Error> {
        Self::all()
            .iter()
            .copied()
            .find(|target| target.rust_target == name)
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "unsupported release target `{name}`; expected one of {}",
                    Self::all()
                        .iter()
                        .map(|target| target.rust_target)
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
    }

    pub fn artifact_name(self, tag: &str) -> String {
        format!(
            "mono-{tag}-{}.{}",
            self.rust_target,
            self.archive.extension()
        )
    }

    pub fn binary_name(self) -> &'static str {
        if self.archive == ArchiveKind::Zip {
            "mono.exe"
        } else {
            "mono"
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseContext {
    pub version: Version,
    pub tag: String,
    pub source_commit: String,
    pub tag_object: Option<String>,
    pub repository: Option<String>,
    pub workflow_run: Option<String>,
}

/// Construct release identity without consulting the environment or Git.
pub fn release_context(
    tag: &str,
    source_commit: impl Into<String>,
    repository: Option<&str>,
    workflow_run: Option<&str>,
) -> Result<ReleaseContext, Error> {
    let source_commit = source_commit.into();
    if source_commit.is_empty() {
        return Err(Error::Invalid(
            "release context requires a non-empty source commit".to_owned(),
        ));
    }
    Ok(ReleaseContext {
        version: parse_tag(tag)?,
        tag: tag.to_owned(),
        source_commit,
        tag_object: None,
        repository: repository.map(str::to_owned),
        workflow_run: workflow_run.map(str::to_owned),
    })
}

impl ReleaseContext {
    pub fn from_environment(root: &Path) -> Result<Self, Error> {
        let tag = required_env(RELEASE_TAG_ENV)?;
        let source_commit = match non_empty_env("RELEASE_COMMIT") {
            Some(commit) => commit,
            None => git_output(root, &["rev-parse", "HEAD"])?,
        };
        let mut context = release_context(
            &tag,
            source_commit,
            non_empty_env("GITHUB_REPOSITORY").as_deref(),
            non_empty_env("RELEASE_RUN_URL").as_deref(),
        )?;
        context.tag_object = non_empty_env("RELEASE_TAG_OBJECT");
        Ok(context)
    }

    pub fn validate_changelog(&self, root: &Path) -> Result<(), Error> {
        let path = root.join("CHANGELOG.md");
        let text = fs::read_to_string(&path).map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        validate_changelog_text(&text, self.version)
            .map_err(|message| Error::Invalid(format!("{}: {message}", path.display())))
    }
}

/// Construct the immutable plan shared by building, contracts, and validation.
pub fn release_plan(context: ReleaseContext) -> Result<ReleasePlan, Error> {
    let artifact_names = artifact_inventory(&context.tag)?;
    Ok(ReleasePlan {
        context,
        targets: ReleaseTarget::all().to_vec(),
        artifact_names,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleasePlan {
    pub context: ReleaseContext,
    pub targets: Vec<ReleaseTarget>,
    pub artifact_names: Vec<String>,
}

/// Return the exact release artifact inventory without touching the filesystem.
pub fn artifact_inventory(tag: &str) -> Result<Vec<String>, Error> {
    parse_tag(tag)?;
    let mut names = ReleaseTarget::all()
        .iter()
        .map(|target| target.artifact_name(tag))
        .collect::<Vec<_>>();
    names.extend(INSTALLER_ASSETS.iter().map(|name| (*name).to_owned()));
    if names.iter().any(String::is_empty) {
        return Err(Error::Invalid(
            "release target produced an empty artifact name".to_owned(),
        ));
    }
    let mut unique = names.clone();
    unique.sort();
    unique.dedup();
    if unique.len() != names.len() {
        return Err(Error::Invalid(
            "release targets produced duplicate artifact names".to_owned(),
        ));
    }
    Ok(names)
}

/// Installer file names published as release assets.
pub const SHELL_INSTALLER_FILE: &str = "install.sh";
pub const POWERSHELL_INSTALLER_FILE: &str = "install.ps1";
pub const INSTALLER_ASSETS: [&str; 2] = [SHELL_INSTALLER_FILE, POWERSHELL_INSTALLER_FILE];

/// Region a release installer asset fills in with its release defaults.
///
/// Both installers carry this region, so the published copy differs from the
/// checked-in one only inside it. Everything else — including the PowerShell
/// `param()` block, which has to precede every statement — stays where it is.
const DEFAULTS_REGION_BEGIN: &str = "#region published defaults\n";
const DEFAULTS_REGION_END: &str = "#endregion\n";

/// Install commands for a published release body, pinned to its tag.
///
/// The commands name the tag-scoped release assets rendered with this tag and
/// this release's archive digests already inside.
pub fn install_section(tag: &str) -> Result<String, Error> {
    parse_tag(tag)?;
    Ok(format!(
        "## Install\n\
\n\
These installers are release assets pinned to `{tag}` and carry this release's\n\
archive checksums. They do not resolve the release version at run time.\n\
\n\
Linux and macOS:\n\
\n\
```bash\n\
curl -fsSL https://github.com/Tomperez98/mono/releases/download/{tag}/install.sh | sh\n\
```\n\
\n\
Windows (PowerShell):\n\
\n\
```powershell\n\
irm https://github.com/Tomperez98/mono/releases/download/{tag}/install.ps1 | iex\n\
```\n\
\n\
Either one verifies the archive against this release's embedded SHA-256 digest and\n\
installs to `~/.local/bin`. Mono keeps no state of its own, so undoing it is `rm`\n\
on the installed path.\n"
    ))
}

/// Compose a GitHub release body: the version heading, this repository's
/// generated install section, then the changelog-derived notes.
pub fn compose_release_notes(tag: &str, notes: &str) -> Result<String, Error> {
    let version = parse_tag(tag)?;
    let heading = format!("# {version}");
    // Require the heading to be a whole line: `# 0.1.5` must not match `# 0.1.50`.
    let Some(body) = notes
        .strip_prefix(&heading)
        .filter(|body| body.is_empty() || body.starts_with(['\n', '\r']))
    else {
        return Err(Error::Invalid(format!(
            "release notes must start with `{heading}` on its own line"
        )));
    };
    let body = body.trim_start_matches(['\n', '\r']).trim_end();
    let install = install_section(tag)?;
    Ok(if body.is_empty() {
        format!("{heading}\n\n{install}")
    } else {
        format!("{heading}\n\n{install}\n{body}\n")
    })
}

/// Parse a `SHA256SUMS` file into artifact name → lowercase digest.
#[cfg(test)]
pub fn checksum_table(text: &str) -> Result<BTreeMap<String, String>, Error> {
    let mut table = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((digest, name)) = line.split_once("  ") else {
            return Err(Error::Invalid(format!(
                "SHA256SUMS:{}: expected `<sha256>  <file>`",
                index + 1
            )));
        };
        let digest = digest.trim();
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(Error::Invalid(format!(
                "SHA256SUMS:{}: invalid SHA-256 digest",
                index + 1
            )));
        }
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::Invalid(format!(
                "SHA256SUMS:{}: empty artifact name",
                index + 1
            )));
        }
        table.insert(name.to_owned(), digest.to_ascii_lowercase());
    }
    if table.is_empty() {
        return Err(Error::Invalid("SHA256SUMS contains no entries".to_owned()));
    }
    Ok(table)
}

/// The `target=digest` defaults a release installer asset bakes in.
///
/// Every canonical target must appear. An installer missing one would fall back
/// to downloading `SHA256SUMS` for that platform, which is the run-time
/// dependency the baked copy exists to remove.
pub fn installer_digests(tag: &str, checksums: &BTreeMap<String, String>) -> Result<String, Error> {
    let mut defaults = Vec::new();
    for target in ReleaseTarget::all() {
        let artifact = target.artifact_name(tag);
        let digest = checksums
            .get(&artifact)
            .ok_or_else(|| Error::Invalid(format!("SHA256SUMS has no entry for {artifact}")))?;
        defaults.push(format!("{}={digest}", target.rust_target));
    }
    Ok(defaults.join(" "))
}

/// The assignment that identifies which release an installer asset serves.
pub fn shell_installer_version_marker(tag: &str) -> String {
    format!("default_version='{tag}'")
}

/// The PowerShell spelling of [`shell_installer_version_marker`].
pub fn powershell_installer_version_marker(tag: &str) -> String {
    format!("$DefaultVersion = '{tag}'")
}

/// Render the POSIX installer published with the versioned documentation.
pub fn shell_installer(source: &str, tag: &str, digests: &str) -> Result<String, Error> {
    replace_defaults_region(
        source,
        &format!(
            "# Generated by `xtask release-docs` for {tag}; do not edit.\n\
{}\n\
default_checksums='{digests}'",
            shell_installer_version_marker(tag)
        ),
    )
}

/// Render the PowerShell installer published with the versioned documentation.
pub fn powershell_installer(source: &str, tag: &str, digests: &str) -> Result<String, Error> {
    replace_defaults_region(
        source,
        &format!(
            "# Generated by `xtask release-docs` for {tag}; do not edit.\n\
{}\n\
$DefaultChecksums = '{digests}'",
            powershell_installer_version_marker(tag)
        ),
    )
}

/// Swap the contents of the published-defaults region for `defaults`.
///
/// The region has to appear exactly once, so a script that lost it or gained a
/// second one fails the release instead of shipping an installer that silently
/// falls back to the GitHub API. Everything outside the region is untouched,
/// which matters for PowerShell: its `param()` block has to precede every
/// statement, so the defaults cannot simply be prepended.
fn replace_defaults_region(source: &str, defaults: &str) -> Result<String, Error> {
    let region_begin = DEFAULTS_REGION_BEGIN.trim_end();
    let region_end = DEFAULTS_REGION_END.trim_end();
    if source.matches(DEFAULTS_REGION_BEGIN).count() != 1
        || source.matches(DEFAULTS_REGION_END).count() != 1
    {
        return Err(Error::Invalid(format!(
            "installer must contain exactly one `{region_begin}` … `{region_end}` region"
        )));
    }
    let begin = source.find(DEFAULTS_REGION_BEGIN).expect("counted above");
    let end = source.find(DEFAULTS_REGION_END).expect("counted above");
    if end < begin + DEFAULTS_REGION_BEGIN.len() {
        return Err(Error::Invalid(format!(
            "`{region_end}` precedes `{region_begin}`"
        )));
    }
    let body = begin + DEFAULTS_REGION_BEGIN.len();
    Ok(format!(
        "{}{defaults}\n{}",
        &source[..body],
        &source[end + DEFAULTS_REGION_END.len()..]
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManifestState {
    Pinned,
    Stamped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocsIdentity {
    pub schema_version: u32,
    pub version: String,
    pub tag: String,
    pub source_commit: String,
}

pub struct VersionContractInput<'a> {
    pub notes: &'a str,
    pub cargo_toml: &'a str,
    pub cargo_lock: &'a str,
    pub zensical: &'a str,
    pub cli_version: Option<&'a str>,
    pub docs: Option<&'a DocsIdentity>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionContractError(pub String);

impl fmt::Display for VersionContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for VersionContractError {}

/// Verify all release-facing version sources from already-read values.
pub fn version_contract(
    context: &ReleaseContext,
    input: VersionContractInput<'_>,
    manifest_state: ManifestState,
) -> Result<(), VersionContractError> {
    let expected = context.version.to_string();
    let expected_tag = context.tag.as_str();
    let expected_manifest = match manifest_state {
        ManifestState::Pinned => PINNED_VERSION,
        ManifestState::Stamped => expected.as_str(),
    };

    require(
        "release notes heading",
        input.notes.starts_with(&format!("# {expected}\n")),
        format!("# {expected}"),
    )?;
    require(
        "Cargo.toml package version",
        input
            .cargo_toml
            .lines()
            .any(|line| line.trim() == format!("version = \"{expected_manifest}\"")),
        expected_manifest.to_owned(),
    )?;
    require(
        "Cargo.lock mono package version",
        lockfile_package_version(input.cargo_lock, "mono").as_deref() == Some(expected_manifest),
        expected_manifest.to_owned(),
    )?;
    require(
        "Zensical displayed version",
        input
            .zensical
            .contains(&format!("Mono v{expected_manifest}")),
        format!("Mono v{expected_manifest}"),
    )?;

    if let Some(cli_version) = input.cli_version {
        require(
            "CLI version",
            cli_version.trim() == format!("mono {expected}"),
            format!("mono {expected}"),
        )?;
    }

    if let Some(docs) = input.docs {
        require(
            "documentation metadata schema",
            docs.schema_version == DOCS_METADATA_SCHEMA,
            DOCS_METADATA_SCHEMA.to_string(),
        )?;
        require(
            "documentation metadata version",
            docs.version == expected,
            expected.clone(),
        )?;
        require(
            "documentation metadata tag",
            docs.tag == expected_tag,
            expected_tag.to_owned(),
        )?;
        require(
            "documentation metadata source commit",
            docs.source_commit == context.source_commit,
            context.source_commit.clone(),
        )?;
    }
    Ok(())
}

fn require(field: &str, valid: bool, expected: String) -> Result<(), VersionContractError> {
    if valid {
        Ok(())
    } else {
        Err(VersionContractError(format!(
            "{field} does not match expected `{expected}`"
        )))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublicationState {
    Missing,
    Draft,
    Published,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublicationIntent {
    Upload,
    Finalize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublicationAction {
    CreateDraft,
    ReuseDraft,
    UploadAssets,
    Finalize,
    AlreadyPublished,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicationTransition {
    pub actions: Vec<PublicationAction>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicationTransitionError(pub String);

impl fmt::Display for PublicationTransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for PublicationTransitionError {}

/// Decide publication behavior without querying GitHub or executing commands.
pub fn publication_transition(
    state: PublicationState,
    intent: PublicationIntent,
) -> Result<PublicationTransition, PublicationTransitionError> {
    let actions = match (state, intent) {
        (PublicationState::Missing, PublicationIntent::Upload) => {
            vec![
                PublicationAction::CreateDraft,
                PublicationAction::UploadAssets,
            ]
        }
        (PublicationState::Draft, PublicationIntent::Upload) => {
            vec![
                PublicationAction::ReuseDraft,
                PublicationAction::UploadAssets,
            ]
        }
        (PublicationState::Published, PublicationIntent::Upload) => {
            vec![PublicationAction::AlreadyPublished]
        }
        (PublicationState::Missing, PublicationIntent::Finalize) => {
            return Err(PublicationTransitionError(
                "cannot finalize a release that does not exist".to_owned(),
            ));
        }
        (PublicationState::Draft, PublicationIntent::Finalize) => {
            vec![PublicationAction::Finalize]
        }
        (PublicationState::Published, PublicationIntent::Finalize) => {
            vec![PublicationAction::AlreadyPublished]
        }
    };
    Ok(PublicationTransition { actions })
}

pub fn parse_tag(tag: &str) -> Result<Version, Error> {
    let version = tag.strip_prefix('v').ok_or_else(|| invalid_tag(tag))?;
    Version::parse(version).ok_or_else(|| invalid_tag(tag))
}

fn invalid_tag(tag: &str) -> Error {
    Error::Invalid(format!(
        "release tag must match `v<major>.<minor>.<patch>`, got `{tag}`"
    ))
}

fn validate_changelog_text(text: &str, version: Version) -> Result<(), String> {
    let changelog = Changelog::parse(text)?;
    match changelog.top().heading {
        Heading::Version(actual) if actual == version => Ok(()),
        Heading::Version(actual) => Err(format!("newest entry is {actual}, expected {version}")),
        Heading::Unreleased => Err(format!(
            "newest entry is `(unreleased)`, expected {version}"
        )),
    }
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

fn required_env(name: &str) -> Result<String, Error> {
    non_empty_env(name).ok_or_else(|| Error::Invalid(format!("{name} is not set")))
}

fn non_empty_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.is_empty())
}

fn git_output(root: &Path, args: &[&str]) -> Result<String, Error> {
    let output = std::process::Command::new("git")
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
        "`git {}` failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

impl fmt::Display for ArchiveKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.extension())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> ReleaseContext {
        release_context("v0.1.5", "commit", Some("owner/repo"), None).unwrap()
    }

    #[test]
    fn release_context_is_pure_and_complete() {
        let context = context();
        assert_eq!(context.version.to_string(), "0.1.5");
        assert_eq!(context.tag, "v0.1.5");
        assert_eq!(context.source_commit, "commit");
        assert_eq!(context.repository.as_deref(), Some("owner/repo"));
    }

    #[test]
    fn release_plan_and_inventory_share_one_target_table() {
        let plan = release_plan(context()).unwrap();
        assert_eq!(plan.targets.len(), 4);
        assert_eq!(
            plan.artifact_names.len(),
            plan.targets.len() + INSTALLER_ASSETS.len()
        );
        assert_eq!(artifact_inventory("v0.1.5").unwrap(), plan.artifact_names);
        assert!(
            plan.artifact_names
                .contains(&SHELL_INSTALLER_FILE.to_owned())
        );
        assert!(
            plan.artifact_names
                .contains(&POWERSHELL_INSTALLER_FILE.to_owned())
        );
    }

    #[test]
    fn install_section_pins_the_tag_and_names_both_installers() {
        let section = install_section("v0.1.5").unwrap();

        assert!(section.starts_with("## Install\n"), "{section}");
        assert!(
            section.contains(
                "https://github.com/Tomperez98/mono/releases/download/v0.1.5/install.sh | sh"
            ),
            "{section}"
        );
        assert!(
            section.contains(
                "irm https://github.com/Tomperez98/mono/releases/download/v0.1.5/install.ps1 | iex"
            ),
            "{section}"
        );
        assert!(!section.contains("tomperez98.github.io"), "{section}");
        assert!(!section.contains("--version"), "{section}");
        assert!(!section.contains("MONO_VERSION"), "{section}");
        assert!(
            install_section("0.1.5").is_err(),
            "the tag carries the `v` prefix; a version alone is not a release"
        );
    }

    #[test]
    fn composed_notes_keep_the_heading_first_and_add_install_before_the_changelog() {
        let notes = "# 0.1.5\n\n## Changelog\n\nReleased: 2026-09-12\n";

        let composed = compose_release_notes("v0.1.5", notes).unwrap();

        assert!(composed.starts_with("# 0.1.5\n"), "{composed}");
        let install = composed.find("## Install").expect("install section");
        let changelog = composed.find("## Changelog").expect("changelog section");
        assert!(install < changelog, "install must precede the changelog");
        assert!(composed.ends_with("Released: 2026-09-12\n"), "{composed}");
    }

    #[test]
    fn composed_notes_accept_a_heading_only_file() {
        let composed = compose_release_notes("v0.1.5", "# 0.1.5\n").unwrap();

        assert!(composed.starts_with("# 0.1.5\n\n## Install"), "{composed}");
        assert!(composed.ends_with('\n'), "{composed}");
    }

    #[test]
    fn composed_notes_reject_anything_but_their_own_heading() {
        for notes in [
            "# 0.1.4\n\nbody\n",
            "## Install\n",
            "# 0.1.50\n\nbody\n",
            "# 0.1.5 \n\nbody\n",
        ] {
            let error = compose_release_notes("v0.1.5", notes).unwrap_err();
            assert!(error.to_string().contains("must start with"), "{error}");
        }
    }

    /// A `SHA256SUMS` body naming every canonical artifact for `tag`, each with
    /// a distinct valid digest.
    fn checksums_for(tag: &str) -> String {
        ReleaseTarget::all()
            .iter()
            .enumerate()
            .map(|(index, target)| format!("{:064x}  {}\n", index + 1, target.artifact_name(tag)))
            .collect()
    }

    const SHELL_SOURCE: &str = "#!/bin/sh\nset -eu\n\n#region published defaults\ndefault_version=\"\"\ndefault_checksums=\"\"\n#endregion\n\ncurl https://example.test\n";
    const POWERSHELL_SOURCE: &str = "#Requires -Version 5.1\nparam(\n    [string]$Version = \"\"\n)\n\n#region published defaults\n$DefaultVersion = \"\"\n$DefaultChecksums = \"\"\n#endregion\n";

    #[test]
    fn checksum_table_reads_the_sha256sums_format() {
        let table = checksum_table(&checksums_for("v0.1.5")).unwrap();
        let digest = format!("{:064x}", 1);

        assert_eq!(table.len(), ReleaseTarget::all().len());
        assert_eq!(
            table
                .get("mono-v0.1.5-x86_64-unknown-linux-gnu.tar.gz")
                .map(String::as_str),
            Some(digest.as_str())
        );
    }

    #[test]
    fn checksum_table_rejects_malformed_input() {
        for text in [
            "",
            "no-separator\n",
            "abc  short-digest.tar.gz\n",
            "aabb  \n",
        ] {
            assert!(checksum_table(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn installer_digests_cover_every_canonical_target() {
        let tag = "v0.1.5";
        let table = checksum_table(&checksums_for(tag)).unwrap();

        let digests = installer_digests(tag, &table).unwrap();

        let expected = ReleaseTarget::all()
            .iter()
            .enumerate()
            .map(|(index, target)| format!("{}={:064x}", target.rust_target, index + 1))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(digests, expected);
    }

    #[test]
    fn installer_digests_refuse_an_incomplete_release_contract() {
        let tag = "v0.1.5";
        let mut table = checksum_table(&checksums_for(tag)).unwrap();
        table.remove(&ReleaseTarget::all()[0].artifact_name(tag));

        let error = installer_digests(tag, &table).unwrap_err();

        assert!(error.to_string().contains("no entry for"), "{error}");
    }

    #[test]
    fn a_published_installer_carries_the_tag_and_digests() {
        let digests = format!("x86_64-unknown-linux-gnu={}", "a".repeat(64));

        let shell = shell_installer(SHELL_SOURCE, "v0.1.5", &digests).unwrap();
        assert!(shell.starts_with("#!/bin/sh\n"), "{shell}");
        assert!(
            shell.contains(&shell_installer_version_marker("v0.1.5")),
            "{shell}"
        );
        assert!(
            shell.contains(&format!("default_checksums='{digests}'")),
            "{shell}"
        );
        assert_eq!(
            shell.matches("#region published defaults").count(),
            1,
            "the region markers survive around the new contents: {shell}"
        );
        assert!(
            !shell.contains("default_version=\"\""),
            "the empty default is replaced: {shell}"
        );
        assert!(
            shell.contains("curl https://example.test"),
            "everything outside the region survives: {shell}"
        );

        let powershell = powershell_installer(POWERSHELL_SOURCE, "v0.1.5", &digests).unwrap();
        assert!(
            powershell.starts_with("#Requires -Version 5.1\n"),
            "{powershell}"
        );
        let defaults = powershell
            .find(&powershell_installer_version_marker("v0.1.5"))
            .expect("the defaults are rendered");
        let parameters = powershell.find("param(").expect("the param block survives");
        assert!(
            parameters < defaults,
            "PowerShell requires param() before every statement: {powershell}"
        );
        assert!(
            powershell.contains(&format!("$DefaultChecksums = '{digests}'")),
            "{powershell}"
        );
    }

    #[test]
    fn rendering_refuses_a_source_without_exactly_one_defaults_region() {
        assert!(shell_installer("#!/bin/sh\nset -eu\n", "v0.1.5", "digest").is_err());
        assert!(powershell_installer("#Requires -Version 5.1\n", "v0.1.5", "digest").is_err());
        assert!(
            shell_installer(&format!("{SHELL_SOURCE}{SHELL_SOURCE}"), "v0.1.5", "digest").is_err(),
            "two regions are ambiguous"
        );
        assert!(
            shell_installer(
                &SHELL_SOURCE.replace("#endregion\n", ""),
                "v0.1.5",
                "digest"
            )
            .is_err(),
            "an unterminated region fails"
        );
    }

    #[test]
    fn canonical_targets_have_stable_names() {
        let targets = ReleaseTarget::all();
        assert_eq!(targets.len(), 4);
        assert_eq!(
            targets[0].artifact_name("v0.1.5"),
            "mono-v0.1.5-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            targets[3].artifact_name("v0.1.5"),
            "mono-v0.1.5-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn version_contract_accepts_pinned_manifests_and_release_outputs() {
        let context = context();
        let docs = DocsIdentity {
            schema_version: 1,
            version: "0.1.5".to_owned(),
            tag: "v0.1.5".to_owned(),
            source_commit: "commit".to_owned(),
        };
        version_contract(
            &context,
            VersionContractInput {
                notes: "# 0.1.5\n",
                cargo_toml: "version = \"0.0.0\"\n",
                cargo_lock: "[[package]]\nname = \"mono\"\nversion = \"0.0.0\"\n",
                zensical: "site_name = \"Mono v0.0.0\"",
                cli_version: Some("mono 0.1.5"),
                docs: Some(&docs),
            },
            ManifestState::Pinned,
        )
        .unwrap();
    }

    #[test]
    fn version_contract_accepts_stamped_manifests() {
        version_contract(
            &context(),
            VersionContractInput {
                notes: "# 0.1.5\n",
                cargo_toml: "version = \"0.1.5\"\n",
                cargo_lock: "[[package]]\nname = \"mono\"\nversion = \"0.1.5\"\n",
                zensical: "site_name = \"Mono v0.1.5\"",
                cli_version: Some("mono 0.1.5"),
                docs: None,
            },
            ManifestState::Stamped,
        )
        .unwrap();
    }

    #[test]
    fn version_contract_rejects_wrong_cli_version() {
        let error = version_contract(
            &context(),
            VersionContractInput {
                notes: "# 0.1.5\n",
                cargo_toml: "version = \"0.0.0\"\n",
                cargo_lock: "[[package]]\nname = \"mono\"\nversion = \"0.0.0\"\n",
                zensical: "site_name = \"Mono v0.0.0\"",
                cli_version: Some("mono 0.1.4"),
                docs: None,
            },
            ManifestState::Pinned,
        )
        .unwrap_err();
        assert!(error.to_string().contains("CLI version"));
    }

    #[test]
    fn publication_transition_is_retry_safe() {
        assert_eq!(
            publication_transition(PublicationState::Missing, PublicationIntent::Upload)
                .unwrap()
                .actions,
            vec![
                PublicationAction::CreateDraft,
                PublicationAction::UploadAssets
            ]
        );
        assert_eq!(
            publication_transition(PublicationState::Draft, PublicationIntent::Finalize)
                .unwrap()
                .actions,
            vec![PublicationAction::Finalize]
        );
        assert_eq!(
            publication_transition(PublicationState::Published, PublicationIntent::Upload)
                .unwrap()
                .actions,
            vec![PublicationAction::AlreadyPublished]
        );
        assert!(
            publication_transition(PublicationState::Missing, PublicationIntent::Finalize).is_err()
        );
    }

    #[test]
    fn tags_require_the_v_prefix() {
        assert!(parse_tag("v1.2.3").is_ok());
        assert!(parse_tag("1.2.3").is_err());
        assert!(parse_tag("v1.2").is_err());
    }
}
