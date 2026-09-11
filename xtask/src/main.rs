//! Project-specific release gates for this repository.
//!
//! Generic changelog, release-source, checksum, and artifact-manifest
//! operations live in the published `mono` CLI. This binary retains only
//! repository-specific release automation: tag creation, binary behavior,
//! examples, and the release plan.

mod process;
mod release_contract;
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
    after_help = "Run `cargo run -p xtask -- verify` for the release gates, or `cargo run -p xtask -- tag --tag v0.1.3` to create and push a release tag."
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
}

/// Every failure this project-specific gate can report.
#[derive(Debug)]
pub enum Error {
    Spawn { program: String, source: io::Error },
    Invalid(String),
    Command(String),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn { program, source } => {
                write!(formatter, "failed to run {program}: {source}")
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

fn binary_path() -> PathBuf {
    path_from_env("MONO_BIN", BINARY_DEFAULT)
}

fn path_from_env(name: &str, default: &str) -> PathBuf {
    match env::var_os(name) {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => PathBuf::from(default),
    }
}
