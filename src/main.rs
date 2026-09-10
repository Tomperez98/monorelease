//! `monore` — language agnostic monorepo tooling.
//!
//! This is the transport edge: it parses the command line, calls into
//! [`monorelease`], and maps the single error vocabulary onto stdout, stderr,
//! and an exit code — in exactly one place.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use monorelease::{CacheMode, Error};

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
        /// Create a standalone project instead of a monorepo workspace.
        #[arg(long)]
        standalone: bool,
        /// Command used by the generated standalone build task.
        #[arg(long = "command", num_args = 1, requires = "standalone")]
        command: Vec<String>,
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
        /// Skip reading and writing the local task cache.
        #[arg(long, conflicts_with = "force")]
        no_cache: bool,
        /// Ignore cache hits and refresh successful cache entries.
        #[arg(long, conflicts_with = "no_cache")]
        force: bool,
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
        /// Skip reading and writing the local task cache.
        #[arg(long, conflicts_with = "force")]
        no_cache: bool,
        /// Ignore cache hits and refresh successful cache entries.
        #[arg(long, conflicts_with = "no_cache")]
        force: bool,
        /// Maximum number of independent tasks to execute concurrently.
        #[arg(long, default_value_t = 1)]
        jobs: usize,
    },
    /// Manage the local task cache.
    Cache {
        #[command(subcommand)]
        command: CacheCommands,
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

#[derive(Subcommand)]
enum CacheCommands {
    /// Remove all local cache entries for a workspace.
    Clean {
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

fn cache_mode(no_cache: bool, force: bool) -> CacheMode {
    if no_cache {
        CacheMode::NoCache
    } else if force {
        CacheMode::Force
    } else {
        CacheMode::ReadWrite
    }
}

/// Run `command` to completion, returning the one line to print on success.
///
/// Each step can fail with its own error; `?` short-circuits on the first
/// failure, and the return type records the whole failure space.
fn run(command: Commands) -> Result<String, Error> {
    match command {
        Commands::Init {
            path,
            standalone,
            command,
        } => {
            let written = if standalone {
                monorelease::init_standalone(&path, command)?
            } else {
                monorelease::init(&path)?
            };
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
            no_cache,
            force,
            jobs,
        } => monorelease::run_pipeline_with_cache(
            &path,
            None,
            package.as_deref(),
            &tasks,
            dry_run,
            jobs,
            cache_mode(no_cache, force),
        )
        .map_err(Error::from),
        Commands::Run {
            pipeline,
            path,
            package,
            tasks,
            dry_run,
            no_cache,
            force,
            jobs,
        } => monorelease::run_pipeline_with_cache(
            &path,
            Some(&pipeline),
            package.as_deref(),
            &tasks,
            dry_run,
            jobs,
            cache_mode(no_cache, force),
        )
        .map_err(Error::from),
        Commands::Cache { command } => match command {
            CacheCommands::Clean { path } => monorelease::clean_cache(&path).map_err(Error::from),
        },
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
