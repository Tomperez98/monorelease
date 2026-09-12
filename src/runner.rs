//! Structured subprocess execution for root-project tasks.

use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::fmt;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::StdinMode;
use crate::process::ManagedChild;
use crate::project::PlannedTask;

// ---------------------------------------------------------------------------
// Runner process vocabulary
// ---------------------------------------------------------------------------

/// The observable result of a completed subprocess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcessExit {
    pub(crate) code: Option<i32>,
    pub(crate) success: bool,
}

/// The semantic result of asking a child process tree to terminate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminationOutcome {
    Terminated,
    AlreadyExited,
}

/// A live subprocess whose output and lifecycle the runner can observe.
pub(crate) trait ChildProcess: Send {
    fn stdout(&mut self) -> Option<Box<dyn Read + Send>>;
    fn stderr(&mut self) -> Option<Box<dyn Read + Send>>;
    fn try_wait(&mut self) -> io::Result<Option<ProcessExit>>;
    fn wait(&mut self) -> io::Result<ProcessExit>;
    fn terminate_tree(&mut self) -> io::Result<TerminationOutcome>;
}

/// A factory for spawning child processes through the runner.
pub(crate) trait ProcessLauncher: Send + Sync {
    fn spawn(&self, spec: &ProcessSpec<'_>) -> io::Result<Box<dyn ChildProcess>>;
}

/// The validated process specification the runner passes to the launcher.
///
/// Carries the already-validated program, arguments, working directory,
/// environment variables, and stdin mode.  The launcher is responsible for
/// requesting piped stdout/stderr.
pub(crate) struct ProcessSpec<'a> {
    pub(crate) program: &'a str,
    pub(crate) args: &'a [String],
    pub(crate) cwd: &'a Path,
    pub(crate) env: &'a BTreeMap<String, String>,
    pub(crate) stdin: StdinMode,
}

/// A clock abstraction so the runner can be driven deterministically in tests.
pub(crate) trait RunnerClock: Send + Sync {
    fn now(&self) -> Instant;
    fn sleep(&self, duration: Duration);
}

// ---------------------------------------------------------------------------

/// First wait between child-status polls. Short tasks finish after one or two
/// of these instead of stalling for a fixed interval.
const POLL_INTERVAL_START: Duration = Duration::from_millis(1);
/// Longest wait between polls, so a long task stops waking the process
/// hundreds of times per second.
const POLL_INTERVAL_MAX: Duration = Duration::from_millis(50);

/// The observable result of one non-blocking child wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitOutcome {
    Exited,
    Running,
    Failed,
}

/// What the poll loop must do next, given the state it can observe.
///
/// Extracted from the loop so the precedence between cancellation, an exceeded
/// output limit, child exit, and timeout is a table a test can assert instead
/// of five interleaved branches over a live child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollDecision {
    /// Terminate the tree and report cancellation.
    Cancel,
    /// Terminate the tree and report that `stream` exceeded its limit.
    OutputLimit(&'static str),
    /// The direct child exited; descendants may still hold the pipes open.
    Exited { descendants_may_hold_pipes: bool },
    /// Terminate the tree and report the timeout.
    Timeout,
    /// `try_wait` failed; report a wait error.
    WaitFailed,
    /// Still running; sleep before polling again.
    Sleep(Duration),
}

/// Why the runner needs to terminate a process tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminationReason {
    Cancellation,
    OutputLimit,
    DescendantCleanup,
    Timeout,
    WaitFailure,
}

impl fmt::Display for TerminationReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Cancellation => "cancellation",
            Self::OutputLimit => "output limit",
            Self::DescendantCleanup => "descendant cleanup",
            Self::Timeout => "timeout",
            Self::WaitFailure => "wait error",
        })
    }
}

fn poll_decision(
    cancelled: bool,
    exceeded_stream: Option<&'static str>,
    wait: WaitOutcome,
    readers_done: u8,
    elapsed: Duration,
    timeout: Duration,
    poll_interval: Duration,
) -> PollDecision {
    if cancelled {
        return PollDecision::Cancel;
    }
    if let Some(stream) = exceeded_stream {
        return PollDecision::OutputLimit(stream);
    }
    match wait {
        WaitOutcome::Exited => PollDecision::Exited {
            descendants_may_hold_pipes: readers_done < 2,
        },
        WaitOutcome::Running if elapsed >= timeout => PollDecision::Timeout,
        WaitOutcome::Running => {
            let remaining = timeout.saturating_sub(elapsed);
            PollDecision::Sleep(poll_interval.min(remaining))
        }
        WaitOutcome::Failed => PollDecision::WaitFailed,
    }
}

fn next_poll_interval(current: Duration) -> Duration {
    (current * 2).min(POLL_INTERVAL_MAX)
}

/// Captured process output, presented by the scheduler after a task completes.
#[derive(Debug, Clone, Default)]
pub struct CapturedOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// The result of a completed task process.
#[derive(Debug, Clone)]
pub struct TaskResult {
    pub output: CapturedOutput,
    pub elapsed: Duration,
    pub cached: bool,
}

/// A cancellation signal shared by the scheduler and running task processes.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

pub(crate) type OutputCallback = Arc<dyn Fn(&'static str, &[u8]) -> io::Result<()> + Send + Sync>;

/// The seam the scheduler executes tasks through.
///
/// [`Runner`] is the production implementation; a test supplies a scripted one
/// to drive the scheduling loop without spawning processes.
pub(crate) trait TaskExecutor: Send + Sync {
    fn execute(
        &self,
        project_root: &Path,
        planned: &PlannedTask,
        cancellation: Option<&CancellationToken>,
        output_callback: Option<OutputCallback>,
    ) -> Result<TaskResult, RunnerError>;
}

/// Production subprocess launcher using [`ManagedChild`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProductionLauncher;

impl ProcessLauncher for ProductionLauncher {
    fn spawn(&self, spec: &ProcessSpec<'_>) -> io::Result<Box<dyn ChildProcess>> {
        let mut command = Command::new(spec.program);
        command
            .args(spec.args)
            .current_dir(spec.cwd)
            .envs(spec.env)
            .stdin(match spec.stdin {
                StdinMode::Null => Stdio::null(),
                StdinMode::Inherit => Stdio::inherit(),
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = ManagedChild::spawn(command)?;
        Ok(Box::new(child))
    }
}

/// Production wall-clock implementation.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WallClock;

impl RunnerClock for WallClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) {
        thread::sleep(duration);
    }
}

/// Executes root-project tasks without changing the process-global working directory.
pub struct Runner {
    launcher: Arc<dyn ProcessLauncher>,
    clock: Arc<dyn RunnerClock>,
}

impl std::fmt::Debug for Runner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runner").finish_non_exhaustive()
    }
}

