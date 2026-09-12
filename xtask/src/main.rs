//! Project-specific release gates for this repository.
//!
//! Generic changelog, release-source, checksum, and artifact-manifest
//! operations live in the published `mono` CLI. This binary retains only
//! repository-specific release automation: tag creation, binary behavior,
//! examples, and the release plan.

mod platform;
mod process;
mod release;
mod release_artifacts;
mod release_contract;
mod release_model;
mod stamp;
mod tag;
mod verify;

use std::env;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use mono::Version;

const VERIFY_COMPONENT: &str = "release-verify";
const BINARY_DEFAULT: &str = "target/debug/mono";

#[derive(Debug, Parser)]
#[command(
    name = "xtask",
    version,
    about = "Repository-specific release automation for mono",
    after_help = "Run `cargo run -p xtask -- release-prepare --tag v0.1.5` to validate a release, `cargo run -p xtask -- release-build --tag v0.1.5 --target x86_64-unknown-linux-gnu` to build one canonical artifact, or `cargo run -p xtask -- tag --tag v0.1.5` to create and push a release tag."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run this repository's gates against the binary in MONO_BIN.
    Verify {
        /// Release tag used for binary identity checks, for example v0.1.5.
        #[arg(long)]
        tag: String,
    },
    /// Write and verify this repository's release artifact contract.
    ReleaseContract {
        /// Release artifact directory.
        #[arg(long, default_value = release_contract::default_directory())]
        directory: PathBuf,
        /// Release tag, for example v0.1.5.
        #[arg(long)]
        tag: String,
        /// Source commit recorded in the release manifest.
        #[arg(long)]
        commit: String,
        /// Repository recorded in the release manifest.
        #[arg(long)]
        repository: Option<String>,
        /// Annotated tag object recorded in the release manifest.
        #[arg(long)]
        tag_object: Option<String>,
        /// Workflow run URL recorded in the release manifest.
        #[arg(long)]
        workflow_run: Option<String>,
        /// Verify an existing manifest without rewriting it.
        #[arg(long)]
        verify_only: bool,
    },
    /// Create and push an annotated tag to start the GitHub release workflow.
    Tag {
        /// Version tag to create, for example v0.1.3.
        #[arg(long)]
        tag: String,
    },
    /// Run the complete release preparation gates from one stamped checkout.
    ReleasePrepare {
        /// Version tag to validate, for example v0.1.5.
        #[arg(long)]
        tag: String,
    },
    /// Build one canonical release artifact for a target.
    ReleaseBuild {
        /// Release tag, for example v0.1.5.
        #[arg(long)]
        tag: String,
        /// Rust target from the canonical release target table.
        #[arg(long)]
        target: String,
        /// Release artifact directory.
        #[arg(long, default_value = "dist")]
        directory: PathBuf,
    },
    /// Build the versioned documentation for a release tag.
    ReleaseDocs {
        /// Release tag, for example v0.1.5.
        #[arg(long)]
        tag: String,
        /// GitHub repository recorded in the generated docs metadata.
        #[arg(long)]
        repository: String,
        /// Release workflow URL recorded in the generated docs metadata.
        #[arg(long)]
        workflow_run: Option<String>,
        /// Release directory holding the SHA256SUMS the installers bake in.
        #[arg(long, default_value = "dist")]
        directory: PathBuf,
    },
    /// Verify the committed repository release state and target contract.
    ReleaseCheck,
    /// Verify every user-visible release version source agrees.
    ReleaseVersionCheck {
        /// Release tag, for example v0.1.5.
        #[arg(long)]
        tag: String,
        /// Binary to query with --version.
        #[arg(long)]
        binary: Option<PathBuf>,
        /// Generated documentation directory.
        #[arg(long, default_value = "site")]
        site: PathBuf,
        /// Generated release notes.
        #[arg(long, default_value = "RELEASE_NOTES.md")]
        notes: PathBuf,
    },
    /// Validate the published GitHub release and deployed documentation.
    ReleaseValidatePublished {
        /// Release tag, for example v0.1.5.
        #[arg(long)]
        tag: String,
        /// GitHub repository, for example org/repo.
        #[arg(long)]
        repository: String,
    },
    /// Validate one native platform release artifact.
    ReleaseValidatePlatform {
        /// Release tag, for example v0.1.5.
        #[arg(long)]
        tag: String,
        /// GitHub repository, for example org/repo.
        #[arg(long)]
        repository: String,
        /// Rust target from the canonical release target table.
        #[arg(long)]
        target: String,
    },
    /// Create or finalize the GitHub release from the assembled artifacts.
    ReleasePublish {
        /// Release tag, for example v0.1.5.
        #[arg(long)]
        tag: String,
        /// GitHub repository to publish to.
        #[arg(long)]
        repository: String,
        /// Release artifact directory.
        #[arg(long, default_value = release_contract::default_directory())]
        directory: PathBuf,
        /// Release notes file.
        #[arg(long, default_value = "RELEASE_NOTES.md")]
        notes: PathBuf,
        /// Publish an existing draft after every other release gate succeeds.
        #[arg(long)]
        finalize: bool,
    },
    /// Stamp the pinned manifests with a release version, or restore them.
    ReleaseStamp {
        /// Version to stamp, for example 0.1.5 or v0.1.5. Required unless --restore is set.
        #[arg(long)]
        version: Option<String>,
        /// Restore the pinned placeholders from their `.backup` files.
        #[arg(long)]
        restore: bool,
    },
}

