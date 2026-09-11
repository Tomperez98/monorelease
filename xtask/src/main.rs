//! Repository automation for monorelease releases.
//!
//! This binary is *not* part of the shipped `monorelease` tool. `monorelease`
//! stays generic: it schedules task graphs and knows nothing about changelogs,
//! release notes, or package registries. The release pipeline in
//! `monorepo.toml` runs these commands as ordinary tasks, and
//! `Release (validate)` runs them against a published binary.
//!
//! Every command is a pure function over the repository, returns an explicit
//! error instead of panicking, and prints one component-prefixed line per
//! action, so a failed release run can be reproduced by hand.

mod changelog;
mod git;
mod notes;
mod process;
mod verify;

use std::env;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::changelog::{Changelog, Request};

/// Prefixes used for every line a command prints, so the release log stays
/// greppable and each message names the step that produced it.
const NOTES_COMPONENT: &str = "release-notes";
const VERIFY_COMPONENT: &str = "release-verify";
const CHANGELOG_COMPONENT: &str = "changelog-scaffold";

const CHANGELOG_DEFAULT: &str = "CHANGELOG.md";
const NOTES_DEFAULT: &str = "RELEASE_NOTES.md";
const BINARY_DEFAULT: &str = "target/debug/monorelease";

#[derive(Parser)]
#[command(
    name = "xtask",
    version,
    about = "Repository automation for monorelease releases",
    after_help = "Run `cargo run -p xtask -- help <command>` for command details."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write RELEASE_NOTES.md from CHANGELOG.md for the tag in RELEASE_TAG
    Notes,
    /// Run the release gates against the binary in MONORELEASE_BIN
    Verify,
    /// Changelog helpers
    Changelog {
        #[command(subcommand)]
        command: ChangelogCommand,
    },
}

#[derive(Subcommand)]
enum ChangelogCommand {
    /// Add the newest entry from the merge history since the last release tag
    Scaffold {
        /// Version to prepare, or `unreleased`; defaults to $VERSION
        #[arg(value_name = "VERSION")]
        version: Option<String>,
    },
}

/// Every failure these commands can report.
///
/// Each variant names the input that failed, because the only thing a release
/// engineer can act on at 2am is which file or command to look at.
#[derive(Debug)]
pub enum Error {
    Read {
        path: PathBuf,
        source: io::Error,
    },
    Write {
        path: PathBuf,
        source: io::Error,
    },
    Spawn {
        program: String,
        source: io::Error,
    },
    /// The repository, or the environment, violates a documented invariant.
    /// These messages carry the fix.
    Invalid(String),
    /// A command that the release gates ran failed.
    Command(String),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(formatter, "failed to read {}: {source}", path.display())
            }
            Self::Write { path, source } => {
                write!(formatter, "failed to write {}: {source}", path.display())
            }
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
        Command::Notes => (NOTES_COMPONENT, notes_command()),
        Command::Verify => (VERIFY_COMPONENT, verify_command()),
        Command::Changelog { command } => (CHANGELOG_COMPONENT, changelog_command(command)),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{component}: {error}");
            ExitCode::FAILURE
        }
    }
}

/// The tag being released, validated once so every command reports the same way.
struct Tag {
    name: String,
    version: changelog::Version,
}

impl Tag {
    fn from_env() -> Result<Self, Error> {
        let name = env::var("RELEASE_TAG").map_err(|_| {
            Error::Invalid("RELEASE_TAG is not set (for example RELEASE_TAG=v0.1.1)".to_owned())
        })?;
        let version = changelog::Version::parse(name.strip_prefix('v').unwrap_or(&name)).ok_or_else(|| {
            Error::Invalid(format!("RELEASE_TAG is `{name}`, expected `v<major>.<minor>.<patch>` (for example v0.1.1)"))
        })?;
        Ok(Self { name, version })
    }
}

fn notes_command() -> Result<(), Error> {
    notes::run(&Tag::from_env()?, &changelog_path(), &notes_path())
}

fn verify_command() -> Result<(), Error> {
    verify::run(&Tag::from_env()?, &binary_path(), assets_path().as_deref())
}

/// Scaffold the next changelog entry, then print what to do with it.
fn changelog_command(command: ChangelogCommand) -> Result<(), Error> {
    let ChangelogCommand::Scaffold { version } = command;

    let version = version
        .or_else(|| env::var("VERSION").ok())
        .ok_or_else(|| {
            Error::Invalid("VERSION is not set (for example VERSION=0.1.2)".to_owned())
        })?;
    let request = Request::parse(&version).map_err(Error::Invalid)?;

    let path = changelog_path();
    let text = read_to_string(&path)?;
    let mut changelog = Changelog::parse(&text).map_err(|message| invalid(&path, message))?;

    // A versioned entry that replaces `(unreleased)` is a rename: the pull
    // requests are already listed under it.
    let (bullets, range) = match changelog.replaces_unreleased(&request) {
        true => (Vec::new(), None),
        false => {
            let slug = git::remote_slug()?;
            let range = git::range()?;
            let bullets = git::merges(&range)?
                .iter()
                .map(|bullet| bullet.line(&slug))
                .collect();
            (bullets, Some(range))
        }
    };

    let action = changelog
        .scaffold(&request, &changelog::today(), &bullets)
        .map_err(|message| invalid(&path, message))?;
    write(&path, &changelog.render())?;

    match action {
        changelog::Action::Inserted => {
            let range = range.unwrap_or_else(|| "HEAD".to_owned());
            println!(
                "{}: added `{}` to {} ({range})",
                CHANGELOG_COMPONENT,
                request.heading(),
                path.display()
            );
        }
        changelog::Action::Renamed { from, to } => {
            println!(
                "{}: renamed `{}` to `{to}` in {}",
                CHANGELOG_COMPONENT,
                from.heading(),
                path.display()
            );
        }
    }
    println!(
        "{CHANGELOG_COMPONENT}: drop trivia, group related pull requests, and describe the release for users"
    );
    Ok(())
}

fn invalid(path: &Path, message: String) -> Error {
    Error::Invalid(format!("{}: {message}", path.display()))
}

fn changelog_path() -> PathBuf {
    path_from_env("CHANGELOG", CHANGELOG_DEFAULT)
}

fn notes_path() -> PathBuf {
    path_from_env("RELEASE_NOTES", NOTES_DEFAULT)
}

fn binary_path() -> PathBuf {
    path_from_env("MONORELEASE_BIN", BINARY_DEFAULT)
}

/// Set-but-empty is treated as unset, so `RELEASE_ASSETS= ` means "no assets".
fn assets_path() -> Option<PathBuf> {
    env::var_os("RELEASE_ASSETS")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn path_from_env(name: &str, default: &str) -> PathBuf {
    match env::var_os(name) {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => PathBuf::from(default),
    }
}

fn read_to_string(path: &Path) -> Result<String, Error> {
    fs::read_to_string(path).map_err(|source| Error::Read {
        path: path.to_path_buf(),
        source,
    })
}

fn write(path: &Path, contents: &str) -> Result<(), Error> {
    fs::write(path, contents).map_err(|source| Error::Write {
        path: path.to_path_buf(),
        source,
    })
}
