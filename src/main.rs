//! `mono` — language agnostic project and workspace tooling.
//!
//! This is the transport edge: it parses the command line, calls into
//! [`mono`], and maps the single error vocabulary onto stdout, stderr,
//! and an exit code — in exactly one place.
//!
//! Every argument is validated while parsing, so [`run`] only ever sees
//! well-formed commands and its failure space is exactly the library's. The
//! exit code this edge publishes is:
//!
//! | code | meaning                                                      |
//! | ---- | ------------------------------------------------------------ |
//! | `0`  | the command succeeded                                        |
//! | `1`  | the command was understood and failed                        |
//! | `2`  | the command line was wrong; emitted by `clap` while parsing  |
//! | `3`  | `mono` or its environment failed                      |

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::builder::NonEmptyStringValueParser;
use clap::{Args, Parser, Subcommand, ValueEnum};
use mono::{
    CacheMode, ChangelogError, CiError, DEFAULT_CHANGELOG_PATH, DEFAULT_RELEASE_DIRECTORY,
    DEFAULT_RELEASE_NOTES_PATH, Error, InitError, OutputMode, PipelineExecution,
    ReleaseCommandError, ReleaseError, ReleaseIdentity, SchedulerError, changelog_notes,
    changelog_scaffold, changelog_validate, release_manifest, release_source, release_verify,
};

#[derive(Parser)]
#[command(
    name = "mono",
    version = env!("CARGO_PKG_VERSION"),
    about = "Language agnostic project and workspace tooling",
    after_help = "Run `mono help <command>` for command details."
)]
struct Cli {
    /// Project or workspace directory.
    #[arg(long = "dir", global = true, default_value = ".")]
    root: PathBuf,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Clone, Copy, ValueEnum)]
enum OutputFormat {
    #[value(name = "terminal")]
    Terminal,
    #[value(name = "json")]
    Json,
    #[value(name = "github-actions")]
    GithubActions,
}

impl From<OutputFormat> for OutputMode {
    fn from(format: OutputFormat) -> Self {
        match format {
            OutputFormat::Terminal => Self::Terminal,
            OutputFormat::Json => Self::Json,
            OutputFormat::GithubActions => Self::GithubActions,
        }
    }
}

#[derive(Args)]
struct ExecutionOptions {
    /// Package the pipeline run is limited to.
    #[arg(long, value_parser = NonEmptyStringValueParser::new())]
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
    #[arg(long, default_value_t = default_jobs(), value_parser = parse_jobs)]
    jobs: usize,
    /// Output contract to use for task status and grouping.
    #[arg(long, value_enum, default_value_t = OutputFormat::Terminal)]
    output: OutputFormat,
}

