//! Core logic for `monore`.
//!
//! Each subcommand lives in its own module under [`commands`], owns its own
//! error vocabulary, and is re-exported at the crate root. Everything here
//! either returns one of its documented failures or panics on a broken
//! invariant; nothing knows about `clap`, stdout, or exit codes — that
//! translation happens once, in `main.rs`.

mod commands;
mod config;
#[cfg(test)]
mod testing;

use std::error::Error as StdError;
use std::fmt;

pub use commands::doctor::{DoctorError, doctor};
pub use commands::init::{InitError, init};
pub use config::{CONFIG_FILE_NAME, CONFIG_VERSION, MonorepoConfig, config_path, render_config};

/// Every expected failure a `monore` command can report.
///
/// The signature of a command documents its whole failure space; this union
/// is what a caller has to handle, and nothing else.
#[derive(Debug)]
pub enum Error {
    Init(InitError),
    Doctor(DoctorError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Init(error) => error.fmt(f),
            Self::Doctor(error) => error.fmt(f),
        }
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Init(error) => Some(error),
            Self::Doctor(error) => Some(error),
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
