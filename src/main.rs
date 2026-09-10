//! `monore` — language agnostic project and monorepo tooling.
//!
//! This is the transport edge: it parses the command line, calls into
//! [`monorelease`], and maps the single error vocabulary onto stdout, stderr,
//! and an exit code — in exactly one place.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use monorelease::{CacheMode, Error, OutputMode, PipelineExecution};

#[derive(Parser)]
#[command(
    name = "monore",
    version = "0.1.0",
    about = "Language agnostic project and monorepo tooling",
    after_help = "Run `monore help <command>` for command details."
)]
struct Cli {
    /// Project or monorepo directory.
    #[arg(long = "dir", global = true, default_value = ".")]
    path: PathBuf,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Clone, Copy, ValueEnum)]
enum OutputFormat {
    #[value(name = "terminal")]
    Terminal,
    #[value(name = "github-actions")]
    GithubActions,
}

impl From<OutputFormat> for OutputMode {
    fn from(format: OutputFormat) -> Self {
        match format {
            OutputFormat::Terminal => Self::Terminal,
            OutputFormat::GithubActions => Self::GithubActions,
        }
    }
}

#[derive(Args, Clone)]
struct ExecutionOptions {
    #[arg(long)]
    package: Option<String>,
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
    /// Output contract to use for task status and grouping.
    #[arg(long, value_enum, default_value = "terminal")]
    output: OutputFormat,
}

#[derive(Subcommand)]
enum Commands {
    /// Write a fresh manifest in the selected directory.
    Init {
        /// Create a standalone project instead of a monorepo workspace.
        #[arg(long)]
        standalone: bool,
        /// Command argument used by the generated standalone build task.
        #[arg(long = "command", num_args = 1, requires = "standalone")]
        command: Vec<String>,
    },
    /// Run a named pipeline, or the default pipeline when omitted.
    #[command(alias = "ci")]
    Run {
        #[arg(value_name = "PIPELINE")]
        pipeline: Option<String>,
        /// Compatibility spelling for task selection; prefer `monore task`.
        #[arg(long = "task", hide = true)]
        tasks: Vec<String>,
        #[command(flatten)]
        options: ExecutionOptions,
    },
    /// Run one or more tasks and their dependencies.
    Task {
        #[arg(required = true, value_name = "TASK")]
        tasks: Vec<String>,
        #[command(flatten)]
        options: ExecutionOptions,
    },
    /// Validate the selected project or monorepo.
    #[command(alias = "doctor")]
    Check,
    /// List pipelines, packages, tasks, and common commands.
    List,
    /// Print the resolved plan for the default or named pipeline.
    Plan {
        #[arg(value_name = "PIPELINE")]
        pipeline: Option<String>,
        #[arg(long)]
        package: Option<String>,
    },
    /// Print dependency edges for the default or named pipeline.
    Graph {
        #[arg(value_name = "PIPELINE")]
        pipeline: Option<String>,
        #[arg(long)]
        package: Option<String>,
    },
    /// Manage the local task cache.
    Cache {
        #[command(subcommand)]
        command: CacheCommands,
    },
}

#[derive(Subcommand)]
enum CacheCommands {
    /// Remove all local cache entries for the selected project.
    Clean,
}

fn main() -> ExitCode {
    let Cli { path, command } = Cli::parse();

    match run(path, command) {
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

/// Run a parsed command to completion, translating domain results once at the
/// process boundary.
fn run(path: PathBuf, command: Option<Commands>) -> Result<String, Error> {
    match command {
        None => monorelease::run_pipeline_with_cache(
            &path,
            None,
            None,
            &[],
            false,
            1,
            CacheMode::ReadWrite,
        )
        .map_err(Error::from),
        Some(Commands::Init {
            standalone,
            command,
        }) => {
            let written = if standalone {
                monorelease::init_standalone(&path, command)?
            } else {
                monorelease::init(&path)?
            };
            Ok(format!("initialized {}", written.display()))
        }
        Some(Commands::Run {
            pipeline,
            tasks,
            options,
        }) => monorelease::run_pipeline_with_mode(
            &path,
            pipeline.as_deref(),
            options.package.as_deref(),
            &tasks,
            options.dry_run,
            options.jobs,
            PipelineExecution {
                cache: cache_mode(options.no_cache, options.force),
                output: options.output.into(),
            },
        )
        .map_err(Error::from),
        Some(Commands::Task { tasks, options }) => monorelease::run_pipeline_with_mode(
            &path,
            None,
            options.package.as_deref(),
            &tasks,
            options.dry_run,
            options.jobs,
            PipelineExecution {
                cache: cache_mode(options.no_cache, options.force),
                output: options.output.into(),
            },
        )
        .map_err(Error::from),
        Some(Commands::Check) => {
            monorelease::doctor(&path)?;
            Ok(format!("checked {}", path.display()))
        }
        Some(Commands::List) => monorelease::list(&path).map_err(Error::from),
        Some(Commands::Plan { pipeline, package }) => {
            monorelease::plan(&path, pipeline.as_deref(), package.as_deref(), &[])
                .map_err(Error::from)
        }
        Some(Commands::Graph { pipeline, package }) => {
            monorelease::graph(&path, pipeline.as_deref(), package.as_deref(), &[])
                .map_err(Error::from)
        }
        Some(Commands::Cache { command }) => match command {
            CacheCommands::Clean => monorelease::clean_cache(&path).map_err(Error::from),
        },
    }
}
