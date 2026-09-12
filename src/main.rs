//! `mono` — language-agnostic root-project tooling.
//!
//! This is the transport edge: it parses the command line, calls into
//! [`mono`], and maps the single error vocabulary onto stdout, stderr,
//! and an exit code — in exactly one place.
//!
//! Every argument is validated while parsing, so [`dispatch`] only ever sees
//! well-formed commands and its failure space is exactly the library's. The
//! exit code this edge publishes is:
//!
//! | code | meaning                                                      |
//! | ---- | ------------------------------------------------------------ |
//! | `0`  | the command succeeded                                        |
//! | `1`  | the command was understood and failed                        |
//! | `2`  | the command line was wrong; emitted by `clap` while parsing  |
//! | `3`  | `mono` or its environment failed                      |

#![allow(clippy::result_large_err)]

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::builder::NonEmptyStringValueParser;
use clap::{Args, Parser, Subcommand, ValueEnum};
use mono::{
    CacheMode, CancellationToken, ChangelogError, CiError, DEFAULT_CHANGELOG_PATH,
    DEFAULT_RELEASE_DIRECTORY, DEFAULT_RELEASE_NOTES_PATH, DoctorError, Error, InitError,
    ListError, OutputMode, PipelineExecution, ProjectError, ReleaseCommandError, ReleaseError,
    ReleaseIdentity, ReleaseNotesTarget, SchedulerError, changelog_prepare,
    changelog_prepare_from_git, changelog_release_notes, changelog_validate, release_manifest,
    release_source, release_verify,
};

/// One-line value proposition: the reader's task, in the README's words.
const ABOUT: &str =
    "Run a project's build, test, lint, and release commands from one root mono.toml";

/// What `--help` adds beyond `-h`: what mono is, and what it deliberately is not.
const LONG_ABOUT: &str = "Mono is a task orchestrator, not a package manager or workspace \
detector. You declare the commands and their dependencies; Mono validates the graph, runs \
independent tasks concurrently, and gives local development and CI the same execution contract.";

/// The screen every reader sees first. It carries the two things `--help` would
/// otherwise lose — the exit-code contract and commands that actually run — and
/// deliberately repeats no part of the option list. Written flush left because a
/// string literal's own indentation is what the reader ends up looking at.
const AFTER_HELP: &str = "\
Exit codes:
  0  success
  1  the command was understood and failed (red pipeline, invalid manifest)
  2  the command line was malformed
  3  mono or its environment failed (unreadable manifest, git unavailable)

Examples:
  mono init        write a starter mono.toml in this directory
  mono list        see what this project can run
  mono plan        see what the default pipeline would do, running nothing
  mono run ci      run the ci pipeline
  mono task test   run one task and its dependencies, skipping the rest

With no command, mono runs the project's default pipeline.
Run `mono help <command>` for command details.";

#[derive(Parser)]
#[command(
    name = "mono",
    version = env!("CARGO_PKG_VERSION"),
    about = ABOUT,
    long_about = LONG_ABOUT,
    after_help = AFTER_HELP
)]
struct Cli {
    /// Directory to search from; mono walks up to the nearest mono.toml.
    #[arg(long = "dir", global = true, default_value = ".")]
    root: PathBuf,
    /// Output contract for command summaries and execution events.
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Text)]
    output: OutputFormat,
    /// Task presentation for bare `mono`; execution subcommands expose their own `--ui`.
    #[arg(long, value_enum)]
    ui: Option<UiFormat>,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Clone, Copy, ValueEnum)]
enum OutputFormat {
    /// Human-readable summaries and live task output
    #[value(name = "text")]
    Text,
    /// Newline-delimited JSON, one document per summary or execution event
    #[value(name = "json")]
    Json,
}

#[derive(Clone, Copy, ValueEnum)]
enum UiFormat {
    /// Full-screen task view when a terminal is attached; prefixed lines otherwise
    #[value(name = "auto")]
    Auto,
    /// Full-screen task view; prefixed lines when no terminal is attached
    #[value(name = "tui")]
    Tui,
    /// One prefixed line at a time, without a terminal
    #[value(name = "stream")]
    Stream,
}

impl From<OutputFormat> for OutputMode {
    fn from(format: OutputFormat) -> Self {
        match format {
            OutputFormat::Text => Self::Terminal,
            OutputFormat::Json => Self::Json,
        }
    }
}

/// Flags that shape an execution, grouped under their own `--help` heading so
/// they never interleave with the global flags shared by every subcommand.
#[derive(Args)]
#[command(next_help_heading = "Execution options")]
struct ExecutionOptions {
    /// Print the resolved plan and run no commands
    #[arg(long)]
    dry_run: bool,
    /// Skip reading and writing the local task cache (conflicts with --force)
    #[arg(long, conflicts_with = "force")]
    no_cache: bool,
    /// Ignore cache hits and refresh successful cache entries (conflicts with --no-cache)
    #[arg(long, conflicts_with = "no_cache")]
    force: bool,
    /// Maximum number of independent tasks to execute concurrently
    #[arg(long, default_value_t = default_jobs(), value_parser = parse_jobs)]
    jobs: usize,
    /// Task presentation for this execution
    #[arg(long, value_enum)]
    ui: Option<UiFormat>,
}

