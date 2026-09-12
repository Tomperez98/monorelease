//! `mono` — language-agnostic root-project tooling.
//!
//! This is the transport edge: it parses the command line, calls into
//! [`mono`], and maps the single error vocabulary onto stdout, stderr,
//! and an exit code — in exactly one place.
//!
//! Every argument whose value has a shape mono can judge on its own is
//! validated while parsing, so [`app::dispatch`] sees well-formed commands and its
//! failure space is exactly the library's. The split is by authority: a value
//! the parser rejects exits `2`, and a value only the project can judge exits
//! `1` once the library has read it. The exit code this edge publishes is:
//!
//! | code | meaning                                                             |
//! | ---- | ------------------------------------------------------------------- |
//! | `0`  | the command succeeded                                               |
//! | `1`  | the command was understood and refused on its merits                |
//! | `2`  | the command line was malformed; emitted by `clap` while parsing     |
//! | `3`  | `mono` or its environment failed                                    |

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

mod app;
mod render;

use clap::builder::NonEmptyStringValueParser;
use clap::{Args, Parser, Subcommand, ValueEnum};
use mono::{
    CancellationToken, ChangelogError, CiError, DEFAULT_CHANGELOG_PATH, DEFAULT_RELEASE_DIRECTORY,
    DEFAULT_RELEASE_NOTES_PATH, DoctorError, Error, InitError, ListError, OutputMode, ProjectError,
    PullRequestUrl, ReleaseCommandError, ReleaseDate, ReleaseError, ReleaseIdentity, Request,
    SchedulerError,
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
  1  the request was refused (red pipeline, invalid manifest, rejected version)
  2  the command line was malformed (unknown flag, `--jobs 0`, bad `--date`)
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
    ///
    /// `changelog` and `release` never load a manifest, so for them this is the
    /// directory their relative paths are resolved against.
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
    /// The contract bare `mono` uses, by way of the application default command:
    /// default pipeline, terminal output, a read/write cache, and machine
    /// parallelism.
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
        #[arg(value_name = "VERSION", value_parser = parse_version)]
        version: Option<Request>,
        /// Compatibility spelling for the pre-0.2 `--version` flag.
        #[arg(long = "version", hide = true, conflicts_with = "version", value_parser = parse_version)]
        version_flag: Option<Request>,
        /// Date to put in a new entry; defaults to today's UTC date.
        #[arg(long, value_name = "YYYY-MM-DD", value_parser = parse_date)]
        date: Option<ReleaseDate>,
        /// First Git ref to include in editable merge bullets.
        #[arg(long, requires = "to", value_parser = NonEmptyStringValueParser::new())]
        from: Option<String>,
        /// Last Git ref to include in editable merge bullets.
        #[arg(long, requires = "from", value_parser = NonEmptyStringValueParser::new())]
        to: Option<String>,
        /// URL template for PR bullets, containing `{number}`.
        #[arg(long, value_parser = parse_pull_request_url)]
        pull_request_url: Option<PullRequestUrl>,
        /// Changelog file to edit
        #[arg(long, default_value = DEFAULT_CHANGELOG_PATH)]
        file: PathBuf,
    },
    /// Write release notes from the newest changelog entry.
    #[command(name = "release-notes", alias = "notes")]
    ReleaseNotes {
        /// Release version to require; defaults to the newest changelog entry.
        #[arg(value_name = "VERSION", value_parser = parse_version)]
        version: Option<Request>,
        /// Compatibility spelling for the pre-0.2 `--version` flag.
        #[arg(long = "version", hide = true, conflicts_with = "version", value_parser = parse_version)]
        version_flag: Option<Request>,
        /// Release tag to require.
        #[arg(long = "release-tag", value_parser = parse_version)]
        release_tag: Option<Request>,
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

/// Parse a version, `--version`, or `--release-tag` value at the CLI boundary
/// so malformed values become usage errors against the flag typed.
fn parse_version(value: &str) -> Result<Request, String> {
    Request::parse(value)
}

/// Parse a `--date` value into the validated domain type.
fn parse_date(value: &str) -> Result<ReleaseDate, String> {
    ReleaseDate::parse(value)
}

/// Parse a `--pull-request-url` template.
fn parse_pull_request_url(value: &str) -> Result<PullRequestUrl, String> {
    PullRequestUrl::parse(value)
}

fn main() -> ExitCode {
    let Cli {
        root,
        output,
        ui,
        command,
    } = Cli::parse();
    let output = output.into();

    // The locks are taken for the single write that needs them, never across
    // `dispatch`: the scheduler renders task output to these same process-global
    // handles from worker threads, and a lock held here would wait for a thread
    // waiting for this one.
    let cancellation = match install_cancellation(command.as_ref()) {
        Ok(cancellation) => cancellation,
        Err(message) => {
            let mut stdout = io::stdout().lock();
            let mut stderr = io::stderr().lock();
            let code = emit_error(
                error_sink(&mut stdout, &mut stderr, output),
                EXIT_TOOL,
                &message,
                output,
            );
            return ExitCode::from(code);
        }
    };

    let terminal = interactive_terminal();
    let code = match app::dispatch(root, output, ui, command, cancellation, terminal) {
        Ok(result) => emit_summary(
            &mut io::stdout().lock(),
            &mut io::stderr().lock(),
            render::render(result, output),
        ),
        Err(error) => {
            let code = exit_code(&error);
            let mut stdout = io::stdout().lock();
            let mut stderr = io::stderr().lock();
            emit_error(
                error_sink(&mut stdout, &mut stderr, output),
                code,
                &error.to_string(),
                output,
            )
        }
    };
    ExitCode::from(code)
}

/// Install the Ctrl-C handler that cancels running tasks.
///
/// Only a command whose scheduler can cooperatively cancel tasks needs one, so
/// a host that refuses a signal handler cannot fail `mono list`. The returned
/// token is inert for every other command.
fn install_cancellation(command: Option<&Commands>) -> Result<CancellationToken, String> {
    let cancellation = CancellationToken::new();
    if !runs_tasks(command) {
        return Ok(cancellation);
    }
    ctrlc::set_handler({
        let cancellation = cancellation.clone();
        move || cancellation.cancel()
    })
    .map_err(|error| format!("could not install Ctrl-C handler: {error}"))?;
    Ok(cancellation)
}

/// Whether this command uses the task scheduler and therefore needs
/// cooperative cancellation. Dry runs only resolve a plan and need no handler.
fn runs_tasks(command: Option<&Commands>) -> bool {
    match command {
        None => true,
        Some(Commands::Run { options, .. }) | Some(Commands::Task { options, .. }) => {
            !options.dry_run
        }
        Some(_) => false,
    }
}

/// The stream a failure belongs on: the JSON stream a machine already reads, or
/// stderr for a person, because every line on stdout belongs to the command.
fn error_sink<'a>(
    stdout: &'a mut dyn Write,
    stderr: &'a mut dyn Write,
    output: OutputMode,
) -> &'a mut dyn Write {
    if output == OutputMode::Json {
        stdout
    } else {
        stderr
    }
}

