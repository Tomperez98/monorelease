//! Task output presentation.
//!
//! Each output mode renders [`ExecutionEvent`] values through its own adapter,
//! so the scheduler never needs to know about terminal prefixes, JSON streams,
//! or interactive panes.

use std::io::{self, Write};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::events::{ExecutionEvent, TaskStatus, TaskStream};
use crate::project::TaskNode;
use crate::runner::{CancellationToken, RunnerError, TaskResult};
use crate::scheduler::TaskReporter;
use crate::stream_output::StreamFormatter;
use crate::tui::TuiController;

/// Selects the output contract for a pipeline run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// Human-readable buffered text output with status on stderr.
    Terminal,
    /// Newline-delimited JSON execution events on stdout.
    Json,
    /// Stream task output as it arrives with task prefixes.
    Stream,
    /// Interactive task list and per-task output panes.
    Tui,
}

/// Owns task presentation so worker threads never write directly to the
/// process-global stdout or stderr handles.
///
/// The two write handles are fields rather than process globals so every
/// output mode is renderable into a captured buffer.
pub(crate) struct OutputSink {
    mode: OutputMode,
    writers: Mutex<Writers>,
    tui: Option<TuiController>,
    tui_failures: Mutex<Vec<TuiFailure>>,
    run_id: u64,
    sequence: AtomicU64,
}

struct TuiFailure {
    task: String,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

struct Writers {
    out: Box<dyn Write + Send>,
    err: Box<dyn Write + Send>,
    stream: StreamFormatter,
}

impl TaskReporter for OutputSink {
    fn present_start(&self, node: &TaskNode) -> io::Result<()> {
        self.present_start(node)
    }

    fn present_success(&self, node: &TaskNode, result: &TaskResult) -> io::Result<()> {
        self.present_success(node, result)
    }

    fn present_blocked(&self, node: &TaskNode) -> io::Result<()> {
        self.present_blocked(node)
    }

    fn present_failure(&self, node: &TaskNode, error: &RunnerError) -> io::Result<()> {
        self.present_failure(node, error)
    }

    fn is_live(&self) -> bool {
        self.is_live()
    }

    fn present_attempt(&self, node: &TaskNode, attempt: u32, max_attempts: u32) -> io::Result<()> {
        self.present_attempt(node, attempt, max_attempts)
    }

    fn present_live_output(
        &self,
        node: &TaskNode,
        stream: TaskStream,
        bytes: Vec<u8>,
    ) -> io::Result<()> {
        self.present_live_output(node, stream, bytes)
    }

    fn present_run_start(&self, project: &Path, task_count: usize) -> io::Result<()> {
        self.present_run_start(project, task_count)
    }

    fn present_run_finished(&self, summary: &crate::scheduler::ExecutionSummary) -> io::Result<()> {
        self.present_run_finished(summary)
    }
}

impl std::fmt::Debug for OutputSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OutputSink")
            .field("mode", &self.mode)
            .field("run_id", &self.run_id)
            .field("tui", &self.tui.is_some())
            .finish_non_exhaustive()
    }
}

impl OutputSink {
    /// Render to the real process handles.
    pub(crate) fn new(mode: OutputMode, cancellation: CancellationToken) -> io::Result<Self> {
        let tui = (mode == OutputMode::Tui)
            .then(|| TuiController::start(cancellation))
            .transpose()?;
        Ok(Self::with_writers_and_tui(
            mode,
            tui,
            Box::new(io::stdout()),
            Box::new(io::stderr()),
        ))
    }

    fn with_writers_and_tui(
        mode: OutputMode,
        tui: Option<TuiController>,
        out: Box<dyn Write + Send>,
        err: Box<dyn Write + Send>,
    ) -> Self {
        Self {
            mode,
            writers: Mutex::new(Writers {
                out,
                err,
                stream: StreamFormatter::default(),
            }),
            tui,
            tui_failures: Mutex::new(Vec::new()),
            run_id: run_identity(),
            sequence: AtomicU64::new(0),
        }
    }

    /// A sink with a pinned run identity, so CI output is deterministic.
    #[cfg(test)]
    pub(crate) fn test_sink(
        mode: OutputMode,
        run_id: u64,
        out: Box<dyn Write + Send>,
        err: Box<dyn Write + Send>,
    ) -> Self {
        let mut sink = Self::with_writers_and_tui(mode, None, out, err);
        sink.run_id = run_id;
        sink
    }

    pub(crate) fn present_start(&self, node: &TaskNode) -> io::Result<()> {
        let mut writers = self.writers.lock().expect("output lock is not poisoned");
        self.render_event(&mut writers, &ExecutionEvent::task_started(node))
    }

