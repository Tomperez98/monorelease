//! Deterministic task output presentation.
//!
//! Each output mode renders [`ExecutionEvent`] values through its own adapter,
//! so the scheduler never needs to know about terminals, JSON streams, or CI
//! providers.

use std::io::{self, Write};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::events::{ExecutionEvent, TaskStatus, TaskStream};
use crate::project::TaskNode;
use crate::runner::{RunnerError, TaskResult};

/// Selects the output contract for a pipeline run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// Human-readable terminal output with status on stderr.
    Terminal,
    /// Newline-delimited JSON execution events on stdout.
    Json,
    /// GitHub Actions log groups with status on stdout.
    GithubActions,
    /// Stream task output as it arrives while keeping status on stderr.
    Live,
}

/// Owns task presentation so worker threads never write directly to the
/// process-global stdout or stderr handles.
#[derive(Debug)]
pub(crate) struct OutputSink {
    mode: OutputMode,
    lock: Mutex<()>,
    run_id: u64,
    sequence: AtomicU64,
    #[cfg(test)]
    json_lines: Option<std::sync::Arc<std::sync::Mutex<Vec<String>>>>,
}

impl OutputSink {
    pub(crate) fn new(mode: OutputMode) -> Self {
        Self {
            mode,
            lock: Mutex::new(()),
            run_id: run_identity(),
            sequence: AtomicU64::new(0),
            #[cfg(test)]
            json_lines: None,
        }
    }

    pub(crate) fn present_start(&self, node: &TaskNode) -> io::Result<()> {
        let _guard = self.lock.lock().expect("output lock is not poisoned");
        self.render_event(&ExecutionEvent::task_started(node))
    }

    pub(crate) fn present_success(&self, node: &TaskNode, result: &TaskResult) -> io::Result<()> {
        let _guard = self.lock.lock().expect("output lock is not poisoned");
        if matches!(self.mode, OutputMode::Json | OutputMode::GithubActions) {
            // Emit separate TaskOutput events so the JSON stream and CI log are lossless.
            if !result.output.stdout.is_empty() {
                self.render_event(&ExecutionEvent::task_output(
                    node,
                    TaskStream::Stdout,
                    result.output.stdout.clone(),
                ))?;
            }
            if !result.output.stderr.is_empty() {
                self.render_event(&ExecutionEvent::task_output(
                    node,
                    TaskStream::Stderr,
                    result.output.stderr.clone(),
                ))?;
            }
        } else {
            self.start_section(node)?;
            if self.mode != OutputMode::Live || result.cached {
                write_bytes(&result.output.stdout, false)?;
                write_bytes(&result.output.stderr, true)?;
            }
        }
        let status = if result.cached {
            TaskStatus::Cached
        } else {
            TaskStatus::Completed
        };
        self.render_event(&ExecutionEvent::task_finished(node, status, result.elapsed))
    }

    pub(crate) fn present_blocked(&self, node: &TaskNode) -> io::Result<()> {
        let _guard = self.lock.lock().expect("output lock is not poisoned");
        self.render_event(&ExecutionEvent::task_finished(
            node,
            TaskStatus::Blocked,
            std::time::Duration::ZERO,
        ))
    }

    pub(crate) fn present_failure(&self, node: &TaskNode, error: &RunnerError) -> io::Result<()> {
        let _guard = self.lock.lock().expect("output lock is not poisoned");
        if matches!(self.mode, OutputMode::Json | OutputMode::GithubActions) {
            if let Some(output) = error.output() {
                if !output.stdout.is_empty() {
                    self.render_event(&ExecutionEvent::task_output(
                        node,
                        TaskStream::Stdout,
                        output.stdout.clone(),
                    ))?;
                }
                if !output.stderr.is_empty() {
                    self.render_event(&ExecutionEvent::task_output(
                        node,
                        TaskStream::Stderr,
                        output.stderr.clone(),
                    ))?;
                }
            }
        } else if self.mode != OutputMode::Live {
            self.start_section(node)?;
            if let Some(output) = error.output() {
                write_bytes(&output.stdout, false)?;
                write_bytes(&output.stderr, true)?;
            }
        }
        let status = match error {
            RunnerError::TimedOut(_) => TaskStatus::TimedOut,
            RunnerError::OutputLimit(_) => TaskStatus::OutputLimit,
            RunnerError::Cancelled(_) => TaskStatus::Cancelled,
            _ => TaskStatus::Failed,
        };
        self.render_event(&ExecutionEvent::task_finished(
            node,
            status,
            error.elapsed().unwrap_or_default(),
        ))
    }

