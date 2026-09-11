//! Dependency-aware bounded task scheduling.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::error::Error as StdError;
use std::fmt;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::cache::{CacheBackend, CacheError, CacheMode, CacheSession, CacheStore};
use crate::output::OutputSink;
use crate::project::{PlannedTask, Project, TaskNode};
use crate::runner::{CancellationToken, RunnerError, TaskExecutor, TaskResult};

/// The environment collected once at the edge of a scheduler run.
///
/// `std::env::vars_os` plus a lossy conversion, not `vars`: `vars` panics
/// when any ambient variable is not valid Unicode, which would abort the
/// entire run because of an unrelated environment variable. A non-Unicode
/// value is an expected condition, not a broken invariant, so it must not
/// panic.
pub(crate) fn ambient_environment() -> BTreeMap<String, String> {
    std::env::vars_os()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect::<BTreeMap<_, _>>()
}

/// The seam the scheduling loop uses for retry backoff.
pub(crate) trait RetrySleeper: Send + Sync {
    fn sleep(&self, duration: Duration);
}

/// Production retry sleeper that delegates to [`thread::sleep`].
struct ThreadSleeper;

impl RetrySleeper for ThreadSleeper {
    fn sleep(&self, duration: Duration) {
        thread::sleep(duration);
    }
}

/// Injectable services replacing ambient state in the scheduler.
pub(crate) struct SchedulerServices {
    pub(crate) cache: Arc<dyn CacheBackend>,
    pub(crate) sleeper: Arc<dyn RetrySleeper>,
    pub(crate) environment: BTreeMap<String, String>,
}

struct SchedulerOptions<'a> {
    jobs: usize,
    cache_mode: CacheMode,
    cancellation: &'a CancellationToken,
    services: &'a SchedulerServices,
}

/// Execute a validated plan through the production services.
///
/// Convenience wrapper: creates the ambient environment, cache storage, and
/// thread sleeper, then delegates to [`execute_plan_with_services`].
pub(crate) fn execute_plan(
    project: &Project,
    plan: &[PlannedTask],
    jobs: usize,
    runner: Arc<dyn TaskExecutor>,
    output: &Arc<OutputSink>,
    cache_mode: CacheMode,
    cancellation: &CancellationToken,
) -> Result<ExecutionSummary, SchedulerError> {
    let root = Arc::new(project.root.clone());
    let services = SchedulerServices {
        cache: Arc::new(CacheStore::new(&root)),
        sleeper: Arc::new(ThreadSleeper),
        environment: ambient_environment(),
    };
    let options = SchedulerOptions {
        jobs,
        cache_mode,
        cancellation,
        services: &services,
    };
    execute_plan_with_services(project, plan, runner, output, &options)
}

/// Execute a validated plan with explicit cache, sleeper, and environment.
///
/// All side effects — cache filesystem access, retry sleeping, and ambient
/// environment reads — flow through the supplied services so that the
/// scheduling loop can be driven deterministically in tests without a real
/// filesystem, process tree, or wall clock.
fn execute_plan_with_services(
    project: &Project,
    plan: &[PlannedTask],
    runner: Arc<dyn TaskExecutor>,
    output: &Arc<OutputSink>,
    options: &SchedulerOptions<'_>,
) -> Result<ExecutionSummary, SchedulerError> {
    assert!(options.jobs > 0, "scheduler requires at least one worker");

    let jobs = options.jobs;
    let cache_mode = options.cache_mode;
    let cancellation = options.cancellation;
    let services = options.services;
    let mut state = PlanState::new(plan)?;
    let root = Arc::new(project.root.clone());

    // Prepare the cache session if any task is caching.
    let cache_session =
        if !matches!(cache_mode, CacheMode::NoCache) && plan.iter().any(PlannedTask::cache) {
            Some(Arc::new(
                services
                    .cache
                    .prepare(&root, services.environment.clone())
                    .map_err(SchedulerError::Cache)?,
            ))
        } else {
            None
        };

    output
        .present_run_start(&project.root, plan.len())
        .map_err(SchedulerError::Output)?;
    let force = matches!(cache_mode, CacheMode::Force);
    let (sender, receiver) = mpsc::channel::<(TaskNode, WorkerReport)>();
    let (job_sender, job_receiver) = mpsc::channel::<WorkerJob>();
    let shared_job_receiver = Arc::new(Mutex::new(job_receiver));
    let worker_count = jobs.min(plan.len());
    let mut workers = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let job_receiver = Arc::clone(&shared_job_receiver);
        let report_sender = sender.clone();
        workers.push(thread::spawn(move || {
            worker_loop(job_receiver, report_sender)
        }));
    }
    drop(sender);

    let mut cache_error = None;
    let mut output_error = None;

    while state.has_active_or_ready() {
        assert!(
            state.active.len() <= jobs,
            "scheduler exceeded its worker limit"
        );
        while state.active.len() < jobs {
            if cancellation.is_cancelled() {
                state.stopping = true;
            }
            let Some(node) = state.next_ready() else {
                break;
            };
            if let Err(error) = output.present_start(&node) {
                output_error = Some(error);
                state.stopping = true;
                break;
            }
            let can_cache = state.can_cache(&node, cache_mode);
            let dependency_keys = if can_cache {
                state.dependency_keys(&node)
            } else {
                Vec::new()
            };
            state.mark_dispatched(&node, can_cache);

            let task = state
                .tasks
                .get(&node)
                .expect("ready task must exist in the validated plan");
            let task = Arc::clone(task);
            job_sender
                .send(WorkerJob {
                    node: node.clone(),
                    task: Arc::clone(&task),
                    cache: Arc::clone(&services.cache),
                    sleeper: Arc::clone(&services.sleeper),
                    cache_session: cache_session.clone(),
                    root: Arc::clone(&root),
                    can_cache,
                    force,
                    dependency_keys,
                    runner: Arc::clone(&runner),
                    cancellation: cancellation.clone(),
                    output: Arc::clone(output),
                })
                .expect("worker pool remains alive while tasks are dispatched");
            assert!(
                state.active.len() <= jobs,
                "scheduler exceeded its worker limit"
            );
        }

        if state.active.is_empty() {
            break;
        }

        let (node, report) = receiver
            .recv()
            .expect("scheduler workers always send one completion result");

        match report {
            WorkerReport::Finished {
                result,
                key,
                cache_error: store_error,
            } => {
                if let Some(error) = state.complete(node, result, key, store_error) {
                    cache_error = Some(error);
                }
            }
            WorkerReport::CacheFailed(error) => {
                assert!(state.active.remove(&node), "completed task must be active");
                cache_error = Some(error);
                state.stopping = true;
                // Release the resource group held by this task so finalizers
                // sharing the group can still run.
                if let Some(group) = state
                    .tasks
                    .get(&node)
                    .expect("completed task must exist in the validated plan")
                    .resource_group()
                {
                    state.active_groups.remove(group);
                }
                // Cache failure still unblocks finalizer dependents so the
                // run can finish with a clean error.
                if let Some(children) = state.dependents.get(&node).cloned() {
                    for child in children {
                        if state.is_finalizer(&child) {
                            let count = state
                                .remaining_dependencies
                                .get_mut(&child)
                                .expect("dependent must exist in the validated plan");
                            *count -= 1;
                            if *count == 0 {
                                state.ready.insert(child);
                            }
                        }
                    }
                }
            }
            WorkerReport::OutputFailed(error) => {
                assert!(state.active.remove(&node), "completed task must be active");
                output_error = Some(error);
                state.stopping = true;
            }
        }
    }

    drop(job_sender);
    for worker in workers {
        worker.join().expect("scheduler worker panicked");
    }

    if state.active.is_empty()
        && !state.stopping
        && state.ready.is_empty()
        && state.results.len() < plan.len()
    {
        return Err(SchedulerError::NoReadyWork);
    }

    let (summary, first_error_node) = state.finish(plan);

    for task in plan {
        let node = task.node();
        match state.results.get(&node) {
            None => output
                .present_blocked(&node)
                .map_err(SchedulerError::Output)?,
            Some(Ok(result)) => output
                .present_success(&node, result)
                .map_err(SchedulerError::Output)?,
            Some(Err(error)) => output
                .present_failure(&node, error)
                .map_err(SchedulerError::Output)?,
        }
    }
    let first_error = first_error_node.and_then(|node| match state.results.remove(&node) {
        Some(Err(error)) => Some(error),
        _ => None,
    });

    if let Some(error) = output_error {
        output
            .present_run_finished(&summary)
            .map_err(SchedulerError::Output)?;
        return Err(SchedulerError::Output(error));
    }
    if let Some(error) = cache_error {
        output
            .present_run_finished(&summary)
            .map_err(SchedulerError::Output)?;
        return Err(SchedulerError::Cache(error));
    }
    if let Some(error) = first_error {
        output
            .present_run_finished(&summary)
            .map_err(SchedulerError::Output)?;
        return Err(SchedulerError::Task(Box::new(error)));
    }
    output
        .present_run_finished(&summary)
        .map_err(SchedulerError::Output)?;
    if cancellation.is_cancelled() && summary.cancelled == 0 && summary.blocked > 0 {
        return Err(SchedulerError::Cancelled);
    }
    Ok(summary)
}