impl Runner {
    pub fn new() -> Self {
        Self {
            launcher: Arc::new(ProductionLauncher),
            clock: Arc::new(WallClock),
        }
    }

    /// Private constructor for unit tests that inject fake services.
    #[cfg(test)]
    pub(crate) fn with_services(
        launcher: Arc<dyn ProcessLauncher>,
        clock: Arc<dyn RunnerClock>,
    ) -> Self {
        Self { launcher, clock }
    }

    /// Execute one planned task and return its output and timing.
    ///
    /// A convenience for tests and for callers that want one task without a
    /// plan; the scheduler always uses [`run_with_options`](Self::run_with_options).
    #[cfg(test)]
    pub fn run(
        &self,
        project_root: &Path,
        planned: &PlannedTask,
    ) -> Result<TaskResult, RunnerError> {
        self.run_with_options(project_root, planned, None, None)
    }

    pub(crate) fn run_with_options(
        &self,
        project_root: &Path,
        planned: &PlannedTask,
        cancellation: Option<&CancellationToken>,
        output_callback: Option<OutputCallback>,
    ) -> Result<TaskResult, RunnerError> {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(RunnerError::Cancelled(Box::new(CancelledTask {
                project: planned.project().to_owned(),
                task: planned.id().to_owned(),
                output: CapturedOutput::default(),
                elapsed: Duration::ZERO,
            })));
        }

        let (program, args) =
            planned
                .command()
                .split_first()
                .ok_or_else(|| RunnerError::EmptyCommand {
                    project: planned.project().to_owned(),
                    task: planned.id().to_owned(),
                })?;
        assert!(planned.timeout() > Duration::ZERO);
        assert!(planned.max_output_bytes() > 0);
        assert!(planned.root().starts_with(project_root));
        assert!(planned.cwd().is_absolute());
        assert!(planned.cwd().starts_with(planned.root()));

        let spec = ProcessSpec {
            program,
            args,
            cwd: planned.cwd(),
            env: planned.env(),
            stdin: planned.stdin(),
        };
        let started = self.clock.now();
        let mut child = self
            .launcher
            .spawn(&spec)
            .map_err(|source| RunnerError::Spawn {
                project: planned.project().to_owned(),
                task: planned.id().to_owned(),
                command: planned.command().to_vec(),
                cwd: planned.cwd().to_path_buf(),
                source,
            })?;

        let stdout = child
            .stdout()
            .expect("launcher must configure stdout as piped");
        let stderr = child
            .stderr()
            .expect("launcher must configure stderr as piped");
        let (limit_sender, limit_receiver) = mpsc::channel();
        let output_limit = planned.max_output_bytes();
        let reader_done = Arc::new(AtomicU8::new(0));
        let stdout_reader = {
            let limit_sender = limit_sender.clone();
            let callback = output_callback.clone();
            let done = Arc::clone(&reader_done);
            thread::spawn(move || {
                let result = read_stream(stdout, output_limit, "stdout", limit_sender, callback);
                done.fetch_add(1, Ordering::Release);
                result
            })
        };
        let stderr_reader = {
            let limit_sender_clone = limit_sender.clone();
            let callback = output_callback;
            let done = Arc::clone(&reader_done);
            thread::spawn(move || {
                let result =
                    read_stream(stderr, output_limit, "stderr", limit_sender_clone, callback);
                done.fetch_add(1, Ordering::Release);
                result
            })
        };

        let mut poll_interval = POLL_INTERVAL_START;
        let mut exceeded_stream: Option<&'static str> = None;
        // `None` means the run is already cancelled, so no wait is attempted:
        // cancellation is decided before the child is touched, exactly as
        // before this refactor.
        let exit = loop {
            if exceeded_stream.is_none()
                && let Ok(stream) = limit_receiver.try_recv()
            {
                exceeded_stream = Some(stream);
            }
            let cancelled = cancellation.is_some_and(CancellationToken::is_cancelled);
            let wait = (!cancelled).then(|| child.try_wait());
            let outcome = match &wait {
                Some(Ok(Some(_))) => WaitOutcome::Exited,
                Some(Err(_)) => WaitOutcome::Failed,
                _ => WaitOutcome::Running,
            };
            let elapsed = self.clock.now().duration_since(started);
            match poll_decision(
                cancelled,
                exceeded_stream,
                outcome,
                reader_done.load(Ordering::Acquire),
                elapsed,
                planned.timeout(),
                poll_interval,
            ) {
                PollDecision::Cancel => {
                    let terminated = terminate_and_collect(TerminationRequest {
                        child: &mut *child,
                        project: planned.project(),
                        task: planned.id(),
                        reason: TerminationReason::Cancellation,
                        started,
                        clock: self.clock.as_ref(),
                        stdout_reader,
                        stderr_reader,
                    })?;
                    return Err(RunnerError::Cancelled(Box::new(CancelledTask {
                        project: planned.project().to_owned(),
                        task: planned.id().to_owned(),
                        output: terminated.output,
                        elapsed: terminated.elapsed,
                    })));
                }
                PollDecision::OutputLimit(stream_name) => {
                    let terminated = terminate_and_collect(TerminationRequest {
                        child: &mut *child,
                        project: planned.project(),
                        task: planned.id(),
                        reason: TerminationReason::OutputLimit,
                        started,
                        clock: self.clock.as_ref(),
                        stdout_reader,
                        stderr_reader,
                    })?;
                    return Err(RunnerError::OutputLimit(Box::new(OutputLimitTask {
                        project: planned.project().to_owned(),
                        task: planned.id().to_owned(),
                        stream: stream_name,
                        limit: output_limit,
                        output: terminated.output,
                        elapsed: terminated.elapsed,
                    })));
                }
                PollDecision::Exited {
                    descendants_may_hold_pipes,
                } => {
                    if descendants_may_hold_pipes {
                        terminate_tree(
                            &mut *child,
                            planned.project(),
                            planned.id(),
                            TerminationReason::DescendantCleanup,
                        )?;
                    }
                    match wait {
                        Some(Ok(Some(exit))) => break exit,
                        _ => unreachable!("PollDecision::Exited implies an exited child"),
                    }
                }
                PollDecision::Timeout => {
                    let terminated = terminate_and_collect(TerminationRequest {
                        child: &mut *child,
                        project: planned.project(),
                        task: planned.id(),
                        reason: TerminationReason::Timeout,
                        started,
                        clock: self.clock.as_ref(),
                        stdout_reader,
                        stderr_reader,
                    })?;
                    return Err(RunnerError::TimedOut(Box::new(TimedOutTask {
                        project: planned.project().to_owned(),
                        task: planned.id().to_owned(),
                        command: planned.command().to_vec(),
                        cwd: planned.cwd().to_path_buf(),
                        timeout: planned.timeout(),
                        output: terminated.output,
                        elapsed: terminated.elapsed,
                    })));
                }
                PollDecision::WaitFailed => {
                    if let Err(error) = terminate_tree(
                        &mut *child,
                        planned.project(),
                        planned.id(),
                        TerminationReason::WaitFailure,
                    ) {
                        let _ = stdout_reader.join();
                        let _ = stderr_reader.join();
                        return Err(error);
                    }
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    let source = match wait {
                        Some(Err(source)) => source,
                        _ => unreachable!("PollDecision::WaitFailed implies a failed wait"),
                    };
                    return Err(RunnerError::Wait {
                        project: planned.project().to_owned(),
                        task: planned.id().to_owned(),
                        source,
                    });
                }
                PollDecision::Sleep(duration) => {
                    self.clock.sleep(duration);
                    poll_interval = next_poll_interval(poll_interval);
                }
            }
        };

