//! Root-project orchestration commands.

use std::error::Error as StdError;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use crate::cache::CacheMode;
use crate::output::{OutputMode, OutputSink};
use crate::project::{PlannedTask, Project, ProjectError};
use crate::runner::{CancellationToken, Runner, TaskExecutor, format_command};
use crate::scheduler::{
    ExecutionSummary, SchedulerError, SchedulerOptions, SchedulerServices, TaskReporter,
    execute_plan_with_services, production_services,
};

pub(crate) struct PipelineServices {
    runner: Arc<dyn TaskExecutor>,
    output: Arc<dyn TaskReporter>,
    scheduler: SchedulerServices,
}

impl PipelineServices {
    fn production(
        project: &Project,
        output: OutputMode,
        cancellation: &CancellationToken,
    ) -> Result<Self, CiError> {
        let output = OutputSink::new(output, cancellation.clone())
            .map_err(|error| CiError::Scheduler(Box::new(SchedulerError::Output(error))))?;
        Ok(Self {
            runner: Arc::new(Runner::new()),
            output: Arc::new(output),
            scheduler: production_services(&project.root),
        })
    }
}

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
            serde_json::to_string(&PlanDocument::from((&project, plan.as_slice())))
                .map_err(|source| CiError::Json { source })
        } else {
            Ok(format_plan_document(&PlanDocument::from((
                &project,
                plan.as_slice(),
            ))))
        };
    }

    let services =
        PipelineServices::production(&project, execution.output, &execution.cancellation)?;
    let summary = execute_loaded_pipeline(&project, &plan, jobs, &execution, &services)?;
    if execution.output == OutputMode::Json {
        Ok(String::new())
    } else {
        Ok(format_summary(&summary))
    }
}

pub(crate) fn execute_loaded_pipeline(
    project: &Project,
    plan: &[PlannedTask],
    jobs: usize,
    execution: &PipelineExecution,
    services: &PipelineServices,
) -> Result<ExecutionSummary, CiError> {
    if jobs == 0 {
        return Err(CiError::InvalidJobs);
    }
    let options = SchedulerOptions {
        jobs,
        cache_mode: execution.cache,
        cancellation: &execution.cancellation,
        services: &services.scheduler,
    };
    execute_plan_with_services(
        project,
        plan,
        Arc::clone(&services.runner),
        &services.output,
        &options,
    )
    .map_err(Into::into)
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
    let document = plan_result(path, pipeline, requested_tasks)?;
    if output_mode == OutputMode::Json {
        return serde_json::to_string(&document).map_err(|source| CiError::Json { source });
    }
    Ok(format_plan_document(&document))
}

/// Resolve a plan into an owned, presentation-independent document.
///
/// The returned value contains no `Project` or filesystem handles. Text and
/// JSON renderers can consume it without repeating project loading or planning.
pub(crate) fn plan_result(
    path: &Path,
    pipeline: Option<&str>,
    requested_tasks: &[String],
) -> Result<PlanDocument, CiError> {
    let project = Project::load(path)?;
    let plan = project.plan(pipeline, requested_tasks)?;
    Ok(PlanDocument::from((&project, plan.as_slice())))
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
    let document = graph_result(path, pipeline, requested_tasks)?;
    if output_mode == OutputMode::Json {
        return serde_json::to_string(&document).map_err(|source| CiError::Json { source });
    }
    Ok(format_graph_document(&document))
}