impl Default for ExecutionOptions {
    /// The contract used when no subcommand or flag selects one: the default
    /// pipeline, terminal output, a read/write cache, and machine parallelism.
    fn default() -> Self {
        Self {
            dry_run: false,
            no_cache: false,
            force: false,
            jobs: default_jobs(),
            ui: None,
        }
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Write a starter mono.toml in the selected directory; refuses to overwrite one
    Init,
    /// Run a named pipeline, or the project's default pipeline
    #[command(
        alias = "ci",
        long_about = "Run a named pipeline end to end: every task it lists, in dependency order.\n\
\n\
With no PIPELINE, runs the project's `default_pipeline`. This is what a bare `mono` does.",
        after_help = "Example: mono run ci --ui stream"
    )]
    Run {
        /// Pipeline to run; defaults to the manifest's `default_pipeline`
        #[arg(value_name = "PIPELINE", value_parser = NonEmptyStringValueParser::new())]
        pipeline: Option<String>,
        /// Compatibility spelling for task selection; prefer `mono task`.
        #[arg(long = "task", hide = true, value_parser = NonEmptyStringValueParser::new())]
        tasks: Vec<String>,
        #[command(flatten)]
        options: ExecutionOptions,
    },
    /// Run specific tasks and their dependencies, skipping the rest
    #[command(
        long_about = "Run one or more tasks, plus everything they depend on, without running the \
rest of the pipeline. Use this to re-run one step of a pipeline.\n\
\n\
Matrix tasks are addressed as `name[dimension=value]`.",
        after_help = "Example: mono task 'build[os=linux]'"
    )]
    Task {
        /// Task names to run, with their dependencies
        #[arg(
            required = true,
            value_name = "TASK",
            value_parser = NonEmptyStringValueParser::new()
        )]
        tasks: Vec<String>,
        #[command(flatten)]
        options: ExecutionOptions,
    },
    /// Validate mono.toml and the complete task graph
    #[command(
        alias = "doctor",
        long_about = "Validate mono.toml and the complete task graph before anything runs. This is \
the check `mono` performs before every execution.\n\nAlso available as `mono doctor`."
    )]
    Check,
    /// List this project's pipelines, tasks, and suggested commands
    List,
    /// Print the resolved plan for the default or named pipeline
    Plan {
        /// Pipeline to resolve; defaults to the manifest's `default_pipeline`
        #[arg(value_name = "PIPELINE", value_parser = NonEmptyStringValueParser::new())]
        pipeline: Option<String>,
    },
    /// Print dependency edges for the default or named pipeline
    Graph {
        /// Pipeline to graph; defaults to the manifest's `default_pipeline`
        #[arg(value_name = "PIPELINE", value_parser = NonEmptyStringValueParser::new())]
        pipeline: Option<String>,
    },
    /// Manage the local task cache.
    Cache {
        #[command(subcommand)]
        command: CacheCommands,
    },
    /// Prepare, check, and extract release notes from a changelog.
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
    /// Check a changelog file against Mono's release conventions.
    #[command(alias = "validate")]
    Check {
        /// Changelog file to check
        #[arg(long, default_value = DEFAULT_CHANGELOG_PATH)]
        file: PathBuf,
    },
    /// Prepare the next changelog entry, or explicitly prepare `unreleased`.
    #[command(alias = "scaffold")]
    Prepare {
        /// Version to prepare; omitted versions increment the newest patch release.
        #[arg(value_name = "VERSION", value_parser = NonEmptyStringValueParser::new())]
        version: Option<String>,
        /// Compatibility spelling for the pre-0.2 `--version` flag.
        #[arg(long = "version", hide = true, conflicts_with = "version", value_parser = NonEmptyStringValueParser::new())]
        version_flag: Option<String>,
        /// Date to put in a new entry; defaults to today's UTC date.
        #[arg(long, value_name = "YYYY-MM-DD")]
        date: Option<String>,
        /// First Git ref to include in editable merge bullets.
        #[arg(long, requires = "to", value_parser = NonEmptyStringValueParser::new())]
        from: Option<String>,
        /// Last Git ref to include in editable merge bullets.
        #[arg(long, requires = "from", value_parser = NonEmptyStringValueParser::new())]
        to: Option<String>,
        /// URL template for PR bullets, containing `{number}`.
        #[arg(long, value_parser = NonEmptyStringValueParser::new())]
        pull_request_url: Option<String>,
        /// Changelog file to edit
        #[arg(long, default_value = DEFAULT_CHANGELOG_PATH)]
        file: PathBuf,
    },
    /// Write release notes from the newest changelog entry.
    #[command(name = "release-notes", alias = "notes")]
    ReleaseNotes {
        /// Release version to require; defaults to the newest changelog entry.
        #[arg(value_name = "VERSION", value_parser = NonEmptyStringValueParser::new())]
        version: Option<String>,
        /// Compatibility spelling for the pre-0.2 `--version` flag.
        #[arg(long = "version", hide = true, conflicts_with = "version", value_parser = NonEmptyStringValueParser::new())]
        version_flag: Option<String>,
        /// Release tag to require.
        #[arg(long = "release-tag", value_parser = NonEmptyStringValueParser::new())]
        release_tag: Option<String>,
        /// Changelog file to read
        #[arg(long, default_value = DEFAULT_CHANGELOG_PATH)]
        file: PathBuf,
        /// File to write the extracted notes to
        #[arg(
            long = "output-file",
            alias = "notes-output",
            default_value = DEFAULT_RELEASE_NOTES_PATH
        )]
        output_file: PathBuf,
    },
}