/// Write a command's summary to `sink`, reporting a failed write on `diagnostics`.
///
/// A command that reported itself through the JSON event stream returns the
/// `EventsAlreadyEmitted` outcome and writes nothing here; a tagged outcome
/// keeps that distinct from an empty summary.
///
/// A consumer that hangs up early — `mono plan | head` — closes the pipe before
/// this write lands. That is an expected outcome, not a broken invariant, so it
/// is reported as success instead of panicking the way `println!` would over
/// the process-global handle. This write happens after the work is finished, so
/// there is nothing left to lose; a hangup *during* a run cannot deliver the
/// requested output contract, and the scheduler exits `3` for that case.
///
/// Taking the sinks as arguments keeps both mappings testable without spawning
/// a process.
fn emit_summary(
    sink: &mut dyn Write,
    diagnostics: &mut dyn Write,
    rendered: render::RenderedCommand,
) -> u8 {
    let render::RenderedCommand::Summary(summary) = rendered else {
        return 0;
    };
    match writeln!(sink, "{summary}") {
        Ok(()) => 0,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => 0,
        Err(error) => {
            let _ = writeln!(diagnostics, "mono: {error}");
            EXIT_TOOL
        }
    }
}

/// The failure document for JSON mode.
#[derive(serde::Serialize)]
struct ErrorDocument<'a> {
    schema: u32,
    kind: &'static str,
    code: u8,
    message: &'a str,
}

fn serialize(document: &impl serde::Serialize) -> String {
    serde_json::to_string(document).expect("output documents contain only serializable fields")
}

fn error_document(code: u8, message: &str) -> String {
    serialize(&ErrorDocument {
        schema: mono::JSON_OUTPUT_SCHEMA,
        kind: "error",
        code,
        message,
    })
}

/// Report a failed command on `sink` and return `code`.
///
/// A closed sink must never mask the failure, so the write is best effort and
/// the code is returned exactly as given.
fn emit_error(sink: &mut dyn Write, code: u8, message: &str, output: OutputMode) -> u8 {
    if output == OutputMode::Json {
        let _ = sink.write_all(error_document(code, message).as_bytes());
        let _ = sink.write_all(b"\n");
    } else {
        let _ = writeln!(sink, "mono: {message}");
    }
    code
}

