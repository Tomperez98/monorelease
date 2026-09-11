//! Central, language-agnostic workspace orchestration commands.

use std::collections::BTreeSet;
use std::error::Error as StdError;
use std::fmt;
use std::path::Path;

use crate::cache::CacheMode;
use crate::output::{OutputMode, OutputSink};
use crate::runner::Runner;
use crate::runner::format_command;
use crate::scheduler::{ExecutionSummary, SchedulerError, execute_plan};
use crate::workspace::{PlannedTask, TaskNode, Workspace, WorkspaceError};

/// Selects the cache and output contracts for a pipeline run.
#[derive(Debug, Clone, Copy)]
pub struct PipelineExecution {
    pub cache: CacheMode,
    pub output: OutputMode,
}

impl Default for PipelineExecution {
    /// The contract used when no flag selects one: terminal output with the
    /// normal read/write cache.
    fn default() -> Self {
        Self {
            cache: CacheMode::ReadWrite,
            output: OutputMode::Terminal,
        }
    }
}

/// Run a pipeline with an explicit execution contract.
///
/// This is the single entry point for workspace orchestration: every cache and
/// output choice travels in `execution`, and `jobs == 0` is rejected here so no
/// caller can reach the scheduler with an invalid worker count.
pub fn run_pipeline_with_mode(
    path: &Path,
    pipeline: Option<&str>,
    selected_package: Option<&str>,
    requested_tasks: &[String],
    dry_run: bool,
    jobs: usize,
    execution: PipelineExecution,
) -> Result<String, CiError> {
    if jobs == 0 {
        return Err(CiError::InvalidJobs);
    }

    let workspace = Workspace::load(path)?;
    let plan = workspace.plan(selected_package, pipeline, requested_tasks)?;

    if dry_run {
        return Ok(format_plan(&workspace, &plan));
    }

    let runner = Runner::new();
    let output = OutputSink::new(execution.output);
    let summary = execute_plan(&workspace, &plan, jobs, &runner, &output, execution.cache)?;

    let package_count = plan
        .iter()
        .map(|task| task.package())
        .collect::<BTreeSet<_>>()
        .len();
    Ok(format_summary(&summary, package_count))
}

/// Return the resolved execution plan without running commands.
pub fn plan(
    path: &Path,
    pipeline: Option<&str>,
    selected_package: Option<&str>,
    requested_tasks: &[String],
) -> Result<String, CiError> {
    let workspace = Workspace::load(path)?;
    let plan = workspace.plan(selected_package, pipeline, requested_tasks)?;
    Ok(format_plan(&workspace, &plan))
}

/// Remove local cache entries while preserving the workspace and ignore file.
pub fn clean_cache(path: &Path) -> Result<String, CiError> {
    let workspace = Workspace::load(path)?;
    let cache_path = workspace.root.join(".monorelease").join("cache");
    match std::fs::remove_dir_all(&cache_path) {
        Ok(()) => Ok(format!("removed cache {}", cache_path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(format!("cache is already empty {}", cache_path.display()))
        }
        Err(source) => Err(CiError::Workspace(WorkspaceError::Io {
            path: cache_path,
            source,
        })),
    }
}

/// Return task-DAG edges without running commands.
pub fn graph(
    path: &Path,
    pipeline: Option<&str>,
    selected_package: Option<&str>,
    requested_tasks: &[String],
) -> Result<String, CiError> {
    let workspace = Workspace::load(path)?;
    let edges = workspace.graph(selected_package, pipeline, requested_tasks)?;
    let mut output = format!(
        "{} {} ({})",
        workspace.scope_label(),
        workspace.name,
        workspace.root.display()
    );
    for (node, dependencies) in edges {
        output.push('\n');
        output.push_str(&format_node(&node));
        if !dependencies.is_empty() {
            output.push_str(" <- ");
            output.push_str(
                &dependencies
                    .iter()
                    .map(format_node)
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
    }
    Ok(output)
}

fn format_plan(workspace: &Workspace, plan: &[PlannedTask]) -> String {
    let mut output = format!(
        "{} {} ({})",
        workspace.scope_label(),
        workspace.name,
        workspace.root.display()
    );
    for task in plan {
        output.push('\n');
        output.push_str(&format!(
            "would run {}:{} in {}: {}",
            task.package(),
            task.task(),
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
        for key in task.env().keys() {
            output.push_str(&format!(" [env {key}=<redacted>]"));
        }
    }
    output
}

fn format_summary(summary: &ExecutionSummary, package_count: usize) -> String {
    format!(
        "summary: {} completed, {} cached, {} failed, {} blocked across {package_count} package(s)",
        summary.completed, summary.cached, summary.failed, summary.blocked
    )
}

fn format_node(node: &TaskNode) -> String {
    format!("{}:{}", node.package, node.task)
}

/// Expected failures of the orchestration commands.
#[derive(Debug)]
pub enum CiError {
    InvalidJobs,
    Workspace(WorkspaceError),
    Scheduler(Box<SchedulerError>),
}

impl fmt::Display for CiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJobs => write!(f, "--jobs must be greater than zero"),
            Self::Workspace(error) => error.fmt(f),
            Self::Scheduler(error) => error.fmt(f),
        }
    }
}

impl StdError for CiError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Workspace(error) => Some(error),
            Self::Scheduler(error) => Some(error),
            Self::InvalidJobs => None,
        }
    }
}