/// Release metadata that is optional per command and must be supplied through
/// explicit flags. [`ReleaseIdentityOptions::resolve`] is a pure transformation.
#[derive(Args, Default)]
#[command(next_help_heading = "Release identity")]
struct ReleaseIdentityOptions {
    /// Release tag to record
    #[arg(long)]
    tag: Option<String>,
    /// Commit the tag points at
    #[arg(long)]
    commit: Option<String>,
    /// Repository the release belongs to
    #[arg(long)]
    repository: Option<String>,
    // An annotated tag has one object and a lightweight tag has none. CI exports the
    // empty string for a lightweight tag, so `resolve` drops a set-but-empty value.
    /// Annotated tag object; empty for a lightweight tag
    #[arg(long = "tag-object")]
    tag_object: Option<String>,
    /// URL of the workflow run that produced the release
    #[arg(long = "workflow-run")]
    workflow_run: Option<String>,
}

impl ReleaseIdentityOptions {
    fn resolve(self) -> ReleaseIdentity {
        ReleaseIdentity {
            repository: non_empty(self.repository),
            release_tag: non_empty(self.tag),
            source_commit: non_empty(self.commit),
            tag_object: non_empty(self.tag_object),
            workflow_run: non_empty(self.workflow_run),
        }
    }
}

/// Treat a variable that is set but empty as absent. The release workflows
/// export `RELEASE_TAG_OBJECT` unconditionally, so a lightweight tag reaches
/// this as `Some("")`; comparing that against a real object would fail.
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

/// Identity for `release source`, where a tag and commit are mandatory.
#[derive(Args)]
#[command(next_help_heading = "Release identity")]
struct SourceIdentityOptions {
    /// Git tag to verify.
    #[arg(
        long,
        required = true,
        value_parser = NonEmptyStringValueParser::new()
    )]
    tag: String,
    /// Commit the tag must point at.
    #[arg(
        long,
        required = true,
        value_parser = NonEmptyStringValueParser::new()
    )]
    commit: String,
}

#[derive(Subcommand)]
enum ReleaseCommands {
    /// Verify that the checkout is exactly the requested Git tag and commit.
    Source {
        #[command(flatten)]
        identity: SourceIdentityOptions,
    },
    /// Generate BUILD-METADATA.json and SHA256SUMS
    Manifest {
        /// Release directory to write the metadata into, below the root
        #[arg(long = "dist", alias = "directory", default_value = DEFAULT_RELEASE_DIRECTORY)]
        dist: PathBuf,
        /// File containing one expected artifact path per line
        #[arg(long)]
        expected: Option<PathBuf>,
        #[command(flatten)]
        identity: ReleaseIdentityOptions,
    },
    /// Verify release metadata, checksums, and exact artifact inventory
    Verify {
        /// Release directory to read the metadata from, below the root
        #[arg(long = "dist", alias = "directory", default_value = DEFAULT_RELEASE_DIRECTORY)]
        dist: PathBuf,
        /// File containing one expected artifact path per line.
        #[arg(long)]
        expected: Option<PathBuf>,
        #[command(flatten)]
        identity: ReleaseIdentityOptions,
    },
}

/// Exit code for a command that was understood and failed.
const EXIT_FAILED: u8 = 1;

/// Exit code for a malformed command line.
///
/// `clap` emits this while parsing, before [`run`] is reached. It is repeated
/// here for the one usage failure the library owns.
const EXIT_USAGE: u8 = 2;

/// Exit code for `mono` or its environment failing.
const EXIT_TOOL: u8 = 3;

/// Worker count used when `--jobs` is omitted.
///
/// `available_parallelism` reports the CPUs this process may actually use, so
/// cgroup quotas and CPU affinity are respected rather than overwritten by the
/// physical core count. A process that cannot learn its own limit runs one task
/// at a time: that is a deliberate degraded default, not a silent bug.
fn default_jobs() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
}

/// Parse `--jobs`, rejecting a worker count the scheduler cannot honor.
///
/// The scheduler requires at least one worker, so `--jobs 0` is a malformed
/// command line and is rejected here rather than reported as a failed run.
fn parse_jobs(value: &str) -> Result<usize, String> {
    let jobs: usize = value
        .parse()
        .map_err(|_| format!("`{value}` is not a whole number of workers"))?;
    if jobs == 0 {
        return Err("must be greater than zero".to_owned());
    }
    Ok(jobs)
}

