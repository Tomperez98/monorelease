//! Structured subprocess execution for workspace tasks.

use std::error::Error as StdError;
use std::fmt;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use crate::workspace::PlannedTask;

/// The execution context supplied to one task process.
#[derive(Debug, Clone)]
pub struct ExecutionContext {
    pub workspace_root: PathBuf,
    pub package_root: PathBuf,
    pub task_name: String,
    pub cwd: PathBuf,
    pub environment: std::collections::BTreeMap<String, String>,
}

/// Captured process output, emitted by the orchestrator after a task completes.
#[derive(Debug, Default)]
pub struct CapturedOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Executes package tasks without changing the process-global working directory.
#[derive(Debug)]
pub struct Runner {
    ci: bool,
}

impl Runner {
    pub fn new() -> Self {
        Self {
            ci: std::env::var_os("CI").is_some() || std::env::var_os("GITHUB_ACTIONS").is_some(),
        }
    }

    /// Execute one planned task and emit its output immediately.
    pub fn run(
        &self,
        workspace_root: &std::path::Path,
        planned: &PlannedTask,
    ) -> Result<(), RunnerError> {
        let output = self.run_captured(workspace_root, planned)?;
        emit_output(&output);
        Ok(())
    }

    /// Execute one planned task while keeping output available to a scheduler.
    pub fn run_captured(
        &self,
        workspace_root: &std::path::Path,
        planned: &PlannedTask,
    ) -> Result<CapturedOutput, RunnerError> {
        let (program, args) =
            planned
                .command
                .split_first()
                .ok_or_else(|| RunnerError::EmptyCommand {
                    package: planned.package.clone(),
                    task: planned.task.clone(),
                })?;
        let context = ExecutionContext {
            workspace_root: workspace_root.to_path_buf(),
            package_root: planned.package_path.clone(),
            task_name: planned.task.clone(),
            cwd: planned.cwd.clone(),
            environment: planned.env.clone(),
        };
        debug_assert!(context.package_root.starts_with(&context.workspace_root));
        let label = format!("{}:{}", planned.package, context.task_name);
        let started = Instant::now();

        self.open_section(&label);
        let output = Command::new(program)
            .args(args)
            .current_dir(&context.cwd)
            .envs(&context.environment)
            .output()
            .map_err(|source| RunnerError::Spawn {
                package: planned.package.clone(),
                task: planned.task.clone(),
                command: planned.command.clone(),
                cwd: context.cwd.clone(),
                source,
            });
        self.close_section(&label, started);

        let output = output?;
        let captured = CapturedOutput {
            stdout: output.stdout,
            stderr: output.stderr,
        };
        if !output.status.success() {
            emit_output(&captured);
            return Err(RunnerError::Failed {
                package: planned.package.clone(),
                task: planned.task.clone(),
                command: planned.command.clone(),
                cwd: context.cwd,
                code: output.status.code(),
            });
        }

        Ok(captured)
    }

    fn open_section(&self, name: &str) {
        if self.ci {
            println!("::group::{name}");
        } else {
            eprintln!("==> {name}");
        }
    }

    fn close_section(&self, name: &str, started: Instant) {
        let elapsed = started.elapsed();
        eprintln!("{name}: {}ms", elapsed.as_millis());
        if self.ci {
            println!("::endgroup::");
        }
    }
}

impl Default for Runner {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) fn emit_output(output: &CapturedOutput) {
    print_bytes(&output.stdout, false);
    print_bytes(&output.stderr, true);
}

fn print_bytes(bytes: &[u8], stderr: bool) {
    if bytes.is_empty() {
        return;
    }
    if stderr {
        eprint!("{}", String::from_utf8_lossy(bytes));
    } else {
        print!("{}", String::from_utf8_lossy(bytes));
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
        source: std::io::Error,
    },
    Failed {
        package: String,
        task: String,
        command: Vec<String>,
        cwd: PathBuf,
        code: Option<i32>,
    },
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
            Self::Failed {
                package,
                task,
                command,
                cwd,
                code,
            } => write!(
                f,
                "{package}/{task} ({}) failed in {} with exit code {}",
                format_command(command),
                cwd.display(),
                code.map_or_else(|| "unknown".to_owned(), |code| code.to_string())
            ),
        }
    }
}

impl StdError for RunnerError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Spawn { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