    pub(crate) fn is_live(&self) -> bool {
        self.mode == OutputMode::Live
    }

    pub(crate) fn present_attempt(
        &self,
        node: &TaskNode,
        attempt: u32,
        max_attempts: u32,
    ) -> io::Result<()> {
        let _guard = self.lock.lock().expect("output lock is not poisoned");
        self.render_event(&ExecutionEvent::task_attempt_started(
            node,
            attempt,
            max_attempts,
        ))
    }

    pub(crate) fn present_live_output(
        &self,
        node: &TaskNode,
        stream: TaskStream,
        bytes: Vec<u8>,
    ) -> io::Result<()> {
        let _guard = self.lock.lock().expect("output lock is not poisoned");
        self.render_event(&ExecutionEvent::task_output(node, stream, bytes))
    }

    pub(crate) fn present_run_start(&self, project: &Path, task_count: usize) -> io::Result<()> {
        let _guard = self.lock.lock().expect("output lock is not poisoned");
        self.render_event(&ExecutionEvent::run_started(
            project.to_path_buf(),
            task_count,
        ))
    }

    pub(crate) fn present_run_finished(
        &self,
        summary: &crate::scheduler::ExecutionSummary,
    ) -> io::Result<()> {
        let _guard = self.lock.lock().expect("output lock is not poisoned");

        // `run_pipeline_with_mode` returns the human-readable summary to the
        // CLI transport, which writes it once to stdout. JSON must receive the
        // lifecycle event here because the transport deliberately returns no
        // second summary line in that mode. Avoid sending a second terminal or
        // GitHub Actions summary to stderr/stdout from the scheduler.
        if self.mode == OutputMode::Json {
            self.render_event(&ExecutionEvent::run_finished(
                summary.completed,
                summary.cached,
                summary.failed,
                summary.cancelled,
                summary.blocked,
            ))
        } else {
            Ok(())
        }
    }