/// Every failure this project-specific gate can report.
#[derive(Debug)]
pub enum Error {
    Spawn { program: String, source: io::Error },
    Io { path: PathBuf, source: io::Error },
    Invalid(String),
    Command(String),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn { program, source } => {
                write!(formatter, "failed to run {program}: {source}")
            }
            Self::Io { path, source } => {
                write!(formatter, "failed to access {}: {source}", path.display())
            }
            Self::Invalid(message) | Self::Command(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let (component, result) = match cli.command {
        Command::Verify { tag } => (VERIFY_COMPONENT, verify_command(tag)),
        Command::ReleaseContract {
            directory,
            tag,
            commit,
            repository,
            tag_object,
            workflow_run,
            verify_only,
        } => (
            release_contract::COMPONENT,
            release_contract::run_with_identity(
                &directory,
                verify_only,
                mono::ReleaseIdentity {
                    repository: non_empty(repository),
                    release_tag: Some(tag),
                    source_commit: Some(commit),
                    tag_object: non_empty(tag_object),
                    workflow_run: non_empty(workflow_run),
                },
            ),
        ),
        Command::Tag { tag } => (tag::COMPONENT, tag::run(&tag)),
        Command::ReleasePrepare { tag } => {
            (release::PREPARE_COMPONENT, release_prepare_command(tag))
        }
        Command::ReleaseBuild {
            tag,
            target,
            directory,
        } => (
            release::BUILD_COMPONENT,
            release_build_command(tag, target, directory),
        ),
        Command::ReleaseDocs {
            tag,
            repository,
            workflow_run,
            directory,
        } => (
            release::COMPONENT,
            release_docs_command(tag, repository, workflow_run, directory),
        ),
        Command::ReleaseCheck => (
            release::STATE_COMPONENT,
            release::check(&stamp::repository_root()),
        ),
        Command::ReleaseVersionCheck {
            tag,
            binary,
            site,
            notes,
        } => (
            release::CHECK_COMPONENT,
            release_version_check_command(tag, binary, site, notes),
        ),
        Command::ReleaseValidatePublished { tag, repository } => (
            release::VALIDATE_COMPONENT,
            release_validate_published_command(tag, repository),
        ),
        Command::ReleaseValidatePlatform {
            tag,
            repository,
            target,
        } => (
            release::VALIDATE_COMPONENT,
            release_validate_platform_command(tag, repository, target),
        ),
        Command::ReleasePublish {
            tag,
            repository,
            directory,
            notes,
            finalize,
        } => (
            release::PUBLISH_COMPONENT,
            release_publish_command(tag, repository, directory, notes, finalize),
        ),
        Command::ReleaseStamp { version, restore } => {
            (stamp::COMPONENT, release_stamp_command(version, restore))
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{component}: {error}");
            ExitCode::FAILURE
        }
    }
}

/// The tag being released, validated once so every gate reports consistently.
pub(crate) struct Tag {
    pub(crate) name: String,
    pub(crate) version: Version,
}

impl Tag {
    pub(crate) fn parse(text: &str) -> Result<Self, Error> {
        let version = Version::parse(text.strip_prefix('v').unwrap_or(text)).ok_or_else(|| {
            Error::Invalid(format!(
                "release tag is `{text}`, expected `v<major>.<minor>.<patch>` (for example v0.1.1)"
            ))
        })?;
        Ok(Self {
            name: text.to_owned(),
            version,
        })
    }
}

fn verify_command(tag: String) -> Result<(), Error> {
    let tag = Tag::parse(&tag)?;
    verify::run(&tag, &binary_path())
}

fn release_prepare_command(tag: String) -> Result<(), Error> {
    let version = parse_version(&tag)?;
    release::prepare(&stamp::repository_root(), &tag, version)
}

fn release_build_command(tag: String, target: String, directory: PathBuf) -> Result<(), Error> {
    let tag = Tag::parse(&tag)?;
    release::build(&stamp::repository_root(), &tag, &target, &directory)
}

fn release_docs_command(
    tag: String,
    repository: String,
    workflow_run: Option<String>,
    directory: PathBuf,
) -> Result<(), Error> {
    let tag = Tag::parse(&tag)?;
    release::docs(
        &stamp::repository_root(),
        &tag,
        &repository,
        workflow_run.as_deref(),
        &directory,
    )
}

fn release_version_check_command(
    tag: String,
    binary: Option<PathBuf>,
    site: PathBuf,
    notes: PathBuf,
) -> Result<(), Error> {
    let tag = Tag::parse(&tag)?;
    release::version_check(
        &stamp::repository_root(),
        &tag,
        binary.as_deref(),
        &site,
        &notes,
    )
}

fn release_validate_published_command(tag: String, repository: String) -> Result<(), Error> {
    let tag = Tag::parse(&tag)?;
    release::validate_published(&stamp::repository_root(), &tag, &repository)
}

fn release_validate_platform_command(
    tag: String,
    repository: String,
    target: String,
) -> Result<(), Error> {
    let tag = Tag::parse(&tag)?;
    release::validate_platform(&stamp::repository_root(), &tag, &repository, &target)
}

fn release_publish_command(
    tag: String,
    repository: String,
    directory: PathBuf,
    notes: PathBuf,
    finalize: bool,
) -> Result<(), Error> {
    let tag = Tag::parse(&tag)?;
    release::publish(&tag.name, &repository, &directory, &notes, finalize)
}

fn release_stamp_command(version: Option<String>, restore: bool) -> Result<(), Error> {
    if restore {
        if version.is_some() {
            return Err(Error::Invalid(
                "`release-stamp --restore` does not take `--version`".to_owned(),
            ));
        }
        return stamp::restore(&stamp::repository_root());
    }

    let version = match version {
        Some(version) => parse_version(&version)?,
        None => {
            return Err(Error::Invalid(
                "`--version` is required unless `--restore` is set".to_owned(),
            ));
        }
    };
    stamp::apply(&stamp::repository_root(), version)
}

/// Accept `X.Y.Z` or `vX.Y.Z`, so the same value works as a tag, a workflow
/// expression, and an xtask argument.
fn parse_version(text: &str) -> Result<Version, Error> {
    let version = text.strip_prefix('v').unwrap_or(text);
    Version::parse(version).ok_or_else(|| {
        Error::Invalid(format!(
            "`{text}` is not a release version; expected `<major>.<minor>.<patch>` or `v<major>.<minor>.<patch>`"
        ))
    })
}

fn binary_path() -> PathBuf {
    path_from_env("MONO_BIN", BINARY_DEFAULT)
}

fn path_from_env(name: &str, default: &str) -> PathBuf {
    match env::var_os(name) {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => PathBuf::from(default),
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_requires_explicit_tag() {
        let error = Cli::try_parse_from(["xtask", "verify"]).unwrap_err();
        assert!(
            error.to_string().contains("--tag"),
            "expected --tag requirement, got: {error}"
        );
    }

    #[test]
    fn release_prepare_requires_explicit_tag() {
        let error = Cli::try_parse_from(["xtask", "release-prepare"]).unwrap_err();
        assert!(
            error.to_string().contains("--tag"),
            "expected --tag requirement, got: {error}"
        );
    }

    #[test]
    fn release_stamp_requires_version_unless_restore() {
        // Parsing succeeds (version is Option), but the command function rejects it.
        let cli = Cli::try_parse_from(["xtask", "release-stamp"]).unwrap();
        let Command::ReleaseStamp { version, restore } = cli.command else {
            panic!("unexpected command");
        };
        assert!(version.is_none());
        assert!(!restore);
        let result = release_stamp_command(version, restore);
        assert!(
            result.is_err(),
            "expected error when neither --version nor --restore is provided"
        );
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("--version"),
            "expected --version requirement, got: {error}"
        );
    }

    #[test]
    fn release_stamp_restore_rejects_version() {
        let cli =
            Cli::try_parse_from(["xtask", "release-stamp", "--restore", "--version", "1.0.0"])
                .unwrap();
        let Command::ReleaseStamp { version, restore } = cli.command else {
            panic!("unexpected command");
        };
        assert!(restore);
        let result = release_stamp_command(version, restore);
        assert!(
            result.is_err(),
            "expected error when --restore is combined with --version"
        );
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("--restore"),
            "expected --restore conflict error, got: {error}"
        );
    }
}