fn main() -> ExitCode {
    let Cli {
        root,
        output,
        ui,
        command,
    } = Cli::parse();
    let output = output.into();

    let cancellation = CancellationToken::new();
    if let Err(error) = ctrlc::set_handler({
        let cancellation = cancellation.clone();
        move || cancellation.cancel()
    }) {
        eprintln!("mono: could not install Ctrl-C handler: {error}");
        return ExitCode::from(EXIT_TOOL);
    }

    let code = match dispatch(root, output, ui, command, cancellation) {
        Ok(summary) => emit_summary(&mut io::stdout().lock(), &summary),
        Err(error) if output == OutputMode::Json => {
            emit_error(&mut io::stdout().lock(), &error, output)
        }
        Err(error) => emit_error(&mut io::stderr().lock(), &error, output),
    };
    ExitCode::from(code)
}

/// Write a command's summary to `sink`.
///
/// A consumer that hangs up early — `mono plan | head` — closes the pipe
/// before this write lands. That is an expected outcome, not a broken
/// invariant, so it is reported as success instead of panicking the way
/// `println!` would over the process-global handle. Taking `sink` as an
/// argument keeps the mapping testable without spawning a process.
///
/// An empty summary (JSON mode) is silently skipped so the
/// newline-delimited JSON stream never contains a blank line.
fn emit_summary(sink: &mut impl Write, summary: &str) -> u8 {
    if summary.is_empty() {
        return 0;
    }
    match writeln!(sink, "{summary}") {
        Ok(()) => 0,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => 0,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "mono: {error}");
            EXIT_TOOL
        }
    }
}

/// Report a failed command on `sink` and map it onto an exit code.
///
/// A closed stderr must never mask the failure, so the write is best effort and
/// the code comes from the error alone.
fn emit_error(sink: &mut impl Write, error: &Error, output: OutputMode) -> u8 {
    let code = exit_code(error);
    if output == OutputMode::Json {
        let _ = sink.write_all(error_document(error).as_bytes());
        let _ = sink.write_all(b"\n");
    } else {
        let _ = writeln!(sink, "mono: {error}");
    }
    code
}

/// Map the single error vocabulary onto this CLI's exit code, once.
///
/// The vocabulary splits into two kinds of failure: the command was understood
/// and the request failed (`1`), or `mono` and its environment failed to
/// carry it out (`3`). `clap` rejects a malformed command line with `2` before
/// this runs; the one usage failure the library owns is classified here too.
fn exit_code(error: &Error) -> u8 {
    match error {
        Error::Init(error) => init_exit_code(error),
        Error::Ci(CiError::InvalidJobs) => EXIT_USAGE,
        Error::Ci(CiError::Scheduler(error)) => scheduler_exit_code(error),
        Error::Ci(CiError::Json { .. }) => EXIT_TOOL,
        Error::Ci(CiError::Project(error))
        | Error::Doctor(DoctorError::Project(error))
        | Error::List(ListError::Project(error)) => project_exit_code(error),
        Error::Changelog(error) => changelog_exit_code(error),
        Error::Release(ReleaseCommandError::Release(error)) => release_exit_code(error),
        // A serialization failure means mono could not produce the requested
        // output, exactly like `CiError::Json`; both are tool failures.
        Error::List(ListError::Json { .. }) => EXIT_TOOL,
    }
}

/// Map a project error onto this CLI's exit code.
///
/// `ProjectError::Io` means the manifest could not be read — a tool or
/// environment failure. Everything else is a rejected request the caller
/// can fix.
fn project_exit_code(error: &mono::ProjectError) -> u8 {
    match error {
        // A manifest that could not be read is a `mono` or environment failure.
        ProjectError::Io { .. } => EXIT_TOOL,
        // Every other variant is a manifest that WAS read and rejected, which the
        // caller can fix. This list is exhaustive on purpose: a `_` arm would
        // silently assign exit `1` to any future variant, including one that is
        // really an environment failure. Adding a variant must force a decision
        // about which bucket it belongs in.
        ProjectError::Parse { .. }
        | ProjectError::MissingRoot { .. }
        | ProjectError::InvalidManifest { .. }
        | ProjectError::InvalidProject { .. }
        | ProjectError::UnknownPipeline { .. }
        | ProjectError::InvalidTaskName { .. }
        | ProjectError::InvalidTask { .. }
        | ProjectError::MissingTask { .. }
        | ProjectError::InvalidTaskReference { .. }
        | ProjectError::TaskCycle { .. }
        | ProjectError::UnsupportedSchema { .. }
        | ProjectError::TaskDirectory { .. } => EXIT_FAILED,
    }
}

fn init_exit_code(error: &InitError) -> u8 {
    match error {
        // The requested directory or file could not be written.
        InitError::CreateDir { .. } | InitError::WriteConfig { .. } => EXIT_TOOL,
        InitError::AlreadyInitialized(_) => EXIT_FAILED,
    }
}