    fn render_event(&self, event: &ExecutionEvent) -> io::Result<()> {
        match self.mode {
            OutputMode::Json => {
                #[cfg(test)]
                if let Some(ref lines) = self.json_lines {
                    let mut buf = Vec::new();
                    write_json_event_with_identity(
                        event,
                        self.run_id,
                        self.sequence.fetch_add(1, Ordering::Relaxed),
                        &mut buf,
                    )?;
                    let line = String::from_utf8(buf).map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 JSON")
                    })?;
                    lines.lock().expect("json_lines lock").push(line);
                    return Ok(());
                }
                let mut stdout = io::stdout().lock();
                write_json_event_with_identity(
                    event,
                    self.run_id,
                    self.sequence.fetch_add(1, Ordering::Relaxed),
                    &mut stdout,
                )
            }
            OutputMode::Terminal => self.render_terminal(event),
            OutputMode::GithubActions => self.render_github_actions(event),
            OutputMode::Live => self.render_live(event),
        }
    }

    fn render_terminal(&self, event: &ExecutionEvent) -> io::Result<()> {
        let mut stderr = io::stderr().lock();
        match event {
            ExecutionEvent::TaskStarted { task, .. } => {
                writeln!(stderr, "▶ {task}")
            }
            ExecutionEvent::TaskOutput { .. } => {
                let _ = event;
                Ok(())
            }
            ExecutionEvent::TaskAttemptStarted {
                task,
                attempt,
                max_attempts,
                ..
            } => {
                if *attempt == 1 {
                    Ok(())
                } else {
                    writeln!(stderr, "↻ {task}: retry {attempt}/{max_attempts}")
                }
            }
            ExecutionEvent::TaskFinished {
                task,
                status,
                elapsed_ms,
                ..
            } => {
                writeln!(stderr, "{task}: {} in {elapsed_ms}ms", status.label())
            }
            ExecutionEvent::RunFinished {
                completed,
                cached,
                failed,
                cancelled,
                blocked,
                ..
            } => writeln!(
                stderr,
                "summary: {completed} completed, {cached} cached, {failed} failed, {cancelled} cancelled, {blocked} blocked"
            ),
            ExecutionEvent::RunStarted { .. } => Ok(()),
        }
    }

    fn render_live(&self, event: &ExecutionEvent) -> io::Result<()> {
        match event {
            ExecutionEvent::TaskOutput { bytes, stream, .. } => {
                if matches!(stream, TaskStream::Stderr) {
                    let mut handle = io::stderr().lock();
                    handle.write_all(bytes)?;
                    handle.flush()
                } else {
                    let mut handle = io::stdout().lock();
                    handle.write_all(bytes)?;
                    handle.flush()
                }
            }
            _ => self.render_terminal(event),
        }
    }

    fn render_github_actions(&self, event: &ExecutionEvent) -> io::Result<()> {
        match event {
            ExecutionEvent::TaskStarted { task, .. } => {
                let mut stdout = io::stdout().lock();
                writeln!(stdout, "::group::{task}")?;
                writeln!(stdout, "▶ {task}")?;
                stdout.flush()
            }
            ExecutionEvent::TaskOutput { bytes, stream, .. } => {
                if bytes.is_empty() {
                    return Ok(());
                }
                let token = format!(
                    "mono_output_{}_{}",
                    self.run_id,
                    self.sequence.fetch_add(1, Ordering::Relaxed)
                );
                {
                    let mut control = io::stdout().lock();
                    writeln!(control, "::stop-commands::{token}")?;
                    control.flush()?;
                }
                let result = if matches!(stream, TaskStream::Stderr) {
                    let mut handle = io::stderr().lock();
                    write_line_terminated(&mut handle, bytes).and_then(|_| handle.flush())
                } else {
                    let mut handle = io::stdout().lock();
                    write_line_terminated(&mut handle, bytes).and_then(|_| handle.flush())
                };
                let mut control = io::stdout().lock();
                writeln!(control, "::{token}::")?;
                control.flush()?;
                result
            }
            ExecutionEvent::TaskAttemptStarted {
                task,
                attempt,
                max_attempts,
                ..
            } => {
                if *attempt > 1 {
                    let mut stdout = io::stdout().lock();
                    writeln!(stdout, "↻ {task}: retry {attempt}/{max_attempts}")?;
                    stdout.flush()
                } else {
                    Ok(())
                }
            }
            ExecutionEvent::TaskFinished {
                task,
                status,
                elapsed_ms,
                ..
            } => {
                let mut stdout = io::stdout().lock();
                writeln!(stdout, "{task}: {} in {elapsed_ms}ms", status.label())?;
                writeln!(stdout, "::endgroup::")?;
                stdout.flush()
            }
            ExecutionEvent::RunFinished {
                completed,
                cached,
                failed,
                cancelled,
                blocked,
                ..
            } => {
                let mut stdout = io::stdout().lock();
                writeln!(
                    stdout,
                    "summary: {completed} completed, {cached} cached, {failed} failed, {cancelled} cancelled, {blocked} blocked"
                )?;
                stdout.flush()
            }
            ExecutionEvent::RunStarted { .. } => Ok(()),
        }
    }

    fn start_section(&self, _node: &TaskNode) -> io::Result<()> {
        // Group begin/end is now handled by the event renderer
        // (TaskStarted emits ::group::, TaskFinished emits ::endgroup::).
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn present_summary(
        &self,
        summary: &crate::scheduler::ExecutionSummary,
    ) -> io::Result<()> {
        let _guard = self.lock.lock().expect("output lock is not poisoned");
        self.render_event(&ExecutionEvent::run_finished(
            summary.completed,
            summary.cached,
            summary.failed,
            summary.cancelled,
            summary.blocked,
        ))
    }

    #[allow(dead_code)]
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

#[derive(serde::Serialize)]
struct JsonEvent<'a> {
    run_id: u64,
    sequence: u64,
    #[serde(flatten)]
    event: &'a ExecutionEvent,
}

#[cfg(test)]
fn write_json_event(event: &ExecutionEvent, writer: &mut impl Write) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, event).map_err(io::Error::other)?;
    writer.write_all(b"\n")
}