        let joined = join_output(
            planned.project(),
            planned.id(),
            stdout_reader,
            stderr_reader,
        )?;
        let elapsed = self.clock.now().duration_since(started);
        if let Some(stream) = joined.exceeded_stream {
            return Err(RunnerError::OutputLimit(Box::new(OutputLimitTask {
                project: planned.project().to_owned(),
                task: planned.id().to_owned(),
                stream,
                limit: output_limit,
                output: joined.output,
                elapsed,
            })));
        }
        let output = joined.output;
        if !exit.success {
            return Err(RunnerError::Failed(Box::new(FailedTask {
                project: planned.project().to_owned(),
                task: planned.id().to_owned(),
                command: planned.command().to_vec(),
                cwd: planned.cwd().to_path_buf(),
                code: exit.code,
                output,
                elapsed,
            })));
        }

        Ok(TaskResult {
            output,
            elapsed,
            cached: false,
        })
    }
}

impl TaskExecutor for Runner {
    fn execute(
        &self,
        project_root: &Path,
        planned: &PlannedTask,
        cancellation: Option<&CancellationToken>,
        output_callback: Option<OutputCallback>,
    ) -> Result<TaskResult, RunnerError> {
        self.run_with_options(project_root, planned, cancellation, output_callback)
    }
}

#[derive(Debug)]
struct StreamCapture {
    bytes: Vec<u8>,
    exceeded: bool,
}

struct JoinedOutput {
    output: CapturedOutput,
    exceeded_stream: Option<&'static str>,
}

struct TerminatedOutput {
    output: CapturedOutput,
    elapsed: Duration,
}

struct TerminationRequest<'a> {
    child: &'a mut dyn ChildProcess,
    project: &'a str,
    task: &'a str,
    reason: TerminationReason,
    started: Instant,
    clock: &'a dyn RunnerClock,
    stdout_reader: thread::JoinHandle<io::Result<StreamCapture>>,
    stderr_reader: thread::JoinHandle<io::Result<StreamCapture>>,
}

fn terminate_and_collect(request: TerminationRequest<'_>) -> Result<TerminatedOutput, RunnerError> {
    let TerminationRequest {
        child,
        project,
        task,
        reason,
        started,
        clock,
        stdout_reader,
        stderr_reader,
    } = request;
    terminate_tree(child, project, task, reason)?;
    let wait_result = child.wait();
    // Join both readers before checking either result. A wait error must not
    // detach a reader blocked on a descendant pipe.
    let joined = join_output(project, task, stdout_reader, stderr_reader);
    wait_result.map_err(|source| RunnerError::Wait {
        project: project.to_owned(),
        task: task.to_owned(),
        source,
    })?;
    let output = joined?.output;
    Ok(TerminatedOutput {
        output,
        elapsed: clock.now().duration_since(started),
    })
}

fn terminate_tree(
    child: &mut dyn ChildProcess,
    project: &str,
    task: &str,
    reason: TerminationReason,
) -> Result<(), RunnerError> {
    child
        .terminate_tree()
        .map(|_| ())
        .map_err(|source| RunnerError::Terminate {
            project: project.to_owned(),
            task: task.to_owned(),
            reason,
            source,
        })
}

fn read_stream(
    mut stream: Box<dyn Read + Send>,
    limit: usize,
    stream_name: &'static str,
    limit_sender: mpsc::Sender<&'static str>,
    output_callback: Option<OutputCallback>,
) -> io::Result<StreamCapture> {
    let mut output = Vec::with_capacity(limit.min(8192));
    let mut buffer = [0u8; 8192];
    let mut exceeded = false;
    loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        if exceeded {
            continue;
        }
        let remaining = limit.saturating_sub(output.len());
        let visible = read.min(remaining);
        if let Some(callback) = output_callback.as_ref() {
            callback(stream_name, &buffer[..visible])?;
        }
        if read > remaining {
            output.extend_from_slice(&buffer[..remaining]);
            exceeded = true;
            let _ = limit_sender.send(stream_name);
            continue;
        }
        output.extend_from_slice(&buffer[..read]);
    }
    Ok(StreamCapture {
        bytes: output,
        exceeded,
    })
}

fn join_output(
    project: &str,
    task: &str,
    stdout_reader: thread::JoinHandle<io::Result<StreamCapture>>,
    stderr_reader: thread::JoinHandle<io::Result<StreamCapture>>,
) -> Result<JoinedOutput, RunnerError> {
    // Join both reader threads before checking either result so a failure
    // from the first stream does not detach the second reader thread.
    let stdout_result = stdout_reader.join().expect("stdout reader thread panicked");
    let stderr_result = stderr_reader.join().expect("stderr reader thread panicked");
    let stdout = stdout_result.map_err(|source| RunnerError::OutputRead {
        project: project.to_owned(),
        task: task.to_owned(),
        stream: "stdout",
        source,
    })?;
    let stderr = stderr_result.map_err(|source| RunnerError::OutputRead {
        project: project.to_owned(),
        task: task.to_owned(),
        stream: "stderr",
        source,
    })?;
    let exceeded_stream = stdout
        .exceeded
        .then_some("stdout")
        .or_else(|| stderr.exceeded.then_some("stderr"));
    Ok(JoinedOutput {
        output: CapturedOutput {
            stdout: stdout.bytes,
            stderr: stderr.bytes,
        },
        exceeded_stream,
    })
}

/// Details shared by a process that exited unsuccessfully.
#[derive(Debug)]
pub struct FailedTask {
    project: String,
    task: String,
    command: Vec<String>,
    cwd: PathBuf,
    code: Option<i32>,
    output: CapturedOutput,
    elapsed: Duration,
}