impl Default for ExecutionOptions {
    /// The contract used when no subcommand or flag selects one: the default
    /// pipeline, terminal output, a read/write cache, and machine parallelism.
    fn default() -> Self {
        Self {
            package: None,
            dry_run: false,
            no_cache: false,
            force: false,
            jobs: default_jobs(),
            output: OutputFormat::Terminal,
        }
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Write a fresh manifest in the selected directory.
    Init {
        /// Create a standalone project instead of a workspace.
        #[arg(long)]
        standalone: bool,
        /// Command argument used by the generated standalone build task.
        #[arg(
            long = "command",
            num_args = 1,
            requires = "standalone",
            value_parser = NonEmptyStringValueParser::new()
        )]
        command: Vec<String>,
    },
    /// Run a named pipeline, or the default pipeline when omitted.
    #[command(alias = "ci")]
    Run {
        #[arg(value_name = "PIPELINE", value_parser = NonEmptyStringValueParser::new())]
        pipeline: Option<String>,
        /// Compatibility spelling for task selection; prefer `mono task`.
        #[arg(long = "task", hide = true, value_parser = NonEmptyStringValueParser::new())]
        tasks: Vec<String>,
        #[command(flatten)]
        options: ExecutionOptions,
    },
    /// Run one or more tasks and their dependencies.
    Task {
        #[arg(
            required = true,
            value_name = "TASK",
            value_parser = NonEmptyStringValueParser::new()
        )]
        tasks: Vec<String>,
        #[command(flatten)]
        options: ExecutionOptions,
    },
    /// Validate the selected project or workspace.
    #[command(alias = "doctor")]
    Check,
    /// List pipelines, packages, tasks, and common commands.
    List,
    /// Print the resolved plan for the default or named pipeline.
    Plan {
        #[arg(value_name = "PIPELINE", value_parser = NonEmptyStringValueParser::new())]
        pipeline: Option<String>,
        #[arg(long, value_parser = NonEmptyStringValueParser::new())]
        package: Option<String>,
    },
    /// Print dependency edges for the default or named pipeline.
    Graph {
        #[arg(value_name = "PIPELINE", value_parser = NonEmptyStringValueParser::new())]
        pipeline: Option<String>,
        #[arg(long, value_parser = NonEmptyStringValueParser::new())]
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
        #[arg(long, default_value = DEFAULT_CHANGELOG_PATH)]
        file: PathBuf,
    },
    /// Insert or promote a changelog entry.
    Scaffold {
        /// Version to scaffold, or the `VERSION` environment variable.
        #[arg(
            long,
            env = "VERSION",
            required = true,
            value_parser = NonEmptyStringValueParser::new()
        )]
        version: String,
        #[arg(long, default_value = DEFAULT_CHANGELOG_PATH)]
        file: PathBuf,
    },
    /// Extract one changelog entry into release notes.
    Notes {
        /// Version to extract, or the `RELEASE_TAG` environment variable.
        #[arg(
            long,
            env = "RELEASE_TAG",
            required = true,
            value_parser = NonEmptyStringValueParser::new()
        )]
        version: String,
        #[arg(long, default_value = DEFAULT_CHANGELOG_PATH)]
        file: PathBuf,
        #[arg(long, default_value = DEFAULT_RELEASE_NOTES_PATH)]
        output: PathBuf,
    },
}

