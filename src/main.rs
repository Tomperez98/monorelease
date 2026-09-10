//! `monore` — language agnostic monorepo tooling.
//!
//! This is the transport edge: it parses the command line, calls into
//! [`monorelease`], and maps the single error vocabulary onto stdout, stderr,
//! and an exit code — in exactly one place.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use monorelease::Error;

#[derive(Parser)]
#[command(
    name = "monore",
    version = "0.1.0",
    about = "Language agnostic monorepo tooling"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Write a fresh monorepo.toml into a directory.
    Init {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Check that a directory holds a healthy monorepo.
    Doctor {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

fn main() -> ExitCode {
    let Cli { command } = Cli::parse();

    match run(command) {
        Ok(summary) => {
            println!("{summary}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("monore: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Run `command` to completion, returning the one line to print on success.
///
/// Each step can fail with its own error; `?` short-circuits on the first
/// failure, and the return type records the whole failure space.
fn run(command: Commands) -> Result<String, Error> {
    match command {
        Commands::Init { path } => {
            let written = monorelease::init(&path)?;
            Ok(format!("initialized {}", written.display()))
        }
        Commands::Doctor { path } => {
            monorelease::doctor(&path)?;
            Ok(format!("checked {}", path.display()))
        }
    }
}
