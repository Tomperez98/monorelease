//! Core logic for `monorelease`.
//!
//! Each subcommand lives in its own module under [`commands`], owns its own
//! error vocabulary, and is re-exported at the crate root. Everything here
//! either returns one of its documented failures or panics on a broken
//! invariant; nothing knows about `clap`, stdout, or exit codes — that
//! translation happens once, in `main.rs`.

mod cache;
mod changelog;
mod commands;
mod config;
mod discovery;
mod output;
mod release;
mod runner;
mod scheduler;
#[cfg(test)]
mod testing;
mod workspace;

use std::error::Error as StdError;
use std::fmt;

pub use cache::CacheMode;
pub use changelog::{
    Action as ChangelogAction, Changelog, Entry as ChangelogEntry, Heading, Request, Version,
};
pub use commands::changelog::{
    ChangelogError, DEFAULT_NOTES_PATH as DEFAULT_RELEASE_NOTES_PATH,
    DEFAULT_PATH as DEFAULT_CHANGELOG_PATH, notes as changelog_notes,
    scaffold as changelog_scaffold, validate as changelog_validate,
};
pub use commands::ci::{
    CiError, PipelineExecution, ci, ci_with_jobs, clean_cache, graph, plan, run_pipeline,
    run_pipeline_with_cache, run_pipeline_with_jobs, run_pipeline_with_mode,
};
pub use commands::doctor::{DoctorError, doctor};
pub use commands::init::{InitError, init, init_standalone};
pub use commands::list::{ListError, list};
pub use commands::release::{
    ReleaseCommandError, manifest as release_manifest, source as release_source,
    verify as release_verify,
};
pub use config::{
    CONFIG_FILE_NAME, MonorepoConfig, PackageConfig, PipelineConfig, TaskConfig,
    WORKSPACE_PACKAGE_NAME, WorkspaceConfig, config_path, render_config,
};
pub use output::OutputMode;
pub use release::{
    Artifact, ReleaseError, ReleaseIdentity, ReleaseManifest, create_manifest, verify_checksums,
    verify_manifest, verify_source,
};
pub use workspace::{Package, PlannedTask, TaskNode, Workspace, WorkspaceError};

/// Every expected failure a `monorelease` command can report.
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
