//! Root-project orchestration commands.

use std::error::Error as StdError;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use crate::cache::CacheMode;
use crate::output::{OutputMode, OutputSink};
use crate::project::{PlannedTask, Project, ProjectError, TaskNode};
use crate::runner::{CancellationToken, Runner, TaskExecutor, format_command};
use crate::scheduler::{ExecutionSummary, SchedulerError, execute_plan};

#[derive(Debug, Clone)]
pub struct PipelineExecution {
    pub cache: CacheMode,
    pub output: OutputMode,
    pub cancellation: CancellationToken,
}

impl Default for PipelineExecution {
    fn default() -> Self {
        Self {
            cache: CacheMode::ReadWrite,
            output: OutputMode::Terminal,
            cancellation: CancellationToken::new(),
        }
    }
}

pub fn run_pipeline_with_mode(
    path: &Path,
    pipeline: Option<&str>,
    requested_tasks: &[String],
    dry_run: bool,
    jobs: usize,
    execution: PipelineExecution,
) -> Result<String, CiError> {
    if jobs == 0 {
        return Err(CiError::InvalidJobs);
    }
    let project = Project::load(path)?;
    let plan = project.plan(pipeline, requested_tasks)?;
    if dry_run {
        return if execution.output == OutputMode::Json {
            plan_with_output(path, pipeline, requested_tasks, OutputMode::Json)
        } else {
            Ok(format_plan(&project, &plan))
        };
    }

    let runner: Arc<dyn TaskExecutor> = Arc::new(Runner::new());
    let output = Arc::new(OutputSink::new(execution.output));
    let summary = execute_plan(
        &project,
        &plan,
        jobs,
        runner,
        &output,
        execution.cache,
        &execution.cancellation,
    )?;
    if execution.output == OutputMode::Json {
        Ok(String::new())
    } else {
        Ok(format_summary(&summary))
    }
}

pub fn plan(
    path: &Path,
    pipeline: Option<&str>,
    requested_tasks: &[String],
) -> Result<String, CiError> {
    plan_with_output(path, pipeline, requested_tasks, OutputMode::Terminal)
}

pub fn plan_with_output(
    path: &Path,
    pipeline: Option<&str>,
    requested_tasks: &[String],
    output_mode: OutputMode,
) -> Result<String, CiError> {
    let project = Project::load(path)?;
    let plan = project.plan(pipeline, requested_tasks)?;
    if output_mode == OutputMode::Json {
        return serde_json::to_string(&PlanDocument::from((&project, plan.as_slice())))
            .map_err(|source| CiError::Json { source });
    }
    Ok(format_plan(&project, &plan))
}

