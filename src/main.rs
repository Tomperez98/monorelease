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
    /// Run the workspace's default pipeline.
    Ci {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        package: Option<String>,
        #[arg(long = "task")]
        tasks: Vec<String>,
        #[arg(long)]
        dry_run: bool,
        /// Maximum number of independent tasks to execute concurrently.
        #[arg(long, default_value_t = 1)]
        jobs: usize,
    },
    /// Run a named workspace pipeline.
    Run {
        pipeline: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        package: Option<String>,
        #[arg(long = "task")]
        tasks: Vec<String>,
        #[arg(long)]
        dry_run: bool,
        /// Maximum number of independent tasks to execute concurrently.
        #[arg(long, default_value_t = 1)]
        jobs: usize,
    },
    /// Print the resolved task plan.
    Plan {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        pipeline: Option<String>,
        #[arg(long)]
        package: Option<String>,
        #[arg(long = "task")]
        tasks: Vec<String>,
    },
    /// Print task dependency edges.
    Graph {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        pipeline: Option<String>,
        #[arg(long)]
        package: Option<String>,
        #[arg(long = "task")]
        tasks: Vec<String>,
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
        Commands::Ci {
            path,
            package,
            tasks,
            dry_run,
            jobs,
        } => monorelease::ci_with_jobs(&path, package.as_deref(), &tasks, dry_run, jobs)
            .map_err(Error::from),
        Commands::Run {
            pipeline,
            path,
            package,
            tasks,
            dry_run,
            jobs,
        } => monorelease::run_pipeline_with_jobs(
            &path,
            Some(&pipeline),
            package.as_deref(),
            &tasks,
            dry_run,
            jobs,
        )
        .map_err(Error::from),
        Commands::Plan {
            path,
            pipeline,
            package,
            tasks,
        } => monorelease::plan(&path, pipeline.as_deref(), package.as_deref(), &tasks)
            .map_err(Error::from),
        Commands::Graph {
            path,
            pipeline,
            package,
            tasks,
        } => monorelease::graph(&path, pipeline.as_deref(), package.as_deref(), &tasks)
            .map_err(Error::from),
    }
}