    pub(crate) fn present_success(&self, node: &TaskNode, result: &TaskResult) -> io::Result<()> {
        let mut writers = self.writers.lock().expect("output lock is not poisoned");
        if self.mode == OutputMode::Json {
            // Emit separate TaskOutput events so the JSON stream is lossless.
            if !result.output.stdout.is_empty() {
                self.render_event(
                    &mut writers,
                    &ExecutionEvent::task_output(
                        node,
                        TaskStream::Stdout,
                        result.output.stdout.clone(),
                    ),
                )?;
            }
            if !result.output.stderr.is_empty() {
                self.render_event(
                    &mut writers,
                    &ExecutionEvent::task_output(
                        node,
                        TaskStream::Stderr,
                        result.output.stderr.clone(),
                    ),
                )?;
            }
        } else if self.mode == OutputMode::Stream {
            if result.cached {
                if !result.output.stdout.is_empty() {
                    self.render_event(
                        &mut writers,
                        &ExecutionEvent::task_output(
                            node,
                            TaskStream::Stdout,
                            result.output.stdout.clone(),
                        ),
                    )?;
                }
                if !result.output.stderr.is_empty() {
                    self.render_event(
                        &mut writers,
                        &ExecutionEvent::task_output(
                            node,
                            TaskStream::Stderr,
                            result.output.stderr.clone(),
                        ),
                    )?;
                }
            }
        } else if self.mode != OutputMode::Tui {
            write_bytes(&mut writers, &result.output.stdout, false)?;
            write_bytes(&mut writers, &result.output.stderr, true)?;
        }
        let status = if result.cached {
            TaskStatus::Cached
        } else {
            TaskStatus::Completed
        };
        self.render_event(
            &mut writers,
            &ExecutionEvent::task_finished(node, status, result.elapsed),
        )
    }

    pub(crate) fn present_blocked(&self, node: &TaskNode) -> io::Result<()> {
        let mut writers = self.writers.lock().expect("output lock is not poisoned");
        self.render_event(
            &mut writers,
            &ExecutionEvent::task_finished(node, TaskStatus::Blocked, std::time::Duration::ZERO),
        )
    }

    pub(crate) fn present_failure(&self, node: &TaskNode, error: &RunnerError) -> io::Result<()> {
        let mut writers = self.writers.lock().expect("output lock is not poisoned");
        if self.mode == OutputMode::Json {
            if let Some(output) = error.output() {
                if !output.stdout.is_empty() {
                    self.render_event(
                        &mut writers,
                        &ExecutionEvent::task_output(
                            node,
                            TaskStream::Stdout,
                            output.stdout.clone(),
                        ),
                    )?;
                }
                if !output.stderr.is_empty() {
                    self.render_event(
                        &mut writers,
                        &ExecutionEvent::task_output(
                            node,
                            TaskStream::Stderr,
                            output.stderr.clone(),
                        ),
                    )?;
                }
            }
        } else if self.mode == OutputMode::Tui {
            if let Some(output) = error.output()
                && (!output.stdout.is_empty() || !output.stderr.is_empty())
            {
                self.tui_failures
                    .lock()
                    .expect("TUI failure lock is not poisoned")
                    .push(TuiFailure {
                        task: node.id().to_owned(),
                        stdout: output.stdout.clone(),
                        stderr: output.stderr.clone(),
                    });
            }
        } else if self.mode != OutputMode::Stream
            && let Some(output) = error.output()
        {
            write_bytes(&mut writers, &output.stdout, false)?;
            write_bytes(&mut writers, &output.stderr, true)?;
        }
        let status = error.status();
        self.render_event(
            &mut writers,
            &ExecutionEvent::task_finished(node, status, error.elapsed().unwrap_or_default()),
        )
    }

    pub(crate) fn is_live(&self) -> bool {
        matches!(self.mode, OutputMode::Stream | OutputMode::Tui)
    }

    pub(crate) fn present_attempt(
        &self,
        node: &TaskNode,
        attempt: u32,
        max_attempts: u32,
    ) -> io::Result<()> {
        let mut writers = self.writers.lock().expect("output lock is not poisoned");
        self.render_event(
            &mut writers,
            &ExecutionEvent::task_attempt_started(node, attempt, max_attempts),
        )
    }

    pub(crate) fn present_live_output(
        &self,
        node: &TaskNode,
        stream: TaskStream,
        bytes: Vec<u8>,
    ) -> io::Result<()> {
        let mut writers = self.writers.lock().expect("output lock is not poisoned");
        self.render_event(
            &mut writers,
            &ExecutionEvent::task_output(node, stream, bytes),
        )
    }

