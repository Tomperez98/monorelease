//! Structured subprocess execution for root-project tasks.

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
use crate::process::{ManagedChild, WaitResult};
use crate::project::PlannedTask;

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

/// Executes root-project tasks without changing the process-global working directory.
#[derive(Debug, Clone, Default)]
pub struct Runner;

impl Runner {
    pub fn new() -> Self {
        Self
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
                task: planned.task().to_owned(),
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
                    task: planned.task().to_owned(),
                })?;
        assert!(planned.timeout() > Duration::ZERO);
        assert!(planned.max_output_bytes() > 0);
        assert!(planned.root().starts_with(project_root));
        assert!(planned.cwd().is_absolute());
        assert!(planned.cwd().starts_with(planned.root()));

        let started = Instant::now();
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(planned.cwd())
            .envs(planned.env())
            .stdin(match planned.stdin() {
                StdinMode::Null => Stdio::null(),
                StdinMode::Inherit => Stdio::inherit(),
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut managed =
            ManagedChild::spawn(&mut command).map_err(|source| RunnerError::Spawn {
                project: planned.project().to_owned(),
                task: planned.task().to_owned(),
                command: planned.command().to_vec(),
                cwd: planned.cwd().to_path_buf(),
                source,
            })?;

        let stdout = managed
            .take_stdout()
            .expect("stdout was configured as piped");
        let stderr = managed
            .take_stderr()
            .expect("stderr was configured as piped");
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
        let status = loop {
            if exceeded_stream.is_none()
                && let Ok(stream) = limit_receiver.try_recv()
            {
                exceeded_stream = Some(stream);
            }
            let cancelled = cancellation.is_some_and(CancellationToken::is_cancelled);
            let wait = (!cancelled).then(|| managed.try_wait());
            let outcome = match &wait {
                Some(Ok(WaitResult::Exited(_))) => WaitOutcome::Exited,
                Some(Err(_)) => WaitOutcome::Failed,
                _ => WaitOutcome::Running,
            };
            match poll_decision(
                cancelled,
                exceeded_stream,
                outcome,
                reader_done.load(Ordering::Acquire),
                started.elapsed(),
                planned.timeout(),
                poll_interval,
            ) {
                PollDecision::Cancel => {
                    terminate_tree(
                        &mut managed,
                        planned.project(),
                        planned.task(),
                        "cancellation",
                    )?;
                    managed.wait().map_err(|source| RunnerError::Wait {
                        project: planned.project().to_owned(),
                        task: planned.task().to_owned(),
                        source,
                    })?;
                    let output = join_output(
                        planned.project(),
                        planned.task(),
                        stdout_reader,
                        stderr_reader,
                    )?
                    .output;
                    return Err(RunnerError::Cancelled(Box::new(CancelledTask {
                        project: planned.project().to_owned(),
                        task: planned.task().to_owned(),
                        output,
                        elapsed: started.elapsed(),
                    })));
                }
                PollDecision::OutputLimit(stream_name) => {
                    terminate_tree(
                        &mut managed,
                        planned.project(),
                        planned.task(),
                        "output limit",
                    )?;
                    managed.wait().map_err(|source| RunnerError::Wait {
                        project: planned.project().to_owned(),
                        task: planned.task().to_owned(),
                        source,
                    })?;
                    let joined = join_output(
                        planned.project(),
                        planned.task(),
                        stdout_reader,
                        stderr_reader,
                    )?;
                    return Err(RunnerError::OutputLimit(Box::new(OutputLimitTask {
                        project: planned.project().to_owned(),
                        task: planned.task().to_owned(),
                        stream: stream_name,
                        limit: output_limit,
                        output: joined.output,
                        elapsed: started.elapsed(),
                    })));
                }
                PollDecision::Exited {
                    descendants_may_hold_pipes,
                } => {
                    if descendants_may_hold_pipes {
                        terminate_tree(
                            &mut managed,
                            planned.project(),
                            planned.task(),
                            "descendant cleanup",
                        )?;
                    }
                    match wait {
                        Some(Ok(WaitResult::Exited(status))) => break status,
                        _ => unreachable!("PollDecision::Exited implies an exited child"),
                    }
                }
                PollDecision::Timeout => {
                    terminate_tree(&mut managed, planned.project(), planned.task(), "timeout")?;
                    managed.wait().map_err(|source| RunnerError::Wait {
                        project: planned.project().to_owned(),
                        task: planned.task().to_owned(),
                        source,
                    })?;
                    let output = join_output(
                        planned.project(),
                        planned.task(),
                        stdout_reader,
                        stderr_reader,
                    )?
                    .output;
                    return Err(RunnerError::TimedOut(Box::new(TimedOutTask {
                        project: planned.project().to_owned(),
                        task: planned.task().to_owned(),
                        command: planned.command().to_vec(),
                        cwd: planned.cwd().to_path_buf(),
                        timeout: planned.timeout(),
                        output,
                        elapsed: started.elapsed(),
                    })));
                }
                PollDecision::WaitFailed => {
                    if let Err(error) = terminate_tree(
                        &mut managed,
                        planned.project(),
                        planned.task(),
                        "wait error",
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
                        task: planned.task().to_owned(),
                        source,
                    });
                }
                PollDecision::Sleep(duration) => {
                    thread::sleep(duration);
                    poll_interval = next_poll_interval(poll_interval);
                }
            }
        };

        let joined = join_output(
            planned.project(),
            planned.task(),
            stdout_reader,
            stderr_reader,
        )?;
        let elapsed = started.elapsed();
        if let Some(stream) = joined.exceeded_stream {
            return Err(RunnerError::OutputLimit(Box::new(OutputLimitTask {
                project: planned.project().to_owned(),
                task: planned.task().to_owned(),
                stream,
                limit: output_limit,
                output: joined.output,
                elapsed,
            })));
        }
        let output = joined.output;
        if !status.success() {
            return Err(RunnerError::Failed(Box::new(FailedTask {
                project: planned.project().to_owned(),
                task: planned.task().to_owned(),
                command: planned.command().to_vec(),
                cwd: planned.cwd().to_path_buf(),
                code: status.code(),
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

struct StreamCapture {
    bytes: Vec<u8>,
    exceeded: bool,
}

struct JoinedOutput {
    output: CapturedOutput,
    exceeded_stream: Option<&'static str>,
}

fn terminate_tree(
    managed: &mut ManagedChild,
    project: &str,
    task: &str,
    operation: &'static str,
) -> Result<(), RunnerError> {
    managed
        .terminate_tree()
        .map_err(|source| RunnerError::Terminate {
            project: project.to_owned(),
            task: task.to_owned(),
            operation,
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
    let stdout = stdout_reader
        .join()
        .expect("stdout reader thread panicked")
        .map_err(|source| RunnerError::OutputRead {
            project: project.to_owned(),
            task: task.to_owned(),
            stream: "stdout",
            source,
        })?;
    let stderr = stderr_reader
        .join()
        .expect("stderr reader thread panicked")
        .map_err(|source| RunnerError::OutputRead {
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
        operation: &'static str,
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
                operation,
                source,
            } => write!(
                f,
                "could not terminate {project}/{task} during {operation}: {source}"
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
    fn terminates_a_task_that_exceeds_its_timeout() {
        let (_temp, project) = project_with_task("sleep 2", Some(1));
        let plan = project.plan(None, &[]).expect("plan succeeds");
        let error = Runner::new()
            .run(&project.root, &plan[0])
            .expect_err("task must time out");

        assert!(matches!(error, RunnerError::TimedOut(_)));
        assert!(error.to_string().contains("timed out after 1s"));
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_terminates_a_running_task() {
        let (_temp, project) = project_with_task("sleep 5", None);
        let plan = project.plan(None, &[]).expect("plan succeeds");
        let cancellation = CancellationToken::new();
        let signal = cancellation.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            signal.cancel();
        });

        let error = Runner::new()
            .run_with_options(&project.root, &plan[0], Some(&cancellation), None)
            .expect_err("task must be cancelled");
        thread.join().expect("cancellation thread joins");
        assert!(matches!(error, RunnerError::Cancelled(_)));
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

        assert!(matches!(error, RunnerError::OutputLimit(_)));
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
            format!(
                "[project]\nname = \"app\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"cmd\", \"/c\", \"spawn_and_exit.bat\"]\n"
            ),
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
                operation: "timeout",
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
}
