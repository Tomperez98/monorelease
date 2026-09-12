//! Project-specific release gates for this repository.
//!
//! Generic changelog, release-source, checksum, and artifact-manifest
//! operations live in the published `mono` CLI. This binary retains only
//! repository-specific release automation: tag creation, binary behavior,
//! examples, and the release plan.

mod process;
mod release;
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

#[derive(Parser)]
#[command(
    name = "xtask",
    version,
    about = "Repository-specific release automation for mono",
    after_help = "Run `cargo run -p xtask -- release-prepare --tag v0.1.5` to validate a release, `cargo run -p xtask -- release-build --target x86_64-unknown-linux-gnu` to build one canonical artifact, or `cargo run -p xtask -- tag --tag v0.1.5` to create and push a release tag."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run this repository's gates against the binary in MONO_BIN.
    Verify,
    /// Write and verify this repository's release artifact contract.
    ReleaseContract {
        /// Release artifact directory.
        #[arg(long, default_value = release_contract::default_directory())]
        directory: PathBuf,
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
        /// Version tag to validate, for example v0.1.5. Defaults to RELEASE_TAG.
        #[arg(long)]
        tag: Option<String>,
    },
    /// Build one canonical release artifact for a target.
    ReleaseBuild {
        /// Rust target from the canonical release target table.
        #[arg(long)]
        target: String,
        /// Release artifact directory.
        #[arg(long, default_value = "dist")]
        directory: PathBuf,
    },
    /// Build the versioned documentation for a release tag.
    ReleaseDocs {
        /// Version to stamp, for example 0.1.5 or v0.1.5. Defaults to RELEASE_TAG.
        #[arg(long)]
        version: Option<String>,
        /// Release directory holding the SHA256SUMS the installers bake in.
        #[arg(long, default_value = "dist")]
        directory: PathBuf,
    },
    /// Verify the committed repository release state and target contract.
    ReleaseCheck,
    /// Verify every user-visible release version source agrees.
    ReleaseVersionCheck {
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
    ReleaseValidatePublished,
    /// Validate one native platform release artifact.
    ReleaseValidatePlatform {
        /// Rust target from the canonical release target table.
        #[arg(long)]
        target: String,
    },
    /// Create or finalize the GitHub release from the assembled artifacts.
    ReleasePublish {
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
        /// Version to stamp, for example 0.1.5 or v0.1.5. Defaults to RELEASE_TAG.
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
        Command::Verify => (VERIFY_COMPONENT, verify_command()),
        Command::ReleaseContract {
            directory,
            verify_only,
        } => (
            release_contract::COMPONENT,
            release_contract::run(&directory, verify_only),
        ),
        Command::Tag { tag } => (tag::COMPONENT, tag::run(&tag)),
        Command::ReleasePrepare { tag } => {
            (release::PREPARE_COMPONENT, release_prepare_command(tag))
        }
        Command::ReleaseBuild { target, directory } => (
            release::BUILD_COMPONENT,
            release_build_command(target, directory),
        ),
        Command::ReleaseDocs { version, directory } => {
            (release::COMPONENT, release_docs_command(version, directory))
        }
        Command::ReleaseCheck => (
            release::STATE_COMPONENT,
            release::check(&stamp::repository_root()),
        ),
        Command::ReleaseVersionCheck {
            binary,
            site,
            notes,
        } => (
            release::CHECK_COMPONENT,
            release_version_check_command(binary, site, notes),
        ),
        Command::ReleaseValidatePublished => (
            release::VALIDATE_COMPONENT,
            release_validate_published_command(),
        ),
        Command::ReleaseValidatePlatform { target } => (
            release::VALIDATE_COMPONENT,
            release_validate_platform_command(target),
        ),
        Command::ReleasePublish {
            directory,
            notes,
            finalize,
        } => (
            release::PUBLISH_COMPONENT,
            release_publish_command(directory, notes, finalize),
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
    pub(crate) fn from_env() -> Result<Self, Error> {
        let name = env::var("RELEASE_TAG").map_err(|_| {
            Error::Invalid("RELEASE_TAG is not set (for example RELEASE_TAG=v0.1.1)".to_owned())
        })?;
        let version = Version::parse(name.strip_prefix('v').unwrap_or(&name)).ok_or_else(|| {
            Error::Invalid(format!(
                "RELEASE_TAG is `{name}`, expected `v<major>.<minor>.<patch>` (for example v0.1.1)"
            ))
        })?;
        Ok(Self { name, version })
    }
}

fn verify_command() -> Result<(), Error> {
    verify::run(&Tag::from_env()?, &binary_path())
}

fn release_prepare_command(tag: Option<String>) -> Result<(), Error> {
    let tag = match tag {
        Some(tag) => tag,
        None => Tag::from_env()?.name,
    };
    let version = parse_version(&tag)?;
    release::prepare(&stamp::repository_root(), &tag, version)
}

fn release_build_command(target: String, directory: PathBuf) -> Result<(), Error> {
    release::build(
        &stamp::repository_root(),
        &Tag::from_env()?,
        &target,
        &directory,
    )
}

fn release_docs_command(version: Option<String>, directory: PathBuf) -> Result<(), Error> {
    let version = match version {
        Some(version) => parse_version(&version)?,
        None => Tag::from_env()?.version,
    };
    release::docs(&stamp::repository_root(), version, &directory)
}

fn release_version_check_command(
    binary: Option<PathBuf>,
    site: PathBuf,
    notes: PathBuf,
) -> Result<(), Error> {
    release::version_check(
        &stamp::repository_root(),
        &Tag::from_env()?,
        binary.as_deref(),
        &site,
        &notes,
    )
}

fn release_validate_published_command() -> Result<(), Error> {
    release::validate_published(&stamp::repository_root())
}

fn release_validate_platform_command(target: String) -> Result<(), Error> {
    release::validate_platform(&stamp::repository_root(), &target)
}

fn release_publish_command(
    directory: PathBuf,
    notes: PathBuf,
    finalize: bool,
) -> Result<(), Error> {
    let tag = Tag::from_env()?;
    release::publish(&tag.name, &directory, &notes, finalize)
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
        None => Tag::from_env()?.version,
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