// --------------------------------------------------------------------------
// PlanState — pure dependency-graph state without I/O, sleeping, or threads
// --------------------------------------------------------------------------

/// Pure dependency-graph state for one scheduler run.
///
/// Owns the task graph, remaining dependency counters, ready/active sets,
/// resource groups, completion results, cache-key index, and the stopping
/// flag.  Methods transition the graph without touching files, sleeping,
/// spawning threads, or reading process-global state.
struct PlanState {
    tasks: BTreeMap<TaskNode, Arc<PlannedTask>>,
    remaining_dependencies: BTreeMap<TaskNode, usize>,
    dependents: BTreeMap<TaskNode, Vec<TaskNode>>,
    ready: BTreeSet<TaskNode>,
    active: HashSet<TaskNode>,
    active_groups: HashSet<String>,
    results: BTreeMap<TaskNode, Result<TaskResult, RunnerError>>,
    task_keys: BTreeMap<TaskNode, String>,
    cacheable: BTreeMap<TaskNode, bool>,
    stopping: bool,
}

impl PlanState {
    fn new(plan: &[PlannedTask]) -> Result<Self, SchedulerError> {
        let mut tasks = BTreeMap::new();
        for task in plan {
            let node = task.node();
            assert!(
                tasks.insert(node, Arc::new(task.clone())).is_none(),
                "validated plan contains duplicate task nodes"
            );
        }
        let remaining_dependencies = tasks
            .iter()
            .map(|(node, task)| (node.clone(), task.depends_on().len()))
            .collect::<BTreeMap<_, _>>();
        let mut dependents = BTreeMap::<TaskNode, Vec<TaskNode>>::new();
        for task in plan {
            for dependency in task.depends_on() {
                if !tasks.contains_key(dependency) {
                    return Err(SchedulerError::UnresolvedDependency {
                        task: task.node(),
                        dependency: dependency.clone(),
                    });
                }
                dependents
                    .entry(dependency.clone())
                    .or_default()
                    .push(task.node());
            }
        }
        let ready = remaining_dependencies
            .iter()
            .filter_map(|(node, count)| (*count == 0).then_some(node.clone()))
            .collect::<BTreeSet<_>>();

        Ok(Self {
            tasks,
            remaining_dependencies,
            dependents,
            ready,
            active: HashSet::new(),
            active_groups: HashSet::new(),
            results: BTreeMap::new(),
            task_keys: BTreeMap::new(),
            cacheable: BTreeMap::new(),
            stopping: false,
        })
    }

    /// Whether there is work to do or tasks in flight.
    ///
    /// Does not short-circuit on errors because [`next_ready`] restricts
    /// dispatch to finalizers only when [`stopping`] is set.  Prematurely
    /// returning `false` on errors would skip finalizer dispatch even when
    /// finalizers are ready.
    fn has_active_or_ready(&self) -> bool {
        if self.active.is_empty() && self.ready.is_empty() {
            return false;
        }
        true
    }

    /// Pick the next task to dispatch, respecting resource groups, stopping,
    /// and finalizer sequencing.
    fn next_ready(&self) -> Option<TaskNode> {
        if self.active.is_empty() && self.ready.is_empty() {
            return None;
        }
        let _finalizers_allowed = self.finalizers_allowed();

        // If only finalizers are ready and they are not permitted, return None.
        if !self.active.is_empty()
            && self.ready.iter().all(|node| self.is_finalizer(node))
            && !_finalizers_allowed
        {
            return None;
        }

        let finalizers_allowed = _finalizers_allowed;

        self.ready
            .iter()
            .find(|node| {
                let task = self
                    .tasks
                    .get(node)
                    .expect("ready task must exist in the validated plan");

                // In stopping mode, only finalizers may start.
                if self.stopping && !task.is_finalizer() {
                    return false;
                }
                // Finalizers may only start when allowed.
                if task.is_finalizer() && !finalizers_allowed {
                    return false;
                }
                // Resource group must be free.
                match task.resource_group() {
                    Some(group) => !self.active_groups.contains(group),
                    None => true,
                }
            })
            .cloned()
    }

    /// Whether any normal task remains uncompleted.
    fn has_normal_uncompleted(&self) -> bool {
        self.tasks
            .iter()
            .filter(|(_, task)| !task.is_finalizer())
            .any(|(node, _)| !self.results.contains_key(node))
    }