fn scheduler_exit_code(error: &SchedulerError) -> u8 {
    match error {
        // A task ran and reported failure. The pipeline being red is a result,
        // not a malfunction, so it is the caller's failure and not the tool's.
        SchedulerError::Task(_) | SchedulerError::Cancelled => EXIT_FAILED,
        // The scheduler never got far enough to run the pipeline.
        SchedulerError::Cache(_)
        | SchedulerError::Output(_)
        | SchedulerError::UnresolvedDependency { .. }
        | SchedulerError::NoReadyWork => EXIT_TOOL,
    }
}

fn changelog_exit_code(error: &ChangelogError) -> u8 {
    match error {
        ChangelogError::Read { .. }
        | ChangelogError::Write { .. }
        | ChangelogError::Command { .. }
        | ChangelogError::CommandFailed { .. } => EXIT_TOOL,
        ChangelogError::Invalid(_) => EXIT_FAILED,
    }
}

fn release_exit_code(error: &ReleaseError) -> u8 {
    match error {
        // `Command` conflates a missing `git` with `git` reporting failure; both
        // leave the tool unable to establish what it was asked to verify, so
        // every read, write, or subprocess failure lands in the tool bucket.
        ReleaseError::Read { .. } | ReleaseError::Write { .. } | ReleaseError::Command(_) => {
            EXIT_TOOL
        }
        ReleaseError::Invalid(_) => EXIT_FAILED,
    }
}

/// Resolve the two mutually exclusive cache flags into one mode.
///
/// `--no-cache` and `--force` are declared `conflicts_with` each other, so
/// `clap` never produces both. The assertion states that invariant rather than
/// silently preferring one flag if the declaration is ever dropped.
fn cache_mode(no_cache: bool, force: bool) -> CacheMode {
    assert!(
        !(no_cache && force),
        "--no-cache and --force are mutually exclusive"
    );
    if no_cache {
        CacheMode::NoCache
    } else if force {
        CacheMode::Force
    } else {
        CacheMode::ReadWrite
    }
}

/// No explicit task selection: the plan is exactly the selected pipeline.
const NO_TASK_FILTER: &[String] = &[];

/// Run a parsed command to completion, translating domain results once at the
/// process boundary.
///
/// Each subcommand owns a small helper below, so this match reads as a dispatch
/// table and each helper documents the failure space it can surface.
fn dispatch(
    root: PathBuf,
    output: OutputMode,
    root_ui: Option<UiFormat>,
    command: Option<Commands>,
    cancellation: CancellationToken,
) -> Result<String, Error> {
    match command {
        None => run_default_pipeline(&root, output, root_ui, cancellation),
        Some(Commands::Init) => run_init(&root, output),
        Some(Commands::Run {
            pipeline,
            tasks,
            options,
        }) => execute_pipeline(
            &root,
            pipeline.as_deref(),
            &tasks,
            options,
            output,
            root_ui,
            cancellation,
        ),
        Some(Commands::Task { tasks, options }) => {
            execute_pipeline(&root, None, &tasks, options, output, root_ui, cancellation)
        }
        Some(Commands::Check) => run_check(&root, output),
        Some(Commands::List) => run_list(&root, output),
        Some(Commands::Plan { pipeline }) => run_plan(&root, pipeline.as_deref(), output),
        Some(Commands::Graph { pipeline }) => run_graph(&root, pipeline.as_deref(), output),
        Some(Commands::Cache { command }) => run_cache(&root, command, output),
        Some(Commands::Changelog { command }) => run_changelog(&root, command, output),
        Some(Commands::Release { command }) => run_release(&root, command, output),
    }
}

/// Run the default pipeline when no subcommand is given.
fn run_default_pipeline(
    root: &Path,
    output: OutputMode,
    ui: Option<UiFormat>,
    cancellation: CancellationToken,
) -> Result<String, Error> {
    execute_pipeline(
        root,
        None,
        NO_TASK_FILTER,
        ExecutionOptions::default(),
        output,
        ui,
        cancellation,
    )
}

fn run_init(root: &Path, output: OutputMode) -> Result<String, Error> {
    let written = mono::init(root)?;
    Ok(success_document(
        output,
        "init",
        format!("initialized {}", written.display()),
    ))
}

/// Run one pipeline or task selection with the parsed execution options.
fn execute_pipeline(
    root: &Path,
    pipeline: Option<&str>,
    tasks: &[String],
    options: ExecutionOptions,
    output: OutputMode,
    root_ui: Option<UiFormat>,
    cancellation: CancellationToken,
) -> Result<String, Error> {
    let ui = options.ui.or(root_ui).unwrap_or(UiFormat::Auto);
    Ok(mono::run_pipeline_with_mode(
        root,
        pipeline,
        tasks,
        options.dry_run,
        options.jobs,
        PipelineExecution {
            cache: cache_mode(options.no_cache, options.force),
            output: resolve_execution_output(output, ui),
            cancellation,
        },
    )?)
}

fn resolve_execution_output(output: OutputMode, ui: UiFormat) -> OutputMode {
    if output == OutputMode::Json {
        return OutputMode::Json;
    }
    match ui {
        UiFormat::Stream => OutputMode::Stream,
        UiFormat::Tui | UiFormat::Auto => {
            if interactive_terminal() {
                OutputMode::Tui
            } else {
                OutputMode::Stream
            }
        }
    }
}