/// Details shared by a process that exceeded its configured timeout.
#[derive(Debug)]
pub struct TimedOutTask {
    project: String,
    task: String,
    command: Vec<String>,
    cwd: PathBuf,
    timeout: Duration,
    output: CapturedOutput,
    elapsed: Duration,
}

#[derive(Debug)]
pub struct OutputLimitTask {
    project: String,
    task: String,
    stream: &'static str,
    limit: usize,
    output: CapturedOutput,
    elapsed: Duration,
}

#[derive(Debug)]
pub struct CancelledTask {
    project: String,
    task: String,
    output: CapturedOutput,
    elapsed: Duration,
}

/// Expected failures while spawning or executing a project task.
#[derive(Debug)]
pub enum RunnerError {
    EmptyCommand {
        project: String,
        task: String,
    },
    Spawn {
        project: String,
        task: String,
        command: Vec<String>,
        cwd: PathBuf,
        source: io::Error,
    },
    Wait {
        project: String,
        task: String,
        source: io::Error,
    },
    OutputRead {
        project: String,
        task: String,
        stream: &'static str,
        source: io::Error,
    },
    Terminate {
        project: String,
        task: String,
        reason: TerminationReason,
        source: io::Error,
    },
    OutputLimit(Box<OutputLimitTask>),
    Cancelled(Box<CancelledTask>),
    Failed(Box<FailedTask>),
    TimedOut(Box<TimedOutTask>),
}

impl RunnerError {
    pub(crate) fn output(&self) -> Option<&CapturedOutput> {
        match self {
            Self::Failed(details) => Some(&details.output),
            Self::TimedOut(details) => Some(&details.output),
            Self::OutputLimit(details) => Some(&details.output),
            Self::Cancelled(details) => Some(&details.output),
            Self::EmptyCommand { .. }
            | Self::Spawn { .. }
            | Self::Wait { .. }
            | Self::OutputRead { .. }
            | Self::Terminate { .. } => None,
        }
    }

    pub(crate) fn elapsed(&self) -> Option<Duration> {
        match self {
            Self::Failed(details) => Some(details.elapsed),
            Self::TimedOut(details) => Some(details.elapsed),
            Self::OutputLimit(details) => Some(details.elapsed),
            Self::Cancelled(details) => Some(details.elapsed),
            Self::EmptyCommand { .. }
            | Self::Spawn { .. }
            | Self::Wait { .. }
            | Self::OutputRead { .. }
            | Self::Terminate { .. } => None,
        }
    }

    /// The lifecycle status this failure presents as.
    ///
    /// One mapping, next to the error vocabulary, so the JSON `status` field
    /// and the run summary can never disagree about what a failure was.
    pub(crate) fn status(&self) -> crate::events::TaskStatus {
        use crate::events::TaskStatus;

        match self {
            Self::TimedOut(_) => TaskStatus::TimedOut,
            Self::OutputLimit(_) => TaskStatus::OutputLimit,
            Self::Cancelled(_) => TaskStatus::Cancelled,
            Self::EmptyCommand { .. }
            | Self::Spawn { .. }
            | Self::Wait { .. }
            | Self::OutputRead { .. }
            | Self::Terminate { .. }
            | Self::Failed(_) => TaskStatus::Failed,
        }
    }
}

impl fmt::Display for RunnerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyCommand { project, task } => {
                write!(f, "project '{project}' task '{task}' has an empty command")
            }
            Self::Spawn {
                project,
                task,
                command,
                cwd,
                source,
            } => write!(
                f,
                "could not start {project}/{task} ({}) in {}: {source}",
                format_command(command),
                cwd.display()
            ),
            Self::Wait {
                project,
                task,
                source,
            } => write!(f, "could not wait for {project}/{task}: {source}"),
            Self::OutputRead {
                project,
                task,
                stream,
                source,
            } => write!(f, "could not read {stream} for {project}/{task}: {source}"),
            Self::Terminate {
                project,
                task,
                reason,
                source,
            } => write!(
                f,
                "could not terminate {project}/{task} during {reason}: {source}"
            ),
            Self::OutputLimit(details) => write!(
                f,
                "{}/{} exceeded the {} output limit of {} bytes",
                details.project, details.task, details.stream, details.limit
            ),
            Self::Cancelled(details) => {
                write!(f, "{}/{} was cancelled", details.project, details.task)
            }
            Self::Failed(details) => write!(
                f,
                "{}/{} ({}) failed in {} with exit status {}",
                details.project,
                details.task,
                format_command(&details.command),
                details.cwd.display(),
                details.code.map_or_else(
                    || "terminated by signal".to_owned(),
                    |code| code.to_string()
                )
            ),
            Self::TimedOut(details) => write!(
                f,
                "{}/{} ({}) timed out after {}s in {}",
                details.project,
                details.task,
                format_command(&details.command),
                details.timeout.as_secs(),
                details.cwd.display()
            ),
        }
    }
}