/// Resolve dependency edges into an owned, presentation-independent document.
pub(crate) fn graph_result(
    path: &Path,
    pipeline: Option<&str>,
    requested_tasks: &[String],
) -> Result<GraphDocument, CiError> {
    let project = Project::load(path)?;
    let edges = project.graph(pipeline, requested_tasks)?;
    Ok(GraphDocument {
        schema: crate::events::EXECUTION_EVENT_SCHEMA,
        kind: "graph",
        project: project.name,
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
}

#[derive(serde::Serialize)]
pub(crate) struct PlanDocument {
    schema: u32,
    kind: &'static str,
    project: String,
    root: String,
    tasks: Vec<PlanTask>,
}

#[derive(serde::Serialize)]
pub(crate) struct PlanTask {
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
pub(crate) struct GraphDocument {
    schema: u32,
    kind: &'static str,
    project: String,
    root: String,
    edges: Vec<GraphEdge>,
}

#[derive(serde::Serialize)]
pub(crate) struct GraphEdge {
    task: String,
    depends_on: Vec<String>,
}

fn format_plan_document(document: &PlanDocument) -> String {
    let mut output = format!("project {} ({})", document.project, document.root);
    for task in &document.tasks {
        output.push('\n');
        output.push_str(&format!(
            "would run {} in {}: {}",
            task.id,
            task.cwd,
            format_command(&task.command)
        ));
        output.push_str(&format!(" [timeout={}s]", task.timeout_seconds));
        output.push_str(&format!(" [max_output_bytes={}]", task.max_output_bytes));
        if let Some(group) = &task.resource_group {
            output.push_str(&format!(" [resource_group={group}]"));
        }
        if !task.inputs.is_empty() {
            output.push_str(&format!(" [inputs={}]", task.inputs.join(", ")));
        }
        if !task.outputs.is_empty() {
            output.push_str(&format!(" [outputs={}]", task.outputs.join(", ")));
        }
        if task.retries > 0 {
            output.push_str(&format!(" [retries={}]", task.retries));
            if task.retry_backoff_seconds > 0 {
                output.push_str(&format!(" [retry_backoff={}s]", task.retry_backoff_seconds));
            }
        }
        if task.finalizer {
            output.push_str(" [finally]");
        }
        for key in &task.env {
            output.push_str(&format!(" [env {key}=<redacted>]"));
        }
    }
    output
}

fn format_graph_document(document: &GraphDocument) -> String {
    let mut output = format!("project {} ({})", document.project, document.root);
    for edge in &document.edges {
        output.push('\n');
        output.push_str(&edge.task);
        if !edge.depends_on.is_empty() {
            output.push_str(" <- ");
            output.push_str(&edge.depends_on.join(", "));
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
    use crate::cache::{CacheBackend, CacheError, CacheSession};
    use crate::config::config_path;
    use crate::runner::{CapturedOutput, RunnerError, TaskResult};
    use crate::scheduler::{RetrySleeper, SchedulerServices};
    use crate::testing::TempDir;
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingExecutor {
        calls: Mutex<Vec<String>>,
    }

    impl RecordingExecutor {
        fn calls(&self) -> Vec<String> {
            self.calls
                .lock()
                .expect("executor calls are not poisoned")
                .clone()
        }
    }

    impl TaskExecutor for RecordingExecutor {
        fn execute(
            &self,
            _project_root: &Path,
            task: &PlannedTask,
            _cancellation: Option<&CancellationToken>,
            _output: Option<crate::runner::OutputCallback>,
        ) -> Result<TaskResult, RunnerError> {
            self.calls
                .lock()
                .expect("executor calls are not poisoned")
                .push(task.id().to_owned());
            Ok(TaskResult {
                output: CapturedOutput::default(),
                elapsed: std::time::Duration::ZERO,
                cached: false,
            })
        }
    }

    struct NoopSleeper;

    impl RetrySleeper for NoopSleeper {
        fn sleep(&self, _duration: std::time::Duration) {}
    }

    struct HitCache {
        result: TaskResult,
    }

    impl CacheBackend for HitCache {
        fn prepare(
            &self,
            _project_root: &Path,
            _environment: BTreeMap<String, String>,
        ) -> Result<CacheSession, CacheError> {
            Ok(CacheSession::for_test())
        }

        fn task_key(
            &self,
            _session: &CacheSession,
            _task: &PlannedTask,
            _dependency_keys: &[String],
        ) -> Result<String, CacheError> {
            Ok("scripted-key".to_owned())
        }

        fn lookup(
            &self,
            _task: &PlannedTask,
            _key: &str,
        ) -> Result<Option<TaskResult>, CacheError> {
            Ok(Some(self.result.clone()))
        }

        fn store(
            &self,
            _task: &PlannedTask,
            _key: &str,
            _result: &TaskResult,
        ) -> Result<(), CacheError> {
            Ok(())
        }
    }

    fn injected_services(
        runner: Arc<dyn TaskExecutor>,
        cache: Arc<dyn CacheBackend>,
    ) -> PipelineServices {
        PipelineServices {
            runner,
            output: Arc::new(OutputSink::test_sink(
                OutputMode::Terminal,
                1,
                Box::new(Vec::new()),
                Box::new(Vec::new()),
            )),
            scheduler: SchedulerServices {
                cache,
                sleeper: Arc::new(NoopSleeper),
                environment: BTreeMap::new(),
            },
        }
    }

    fn load_project(temp: &TempDir, manifest: &str) -> (Project, Vec<PlannedTask>) {
        fs::write(config_path(temp.path()), manifest).expect("write project manifest");
        let project = Project::load(temp.path()).expect("project loads");
        let plan = project.plan(None, &[]).expect("plan succeeds");
        (project, plan)
    }

    #[test]
    fn loaded_pipeline_uses_the_injected_executor_and_returns_summary() {
        let temp = TempDir::new();
        let (project, plan) = load_project(
            &temp,
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"app\"]\n\n[tasks.base]\ncommand = [\"echo\", \"base\"]\n\n[tasks.app]\ncommand = [\"echo\", \"app\"]\ndepends_on = [\"base\"]\n",
        );
        let executor = Arc::new(RecordingExecutor::default());
        let services = injected_services(
            Arc::clone(&executor) as Arc<dyn TaskExecutor>,
            Arc::new(HitCache {
                result: TaskResult {
                    output: CapturedOutput::default(),
                    elapsed: std::time::Duration::ZERO,
                    cached: true,
                },
            }),
        );
        let execution = PipelineExecution {
            cache: CacheMode::NoCache,
            output: OutputMode::Terminal,
            ..PipelineExecution::default()
        };

        let summary = execute_loaded_pipeline(&project, &plan, 1, &execution, &services)
            .expect("injected pipeline succeeds");

        assert_eq!(executor.calls(), vec!["base".to_owned(), "app".to_owned()]);
        assert_eq!(summary.completed, 2);
        assert_eq!(summary.cached, 0);
    }

    #[test]
    fn loaded_pipeline_does_not_invoke_the_executor_when_the_injected_cache_hits() {
        let temp = TempDir::new();
        fs::create_dir(temp.path().join("src")).expect("create input directory");
        fs::write(temp.path().join("src/input.txt"), "input").expect("write input");
        let (project, plan) = load_project(
            &temp,
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\ncache = true\ninputs = [\"src/**\"]\noutputs = [\"dist/**\"]\n",
        );
        let executor = Arc::new(RecordingExecutor::default());
        let services = injected_services(
            Arc::clone(&executor) as Arc<dyn TaskExecutor>,
            Arc::new(HitCache {
                result: TaskResult {
                    output: CapturedOutput::default(),
                    elapsed: std::time::Duration::ZERO,
                    cached: true,
                },
            }),
        );
        let execution = PipelineExecution::default();

        let summary = execute_loaded_pipeline(&project, &plan, 1, &execution, &services)
            .expect("cached pipeline succeeds");

        assert!(executor.calls().is_empty());
        assert_eq!(summary.cached, 1);
        assert_eq!(summary.completed, 0);
    }

    #[test]
    fn loaded_pipeline_rejects_zero_jobs_before_dispatching() {
        let temp = TempDir::new();
        let (project, plan) = load_project(
            &temp,
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n",
        );
        let executor = Arc::new(RecordingExecutor::default());
        let services = injected_services(
            Arc::clone(&executor) as Arc<dyn TaskExecutor>,
            Arc::new(HitCache {
                result: TaskResult {
                    output: CapturedOutput::default(),
                    elapsed: std::time::Duration::ZERO,
                    cached: true,
                },
            }),
        );

        let error =
            execute_loaded_pipeline(&project, &plan, 0, &PipelineExecution::default(), &services)
                .expect_err("zero workers must fail");

        assert!(matches!(error, CiError::InvalidJobs));
        assert!(executor.calls().is_empty());
    }

    #[test]
    fn plan_json_contains_stable_task_metadata_without_environment_values() {
        let temp = TempDir::new();
        fs::write(
            config_path(temp.path()),
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"app\"]\n\n[tasks.base]\ncommand = [\"echo\", \"base\"]\n\n[tasks.app]\ncommand = [\"echo\", \"app\"]\ndepends_on = [\"base\"]\nenv = { TOKEN = \"secret\" }\n",
        )
        .expect("write project manifest");

        let document =
            plan_with_output(temp.path(), None, &[], OutputMode::Json).expect("JSON plan succeeds");
        let value: serde_json::Value = serde_json::from_str(&document).expect("valid JSON");

        assert_eq!(value["schema"], crate::events::EXECUTION_EVENT_SCHEMA);
        assert_eq!(value["kind"], "plan");
        assert_eq!(value["tasks"][0]["id"], "base");
        assert_eq!(value["tasks"][1]["id"], "app");
        assert_eq!(value["tasks"][1]["env"][0], "TOKEN");
        assert!(!document.contains("secret"));
    }

    #[test]
    fn graph_json_contains_one_dependency_edge_per_planned_task() {
        let temp = TempDir::new();
        fs::write(
            config_path(temp.path()),
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"app\"]\n\n[tasks.base]\ncommand = [\"echo\", \"base\"]\n\n[tasks.app]\ncommand = [\"echo\", \"app\"]\ndepends_on = [\"base\"]\n",
        )
        .expect("write project manifest");

        let document = graph_with_output(temp.path(), None, &[], OutputMode::Json)
            .expect("JSON graph succeeds");
        let value: serde_json::Value = serde_json::from_str(&document).expect("valid JSON");

        assert_eq!(value["kind"], "graph");
        assert_eq!(value["edges"].as_array().expect("edge array").len(), 2);
        assert_eq!(value["edges"][1]["task"], "app");
        assert_eq!(value["edges"][1]["depends_on"][0], "base");
    }

    #[test]
    fn terminal_plan_redacts_environment_values() {
        let temp = TempDir::new();
        fs::write(
            config_path(temp.path()),
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\nenv = { TOKEN = \"secret\", MODE = \"check\" }\n",
        )
        .expect("write project manifest");

        let output = plan(temp.path(), None, &[]).expect("terminal plan succeeds");

        assert!(output.contains("env TOKEN=<redacted>"));
        assert!(output.contains("env MODE=<redacted>"));
        assert!(!output.contains("secret"));
        assert!(!output.contains("check"));
    }

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
