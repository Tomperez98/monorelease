//! Structured subprocess execution for workspace tasks.

use std::error::Error as StdError;
use std::fmt;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::workspace::PlannedTask;

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

/// Executes package tasks without changing the process-global working directory.
#[derive(Debug, Clone, Default)]
pub struct Runner;

impl Runner {
    pub fn new() -> Self {
        Self
    }

    /// Execute one planned task and return its output and timing.
    pub fn run(
        &self,
        workspace_root: &Path,
        planned: &PlannedTask,
    ) -> Result<TaskResult, RunnerError> {
        let (program, args) =
            planned
                .command()
                .split_first()
                .ok_or_else(|| RunnerError::EmptyCommand {
                    package: planned.package().to_owned(),
                    task: planned.task().to_owned(),
                })?;
        assert!(planned.timeout() > Duration::ZERO);
        assert!(planned.max_output_bytes() > 0);
        assert!(planned.package_path().starts_with(workspace_root));
        assert!(planned.cwd().is_absolute());
        assert!(planned.cwd().starts_with(planned.package_path()));

        let started = Instant::now();
        let mut child = Command::new(program)
            .args(args)
            .current_dir(planned.cwd())
            .envs(planned.env())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| RunnerError::Spawn {
                package: planned.package().to_owned(),
                task: planned.task().to_owned(),
                command: planned.command().to_vec(),
                cwd: planned.cwd().to_path_buf(),
                source,
            })?;

        let stdout = child.stdout.take().expect("stdout was configured as piped");
        let stderr = child.stderr.take().expect("stderr was configured as piped");
        let (limit_sender, limit_receiver) = mpsc::channel();
        let output_limit = planned.max_output_bytes();
        let stdout_reader = {
            let limit_sender = limit_sender.clone();
            thread::spawn(move || read_stream(stdout, output_limit, "stdout", limit_sender))
        };
        let stderr_reader =
            thread::spawn(move || read_stream(stderr, output_limit, "stderr", limit_sender));