    /// Whether a finalizer may start now.
    fn finalizers_allowed(&self) -> bool {
        if self.stopping {
            !self.active.iter().any(|node| {
                !self
                    .tasks
                    .get(node)
                    .expect("active task must exist in the validated plan")
                    .is_finalizer()
            })
        } else {
            !self.has_normal_uncompleted()
        }
    }

    fn is_finalizer(&self, node: &TaskNode) -> bool {
        self.tasks
            .get(node)
            .expect("task must exist in the validated plan")
            .is_finalizer()
    }

    /// Whether the given task can participate in caching.
    fn can_cache(&self, node: &TaskNode, cache_mode: CacheMode) -> bool {
        let task = self
            .tasks
            .get(node)
            .expect("task must exist in the validated plan");
        if !task.cache() || matches!(cache_mode, CacheMode::NoCache) {
            return false;
        }
        task.depends_on()
            .iter()
            .all(|dependency| self.cacheable.get(dependency).copied().unwrap_or(false))
    }

    /// The cache keys of every cacheable dependency.
    fn dependency_keys(&self, node: &TaskNode) -> Vec<String> {
        let task = self
            .tasks
            .get(node)
            .expect("task must exist in the validated plan");
        task.depends_on()
            .iter()
            .map(|dependency| {
                self.task_keys
                    .get(dependency)
                    .expect("cacheable dependency must have a task key")
                    .clone()
            })
            .collect()
    }

    /// Record that `node` has been dispatched to a worker.
    fn mark_dispatched(&mut self, node: &TaskNode, can_cache: bool) {
        self.ready.remove(node);
        self.cacheable.insert(node.clone(), can_cache);
        if let Some(group) = self
            .tasks
            .get(node)
            .expect("dispatched task must exist in the validated plan")
            .resource_group()
        {
            self.active_groups.insert(group.to_owned());
        }
        self.active.insert(node.clone());
    }

    /// Record one worker completion.
    fn complete(
        &mut self,
        node: TaskNode,
        result: Result<TaskResult, RunnerError>,
        key: Option<String>,
        cache_error: Option<CacheError>,
    ) -> Option<CacheError> {
        assert!(self.active.remove(&node), "completed task must be active");
        if let Some(group) = self
            .tasks
            .get(&node)
            .expect("completed task must exist in the validated plan")
            .resource_group()
        {
            self.active_groups.remove(group);
        }

        if let Some(key) = key {
            self.task_keys.insert(node.clone(), key);
        }
        if cache_error.is_some() {
            self.stopping = true;
        }

        if result.is_err() || cache_error.is_some() {
            self.stopping = true;
        }

        // Unblock dependents.
        if let Some(children) = self.dependents.get(&node) {
            for child in children {
                let child_is_finalizer = self.is_finalizer(child);
                if result.is_ok() && cache_error.is_none() || child_is_finalizer {
                    let count = self
                        .remaining_dependencies
                        .get_mut(child)
                        .expect("dependent must exist in the validated plan");
                    *count -= 1;
                    if *count == 0 {
                        self.ready.insert(child.clone());
                    }
                }
            }
        }

        self.results.insert(node, result);
        cache_error
    }

    /// Classify the completed plan and return the first error node.
    fn finish(&self, plan: &[PlannedTask]) -> (ExecutionSummary, Option<TaskNode>) {
        let nodes = plan.iter().map(PlannedTask::node).collect::<Vec<_>>();
        classify(&nodes, |node| match self.results.get(node) {
            None => None,
            Some(Ok(result)) if result.cached => Some(crate::events::TaskStatus::Cached),
            Some(Ok(_)) => Some(crate::events::TaskStatus::Completed),
            Some(Err(error)) => Some(error.status()),
        })
    }
}

/// Classify a completed plan in plan order.
///
/// `status` answers what happened to a node, or `None` when the node never
/// ran. The summary parts always add up to the plan length, and the returned
/// node is the first one that reported an error in plan order.
///
/// A cancelled task is counted in `cancelled`, not in `failed`, but it still
/// counts as the reported error. That distinction is load-bearing: an
/// interrupted run must fail, not report success. Dropping it makes Ctrl-C
/// exit `0`, because `execute_plan` then falls through to its `Ok(summary)`
/// tail instead of returning `SchedulerError::Task`.
fn classify(
    nodes: &[TaskNode],
    status: impl Fn(&TaskNode) -> Option<crate::events::TaskStatus>,
) -> (ExecutionSummary, Option<TaskNode>) {
    use crate::events::TaskStatus;

    let mut summary = ExecutionSummary::default();
    let mut first_error = None;
    for node in nodes {
        match status(node) {
            None | Some(TaskStatus::Blocked) => summary.blocked += 1,
            Some(TaskStatus::Cached) => summary.cached += 1,
            Some(TaskStatus::Completed) => summary.completed += 1,
            Some(TaskStatus::Cancelled) => {
                summary.cancelled += 1;
                first_error.get_or_insert_with(|| node.clone());
            }
            Some(TaskStatus::Failed | TaskStatus::TimedOut | TaskStatus::OutputLimit) => {
                summary.failed += 1;
                first_error.get_or_insert_with(|| node.clone());
            }
        }
    }
    assert_eq!(
        summary.completed + summary.cached + summary.failed + summary.cancelled + summary.blocked,
        nodes.len(),
        "scheduler result accounting must cover the entire plan"
    );
    (summary, first_error)
}

struct WorkerJob {
    node: TaskNode,
    task: Arc<PlannedTask>,
    cache: Arc<dyn CacheBackend>,
    sleeper: Arc<dyn RetrySleeper>,
    cache_session: Option<Arc<CacheSession>>,
    root: Arc<PathBuf>,
    can_cache: bool,
    force: bool,
    dependency_keys: Vec<String>,
    runner: Arc<dyn TaskExecutor>,
    cancellation: CancellationToken,
    output: Arc<OutputSink>,
}

fn worker_loop(
    receiver: Arc<Mutex<mpsc::Receiver<WorkerJob>>>,
    sender: mpsc::Sender<(TaskNode, WorkerReport)>,
) {
    loop {
        let job = match receiver
            .lock()
            .expect("worker queue lock is not poisoned")
            .recv()
        {
            Ok(job) => job,
            Err(_) => break,
        };
        let node = job.node.clone();
        let report = execute_task(job);
        if sender.send((node, report)).is_err() {
            break;
        }
    }
}