/// Release metadata that is optional per command and inherited from CI when
/// the matching environment variable is set. `clap` reads the environment, so
/// [`ReleaseIdentityOptions::resolve`] is a pure transformation.
#[derive(Args, Default)]
struct ReleaseIdentityOptions {
    #[arg(long, env = "RELEASE_TAG")]
    tag: Option<String>,
    #[arg(long, env = "GITHUB_SHA")]
    commit: Option<String>,
    #[arg(long, env = "GITHUB_REPOSITORY")]
    repository: Option<String>,
    /// Annotated tag object, absent for a lightweight tag. CI exports the empty
    /// string for a lightweight tag, so [`resolve`](Self::resolve) drops it.
    #[arg(long = "tag-object", env = "RELEASE_TAG_OBJECT")]
    tag_object: Option<String>,
    #[arg(long = "workflow-run", env = "GITHUB_RUN_URL")]
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
struct SourceIdentityOptions {
    /// Git tag to verify, or the `RELEASE_TAG` environment variable.
    #[arg(
        long,
        env = "RELEASE_TAG",
        required = true,
        value_parser = NonEmptyStringValueParser::new()
    )]
    tag: String,
    /// Commit the tag must point at, or the `GITHUB_SHA` environment variable.
    #[arg(
        long,
        env = "GITHUB_SHA",
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
    /// Generate BUILD-METADATA.json and SHA256SUMS.
    Manifest {
        /// Directory the release metadata is written to, below the root.
        #[arg(long, default_value = DEFAULT_RELEASE_DIRECTORY)]
        directory: PathBuf,
        /// File containing one expected artifact path per line.
        #[arg(long)]
        expected: Option<PathBuf>,
        #[command(flatten)]
        identity: ReleaseIdentityOptions,
    },
    /// Verify release metadata, checksums, and exact artifact inventory.
    Verify {
        /// Directory the release metadata is read from, below the root.
        #[arg(long, default_value = DEFAULT_RELEASE_DIRECTORY)]
        directory: PathBuf,
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
    let Cli { root, command } = Cli::parse();

    let code = match run(root, command) {
        Ok(summary) => emit_summary(&mut io::stdout().lock(), &summary),
        Err(error) => emit_error(&mut io::stderr().lock(), &error),
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
fn emit_error(sink: &mut impl Write, error: &Error) -> u8 {
    let _ = writeln!(sink, "mono: {error}");
    exit_code(error)
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
        Error::Changelog(error) => changelog_exit_code(error),
        Error::Release(ReleaseCommandError::Release(error)) => release_exit_code(error),
        // A workspace failure is a rejected request, not a failed tool.
        //
        // `WorkspaceError::Io` is overloaded: it covers failing to read a
        // manifest *and* failing to resolve a directory the manifest declares,
        // such as a task `cwd`. The second is exactly the defect `mono
        // check` exists to report, so the variant cannot be split by exit code
        // here. A genuine environment failure surfaces as a write error, which
        // is classified in `init_exit_code`, `changelog_exit_code`, and
        // `release_exit_code`.
        Error::Doctor(_) | Error::List(_) | Error::Ci(CiError::Workspace(_)) => EXIT_FAILED,
    }
}

fn init_exit_code(error: &InitError) -> u8 {
    match error {
        // The requested directory or file could not be written.
        InitError::CreateDir { .. } | InitError::WriteConfig { .. } => EXIT_TOOL,
        InitError::StandaloneCommandRequired | InitError::AlreadyInitialized(_) => EXIT_FAILED,
    }
}

fn scheduler_exit_code(error: &SchedulerError) -> u8 {
    match error {
        // A task ran and reported failure. The pipeline being red is a result,
        // not a malfunction, so it is the caller's failure and not the tool's.
        SchedulerError::Task(_) => EXIT_FAILED,
        // The scheduler never got far enough to run the pipeline.
        SchedulerError::Cache(_)
        | SchedulerError::Output(_)
        | SchedulerError::UnresolvedDependency { .. }
        | SchedulerError::NoReadyWork => EXIT_TOOL,
    }
}

fn changelog_exit_code(error: &ChangelogError) -> u8 {
    match error {
        ChangelogError::Read { .. } | ChangelogError::Write { .. } => EXIT_TOOL,
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
fn run(root: PathBuf, command: Option<Commands>) -> Result<String, Error> {
    match command {
        None => run_default_pipeline(&root),
        Some(Commands::Init {
            standalone,
            command,
        }) => run_init(&root, standalone, command),
        Some(Commands::Run {
            pipeline,
            tasks,
            options,
        }) => execute_pipeline(&root, pipeline.as_deref(), &tasks, options),
        Some(Commands::Task { tasks, options }) => execute_pipeline(&root, None, &tasks, options),
        Some(Commands::Check) => run_check(&root),
        Some(Commands::List) => run_list(&root),
        Some(Commands::Plan { pipeline, package }) => {
            run_plan(&root, pipeline.as_deref(), package.as_deref())
        }
        Some(Commands::Graph { pipeline, package }) => {
            run_graph(&root, pipeline.as_deref(), package.as_deref())
        }
        Some(Commands::Cache { command }) => run_cache(&root, command),
        Some(Commands::Changelog { command }) => run_changelog(&root, command),
        Some(Commands::Release { command }) => run_release(&root, command),
    }
}

/// Run the default pipeline when no subcommand is given.
fn run_default_pipeline(root: &Path) -> Result<String, Error> {
    execute_pipeline(root, None, NO_TASK_FILTER, ExecutionOptions::default())
}

fn run_init(root: &Path, standalone: bool, command: Vec<String>) -> Result<String, Error> {
    let written = if standalone {
        mono::init_standalone(root, command)?
    } else {
        mono::init(root)?
    };
    Ok(format!("initialized {}", written.display()))
}

/// Run one pipeline or task selection with the parsed execution options.
fn execute_pipeline(
    root: &Path,
    pipeline: Option<&str>,
    tasks: &[String],
    options: ExecutionOptions,
) -> Result<String, Error> {
    Ok(mono::run_pipeline_with_mode(
        root,
        pipeline,
        options.package.as_deref(),
        tasks,
        options.dry_run,
        options.jobs,
        PipelineExecution {
            cache: cache_mode(options.no_cache, options.force),
            output: options.output.into(),
        },
    )?)
}

fn run_check(root: &Path) -> Result<String, Error> {
    mono::doctor(root)?;
    Ok(format!("checked {}", root.display()))
}

fn run_list(root: &Path) -> Result<String, Error> {
    Ok(mono::list(root)?)
}

fn run_plan(root: &Path, pipeline: Option<&str>, package: Option<&str>) -> Result<String, Error> {
    Ok(mono::plan(root, pipeline, package, NO_TASK_FILTER)?)
}

fn run_graph(root: &Path, pipeline: Option<&str>, package: Option<&str>) -> Result<String, Error> {
    Ok(mono::graph(root, pipeline, package, NO_TASK_FILTER)?)
}

fn run_cache(root: &Path, command: CacheCommands) -> Result<String, Error> {
    match command {
        CacheCommands::Clean => Ok(mono::clean_cache(root)?),
    }
}

fn run_changelog(root: &Path, command: ChangelogCommands) -> Result<String, Error> {
    match command {
        ChangelogCommands::Validate { file } => Ok(changelog_validate(&resolve_path(root, file))?),
        ChangelogCommands::Scaffold { version, file } => {
            Ok(changelog_scaffold(&resolve_path(root, file), &version)?)
        }
        ChangelogCommands::Notes {
            version,
            file,
            output,
        } => Ok(changelog_notes(
            &resolve_path(root, file),
            &version,
            &resolve_path(root, output),
        )?),
    }
}

fn run_release(root: &Path, command: ReleaseCommands) -> Result<String, Error> {
    match command {
        ReleaseCommands::Source { identity } => {
            Ok(release_source(root, &identity.tag, &identity.commit)?)
        }
        ReleaseCommands::Manifest {
            directory,
            expected,
            identity,
        } => {
            let directory = resolve_path(root, directory);
            let expected = expected.map(|path| resolve_path(root, path));
            Ok(release_manifest(
                &directory,
                identity.resolve(),
                expected.as_deref(),
            )?)
        }
        ReleaseCommands::Verify {
            directory,
            expected,
            identity,
        } => {
            let directory = resolve_path(root, directory);
            let expected = expected.map(|path| resolve_path(root, path));
            Ok(release_verify(
                &directory,
                identity.resolve(),
                expected.as_deref(),
            )?)
        }
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
    use mono::{DoctorError, ListError, WorkspaceError};

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

    fn missing_root() -> WorkspaceError {
        WorkspaceError::MissingRoot {
            start: PathBuf::from("."),
        }
    }

    fn missing_declared_directory() -> WorkspaceError {
        // A task `cwd` that the manifest declares but the worktree lacks.
        WorkspaceError::Io {
            path: PathBuf::from("packages/api/missing"),
            source: io::Error::new(io::ErrorKind::NotFound, "missing"),
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

        assert_eq!(emit_error(&mut sink, &error), EXIT_USAGE);
        assert_eq!(
            String::from_utf8(sink).unwrap(),
            "mono: --jobs must be greater than zero\n"
        );
    }

    #[test]
    fn a_broken_stderr_does_not_mask_the_failure() {
        let error = Error::Doctor(DoctorError::Workspace(missing_root()));
        let mut sink = FailingWriter(io::ErrorKind::BrokenPipe);

        assert_eq!(emit_error(&mut sink, &error), EXIT_FAILED);
    }

    #[test]
    fn requests_that_were_understood_fail_with_one() {
        for error in [
            Error::Init(InitError::StandaloneCommandRequired),
            Error::Init(InitError::AlreadyInitialized(PathBuf::from("mono.toml"))),
            Error::Doctor(DoctorError::Workspace(missing_root())),
            Error::Doctor(DoctorError::Workspace(missing_declared_directory())),
            Error::List(ListError::Workspace(WorkspaceError::UnknownPackage {
                name: "api".to_owned(),
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
}