pub fn clean_cache(path: &Path) -> Result<String, CiError> {
    let project = Project::load(path)?;
    let cache_path = project.root.join(".mono").join("cache");
    match std::fs::remove_dir_all(&cache_path) {
        Ok(()) => Ok(format!("removed cache {}", cache_path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(format!("cache is already empty {}", cache_path.display()))
        }
        Err(source) => Err(CiError::Project(ProjectError::Io {
            path: cache_path,
            source,
        })),
    }
}

pub fn graph(
    path: &Path,
    pipeline: Option<&str>,
    requested_tasks: &[String],
) -> Result<String, CiError> {
    graph_with_output(path, pipeline, requested_tasks, OutputMode::Terminal)
}

pub fn graph_with_output(
    path: &Path,
    pipeline: Option<&str>,
    requested_tasks: &[String],
    output_mode: OutputMode,
) -> Result<String, CiError> {
    let project = Project::load(path)?;
    let edges = project.graph(pipeline, requested_tasks)?;
    if output_mode == OutputMode::Json {
        return serde_json::to_string(&GraphDocument {
            schema: crate::events::EXECUTION_EVENT_SCHEMA,
            kind: "graph",
            project: project.name.clone(),
            root: project.root.display().to_string(),
            edges: edges
                .into_iter()
                .map(|(task, dependencies)| GraphEdge {
                    task: task.id().to_owned(),
                    depends_on: dependencies
                        .into_iter()
                        .map(|dependency| dependency.id().to_owned())
                        .collect(),
                })
                .collect(),
        })
        .map_err(|source| CiError::Json { source });
    }
    let mut output = format!(
        "{} {} ({})",
        "project",
        project.name,
        project.root.display()
    );
    for (node, dependencies) in edges {
        output.push('\n');
        output.push_str(node.id());
        if !dependencies.is_empty() {
            output.push_str(" <- ");
            output.push_str(
                &dependencies
                    .iter()
                    .map(TaskNode::id)
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
    }
    Ok(output)
}

#[derive(serde::Serialize)]
struct PlanDocument {
    schema: u32,
    kind: &'static str,
    project: String,
    root: String,
    tasks: Vec<PlanTask>,
}

#[derive(serde::Serialize)]
struct PlanTask {
    id: String,
    command: Vec<String>,
    cwd: String,
    stdin: &'static str,
    cache: bool,
    inputs: Vec<String>,
    outputs: Vec<String>,
    cache_env: Vec<String>,
    timeout_seconds: u64,
    max_output_bytes: usize,
    resource_group: Option<String>,
    retries: u32,
    retry_backoff_seconds: u64,
    finalizer: bool,
    depends_on: Vec<String>,
    env: Vec<String>,
}

impl<'a> From<(&'a Project, &'a [PlannedTask])> for PlanDocument {
    fn from((project, plan): (&'a Project, &'a [PlannedTask])) -> Self {
        Self {
            schema: crate::events::EXECUTION_EVENT_SCHEMA,
            kind: "plan",
            project: project.name.clone(),
            root: project.root.display().to_string(),
            tasks: plan
                .iter()
                .map(|task| PlanTask {
                    id: task.id().to_owned(),
                    command: task.command().to_vec(),
                    cwd: task.cwd().display().to_string(),
                    stdin: task.stdin().as_str(),
                    cache: task.cache(),
                    inputs: task.inputs().to_vec(),
                    outputs: task.outputs().to_vec(),
                    cache_env: task.cache_env().to_vec(),
                    timeout_seconds: task.timeout().as_secs(),
                    max_output_bytes: task.max_output_bytes(),
                    resource_group: task.resource_group().map(str::to_owned),
                    retries: task.retries(),
                    retry_backoff_seconds: task.retry_backoff().as_secs(),
                    finalizer: task.is_finalizer(),
                    depends_on: task
                        .depends_on()
                        .iter()
                        .map(|dependency| dependency.id().to_owned())
                        .collect(),
                    env: task.env().keys().cloned().collect(),
                })
                .collect(),
        }
    }
}

#[derive(serde::Serialize)]
struct GraphDocument {
    schema: u32,
    kind: &'static str,
    project: String,
    root: String,
    edges: Vec<GraphEdge>,
}

#[derive(serde::Serialize)]
struct GraphEdge {
    task: String,
    depends_on: Vec<String>,
}

fn format_plan(project: &Project, plan: &[PlannedTask]) -> String {
    let mut output = format!(
        "{} {} ({})",
        "project",
        project.name,
        project.root.display()
    );
    for task in plan {
        output.push('\n');
        output.push_str(&format!(
            "would run {} in {}: {}",
            task.id(),
            task.cwd().display(),
            format_command(task.command())
        ));
        output.push_str(&format!(" [timeout={}s]", task.timeout().as_secs()));
        output.push_str(&format!(" [max_output_bytes={}]", task.max_output_bytes()));
        if let Some(group) = task.resource_group() {
            output.push_str(&format!(" [resource_group={group}]"));
        }
        if !task.inputs().is_empty() {
            output.push_str(&format!(" [inputs={}]", task.inputs().join(", ")));
        }
        if !task.outputs().is_empty() {
            output.push_str(&format!(" [outputs={}]", task.outputs().join(", ")));
        }
        if task.retries() > 0 {
            output.push_str(&format!(" [retries={}]", task.retries()));
            if task.retry_backoff() > std::time::Duration::ZERO {
                output.push_str(&format!(
                    " [retry_backoff={}s]",
                    task.retry_backoff().as_secs()
                ));
            }
        }
        if task.is_finalizer() {
            output.push_str(" [finally]");
        }
        for key in task.env().keys() {
            output.push_str(&format!(" [env {key}=<redacted>]"));
        }
    }
    output
}

fn format_summary(summary: &ExecutionSummary) -> String {
    format!(
        "summary: {} completed, {} cached, {} failed, {} cancelled, {} blocked",
        summary.completed, summary.cached, summary.failed, summary.cancelled, summary.blocked
    )
}

#[derive(Debug)]
pub enum CiError {
    InvalidJobs,
    Project(ProjectError),
    Scheduler(Box<SchedulerError>),
    Json { source: serde_json::Error },
}

impl fmt::Display for CiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJobs => write!(f, "--jobs must be greater than zero"),
            Self::Project(error) => error.fmt(f),
            Self::Scheduler(error) => error.fmt(f),
            Self::Json { source } => write!(f, "could not serialize JSON output: {source}"),
        }
    }
}

