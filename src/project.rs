//! Manifest validation and task-graph planning for one root project.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use crate::config::{PipelineConfig, StdinMode, TaskConfig, config_path, validate_process_value};
use crate::discovery::find_root;

/// A task identity in the single root graph.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct TaskNode {
    pub id: String,
}

/// One executable task in a deterministic task-DAG plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedTask {
    id: String,
    project: String,
    root: PathBuf,
    command: Vec<String>,
    stdin: StdinMode,
    cwd: PathBuf,
    env: BTreeMap<String, String>,
    cache: bool,
    inputs: Vec<String>,
    outputs: Vec<String>,
    cache_env: Vec<String>,
    timeout: Duration,
    max_output_bytes: usize,
    resource_group: Option<String>,
    retries: u32,
    retry_backoff_seconds: Duration,
    finalizer: bool,
    depends_on: Vec<TaskNode>,
}

impl TaskNode {
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

impl PlannedTask {
    pub fn node(&self) -> TaskNode {
        TaskNode::new(self.id.clone())
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn task(&self) -> &str {
        &self.id
    }

    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn command(&self) -> &[String] {
        &self.command
    }

    pub fn stdin(&self) -> StdinMode {
        self.stdin
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    pub fn cache(&self) -> bool {
        self.cache
    }

    pub fn inputs(&self) -> &[String] {
        &self.inputs
    }

    pub fn outputs(&self) -> &[String] {
        &self.outputs
    }

    pub fn cache_env(&self) -> &[String] {
        &self.cache_env
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn max_output_bytes(&self) -> usize {
        self.max_output_bytes
    }

    pub fn resource_group(&self) -> Option<&str> {
        self.resource_group.as_deref()
    }

    pub fn retries(&self) -> u32 {
        self.retries
    }

    pub fn retry_backoff(&self) -> Duration {
        self.retry_backoff_seconds
    }

    pub fn is_finalizer(&self) -> bool {
        self.finalizer
    }

    pub fn depends_on(&self) -> &[TaskNode] {
        &self.depends_on
    }
}

/// A complete, validated root project.
#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    pub name: String,
    pub default_pipeline: String,
    pub pipelines: BTreeMap<String, PipelineConfig>,
    pub tasks: BTreeMap<String, TaskConfig>,
}

impl Project {
    /// Find and load the nearest root `mono.toml`.
    pub fn load(start: &Path) -> Result<Self, ProjectError> {
        let discovered = find_root(start)?;
        let root = discovered.root;
        let config = discovered.config;
        let manifest_path = config_path(&root);
        validate_schema(&manifest_path, config.schema)?;

        validate_identifier(&manifest_path, "project name", &config.project.name)?;
        validate_identifier(
            &manifest_path,
            "default pipeline",
            &config.project.default_pipeline,
        )?;
        if config.pipelines.is_empty() {
            return Err(ProjectError::InvalidProject {
                message: "project must define at least one pipeline".to_owned(),
            });
        }
        for pipeline_name in config.pipelines.keys() {
            validate_identifier(&manifest_path, "pipeline name", pipeline_name)?;
        }
        for (pipeline_name, pipeline) in &config.pipelines {
            if pipeline.tasks.is_empty() && pipeline.finally.is_empty() {
                return Err(ProjectError::InvalidProject {
                    message: format!("pipeline '{pipeline_name}' has no tasks"),
                });
            }
            for task_name in pipeline.tasks.iter().chain(&pipeline.finally) {
                validate_task_reference(task_name).map_err(|message| {
                    ProjectError::InvalidManifest {
                        path: manifest_path.clone(),
                        message: format!("pipeline '{pipeline_name}': {message}"),
                    }
                })?;
            }
        }

        for (task_name, task) in &config.tasks {
            validate_task_config(&manifest_path, &root, task_name, task)?;
        }

        let project = Self {
            root,
            name: config.project.name,
            default_pipeline: config.project.default_pipeline,
            pipelines: config.pipelines,
            tasks: config.tasks,
        };
        project.validate_graph()?;
        project.validate_pipelines()?;
        project.assert_invariants();
        Ok(project)
    }

    fn assert_invariants(&self) {
        assert!(self.root.is_absolute(), "project root must be absolute");
        assert!(self.root.is_dir(), "project root must be a directory");
        assert!(!self.name.is_empty(), "project name must not be empty");
        assert!(self.pipelines.contains_key(&self.default_pipeline));
    }

    /// Produce a dependency-first plan for a pipeline or explicit task roots.
    pub fn plan(
        &self,
        pipeline: Option<&str>,
        requested_tasks: &[String],
    ) -> Result<Vec<PlannedTask>, ProjectError> {
        let mut ordered_nodes = self.plan_nodes(pipeline, requested_tasks)?;
        let mut finalizers = BTreeSet::new();
        if requested_tasks.is_empty() {
            let pipeline_name = pipeline.unwrap_or(&self.default_pipeline);
            let pipeline_config = self
                .pipelines
                .get(pipeline_name)
                .ok_or_else(|| self.unknown_pipeline(pipeline_name))?;
            let mut state = ordered_nodes
                .iter()
                .cloned()
                .map(|node| (node, VisitState::Visited))
                .collect::<BTreeMap<_, _>>();
            let mut stack = Vec::new();
            for task_name in &pipeline_config.finally {
                let roots = self.root_nodes(task_name)?;
                for root in roots {
                    let start = ordered_nodes.len();
                    self.visit_task(&root, &mut state, &mut stack, &mut ordered_nodes)?;
                    finalizers.insert(root);
                    for node in &ordered_nodes[start..] {
                        finalizers.insert(node.clone());
                    }
                }
            }
        }

        ordered_nodes
            .into_iter()
            .map(|node| {
                let is_finalizer = finalizers.contains(&node);
                self.planned_task(node, is_finalizer)
            })
            .collect()
    }

    pub fn graph(
        &self,
        pipeline: Option<&str>,
        requested_tasks: &[String],
    ) -> Result<Vec<(TaskNode, Vec<TaskNode>)>, ProjectError> {
        let nodes = self.plan_nodes(pipeline, requested_tasks)?;
        nodes
            .into_iter()
            .map(|node| {
                let dependencies =
                    self.resolve_task_refs(&node, &self.task_config(&node)?.depends_on)?;
                Ok((node, dependencies))
            })
            .collect()
    }

    fn plan_nodes(
        &self,
        pipeline: Option<&str>,
        requested_tasks: &[String],
    ) -> Result<Vec<TaskNode>, ProjectError> {
        let task_names: Vec<String> = if requested_tasks.is_empty() {
            let name = pipeline.unwrap_or(&self.default_pipeline);
            self.pipelines
                .get(name)
                .ok_or_else(|| self.unknown_pipeline(name))?
                .tasks
                .clone()
        } else {
            requested_tasks.to_vec()
        };
        if task_names.is_empty() {
            return Err(ProjectError::InvalidProject {
                message: "the selected pipeline has no tasks".to_owned(),
            });
        }

        let mut roots = BTreeSet::new();
        for task_name in task_names {
            roots.extend(self.root_nodes(&task_name)?);
        }
        let mut state = BTreeMap::new();
        let mut ordered = Vec::new();
        let mut stack = Vec::new();
        for root in roots {
            self.visit_task(&root, &mut state, &mut stack, &mut ordered)?;
        }
        Ok(ordered)
    }

    fn root_nodes(&self, task_name: &str) -> Result<Vec<TaskNode>, ProjectError> {
        let base = base_task_name(task_name).map_err(|_| ProjectError::InvalidTaskReference {
            reference: task_name.to_owned(),
            from: TaskNode::new(task_name),
        })?;
        let task = self
            .tasks
            .get(base)
            .ok_or_else(|| self.missing_task(task_name))?;
        let dimensions =
            task_dimensions(task_name).map_err(|_| ProjectError::InvalidTaskReference {
                reference: task_name.to_owned(),
                from: TaskNode::new(task_name),
            })?;
        if !dimensions.is_empty() {
            validate_matrix_instance(&task.matrix, &dimensions).map_err(|message| {
                ProjectError::InvalidTask {
                    task: task_name.to_owned(),
                    message,
                }
            })?;
            return Ok(vec![TaskNode::new(task_name)]);
        }
        let instances = matrix_instances(&task.matrix, &BTreeMap::new()).map_err(|message| {
            ProjectError::InvalidTask {
                task: task_name.to_owned(),
                message,
            }
        })?;
        Ok(instances
            .into_iter()
            .map(|instance| TaskNode::new(format_task_instance(base, &instance)))
            .collect())
    }

    fn validate_graph(&self) -> Result<(), ProjectError> {
        for task_name in self.tasks.keys() {
            if task_name.is_empty() || task_name.contains(['[', ']', '=', ',']) {
                return Err(ProjectError::InvalidTaskName {
                    task: task_name.clone(),
                });
            }
        }
        for task_name in self.tasks.keys() {
            let node = TaskNode::new(task_name.clone());
            for dependency in &self.tasks[task_name].depends_on {
                self.resolve_task_refs(&node, std::slice::from_ref(dependency))?;
            }
        }
        Ok(())
    }

    fn validate_pipelines(&self) -> Result<(), ProjectError> {
        if !self.pipelines.contains_key(&self.default_pipeline) {
            return Err(self.unknown_pipeline(&self.default_pipeline));
        }
        for pipeline_name in self.pipelines.keys() {
            self.plan(Some(pipeline_name), &[])?;
        }
        Ok(())
    }

    fn visit_task(
        &self,
        node: &TaskNode,
        state: &mut BTreeMap<TaskNode, VisitState>,
        stack: &mut Vec<TaskNode>,
        order: &mut Vec<TaskNode>,
    ) -> Result<(), ProjectError> {
        match state.get(node) {
            Some(VisitState::Visited) => return Ok(()),
            Some(VisitState::Visiting) => {
                let start = stack
                    .iter()
                    .position(|current| current == node)
                    .expect("visiting task is on stack");
                let mut path = stack[start..].to_vec();
                path.push(node.clone());
                return Err(ProjectError::TaskCycle { path });
            }
            None => {}
        }
        let task = self.task_config(node)?;
        state.insert(node.clone(), VisitState::Visiting);
        stack.push(node.clone());
        let dependencies = self.resolve_task_refs(node, &task.depends_on)?;
        for dependency in dependencies {
            self.visit_task(&dependency, state, stack, order)?;
        }
        stack.pop().expect("visiting task is present on stack");
        state.insert(node.clone(), VisitState::Visited);
        order.push(node.clone());
        Ok(())
    }

    fn planned_task(&self, node: TaskNode, finalizer: bool) -> Result<PlannedTask, ProjectError> {
        let base = base_task_name(&node.id)
            .map_err(|_| ProjectError::InvalidTaskReference {
                reference: node.id.clone(),
                from: node.clone(),
            })?
            .to_owned();
        let task = self
            .tasks
            .get(&base)
            .ok_or_else(|| self.missing_task(&node.id))?;
        let dimensions =
            task_dimensions(&node.id).map_err(|_| ProjectError::InvalidTaskReference {
                reference: node.id.clone(),
                from: node.clone(),
            })?;
        validate_matrix_instance(&task.matrix, &dimensions).map_err(|message| {
            ProjectError::InvalidTask {
                task: node.id.clone(),
                message,
            }
        })?;
        if finalizer && task.cache {
            return Err(ProjectError::InvalidTask {
                task: node.id.clone(),
                message: "finalizer tasks cannot be cached".to_owned(),
            });
        }
        let interpolate = |value: &str| {
            interpolate_value(value, &dimensions).map_err(|message| ProjectError::InvalidTask {
                task: node.id.clone(),
                message,
            })
        };
        let cwd = interpolate(task.cwd.as_deref().unwrap_or("."))?;
        let cwd_path =
            fs::canonicalize(self.root.join(&cwd)).map_err(|source| ProjectError::Io {
                path: self.root.join(&cwd),
                source,
            })?;
        if !cwd_path.starts_with(&self.root) || !cwd_path.is_dir() {
            return Err(ProjectError::InvalidTask {
                task: node.id.clone(),
                message: "cwd must resolve to a directory inside the project root".to_owned(),
            });
        }
        let depends_on = self.resolve_task_refs(&node, &task.depends_on)?;
        let command = task
            .command
            .iter()
            .map(|value| interpolate(value))
            .collect::<Result<Vec<_>, _>>()?;
        let env = task
            .env
            .iter()
            .map(|(key, value)| Ok((key.clone(), interpolate(value)?)))
            .collect::<Result<BTreeMap<_, _>, ProjectError>>()?;
        let inputs = task
            .inputs
            .iter()
            .map(|value| interpolate(value))
            .collect::<Result<Vec<_>, _>>()?;
        let outputs = task
            .outputs
            .iter()
            .map(|value| interpolate(value))
            .collect::<Result<Vec<_>, _>>()?;
        let planned = PlannedTask {
            id: node.id,
            project: self.name.clone(),
            root: self.root.clone(),
            command,
            stdin: task.stdin,
            cwd: cwd_path,
            env,
            cache: task.cache,
            inputs,
            outputs,
            cache_env: task.cache_env.clone(),
            timeout: Duration::from_secs(task.timeout_seconds),
            max_output_bytes: usize::try_from(task.max_output_bytes).map_err(|_| {
                ProjectError::InvalidTask {
                    task: base.clone(),
                    message: "max_output_bytes does not fit in platform usize".to_owned(),
                }
            })?,
            resource_group: task.resource_group.clone(),
            retries: task.retries,
            retry_backoff_seconds: Duration::from_secs(task.retry_backoff_seconds),
            finalizer,
            depends_on,
        };
        assert!(!planned.command.is_empty());
        assert!(planned.cwd.is_absolute() && planned.cwd.starts_with(&planned.root));
        Ok(planned)
    }

    fn task_config(&self, node: &TaskNode) -> Result<&TaskConfig, ProjectError> {
        let base = base_task_name(&node.id).map_err(|_| ProjectError::InvalidTaskReference {
            reference: node.id.clone(),
            from: node.clone(),
        })?;
        self.tasks
            .get(base)
            .ok_or_else(|| self.missing_task(&node.id))
    }

    fn resolve_task_refs(
        &self,
        current: &TaskNode,
        references: &[String],
    ) -> Result<Vec<TaskNode>, ProjectError> {
        let mut resolved = Vec::new();
        for reference in references {
            let base = self.resolve_task_ref(current, reference)?;
            let task = self.task_config(&base)?;
            let explicit =
                task_dimensions(&base.id).map_err(|_| ProjectError::InvalidTaskReference {
                    reference: reference.clone(),
                    from: current.clone(),
                })?;
            if !explicit.is_empty() {
                validate_matrix_instance(&task.matrix, &explicit).map_err(|message| {
                    ProjectError::InvalidTask {
                        task: base.id.clone(),
                        message,
                    }
                })?;
                resolved.push(base);
                continue;
            }
            let current_dimensions =
                task_dimensions(&current.id).map_err(|_| ProjectError::InvalidTaskReference {
                    reference: current.id.clone(),
                    from: current.clone(),
                })?;
            for instance in
                matrix_instances(&task.matrix, &current_dimensions).map_err(|message| {
                    ProjectError::InvalidTask {
                        task: base.id.clone(),
                        message,
                    }
                })?
            {
                resolved.push(TaskNode::new(format_task_instance(
                    base_task_name(&base.id).expect("base task parsed"),
                    &instance,
                )));
            }
        }
        Ok(resolved)
    }

    fn resolve_task_ref(
        &self,
        current: &TaskNode,
        reference: &str,
    ) -> Result<TaskNode, ProjectError> {
        validate_task_reference(reference).map_err(|_| ProjectError::InvalidTaskReference {
            reference: reference.to_owned(),
            from: current.clone(),
        })?;
        Ok(TaskNode::new(reference.to_owned()))
    }

    fn missing_task(&self, task: &str) -> ProjectError {
        ProjectError::MissingTask {
            task: task.to_owned(),
            suggestion: closest_name(task, self.tasks.keys()),
        }
    }

    fn unknown_pipeline(&self, name: &str) -> ProjectError {
        ProjectError::UnknownPipeline {
            name: name.to_owned(),
            suggestion: closest_name(name, self.pipelines.keys()),
        }
    }
}

pub(crate) fn validate_schema(path: &Path, schema: u32) -> Result<(), ProjectError> {
    if schema != crate::config::SUPPORTED_SCHEMA {
        return Err(ProjectError::UnsupportedSchema {
            path: path.to_path_buf(),
            found: schema,
            supported: crate::config::SUPPORTED_SCHEMA,
        });
    }
    Ok(())
}

fn validate_task_config(
    manifest_path: &Path,
    root: &Path,
    task_name: &str,
    task: &TaskConfig,
) -> Result<(), ProjectError> {
    validate_identifier(manifest_path, "task name", task_name)?;
    if task_name.contains(['[', ']', '=', ',']) {
        return Err(ProjectError::InvalidTaskName {
            task: task_name.to_owned(),
        });
    }
    if task.command.is_empty() || task.command[0].is_empty() {
        return Err(ProjectError::InvalidTask {
            task: task_name.to_owned(),
            message: "command must contain an executable".to_owned(),
        });
    }
    for (index, argument) in task.command.iter().enumerate() {
        validate_process_value(argument, &format!("command argument {index}")).map_err(
            |message| ProjectError::InvalidTask {
                task: task_name.to_owned(),
                message,
            },
        )?;
    }
    for (key, value) in &task.env {
        validate_process_value(key, "environment key")
            .and_then(|_| validate_process_value(value, "environment value"))
            .map_err(|message| ProjectError::InvalidTask {
                task: task_name.to_owned(),
                message,
            })?;
    }
    if task.cache
        && (task.inputs.is_empty() || !task.inputs.iter().any(|pattern| !pattern.starts_with('!')))
    {
        return Err(ProjectError::InvalidTask {
            task: task_name.to_owned(),
            message: "cacheable tasks must declare at least one positive input pattern".to_owned(),
        });
    }
    if task.cache && task.stdin == StdinMode::Inherit {
        return Err(ProjectError::InvalidTask {
            task: task_name.to_owned(),
            message: "cacheable tasks cannot inherit standard input".to_owned(),
        });
    }
    if !task.outputs.is_empty() && !task.outputs.iter().any(|pattern| !pattern.starts_with('!')) {
        return Err(ProjectError::InvalidTask {
            task: task_name.to_owned(),
            message: "outputs must declare at least one positive pattern".to_owned(),
        });
    }
    for pattern in task.inputs.iter().chain(&task.outputs) {
        validate_cache_pattern(pattern).map_err(|message| ProjectError::InvalidTask {
            task: task_name.to_owned(),
            message,
        })?;
    }
    for variable in &task.cache_env {
        if variable.is_empty() || variable.contains('=') || variable.contains('\0') {
            return Err(ProjectError::InvalidTask {
                task: task_name.to_owned(),
                message: "cache_env names must be non-empty and cannot contain '=' or NUL"
                    .to_owned(),
            });
        }
    }
    if task.timeout_seconds == 0 {
        return Err(ProjectError::InvalidTask {
            task: task_name.to_owned(),
            message: "timeout_seconds must be greater than zero".to_owned(),
        });
    }
    if task.max_output_bytes == 0 || usize::try_from(task.max_output_bytes).is_err() {
        return Err(ProjectError::InvalidTask {
            task: task_name.to_owned(),
            message: "max_output_bytes must be positive and fit in platform usize".to_owned(),
        });
    }
    if task.retries > 0 && task.retry_backoff_seconds > 86_400 {
        return Err(ProjectError::InvalidTask {
            task: task_name.to_owned(),
            message: "retry_backoff_seconds must not exceed 86400".to_owned(),
        });
    }
    if let Some(group) = &task.resource_group
        && (group.is_empty()
            || group.contains(':')
            || group.contains('\0')
            || group.chars().any(char::is_control))
    {
        return Err(ProjectError::InvalidTask {
            task: task_name.to_owned(),
            message: "resource_group must be non-empty and cannot contain ':', NUL, or control characters".to_owned(),
        });
    }
    if let Some(cwd) = &task.cwd {
        if !valid_relative_path(cwd) {
            return Err(ProjectError::InvalidTask {
                task: task_name.to_owned(),
                message: "cwd must be an existing relative directory without '..'".to_owned(),
            });
        }
        if !cwd.contains("${") {
            let cwd_path = root.join(cwd);
            let canonical = fs::canonicalize(&cwd_path).map_err(|source| ProjectError::Io {
                path: cwd_path.clone(),
                source,
            })?;
            if !canonical.starts_with(root) || !canonical.is_dir() {
                return Err(ProjectError::InvalidTask {
                    task: task_name.to_owned(),
                    message: "cwd must resolve to a directory inside the project root".to_owned(),
                });
            }
        }
    }
    for (dimension, values) in &task.matrix {
        validate_identifier(manifest_path, "matrix dimension", dimension)?;
        if values.is_empty() {
            return Err(ProjectError::InvalidTask {
                task: task_name.to_owned(),
                message: format!("matrix dimension '{dimension}' must have at least one value"),
            });
        }
        let mut seen = BTreeSet::new();
        for value in values {
            validate_process_value(value, "matrix value")
                .and_then(|_| {
                    if value.contains(['[', ']', '=', ',']) {
                        Err("matrix values cannot contain '[', ']', '=', or ','".to_owned())
                    } else if value.chars().any(char::is_control) {
                        Err("matrix values cannot contain control characters".to_owned())
                    } else {
                        Ok(())
                    }
                })
                .map_err(|message| ProjectError::InvalidTask {
                    task: task_name.to_owned(),
                    message,
                })?;
            if !seen.insert(value) {
                return Err(ProjectError::InvalidTask {
                    task: task_name.to_owned(),
                    message: format!(
                        "matrix dimension '{dimension}' contains duplicate value '{value}'"
                    ),
                });
            }
        }
    }
    let matrix_size = task
        .matrix
        .values()
        .try_fold(1usize, |size, values| size.checked_mul(values.len()))
        .ok_or_else(|| ProjectError::InvalidTask {
            task: task_name.to_owned(),
            message: "matrix has too many instances".to_owned(),
        })?;
    if matrix_size > 1024 {
        return Err(ProjectError::InvalidTask {
            task: task_name.to_owned(),
            message: "matrix cannot expand to more than 1024 instances".to_owned(),
        });
    }
    Ok(())
}

fn validate_identifier(path: &Path, label: &str, value: &str) -> Result<(), ProjectError> {
    if value.is_empty() || value.contains('\0') || value.chars().any(char::is_control) {
        return Err(ProjectError::InvalidManifest {
            path: path.to_path_buf(),
            message: format!(
                "{label} must be non-empty and cannot contain NUL or control characters"
            ),
        });
    }
    Ok(())
}

fn validate_task_reference(reference: &str) -> Result<(), String> {
    if reference.is_empty() || reference.contains('\0') || reference.chars().any(char::is_control) {
        Err(
            "task reference must be non-empty and cannot contain NUL or control characters"
                .to_owned(),
        )
    } else {
        Ok(())
    }
}

fn valid_relative_path(path: &str) -> bool {
    let path = Path::new(path);
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

fn validate_cache_pattern(pattern: &str) -> Result<(), String> {
    let pattern = pattern.strip_prefix('!').unwrap_or(pattern);
    if pattern.is_empty() || pattern.contains('\0') || Path::new(pattern).is_absolute() {
        return Err("cache patterns must be non-empty and relative".to_owned());
    }
    if pattern
        .split('/')
        .any(|segment| segment.is_empty() || segment == "..")
    {
        return Err("cache patterns cannot contain empty or '..' path segments".to_owned());
    }
    Ok(())
}

fn base_task_name(task: &str) -> Result<&str, ()> {
    match task.find('[') {
        None => Ok(task),
        Some(index) if task.ends_with(']') && index > 0 => Ok(&task[..index]),
        Some(_) => Err(()),
    }
}

fn task_dimensions(task: &str) -> Result<BTreeMap<String, String>, ()> {
    let Some(index) = task.find('[') else {
        return Ok(BTreeMap::new());
    };
    if !task.ends_with(']') {
        return Err(());
    }
    let body = &task[index + 1..task.len() - 1];
    if body.is_empty() {
        return Err(());
    }
    let mut dimensions = BTreeMap::new();
    for pair in body.split(',') {
        let Some((key, value)) = pair.split_once('=') else {
            return Err(());
        };
        if key.is_empty()
            || value.is_empty()
            || dimensions
                .insert(key.to_owned(), value.to_owned())
                .is_some()
        {
            return Err(());
        }
    }
    Ok(dimensions)
}

fn validate_matrix_instance(
    matrix: &BTreeMap<String, Vec<String>>,
    dimensions: &BTreeMap<String, String>,
) -> Result<(), String> {
    if matrix.is_empty() {
        return if dimensions.is_empty() {
            Ok(())
        } else {
            Err("task is not matrix-parameterized".to_owned())
        };
    }
    if matrix.len() != dimensions.len() {
        return Err("matrix task references must specify every dimension".to_owned());
    }
    for (key, values) in matrix {
        let Some(value) = dimensions.get(key) else {
            return Err(format!(
                "matrix task reference is missing dimension '{key}'"
            ));
        };
        if !values.contains(value) {
            return Err(format!("matrix dimension '{key}' has no value '{value}'"));
        }
    }
    Ok(())
}

fn format_task_instance(base: &str, dimensions: &BTreeMap<String, String>) -> String {
    if dimensions.is_empty() {
        return base.to_owned();
    }
    let values = dimensions
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("{base}[{values}]")
}

fn matrix_instances(
    matrix: &BTreeMap<String, Vec<String>>,
    fixed: &BTreeMap<String, String>,
) -> Result<Vec<BTreeMap<String, String>>, String> {
    if matrix.is_empty() {
        return Ok(vec![BTreeMap::new()]);
    }
    let mut instances = vec![BTreeMap::new()];
    for (key, values) in matrix {
        let selected = if let Some(value) = fixed.get(key) {
            if !values.contains(value) {
                return Err(format!("matrix dimension '{key}' has no value '{value}'"));
            }
            vec![value.clone()]
        } else {
            values.clone()
        };
        let mut next = Vec::new();
        for instance in instances {
            for value in &selected {
                let mut expanded = instance.clone();
                expanded.insert(key.clone(), value.clone());
                next.push(expanded);
            }
        }
        instances = next;
    }
    Ok(instances)
}

fn interpolate_value(value: &str, dimensions: &BTreeMap<String, String>) -> Result<String, String> {
    let mut output = String::with_capacity(value.len());
    let mut remaining = value;
    while let Some(start) = remaining.find("${") {
        output.push_str(&remaining[..start]);
        let after = &remaining[start + 2..];
        let Some(end) = after.find('}') else {
            return Err(format!("unterminated matrix placeholder in `{value}`"));
        };
        let key = &after[..end];
        let replacement = dimensions
            .get(key)
            .ok_or_else(|| format!("unknown matrix placeholder `${{{key}}}` in `{value}`"))?;
        output.push_str(replacement);
        remaining = &after[end + 1..];
    }
    output.push_str(remaining);
    Ok(output)
}

fn closest_name<'a>(
    name: &str,
    candidates: impl IntoIterator<Item = &'a String>,
) -> Option<String> {
    let max_distance = if name.len() <= 4 { 1 } else { 2 };
    candidates
        .into_iter()
        .filter(|candidate| candidate.as_str() != name)
        .map(|candidate| (edit_distance(name, candidate), candidate))
        .filter(|(distance, _)| *distance <= max_distance)
        .min_by(|(left_distance, left), (right_distance, right)| {
            left_distance
                .cmp(right_distance)
                .then_with(|| left.cmp(right))
        })
        .map(|(_, candidate)| candidate.clone())
}

fn edit_distance(left: &str, right: &str) -> usize {
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    for (left_index, left_byte) in left.bytes().enumerate() {
        let mut current = vec![left_index + 1];
        for (right_index, right_byte) in right.bytes().enumerate() {
            let substitution = previous[right_index] + usize::from(left_byte != right_byte);
            let insertion = current[right_index] + 1;
            let deletion = previous[right_index + 1] + 1;
            current.push(substitution.min(insertion).min(deletion));
        }
        previous = current;
    }
    previous[right.len()]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VisitState {
    Visiting,
    Visited,
}

#[derive(Debug)]
pub enum ProjectError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    MissingRoot {
        start: PathBuf,
    },
    InvalidManifest {
        path: PathBuf,
        message: String,
    },
    InvalidProject {
        message: String,
    },
    UnknownPipeline {
        name: String,
        suggestion: Option<String>,
    },
    InvalidTaskName {
        task: String,
    },
    InvalidTask {
        task: String,
        message: String,
    },
    MissingTask {
        task: String,
        suggestion: Option<String>,
    },
    InvalidTaskReference {
        reference: String,
        from: TaskNode,
    },
    TaskCycle {
        path: Vec<TaskNode>,
    },
    UnsupportedSchema {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
}

impl fmt::Display for ProjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "could not read {}: {source}", path.display()),
            Self::Parse { path, source } => {
                write!(f, "could not parse {}: {source}", path.display())
            }
            Self::MissingRoot { start } => write!(
                f,
                "could not find a root mono.toml from {}",
                start.display()
            ),
            Self::InvalidManifest { path, message } => {
                write!(f, "invalid manifest {}: {message}", path.display())
            }
            Self::InvalidProject { message } => write!(f, "invalid project: {message}"),
            Self::UnknownPipeline { name, suggestion } => {
                write!(f, "unknown pipeline '{name}'")?;
                if let Some(suggestion) = suggestion {
                    write!(f, ". Did you mean '{suggestion}'?")?;
                }
                Ok(())
            }
            Self::InvalidTaskName { task } => write!(f, "invalid task name '{task}'"),
            Self::InvalidTask { task, message } => write!(f, "invalid task '{task}': {message}"),
            Self::MissingTask { task, suggestion } => {
                write!(f, "project has no task '{task}'")?;
                if let Some(suggestion) = suggestion {
                    write!(f, ". Did you mean '{suggestion}'?")?;
                }
                Ok(())
            }
            Self::InvalidTaskReference { reference, from } => {
                write!(f, "invalid task reference '{reference}' from '{}'", from.id)
            }
            Self::TaskCycle { path } => write!(
                f,
                "task dependency cycle detected: {}",
                path.iter()
                    .map(|node| node.id.as_str())
                    .collect::<Vec<_>>()
                    .join(" -> ")
            ),
            Self::UnsupportedSchema {
                path,
                found,
                supported,
            } => write!(
                f,
                "unsupported manifest schema {found} in {}; supported schema is {supported}",
                path.display()
            ),
        }
    }
}

impl StdError for ProjectError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            _ => None,
        }
    }
}
