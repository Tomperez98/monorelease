//! Dependency-aware bounded task scheduling.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::error::Error as StdError;
use std::fmt;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::cache::{CacheError, CacheMode, CacheSession, CacheStore};
use crate::output::OutputSink;
use crate::project::{PlannedTask, Project, TaskNode};
use crate::runner::{CancellationToken, Runner, RunnerError, TaskResult};

/// Execute a validated plan while starting newly-ready tasks immediately.
pub(crate) fn execute_plan(
    project: &Project,
    plan: &[PlannedTask],
    jobs: usize,
    runner: &Runner,
    output: &Arc<OutputSink>,
    cache_mode: CacheMode,
    cancellation: &CancellationToken,
) -> Result<ExecutionSummary, SchedulerError> {
    assert!(jobs > 0, "scheduler requires at least one worker");

    let mut tasks = BTreeMap::new();
    for task in plan {
        let node = task.node();
        assert!(
            tasks.insert(node, Arc::new(task.clone())).is_none(),
            "validated plan contains duplicate task nodes"
        );
    }
    let mut remaining_dependencies = tasks
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

    let mut ready = remaining_dependencies
        .iter()
        .filter_map(|(node, count)| (*count == 0).then_some(node.clone()))
        .collect::<BTreeSet<_>>();
    let root = Arc::new(project.root.clone());
    let cache = CacheStore::new(&root);
    let cache_session =
        if !matches!(cache_mode, CacheMode::NoCache) && plan.iter().any(PlannedTask::cache) {
            Some(Arc::new(
                cache.prepare(&root, plan).map_err(SchedulerError::Cache)?,
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
    let mut active = HashSet::<TaskNode>::new();
    let mut active_groups = HashSet::<String>::new();
    let mut results = BTreeMap::<TaskNode, Result<TaskResult, RunnerError>>::new();
    let mut task_keys = BTreeMap::<TaskNode, String>::new();
    let mut cacheable = BTreeMap::<TaskNode, bool>::new();
    let mut cache_error = None;
    let mut output_error = None;
    let mut stopping = cancellation.is_cancelled();

    while !active.is_empty()
        || (!ready.is_empty() && (!stopping || has_ready_finalizer(&ready, &tasks)))
    {
        assert!(active.len() <= jobs, "scheduler exceeded its worker limit");
        while active.len() < jobs {
            if cancellation.is_cancelled() {
                stopping = true;
            }
            let finalizers_ready = finalizers_allowed(&tasks, &results, &active, stopping);
            let Some(node) = next_ready(&ready, &tasks, &active_groups, stopping, finalizers_ready)
            else {
                break;
            };
            ready.remove(&node);
            let task = tasks
                .get(&node)
                .expect("ready task must exist in the validated plan");
            if let Err(error) = output.present_start(&node) {
                output_error = Some(error);
                stopping = true;
                break;
            }
            let dependencies_cacheable = task
                .depends_on()
                .iter()
                .all(|dependency| cacheable.get(dependency).copied().unwrap_or(false));
            let can_cache =
                task.cache() && dependencies_cacheable && !matches!(cache_mode, CacheMode::NoCache);
            let dependency_keys = if can_cache {
                task.depends_on()
                    .iter()
                    .map(|dependency| {
                        task_keys
                            .get(dependency)
                            .expect("cacheable dependency must have a task key")
                            .clone()
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            cacheable.insert(node.clone(), can_cache);
            if let Some(group) = task.resource_group() {
                active_groups.insert(group.to_owned());
            }

            // Fingerprinting inputs, replaying a cache hit, and copying
            // artifacts all happen in the worker. The scheduling loop stays
            // cheap, so `--jobs` parallelizes the work that costs real time
            // instead of serializing it behind task dispatch.
            let task = Arc::clone(task);
            job_sender
                .send(WorkerJob {
                    node: node.clone(),
                    task: Arc::clone(&task),
                    cache: cache.clone(),
                    cache_session: cache_session.clone(),
                    root: Arc::clone(&root),
                    can_cache,
                    force,
                    dependency_keys,
                    runner: runner.clone(),
                    cancellation: cancellation.clone(),
                    output: Arc::clone(output),
                })
                .expect("worker pool remains alive while tasks are dispatched");
            active.insert(node);
            assert!(active.len() <= jobs, "scheduler exceeded its worker limit");
        }

        if active.is_empty() {
            if cache_error.is_some() || output_error.is_some() {
                break;
            }
            break;
        }

        let (node, report) = receiver
            .recv()
            .expect("scheduler workers always send one completion result");
        assert!(active.remove(&node), "completed task must be active");
        if let Some(group) = tasks
            .get(&node)
            .expect("completed task must exist in the validated plan")
            .resource_group()
        {
            active_groups.remove(group);
        }

        let (result, key, store_error) = match report {
            WorkerReport::Finished {
                result,
                key,
                cache_error,
            } => (result, key, cache_error),
            WorkerReport::CacheFailed(error) => {
                cache_error = Some(error);
                stopping = true;
                if let Some(children) = dependents.get(&node) {
                    for child in children {
                        if tasks
                            .get(child)
                            .expect("dependent must exist in the validated plan")
                            .is_finalizer()
                        {
                            let count = remaining_dependencies
                                .get_mut(child)
                                .expect("dependent must exist in the validated plan");
                            *count -= 1;
                            if *count == 0 {
                                ready.insert(child.clone());
                            }
                        }
                    }
                }
                continue;
            }
            WorkerReport::OutputFailed(error) => {
                output_error = Some(error);
                stopping = true;
                continue;
            }
        };
        if let Some(key) = key {
            task_keys.insert(node.clone(), key);
        }
        if let Some(error) = store_error {
            cache_error = Some(error);
            stopping = true;
        }

        if result.is_err() || cache_error.is_some() {
            stopping = true;
        }
        if let Some(children) = dependents.get(&node) {
            for child in children {
                let child_is_finalizer = tasks
                    .get(child)
                    .expect("dependent must exist in the validated plan")
                    .is_finalizer();
                if result.is_ok() && cache_error.is_none() || child_is_finalizer {
                    let count = remaining_dependencies
                        .get_mut(child)
                        .expect("dependent must exist in the validated plan");
                    *count -= 1;
                    if *count == 0 {
                        ready.insert(child.clone());
                    }
                }
            }
        }
        results.insert(node, result);
    }

    drop(job_sender);
    for worker in workers {
        worker.join().expect("scheduler worker panicked");
    }

    if active.is_empty() && !stopping && ready.is_empty() && results.len() < plan.len() {
        return Err(SchedulerError::NoReadyWork);
    }

    let nodes = plan.iter().map(PlannedTask::node).collect::<Vec<_>>();
    let (summary, first_error_node) = classify(&nodes, |node| match results.get(node) {
        None => None,
        Some(Ok(result)) if result.cached => Some(crate::events::TaskStatus::Cached),
        Some(Ok(_)) => Some(crate::events::TaskStatus::Completed),
        Some(Err(error)) => Some(error.status()),
    });
    for task in plan {
        let node = task.node();
        match results.get(&node) {
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
    let first_error = first_error_node.and_then(|node| match results.remove(&node) {
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

/// Pick the lowest-numbered ready task that no active resource group blocks.
///
/// Every ready task is available while no group is held, which is the common
/// case, so the scan only runs to find work that a held group is holding back.
fn next_ready(
    ready: &BTreeSet<TaskNode>,
    tasks: &BTreeMap<TaskNode, Arc<PlannedTask>>,
    active_groups: &HashSet<String>,
    stopping: bool,
    finalizers_allowed: bool,
) -> Option<TaskNode> {
    let available = |node: &TaskNode| {
        let task = tasks
            .get(node)
            .expect("ready task must exist in the validated plan");
        if stopping && !task.is_finalizer() {
            return false;
        }
        if task.is_finalizer() && !finalizers_allowed {
            return false;
        }
        match task.resource_group() {
            Some(group) => !active_groups.contains(group),
            None => true,
        }
    };
    ready.iter().find(|node| available(node)).cloned()
}

fn has_ready_finalizer(
    ready: &BTreeSet<TaskNode>,
    tasks: &BTreeMap<TaskNode, Arc<PlannedTask>>,
) -> bool {
    ready.iter().any(|node| {
        tasks
            .get(node)
            .expect("ready task must exist in the validated plan")
            .is_finalizer()
    })
}

/// Whether a finalizer may start now.
///
/// While normal work is running, finalizers wait until every normal task has
/// a result. Once the run is stopping, they wait only for the normal tasks
/// still in flight.
fn finalizers_allowed(
    tasks: &BTreeMap<TaskNode, Arc<PlannedTask>>,
    results: &BTreeMap<TaskNode, Result<TaskResult, RunnerError>>,
    active: &HashSet<TaskNode>,
    stopping: bool,
) -> bool {
    if stopping {
        !active.iter().any(|node| {
            !tasks
                .get(node)
                .expect("active task must exist in the validated plan")
                .is_finalizer()
        })
    } else {
        tasks
            .iter()
            .filter(|(_, task)| !task.is_finalizer())
            .all(|(node, _)| results.contains_key(node))
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
    cache: CacheStore,
    cache_session: Option<Arc<CacheSession>>,
    root: Arc<PathBuf>,
    can_cache: bool,
    force: bool,
    dependency_keys: Vec<String>,
    runner: Runner,
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
        match cache.task_key_with_session(session.as_ref(), &root, &task, &dependency_keys) {
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
        match runner.run_with_options(&root, &task, task_cancellation, output_callback.clone()) {
            Ok(result) => break Ok(result),
            Err(_error) if attempt < task.retries() => {
                attempt += 1;
                if task.retry_backoff() > std::time::Duration::ZERO {
                    thread::sleep(task.retry_backoff());
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
    use crate::config::config_path;
    use crate::events::TaskStatus;
    use crate::project::Project;
    use crate::runner::{CapturedOutput, RunnerError, TaskResult};
    use crate::testing::TempDir;
    use std::fs;
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

    fn task_map(plan: &[PlannedTask]) -> BTreeMap<TaskNode, Arc<PlannedTask>> {
        plan.iter()
            .map(|task| (task.node(), Arc::new(task.clone())))
            .collect()
    }

    fn succeeded() -> Result<TaskResult, RunnerError> {
        Ok(TaskResult {
            output: CapturedOutput::default(),
            elapsed: Duration::ZERO,
            cached: false,
        })
    }

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

    #[test]
    fn a_held_resource_group_blocks_its_next_task() {
        let plan = planned(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"a\", \"b\"]\n\n[tasks.a]\ncommand = [\"echo\", \"a\"]\nresource_group = \"db\"\n\n[tasks.b]\ncommand = [\"echo\", \"b\"]\nresource_group = \"db\"\n",
        );
        let tasks = task_map(&plan);
        let ready = plan.iter().map(PlannedTask::node).collect::<BTreeSet<_>>();

        assert_eq!(
            next_ready(
                &ready,
                &tasks,
                &HashSet::from(["db".to_owned()]),
                false,
                true
            ),
            None
        );
        assert_eq!(
            next_ready(&ready, &tasks, &HashSet::new(), false, true),
            Some(TaskNode::new("a"))
        );
    }

    #[test]
    fn stopping_admits_only_finalizers() {
        let plan = planned(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\nfinally = [\"cleanup\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n\n[tasks.cleanup]\ncommand = [\"echo\", \"cleanup\"]\n",
        );
        let tasks = task_map(&plan);
        let ready = plan.iter().map(PlannedTask::node).collect::<BTreeSet<_>>();

        assert_eq!(
            next_ready(&ready, &tasks, &HashSet::new(), true, true),
            Some(TaskNode::new("cleanup"))
        );
    }

    #[test]
    fn a_finalizer_waits_until_normal_work_is_done() {
        let plan = planned(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\nfinally = [\"cleanup\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n\n[tasks.cleanup]\ncommand = [\"echo\", \"cleanup\"]\n",
        );
        let tasks = task_map(&plan);
        let ready = BTreeSet::from([TaskNode::new("cleanup")]);

        assert_eq!(
            next_ready(&ready, &tasks, &HashSet::new(), false, false),
            None
        );
        assert_eq!(
            next_ready(&ready, &tasks, &HashSet::new(), false, true),
            Some(TaskNode::new("cleanup"))
        );
    }

    #[test]
    fn a_ready_finalizer_is_detected() {
        let plan = planned(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\nfinally = [\"cleanup\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n\n[tasks.cleanup]\ncommand = [\"echo\", \"cleanup\"]\n",
        );
        let tasks = task_map(&plan);

        assert!(!has_ready_finalizer(
            &BTreeSet::from([TaskNode::new("build")]),
            &tasks
        ));
        assert!(has_ready_finalizer(
            &BTreeSet::from([TaskNode::new("cleanup")]),
            &tasks
        ));
    }

    #[test]
    fn finalizers_wait_for_every_normal_task_while_running() {
        let plan = planned(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\nfinally = [\"cleanup\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n\n[tasks.cleanup]\ncommand = [\"echo\", \"cleanup\"]\n",
        );
        let tasks = task_map(&plan);
        let results = BTreeMap::new();

        assert!(!finalizers_allowed(
            &tasks,
            &results,
            &HashSet::new(),
            false
        ));

        let mut done = BTreeMap::new();
        done.insert(TaskNode::new("build"), succeeded());
        assert!(finalizers_allowed(&tasks, &done, &HashSet::new(), false));
    }
}