    pub(crate) fn present_run_start(&self, project: &Path, task_count: usize) -> io::Result<()> {
        let mut writers = self.writers.lock().expect("output lock is not poisoned");
        self.render_event(
            &mut writers,
            &ExecutionEvent::run_started(project.to_path_buf(), task_count),
        )
    }

    pub(crate) fn present_run_finished(
        &self,
        summary: &crate::scheduler::ExecutionSummary,
    ) -> io::Result<()> {
        let mut writers = self.writers.lock().expect("output lock is not poisoned");

        // `run_pipeline_with_mode` returns the human-readable summary to the
        // CLI transport, which writes it once to stdout; it returns `None` in
        // JSON mode, where the lifecycle event below is the only summary. Avoid
        // sending a second text summary to stderr/stdout from the scheduler.
        if self.mode == OutputMode::Json {
            self.render_event(
                &mut writers,
                &ExecutionEvent::run_finished(
                    summary.completed,
                    summary.cached,
                    summary.failed,
                    summary.cancelled,
                    summary.blocked,
                ),
            )
        } else if self.mode == OutputMode::Tui {
            let result = self
                .tui
                .as_ref()
                .expect("TUI output has a controller")
                .finish();
            if result.is_ok() {
                self.write_tui_failures(&mut writers)?;
            }
            result
        } else {
            Ok(())
        }
    }

    fn write_tui_failures(&self, writers: &mut Writers) -> io::Result<()> {
        let failures = std::mem::take(
            &mut *self
                .tui_failures
                .lock()
                .expect("TUI failure lock is not poisoned"),
        );
        for failure in failures {
            writeln!(&mut *writers.err, "\n{} captured output:", failure.task)?;
            if !failure.stdout.is_empty() {
                writeln!(&mut *writers.err, "stdout:")?;
                write_bytes(writers, &failure.stdout, true)?;
            }
            if !failure.stderr.is_empty() {
                writeln!(&mut *writers.err, "stderr:")?;
                write_bytes(writers, &failure.stderr, true)?;
            }
        }
        Ok(())
    }

    fn render_event(&self, writers: &mut Writers, event: &ExecutionEvent) -> io::Result<()> {
        match self.mode {
            OutputMode::Json => write_json_event_with_identity(
                event,
                self.run_id,
                self.sequence.fetch_add(1, Ordering::Relaxed),
                &mut *writers.out,
            ),
            OutputMode::Terminal => self.render_terminal(writers, event),
            OutputMode::Stream => self.render_stream(writers, event),
            OutputMode::Tui => self.render_tui(event),
        }
    }

    fn render_terminal(&self, writers: &mut Writers, event: &ExecutionEvent) -> io::Result<()> {
        match event {
            ExecutionEvent::TaskStarted { task, .. } => {
                writeln!(&mut *writers.err, "▶ {task}")
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
                    writeln!(
                        &mut *writers.err,
                        "↻ {task}: retry {attempt}/{max_attempts}"
                    )
                }
            }
            ExecutionEvent::TaskFinished {
                task,
                status,
                elapsed_ms,
                ..
            } => {
                writeln!(
                    &mut *writers.err,
                    "{task}: {} in {elapsed_ms}ms",
                    status.label()
                )
            }
            // `present_run_finished` emits this event only for JSON. The CLI
            // transport prints the summary for every other mode, so rendering
            // it here would either duplicate the line or drift from
            // `commands::ci::format_summary`.
            ExecutionEvent::RunFinished { .. } => Ok(()),
            ExecutionEvent::RunStarted { .. } => Ok(()),
        }
    }

    fn render_stream(&self, writers: &mut Writers, event: &ExecutionEvent) -> io::Result<()> {
        match event {
            ExecutionEvent::TaskOutput {
                task,
                bytes,
                stream,
                ..
            } => {
                let framed = writers.stream.push(task, *stream, bytes);
                write_stream_bytes(writers, *stream, &framed)
            }
            ExecutionEvent::TaskFinished {
                task,
                status,
                elapsed_ms,
                ..
            } => {
                for (stream, bytes) in writers.stream.finish(task) {
                    write_stream_bytes(writers, stream, &bytes)?;
                }
                writeln!(
                    &mut *writers.err,
                    "└─ {task}: {} in {elapsed_ms}ms",
                    status.label()
                )
            }
            ExecutionEvent::TaskAttemptStarted {
                task,
                attempt,
                max_attempts,
                ..
            } if *attempt > 1 => {
                for (stream, bytes) in writers.stream.finish(task) {
                    write_stream_bytes(writers, stream, &bytes)?;
                }
                writeln!(
                    &mut *writers.err,
                    "↻ {task}: retry {attempt}/{max_attempts}"
                )
            }
            _ => self.render_terminal(writers, event),
        }
    }

    fn render_tui(&self, event: &ExecutionEvent) -> io::Result<()> {
        self.tui
            .as_ref()
            .expect("TUI output has a controller")
            .send(event.clone())
    }
}

