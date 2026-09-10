//! `monore list` — describe the available execution targets.

use std::error::Error as StdError;
use std::fmt;
use std::path::Path;

use crate::runner::format_command;
use crate::workspace::{Workspace, WorkspaceError};

/// Describe the loaded project or monorepo without executing tasks.
pub fn list(path: &Path) -> Result<String, ListError> {
    let workspace = Workspace::load(path)?;
    let mut output = format!(
        "{} {} ({})",
        workspace.scope_label(),
        workspace.name,
        workspace.root.display()
    );
    let pipeline_rows = workspace
        .pipelines
        .iter()
        .map(|(name, pipeline)| (name.clone(), pipeline.tasks.join(", ")))
        .collect::<Vec<_>>();
    output.push_str("\n\nPipelines:");
    output.push_str(&format_rows(&pipeline_rows));

    let mut task_rows = Vec::new();
    for (name, task) in &workspace.workspace_tasks {
        task_rows.push((format!("workspace:{name}"), format_command(&task.command)));
    }
    for package in workspace.packages.values() {
        for (name, task) in &package.tasks {
            task_rows.push((
                format!("{}:{name}", package.name),
                format_command(&task.command),
            ));
        }
    }
    output.push_str("\n\nTasks:");
    output.push_str(&format_rows(&task_rows));

    let example_rows = vec![
        (
            "monorelease".to_owned(),
            "Run the default pipeline".to_owned(),
        ),
        (
            "monorelease task <task>".to_owned(),
            "Run one or more tasks".to_owned(),
        ),
        (
            "monorelease plan".to_owned(),
            "Print the resolved plan".to_owned(),
        ),
    ];
    output.push_str("\n\nExamples:");
    output.push_str(&format_rows(&example_rows));
    Ok(output)
}

fn format_rows(rows: &[(String, String)]) -> String {
    if rows.is_empty() {
        return "\n  (none)".to_owned();
    }

    let label_width = rows
        .iter()
        .map(|(label, _)| label.chars().count())
        .max()
        .expect("non-empty rows have a maximum label width");
    rows.iter()
        .map(|(label, value)| {
            let padding = " ".repeat(label_width - label.chars().count());
            format!("\n  {label}{padding}  {value}")
        })
        .collect()
}

/// Expected failures of [`list`].
#[derive(Debug)]
pub enum ListError {
    Workspace(WorkspaceError),
}

impl fmt::Display for ListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Workspace(error) => error.fmt(f),
        }
    }
}

impl StdError for ListError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Workspace(error) => Some(error),
        }
    }
}

impl From<WorkspaceError> for ListError {
    fn from(error: WorkspaceError) -> Self {
        Self::Workspace(error)
    }
}
