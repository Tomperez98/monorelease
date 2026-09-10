//! Structured subprocess execution for workspace tasks.

use std::error::Error as StdError;
use std::fmt;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::workspace::PlannedTask;

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
        let stdout_reader = thread::spawn(move || read_stream(stdout));
        let stderr_reader = thread::spawn(move || read_stream(stderr));

        let status = loop {
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
                    )?;
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
                Ok(None) => thread::sleep(Duration::from_millis(10)),
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

        let output = join_output(
            planned.package(),
            planned.task(),
            stdout_reader,
            stderr_reader,
        )?;
        let elapsed = started.elapsed();
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

fn read_stream<R: Read>(mut stream: R) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    stream.read_to_end(&mut output)?;
    Ok(output)
}

fn join_output(
    package: &str,
    task: &str,
    stdout_reader: thread::JoinHandle<io::Result<Vec<u8>>>,
    stderr_reader: thread::JoinHandle<io::Result<Vec<u8>>>,
) -> Result<CapturedOutput, RunnerError> {
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
    Ok(CapturedOutput { stdout, stderr })
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
    Failed(Box<FailedTask>),
    TimedOut(Box<TimedOutTask>),
}

impl RunnerError {
    pub(crate) fn output(&self) -> Option<&CapturedOutput> {
        match self {
            Self::Failed(details) => Some(&details.output),
            Self::TimedOut(details) => Some(&details.output),
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