fn interactive_terminal() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal() && std::env::var_os("CI").is_none()
}

fn run_check(root: &Path, output: OutputMode) -> Result<String, Error> {
    let project = mono::Project::load(root).map_err(mono::DoctorError::from)?;
    Ok(check_document(output, &project.root))
}

fn run_list(root: &Path, output: OutputMode) -> Result<String, Error> {
    Ok(mono::list_with_output(root, output)?)
}

fn run_plan(root: &Path, pipeline: Option<&str>, output: OutputMode) -> Result<String, Error> {
    Ok(mono::plan_with_output(
        root,
        pipeline,
        NO_TASK_FILTER,
        output,
    )?)
}

fn run_graph(root: &Path, pipeline: Option<&str>, output: OutputMode) -> Result<String, Error> {
    Ok(mono::graph_with_output(
        root,
        pipeline,
        NO_TASK_FILTER,
        output,
    )?)
}

fn run_cache(root: &Path, command: CacheCommands, output: OutputMode) -> Result<String, Error> {
    match command {
        CacheCommands::Clean => Ok(success_document(
            output,
            "cache_clean",
            mono::clean_cache(root)?,
        )),
    }
}

fn run_changelog(
    root: &Path,
    command: ChangelogCommands,
    output: OutputMode,
) -> Result<String, Error> {
    let (kind, message) = match command {
        ChangelogCommands::Check { file } => (
            "changelog_check",
            changelog_validate(&resolve_path(root, file))?,
        ),
        ChangelogCommands::Prepare {
            version,
            version_flag,
            date,
            from,
            to,
            pull_request_url,
            file,
        } => {
            let path = resolve_path(root, file);
            let version = version.or(version_flag);
            let message = match (from.as_deref(), to.as_deref()) {
                (Some(from), Some(to)) => changelog_prepare_from_git(
                    &path,
                    version.as_deref(),
                    date.as_deref(),
                    from,
                    to,
                    pull_request_url.as_deref(),
                )?,
                (None, None) => changelog_prepare(&path, version.as_deref(), date.as_deref())?,
                _ => unreachable!("clap requires --from and --to together"),
            };
            ("changelog_prepare", message)
        }
        ChangelogCommands::ReleaseNotes {
            version,
            version_flag,
            release_tag,
            file,
            output_file,
        } => {
            let version = version.or(version_flag);
            let target = ReleaseNotesTarget::parse(version.as_deref(), release_tag.as_deref())?;
            (
                "changelog_release_notes",
                changelog_release_notes(
                    &resolve_path(root, file),
                    target,
                    &resolve_path(root, output_file),
                )?,
            )
        }
    };
    Ok(success_document(output, kind, message))
}

fn run_release(root: &Path, command: ReleaseCommands, output: OutputMode) -> Result<String, Error> {
    let (kind, message) = match command {
        ReleaseCommands::Source { identity } => (
            "release_source",
            release_source(root, &identity.tag, &identity.commit)?,
        ),
        ReleaseCommands::Manifest {
            dist,
            expected,
            identity,
        } => {
            let directory = resolve_path(root, dist);
            let expected = expected.map(|path| resolve_path(root, path));
            (
                "release_manifest",
                release_manifest(&directory, identity.resolve(), expected.as_deref())?,
            )
        }
        ReleaseCommands::Verify {
            dist,
            expected,
            identity,
        } => {
            let directory = resolve_path(root, dist);
            let expected = expected.map(|path| resolve_path(root, path));
            (
                "release_verify",
                release_verify(&directory, identity.resolve(), expected.as_deref())?,
            )
        }
    };
    Ok(success_document(output, kind, message))
}

/// The success document every non-execution command returns in JSON mode.
#[derive(serde::Serialize)]
struct SuccessDocument {
    schema: u32,
    kind: &'static str,
    status: &'static str,
    message: String,
}

/// The `check` document, which reports the resolved project root.
#[derive(serde::Serialize)]
struct CheckDocument {
    schema: u32,
    kind: &'static str,
    status: &'static str,
    project: String,
}

/// The failure document for JSON mode.
#[derive(serde::Serialize)]
struct ErrorDocument<'a> {
    schema: u32,
    kind: &'static str,
    code: u8,
    message: &'a str,
}

/// Serialize a document that contains only serializable fields.
fn serialize(document: &impl serde::Serialize) -> String {
    serde_json::to_string(document).expect("output documents contain only serializable fields")
}

fn error_document(error: &Error) -> String {
    serialize(&ErrorDocument {
        schema: mono::JSON_OUTPUT_SCHEMA,
        kind: "error",
        code: exit_code(error),
        message: &error.to_string(),
    })
}

fn success_document(output: OutputMode, kind: &'static str, message: String) -> String {
    if output == OutputMode::Json {
        serialize(&SuccessDocument {
            schema: mono::JSON_OUTPUT_SCHEMA,
            kind,
            status: "ok",
            message,
        })
    } else {
        message
    }
}