/// One worker's whole lifecycle: fingerprint inputs, replay a cache hit, run
/// the task, and refresh the cache entry.
fn execute_task(job: WorkerJob) -> WorkerReport {
    let WorkerJob {
        node: _,
        task,
        cache,
        sleeper,
        cache_session,
        root,
        can_cache,
        force,
        dependency_keys,
        runner,
        cancellation,
        output,
    } = job;
    let mut key = None;
    if can_cache {
        let session = cache_session.expect("cacheable tasks require a prepared cache session");
        match cache.task_key(session.as_ref(), &task, &dependency_keys) {
            Ok(computed) => key = Some(computed),
            Err(error) => return WorkerReport::CacheFailed(error),
        }
    }

    // A hit replays the stored result; `Force` deliberately skips this so the
    // task runs and refreshes its entry.
    let replayed = match (key.as_deref(), can_cache && !force) {
        (Some(computed), true) => match cache.lookup(&task, computed) {
            Ok(hit) => hit,
            Err(error) => return WorkerReport::CacheFailed(error),
        },
        _ => None,
    };
    if let Some(result) = replayed {
        return WorkerReport::Finished {
            result: Ok(result),
            key,
            cache_error: None,
        };
    }

    let mut attempt = 0;
    let output_callback = if output.is_live() {
        let output = Arc::clone(&output);
        let node = task.node();
        Some(Arc::new(move |stream, bytes: &[u8]| {
            let stream = if stream == "stderr" {
                crate::events::TaskStream::Stderr
            } else {
                crate::events::TaskStream::Stdout
            };
            output.present_live_output(&node, stream, bytes.to_vec())
        }) as crate::runner::OutputCallback)
    } else {
        None
    };
    let result = loop {
        if let Err(error) =
            output.present_attempt(&task.node(), attempt + 1, task.retries().saturating_add(1))
        {
            return WorkerReport::OutputFailed(error);
        }
        let task_cancellation = (!task.is_finalizer()).then_some(&cancellation);
        match runner.execute(&root, &task, task_cancellation, output_callback.clone()) {
            Ok(result) => break Ok(result),
            Err(_error) if attempt < task.retries() => {
                attempt += 1;
                if task.retry_backoff() > std::time::Duration::ZERO {
                    sleeper.sleep(task.retry_backoff());
                }
            }
            Err(error) => break Err(error),
        }
    };

    match result {
        Ok(result) => {
            let cache_error = key
                .as_deref()
                .and_then(|computed| cache.store(&task, computed, &result).err());
            WorkerReport::Finished {
                result: Ok(result),
                key,
                cache_error,
            }
        }
        Err(error) => WorkerReport::Finished {
            result: Err(error),
            key,
            cache_error: None,
        },
    }
}

/// One worker's report to the scheduling loop.
enum WorkerReport {
    /// The worker finished, with either a task result or a runner failure.
    /// `cache_error` records a successful run whose cache entry could not be
    /// written; it is `None` when `result` itself is an error.
    Finished {
        result: Result<TaskResult, RunnerError>,
        key: Option<String>,
        cache_error: Option<CacheError>,
    },
    /// Cache state could not be computed or read, so the task never started.
    CacheFailed(CacheError),
    /// The output renderer failed before the task attempt could start.
    OutputFailed(std::io::Error),
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExecutionSummary {
    pub(crate) completed: usize,
    pub(crate) cached: usize,
    pub(crate) failed: usize,
    pub(crate) cancelled: usize,
    pub(crate) blocked: usize,
}

#[derive(Debug)]
pub enum SchedulerError {
    Task(Box<RunnerError>),
    Cache(CacheError),
    UnresolvedDependency {
        task: TaskNode,
        dependency: TaskNode,
    },
    NoReadyWork,
    Cancelled,
    Output(std::io::Error),
}

impl fmt::Display for SchedulerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Task(error) => error.fmt(f),
            Self::Cache(error) => error.fmt(f),
            Self::UnresolvedDependency { task, dependency } => write!(
                f,
                "scheduler could not resolve '{}' for '{}'",
                dependency.id, task.id
            ),
            Self::NoReadyWork => write!(f, "scheduler found no ready task in the execution plan"),
            Self::Cancelled => f.write_str("execution was cancelled"),
            Self::Output(error) => write!(f, "could not write task output: {error}"),
        }
    }
}

impl StdError for SchedulerError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Task(error) => Some(error.as_ref()),
            Self::Cache(error) => Some(error),
            Self::UnresolvedDependency { .. } | Self::NoReadyWork | Self::Cancelled => None,
            Self::Output(error) => Some(error),
        }
    }
}