impl From<WorkspaceError> for CiError {
    fn from(error: WorkspaceError) -> Self {
        Self::Workspace(error)
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
        fs::write(
            config_path(temp.path()),
            "[workspace]\nname = \"fixture\"\nmembers = [\"packages/*\"]\n\n[pipelines.ci]\ntasks = [\"build\"]\n",
        )
        .expect("write root manifest");
        for (name, dependency) in [("base", ""), ("app", "depends_on = [\"base:build\"]\n")] {
            let package = temp.path().join("packages").join(name);
            fs::create_dir_all(&package).expect("create package");
            fs::write(
                config_path(&package),
                format!(
                    "[package]\nname = \"{name}\"\n\n[tasks.build]\ncommand = [\"echo\", \"{name}\"]\n{dependency}"
                ),
            )
            .expect("write package manifest");
        }

        let output = run_pipeline_with_mode(
            temp.path(),
            None,
            None,
            &[],
            true,
            1,
            PipelineExecution::default(),
        )
        .expect("dry run succeeds");

        assert!(output.find("base:build").unwrap() < output.find("app:build").unwrap());
        assert!(output.contains("would run app:build"));
    }

    #[cfg(unix)]
    #[test]
    fn resource_groups_prevent_overlapping_tasks() {
        let temp = TempDir::new();
        fs::write(
            config_path(temp.path()),
            "[workspace]\nname = \"fixture\"\nmembers = [\"packages/*\"]\n\n[pipelines.ci]\ntasks = [\"build\"]\n",
        )
        .expect("write root manifest");
        let lock = temp.path().join("resource.lock");
        let lock = lock.to_string_lossy();
        for (name, command) in [
            ("a", format!("touch '{lock}'; sleep 0.2; rm '{lock}'")),
            ("b", format!("sleep 0.05; test ! -e '{lock}'")),
        ] {
            let package = temp.path().join("packages").join(name);
            fs::create_dir_all(&package).expect("create package");
            fs::write(
                config_path(&package),
                format!(
                    "[package]\nname = \"{name}\"\n\n[tasks.build]\ncommand = [\"sh\", \"-c\", \"{command}\"]\nresource_group = \"integration\"\n"
                ),
            )
            .expect("write package manifest");
        }

        run_pipeline_with_mode(
            temp.path(),
            None,
            None,
            &[],
            false,
            2,
            PipelineExecution::default(),
        )
        .expect("resource group serializes tasks");
    }

    #[test]
    fn rejects_zero_workers_before_loading_the_workspace() {
        let error = run_pipeline_with_mode(
            Path::new("."),
            None,
            None,
            &[],
            false,
            0,
            PipelineExecution::default(),
        )
        .expect_err("zero workers are invalid");

        assert!(matches!(error, CiError::InvalidJobs));
    }
}
