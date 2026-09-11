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
    let project = Project::load(path)?;
    if output_mode == OutputMode::Json {
        return serde_json::to_string(&ListDocument::from(&project))
            .map_err(|source| ListError::Json { source });
    }

    let mut output = format!("project {} ({})", project.name, project.root.display());
    output.push_str("\n\npipelines:\n");
    for (name, pipeline) in &project.pipelines {
        output.push_str(&format!("  {name}: {}\n", pipeline.tasks.join(", ")));
        if !pipeline.finally.is_empty() {
            output.push_str(&format!("    finally: {}\n", pipeline.finally.join(", ")));
        }
    }
    output.push_str("\ntasks:\n");
    for (name, task) in &project.tasks {
        output.push_str(&format!("  {name}: {}\n", format_command(&task.command)));
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
    Ok(output.trim_end().to_owned())
}

#[derive(serde::Serialize)]
struct ListDocument<'a> {
    schema: u32,
    kind: &'static str,
    project: &'a str,
    root: &'a Path,
    default_pipeline: &'a str,
    pipelines: Vec<ListPipeline<'a>>,
    tasks: Vec<ListTask<'a>>,
}

#[derive(serde::Serialize)]
struct ListPipeline<'a> {
    name: &'a str,
    tasks: &'a [String],
    finally: &'a [String],
}

#[derive(serde::Serialize)]
struct ListTask<'a> {
    id: &'a str,
    command: &'a [String],
    stdin: &'static str,
}

impl<'a> From<&'a Project> for ListDocument<'a> {
    fn from(project: &'a Project) -> Self {
        Self {
            schema: crate::events::EXECUTION_EVENT_SCHEMA,
            kind: "list",
            project: &project.name,
            root: &project.root,
            default_pipeline: &project.default_pipeline,
            pipelines: project
                .pipelines
                .iter()
                .map(|(name, pipeline)| ListPipeline {
                    name,
                    tasks: &pipeline.tasks,
                    finally: &pipeline.finally,
                })
                .collect(),
            tasks: project
                .tasks
                .iter()
                .map(|(id, task)| ListTask {
                    id,
                    command: &task.command,
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