        // Poll with exponential backoff capped at the remaining timeout: a
        // short task is noticed within a millisecond, and a long task costs a
        // handful of wakeups per second instead of a hundred.
        let mut poll_interval = POLL_INTERVAL_START;
        let status = loop {
            if let Ok(stream) = limit_receiver.try_recv() {
                let _ = child.kill();
                let _ = child.wait();
                let joined = join_output(
                    planned.package(),
                    planned.task(),
                    stdout_reader,
                    stderr_reader,
                )?;
                return Err(RunnerError::OutputLimit(Box::new(OutputLimitTask {
                    package: planned.package().to_owned(),
                    task: planned.task().to_owned(),
                    stream,
                    limit: output_limit,
                    output: joined.output,
                    elapsed: started.elapsed(),
                })));
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() >= planned.timeout() => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let output = join_output(
                        planned.package(),
                        planned.task(),
                        stdout_reader,
                        stderr_reader,
                    )?
                    .output;
                    return Err(RunnerError::TimedOut(Box::new(TimedOutTask {
                        package: planned.package().to_owned(),
                        task: planned.task().to_owned(),
                        command: planned.command().to_vec(),
                        cwd: planned.cwd().to_path_buf(),
                        timeout: planned.timeout(),
                        output,
                        elapsed: started.elapsed(),
                    })));
                }
                Ok(None) => {
                    let remaining = planned.timeout().saturating_sub(started.elapsed());
                    thread::sleep(poll_interval.min(remaining));
                    poll_interval = (poll_interval * 2).min(POLL_INTERVAL_MAX);
                }
                Err(source) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    return Err(RunnerError::Wait {
                        package: planned.package().to_owned(),
                        task: planned.task().to_owned(),
                        source,
                    });
                }
            }
        };

        let joined = join_output(
            planned.package(),
            planned.task(),
            stdout_reader,
            stderr_reader,
        )?;
        let elapsed = started.elapsed();
        if let Some(stream) = joined.exceeded_stream {
            return Err(RunnerError::OutputLimit(Box::new(OutputLimitTask {
                package: planned.package().to_owned(),
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
                package: planned.package().to_owned(),
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

fn read_stream<R: Read>(
    mut stream: R,
    limit: usize,
    stream_name: &'static str,
    limit_sender: mpsc::Sender<&'static str>,
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
    package: &str,
    task: &str,
    stdout_reader: thread::JoinHandle<io::Result<StreamCapture>>,
    stderr_reader: thread::JoinHandle<io::Result<StreamCapture>>,
) -> Result<JoinedOutput, RunnerError> {
    let stdout = stdout_reader
        .join()
        .expect("stdout reader thread panicked")
        .map_err(|source| RunnerError::OutputRead {
            package: package.to_owned(),
            task: task.to_owned(),
            stream: "stdout",
            source,
        })?;
    let stderr = stderr_reader
        .join()
        .expect("stderr reader thread panicked")
        .map_err(|source| RunnerError::OutputRead {
            package: package.to_owned(),
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
    package: String,
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
    package: String,
    task: String,
    command: Vec<String>,
    cwd: PathBuf,
    timeout: Duration,
    output: CapturedOutput,
    elapsed: Duration,
}

#[derive(Debug)]
pub struct OutputLimitTask {
    package: String,
    task: String,
    stream: &'static str,
    limit: usize,
    output: CapturedOutput,
    elapsed: Duration,
}

/// Expected failures while spawning or executing a package task.
#[derive(Debug)]
pub enum RunnerError {
    EmptyCommand {
        package: String,
        task: String,
    },
    Spawn {
        package: String,
        task: String,
        command: Vec<String>,
        cwd: PathBuf,
        source: io::Error,
    },
    Wait {
        package: String,
        task: String,
        source: io::Error,
    },
    OutputRead {
        package: String,
        task: String,
        stream: &'static str,
        source: io::Error,
    },
    OutputLimit(Box<OutputLimitTask>),
    Failed(Box<FailedTask>),
    TimedOut(Box<TimedOutTask>),
}

impl RunnerError {
    pub(crate) fn output(&self) -> Option<&CapturedOutput> {
        match self {
            Self::Failed(details) => Some(&details.output),
            Self::TimedOut(details) => Some(&details.output),
            Self::OutputLimit(details) => Some(&details.output),
            Self::EmptyCommand { .. }
            | Self::Spawn { .. }
            | Self::Wait { .. }
            | Self::OutputRead { .. } => None,
        }
    }

    pub(crate) fn elapsed(&self) -> Option<Duration> {
        match self {
            Self::Failed(details) => Some(details.elapsed),
            Self::TimedOut(details) => Some(details.elapsed),
            Self::OutputLimit(details) => Some(details.elapsed),
            Self::EmptyCommand { .. }
            | Self::Spawn { .. }
            | Self::Wait { .. }
            | Self::OutputRead { .. } => None,
        }
    }
}

impl fmt::Display for RunnerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyCommand { package, task } => {
                write!(f, "package '{package}' task '{task}' has an empty command")
            }
            Self::Spawn {
                package,
                task,
                command,
                cwd,
                source,
            } => write!(
                f,
                "could not start {package}/{task} ({}) in {}: {source}",
                format_command(command),
                cwd.display()
            ),
            Self::Wait {
                package,
                task,
                source,
            } => write!(f, "could not wait for {package}/{task}: {source}"),
            Self::OutputRead {
                package,
                task,
                stream,
                source,
            } => write!(f, "could not read {stream} for {package}/{task}: {source}"),
            Self::OutputLimit(details) => write!(
                f,
                "{}/{} exceeded the {} output limit of {} bytes",
                details.package, details.task, details.stream, details.limit
            ),
            Self::Failed(details) => write!(
                f,
                "{}/{} ({}) failed in {} with exit status {}",
                details.package,
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
                details.package,
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
            | Self::OutputRead { source, .. } => Some(source),
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
    use crate::testing::TempDir;
    use crate::workspace::Workspace;
    use std::fs;

    fn workspace_with_task(command: &str, timeout_seconds: Option<u64>) -> (TempDir, Workspace) {
        let temp = TempDir::new();
        fs::write(
            config_path(temp.path()),
            "[workspace]\nname = \"fixture\"\nmembers = [\"packages/*\"]\n\n[pipelines.ci]\ntasks = [\"build\"]\n",
        )
        .expect("write root manifest");
        let package = temp.path().join("packages/app");
        fs::create_dir_all(&package).expect("create package");
        let timeout = timeout_seconds
            .map(|seconds| format!("timeout_seconds = {seconds}\n"))
            .unwrap_or_default();
        fs::write(
            config_path(&package),
            format!(
                "[package]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"sh\", \"-c\", \"{command}\"]\n{timeout}"
            ),
        )
        .expect("write package manifest");
        let workspace = Workspace::load(temp.path()).expect("workspace loads");
        (temp, workspace)
    }

    #[cfg(unix)]
    #[test]
    fn runs_a_task_and_returns_captured_output_without_printing() {
        let (_temp, workspace) = workspace_with_task("printf stdout; printf stderr >&2", None);
        let plan = workspace.plan(None, None, &[]).expect("plan succeeds");
        let result = Runner::new()
            .run(&workspace.root, &plan[0])
            .expect("task succeeds");

        assert_eq!(result.output.stdout, b"stdout");
        assert_eq!(result.output.stderr, b"stderr");
    }

    #[cfg(unix)]
    #[test]
    fn returns_failed_task_output_for_the_presenter() {
        let (_temp, workspace) = workspace_with_task("printf failed >&2; exit 7", None);
        let plan = workspace.plan(None, None, &[]).expect("plan succeeds");
        let error = Runner::new()
            .run(&workspace.root, &plan[0])
            .expect_err("task must fail");

        assert!(error.to_string().contains("app/build"));
        assert_eq!(
            error.output().expect("failed output is captured").stderr,
            b"failed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn terminates_a_task_that_exceeds_its_timeout() {
        let (_temp, workspace) = workspace_with_task("sleep 2", Some(1));
        let plan = workspace.plan(None, None, &[]).expect("plan succeeds");
        let error = Runner::new()
            .run(&workspace.root, &plan[0])
            .expect_err("task must time out");

        assert!(matches!(error, RunnerError::TimedOut(_)));
        assert!(error.to_string().contains("timed out after 1s"));
    }

    #[cfg(unix)]
    #[test]
    fn stops_and_reports_when_task_output_exceeds_the_limit() {
        let (_temp, mut workspace) = workspace_with_task("printf 123456789", None);
        workspace
            .packages
            .get_mut("app")
            .expect("fixture package exists")
            .tasks
            .get_mut("build")
            .expect("fixture task exists")
            .max_output_bytes = 8;
        let plan = workspace.plan(None, None, &[]).expect("plan succeeds");
        let error = Runner::new()
            .run(&workspace.root, &plan[0])
            .expect_err("task output must be bounded");

        assert!(matches!(error, RunnerError::OutputLimit(_)));
        assert!(error.to_string().contains("stdout output limit of 8 bytes"));
        assert_eq!(
            error.output().expect("partial output is retained").stdout,
            b"12345678"
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