/// Map the single error vocabulary onto this CLI's exit code, once.
///
/// The vocabulary splits into two kinds of failure: the command was understood
/// and the request failed (`1`), or `mono` and its environment failed to
/// carry it out (`3`). `clap` rejects a malformed command line with `2` while
/// parsing, before this runs; the one usage failure the library owns — a worker
/// count the scheduler cannot honor — is classified here too, because it is a
/// malformed command line whatever layer noticed it.
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
        // The scheduler never got far enough to run the pipeline. This includes
        // a hung-up consumer while the run is still in flight: the output
        // contract mono was asked for can no longer be delivered, and the
        // scheduler stops dispatching. A hangup after the run finished is
        // handled by `emit_summary`, where there is nothing left to lose.
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

/// Resolve whether the selected command needs the interactive task transport.
fn interactive_terminal() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal() && std::env::var_os("CI").is_none()
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
        let mut diagnostics = Vec::new();

        assert_eq!(
            emit_summary(
                &mut sink,
                &mut diagnostics,
                render::RenderedCommand::Summary("summary: 1 completed".to_owned()),
            ),
            0
        );
        assert_eq!(String::from_utf8(sink).unwrap(), "summary: 1 completed\n");
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn a_command_with_no_summary_writes_nothing_and_returns_zero() {
        let mut sink = Vec::new();
        let mut diagnostics = Vec::new();

        assert_eq!(
            emit_summary(
                &mut sink,
                &mut diagnostics,
                render::RenderedCommand::EventsAlreadyEmitted,
            ),
            0
        );
        assert!(sink.is_empty(), "no summary must write nothing");
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn a_consumer_that_hangs_up_is_not_a_failure() {
        let mut sink = FailingWriter(io::ErrorKind::BrokenPipe);
        let mut diagnostics = Vec::new();

        assert_eq!(
            emit_summary(
                &mut sink,
                &mut diagnostics,
                render::RenderedCommand::Summary("summary: 1 completed".to_owned()),
            ),
            0
        );
        assert!(diagnostics.is_empty(), "a hangup is not worth a diagnostic");
    }

    #[test]
    fn a_write_failure_that_is_not_a_hangup_is_a_tool_failure() {
        let mut sink = FailingWriter(io::ErrorKind::StorageFull);
        let mut diagnostics = Vec::new();

        assert_eq!(
            emit_summary(
                &mut sink,
                &mut diagnostics,
                render::RenderedCommand::Summary("summary: 1 completed".to_owned()),
            ),
            EXIT_TOOL
        );
        assert_eq!(
            String::from_utf8(diagnostics).unwrap(),
            "mono: writer is unavailable\n"
        );
    }

    #[test]
    fn failures_go_to_the_json_stream_for_machines_and_to_stderr_for_people() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let sink = error_sink(&mut stdout, &mut stderr, OutputMode::Json);
        write!(sink, "machine").unwrap();
        assert_eq!(stdout, b"machine");
        assert!(stderr.is_empty(), "the JSON stream is the only output");

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let sink = error_sink(&mut stdout, &mut stderr, OutputMode::Terminal);
        write!(sink, "person").unwrap();
        assert!(stdout.is_empty(), "stdout belongs to the command");
        assert_eq!(stderr, b"person");
    }

    #[test]
    fn a_failure_is_reported_on_the_sink_with_its_exit_code() {
        let error = Error::Ci(CiError::InvalidJobs);
        let mut sink = Vec::new();

        assert_eq!(
            emit_error(
                &mut sink,
                exit_code(&error),
                &error.to_string(),
                OutputMode::Terminal
            ),
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
            emit_error(
                &mut sink,
                exit_code(&error),
                &error.to_string(),
                OutputMode::Terminal
            ),
            EXIT_FAILED
        );
    }

    #[test]
    fn requests_that_were_understood_fail_with_one() {
        for error in [
            Error::Init(InitError::AlreadyInitialized(PathBuf::from("mono.toml"))),
            Error::Ci(CiError::Scheduler(Box::new(SchedulerError::Cancelled))),
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

    /// Every value parser assigned to a flag: a value whose shape mono can judge
    /// on its own is refused while parsing, so it exits `2` and the message
    /// names the flag the user typed.
    #[test]
    fn value_parsers_reject_shapes_they_can_judge() {
        assert_eq!(
            parse_version("1.2.3").unwrap(),
            Request::parse("1.2.3").unwrap()
        );
        assert_eq!(parse_version("unreleased").unwrap(), Request::Unreleased);
        assert!(parse_version("banana").is_err());
        assert!(parse_version("").is_err());

        assert_eq!(parse_date("2001-02-03").unwrap().as_str(), "2001-02-03");
        assert!(parse_date("2001-02-30").is_err());
        assert!(parse_date("03/02/2001").is_err());

        assert_eq!(
            parse_pull_request_url("https://example.test/pull/{number}")
                .unwrap()
                .as_str(),
            "https://example.test/pull/{number}"
        );
        assert!(parse_pull_request_url("https://example.test/pull/").is_err());
    }

    #[test]
    fn a_set_but_empty_identity_value_is_absent() {
        let identity = ReleaseIdentityOptions {
            tag: Some("v1.2.3".to_owned()),
            commit: Some("abc123".to_owned()),
            repository: Some("acme/mono".to_owned()),
            // CI forwards a variable it never set as an empty argument.
            tag_object: Some(String::new()),
            workflow_run: None,
        }
        .resolve();

        assert_eq!(identity.release_tag.as_deref(), Some("v1.2.3"));
        assert_eq!(identity.source_commit.as_deref(), Some("abc123"));
        assert_eq!(identity.repository.as_deref(), Some("acme/mono"));
        assert_eq!(identity.tag_object, None, "a lightweight tag has no object");
        assert_eq!(identity.workflow_run, None);
    }

    #[test]
    fn only_the_commands_that_spawn_tasks_need_a_signal_handler() {
        let run = Some(Commands::Run {
            pipeline: None,
            tasks: Vec::new(),
            options: ExecutionOptions::default(),
        });
        let task = Some(Commands::Task {
            tasks: vec!["test".to_owned()],
            options: ExecutionOptions::default(),
        });

        assert!(runs_tasks(None), "bare mono runs the default pipeline");
        assert!(runs_tasks(run.as_ref()), "mono run executes tasks");
        assert!(runs_tasks(task.as_ref()), "mono task executes tasks");

        let dry_run = Some(Commands::Run {
            pipeline: None,
            tasks: Vec::new(),
            options: ExecutionOptions {
                dry_run: true,
                ..ExecutionOptions::default()
            },
        });
        assert!(
            !runs_tasks(dry_run.as_ref()),
            "dry-run spawns no child process"
        );

        let dry_task = Some(Commands::Task {
            tasks: vec!["test".to_owned()],
            options: ExecutionOptions {
                dry_run: true,
                ..ExecutionOptions::default()
            },
        });
        assert!(
            !runs_tasks(dry_task.as_ref()),
            "dry-run task spawns no child process"
        );

        for (label, command) in [
            ("init", Some(Commands::Init)),
            ("check", Some(Commands::Check)),
            ("list", Some(Commands::List)),
            ("plan", Some(Commands::Plan { pipeline: None })),
            ("graph", Some(Commands::Graph { pipeline: None })),
            (
                "cache clean",
                Some(Commands::Cache {
                    command: CacheCommands::Clean,
                }),
            ),
        ] {
            assert!(
                !runs_tasks(command.as_ref()),
                "mono {label} spawns no child process"
            );
        }
    }

    #[test]
    fn error_document_is_json_ready_without_writing_to_a_stream() {
        let error = Error::Ci(CiError::InvalidJobs);
        let value: serde_json::Value =
            serde_json::from_str(&error_document(exit_code(&error), &error.to_string()))
                .expect("valid JSON");

        assert_eq!(value["schema"], mono::JSON_OUTPUT_SCHEMA);
        assert_eq!(value["kind"], "error");
        assert_eq!(value["code"], EXIT_USAGE);
        assert!(value["message"].as_str().unwrap().contains("--jobs"));
    }

    #[test]
    fn error_documents_have_the_documented_shape_on_the_json_transport() {
        let error = Error::Ci(CiError::InvalidJobs);
        let mut sink = Vec::new();

        emit_error(
            &mut sink,
            exit_code(&error),
            &error.to_string(),
            OutputMode::Json,
        );

        let value: serde_json::Value = serde_json::from_slice(&sink).expect("valid JSON");
        assert_eq!(value["schema"], mono::JSON_OUTPUT_SCHEMA);
        assert_eq!(value["kind"], "error");
        assert_eq!(value["code"], EXIT_USAGE);
        assert!(value["message"].as_str().unwrap().contains("--jobs"));
    }

    /// A tool failure `mono` owns before any command runs is reported through
    /// the same transport as every other failure, JSON included.
    #[test]
    fn a_tool_failure_before_dispatch_is_a_document_in_json_mode() {
        let mut sink = Vec::new();
        let code = emit_error(
            &mut sink,
            EXIT_TOOL,
            "could not install Ctrl-C handler: denied",
            OutputMode::Json,
        );

        assert_eq!(code, EXIT_TOOL);
        let value: serde_json::Value = serde_json::from_slice(&sink).expect("valid JSON");
        assert_eq!(value["kind"], "error");
        assert_eq!(value["code"], EXIT_TOOL);
    }

    #[test]
    fn dispatch_rejects_invalid_jobs_before_loading_a_project() {
        let error = app::dispatch(
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
            false,
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