fn write_json_event_with_identity(
    event: &ExecutionEvent,
    run_id: u64,
    sequence: u64,
    writer: &mut impl Write,
) -> io::Result<()> {
    serde_json::to_writer(
        &mut *writer,
        &JsonEvent {
            run_id,
            sequence,
            event,
        },
    )
    .map_err(io::Error::other)?;
    writer.write_all(b"\n")
}

fn run_identity() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is after the epoch")
        .as_nanos();
    (nanos as u64) ^ u64::from(std::process::id())
}

fn write_bytes(bytes: &[u8], to_stderr: bool) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    if to_stderr {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::ExecutionSummary;

    #[test]
    fn json_output_is_one_valid_object_per_line() {
        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = OutputSink {
            mode: OutputMode::Json,
            lock: Mutex::new(()),
            run_id: 1,
            sequence: AtomicU64::new(0),
            json_lines: Some(lines.clone()),
        };

        sink.present_start(&TaskNode::new("build"))
            .expect("start event writes");

        let captured = lines.lock().unwrap();
        assert_eq!(captured.len(), 1);
        let event: serde_json::Value =
            serde_json::from_str(&captured[0]).expect("valid JSON event");
        assert_eq!(event["event"], "task_started");
        assert_eq!(event["schema"], crate::events::EXECUTION_EVENT_SCHEMA);
        assert_eq!(event["task"], "build");
    }

    #[test]
    fn json_run_started_event() {
        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = OutputSink {
            mode: OutputMode::Json,
            lock: Mutex::new(()),
            run_id: 1,
            sequence: AtomicU64::new(0),
            json_lines: Some(lines.clone()),
        };

        sink.present_run_start(Path::new("/workspace"), 3)
            .expect("run start event writes");

        let captured = lines.lock().unwrap();
        assert_eq!(captured.len(), 1);
        let event: serde_json::Value =
            serde_json::from_str(&captured[0]).expect("valid JSON event");
        assert_eq!(event["event"], "run_started");
        assert_eq!(event["task_count"], 3);
    }

    #[test]
    fn json_run_finished_event() {
        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = OutputSink {
            mode: OutputMode::Json,
            lock: Mutex::new(()),
            run_id: 1,
            sequence: AtomicU64::new(0),
            json_lines: Some(lines.clone()),
        };

        let summary = ExecutionSummary {
            completed: 2,
            cached: 1,
            failed: 0,
            cancelled: 0,
            blocked: 0,
        };
        sink.present_run_finished(&summary)
            .expect("run finished event writes");

        let captured = lines.lock().unwrap();
        assert_eq!(captured.len(), 1);
        let event: serde_json::Value =
            serde_json::from_str(&captured[0]).expect("valid JSON event");
        assert_eq!(event["event"], "run_finished");
        assert_eq!(event["completed"], 2);
        assert_eq!(event["cached"], 1);
    }

    #[test]
    fn present_summary_produces_json_event_in_json_mode() {
        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = OutputSink {
            mode: OutputMode::Json,
            lock: Mutex::new(()),
            run_id: 1,
            sequence: AtomicU64::new(0),
            json_lines: Some(lines.clone()),
        };

        let summary = ExecutionSummary {
            completed: 1,
            cached: 0,
            failed: 0,
            cancelled: 0,
            blocked: 0,
        };
        sink.present_summary(&summary)
            .expect("summary event writes");

        let captured = lines.lock().unwrap();
        assert_eq!(captured.len(), 1);
        let event: serde_json::Value =
            serde_json::from_str(&captured[0]).expect("valid JSON event");
        assert_eq!(event["event"], "run_finished");
    }

    #[test]
    fn json_task_attempt_event() {
        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = OutputSink {
            mode: OutputMode::Json,
            lock: Mutex::new(()),
            run_id: 1,
            sequence: AtomicU64::new(0),
            json_lines: Some(lines.clone()),
        };
        let node = TaskNode::new("build");

        sink.present_attempt(&node, 2, 3)
            .expect("attempt event writes");

        let captured = lines.lock().unwrap();
        let event: serde_json::Value = serde_json::from_str(&captured[0]).unwrap();
        assert_eq!(event["event"], "task_attempt_started");
        assert_eq!(event["attempt"], 2);
        assert_eq!(event["max_attempts"], 3);
    }

    #[test]
    fn json_task_output_event() {
        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = OutputSink {
            mode: OutputMode::Json,
            lock: Mutex::new(()),
            run_id: 1,
            sequence: AtomicU64::new(0),
            json_lines: Some(lines.clone()),
        };

        let node = TaskNode::new("build");

        let result = TaskResult {
            output: crate::runner::CapturedOutput {
                stdout: b"hello\n".to_vec(),
                stderr: Vec::new(),
            },
            elapsed: std::time::Duration::from_millis(42),
            cached: false,
        };

        sink.present_success(&node, &result)
            .expect("success event writes");

        let captured = lines.lock().unwrap();
        // TaskOutput (stdout) + TaskFinished
        assert_eq!(captured.len(), 2);

        let event: serde_json::Value =
            serde_json::from_str(&captured[0]).expect("valid JSON event");
        assert_eq!(event["event"], "task_output");
        assert_eq!(event["stream"], "stdout");
        assert_eq!(event["task"], "build");

        let event: serde_json::Value =
            serde_json::from_str(&captured[1]).expect("valid JSON event");
        assert_eq!(event["event"], "task_finished");
        assert_eq!(event["status"], "completed");
        assert_eq!(event["elapsed_ms"], 42);
    }

    #[test]
    fn json_task_output_preserves_non_utf8_bytes() {
        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = OutputSink {
            mode: OutputMode::Json,
            lock: Mutex::new(()),
            run_id: 1,
            sequence: AtomicU64::new(0),
            json_lines: Some(lines.clone()),
        };

        let node = TaskNode::new("build");

        // Bytes that are not valid UTF-8: null byte (0), raw byte 255, newline (10)
        let result = TaskResult {
            output: crate::runner::CapturedOutput {
                stdout: vec![0u8, 255u8, 10u8],
                stderr: Vec::new(),
            },
            elapsed: std::time::Duration::from_millis(42),
            cached: false,
        };

        sink.present_success(&node, &result)
            .expect("success event writes");

        let captured = lines.lock().unwrap();
        assert_eq!(captured.len(), 2, "TaskOutput + TaskFinished");

        // Parse the TaskOutput event and verify the bytes field
        let event: serde_json::Value =
            serde_json::from_str(&captured[0]).expect("valid JSON event");
        assert_eq!(event["event"], "task_output");

        let actual_bytes: Vec<u8> = event["bytes"]
            .as_array()
            .expect("bytes should be a JSON array")
            .iter()
            .map(|v| v.as_u64().expect("each byte is a number") as u8)
            .collect();
        assert_eq!(
            actual_bytes,
            vec![0u8, 255u8, 10u8],
            "non-UTF-8 bytes must round-trip through the JSON event"
        );
    }

    #[test]
    fn write_json_event_with_non_utf8_bytes_through_write_sink() {
        // A Write sink that accumulates partial writes into a byte buffer.
        struct AccumulatingWriter {
            buffer: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
        }

        impl Write for AccumulatingWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                let mut buffer = self.buffer.lock().expect("AccumulatingWriter lock");
                buffer.extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut writer = AccumulatingWriter {
            buffer: buffer.clone(),
        };

        let event = ExecutionEvent::TaskOutput {
            schema: crate::events::EXECUTION_EVENT_SCHEMA,
            task: "build".to_owned(),
            stream: TaskStream::Stdout,
            bytes: vec![0u8, 255u8, 10u8],
        };

        write_json_event(&event, &mut writer).expect("write_json_event succeeds");

        let bytes = buffer.lock().expect("AccumulatingWriter buffer");
        let json_line = String::from_utf8(bytes.clone()).expect("JSON output must be valid UTF-8");
        let parsed: serde_json::Value =
            serde_json::from_str(json_line.trim_end()).expect("parse JSON");
        assert_eq!(parsed["event"], "task_output");
        let actual_bytes: Vec<u8> = parsed["bytes"]
            .as_array()
            .expect("bytes array")
            .iter()
            .map(|v| v.as_u64().expect("byte") as u8)
            .collect();
        assert_eq!(actual_bytes, vec![0u8, 255u8, 10u8]);
        // The buffer must end with a newline.
        assert!(
            bytes.ends_with(b"\n"),
            "JSON event must be newline-terminated"
        );
    }
}
