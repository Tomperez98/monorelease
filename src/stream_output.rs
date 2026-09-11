//! Line framing for concurrent human-readable task output.
//!
//! A task's output can arrive in arbitrary chunks, and independent tasks can
//! produce those chunks concurrently. `StreamFormatter` keeps one partial-line
//! buffer per task and stream so every complete line receives the task prefix
//! that identifies it.

use std::collections::HashMap;

use crate::events::TaskStream;

#[derive(Debug, Default)]
pub(crate) struct StreamFormatter {
    tasks: HashMap<String, TaskBuffers>,
}

#[derive(Debug, Default)]
struct TaskBuffers {
    stdout: StreamBuffer,
    stderr: StreamBuffer,
}

#[derive(Debug, Default)]
struct StreamBuffer {
    partial: Vec<u8>,
}

impl StreamFormatter {
    /// Add a chunk and return all newly completed, prefixed lines.
    pub(crate) fn push(&mut self, task: &str, stream: TaskStream, bytes: &[u8]) -> Vec<u8> {
        if bytes.is_empty() {
            return Vec::new();
        }

        let buffers = self.tasks.entry(task.to_owned()).or_default();
        let buffer = match stream {
            TaskStream::Stdout => &mut buffers.stdout,
            TaskStream::Stderr => &mut buffers.stderr,
        };
        buffer.partial.extend_from_slice(bytes);

        let mut framed = Vec::new();
        while let Some(newline) = buffer.partial.iter().position(|byte| *byte == b'\n') {
            let line = buffer.partial.drain(..=newline).collect::<Vec<_>>();
            append_prefixed(&mut framed, task, &line);
        }
        framed
    }

    /// Flush unterminated lines for a task and discard its framing state.
    pub(crate) fn finish(&mut self, task: &str) -> Vec<(TaskStream, Vec<u8>)> {
        let Some(mut buffers) = self.tasks.remove(task) else {
            return Vec::new();
        };

        let mut flushed = Vec::new();
        if !buffers.stdout.partial.is_empty() {
            let partial = std::mem::take(&mut buffers.stdout.partial);
            let mut framed = Vec::new();
            append_prefixed(&mut framed, task, &partial);
            framed.push(b'\n');
            flushed.push((TaskStream::Stdout, framed));
        }
        if !buffers.stderr.partial.is_empty() {
            let partial = std::mem::take(&mut buffers.stderr.partial);
            let mut framed = Vec::new();
            append_prefixed(&mut framed, task, &partial);
            framed.push(b'\n');
            flushed.push((TaskStream::Stderr, framed));
        }
        flushed
    }
}

fn append_prefixed(output: &mut Vec<u8>, task: &str, line: &[u8]) {
    output.extend_from_slice(b"[");
    output.extend_from_slice(task.as_bytes());
    output.extend_from_slice(b"] ");
    output.extend_from_slice(line);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_complete_lines() {
        let mut formatter = StreamFormatter::default();

        assert_eq!(
            formatter.push("api", TaskStream::Stdout, b"one\ntwo\n"),
            b"[api] one\n[api] two\n"
        );
    }

    #[test]
    fn holds_partial_chunks_until_a_newline_arrives() {
        let mut formatter = StreamFormatter::default();

        assert!(formatter.push("api", TaskStream::Stdout, b"one").is_empty());
        assert_eq!(
            formatter.push("api", TaskStream::Stdout, b"\ntwo\n"),
            b"[api] one\n[api] two\n"
        );
    }

    #[test]
    fn keeps_task_and_stream_buffers_independent() {
        let mut formatter = StreamFormatter::default();

        assert!(formatter.push("api", TaskStream::Stdout, b"api").is_empty());
        assert_eq!(
            formatter.push("web", TaskStream::Stdout, b"web\n"),
            b"[web] web\n"
        );
        assert_eq!(
            formatter.push("api", TaskStream::Stderr, b"warning\n"),
            b"[api] warning\n"
        );
        assert_eq!(
            formatter.finish("api"),
            vec![(TaskStream::Stdout, b"[api] api\n".to_vec())]
        );
    }

    #[test]
    fn finishing_an_empty_or_already_terminated_task_is_noop() {
        let mut formatter = StreamFormatter::default();

        assert!(formatter.finish("missing").is_empty());
        assert_eq!(
            formatter.push("api", TaskStream::Stdout, b"done\n"),
            b"[api] done\n"
        );
        assert!(formatter.finish("api").is_empty());
    }
}
