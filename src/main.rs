//! `monorelease` — language agnostic project and monorepo tooling.
//!
//! This is the transport edge: it parses the command line, calls into
//! [`monorelease`], and maps the single error vocabulary onto stdout, stderr,
//! and an exit code — in exactly one place.

use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use monorelease::{
    CacheMode, Error, OutputMode, PipelineExecution, ReleaseCommandError, ReleaseError,
    ReleaseIdentity, changelog_notes, changelog_scaffold, changelog_validate, release_manifest,
    release_source, release_verify,
};

#[derive(Parser)]
#[command(
    name = "monorelease",
    version = env!("CARGO_PKG_VERSION"),
    about = "Language agnostic project and monorepo tooling",
    after_help = "Run `monorelease help <command>` for command details."
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
    #[arg(long, default_value_t = default_jobs())]
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
        /// Compatibility spelling for task selection; prefer `monorelease task`.
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
    /// Validate and prepare a changelog.
    Changelog {
        #[command(subcommand)]
        command: ChangelogCommands,
    },
    /// Create and verify provider-neutral release metadata.
    Release {
        #[command(subcommand)]
        command: ReleaseCommands,
    },
}

#[derive(Subcommand)]
enum CacheCommands {
    /// Remove all local cache entries for the selected project.
    Clean,
}

#[derive(Subcommand)]
enum ChangelogCommands {
    /// Validate a changelog file.
    Validate {
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Insert or promote a changelog entry.
    Scaffold {
        #[arg(long)]
        version: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Extract one changelog entry into release notes.
    Notes {
        #[arg(long)]
        version: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

#[derive(Args, Clone, Default)]
struct ReleaseIdentityOptions {
    #[arg(long)]
    tag: Option<String>,
    #[arg(long)]
    commit: Option<String>,
    #[arg(long)]
    repository: Option<String>,
    #[arg(long = "workflow-run")]
    workflow_run: Option<String>,
}

#[derive(Subcommand)]
enum ReleaseCommands {
    /// Verify that the checkout is exactly the requested Git tag and commit.
    Source {
        #[command(flatten)]
        identity: ReleaseIdentityOptions,
    },
    /// Generate BUILD-METADATA.json and SHA256SUMS.
    Manifest {
        #[arg(long, default_value = "dist")]
        directory: PathBuf,
        #[command(flatten)]
        identity: ReleaseIdentityOptions,
    },
    /// Verify release metadata, checksums, and exact artifact inventory.
    Verify {
        #[arg(long, default_value = "dist")]
        directory: PathBuf,
        #[command(flatten)]
        identity: ReleaseIdentityOptions,
    },
}

/// Worker count used when `--jobs` is omitted.
///
/// `available_parallelism` reports the CPUs this process may actually use, so
/// cgroup quotas and CPU affinity are respected rather than overwritten by the
/// physical core count.
fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
}

fn main() -> ExitCode {
    let Cli { path, command } = Cli::parse();

    match run(path, command) {
        Ok(summary) => {
            println!("{summary}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("monorelease: {error}");
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
            default_jobs(),
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
        Some(Commands::Changelog { command }) => match command {
            ChangelogCommands::Validate { file } => {
                changelog_validate(&rooted_path(&path, file, "CHANGELOG.md")).map_err(Error::from)
            }
            ChangelogCommands::Scaffold { version, file } => {
                let version = version
                    .or_else(|| env::var("VERSION").ok())
                    .ok_or_else(|| {
                        Error::Changelog(monorelease::ChangelogError::Invalid(
                            "VERSION is not set (for example VERSION=1.2.3)".to_owned(),
                        ))
                    })?;
                changelog_scaffold(&rooted_path(&path, file, "CHANGELOG.md"), &version)
                    .map_err(Error::from)
            }
            ChangelogCommands::Notes {
                version,
                file,
                output,
            } => {
                let version = version
                    .or_else(|| env::var("RELEASE_TAG").ok())
                    .ok_or_else(|| {
                        Error::Changelog(monorelease::ChangelogError::Invalid(
                            "RELEASE_TAG is not set (for example RELEASE_TAG=v1.2.3)".to_owned(),
                        ))
                    })?;
                changelog_notes(
                    &rooted_path(&path, file, "CHANGELOG.md"),
                    &version,
                    &rooted_path(&path, output, "RELEASE_NOTES.md"),
                )
                .map_err(Error::from)
            }
        },
        Some(Commands::Release { command }) => match command {
            ReleaseCommands::Source { identity } => {
                let identity = identity.resolve();
                let tag = required_identity(identity.release_tag, "--tag or RELEASE_TAG")?;
                let commit = required_identity(identity.source_commit, "--commit or GITHUB_SHA")?;
                release_source(&path, &tag, &commit).map_err(Error::from)
            }
            ReleaseCommands::Manifest {
                directory,
                identity,
            } => release_manifest(
                &rooted_path(&path, Some(directory), "dist"),
                identity.resolve(),
            )
            .map_err(Error::from),
            ReleaseCommands::Verify {
                directory,
                identity,
            } => release_verify(
                &rooted_path(&path, Some(directory), "dist"),
                identity.resolve(),
            )
            .map_err(Error::from),
        },
    }
}

fn rooted_path(root: &Path, path: Option<PathBuf>, default: &str) -> PathBuf {
    let path = path.unwrap_or_else(|| PathBuf::from(default));
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

fn required_identity(value: Option<String>, name: &str) -> Result<String, Error> {
    value.filter(|value| !value.is_empty()).ok_or_else(|| {
        Error::Release(ReleaseCommandError::Release(ReleaseError::Invalid(
            format!("{name} is required"),
        )))
    })
}

impl ReleaseIdentityOptions {
    fn resolve(self) -> ReleaseIdentity {
        ReleaseIdentity {
            repository: self
                .repository
                .or_else(|| env::var("GITHUB_REPOSITORY").ok()),
            release_tag: self.tag.or_else(|| env::var("RELEASE_TAG").ok()),
            source_commit: self.commit.or_else(|| env::var("GITHUB_SHA").ok()),
            workflow_run: self
                .workflow_run
                .or_else(|| env::var("GITHUB_RUN_URL").ok()),
        }
    }
}
