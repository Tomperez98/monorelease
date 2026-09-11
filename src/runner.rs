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

        // Poll with exponential backoff capped at the remaining timeout: a
        // short task is noticed within a millisecond, and a long task costs a
        // handful of wakeups per second instead of a hundred.
        let mut poll_interval = POLL_INTERVAL_START;
        let mut exceeded_stream: Option<&'static str> = None;
        let status = loop {
            // Check stream-limit notification before checking child status
            // so overflow is detected even when the notification arrives
            // between polls.
            if cancellation.is_some_and(CancellationToken::is_cancelled) {
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
            if exceeded_stream.is_none()
                && let Ok(stream) = limit_receiver.try_recv()
            {
                exceeded_stream = Some(stream);
            }
            if let Some(stream_name) = exceeded_stream {
                // Terminate the tree so pipe-holding descendants release
                // the readers, then join them and report the overflow.
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
            match managed.try_wait() {
                Ok(WaitResult::Exited(status)) => {
                    // The child has exited. If both output readers have
                    // already finished (no descendant inherited the pipe),
                    // skip terminate_tree entirely.  Otherwise a descendant
                    // still holds a pipe open and we must terminate the tree
                    // to unblock the readers.
                    if reader_done.load(Ordering::Acquire) < 2 {
                        terminate_tree(
                            &mut managed,
                            planned.project(),
                            planned.task(),
                            "descendant cleanup",
                        )?;
                    }
                    break status;
                }
                Ok(WaitResult::Running) if started.elapsed() >= planned.timeout() => {
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
                Ok(WaitResult::Running) => {
                    let remaining = planned.timeout().saturating_sub(started.elapsed());
                    thread::sleep(poll_interval.min(remaining));
                    poll_interval = (poll_interval * 2).min(POLL_INTERVAL_MAX);
                }
                Err(source) => {
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
                    return Err(RunnerError::Wait {
                        project: planned.project().to_owned(),
                        task: planned.task().to_owned(),
                        source,
                    });
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
}
