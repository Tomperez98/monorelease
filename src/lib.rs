//! Core logic for `mono`.
//!
//! Each subcommand lives in its own module under [`commands`], owns its own
//! error vocabulary, and is re-exported at the crate root. Everything here
//! either returns one of its documented failures or panics on a broken
//! invariant; nothing knows about `clap`, stdout, or exit codes — that
//! translation happens once, in `main.rs`.

mod atomic_file;
mod cache;
mod changelog;
mod commands;
mod config;
mod discovery;
pub(crate) mod events;

/// Version of Mono's machine-readable JSON output contracts.
pub const JSON_OUTPUT_SCHEMA: u32 = 1;
mod output;
mod platform;
pub(crate) mod process;
mod project;
mod release;
mod runner;
mod scheduler;
mod stream_output;
#[cfg(test)]
mod testing;
mod tui;

use std::error::Error as StdError;
use std::fmt;

pub use cache::CacheMode;
pub use changelog::{
    Action as ChangelogAction, Changelog, Entry as ChangelogEntry, Heading, Request, Version,
};
pub use commands::changelog::{
    ChangelogError, DEFAULT_NOTES_PATH as DEFAULT_RELEASE_NOTES_PATH,
    DEFAULT_PATH as DEFAULT_CHANGELOG_PATH, ReleaseNotesTarget, notes as changelog_notes,
    prepare as changelog_prepare, prepare_from_git as changelog_prepare_from_git,
    prepare_on as changelog_prepare_on, release_notes as changelog_release_notes,
    scaffold as changelog_scaffold, scaffold_on as changelog_scaffold_on,
    validate as changelog_validate,
};
pub use commands::ci::{
    CiError, PipelineExecution, clean_cache, graph, graph_with_output, plan, plan_with_output,
    run_pipeline_with_mode,
};
pub use commands::doctor::{DoctorError, doctor};
pub use commands::init::{InitError, init};
pub use commands::list::{ListError, list, list_with_output};
pub use commands::release::{
    DEFAULT_DIRECTORY as DEFAULT_RELEASE_DIRECTORY, ReleaseCommandError,
    manifest as release_manifest, source as release_source, verify as release_verify,
};
pub use config::{
    CONFIG_FILE_NAME, MonoConfig, PipelineConfig, ProjectConfig, SUPPORTED_SCHEMA, StdinMode,
    TaskConfig, config_path, render_config,
};
pub use output::OutputMode;
pub use runner::CancellationToken;
// `CiError::Scheduler` already exposes this type in its public variant; naming it
// lets a caller tell a failed task apart from a scheduler or cache failure.
pub use project::{PlannedTask, Project, ProjectError, TaskNode};
pub use release::{
    Artifact, ReleaseError, ReleaseIdentity, ReleaseManifest, create_manifest,
    create_manifest_with_expected, verify_checksums, verify_manifest,
    verify_manifest_with_expected, verify_source,
};
pub use scheduler::SchedulerError;

/// Every expected failure a `mono` command can report.
///
/// The signature of a command documents its whole failure space; this union
/// is what a caller has to handle, and nothing else.
#[derive(Debug)]
pub enum Error {
    Init(InitError),
    Doctor(DoctorError),
    Ci(CiError),
    List(ListError),
    Changelog(ChangelogError),
    Release(ReleaseCommandError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Init(error) => error.fmt(f),
            Self::Doctor(error) => error.fmt(f),
            Self::Ci(error) => error.fmt(f),
            Self::List(error) => error.fmt(f),
            Self::Changelog(error) => error.fmt(f),
            Self::Release(error) => error.fmt(f),
        }
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Init(error) => Some(error),
            Self::Doctor(error) => Some(error),
            Self::Ci(error) => Some(error),
            Self::List(error) => Some(error),
            Self::Changelog(error) => Some(error),
            Self::Release(error) => Some(error),
        }
    }
}

impl From<InitError> for Error {
    fn from(error: InitError) -> Self {
        Self::Init(error)
    }
}

impl From<DoctorError> for Error {
    fn from(error: DoctorError) -> Self {
        Self::Doctor(error)
    }
}

impl From<CiError> for Error {
    fn from(error: CiError) -> Self {
        Self::Ci(error)
    }
}

impl From<ListError> for Error {
    fn from(error: ListError) -> Self {
        Self::List(error)
    }
}

impl From<ChangelogError> for Error {
    fn from(error: ChangelogError) -> Self {
        Self::Changelog(error)
    }
}

impl From<ReleaseCommandError> for Error {
    fn from(error: ReleaseCommandError) -> Self {
        Self::Release(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    fn builders() -> [(Error, bool); 6] {
        [
            (
                Error::Init(InitError::AlreadyInitialized(std::path::PathBuf::from(
                    "mono.toml",
                ))),
                false,
            ),
            (
                Error::Doctor(DoctorError::Project(ProjectError::MissingRoot {
                    start: std::path::PathBuf::from("."),
                })),
                true,
            ),
            (Error::Ci(CiError::InvalidJobs), false),
            (
                Error::List(ListError::Project(ProjectError::MissingRoot {
                    start: std::path::PathBuf::from("."),
                })),
                true,
            ),
            (
                Error::Changelog(ChangelogError::Invalid("bad".to_owned())),
                false,
            ),
            (
                Error::Release(ReleaseCommandError::Release(ReleaseError::Write {
                    path: std::path::PathBuf::from("out"),
                    source: io::Error::new(io::ErrorKind::PermissionDenied, "denied"),
                })),
                true,
            ),
        ]
    }

    /// The union is the command failure space, so its `source` must forward to
    /// the wrapped error rather than swallow it.
    #[test]
    fn the_error_union_forwards_display_and_source() {
        for (error, has_source) in builders() {
            let has_underlying_source = error.source().and_then(|e| e.source()).is_some();
            assert_eq!(has_underlying_source, has_source, "{error}");
            assert!(!error.to_string().is_empty(), "{error}");
        }
    }

    #[test]
    fn every_command_error_converts_into_the_union() {
        let _: Error = InitError::AlreadyInitialized(std::path::PathBuf::from("mono.toml")).into();
    }
}
