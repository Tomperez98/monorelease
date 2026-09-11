//! Provider-neutral, serializable execution events.
//!
//! This vocabulary is the stable wire contract between the scheduler and every
//! output renderer. The scheduler constructs events through public helpers; the
//! output adapter serialises them without knowing what a "package" or "task"
//! means. No language, framework, or CI-provider knowledge lives here.

use serde::Serialize;
use std::path::PathBuf;
use std::time::Duration;

use crate::workspace::TaskNode;

/// Version of the newline-delimited execution event contract.
pub const EXECUTION_EVENT_SCHEMA: u32 = 1;

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

/// A single execution event that can be rendered as JSON, terminal status, or
/// GitHub Actions group annotations.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ExecutionEvent {
    RunStarted {
        schema: u32,
        workspace: PathBuf,
        task_count: usize,
    },
    TaskStarted {
        schema: u32,
        package: String,
        task: String,
    },
    TaskOutput {
        schema: u32,
        package: String,
        task: String,
        stream: TaskStream,
        bytes: Vec<u8>,
    },
    TaskFinished {
        schema: u32,
        package: String,
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
            package: node.package.clone(),
            task: node.task.clone(),
        }
    }

    pub fn task_output(node: &TaskNode, stream: TaskStream, bytes: Vec<u8>) -> Self {
        Self::TaskOutput {
            schema: EXECUTION_EVENT_SCHEMA,
            package: node.package.clone(),
            task: node.task.clone(),
            stream,
            bytes,
        }
    }

    pub fn task_finished(node: &TaskNode, status: TaskStatus, elapsed: Duration) -> Self {
        Self::TaskFinished {
            schema: EXECUTION_EVENT_SCHEMA,
            package: node.package.clone(),
            task: node.task.clone(),
            status,
            elapsed_ms: elapsed.as_millis(),
        }
    }

    pub fn run_started(workspace: PathBuf, task_count: usize) -> Self {
        Self::RunStarted {
            schema: EXECUTION_EVENT_SCHEMA,
            workspace,
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

#[allow(dead_code)]
pub(crate) fn elapsed_millis(duration: Duration) -> u128 {
    duration.as_millis()
}
