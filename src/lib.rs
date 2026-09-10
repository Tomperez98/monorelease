//! Core logic for `monore`.
//!
//! Each subcommand lives in its own module under [`commands`], owns its own
//! error vocabulary, and is re-exported at the crate root. Everything here
//! either returns one of its documented failures or panics on a broken
//! invariant; nothing knows about `clap`, stdout, or exit codes — that
//! translation happens once, in `main.rs`.

mod cache;
mod commands;
mod config;
mod discovery;
mod output;
mod runner;
mod scheduler;
#[cfg(test)]
mod testing;
mod workspace;

use std::error::Error as StdError;
use std::fmt;

pub use cache::CacheMode;
pub use commands::ci::{
    CiError, PipelineExecution, ci, ci_with_jobs, clean_cache, graph, plan, run_pipeline,
    run_pipeline_with_cache, run_pipeline_with_jobs, run_pipeline_with_mode,
};
pub use commands::doctor::{DoctorError, doctor};
pub use commands::init::{InitError, init, init_standalone};
pub use commands::list::{ListError, list};
pub use config::{
    CONFIG_FILE_NAME, MonorepoConfig, PackageConfig, PipelineConfig, TaskConfig,
    WORKSPACE_PACKAGE_NAME, WorkspaceConfig, config_path, render_config,
};
pub use output::OutputMode;
pub use workspace::{Package, PlannedTask, TaskNode, Workspace, WorkspaceError};

/// Every expected failure a `monore` command can report.
///
/// The signature of a command documents its whole failure space; this union
/// is what a caller has to handle, and nothing else.
#[derive(Debug)]
pub enum Error {
    Init(InitError),
    Doctor(DoctorError),
    Ci(CiError),
    List(ListError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Init(error) => error.fmt(f),
            Self::Doctor(error) => error.fmt(f),
            Self::Ci(error) => error.fmt(f),
            Self::List(error) => error.fmt(f),
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
