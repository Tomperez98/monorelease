//! Provider-neutral, serializable execution events.

use serde::Serialize;
use std::path::PathBuf;
use std::time::Duration;

use crate::project::TaskNode;

/// Version of the newline-delimited execution event contract.
pub const EXECUTION_EVENT_SCHEMA: u32 = crate::JSON_OUTPUT_SCHEMA;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Completed,
    Cached,
    Failed,
    TimedOut,
    OutputLimit,
    Cancelled,
    Blocked,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ExecutionEvent {
    RunStarted {
        schema: u32,
        project: PathBuf,
        task_count: usize,
    },
    TaskStarted {
        schema: u32,
        task: String,
    },
    TaskOutput {
        schema: u32,
        task: String,
        stream: TaskStream,
        bytes: Vec<u8>,
    },
    TaskAttemptStarted {
        schema: u32,
        task: String,
        attempt: u32,
        max_attempts: u32,
    },
    TaskFinished {
        schema: u32,
        task: String,
        status: TaskStatus,
        elapsed_ms: u128,
    },
    RunFinished {
        schema: u32,
        completed: usize,
        cached: usize,
        failed: usize,
        cancelled: usize,
        blocked: usize,
    },
}

impl ExecutionEvent {
    pub fn task_started(node: &TaskNode) -> Self {
        Self::TaskStarted {
            schema: EXECUTION_EVENT_SCHEMA,
            task: node.id.clone(),
        }
    }

    pub fn task_output(node: &TaskNode, stream: TaskStream, bytes: Vec<u8>) -> Self {
        Self::TaskOutput {
            schema: EXECUTION_EVENT_SCHEMA,
            task: node.id.clone(),
            stream,
            bytes,
        }
    }

    pub fn task_attempt_started(node: &TaskNode, attempt: u32, max_attempts: u32) -> Self {
        Self::TaskAttemptStarted {
            schema: EXECUTION_EVENT_SCHEMA,
            task: node.id.clone(),
            attempt,
            max_attempts,
        }
    }

    pub fn task_finished(node: &TaskNode, status: TaskStatus, elapsed: Duration) -> Self {
        Self::TaskFinished {
            schema: EXECUTION_EVENT_SCHEMA,
            task: node.id.clone(),
            status,
            elapsed_ms: elapsed.as_millis(),
        }
    }

    pub fn run_started(project: PathBuf, task_count: usize) -> Self {
        Self::RunStarted {
            schema: EXECUTION_EVENT_SCHEMA,
            project,
            task_count,
        }
    }

    pub fn run_finished(
        completed: usize,
        cached: usize,
        failed: usize,
        cancelled: usize,
        blocked: usize,
    ) -> Self {
        Self::RunFinished {
            schema: EXECUTION_EVENT_SCHEMA,
            completed,
            cached,
            failed,
            cancelled,
            blocked,
        }
    }
}

impl TaskStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Cached => "cache hit",
            Self::Failed => "failed",
            Self::TimedOut => "timed out",
            Self::OutputLimit => "output limit exceeded",
            Self::Cancelled => "cancelled",
            Self::Blocked => "blocked",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These strings are the machine contract a CI consumer reads. Renaming a
    /// variant must break this test, not silently change the JSON stream.
    #[test]
    fn task_status_serializes_to_its_documented_token() {
        for (status, token) in [
            (TaskStatus::Completed, "completed"),
            (TaskStatus::Cached, "cached"),
            (TaskStatus::Failed, "failed"),
            (TaskStatus::TimedOut, "timed_out"),
            (TaskStatus::OutputLimit, "output_limit"),
            (TaskStatus::Cancelled, "cancelled"),
            (TaskStatus::Blocked, "blocked"),
        ] {
            assert_eq!(
                serde_json::to_value(status).expect("status serializes"),
                serde_json::json!(token)
            );
        }
    }

    #[test]
    fn task_stream_serializes_to_its_documented_token() {
        assert_eq!(
            serde_json::to_value(TaskStream::Stdout).unwrap(),
            serde_json::json!("stdout")
        );
        assert_eq!(
            serde_json::to_value(TaskStream::Stderr).unwrap(),
            serde_json::json!("stderr")
        );
    }

    #[test]
    fn every_event_carries_its_name_tag_and_schema() {
        let node = TaskNode::new("build");
        let events = [
            (
                ExecutionEvent::run_started(PathBuf::from("/workspace"), 2),
                "run_started",
            ),
            (ExecutionEvent::task_started(&node), "task_started"),
            (
                ExecutionEvent::task_output(&node, TaskStream::Stdout, b"hi".to_vec()),
                "task_output",
            ),
            (
                ExecutionEvent::task_attempt_started(&node, 2, 3),
                "task_attempt_started",
            ),
            (
                ExecutionEvent::task_finished(&node, TaskStatus::Completed, Duration::ZERO),
                "task_finished",
            ),
            (ExecutionEvent::run_finished(1, 0, 0, 0, 0), "run_finished"),
        ];

        for (event, name) in events {
            let value = serde_json::to_value(&event).expect("event serializes");
            assert_eq!(value["event"], name, "{value}");
            assert_eq!(value["schema"], EXECUTION_EVENT_SCHEMA, "{value}");
        }
    }

    #[test]
    fn status_labels_are_distinct_and_non_empty() {
        let labels = [
            TaskStatus::Completed,
            TaskStatus::Cached,
            TaskStatus::Failed,
            TaskStatus::TimedOut,
            TaskStatus::OutputLimit,
            TaskStatus::Cancelled,
            TaskStatus::Blocked,
        ]
        .map(TaskStatus::label);

        for label in labels {
            assert!(!label.is_empty());
        }
        let mut sorted = labels.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len(), "two statuses share a label");
    }
}
