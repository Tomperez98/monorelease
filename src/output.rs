//! Deterministic task output presentation.

use std::io::{self, Write};
use std::sync::Mutex;

use crate::runner::{RunnerError, TaskResult};
use crate::workspace::TaskNode;

/// Selects the output contract for a pipeline run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// Human-readable terminal output with status on stderr.
    Terminal,
    /// GitHub Actions log groups with status on stdout.
    GithubActions,
}

/// Owns task presentation so worker threads never write directly to the
/// process-global stdout or stderr handles.
#[derive(Debug)]
pub(crate) struct OutputSink {
    mode: OutputMode,
    lock: Mutex<()>,
}

impl OutputSink {
    pub(crate) fn new(mode: OutputMode) -> Self {
        Self {
            mode,
            lock: Mutex::new(()),
        }
    }

    pub(crate) fn present_start(&self, node: &TaskNode) -> io::Result<()> {
        let _guard = self.lock.lock().expect("output lock is not poisoned");
        self.write_status(|handle| writeln!(handle, "▶ {}:{}", node.package, node.task))
    }

    pub(crate) fn present_success(&self, node: &TaskNode, result: &TaskResult) -> io::Result<()> {
        let _guard = self.lock.lock().expect("output lock is not poisoned");
        self.start_section(node)?;
        write_bytes(&result.output.stdout, false)?;
        write_bytes(&result.output.stderr, true)?;
        let status = if result.cached {
            TaskStatus::Cached
        } else {
            TaskStatus::Completed
        };
        self.finish_section(node, result.elapsed, status)
    }

    pub(crate) fn present_failure(&self, node: &TaskNode, error: &RunnerError) -> io::Result<()> {
        let _guard = self.lock.lock().expect("output lock is not poisoned");
        self.start_section(node)?;
        if let Some(output) = error.output() {
            write_bytes(&output.stdout, false)?;
            write_bytes(&output.stderr, true)?;
        }
        let status = if matches!(error, RunnerError::TimedOut(_)) {
            TaskStatus::TimedOut
        } else {
            TaskStatus::Failed
        };
        self.finish_section(node, error.elapsed().unwrap_or_default(), status)
    }

    fn start_section(&self, node: &TaskNode) -> io::Result<()> {
        if self.mode == OutputMode::GithubActions {
            let mut stdout = io::stdout().lock();
            writeln!(stdout, "::group::{}:{}", node.package, node.task)?;
            stdout.flush()
        } else {
            Ok(())
        }
    }

    fn finish_section(
        &self,
        node: &TaskNode,
        elapsed: std::time::Duration,
        status: TaskStatus,
    ) -> io::Result<()> {
        self.write_status(|handle| {
            writeln!(
                handle,
                "{}:{}: {} in {}ms",
                node.package,
                node.task,
                status.label(),
                elapsed.as_millis()
            )
        })?;
        if self.mode == OutputMode::GithubActions {
            let mut stdout = io::stdout().lock();
            writeln!(stdout, "::endgroup::")?;
            stdout.flush()?;
        }
        Ok(())
    }

    pub(crate) fn present_summary(
        &self,
        summary: &crate::scheduler::ExecutionSummary,
    ) -> io::Result<()> {
        let _guard = self.lock.lock().expect("output lock is not poisoned");
        self.write_status(|handle| {
            writeln!(
                handle,
                "summary: {} completed, {} cached, {} failed, {} blocked",
                summary.completed, summary.cached, summary.failed, summary.blocked
            )
        })
    }

    fn write_status<F>(&self, write: F) -> io::Result<()>
    where
        F: FnOnce(&mut dyn Write) -> io::Result<()>,
    {
        if self.mode == OutputMode::GithubActions {
            let mut stdout = io::stdout().lock();
            write(&mut stdout)?;
            stdout.flush()
        } else {
            let mut stderr = io::stderr().lock();
            write(&mut stderr)?;
            stderr.flush()
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum TaskStatus {
    Completed,
    Cached,
    Failed,
    TimedOut,
}

impl TaskStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Cached => "cache hit",
            Self::Failed => "failed",
            Self::TimedOut => "timed out",
        }
    }
}

fn write_bytes(bytes: &[u8], stderr: bool) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    if stderr {
        let mut handle = io::stderr().lock();
        write_line_terminated(&mut handle, bytes)?;
        handle.flush()
    } else {
        let mut handle = io::stdout().lock();
        write_line_terminated(&mut handle, bytes)?;
        handle.flush()
    }
}

/// Keep task metadata on its own line when a command omits its final newline.
fn write_line_terminated(handle: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    handle.write_all(bytes)?;
    if !bytes.ends_with(b"\n") {
        handle.write_all(b"\n")?;
    }
    Ok(())
}