fn check_document(output: OutputMode, root: &Path) -> String {
    if output == OutputMode::Json {
        serialize(&CheckDocument {
            schema: mono::JSON_OUTPUT_SCHEMA,
            kind: "check",
            status: "ok",
            project: root.display().to_string(),
        })
    } else {
        format!("checked {}", root.display())
    }
}

/// Resolve a command's path argument against the selected root directory.
///
/// An absolute path is already resolved; a relative one is joined onto `root`,
/// so every path handed to the library is rooted in the same place. Each flag's
/// default is declared on the flag itself, which keeps it visible in `--help`
/// and defined in exactly one place.
fn resolve_path(root: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mono::{CiError, DoctorError, ListError, ProjectError};

    /// A writer that fails every write, standing in for a consumer that hung up
    /// (`BrokenPipe`) or a disk that is full (`StorageFull`).
    struct FailingWriter(io::ErrorKind);

    impl Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(self.0, "writer is unavailable"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn missing_root() -> ProjectError {
        ProjectError::MissingRoot {
            start: PathBuf::from("."),
        }
    }

    fn missing_declared_directory() -> ProjectError {
        // A task `cwd` that the manifest declares but the worktree lacks.
        ProjectError::TaskDirectory {
            task: "build".to_owned(),
            path: PathBuf::from("packages/api/missing"),
            source: io::Error::new(io::ErrorKind::NotFound, "missing"),
        }
    }

    fn unreadable_manifest() -> ProjectError {
        // The manifest file could not be read — a tool/environment failure.
        ProjectError::Io {
            path: PathBuf::from("mono.toml"),
            source: io::Error::new(io::ErrorKind::PermissionDenied, "access denied"),
        }
    }

    #[test]
    fn a_summary_is_written_once_with_a_trailing_newline() {
        let mut sink = Vec::new();

        assert_eq!(emit_summary(&mut sink, "summary: 1 completed"), 0);
        assert_eq!(String::from_utf8(sink).unwrap(), "summary: 1 completed\n");
    }

    #[test]
    fn an_empty_summary_writes_nothing_and_returns_zero() {
        let mut sink = Vec::new();

        assert_eq!(emit_summary(&mut sink, ""), 0);
        assert!(sink.is_empty(), "empty summary must write nothing");
    }

    #[test]
    fn a_consumer_that_hangs_up_is_not_a_failure() {
        let mut sink = FailingWriter(io::ErrorKind::BrokenPipe);

        assert_eq!(emit_summary(&mut sink, "summary: 1 completed"), 0);
    }

    #[test]
    fn a_write_failure_that_is_not_a_hangup_is_a_tool_failure() {
        let mut sink = FailingWriter(io::ErrorKind::StorageFull);

        assert_eq!(emit_summary(&mut sink, "summary: 1 completed"), EXIT_TOOL);
    }

    #[test]
    fn a_failure_is_reported_on_the_sink_with_its_exit_code() {
        let error = Error::Ci(CiError::InvalidJobs);
        let mut sink = Vec::new();

        assert_eq!(
            emit_error(&mut sink, &error, OutputMode::Terminal),
            EXIT_USAGE
        );
        assert_eq!(
            String::from_utf8(sink).unwrap(),
            "mono: --jobs must be greater than zero\n"
        );
    }

    #[test]
    fn a_broken_stderr_does_not_mask_the_failure() {
        let error = Error::Doctor(DoctorError::Project(missing_root()));
        let mut sink = FailingWriter(io::ErrorKind::BrokenPipe);

        assert_eq!(
            emit_error(&mut sink, &error, OutputMode::Terminal),
            EXIT_FAILED
        );
    }

    #[test]
    fn requests_that_were_understood_fail_with_one() {
        for error in [
            Error::Init(InitError::AlreadyInitialized(PathBuf::from("mono.toml"))),
            Error::Init(InitError::AlreadyInitialized(PathBuf::from("mono.toml"))),
            Error::Doctor(DoctorError::Project(missing_root())),
            Error::Doctor(DoctorError::Project(missing_declared_directory())),
            Error::List(ListError::Project(ProjectError::MissingTask {
                task: "missing".to_owned(),
                suggestion: None,
            })),
            Error::Changelog(ChangelogError::Invalid("no entries".to_owned())),
            Error::Release(ReleaseCommandError::Release(ReleaseError::Invalid(
                "checksum mismatch".to_owned(),
            ))),
        ] {
            assert_eq!(exit_code(&error), EXIT_FAILED, "{error}");
        }
    }

    #[test]
    fn environment_failures_exit_with_three() {
        for error in [
            Error::Init(InitError::WriteConfig {
                path: PathBuf::from("mono.toml"),
                source: io::Error::new(io::ErrorKind::PermissionDenied, "denied"),
            }),
            Error::Doctor(DoctorError::Project(unreadable_manifest())),
            Error::Changelog(ChangelogError::Read {
                path: PathBuf::from("CHANGELOG.md"),
                source: io::Error::new(io::ErrorKind::NotFound, "missing"),
            }),
            Error::Release(ReleaseCommandError::Release(ReleaseError::Command(
                "`git rev-parse` failed".to_owned(),
            ))),
            Error::Ci(CiError::Scheduler(Box::new(SchedulerError::NoReadyWork))),
            Error::Ci(CiError::Scheduler(Box::new(SchedulerError::Output(
                io::Error::new(io::ErrorKind::BrokenPipe, "closed"),
            )))),
        ] {
            assert_eq!(exit_code(&error), EXIT_TOOL, "{error}");
        }
    }

    #[test]
    fn zero_workers_is_a_usage_error() {
        assert_eq!(exit_code(&Error::Ci(CiError::InvalidJobs)), EXIT_USAGE);
    }

    #[test]
    fn parse_jobs_rejects_zero_and_non_numbers() {
        assert_eq!(parse_jobs("1").unwrap(), 1);
        assert_eq!(parse_jobs("8").unwrap(), 8);
        assert!(parse_jobs("0").is_err());
        assert!(parse_jobs("many").is_err());
    }

    #[test]
    fn cache_flags_resolve_to_one_mode() {
        assert!(matches!(cache_mode(false, false), CacheMode::ReadWrite));
        assert!(matches!(cache_mode(true, false), CacheMode::NoCache));
        assert!(matches!(cache_mode(false, true), CacheMode::Force));
    }

    #[test]
    #[should_panic(expected = "mutually exclusive")]
    fn conflicting_cache_flags_are_a_broken_invariant() {
        cache_mode(true, true);
    }

    #[test]
    fn an_absolute_path_is_left_alone_and_a_relative_one_is_rooted() {
        let root = Path::new("/workspace");

        assert_eq!(
            resolve_path(root, PathBuf::from("CHANGELOG.md")),
            PathBuf::from("/workspace/CHANGELOG.md")
        );
        assert_eq!(
            resolve_path(root, PathBuf::from("notes.md")),
            PathBuf::from("/workspace/notes.md")
        );
        assert_eq!(
            resolve_path(root, PathBuf::from("/tmp/notes.md")),
            PathBuf::from("/tmp/notes.md")
        );
    }

    #[test]
    fn success_documents_have_the_documented_shape() {
        let document =
            success_document(OutputMode::Json, "cache_clean", "removed cache".to_owned());
        let value: serde_json::Value = serde_json::from_str(&document).expect("valid JSON");

        assert_eq!(value["schema"], mono::JSON_OUTPUT_SCHEMA);
        assert_eq!(value["kind"], "cache_clean");
        assert_eq!(value["status"], "ok");
        assert_eq!(value["message"], "removed cache");
    }

    #[test]
    fn a_check_document_reports_the_project_root() {
        let document = check_document(OutputMode::Json, Path::new("/workspace"));
        let value: serde_json::Value = serde_json::from_str(&document).expect("valid JSON");

        assert_eq!(value["schema"], mono::JSON_OUTPUT_SCHEMA);
        assert_eq!(value["kind"], "check");
        assert_eq!(value["status"], "ok");
        assert_eq!(value["project"], "/workspace");
    }

    #[test]
    fn error_document_is_json_ready_without_writing_to_a_stream() {
        let error = Error::Ci(CiError::InvalidJobs);
        let value: serde_json::Value =
            serde_json::from_str(&error_document(&error)).expect("valid JSON");

        assert_eq!(value["schema"], mono::JSON_OUTPUT_SCHEMA);
        assert_eq!(value["kind"], "error");
        assert_eq!(value["code"], EXIT_USAGE);
        assert!(value["message"].as_str().unwrap().contains("--jobs"));
    }

    #[test]
    fn error_documents_have_the_documented_shape_on_the_json_transport() {
        let error = Error::Ci(CiError::InvalidJobs);
        let mut sink = Vec::new();

        emit_error(&mut sink, &error, OutputMode::Json);

        let value: serde_json::Value = serde_json::from_slice(&sink).expect("valid JSON");
        assert_eq!(value["schema"], mono::JSON_OUTPUT_SCHEMA);
        assert_eq!(value["kind"], "error");
        assert_eq!(value["code"], EXIT_USAGE);
        assert!(value["message"].as_str().unwrap().contains("--jobs"));
    }

    #[test]
    fn dispatch_rejects_invalid_jobs_before_loading_a_project() {
        let error = dispatch(
            PathBuf::from("does-not-exist"),
            OutputMode::Terminal,
            Some(UiFormat::Stream),
            Some(Commands::Run {
                pipeline: None,
                tasks: Vec::new(),
                options: ExecutionOptions {
                    jobs: 0,
                    ..ExecutionOptions::default()
                },
            }),
            CancellationToken::new(),
        )
        .expect_err("zero workers must fail");

        assert!(matches!(error, Error::Ci(CiError::InvalidJobs)));
    }

    #[test]
    fn a_list_serialization_failure_is_a_tool_failure_like_the_ci_one() {
        let source = serde_json::from_str::<u32>("not json").expect_err("invalid JSON");

        assert_eq!(
            exit_code(&Error::List(ListError::Json { source })),
            EXIT_TOOL,
            "a failed serialization is a mono failure, not a rejected request"
        );
    }
}
