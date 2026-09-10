//! Dependency-aware bounded task scheduling.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::error::Error as StdError;
use std::fmt;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

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
) -> Result<(), SchedulerError> {
    if plan.is_empty() {
        return Ok(());
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
    let mut stopping = false;
    let root = workspace.root.clone();

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

        if result.is_err() {
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
    for task in plan {
        let node = task.node();
        let Some(result) = results.remove(&node) else {
            continue;
        };
        match result {
            Ok(result) => output
                .present_success(&node, &result)
                .map_err(SchedulerError::Output)?,
            Err(error) => {
                output
                    .present_failure(&node, &error)
                    .map_err(SchedulerError::Output)?;
                first_error.get_or_insert(error);
            }
        }
    }

    first_error.map_or(Ok(()), |error| Err(SchedulerError::Task(Box::new(error))))
}

#[derive(Debug)]
pub enum SchedulerError {
    Task(Box<RunnerError>),
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