#[derive(serde::Serialize)]
struct JsonEvent<'a> {
    run_id: u64,
    sequence: u64,
    #[serde(flatten)]
    event: &'a ExecutionEvent,
}

fn write_json_event_with_identity(
    event: &ExecutionEvent,
    run_id: u64,
    sequence: u64,
    writer: &mut (impl Write + ?Sized),
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

fn write_bytes(writers: &mut Writers, bytes: &[u8], to_stderr: bool) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    let handle: &mut dyn Write = if to_stderr {
        &mut *writers.err
    } else {
        &mut *writers.out
    };
    write_line_terminated(handle, bytes)?;
    handle.flush()
}

fn write_stream_bytes(writers: &mut Writers, stream: TaskStream, bytes: &[u8]) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    let handle: &mut dyn Write = if matches!(stream, TaskStream::Stderr) {
        &mut *writers.err
    } else {
        &mut *writers.out
    };
    handle.write_all(bytes)?;
    handle.flush()
}

/// Keep task metadata on its own line when a command omits its final newline.
fn write_line_terminated(handle: &mut (impl Write + ?Sized), bytes: &[u8]) -> io::Result<()> {
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
    use std::sync::{Arc, Mutex};

    /// A `Write` handle over a buffer the test can read back.
    #[derive(Clone)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("shared writer lock")
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    type Captured = (OutputSink, Arc<Mutex<Vec<u8>>>, Arc<Mutex<Vec<u8>>>);

    /// A sink with a pinned run identity, so CI output is byte-for-byte assertable.
    fn captured(mode: OutputMode, run_id: u64) -> Captured {
        let out = Arc::new(Mutex::new(Vec::new()));
        let err = Arc::new(Mutex::new(Vec::new()));
        let sink = OutputSink::test_sink(
            mode,
            run_id,
            Box::new(SharedWriter(Arc::clone(&out))),
            Box::new(SharedWriter(Arc::clone(&err))),
        );
        (sink, out, err)
    }

    fn text(buffer: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8(buffer.lock().expect("buffer lock").clone())
            .expect("rendered output is UTF-8")
    }

    fn result(stdout: &[u8], cached: bool) -> TaskResult {
        TaskResult {
            output: crate::runner::CapturedOutput {
                stdout: stdout.to_vec(),
                stderr: Vec::new(),
            },
            elapsed: std::time::Duration::from_millis(42),
            cached,
        }
    }

    #[test]
    fn terminal_renders_a_golden_transcript() {
        let (sink, out, err) = captured(OutputMode::Terminal, 1);
        let node = TaskNode::new("build");

        sink.present_start(&node).expect("start renders");
        sink.present_attempt(&node, 2, 3).expect("attempt renders");
        sink.present_success(&node, &result(b"hello\n", false))
            .expect("success renders");

        assert_eq!(text(&out), "hello\n");
        assert_eq!(
            text(&err),
            "\u{25b6} build\n\u{21bb} build: retry 2/3\nbuild: completed in 42ms\n"
        );
    }

    #[test]
    fn terminal_marks_a_cache_hit() {
        let (sink, out, err) = captured(OutputMode::Terminal, 1);
        let node = TaskNode::new("build");

        sink.present_success(&node, &result(b"cached\n", true))
            .expect("success renders");

        assert_eq!(text(&out), "cached\n");
        assert_eq!(text(&err), "build: cache hit in 42ms\n");
    }

    #[test]
    fn live_streams_bytes_once_and_keeps_status_on_stderr() {
        let (sink, out, err) = captured(OutputMode::Stream, 1);
        let node = TaskNode::new("build");

        sink.present_start(&node).expect("start renders");
        sink.present_live_output(&node, TaskStream::Stdout, b"streamed".to_vec())
            .expect("live output renders");
        sink.present_success(&node, &result(b"streamed", false))
            .expect("success does not repeat live bytes");

        assert_eq!(text(&out), "[build] streamed\n");
        assert_eq!(text(&err), "\u{25b6} build\n└─ build: completed in 42ms\n");
    }

    #[test]
    fn live_frames_interleaved_tasks_and_flushes_each_partial_line() {
        let (sink, out, err) = captured(OutputMode::Stream, 1);
        let api = TaskNode::new("api");
        let web = TaskNode::new("web");

        sink.present_live_output(&api, TaskStream::Stdout, b"api ".to_vec())
            .expect("api output renders");
        sink.present_live_output(&web, TaskStream::Stdout, b"web\n".to_vec())
            .expect("web output renders");
        sink.present_live_output(&api, TaskStream::Stdout, b"done".to_vec())
            .expect("api completion output renders");
        sink.present_success(&api, &result(b"api done", false))
            .expect("api success renders");

        assert_eq!(text(&out), "[web] web\n[api] api done\n");
        assert!(text(&err).contains("└─ api: completed"), "{}", text(&err));
    }

    #[test]
    fn json_writes_one_identified_object_per_line_through_the_production_path() {
        let (sink, out, err) = captured(OutputMode::Json, 7);
        let node = TaskNode::new("build");

        sink.present_run_start(Path::new("/workspace"), 1)
            .expect("run start renders");
        sink.present_start(&node).expect("start renders");
        sink.present_success(&node, &result(b"hello\n", false))
            .expect("success renders");

        assert!(text(&err).is_empty(), "JSON writes nothing to stderr");
        let lines = text(&out);
        let events = lines
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("valid JSON"))
            .collect::<Vec<_>>();

        // run_started, task_started, task_output, task_finished
        assert_eq!(events.len(), 4);
        assert_eq!(events[0]["event"], "run_started");
        assert_eq!(events[1]["event"], "task_started");
        assert_eq!(events[2]["event"], "task_output");
        assert_eq!(events[3]["event"], "task_finished");
        assert!(events.iter().all(|event| event["run_id"] == 7));
        assert_eq!(
            events
                .iter()
                .map(|event| event["sequence"].as_u64().expect("sequence"))
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
    }

    #[test]
    fn json_run_finished_carries_the_summary_counts() {
        let (sink, out, _err) = captured(OutputMode::Json, 7);
        let summary = ExecutionSummary {
            completed: 2,
            cached: 1,
            failed: 0,
            cancelled: 0,
            blocked: 0,
        };

        sink.present_run_finished(&summary)
            .expect("summary renders");

        let events = text(&out)
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("valid JSON"))
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event"], "run_finished");
        assert_eq!(events[0]["completed"], 2);
        assert_eq!(events[0]["cached"], 1);
    }

    #[test]
    fn json_preserves_non_utf8_task_output_as_bytes() {
        let (sink, out, _err) = captured(OutputMode::Json, 7);
        let node = TaskNode::new("build");

        sink.present_success(&node, &result(&[0u8, 255u8, 10u8], false))
            .expect("success renders");

        let first = text(&out);
        let event: serde_json::Value =
            serde_json::from_str(first.lines().next().expect("a first event line"))
                .expect("valid JSON");
        let bytes = event["bytes"]
            .as_array()
            .expect("bytes array")
            .iter()
            .map(|value| value.as_u64().expect("byte") as u8)
            .collect::<Vec<_>>();

        assert_eq!(bytes, vec![0u8, 255u8, 10u8]);
    }

    #[test]
    fn every_mode_renders_a_blocked_task() {
        let node = TaskNode::new("build");

        for mode in [OutputMode::Terminal, OutputMode::Json, OutputMode::Stream] {
            let (sink, out, err) = captured(mode, 1);
            sink.present_blocked(&node).expect("blocked renders");
            let rendered = format!("{}{}", text(&out), text(&err));
            assert!(rendered.contains("blocked"), "{mode:?}: {rendered}");
        }
    }

    #[test]
    fn run_finished_renders_only_in_json_mode() {
        let summary = ExecutionSummary {
            completed: 1,
            cached: 0,
            failed: 0,
            cancelled: 0,
            blocked: 0,
        };

        for mode in [OutputMode::Terminal, OutputMode::Stream] {
            let (sink, out, err) = captured(mode, 1);
            sink.present_run_finished(&summary)
                .expect("summary renders");
            assert!(
                text(&out).is_empty() && text(&err).is_empty(),
                "{mode:?} must leave the summary to the CLI transport, not render it: {}{}",
                text(&out),
                text(&err)
            );
        }

        let (sink, out, _err) = captured(OutputMode::Json, 1);
        sink.present_run_finished(&summary)
            .expect("summary renders");
        let rendered = text(&out);
        assert!(
            rendered.contains("\"event\":\"run_finished\""),
            "{rendered}"
        );
    }
}
