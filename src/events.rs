//! Provider-neutral, serializable execution events.

use serde::Serialize;
use std::path::PathBuf;
use std::time::Duration;

use crate::workspace::TaskNode;

/// Version of the newline-delimited execution event contract.
pub const EXECUTION_EVENT_SCHEMA: u32 = crate::JSON_OUTPUT_SCHEMA;

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Completed,
    Cached,
    Failed,
    TimedOut,
    OutputLimit,
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

    pub fn run_finished(completed: usize, cached: usize, failed: usize, blocked: usize) -> Self {
        Self::RunFinished {
            schema: EXECUTION_EVENT_SCHEMA,
            completed,
            cached,
            failed,
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
            Self::Blocked => "blocked",
        }
    }
}
