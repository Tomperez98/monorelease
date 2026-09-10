//! Deterministic task output presentation.

use std::io::{self, Write};
use std::sync::Mutex;

use crate::runner::{RunnerError, TaskResult};
use crate::workspace::TaskNode;

/// Owns task presentation so worker threads never write directly to the
/// process-global stdout or stderr handles.
#[derive(Debug)]
pub(crate) struct OutputSink {
    ci: bool,
    lock: Mutex<()>,
}

impl OutputSink {
    pub(crate) fn new() -> Self {
        Self {
            ci: std::env::var_os("CI").is_some() || std::env::var_os("GITHUB_ACTIONS").is_some(),
            lock: Mutex::new(()),
        }
    }

    pub(crate) fn present_success(&self, node: &TaskNode, result: &TaskResult) -> io::Result<()> {
        let _guard = self.lock.lock().expect("output lock is not poisoned");
        self.start_section(node)?;
        write_bytes(&result.output.stdout, false)?;
        write_bytes(&result.output.stderr, true)?;
        self.finish_section(node, result.elapsed)
    }

    pub(crate) fn present_failure(&self, node: &TaskNode, error: &RunnerError) -> io::Result<()> {
        let _guard = self.lock.lock().expect("output lock is not poisoned");
        self.start_section(node)?;
        if let Some(output) = error.output() {
            write_bytes(&output.stdout, false)?;
            write_bytes(&output.stderr, true)?;
        }
        self.finish_section(node, error.elapsed().unwrap_or_default())
    }

    fn start_section(&self, node: &TaskNode) -> io::Result<()> {
        if self.ci {
            let mut stdout = io::stdout().lock();
            writeln!(stdout, "::group::{}:{}", node.package, node.task)?;
            stdout.flush()
        } else {
            let mut stderr = io::stderr().lock();
            writeln!(stderr, "==> {}:{}", node.package, node.task)?;
            stderr.flush()
        }
    }

    fn finish_section(&self, node: &TaskNode, elapsed: std::time::Duration) -> io::Result<()> {
        let mut stderr = io::stderr().lock();
        writeln!(
            stderr,
            "{}:{}: {}ms",
            node.package,
            node.task,
            elapsed.as_millis()
        )?;
        stderr.flush()?;
        if self.ci {
            let mut stdout = io::stdout().lock();
            writeln!(stdout, "::endgroup::")?;
            stdout.flush()?;
        }
        Ok(())
    }
}

fn write_bytes(bytes: &[u8], stderr: bool) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    if stderr {
        let mut handle = io::stderr().lock();
        handle.write_all(bytes)?;
        handle.flush()
    } else {
        let mut handle = io::stdout().lock();
        handle.write_all(bytes)?;
        handle.flush()
    }
}