impl StdError for CiError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Project(error) => Some(error),
            Self::Scheduler(error) => Some(error),
            Self::Json { source } => Some(source),
            Self::InvalidJobs => None,
        }
    }
}

impl From<ProjectError> for CiError {
    fn from(error: ProjectError) -> Self {
        Self::Project(error)
    }
}

impl From<SchedulerError> for CiError {
    fn from(error: SchedulerError) -> Self {
        Self::Scheduler(Box::new(error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::config_path;
    use crate::testing::TempDir;
    use std::fs;

    #[test]
    fn dry_run_reports_dependency_order_and_commands() {
        let temp = TempDir::new();
        fs::write(config_path(temp.path()), "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"app-build\"]\n\n[tasks.base-build]\ncommand = [\"echo\", \"base\"]\n\n[tasks.app-build]\ncommand = [\"echo\", \"app\"]\ndepends_on = [\"base-build\"]\n").unwrap();
        let output = run_pipeline_with_mode(
            temp.path(),
            None,
            &[],
            true,
            1,
            PipelineExecution::default(),
        )
        .unwrap();
        assert!(output.find("base-build").unwrap() < output.find("app-build").unwrap());
        assert!(output.contains("would run app-build"));
    }

    #[cfg(unix)]
    #[test]
    fn retries_a_failed_task_before_reporting_failure() {
        let temp = TempDir::new();
        let marker = temp.path().join("attempted");
        let marker = marker.to_string_lossy();
        fs::write(config_path(temp.path()), format!("[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"sh\", \"-c\", \"if [ ! -e '{marker}' ]; then touch '{marker}'; exit 7; fi; printf success\"]\nretries = 1\n")).unwrap();
        let output = run_pipeline_with_mode(
            temp.path(),
            None,
            &[],
            false,
            1,
            PipelineExecution {
                cache: CacheMode::NoCache,
                output: OutputMode::Terminal,
                ..PipelineExecution::default()
            },
        )
        .unwrap();
        assert!(output.contains("1 completed"));
    }

    #[cfg(unix)]
    #[test]
    fn runs_finalizers_after_a_failed_task() {
        let temp = TempDir::new();
        let marker = temp.path().join("cleanup-ran");
        let marker_text = marker.to_string_lossy();
        fs::write(config_path(temp.path()), format!("[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\nfinally = [\"cleanup\"]\n\n[tasks.build]\ncommand = [\"sh\", \"-c\", \"exit 7\"]\n\n[tasks.cleanup]\ncommand = [\"touch\", \"{marker_text}\"]\n")).unwrap();
        let error = run_pipeline_with_mode(
            temp.path(),
            None,
            &[],
            false,
            1,
            PipelineExecution {
                cache: CacheMode::NoCache,
                output: OutputMode::Terminal,
                ..PipelineExecution::default()
            },
        )
        .unwrap_err();
        assert!(matches!(error, CiError::Scheduler(_)));
        assert!(marker.exists());
    }

    #[cfg(unix)]
    #[test]
    fn finalizer_dependencies_run_after_a_normal_failure() {
        let temp = TempDir::new();
        let marker = temp.path().join("cleanup-ran");
        let marker_text = marker.to_string_lossy();
        fs::write(config_path(temp.path()), format!("[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\nfinally = [\"cleanup\"]\n\n[tasks.build]\ncommand = [\"sh\", \"-c\", \"exit 7\"]\n\n[tasks.prepare-cleanup]\ncommand = [\"touch\", \"prepared\"]\n\n[tasks.cleanup]\ncommand = [\"sh\", \"-c\", \"test -f prepared && touch '{marker_text}'\"]\ndepends_on = [\"prepare-cleanup\"]\n")).unwrap();
        let error = run_pipeline_with_mode(
            temp.path(),
            None,
            &[],
            false,
            1,
            PipelineExecution {
                cache: CacheMode::NoCache,
                output: OutputMode::Terminal,
                ..PipelineExecution::default()
            },
        )
        .unwrap_err();
        assert!(matches!(error, CiError::Scheduler(_)));
        assert!(marker.exists(), "finalizer dependency closure did not run");
    }

    #[test]
    fn rejects_zero_workers_before_loading_the_project() {
        let error = run_pipeline_with_mode(
            Path::new("."),
            None,
            &[],
            false,
            0,
            PipelineExecution::default(),
        )
        .unwrap_err();
        assert!(matches!(error, CiError::InvalidJobs));
    }
}