impl StdError for RunnerError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Spawn { source, .. }
            | Self::Wait { source, .. }
            | Self::OutputRead { source, .. }
            | Self::Terminate { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Format an argv vector for dry-run output.
pub fn format_command(command: &[String]) -> String {
    command
        .iter()
        .map(|argument| {
            if argument
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "-_.=/:+".contains(character))
            {
                argument.clone()
            } else {
                format!("'{}'", argument.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::config_path;
    use crate::project::Project;
    use crate::testing::TempDir;
    use std::fs;
    use std::sync::Mutex;

    #[cfg(unix)]
    fn project_with_task(command: &str, timeout_seconds: Option<u64>) -> (TempDir, Project) {
        let temp = TempDir::new();
        let timeout = timeout_seconds
            .map(|seconds| format!("timeout_seconds = {seconds}\n"))
            .unwrap_or_default();
        fs::write(
            config_path(temp.path()),
            format!("[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"sh\", \"-c\", \"{command}\"]\n{timeout}"),
        )
        .expect("write project manifest");
        let project = Project::load(temp.path()).expect("project loads");
        (temp, project)
    }

    #[cfg(unix)]
    #[test]
    fn runs_a_task_and_returns_captured_output_without_printing() {
        let (_temp, project) = project_with_task("printf stdout; printf stderr >&2", None);
        let plan = project.plan(None, &[]).expect("plan succeeds");
        let result = Runner::new()
            .run(&project.root, &plan[0])
            .expect("task succeeds");

        assert_eq!(result.output.stdout, b"stdout");
        assert_eq!(result.output.stderr, b"stderr");
    }

    #[cfg(unix)]
    #[test]
    fn returns_failed_task_output_for_the_presenter() {
        let (_temp, project) = project_with_task("printf failed >&2; exit 7", None);
        let plan = project.plan(None, &[]).expect("plan succeeds");
        let error = Runner::new()
            .run(&project.root, &plan[0])
            .expect_err("task must fail");

        assert!(error.to_string().contains("fixture/build"));
        assert_eq!(
            error.output().expect("failed output is captured").stderr,
            b"failed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn timeout_terminates_descendants_before_their_delayed_write() {
        let temp = TempDir::new();
        let marker = temp.path().join("descendant-finished");
        let marker_text = marker.to_string_lossy();
        let (_temp, project) = project_with_task(
            &format!("(sleep 5; printf leaked > '{marker_text}') & wait"),
            Some(1),
        );
        let plan = project.plan(None, &[]).expect("plan succeeds");

        let error = Runner::new()
            .run(&project.root, &plan[0])
            .expect_err("task must time out");
        assert!(matches!(error, RunnerError::TimedOut(_)));

        std::thread::sleep(std::time::Duration::from_millis(1200));
        assert!(!marker.exists(), "a timed-out descendant kept running");
    }

    #[cfg(unix)]
    #[test]
    fn successful_exit_spares_a_detached_descendant() {
        let temp = TempDir::new();
        let marker = temp.path().join("descendant-finished");
        let marker_text = marker.to_string_lossy();
        // The direct child closes its own stdout/stderr first, so the output
        // readers reach EOF while it sleeps; then it detaches a descendant
        // whose stdout/stderr point at /dev/null and exits successfully.
        // Because the descendant never held the pipes, output collection
        // completes without touching it, and a normal successful exit must
        // not terminate its tree.  Only timeout and output-limit paths
        // terminate trees.
        let (_temp, project) = project_with_task(
            &format!(
                "exec 1>&- 2>&-; (sleep 3; printf leaked > '{marker_text}') </dev/null >/dev/null 2>&1 & sleep 0.5; exit 0"
            ),
            None,
        );
        let plan = project.plan(None, &[]).expect("plan succeeds");
        Runner::new()
            .run(&project.root, &plan[0])
            .expect("task succeeds");

        std::thread::sleep(std::time::Duration::from_secs(4));
        assert!(
            marker.exists(),
            "a successful task killed a detached descendant"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stops_and_reports_when_task_output_exceeds_the_limit() {
        let (_temp, mut project) = project_with_task("printf 123456789", None);
        project
            .tasks
            .get_mut("build")
            .expect("fixture task exists")
            .max_output_bytes = 8;
        let plan = project.plan(None, &[]).expect("plan succeeds");
        let error = Runner::new()
            .run(&project.root, &plan[0])
            .expect_err("task output must be bounded");

        assert!(
            matches!(error, RunnerError::OutputLimit(_)),
            "unexpected error: {error:?}"
        );
        assert!(error.to_string().contains("stdout output limit of 8 bytes"));
        assert_eq!(
            error.output().expect("partial output is retained").stdout,
            b"12345678"
        );
    }

    #[cfg(windows)]
    #[test]
    fn successful_exit_spares_a_detached_descendant_on_windows() {
        // Regression: a child that exits successfully must not kill its
        // descendants via Job Object cleanup.  The kill-on-close flag
        // must not be set (Finding 1 from the P0 review).
        let temp = TempDir::new();
        let marker = temp.path().join("descendant-finished");
        let marker_text = marker.to_string_lossy();
        let script = temp.path().join("spawn_and_exit.bat");

        // Lift escaped paths into a batch variable to avoid quoting chaos.
        fs::write(
            &script,
            format!(
                "@echo off\r\nstart /b cmd /c ping -n 4 127.0.0.1 >nul && echo leaked > \"{marker_text}\"\r\nexit /b 0\r\n"
            ),
        )
        .expect("write batch script");

        fs::write(
            config_path(temp.path()),
            "[project]\nname = \"app\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"cmd\", \"/c\", \"spawn_and_exit.bat\"]\n",
        )
        .expect("write manifest");

        let project = Project::load(temp.path()).expect("project loads");
        let plan = project.plan(None, &[]).expect("plan succeeds");
        Runner::new()
            .run(&project.root, &plan[0])
            .expect("task succeeds");

        std::thread::sleep(std::time::Duration::from_secs(6));
        assert!(
            marker.exists(),
            "a successful task killed a detached descendant on Windows"
        );
    }

    #[test]
    fn a_failure_maps_onto_exactly_one_lifecycle_status() {
        use crate::events::TaskStatus;

        assert_eq!(
            RunnerError::Cancelled(Box::new(CancelledTask {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
                output: CapturedOutput::default(),
                elapsed: Duration::ZERO,
            }))
            .status(),
            TaskStatus::Cancelled
        );
        assert_eq!(
            RunnerError::TimedOut(Box::new(TimedOutTask {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
                command: vec!["sleep".to_owned()],
                cwd: PathBuf::from("/tmp"),
                timeout: Duration::from_secs(1),
                output: CapturedOutput::default(),
                elapsed: Duration::ZERO,
            }))
            .status(),
            TaskStatus::TimedOut
        );
        assert_eq!(
            RunnerError::OutputLimit(Box::new(OutputLimitTask {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
                stream: "stdout",
                limit: 8,
                output: CapturedOutput::default(),
                elapsed: Duration::ZERO,
            }))
            .status(),
            TaskStatus::OutputLimit
        );
        assert_eq!(
            RunnerError::EmptyCommand {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
            }
            .status(),
            TaskStatus::Failed
        );
    }

    #[test]
    fn formats_simple_commands_without_shell_noise() {
        assert_eq!(
            format_command(&["make".to_owned(), "build".to_owned(), "--locked".to_owned()]),
            "make build --locked"
        );
    }

    #[test]
    fn quotes_arguments_that_need_shell_safe_display() {
        assert_eq!(
            format_command(&["echo".to_owned(), "hello world".to_owned()]),
            "echo 'hello world'"
        );
    }

    // `PollDecision`, `WaitOutcome`, `poll_decision`, `next_poll_interval`, and
    // the `POLL_INTERVAL_*` constants are reachable through the existing
    // `use super::*;` at the top of this module.
    const TIMEOUT: Duration = Duration::from_secs(10);

    #[test]
    fn cancellation_outranks_every_other_condition() {
        assert_eq!(
            poll_decision(
                true,
                Some("stdout"),
                WaitOutcome::Exited,
                2,
                Duration::ZERO,
                TIMEOUT,
                POLL_INTERVAL_START,
            ),
            PollDecision::Cancel
        );
    }

    #[test]
    fn an_exceeded_stream_stops_the_task_before_the_exit_is_considered() {
        assert_eq!(
            poll_decision(
                false,
                Some("stderr"),
                WaitOutcome::Exited,
                2,
                Duration::ZERO,
                TIMEOUT,
                POLL_INTERVAL_START,
            ),
            PollDecision::OutputLimit("stderr")
        );
    }

    #[test]
    fn an_exit_wants_descendant_cleanup_until_both_readers_finish() {
        for (readers_done, descendants_may_hold_pipes) in [(0, true), (1, true), (2, false)] {
            assert_eq!(
                poll_decision(
                    false,
                    None,
                    WaitOutcome::Exited,
                    readers_done,
                    Duration::ZERO,
                    TIMEOUT,
                    POLL_INTERVAL_START,
                ),
                PollDecision::Exited {
                    descendants_may_hold_pipes
                },
                "readers_done={readers_done}"
            );
        }
    }

    #[test]
    fn a_running_child_past_its_timeout_times_out() {
        assert_eq!(
            poll_decision(
                false,
                None,
                WaitOutcome::Running,
                2,
                TIMEOUT,
                TIMEOUT,
                POLL_INTERVAL_START,
            ),
            PollDecision::Timeout
        );
    }

    #[test]
    fn a_running_child_sleeps_for_the_shorter_of_the_interval_and_the_remaining_time() {
        assert_eq!(
            poll_decision(
                false,
                None,
                WaitOutcome::Running,
                2,
                Duration::from_secs(9),
                TIMEOUT,
                Duration::from_millis(5),
            ),
            PollDecision::Sleep(Duration::from_millis(5))
        );
        assert_eq!(
            poll_decision(
                false,
                None,
                WaitOutcome::Running,
                2,
                Duration::from_millis(9_999),
                TIMEOUT,
                Duration::from_millis(5),
            ),
            PollDecision::Sleep(Duration::from_millis(1))
        );
    }

    #[test]
    fn a_failed_wait_is_reported() {
        assert_eq!(
            poll_decision(
                false,
                None,
                WaitOutcome::Failed,
                2,
                Duration::ZERO,
                TIMEOUT,
                POLL_INTERVAL_START,
            ),
            PollDecision::WaitFailed
        );
    }

    #[test]
    fn the_poll_interval_doubles_up_to_the_cap() {
        assert_eq!(
            next_poll_interval(POLL_INTERVAL_START),
            (POLL_INTERVAL_START * 2).min(POLL_INTERVAL_MAX)
        );
        assert_eq!(next_poll_interval(POLL_INTERVAL_MAX), POLL_INTERVAL_MAX);
    }

    #[test]
    fn runner_errors_expose_a_source_exactly_when_they_wrap_one() {
        let io = || io::Error::new(io::ErrorKind::NotFound, "missing");

        let with_source = [
            RunnerError::Spawn {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
                command: vec!["tool".to_owned()],
                cwd: PathBuf::from("/tmp"),
                source: io(),
            },
            RunnerError::Wait {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
                source: io(),
            },
            RunnerError::OutputRead {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
                stream: "stdout",
                source: io(),
            },
            RunnerError::Terminate {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
                reason: TerminationReason::Timeout,
                source: io(),
            },
        ];
        for error in &with_source {
            assert!(error.source().is_some(), "{error}");
        }

        let bare = [
            RunnerError::EmptyCommand {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
            },
            RunnerError::Failed(Box::new(FailedTask {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
                command: vec!["tool".to_owned()],
                cwd: PathBuf::from("/tmp"),
                code: Some(1),
                output: CapturedOutput::default(),
                elapsed: Duration::ZERO,
            })),
            RunnerError::TimedOut(Box::new(TimedOutTask {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
                command: vec!["tool".to_owned()],
                cwd: PathBuf::from("/tmp"),
                timeout: Duration::from_secs(1),
                output: CapturedOutput::default(),
                elapsed: Duration::ZERO,
            })),
            RunnerError::OutputLimit(Box::new(OutputLimitTask {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
                stream: "stdout",
                limit: 8,
                output: CapturedOutput::default(),
                elapsed: Duration::ZERO,
            })),
            RunnerError::Cancelled(Box::new(CancelledTask {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
                output: CapturedOutput::default(),
                elapsed: Duration::ZERO,
            })),
        ];
        for error in &bare {
            assert!(error.source().is_none(), "{error}");
            assert!(!error.to_string().is_empty(), "{error}");
        }
    }

    #[test]
    fn read_stream_retains_bytes_up_to_the_exact_limit_without_a_notification() {
        let (limit_sender, limit_receiver) = mpsc::channel();

        let capture = read_stream(
            Box::new(std::io::Cursor::new(b"12345678".to_vec())),
            8,
            "stdout",
            limit_sender,
            None,
        )
        .expect("read succeeds");

        assert_eq!(capture.bytes, b"12345678");
        assert!(!capture.exceeded);
        assert!(
            limit_receiver.try_recv().is_err(),
            "an exact-limit stream must not send a limit notification"
        );
    }

    #[test]
    fn read_stream_truncates_and_reports_an_overflowing_stream_once() {
        let (limit_sender, limit_receiver) = mpsc::channel();
        let callback = Arc::new(|stream: &'static str, bytes: &[u8]| {
            assert_eq!(stream, "stdout");
            assert_eq!(
                bytes, b"12345678",
                "the callback sees only the visible prefix"
            );
            Ok(())
        }) as OutputCallback;

        let capture = read_stream(
            Box::new(std::io::Cursor::new(b"123456789".to_vec())),
            8,
            "stdout",
            limit_sender,
            Some(callback),
        )
        .expect("read succeeds");

        assert_eq!(capture.bytes, b"12345678");
        assert!(capture.exceeded);
        assert_eq!(limit_receiver.try_recv(), Ok("stdout"));
        assert!(
            limit_receiver.try_recv().is_err(),
            "the stream name must be sent exactly once"
        );
    }

    #[test]
    fn read_stream_returns_a_callback_io_error_to_the_reader() {
        let (limit_sender, _limit_receiver) = mpsc::channel();
        let callback = Arc::new(|_stream: &'static str, _bytes: &[u8]| {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "consumer hung up",
            ))
        }) as OutputCallback;

        let error = read_stream(
            Box::new(std::io::Cursor::new(b"123".to_vec())),
            8,
            "stdout",
            limit_sender,
            Some(callback),
        )
        .expect_err("a failing callback must fail the read");

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    // -----------------------------------------------------------------------
    // Deterministic fake-driver tests (Phase C)
    // -----------------------------------------------------------------------

    /// A fake child whose `try_wait` and `wait` responses are scripted.
    struct ScriptedChild {
        stdout_data: Vec<u8>,
        stderr_data: Vec<u8>,
        try_wait_states: Vec<io::Result<Option<ProcessExit>>>,
        wait_response: io::Result<ProcessExit>,
        terminated: Arc<AtomicBool>,
        cancel_on_first_try_wait: Option<CancellationToken>,
        blocking_readers: bool,
        next_try_wait: usize,
    }

    struct ReleaseReader {
        bytes: Vec<u8>,
        released: Arc<AtomicBool>,
        emitted: bool,
    }

    impl Read for ReleaseReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            while !self.released.load(Ordering::Acquire) {
                thread::yield_now();
            }
            if self.emitted {
                return Ok(0);
            }
            self.emitted = true;
            let length = self.bytes.len().min(buffer.len());
            buffer[..length].copy_from_slice(&self.bytes[..length]);
            Ok(length)
        }
    }

    impl ChildProcess for ScriptedChild {
        fn stdout(&mut self) -> Option<Box<dyn Read + Send>> {
            if self.blocking_readers {
                Some(Box::new(ReleaseReader {
                    bytes: std::mem::take(&mut self.stdout_data),
                    released: Arc::clone(&self.terminated),
                    emitted: false,
                }))
            } else {
                Some(Box::new(std::io::Cursor::new(std::mem::take(
                    &mut self.stdout_data,
                ))))
            }
        }

        fn stderr(&mut self) -> Option<Box<dyn Read + Send>> {
            if self.blocking_readers {
                Some(Box::new(ReleaseReader {
                    bytes: std::mem::take(&mut self.stderr_data),
                    released: Arc::clone(&self.terminated),
                    emitted: false,
                }))
            } else {
                Some(Box::new(std::io::Cursor::new(std::mem::take(
                    &mut self.stderr_data,
                ))))
            }
        }

        fn try_wait(&mut self) -> io::Result<Option<ProcessExit>> {
            let idx = self.next_try_wait;
            if idx == 0
                && let Some(cancellation) = &self.cancel_on_first_try_wait
            {
                cancellation.cancel();
            }
            // Re-spawn the last state once we run out of scripted entries.
            if idx >= self.try_wait_states.len() {
                let last = self.try_wait_states.last().expect("at least one state");
                return match last {
                    Ok(inner) => Ok(*inner),
                    Err(_) => Err(io::Error::other("wait failed")),
                };
            }
            self.next_try_wait += 1;
            match &self.try_wait_states[idx] {
                Ok(inner) => Ok(*inner),
                Err(_) => Err(io::Error::other("wait failed")),
            }
        }

        fn wait(&mut self) -> io::Result<ProcessExit> {
            match &self.wait_response {
                Ok(exit) => Ok(*exit),
                Err(_) => Err(io::Error::other("wait failed")),
            }
        }

        fn terminate_tree(&mut self) -> io::Result<TerminationOutcome> {
            self.terminated.store(true, Ordering::SeqCst);
            Ok(TerminationOutcome::Terminated)
        }
    }

    /// A launcher that produces a single pre-built `ScriptedChild`.
    struct FakeLauncher {
        child: Mutex<Option<ScriptedChild>>,
    }

    impl FakeLauncher {
        fn new(child: ScriptedChild) -> (Self, Arc<AtomicBool>) {
            let terminated_flag = Arc::clone(&child.terminated);
            (
                Self {
                    child: Mutex::new(Some(child)),
                },
                terminated_flag,
            )
        }
    }

    impl ProcessLauncher for FakeLauncher {
        fn spawn(&self, _spec: &ProcessSpec<'_>) -> io::Result<Box<dyn ChildProcess>> {
            Ok(Box::new(
                self.child
                    .lock()
                    .unwrap()
                    .take()
                    .expect("FakeLauncher spawned only once"),
            ))
        }
    }

    /// A clock whose `now` and `sleep` advance by scripted durations.
    #[derive(Clone)]
    struct FakeClock {
        now: Arc<Mutex<Instant>>,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                now: Arc::new(Mutex::new(Instant::now())),
            }
        }
    }

    impl RunnerClock for FakeClock {
        fn now(&self) -> Instant {
            *self.now.lock().unwrap()
        }

        fn sleep(&self, duration: Duration) {
            thread::yield_now();
            let mut now = self.now.lock().unwrap();
            *now += duration;
        }
    }

    /// A child whose process group is already gone reports an explicit
    /// `AlreadyExited` termination outcome.
    struct AlreadyGoneChild {
        exit: Option<ProcessExit>,
    }

    impl ChildProcess for AlreadyGoneChild {
        fn stdout(&mut self) -> Option<Box<dyn Read + Send>> {
            None
        }

        fn stderr(&mut self) -> Option<Box<dyn Read + Send>> {
            None
        }

        fn try_wait(&mut self) -> io::Result<Option<ProcessExit>> {
            Ok(self.exit)
        }

        fn wait(&mut self) -> io::Result<ProcessExit> {
            self.exit.ok_or_else(|| io::Error::other("no exit"))
        }

        fn terminate_tree(&mut self) -> io::Result<TerminationOutcome> {
            if self.exit.is_some() {
                Ok(TerminationOutcome::AlreadyExited)
            } else {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            }
        }
    }

    #[test]
    fn an_already_exited_tree_is_not_a_cleanup_failure() {
        let mut child = AlreadyGoneChild {
            exit: Some(ProcessExit {
                code: Some(0),
                success: true,
            }),
        };

        assert!(
            terminate_tree(
                &mut child,
                "fixture",
                "build",
                TerminationReason::OutputLimit,
            )
            .is_ok(),
            "an already-exited tree is not a termination failure"
        );
    }

    #[test]
    fn a_failed_terminate_on_a_running_child_is_reported() {
        let mut child = AlreadyGoneChild { exit: None };

        let error = terminate_tree(&mut child, "fixture", "build", TerminationReason::Timeout)
            .expect_err("a live tree that cannot be signalled must be reported");

        assert!(matches!(
            error,
            RunnerError::Terminate {
                reason: TerminationReason::Timeout,
                ..
            }
        ));
    }

    /// Helper: create a small PlannedTask for fake-runner tests.
    fn fake_planned_task(timeout_secs: u64) -> (TempDir, PlannedTask) {
        let temp = TempDir::new();
        let config = format!(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"hi\"]\ntimeout_seconds = {timeout_secs}\n"
        );
        std::fs::write(crate::config::config_path(temp.path()), &config)
            .expect("write project config");
        let project = Project::load(temp.path()).expect("project loads");
        let plan = project.plan(None, &[]).expect("plan succeeds");
        (temp, plan.into_iter().next().expect("one planned task"))
    }

    #[test]
    fn fake_timeout_returns_timed_out_without_real_waiting() {
        // Child never exits; clock advances past the timeout.
        let child = ScriptedChild {
            stdout_data: Vec::new(),
            stderr_data: Vec::new(),
            try_wait_states: vec![Ok(None)],
            wait_response: Ok(ProcessExit {
                code: Some(0),
                success: true,
            }),
            terminated: Arc::new(AtomicBool::new(false)),
            cancel_on_first_try_wait: None,
            blocking_readers: false,
            next_try_wait: 0,
        };
        let (launcher, _terminated) = FakeLauncher::new(child);
        let clock = FakeClock::new();
        let runner = Runner::with_services(Arc::new(launcher), Arc::new(clock.clone()));
        let (_temp, planned) = fake_planned_task(1u64);

        // Advance the clock past the timeout before running so the first
        // poll immediately exceeds the timeout.
        *clock.now.lock().unwrap() += Duration::from_secs(2);

        let result = runner.run_with_options(planned.root(), &planned, None, None);

        assert!(
            matches!(&result, Err(RunnerError::TimedOut(..))),
            "expected TimedOut, got {result:?}"
        );
    }

    #[test]
    fn fake_cancellation_terminates_child_and_returns_cancelled() {
        let cancel = CancellationToken::new();
        let child = ScriptedChild {
            stdout_data: Vec::new(),
            stderr_data: Vec::new(),
            try_wait_states: vec![Ok(None)],
            wait_response: Ok(ProcessExit {
                code: Some(0),
                success: true,
            }),
            terminated: Arc::new(AtomicBool::new(false)),
            cancel_on_first_try_wait: Some(cancel.clone()),
            blocking_readers: false,
            next_try_wait: 0,
        };
        let terminated_flag = Arc::clone(&child.terminated);
        let (launcher, _) = FakeLauncher::new(child);
        let clock = FakeClock::new();
        let runner = Runner::with_services(Arc::new(launcher), Arc::new(clock));
        let (_temp, planned) = fake_planned_task(60);

        // The fake child cancels the token during the first in-flight poll.
        let result = runner.run_with_options(planned.root(), &planned, Some(&cancel), None);

        assert!(
            matches!(&result, Err(RunnerError::Cancelled(..))),
            "expected Cancelled, got {result:?}"
        );
        // The driver calls terminate_tree when cancellation fires.
        assert!(terminated_flag.load(Ordering::SeqCst));
    }

    #[test]
    fn fake_wait_error_returns_wait_and_joins_readers() {
        let child = ScriptedChild {
            stdout_data: b"x".to_vec(),
            stderr_data: b"y".to_vec(),
            try_wait_states: vec![Err(io::Error::other("broken"))],
            wait_response: Err(io::Error::other("broken")),
            terminated: Arc::new(AtomicBool::new(false)),
            cancel_on_first_try_wait: None,
            blocking_readers: false,
            next_try_wait: 0,
        };
        let (launcher, _terminated) = FakeLauncher::new(child);
        let clock = FakeClock::new();
        let runner = Runner::with_services(Arc::new(launcher), Arc::new(clock));
        let (_temp, planned) = fake_planned_task(60);

        let result = runner.run_with_options(planned.root(), &planned, None, None);

        assert!(
            matches!(&result, Err(RunnerError::Wait { .. })),
            "expected Wait, got {result:?}"
        );
    }

    #[test]
    fn fake_callback_error_produces_output_read() {
        let child = ScriptedChild {
            stdout_data: b"hello".to_vec(),
            stderr_data: Vec::new(),
            try_wait_states: vec![Ok(Some(ProcessExit {
                code: Some(0),
                success: true,
            }))],
            wait_response: Ok(ProcessExit {
                code: Some(0),
                success: true,
            }),
            terminated: Arc::new(AtomicBool::new(false)),
            cancel_on_first_try_wait: None,
            blocking_readers: false,
            next_try_wait: 0,
        };
        let (launcher, _terminated) = FakeLauncher::new(child);
        let clock = FakeClock::new();
        let runner = Runner::with_services(Arc::new(launcher), Arc::new(clock));
        let (_temp, planned) = fake_planned_task(60);
        let callback = Arc::new(|_stream: &'static str, _bytes: &[u8]| {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "consumer failed"))
        }) as OutputCallback;

        let result = runner.run_with_options(planned.root(), &planned, None, Some(callback));

        assert!(
            matches!(
                &result,
                Err(RunnerError::OutputRead {
                    stream: "stdout",
                    ..
                })
            ),
            "expected OutputRead stdout, got {result:?}"
        );
    }

    #[test]
    fn fake_exit_unfinished_readers_invokes_descendant_cleanup() {
        let terminated = Arc::new(AtomicBool::new(false));
        let child = ScriptedChild {
            stdout_data: b"hello".to_vec(),
            stderr_data: b"world".to_vec(),
            try_wait_states: vec![Ok(Some(ProcessExit {
                code: Some(0),
                success: true,
            }))],
            wait_response: Ok(ProcessExit {
                code: Some(0),
                success: true,
            }),
            terminated: Arc::clone(&terminated),
            cancel_on_first_try_wait: None,
            blocking_readers: true,
            next_try_wait: 0,
        };
        let flag = Arc::clone(&child.terminated);
        let (launcher, _) = FakeLauncher::new(child);
        let clock = FakeClock::new();
        let runner = Runner::with_services(Arc::new(launcher), Arc::new(clock));
        let (_temp, planned) = fake_planned_task(60);

        let result = runner.run_with_options(planned.root(), &planned, None, None);

        assert!(result.is_ok(), "expected Ok, got {result:?}");
        assert!(
            flag.load(Ordering::SeqCst),
            "unfinished readers must trigger descendant cleanup"
        );
    }

    #[test]
    fn fake_failed_exit_maps_status_code() {
        let child = ScriptedChild {
            stdout_data: b"error".to_vec(),
            stderr_data: Vec::new(),
            try_wait_states: vec![Ok(Some(ProcessExit {
                code: Some(42),
                success: false,
            }))],
            wait_response: Ok(ProcessExit {
                code: Some(42),
                success: false,
            }),
            terminated: Arc::new(AtomicBool::new(false)),
            cancel_on_first_try_wait: None,
            blocking_readers: false,
            next_try_wait: 0,
        };
        let (launcher, _terminated) = FakeLauncher::new(child);
        let clock = FakeClock::new();
        let runner = Runner::with_services(Arc::new(launcher), Arc::new(clock));
        let (_temp, planned) = fake_planned_task(60);

        let result = runner.run_with_options(planned.root(), &planned, None, None);

        match result {
            Err(RunnerError::Failed(ref failed)) => {
                assert_eq!(failed.code, Some(42), "exit code mismatch");
                assert_eq!(failed.output.stdout, b"error");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
