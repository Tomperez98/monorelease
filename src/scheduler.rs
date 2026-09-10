//! Dependency-aware bounded task scheduling.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::error::Error as StdError;
use std::fmt;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use crate::cache::{CacheError, CacheMode, CacheStore};
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
    if plan.is_empty() {
        return Ok(ExecutionSummary::default());
    }

    let tasks = plan
        .iter()
        .map(|task| (task.node(), task.clone()))
        .collect::<BTreeMap<_, _>>();
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
    let (sender, receiver) = mpsc::channel::<(TaskNode, Result<TaskResult, RunnerError>)>();
    let mut active = HashMap::<TaskNode, JoinHandle<()>>::new();
    let mut active_groups = HashSet::<String>::new();
    let mut results = BTreeMap::<TaskNode, Result<TaskResult, RunnerError>>::new();
    let mut task_keys = BTreeMap::<TaskNode, String>::new();
    let mut cacheable = BTreeMap::<TaskNode, bool>::new();
    let mut cache_error = None;
    let mut output_error = None;
    let mut stopping = false;
    let root = workspace.root.clone();
    let cache = CacheStore::new(&root);

    while !active.is_empty() || (!stopping && !ready.is_empty()) {
        while !stopping && active.len() < jobs {
            let Some(node) = ready.iter().find_map(|node| {
                let task = tasks
                    .get(node)
                    .expect("ready task must exist in the validated plan");
                let available = match task.resource_group() {
                    Some(group) => !active_groups.contains(group),
                    None => true,
                };
                available.then_some(node.clone())
            }) else {
                break;
            };
            ready.remove(&node);
            let task = tasks
                .get(&node)
                .expect("ready task must exist in the validated plan")
                .clone();
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
            let key = if can_cache {
                let dependency_keys = task
                    .depends_on()
                    .iter()
                    .map(|dependency| {
                        task_keys
                            .get(dependency)
                            .expect("cacheable dependency must have a task key")
                            .clone()
                    })
                    .collect::<Vec<_>>();
                match cache.task_key(&root, &task, &dependency_keys) {
                    Ok(key) => Some(key),
                    Err(error) => {
                        cache_error = Some(error);
                        stopping = true;
                        None
                    }
                }
            } else {
                None
            };
            if stopping {
                break;
            }
            cacheable.insert(node.clone(), can_cache);
            if let Some(key) = key.as_ref() {
                task_keys.insert(node.clone(), key.clone());
            }

            if let Some(key) = key {
                if matches!(cache_mode, CacheMode::Force) {
                    // Force still refreshes successful cache entries after execution.
                } else {
                    match cache.lookup(&task, &key) {
                        Ok(Some(result)) => {
                            let sender = sender.clone();
                            let worker_node = node.clone();
                            let handle = thread::spawn(move || {
                                sender
                                .send((worker_node, Ok(result)))
                                .expect("scheduler receiver remains alive while cache results are processed");
                            });
                            active.insert(node, handle);
                            continue;
                        }
                        Ok(None) => {}
                        Err(error) => {
                            cache_error = Some(error);
                            stopping = true;
                            break;
                        }
                    }
                }
            }

            let resource_group = task.resource_group().map(str::to_owned);
            let sender = sender.clone();
            let runner = runner.clone();
            let root = root.clone();
            let worker_node = node.clone();
            let handle = thread::spawn(move || {
                let result = runner.run(&root, &task);
                sender
                    .send((worker_node, result))
                    .expect("scheduler receiver remains alive while workers run");
            });
            if let Some(group) = resource_group {
                active_groups.insert(group);
            }
            active.insert(node, handle);
        }

        if active.is_empty() {
            if cache_error.is_some() || output_error.is_some() {
                break;
            }
            return Err(SchedulerError::NoReadyWork);
        }

        let (node, result) = receiver
            .recv()
            .expect("scheduler workers always send one completion result");
        let handle = active.remove(&node).expect("completed task must be active");
        handle.join().expect("task worker panicked");
        if let Some(group) = tasks
            .get(&node)
            .expect("completed task must exist in the validated plan")
            .resource_group()
        {
            active_groups.remove(group);
        }

        if result.is_ok()
            && cacheable.get(&node).copied().unwrap_or(false)
            && !matches!(cache_mode, CacheMode::NoCache)
        {
            let key = task_keys
                .get(&node)
                .expect("cacheable task must have a task key");
            if let Ok(task_result) = &result
                && let Err(error) = cache.store(
                    tasks
                        .get(&node)
                        .expect("completed task must exist in the validated plan"),
                    key,
                    task_result,
                )
            {
                cache_error = Some(error);
                stopping = true;
            }
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
