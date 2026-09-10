//! Central, language-agnostic workspace orchestration commands.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error as StdError;
use std::fmt;
use std::path::Path;

use crate::runner::{Runner, RunnerError, emit_output, format_command};
use crate::workspace::{PlannedTask, TaskNode, Workspace, WorkspaceError};

/// Run the workspace's default pipeline with one worker.
pub fn ci(
    path: &Path,
    selected_package: Option<&str>,
    requested_tasks: &[String],
    dry_run: bool,
) -> Result<String, CiError> {
    ci_with_jobs(path, selected_package, requested_tasks, dry_run, 1)
}

/// Run the workspace's default pipeline with a bounded worker count.
pub fn ci_with_jobs(
    path: &Path,
    selected_package: Option<&str>,
    requested_tasks: &[String],
    dry_run: bool,
    jobs: usize,
) -> Result<String, CiError> {
    run_pipeline_with_jobs(path, None, selected_package, requested_tasks, dry_run, jobs)
}

/// Run a named pipeline with one worker.
pub fn run_pipeline(
    path: &Path,
    pipeline: Option<&str>,
    selected_package: Option<&str>,
    requested_tasks: &[String],
    dry_run: bool,
) -> Result<String, CiError> {
    run_pipeline_with_jobs(
        path,
        pipeline,
        selected_package,
        requested_tasks,
        dry_run,
        1,
    )
}

/// Run a named pipeline with a bounded worker count.
pub fn run_pipeline_with_jobs(
    path: &Path,
    pipeline: Option<&str>,
    selected_package: Option<&str>,
    requested_tasks: &[String],
    dry_run: bool,
    jobs: usize,
) -> Result<String, CiError> {
    if jobs == 0 {
        return Err(CiError::InvalidJobs);
    }

    let workspace = Workspace::load(path)?;
    let plan = workspace.plan(selected_package, pipeline, requested_tasks)?;

    if dry_run {
        return Ok(format_plan(&workspace, &plan));
    }

    execute_plan(&workspace, &plan, jobs)?;

    let package_count = plan
        .iter()
        .map(|task| task.package.as_str())
        .collect::<BTreeSet<_>>()
        .len();
    Ok(format!(
        "completed {} task(s) across {package_count} package(s)",
        plan.len()
    ))
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
        "workspace {} ({})",
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

fn execute_plan(workspace: &Workspace, plan: &[PlannedTask], jobs: usize) -> Result<(), CiError> {
    if jobs == 1 {
        let runner = Runner::new();
        for task in plan {
            runner.run(&workspace.root, task)?;
        }
        return Ok(());
    }

    let mut remaining = plan
        .iter()
        .map(|task| (task.node(), task.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut completed = BTreeSet::new();
    let runner = Runner::new();

    while !remaining.is_empty() {
        let ready = remaining
            .iter()
            .filter(|(_, task)| {
                task.depends_on
                    .iter()
                    .all(|dependency| completed.contains(dependency))
            })
            .take(jobs)
            .map(|(node, task)| (node.clone(), task.clone()))
            .collect::<Vec<_>>();

        if ready.is_empty() {
            return Err(CiError::Workspace(WorkspaceError::InvalidWorkspace {
                message: "execution plan contains an unresolved dependency".to_owned(),
            }));
        }

        let results = std::thread::scope(|scope| {
            let runner = &runner;
            let workspace_root = &workspace.root;
            let handles = ready
                .iter()
                .map(|(node, task)| {
                    let node = node.clone();
                    let task = task.clone();
                    scope.spawn(move || (node, runner.run_captured(workspace_root, &task)))
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("task worker panicked"))
                .collect::<Vec<_>>()
        });

        let mut first_error = None;
        for (node, result) in results {
            match result {
                Ok(output) => {
                    emit_output(&output);
                    completed.insert(node.clone());
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
            remaining.remove(&node);
        }
        if let Some(error) = first_error {
            return Err(CiError::Runner(error));
        }
    }

    Ok(())
}

fn format_plan(workspace: &Workspace, plan: &[PlannedTask]) -> String {
    let mut output = format!(
        "workspace {} ({})",
        workspace.name,
        workspace.root.display()
    );
    for task in plan {
        output.push('\n');
        output.push_str(&format!(
            "would run {}:{} in {}: {}",
            task.package,
            task.task,
            task.cwd.display(),
            format_command(&task.command)
        ));
        for (key, value) in &task.env {
            output.push_str(&format!(" [{key}={value}]"));
        }
    }
    output
}

fn format_node(node: &TaskNode) -> String {
    format!("{}:{}", node.package, node.task)
}

/// Expected failures of the orchestration commands.
#[derive(Debug)]
pub enum CiError {
    InvalidJobs,
    Workspace(WorkspaceError),
    Runner(RunnerError),
}

impl fmt::Display for CiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJobs => write!(f, "--jobs must be greater than zero"),
            Self::Workspace(error) => error.fmt(f),
            Self::Runner(error) => error.fmt(f),
        }
    }
}

impl StdError for CiError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Workspace(error) => Some(error),
            Self::Runner(error) => Some(error),
            Self::InvalidJobs => None,
        }
    }
}

impl From<WorkspaceError> for CiError {
    fn from(error: WorkspaceError) -> Self {
        Self::Workspace(error)
    }
}

impl From<RunnerError> for CiError {
    fn from(error: RunnerError) -> Self {
        Self::Runner(error)
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

        let output = ci(temp.path(), None, &[], true).expect("dry run succeeds");

        assert!(output.find("base:build").unwrap() < output.find("app:build").unwrap());
        assert!(output.contains("would run app:build"));
    }

    #[test]
    fn rejects_zero_workers_before_loading_the_workspace() {
        let error = ci_with_jobs(Path::new("."), None, &[], false, 0)
            .expect_err("zero workers are invalid");

        assert!(matches!(error, CiError::InvalidJobs));
    }
}