impl From<RunnerError> for SchedulerError {
    fn from(error: RunnerError) -> Self {
        Self::Task(Box::new(error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::CacheBackend;
    use crate::config::config_path;
    use crate::events::TaskStatus;
    use crate::project::Project;
    use crate::runner::{CapturedOutput, RunnerError, TaskResult};
    use crate::testing::TempDir;
    use std::fs;
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::Duration;

    fn nodes(ids: &[&str]) -> Vec<TaskNode> {
        ids.iter().map(|id| TaskNode::new(*id)).collect()
    }

    fn planned(manifest: &str) -> Vec<PlannedTask> {
        let temp = TempDir::new();
        fs::write(config_path(temp.path()), manifest).expect("write manifest");
        let project = Project::load(temp.path()).expect("project loads");
        project.plan(None, &[]).expect("plan succeeds")
    }

    fn succeeded() -> Result<TaskResult, RunnerError> {
        Ok(TaskResult {
            output: CapturedOutput::default(),
            elapsed: Duration::ZERO,
            cached: false,
        })
    }

    // ----------------------------------------------------------------------
    // classify tests (pure, unchanged)
    // ----------------------------------------------------------------------

    #[test]
    fn a_finished_plan_accounts_for_every_task() {
        let plan = nodes(&["a", "b", "c"]);

        let (summary, first_error) = classify(&plan, |node| match node.id() {
            "a" => Some(TaskStatus::Completed),
            "b" => Some(TaskStatus::Cached),
            _ => None,
        });

        assert_eq!(
            summary,
            ExecutionSummary {
                completed: 1,
                cached: 1,
                failed: 0,
                cancelled: 0,
                blocked: 1,
            }
        );
        assert!(first_error.is_none());
    }

    #[test]
    fn every_failure_kind_counts_as_failed_and_the_first_one_wins() {
        let plan = nodes(&["a", "b", "c"]);

        let (summary, first_error) = classify(&plan, |node| match node.id() {
            "a" => Some(TaskStatus::TimedOut),
            "b" => Some(TaskStatus::Failed),
            _ => Some(TaskStatus::OutputLimit),
        });

        assert_eq!(summary.failed, 3);
        assert_eq!(
            first_error.map(|node| node.id().to_owned()),
            Some("a".to_owned())
        );
    }

    #[test]
    fn a_cancelled_task_is_counted_separately_and_still_reported() {
        let plan = nodes(&["a", "b"]);

        let (summary, first_error) = classify(&plan, |node| match node.id() {
            "a" => Some(TaskStatus::Cancelled),
            _ => Some(TaskStatus::Failed),
        });

        assert_eq!(summary.cancelled, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(
            first_error.map(|node| node.id().to_owned()),
            Some("a".to_owned()),
            "an interrupted run must still report an error, or Ctrl-C would exit 0"
        );
    }

    #[test]
    fn a_plan_of_only_cancelled_tasks_still_reports_an_error() {
        let plan = nodes(&["a", "b"]);

        let (summary, first_error) = classify(&plan, |_| Some(TaskStatus::Cancelled));

        assert_eq!(summary.cancelled, 2);
        assert_eq!(summary.failed, 0);
        assert_eq!(
            first_error.map(|node| node.id().to_owned()),
            Some("a".to_owned())
        );
    }

    // ----------------------------------------------------------------------
    // PlanState pure transition tests
    // ----------------------------------------------------------------------

    #[test]
    fn constructor_accepts_a_valid_plan() {
        let plan = planned(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"a\"]\n\n[tasks.a]\ncommand = [\"echo\", \"a\"]\n",
        );
        let state = PlanState::new(&plan).expect("valid plan succeeds");
        assert_eq!(state.ready.len(), 1);
        assert_eq!(state.active.len(), 0);
    }

    #[test]
    fn dispatching_reserves_a_resource_group_and_completion_releases_it() {
        let plan = planned(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"a\", \"b\"]\n\n[tasks.a]\ncommand = [\"echo\", \"a\"]\nresource_group = \"db\"\n\n[tasks.b]\ncommand = [\"echo\", \"b\"]\nresource_group = \"db\"\n",
        );
        let mut state = PlanState::new(&plan).expect("plan succeeds");

        // Both tasks initially ready; "a" is first in plan order.
        assert_eq!(state.next_ready(), Some(TaskNode::new("a")));
        assert!(state.active_groups.is_empty());

        // Dispatch "a" — it holds "db".
        state.mark_dispatched(&TaskNode::new("a"), false);
        assert!(state.active_groups.contains("db"));

        // "b" is blocked by the held group.
        assert_eq!(state.next_ready(), None);

        // Complete "a" — the group is released.
        state.complete(TaskNode::new("a"), succeeded(), None, None);
        assert!(!state.active_groups.contains("db"));

        // Now "b" is ready.
        assert_eq!(state.next_ready(), Some(TaskNode::new("b")));
    }

    #[test]
    fn normal_failure_unblocks_finalizers_but_not_new_normal_tasks() {
        let plan = planned(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\nfinally = [\"cleanup\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n\n[tasks.cleanup]\ncommand = [\"echo\", \"cleanup\"]\n",
        );
        let mut state = PlanState::new(&plan).expect("plan succeeds");

        // Nothing started — nothing is stopping, so finalizers not yet allowed.
        assert!(!state.finalizers_allowed());

        // Dispatch and fail "build".
        state.mark_dispatched(&TaskNode::new("build"), false);
        state.complete(
            TaskNode::new("build"),
            Err(RunnerError::EmptyCommand {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
            }),
            None,
            None,
        );

        // Build failed — stopping, so finalizers are allowed. Cleanup becomes ready.
        assert!(state.stopping);
        assert!(state.finalizers_allowed());
        assert_eq!(state.next_ready(), Some(TaskNode::new("cleanup")));
    }

    #[test]
    fn stopping_admits_only_finalizers() {
        let plan = planned(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\nfinally = [\"cleanup\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n\n[tasks.cleanup]\ncommand = [\"echo\", \"cleanup\"]\n",
        );
        let mut state = PlanState::new(&plan).expect("plan succeeds");

        // Start build, then fail it to enter stopping mode.
        state.mark_dispatched(&TaskNode::new("build"), false);
        state.complete(
            TaskNode::new("build"),
            Err(RunnerError::EmptyCommand {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
            }),
            None,
            None,
        );
        assert!(state.stopping);

        // Only cleanup (finalizer) should be selectable.
        assert_eq!(state.next_ready(), Some(TaskNode::new("cleanup")));
    }

    #[test]
    fn a_finalizer_waits_until_normal_work_is_done() {
        let plan = planned(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\nfinally = [\"cleanup\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n\n[tasks.cleanup]\ncommand = [\"echo\", \"cleanup\"]\n",
        );
        let mut state = PlanState::new(&plan).expect("plan succeeds");

        // "build" (normal) and "cleanup" (finalizer) are both ready (no deps).
        // Only "build" can start because finalizers are not allowed yet.
        assert_eq!(state.next_ready(), Some(TaskNode::new("build")));

        // Complete the normal task.
        state.mark_dispatched(&TaskNode::new("build"), false);
        state.complete(TaskNode::new("build"), succeeded(), None, None);

        // Now finalizers are allowed; "cleanup" becomes available.
        assert!(state.finalizers_allowed());
        assert_eq!(state.next_ready(), Some(TaskNode::new("cleanup")));
    }

    #[test]
    fn cache_failure_stops_dispatch_and_still_permits_finalizer_closure() {
        let plan = planned(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\nfinally = [\"cleanup\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n\n[tasks.cleanup]\ncommand = [\"echo\", \"cleanup\"]\n",
        );
        let mut state = PlanState::new(&plan).expect("plan succeeds");

        // Dispatch "build" and complete it with a cache error.
        state.mark_dispatched(&TaskNode::new("build"), false);
        state.complete(
            TaskNode::new("build"),
            succeeded(),
            None,
            Some(CacheError::Invalid {
                message: "disk full".to_owned(),
            }),
        );

        // Cache failure stops the run but permits finalizers.
        assert!(state.stopping);
        assert!(state.finalizers_allowed());
        assert_eq!(state.next_ready(), Some(TaskNode::new("cleanup")));
    }

    #[test]
    fn completion_classification_accounts_for_every_node_exactly_once() {
        let plan = planned(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"a\"]\n\n[tasks.a]\ncommand = [\"echo\", \"a\"]\n",
        );
        let mut state = PlanState::new(&plan).expect("plan succeeds");

        state.mark_dispatched(&TaskNode::new("a"), false);
        state.complete(TaskNode::new("a"), succeeded(), None, None);

        let (summary, first_error) = state.finish(&plan);
        assert_eq!(summary.completed, 1);
        assert_eq!(
            summary.completed
                + summary.cached
                + summary.failed
                + summary.cancelled
                + summary.blocked,
            plan.len()
        );
        assert!(first_error.is_none());
    }

    #[test]
    fn the_first_failed_node_is_selected_in_plan_order() {
        let plan = planned(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"a\", \"b\"]\n\n[tasks.a]\ncommand = [\"echo\", \"a\"]\n\n[tasks.b]\ncommand = [\"echo\", \"b\"]\ndepends_on = [\"a\"]\n",
        );
        let mut state = PlanState::new(&plan).expect("plan succeeds");

        state.mark_dispatched(&TaskNode::new("a"), false);
        state.complete(TaskNode::new("a"), succeeded(), None, None);
        state.mark_dispatched(&TaskNode::new("b"), false);
        state.complete(
            TaskNode::new("b"),
            Err(RunnerError::EmptyCommand {
                project: "fixture".to_owned(),
                task: "b".to_owned(),
            }),
            None,
            None,
        );

        let (_, first_error) = state.finish(&plan);
        assert_eq!(first_error.map(|n| n.id().to_owned()), Some("b".to_owned()));
    }

    // ----------------------------------------------------------------------
    // Errors
    // ----------------------------------------------------------------------

    #[test]
    fn scheduler_errors_expose_a_source_exactly_when_they_wrap_one() {
        let with_source = [
            SchedulerError::Task(Box::new(RunnerError::EmptyCommand {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
            })),
            SchedulerError::Cache(CacheError::Invalid {
                message: "bad entry".to_owned(),
            }),
            SchedulerError::Output(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "closed",
            )),
        ];
        for error in &with_source {
            assert!(error.source().is_some(), "{error}");
        }

        let bare = [
            SchedulerError::UnresolvedDependency {
                task: TaskNode::new("app"),
                dependency: TaskNode::new("base"),
            },
            SchedulerError::NoReadyWork,
            SchedulerError::Cancelled,
        ];
        for error in &bare {
            assert!(error.source().is_none(), "{error}");
            assert!(!error.to_string().is_empty(), "{error}");
        }
    }

    // ----------------------------------------------------------------------
    // ScriptedExecutor (unchanged)
    // ----------------------------------------------------------------------

    /// A task executor that records every call and fails a scripted set of
    /// tasks, so the scheduling loop can be driven deterministically.
    #[derive(Default)]
    struct ScriptedExecutor {
        calls: Mutex<Vec<String>>,
        failures: Mutex<BTreeSet<String>>,
    }

    impl ScriptedExecutor {
        fn failing(ids: &[&str]) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                failures: Mutex::new(ids.iter().map(|id| (*id).to_owned()).collect()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("call log is not poisoned").clone()
        }
    }

    impl TaskExecutor for ScriptedExecutor {
        fn execute(
            &self,
            _project_root: &std::path::Path,
            task: &PlannedTask,
            _cancellation: Option<&CancellationToken>,
            _output: Option<crate::runner::OutputCallback>,
        ) -> Result<TaskResult, RunnerError> {
            self.calls
                .lock()
                .expect("call log is not poisoned")
                .push(task.id().to_owned());
            if self
                .failures
                .lock()
                .expect("failure set is not poisoned")
                .contains(task.id())
            {
                return Err(RunnerError::EmptyCommand {
                    project: task.project().to_owned(),
                    task: task.id().to_owned(),
                });
            }
            succeeded()
        }
    }

    // ----------------------------------------------------------------------
    // Fake cache and sleeper services
    // ----------------------------------------------------------------------

    /// A fake cache backend that records calls and returns scripted results.
    struct ScriptedCache {
        /// Cache key to return from `task_key`.
        fixed_key: Mutex<Option<String>>,
        /// Result to return from `lookup`.
        hit: Mutex<Option<TaskResult>>,
        /// Per-method errors. Each call takes the next error for its method
        /// if present, otherwise succeeds. Indexed by method name.
        errors: Mutex<BTreeMap<&'static str, Vec<CacheError>>>,
        /// Record of every method call.
        calls: Mutex<Vec<&'static str>>,
        /// Environment snapshots received during cache preparation.
        environments: Mutex<Vec<BTreeMap<String, String>>>,
    }

    impl ScriptedCache {
        fn new() -> Self {
            Self {
                fixed_key: Mutex::new(None),
                hit: Mutex::new(None),
                errors: Mutex::new(BTreeMap::new()),
                calls: Mutex::new(Vec::new()),
                environments: Mutex::new(Vec::new()),
            }
        }

        fn with_hit(hit: TaskResult) -> Self {
            Self {
                fixed_key: Mutex::new(None),
                hit: Mutex::new(Some(hit)),
                errors: Mutex::new(BTreeMap::new()),
                calls: Mutex::new(Vec::new()),
                environments: Mutex::new(Vec::new()),
            }
        }

        fn with_store_error(error: CacheError) -> Self {
            let mut errors = BTreeMap::new();
            errors.insert("store", vec![error]);
            Self {
                fixed_key: Mutex::new(None),
                hit: Mutex::new(None),
                errors: Mutex::new(errors),
                calls: Mutex::new(Vec::new()),
                environments: Mutex::new(Vec::new()),
            }
        }

        fn with_key_error(error: CacheError) -> Self {
            let mut errors = BTreeMap::new();
            errors.insert("task_key", vec![error]);
            Self {
                fixed_key: Mutex::new(None),
                hit: Mutex::new(None),
                errors: Mutex::new(errors),
                calls: Mutex::new(Vec::new()),
                environments: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().expect("call log is not poisoned").clone()
        }

        fn environments(&self) -> Vec<BTreeMap<String, String>> {
            self.environments
                .lock()
                .expect("environment log is not poisoned")
                .clone()
        }

        fn take_error(&self, method: &'static str) -> Option<CacheError> {
            self.errors
                .lock()
                .expect("ok")
                .get_mut(method)
                .and_then(|errors| errors.pop())
        }
    }

    impl CacheBackend for ScriptedCache {
        fn prepare(
            &self,
            _project_root: &Path,
            environment: BTreeMap<String, String>,
        ) -> Result<CacheSession, CacheError> {
            self.calls.lock().expect("ok").push("prepare");
            self.environments.lock().expect("ok").push(environment);
            if let Some(error) = self.take_error("prepare") {
                return Err(error);
            }
            Ok(CacheSession::for_test())
        }

        fn task_key(
            &self,
            _session: &CacheSession,
            _task: &PlannedTask,
            _dependency_keys: &[String],
        ) -> Result<String, CacheError> {
            self.calls.lock().expect("ok").push("task_key");
            if let Some(error) = self.take_error("task_key") {
                return Err(error);
            }
            let key = self
                .fixed_key
                .lock()
                .expect("ok")
                .clone()
                .unwrap_or_else(|| "abc123".to_owned());
            Ok(key)
        }

        fn lookup(
            &self,
            _task: &PlannedTask,
            _key: &str,
        ) -> Result<Option<TaskResult>, CacheError> {
            self.calls.lock().expect("ok").push("lookup");
            if let Some(error) = self.take_error("lookup") {
                return Err(error);
            }
            Ok(self.hit.lock().expect("ok").clone())
        }

        fn store(
            &self,
            _task: &PlannedTask,
            _key: &str,
            _result: &TaskResult,
        ) -> Result<(), CacheError> {
            self.calls.lock().expect("ok").push("store");
            if let Some(error) = self.take_error("store") {
                return Err(error);
            }
            Ok(())
        }
    }

    /// A sleeper that records durations without actually sleeping.
    #[derive(Default)]
    struct RecordingSleeper {
        sleeps: Mutex<Vec<Duration>>,
    }

    impl RecordingSleeper {
        fn sleeps(&self) -> Vec<Duration> {
            self.sleeps.lock().expect("ok").clone()
        }
    }

    impl RetrySleeper for RecordingSleeper {
        fn sleep(&self, duration: Duration) {
            self.sleeps.lock().expect("ok").push(duration);
        }
    }

    // ----------------------------------------------------------------------
    // Helpers
    // ----------------------------------------------------------------------

    /// A sink whose `Write` always fails, standing in for a hung-up pipe.
    struct FailingWriter;

    impl std::io::Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "consumer hung up",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn terminal_sink() -> Arc<OutputSink> {
        Arc::new(OutputSink::test_sink(
            crate::output::OutputMode::Terminal,
            1,
            Box::new(Vec::new()),
            Box::new(Vec::new()),
        ))
    }

    fn project_and_plan(manifest: &str) -> (TempDir, Project, Vec<PlannedTask>) {
        let temp = TempDir::new();
        fs::write(config_path(temp.path()), manifest).expect("write manifest");
        let project = Project::load(temp.path()).expect("project loads");
        let plan = project.plan(None, &[]).expect("plan succeeds");
        (temp, project, plan)
    }

    fn run_with_script(
        manifest: &str,
        jobs: usize,
        executor: &Arc<ScriptedExecutor>,
        output: &Arc<OutputSink>,
        cache_mode: CacheMode,
        cancellation: &CancellationToken,
    ) -> Result<ExecutionSummary, SchedulerError> {
        let (_temp, project, plan) = project_and_plan(manifest);
        // The temp project must outlive the call, so keep `_temp` in scope.
        execute_plan(
            &project,
            &plan,
            jobs,
            Arc::clone(executor) as Arc<dyn TaskExecutor>,
            output,
            cache_mode,
            cancellation,
        )
    }

    /// Run a plan with fake cache and sleeper services.
    #[allow(clippy::too_many_arguments)]
    fn run_with_fakes(
        manifest: &str,
        jobs: usize,
        executor: &Arc<ScriptedExecutor>,
        output: &Arc<OutputSink>,
        cache_mode: CacheMode,
        cancellation: &CancellationToken,
        cache: Arc<dyn CacheBackend>,
        sleeper: Arc<dyn RetrySleeper>,
        environment: BTreeMap<String, String>,
    ) -> Result<ExecutionSummary, SchedulerError> {
        let (_temp, project, plan) = project_and_plan(manifest);
        let services = SchedulerServices {
            cache,
            sleeper,
            environment,
        };
        let options = SchedulerOptions {
            jobs,
            cache_mode,
            cancellation,
            services: &services,
        };
        execute_plan_with_services(
            &project,
            &plan,
            Arc::clone(executor) as Arc<dyn TaskExecutor>,
            output,
            &options,
        )
    }

    // ----------------------------------------------------------------------
    // Existing scheduler integration tests (unchanged semantics)
    // ----------------------------------------------------------------------

    #[test]
    fn a_chain_runs_in_dependency_order_and_accounts_for_every_task() {
        let executor = Arc::new(ScriptedExecutor::default());
        let summary = run_with_script(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"app\"]\n\n[tasks.base]\ncommand = [\"echo\", \"base\"]\n\n[tasks.app]\ncommand = [\"echo\", \"app\"]\ndepends_on = [\"base\"]\n",
            1,
            &executor,
            &terminal_sink(),
            CacheMode::NoCache,
            &CancellationToken::new(),
        )
        .expect("the chain succeeds");

        assert_eq!(executor.calls(), vec!["base".to_owned(), "app".to_owned()]);
        assert_eq!(summary.completed, 2);
        assert_eq!(summary.failed + summary.cancelled + summary.blocked, 0);
    }

    #[test]
    fn a_failed_task_stops_normal_work_but_still_runs_finalizers() {
        let executor = Arc::new(ScriptedExecutor::failing(&["build"]));
        let error = run_with_script(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\nfinally = [\"cleanup\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n\n[tasks.cleanup]\ncommand = [\"echo\", \"cleanup\"]\n",
            1,
            &executor,
            &terminal_sink(),
            CacheMode::NoCache,
            &CancellationToken::new(),
        )
        .expect_err("a failed task is the reported error");

        assert!(matches!(error, SchedulerError::Task(_)), "{error}");
        assert_eq!(
            executor.calls(),
            vec!["build".to_owned(), "cleanup".to_owned()],
            "the finalizer must run after a normal failure"
        );
    }

    #[test]
    fn a_cancelled_run_blocks_normal_work_and_reports_cancelled() {
        let executor = Arc::new(ScriptedExecutor::default());
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = run_with_script(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n",
            1,
            &executor,
            &terminal_sink(),
            CacheMode::NoCache,
            &cancellation,
        )
        .expect_err("a cancelled run is an error");

        assert!(matches!(error, SchedulerError::Cancelled), "{error}");
        assert!(
            executor.calls().is_empty(),
            "a cancelled task must never reach the executor"
        );
    }

    #[test]
    fn an_output_failure_is_reported_before_any_task_runs() {
        let executor = Arc::new(ScriptedExecutor::default());
        let output: Arc<OutputSink> = Arc::new(OutputSink::test_sink(
            crate::output::OutputMode::Terminal,
            1,
            Box::new(FailingWriter),
            Box::new(FailingWriter),
        ));

        let error = run_with_script(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n",
            1,
            &executor,
            &output,
            CacheMode::NoCache,
            &CancellationToken::new(),
        )
        .expect_err("a renderer failure stops the run");

        assert!(matches!(error, SchedulerError::Output(_)), "{error}");
        assert!(executor.calls().is_empty());
    }

    #[test]
    fn a_cache_failure_is_reported_before_the_task_runs() {
        let executor = Arc::new(ScriptedExecutor::default());
        let error = run_with_script(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\ncache = true\ninputs = [\"missing/**\"]\noutputs = [\"out/**\"]\n",
            1,
            &executor,
            &terminal_sink(),
            CacheMode::ReadWrite,
            &CancellationToken::new(),
        )
        .expect_err("an unmatched input pattern is a cache failure");

        assert!(matches!(error, SchedulerError::Cache(_)), "{error}");
        assert!(
            executor.calls().is_empty(),
            "the task must not run when its key cannot be computed"
        );
    }

    #[test]
    fn independent_tasks_run_once_each_at_one_worker() {
        let executor = Arc::new(ScriptedExecutor::default());
        let summary = run_with_script(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"a\", \"b\", \"c\"]\n\n[tasks.a]\ncommand = [\"echo\", \"a\"]\n\n[tasks.b]\ncommand = [\"echo\", \"b\"]\n\n[tasks.c]\ncommand = [\"echo\", \"c\"]\n",
            1,
            &executor,
            &terminal_sink(),
            CacheMode::NoCache,
            &CancellationToken::new(),
        )
        .expect("independent tasks succeed");

        let mut calls = executor.calls();
        calls.sort();
        assert_eq!(calls, vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]);
        assert_eq!(summary.completed, 3);
    }

    // ----------------------------------------------------------------------
    // Service-path tests (deterministic, no real filesystem/sleep)
    // ----------------------------------------------------------------------

    #[test]
    fn a_cache_hit_returns_the_cached_result_and_never_calls_the_executor() {
        let executor = Arc::new(ScriptedExecutor::default());
        let cache = Arc::new(ScriptedCache::with_hit(TaskResult {
            output: CapturedOutput::default(),
            elapsed: Duration::ZERO,
            cached: true,
        }));

        let summary = run_with_fakes(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\ncache = true\ninputs = [\"src/**\"]\noutputs = [\"out/**\"]\n",
            1,
            &executor,
            &terminal_sink(),
            CacheMode::ReadWrite,
            &CancellationToken::new(),
            cache as Arc<dyn CacheBackend>,
            Arc::new(RecordingSleeper::default()),
            BTreeMap::new(),
        )
        .expect("cache hit succeeds");

        assert!(
            executor.calls().is_empty(),
            "a cache hit must not invoke the task executor"
        );
        assert_eq!(summary.cached, 1);
        assert_eq!(summary.completed, 0);
    }

    #[test]
    fn a_key_failure_returns_cache_error_before_the_executor_runs() {
        let executor = Arc::new(ScriptedExecutor::default());
        let cache = Arc::new(ScriptedCache::with_key_error(CacheError::Invalid {
            message: "key computation failed".to_owned(),
        }));

        let error = run_with_fakes(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\ncache = true\ninputs = [\"src/**\"]\noutputs = [\"out/**\"]\n",
            1,
            &executor,
            &terminal_sink(),
            CacheMode::ReadWrite,
            &CancellationToken::new(),
            cache as Arc<dyn CacheBackend>,
            Arc::new(RecordingSleeper::default()),
            BTreeMap::new(),
        )
        .expect_err("a key failure must return a cache error");

        assert!(matches!(error, SchedulerError::Cache(_)), "{error}");
        assert!(
            executor.calls().is_empty(),
            "a key failure must not invoke the task executor"
        );
    }

    #[test]
    fn a_store_failure_reports_cache_error_after_the_task_result_is_presented() {
        let executor = Arc::new(ScriptedExecutor::default());
        let cache = Arc::new(ScriptedCache::with_store_error(CacheError::Invalid {
            message: "store failed".to_owned(),
        }));

        let error = run_with_fakes(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\ncache = true\ninputs = [\"src/**\"]\noutputs = [\"out/**\"]\n",
            1,
            &executor,
            &terminal_sink(),
            CacheMode::ReadWrite,
            &CancellationToken::new(),
            cache as Arc<dyn CacheBackend>,
            Arc::new(RecordingSleeper::default()),
            BTreeMap::new(),
        )
        .expect_err("a store failure must return a cache error");

        assert!(matches!(error, SchedulerError::Cache(_)), "{error}");
        // The executor runs because the task must complete before storing.
        // We expect it to have been called.
        assert_eq!(executor.calls(), vec!["build"]);
    }

    #[test]
    fn a_task_with_retries_invokes_the_executor_repeatedly_with_backoff() {
        let executor = Arc::new(ScriptedExecutor::failing(&["build"]));
        let sleeper = Arc::new(RecordingSleeper::default());

        let error = run_with_fakes(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\nretries = 2\nretry_backoff_seconds = 1\n",
            1,
            &executor,
            &terminal_sink(),
            CacheMode::NoCache,
            &CancellationToken::new(),
            Arc::new(ScriptedCache::new()) as Arc<dyn CacheBackend>,
            Arc::clone(&sleeper) as Arc<dyn RetrySleeper>,
            BTreeMap::new(),
        )
        .expect_err("a failing task is the reported error");

        assert!(matches!(error, SchedulerError::Task(_)), "{error}");
        // 3 attempts = 1 initial + 2 retries
        assert_eq!(executor.calls(), vec!["build", "build", "build"]);
        // 2 backoff sleeps = configured backoff each retry
        let sleeps = sleeper.sleeps();
        assert_eq!(sleeps.len(), 2, "expected exactly two backoff sleeps");
        for d in &sleeps {
            assert_eq!(*d, Duration::from_secs(1));
        }
    }

    #[test]
    fn nocache_does_not_call_the_fake_cache_at_all() {
        let executor = Arc::new(ScriptedExecutor::default());
        let cache = Arc::new(ScriptedCache::new());

        let summary = run_with_fakes(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\ncache = true\ninputs = [\"src/**\"]\noutputs = [\"out/**\"]\n",
            1,
            &executor,
            &terminal_sink(),
            CacheMode::NoCache,
            &CancellationToken::new(),
            cache.clone() as Arc<dyn CacheBackend>,
            Arc::new(RecordingSleeper::default()),
            BTreeMap::new(),
        )
        .expect("NoCache succeeds");

        assert_eq!(summary.completed, 1);
        assert_eq!(summary.cached, 0);
        assert!(
            cache.calls().is_empty(),
            "NoCache must not call any cache method"
        );
    }

    #[test]
    fn the_fixed_environment_map_is_passed_to_cache_preparation() {
        let executor = Arc::new(ScriptedExecutor::default());
        let cache = Arc::new(ScriptedCache::new());
        let environment = BTreeMap::from([
            ("CI".to_owned(), "true".to_owned()),
            ("BRANCH".to_owned(), "main".to_owned()),
        ]);

        // Use ReadWrite cache mode so prepare is called
        let _ = run_with_fakes(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\ncache = true\ninputs = [\"src/**\"]\noutputs = [\"out/**\"]\n",
            1,
            &executor,
            &terminal_sink(),
            CacheMode::ReadWrite,
            &CancellationToken::new(),
            Arc::clone(&cache) as Arc<dyn CacheBackend>,
            Arc::new(RecordingSleeper::default()),
            environment.clone(),
        );

        assert_eq!(cache.environments(), vec![environment]);
    }
}
