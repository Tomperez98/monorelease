//! `mono list` — describe one root project's pipelines and tasks.

use std::error::Error as StdError;
use std::fmt;
use std::path::Path;

use crate::output::OutputMode;
use crate::project::{Project, ProjectError};
use crate::runner::format_command;

pub fn list(path: &Path) -> Result<String, ListError> {
    list_with_output(path, OutputMode::Terminal)
}

pub fn list_with_output(path: &Path, output_mode: OutputMode) -> Result<String, ListError> {
    let document = describe(path)?;
    if output_mode == OutputMode::Json {
        return serde_json::to_string(&document).map_err(|source| ListError::Json { source });
    }
    Ok(format_list_document(&document))
}

/// Load the project description into an owned value that can be rendered by
/// multiple transports without retaining the loaded project.
pub(crate) fn describe(path: &Path) -> Result<ListDocument, ListError> {
    let project = Project::load(path)?;
    Ok(ListDocument::from(&project))
}

fn format_list_document(document: &ListDocument) -> String {
    let mut output = format!("project {} ({})", document.project, document.root.display());
    output.push_str("\n\npipelines:\n");
    for pipeline in &document.pipelines {
        output.push_str(&format!(
            "  {}: {}\n",
            pipeline.name,
            pipeline.tasks.join(", ")
        ));
        if !pipeline.finally.is_empty() {
            output.push_str(&format!("    finally: {}\n", pipeline.finally.join(", ")));
        }
    }
    output.push_str("\ntasks:\n");
    for task in &document.tasks {
        output.push_str(&format!(
            "  {}: {}\n",
            task.id,
            format_command(&task.command)
        ));
    }
    output.push_str("\ncommon commands:\n");
    for (command, description) in [
        ("mono", "Run the default pipeline"),
        ("mono run <pipeline>", "Run a named pipeline"),
        ("mono task <task>", "Run one or more tasks"),
        ("mono plan", "Print the dependency-first plan"),
        ("mono graph", "Print dependency edges"),
        ("mono check", "Validate the project"),
    ] {
        output.push_str(&format!("  {command}: {description}\n"));
    }
    output.trim_end().to_owned()
}

#[derive(serde::Serialize)]
pub(crate) struct ListDocument {
    schema: u32,
    kind: &'static str,
    project: String,
    root: std::path::PathBuf,
    default_pipeline: String,
    pipelines: Vec<ListPipeline>,
    tasks: Vec<ListTask>,
}

#[derive(serde::Serialize)]
struct ListPipeline {
    name: String,
    tasks: Vec<String>,
    finally: Vec<String>,
}

#[derive(serde::Serialize)]
struct ListTask {
    id: String,
    command: Vec<String>,
    stdin: &'static str,
}

impl From<&Project> for ListDocument {
    fn from(project: &Project) -> Self {
        Self {
            schema: crate::events::EXECUTION_EVENT_SCHEMA,
            kind: "list",
            project: project.name.clone(),
            root: project.root.clone(),
            default_pipeline: project.default_pipeline.clone(),
            pipelines: project
                .pipelines
                .iter()
                .map(|(name, pipeline)| ListPipeline {
                    name: name.clone(),
                    tasks: pipeline.tasks.clone(),
                    finally: pipeline.finally.clone(),
                })
                .collect(),
            tasks: project
                .tasks
                .iter()
                .map(|(id, task)| ListTask {
                    id: id.clone(),
                    command: task.command.clone(),
                    stdin: task.stdin.as_str(),
                })
                .collect(),
        }
    }
}

#[derive(Debug)]
pub enum ListError {
    Project(ProjectError),
    Json { source: serde_json::Error },
}

impl fmt::Display for ListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Project(error) => error.fmt(f),
            Self::Json { source } => write!(f, "could not serialize list output: {source}"),
        }
    }
}

impl StdError for ListError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Project(error) => Some(error),
            Self::Json { source } => Some(source),
        }
    }
}

impl From<ProjectError> for ListError {
    fn from(error: ProjectError) -> Self {
        Self::Project(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::config_path;
    use crate::testing::TempDir;
    use std::fs;

    fn write_manifest(temp: &TempDir) {
        fs::write(
            config_path(temp.path()),
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\nfinally = [\"cleanup\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n\n[tasks.cleanup]\ncommand = [\"echo\", \"cleanup\"]\nstdin = \"inherit\"\n",
        )
        .expect("write project manifest");
    }

    #[test]
    fn terminal_list_contains_pipeline_finalizer_and_task_command() {
        let temp = TempDir::new();
        write_manifest(&temp);

        let output = list_with_output(temp.path(), OutputMode::Terminal).expect("list succeeds");

        assert!(output.contains("ci: build"));
        assert!(output.contains("finally: cleanup"));
        assert!(output.contains("build: echo build"));
        assert!(output.contains("cleanup: echo cleanup"));
    }

    #[test]
    fn json_list_contains_pipeline_task_and_stdin_contracts() {
        let temp = TempDir::new();
        write_manifest(&temp);

        let output = list_with_output(temp.path(), OutputMode::Json).expect("JSON list succeeds");
        let value: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");

        assert_eq!(value["schema"], crate::events::EXECUTION_EVENT_SCHEMA);
        assert_eq!(value["kind"], "list");
        assert_eq!(value["default_pipeline"], "ci");
        assert_eq!(value["pipelines"][0]["finally"][0], "cleanup");
        assert_eq!(value["tasks"][1]["stdin"], "inherit");
    }
}
