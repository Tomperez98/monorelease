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
use crate::runner::{Runner, RunnerError, TaskResult};
use crate::workspace::{PlannedTask, TaskNode, Workspace};

/// Execute a validated plan while starting newly-ready tasks immediately.
pub(crate) fn execute_plan(
    workspace: &Workspace,
    plan: &[PlannedTask],
    jobs: usize,
    runner: &Runner,
    output: &OutputSink,
    cache_mode: CacheMode,
) -> Result<ExecutionSummary, SchedulerError> {
    assert!(jobs > 0, "scheduler requires at least one worker");
    if plan.is_empty() {
        return Ok(ExecutionSummary::default());
    }

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
    let root = Arc::new(workspace.root.clone());
    let cache = CacheStore::new(&root);
    let cache_session =
        if !matches!(cache_mode, CacheMode::NoCache) && plan.iter().any(PlannedTask::cache) {
            Some(Arc::new(
                cache.prepare(&root, plan).map_err(SchedulerError::Cache)?,
            ))
        } else {
            None
        };
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
    let mut stopping = false;

    while !active.is_empty() || (!stopping && !ready.is_empty()) {
        assert!(active.len() <= jobs, "scheduler exceeded its worker limit");
        while !stopping && active.len() < jobs {
            let Some(node) = next_ready(&ready, &tasks, &active_groups) else {
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
        } else if let Some(children) = dependents.get(&node) {
            for child in children {
                let count = remaining_dependencies
                    .get_mut(child)
                    .expect("dependent must exist in the validated plan");
                *count -= 1;
                if *count == 0 {
                    ready.insert(child.clone());
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

    let mut first_error = None;
    let mut summary = ExecutionSummary {
        blocked: plan.len().saturating_sub(results.len()),
        ..ExecutionSummary::default()
    };
    for task in plan {
        let node = task.node();
        let Some(result) = results.remove(&node) else {
            continue;
        };
        match result {
            Ok(result) => {
                if result.cached {
                    summary.cached += 1;
                } else {
                    summary.completed += 1;
                }
                output
                    .present_success(&node, &result)
                    .map_err(SchedulerError::Output)?;
            }
            Err(error) => {
                summary.failed += 1;
                output
                    .present_failure(&node, &error)
                    .map_err(SchedulerError::Output)?;
                first_error.get_or_insert(error);
            }
        }
    }
    assert_eq!(
        summary.completed + summary.cached + summary.failed + summary.blocked,
        plan.len(),
        "scheduler result accounting must cover the entire plan"
    );

    if let Some(error) = output_error {
        output
            .present_summary(&summary)
            .map_err(SchedulerError::Output)?;
        return Err(SchedulerError::Output(error));
    }
    if let Some(error) = cache_error {
        output
            .present_summary(&summary)
            .map_err(SchedulerError::Output)?;
        return Err(SchedulerError::Cache(error));
    }
    if let Some(error) = first_error {
        output
            .present_summary(&summary)
            .map_err(SchedulerError::Output)?;
        return Err(SchedulerError::Task(Box::new(error)));
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
) -> Option<TaskNode> {
    if active_groups.is_empty() {
        return ready.iter().next().cloned();
    }
    ready.iter().find_map(|node| {
        let task = tasks
            .get(node)
            .expect("ready task must exist in the validated plan");
        let available = match task.resource_group() {
            Some(group) => !active_groups.contains(group),
            None => true,
        };
        available.then_some(node.clone())
    })
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

    match runner.run(&root, &task) {
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
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExecutionSummary {
    pub(crate) completed: usize,
    pub(crate) cached: usize,
    pub(crate) failed: usize,
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
    Output(std::io::Error),
}

impl fmt::Display for SchedulerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Task(error) => error.fmt(f),
            Self::Cache(error) => error.fmt(f),
            Self::UnresolvedDependency { task, dependency } => write!(
                f,
                "scheduler could not resolve '{}:{}' for '{}:{}'",
                dependency.package, dependency.task, task.package, task.task
            ),
            Self::NoReadyWork => write!(f, "scheduler found no ready task in the execution plan"),
            Self::Output(error) => write!(f, "could not write task output: {error}"),
        }
    }
}

impl StdError for SchedulerError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Task(error) => Some(error.as_ref()),
            Self::Cache(error) => Some(error),
            Self::UnresolvedDependency { .. } | Self::NoReadyWork => None,
            Self::Output(error) => Some(error),
        }
    }
}

impl From<RunnerError> for SchedulerError {
    fn from(error: RunnerError) -> Self {
        Self::Task(Box::new(error))
    }
}
