//! Manifest validation and task-graph planning for a manifest-driven monorepo.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crate::config::{
    MonorepoConfig, PackageConfig, PipelineConfig, TaskConfig, WORKSPACE_PACKAGE_NAME, config_path,
    validate_process_value,
};
use crate::discovery::{RootKind, expand_member_pattern, find_root, read_manifest};

/// A validated package in a workspace.
#[derive(Debug, Clone)]
pub struct Package {
    pub name: String,
    pub path: PathBuf,
    pub tasks: BTreeMap<String, TaskConfig>,
}

/// A task node uniquely identifies one package task.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskNode {
    pub package: String,
    pub task: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
    Workspace,
    Standalone,
}

/// A complete, validated execution scope.
#[derive(Debug, Clone)]
pub struct Workspace {
    pub root: PathBuf,
    pub name: String,
    pub default_pipeline: String,
    pub pipelines: BTreeMap<String, PipelineConfig>,
    pub workspace_tasks: BTreeMap<String, TaskConfig>,
    pub packages: BTreeMap<String, Package>,
    kind: ScopeKind,
}

/// One executable task in a deterministic task-DAG plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedTask {
    package: String,
    package_path: PathBuf,
    task: String,
    command: Vec<String>,
    cwd: PathBuf,
    env: BTreeMap<String, String>,
    cache: bool,
    inputs: Vec<String>,
    outputs: Vec<String>,
    cache_env: Vec<String>,
    timeout: Duration,
    resource_group: Option<String>,
    depends_on: Vec<TaskNode>,
}

impl PlannedTask {
    pub fn node(&self) -> TaskNode {
        TaskNode {
            package: self.package.clone(),
            task: self.task.clone(),
        }
    }

    pub fn package(&self) -> &str {
        &self.package
    }

    pub fn package_path(&self) -> &Path {
        &self.package_path
    }

    pub fn task(&self) -> &str {
        &self.task
    }

    pub fn command(&self) -> &[String] {
        &self.command
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

    pub fn resource_group(&self) -> Option<&str> {
        self.resource_group.as_deref()
    }

    pub fn depends_on(&self) -> &[TaskNode] {
        &self.depends_on
    }
}

impl Workspace {
    /// Find and load the root workspace at `start` or one of its ancestors.
    pub fn load(start: &Path) -> Result<Self, WorkspaceError> {
        let discovered = find_root(start)?;
        let root = discovered.root;
        let root_config = discovered.config;
        if discovered.kind == RootKind::Standalone {
            return Self::load_standalone(root, root_config);
        }

        let workspace_config =
            root_config
                .workspace
                .ok_or_else(|| WorkspaceError::InvalidManifest {
                    path: config_path(&root),
                    message: "root manifest is missing [workspace]".to_owned(),
                })?;

        validate_identifier(
            &config_path(&root),
            "workspace name",
            &workspace_config.name,
        )?;
        validate_identifier(
            &config_path(&root),
            "default pipeline",
            &workspace_config.default_pipeline,
        )?;
        for pipeline_name in root_config.pipelines.keys() {
            validate_identifier(&config_path(&root), "pipeline name", pipeline_name)?;
        }
        for (pipeline_name, pipeline) in &root_config.pipelines {
            if pipeline.tasks.is_empty() {
                return Err(WorkspaceError::InvalidWorkspace {
                    message: format!("pipeline '{pipeline_name}' has no tasks"),
                });
            }
            for task_name in &pipeline.tasks {
                validate_task_reference(task_name).map_err(|message| {
                    WorkspaceError::InvalidManifest {
                        path: config_path(&root),
                        message: format!("pipeline '{pipeline_name}': {message}"),
                    }
                })?;
            }
        }

        if root_config.package.is_some() {
            return Err(WorkspaceError::InvalidManifest {
                path: config_path(&root),
                message: "root manifests may only contain [workspace], [tasks], and [pipelines]"
                    .to_owned(),
            });
        }
        let workspace_package_config = PackageConfig {
            name: WORKSPACE_PACKAGE_NAME.to_owned(),
        };
        validate_package_config(
            &config_path(&root),
            &root,
            &workspace_package_config,
            &root_config.tasks,
        )?;
        if root_config.pipelines.is_empty() {
            return Err(WorkspaceError::InvalidWorkspace {
                message: "workspace must define at least one pipeline".to_owned(),
            });
        }
        if !root_config
            .pipelines
            .contains_key(&workspace_config.default_pipeline)
        {
            return Err(WorkspaceError::UnknownPipeline {
                suggestion: closest_name(
                    &workspace_config.default_pipeline,
                    root_config.pipelines.keys(),
                ),
                name: workspace_config.default_pipeline.clone(),
            });
        }

        let mut packages = BTreeMap::new();
        let mut package_paths = BTreeSet::new();

        for pattern in &workspace_config.members {
            for discovered_path in expand_member_pattern(&root, pattern)? {
                let package_path =
                    fs::canonicalize(&discovered_path).map_err(|source| WorkspaceError::Io {
                        path: discovered_path.clone(),
                        source,
                    })?;
                if !package_path.starts_with(&root) {
                    return Err(WorkspaceError::InvalidManifest {
                        path: discovered_path,
                        message: "workspace member resolves outside the workspace root".to_owned(),
                    });
                }
                if !package_paths.insert(package_path.clone()) {
                    continue;
                }

                let manifest_path = config_path(&package_path);
                if !manifest_path.is_file() {
                    return Err(WorkspaceError::MissingPackageManifest {
                        path: manifest_path,
                    });
                }

                let config = read_manifest(&manifest_path)?;
                let package_config =
                    config
                        .package
                        .ok_or_else(|| WorkspaceError::InvalidManifest {
                            path: manifest_path.clone(),
                            message: "package manifest is missing [package]".to_owned(),
                        })?;

                if config.workspace.is_some() || !config.pipelines.is_empty() {
                    return Err(WorkspaceError::InvalidManifest {
                        path: manifest_path,
                        message: "package manifests may only contain [package] and [tasks]"
                            .to_owned(),
                    });
                }

                if package_config.name == WORKSPACE_PACKAGE_NAME {
                    return Err(WorkspaceError::InvalidManifest {
                        path: manifest_path,
                        message: format!(
                            "package name '{}' is reserved for workspace tasks",
                            WORKSPACE_PACKAGE_NAME
                        ),
                    });
                }

                validate_package_config(
                    &manifest_path,
                    &package_path,
                    &package_config,
                    &config.tasks,
                )?;
                let package = Package::from_config(package_path, package_config, config.tasks);
                let package_name = package.name.clone();
                if packages.insert(package_name.clone(), package).is_some() {
                    return Err(WorkspaceError::DuplicatePackage { name: package_name });
                }
            }
        }

        let workspace = Self {
            root,
            name: workspace_config.name,
            default_pipeline: workspace_config.default_pipeline,
            pipelines: root_config.pipelines,
            workspace_tasks: root_config.tasks,
            packages,
            kind: ScopeKind::Workspace,
        };
        workspace.validate_graph()?;
        let pipeline_names = workspace.pipelines.keys().cloned().collect::<Vec<_>>();
        for pipeline_name in pipeline_names {
            workspace.plan(None, Some(&pipeline_name), &[])?;
        }
        workspace.assert_invariants();
        Ok(workspace)
    }

    fn load_standalone(root: PathBuf, root_config: MonorepoConfig) -> Result<Self, WorkspaceError> {
        let package_config =
            root_config
                .package
                .ok_or_else(|| WorkspaceError::InvalidManifest {
                    path: config_path(&root),
                    message: "standalone manifests require [package]".to_owned(),
                })?;

        validate_identifier(&config_path(&root), "package name", &package_config.name)?;
        if package_config.name == WORKSPACE_PACKAGE_NAME {
            return Err(WorkspaceError::InvalidManifest {
                path: config_path(&root),
                message: format!(
                    "package name '{}' is reserved for workspace tasks",
                    WORKSPACE_PACKAGE_NAME
                ),
            });
        }
        for pipeline_name in root_config.pipelines.keys() {
            validate_identifier(&config_path(&root), "pipeline name", pipeline_name)?;
        }
        for (pipeline_name, pipeline) in &root_config.pipelines {
            if pipeline.tasks.is_empty() {
                return Err(WorkspaceError::InvalidWorkspace {
                    message: format!("pipeline '{pipeline_name}' has no tasks"),
                });
            }
            for task_name in &pipeline.tasks {
                validate_task_reference(task_name).map_err(|message| {
                    WorkspaceError::InvalidManifest {
                        path: config_path(&root),
                        message: format!("pipeline '{pipeline_name}': {message}"),
                    }
                })?;
            }
        }
        if root_config.pipelines.is_empty() {
            return Err(WorkspaceError::InvalidWorkspace {
                message: "standalone project must define at least one pipeline".to_owned(),
            });
        }
        if !root_config.pipelines.contains_key("ci") {
            return Err(WorkspaceError::UnknownPipeline {
                suggestion: closest_name("ci", root_config.pipelines.keys()),
                name: "ci".to_owned(),
            });
        }
        validate_package_config(
            &config_path(&root),
            &root,
            &package_config,
            &root_config.tasks,
        )?;

        let package = Package::from_config(root.clone(), package_config, root_config.tasks);
        let package_name = package.name.clone();
        let workspace = Self {
            root,
            name: package_name.clone(),
            default_pipeline: "ci".to_owned(),
            pipelines: root_config.pipelines,
            workspace_tasks: BTreeMap::new(),
            packages: BTreeMap::from([(package_name, package)]),
            kind: ScopeKind::Standalone,
        };
        workspace.validate_graph()?;
        let pipeline_names = workspace.pipelines.keys().cloned().collect::<Vec<_>>();
        for pipeline_name in pipeline_names {
            workspace.plan(None, Some(&pipeline_name), &[])?;
        }
        workspace.assert_invariants();
        Ok(workspace)
    }

    fn assert_invariants(&self) {
        assert!(self.root.is_absolute(), "workspace root must be absolute");
        assert!(self.root.is_dir(), "workspace root must be a directory");
        assert!(!self.name.is_empty(), "workspace name must not be empty");
        assert!(
            self.pipelines.contains_key(&self.default_pipeline),
            "default pipeline must exist"
        );

        for package in self.packages.values() {
            assert!(!package.name.is_empty(), "package name must not be empty");
            assert!(
                package.path.starts_with(&self.root),
                "package path must remain inside the workspace root"
            );
            assert!(package.path.is_absolute(), "package path must be absolute");
        }

        match self.kind {
            ScopeKind::Standalone => {
                assert_eq!(
                    self.packages.len(),
                    1,
                    "standalone scope must have one package"
                );
                let package = self
                    .packages
                    .values()
                    .next()
                    .expect("singleton package exists");
                assert_eq!(
                    package.path, self.root,
                    "standalone package must be rooted at scope root"
                );
                assert!(
                    self.workspace_tasks.is_empty(),
                    "standalone scope has no workspace tasks"
                );
            }
            ScopeKind::Workspace => {
                assert!(
                    self.packages
                        .values()
                        .all(|package| package.name != WORKSPACE_PACKAGE_NAME),
                    "workspace namespace cannot be a real package"
                );
            }
        }
    }

    /// Return the user-facing kind of this execution scope.
    pub fn scope_label(&self) -> &'static str {
        match self.kind {
            ScopeKind::Standalone => "project",
            ScopeKind::Workspace => "monorepo",
        }
    }

    /// Produce a dependency-first task plan.
    ///
    /// When `requested_tasks` is empty, the selected pipeline is used. An
    /// unqualified task in a pipeline runs for every selected package. A
    /// qualified task such as `docs:generate` is a single explicit root node.
    pub fn plan(
        &self,
        selected_package: Option<&str>,
        pipeline: Option<&str>,
        requested_tasks: &[String],
    ) -> Result<Vec<PlannedTask>, WorkspaceError> {
        let task_names = if requested_tasks.is_empty() {
            let pipeline_name = pipeline.unwrap_or(&self.default_pipeline);
            &self
                .pipelines
                .get(pipeline_name)
                .ok_or_else(|| WorkspaceError::UnknownPipeline {
                    suggestion: closest_name(pipeline_name, self.pipelines.keys()),
                    name: pipeline_name.to_owned(),
                })?
                .tasks
        } else {
            requested_tasks
        };

        if task_names.is_empty() {
            return Err(WorkspaceError::InvalidWorkspace {
                message: "the selected pipeline has no tasks".to_owned(),
            });
        }

        let package_names = self.selected_packages(selected_package)?;
        let mut roots = BTreeSet::new();
        for task_name in task_names {
            if let Some((package, task)) = split_task_ref(task_name) {
                if let Some(selected) = selected_package
                    && package != selected
                {
                    return Err(WorkspaceError::TaskOutsideSelectedPackage {
                        package: package.to_owned(),
                        selected: selected.to_owned(),
                    });
                }
                roots.insert(TaskNode {
                    package: package.to_owned(),
                    task: task.to_owned(),
                });
            } else {
                for package in &package_names {
                    roots.insert(TaskNode {
                        package: package.clone(),
                        task: task_name.clone(),
                    });
                }
            }
        }

        if roots.is_empty() {
            return Ok(Vec::new());
        }

        let mut state = BTreeMap::new();
        let mut ordered_nodes = Vec::new();
        let mut stack = Vec::new();
        for root in roots {
            self.visit_task(&root, &mut state, &mut stack, &mut ordered_nodes)?;
        }

        let mut planned = Vec::with_capacity(ordered_nodes.len());
        let mut planned_nodes = BTreeSet::new();
        for node in ordered_nodes {
            assert!(
                planned_nodes.insert(node.clone()),
                "task planner must not emit duplicate task nodes"
            );
            planned.push(self.planned_task(node)?);
        }
        Ok(planned)
    }

    /// Return graph edges for the selected pipeline, using the same plan as execution.
    pub fn graph(
        &self,
        selected_package: Option<&str>,
        pipeline: Option<&str>,
        requested_tasks: &[String],
    ) -> Result<Vec<(TaskNode, Vec<TaskNode>)>, WorkspaceError> {
        Ok(self
            .plan(selected_package, pipeline, requested_tasks)?
            .into_iter()
            .map(|task| (task.node(), task.depends_on))
            .collect())
    }

    fn validate_graph(&self) -> Result<(), WorkspaceError> {
        for (package, tasks) in std::iter::once((WORKSPACE_PACKAGE_NAME, &self.workspace_tasks))
            .chain(
                self.packages
                    .values()
                    .map(|package| (package.name.as_str(), &package.tasks)),
            )
        {
            for task_name in tasks.keys() {
                if task_name.is_empty() || task_name.contains(':') {
                    return Err(WorkspaceError::InvalidTaskName {
                        package: package.to_owned(),
                        task: task_name.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    fn task_suggestion(&self, package: &str, task: &str) -> Option<String> {
        if package == WORKSPACE_PACKAGE_NAME {
            closest_name(task, self.workspace_tasks.keys())
        } else {
            self.packages
                .get(package)
                .and_then(|package| closest_name(task, package.tasks.keys()))
        }
    }

    fn selected_packages(
        &self,
        selected_package: Option<&str>,
    ) -> Result<BTreeSet<String>, WorkspaceError> {
        if let Some(selected) = selected_package {
            if !self.packages.contains_key(selected) {
                return Err(WorkspaceError::UnknownPackage {
                    suggestion: closest_name(selected, self.packages.keys()),
                    name: selected.to_owned(),
                });
            }
            return Ok([selected.to_owned()].into_iter().collect());
        }
        Ok(self.packages.keys().cloned().collect())
    }

    fn visit_task(
        &self,
        node: &TaskNode,
        state: &mut BTreeMap<TaskNode, VisitState>,
        stack: &mut Vec<TaskNode>,
        order: &mut Vec<TaskNode>,
    ) -> Result<(), WorkspaceError> {
        match state.get(node) {
            Some(VisitState::Visited) => return Ok(()),
            Some(VisitState::Visiting) => {
                let start = stack
                    .iter()
                    .position(|current| current == node)
                    .expect("visiting task must be present on the DFS stack");
                let mut path = stack[start..].to_vec();
                path.push(node.clone());
                return Err(WorkspaceError::TaskCycle { path });
            }
            None => {}
        }

        let task = self.task_config(node)?;
        state.insert(node.clone(), VisitState::Visiting);
        stack.push(node.clone());
        let dependencies = task
            .depends_on
            .iter()
            .map(|dependency| self.resolve_task_ref(node, dependency))
            .collect::<Result<Vec<_>, _>>()?;
        for dependency in &dependencies {
            self.visit_task(dependency, state, stack, order)?;
        }
        stack
            .pop()
            .expect("visiting task is present on the DFS stack");
        state.insert(node.clone(), VisitState::Visited);
        order.push(node.clone());
        Ok(())
    }

    fn planned_task(&self, node: TaskNode) -> Result<PlannedTask, WorkspaceError> {
        let (package_name, package_path, task) = if node.package == WORKSPACE_PACKAGE_NAME {
            (
                WORKSPACE_PACKAGE_NAME,
                &self.root,
                self.workspace_tasks.get(&node.task),
            )
        } else {
            let package =
                self.packages
                    .get(&node.package)
                    .ok_or_else(|| WorkspaceError::UnknownPackage {
                        suggestion: closest_name(&node.package, self.packages.keys()),
                        name: node.package.clone(),
                    })?;
            (
                package.name.as_str(),
                &package.path,
                package.tasks.get(&node.task),
            )
        };
        let task = task.ok_or_else(|| WorkspaceError::MissingTask {
            suggestion: self.task_suggestion(&node.package, &node.task),
            package: node.package.clone(),
            task: node.task.clone(),
        })?;
        let cwd = task.cwd.as_deref().unwrap_or(".");
        let cwd_path =
            fs::canonicalize(package_path.join(cwd)).map_err(|source| WorkspaceError::Io {
                path: package_path.join(cwd),
                source,
            })?;
        if !cwd_path.starts_with(package_path) || !cwd_path.is_dir() {
            return Err(WorkspaceError::InvalidTask {
                package: package_name.to_owned(),
                task: node.task.clone(),
                message: "cwd must resolve to a directory inside the package".to_owned(),
            });
        }
        let depends_on = task
            .depends_on
            .iter()
            .map(|dependency| self.resolve_task_ref(&node, dependency))
            .collect::<Result<Vec<_>, _>>()?;
        let planned = PlannedTask {
            package: package_name.to_owned(),
            package_path: package_path.clone(),
            task: node.task,
            command: task.command.clone(),
            cwd: cwd_path,
            env: task.env.clone(),
            cache: task.cache,
            inputs: task.inputs.clone(),
            outputs: task.outputs.clone(),
            cache_env: task.cache_env.clone(),
            timeout: Duration::from_secs(task.timeout_seconds),
            resource_group: task.resource_group.clone(),
            depends_on,
        };
        assert!(
            !planned.command.is_empty(),
            "validated planned task command must not be empty"
        );
        assert!(
            planned.package_path.starts_with(&self.root),
            "planned task package must remain inside the workspace root"
        );
        assert!(
            planned.cwd.is_absolute() && planned.cwd.starts_with(&planned.package_path),
            "planned task cwd must be absolute and package-contained"
        );
        assert!(
            planned.cwd.is_dir(),
            "validated planned task cwd must remain a directory"
        );
        assert!(
            planned.timeout > Duration::ZERO,
            "validated planned task timeout must be positive"
        );
        Ok(planned)
    }

    fn task_config(&self, node: &TaskNode) -> Result<&TaskConfig, WorkspaceError> {
        if node.package == WORKSPACE_PACKAGE_NAME {
            return self.workspace_tasks.get(&node.task).ok_or_else(|| {
                WorkspaceError::MissingTask {
                    suggestion: self.task_suggestion(&node.package, &node.task),
                    package: node.package.clone(),
                    task: node.task.clone(),
                }
            });
        }

        let package =
            self.packages
                .get(&node.package)
                .ok_or_else(|| WorkspaceError::UnknownPackage {
                    suggestion: closest_name(&node.package, self.packages.keys()),
                    name: node.package.clone(),
                })?;
        package
            .tasks
            .get(&node.task)
            .ok_or_else(|| WorkspaceError::MissingTask {
                suggestion: self.task_suggestion(&node.package, &node.task),
                package: node.package.clone(),
                task: node.task.clone(),
            })
    }

    fn resolve_task_ref(
        &self,
        current: &TaskNode,
        reference: &str,
    ) -> Result<TaskNode, WorkspaceError> {
        if validate_task_reference(reference).is_err() {
            return Err(WorkspaceError::InvalidTaskReference {
                reference: reference.to_owned(),
                from: current.clone(),
            });
        }

        if let Some((package, task)) = split_task_ref(reference) {
            if package.is_empty() || task.is_empty() || task.contains(':') {
                return Err(WorkspaceError::InvalidTaskReference {
                    reference: reference.to_owned(),
                    from: current.clone(),
                });
            }
            Ok(TaskNode {
                package: package.to_owned(),
                task: task.to_owned(),
            })
        } else {
            Ok(TaskNode {
                package: current.package.clone(),
                task: reference.to_owned(),
            })
        }
    }
}

impl Package {
    fn from_config(
        path: PathBuf,
        config: PackageConfig,
        tasks: BTreeMap<String, TaskConfig>,
    ) -> Self {
        Self {
            name: config.name,
            path,
            tasks,
        }
    }
}

fn validate_package_config(
    manifest_path: &Path,
    package_path: &Path,
    config: &PackageConfig,
    tasks: &BTreeMap<String, TaskConfig>,
) -> Result<(), WorkspaceError> {
    validate_identifier(manifest_path, "package name", &config.name)?;

    for (task_name, task) in tasks {
        validate_identifier(
            manifest_path,
            &format!("task name in package '{}'", config.name),
            task_name,
        )?;
        if task_name.is_empty() || task_name.contains(':') {
            return Err(WorkspaceError::InvalidTaskName {
                package: config.name.clone(),
                task: task_name.clone(),
            });
        }
        if task.command.is_empty() || task.command[0].is_empty() {
            return Err(WorkspaceError::InvalidTask {
                package: config.name.clone(),
                task: task_name.clone(),
                message: "command must contain an executable".to_owned(),
            });
        }
        for (index, argument) in task.command.iter().enumerate() {
            validate_process_value(argument, &format!("command argument {index}")).map_err(
                |message| WorkspaceError::InvalidTask {
                    package: config.name.clone(),
                    task: task_name.clone(),
                    message,
                },
            )?;
        }
        for (key, value) in &task.env {
            validate_process_value(key, "environment key")
                .and_then(|_| validate_process_value(value, "environment value"))
                .map_err(|message| WorkspaceError::InvalidTask {
                    package: config.name.clone(),
                    task: task_name.clone(),
                    message,
                })?;
        }
        if task.cache
            && (task.inputs.is_empty()
                || !task.inputs.iter().any(|pattern| !pattern.starts_with('!')))
        {
            return Err(WorkspaceError::InvalidTask {
                package: config.name.clone(),
                task: task_name.clone(),
                message: "cacheable tasks must declare at least one positive input pattern"
                    .to_owned(),
            });
        }
        if !task.outputs.is_empty() && !task.outputs.iter().any(|pattern| !pattern.starts_with('!'))
        {
            return Err(WorkspaceError::InvalidTask {
                package: config.name.clone(),
                task: task_name.clone(),
                message: "outputs must declare at least one positive pattern".to_owned(),
            });
        }
        for pattern in task.inputs.iter().chain(task.outputs.iter()) {
            validate_cache_pattern(pattern).map_err(|message| WorkspaceError::InvalidTask {
                package: config.name.clone(),
                task: task_name.clone(),
                message,
            })?;
        }
        for variable in &task.cache_env {
            if variable.is_empty() || variable.contains('=') {
                return Err(WorkspaceError::InvalidTask {
                    package: config.name.clone(),
                    task: task_name.clone(),
                    message: "cache_env names must be non-empty and cannot contain '='".to_owned(),
                });
            }
            validate_process_value(variable, "cache environment name").map_err(|message| {
                WorkspaceError::InvalidTask {
                    package: config.name.clone(),
                    task: task_name.clone(),
                    message,
                }
            })?;
        }
        if task.timeout_seconds == 0 {
            return Err(WorkspaceError::InvalidTask {
                package: config.name.clone(),
                task: task_name.clone(),
                message: "timeout_seconds must be greater than zero".to_owned(),
            });
        }
        if let Some(resource_group) = task.resource_group.as_deref() {
            if resource_group.is_empty() || resource_group.contains(':') {
                return Err(WorkspaceError::InvalidTask {
                    package: config.name.clone(),
                    task: task_name.clone(),
                    message: "resource_group must be non-empty and cannot contain ':'".to_owned(),
                });
            }
            validate_process_value(resource_group, "resource_group").map_err(|message| {
                WorkspaceError::InvalidTask {
                    package: config.name.clone(),
                    task: task_name.clone(),
                    message,
                }
            })?;
        }
        if let Some(cwd) = task.cwd.as_deref() {
            if !valid_relative_path(cwd) {
                return Err(WorkspaceError::InvalidTask {
                    package: config.name.clone(),
                    task: task_name.clone(),
                    message: "cwd must be an existing relative directory without '..'".to_owned(),
                });
            }
            let cwd_path = package_path.join(cwd);
            let canonical_cwd =
                fs::canonicalize(&cwd_path).map_err(|source| WorkspaceError::Io {
                    path: cwd_path.clone(),
                    source,
                })?;
            if !canonical_cwd.starts_with(package_path) || !canonical_cwd.is_dir() {
                return Err(WorkspaceError::InvalidTask {
                    package: config.name.clone(),
                    task: task_name.clone(),
                    message: "cwd must resolve to a directory inside the package".to_owned(),
                });
            }
        }
    }
    Ok(())
}

fn validate_identifier(path: &Path, label: &str, value: &str) -> Result<(), WorkspaceError> {
    if value.is_empty() || value.contains(':') || value.contains('\0') {
        return Err(WorkspaceError::InvalidManifest {
            path: path.to_path_buf(),
            message: format!("{label} must be non-empty and cannot contain ':' or NUL"),
        });
    }
    Ok(())
}

fn validate_task_reference(reference: &str) -> Result<(), String> {
    if reference.is_empty() || reference.contains('\0') {
        return Err("task reference must be non-empty and cannot contain NUL".to_owned());
    }
    match split_task_ref(reference) {
        Some((package, task)) if !package.is_empty() && !task.is_empty() && !task.contains(':') => {
            Ok(())
        }
        Some(_) => Err(format!("invalid task reference '{reference}'")),
        None => Ok(()),
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

fn split_task_ref(reference: &str) -> Option<(&str, &str)> {
    reference.split_once(':')
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
        .min_by_key(|(distance, candidate)| (*distance, (*candidate).clone()))
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

/// Expected failures while discovering or planning a workspace.
#[derive(Debug)]
pub enum WorkspaceError {
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
    MissingPackageManifest {
        path: PathBuf,
    },
    InvalidManifest {
        path: PathBuf,
        message: String,
    },
    InvalidWorkspace {
        message: String,
    },
    InvalidMemberPattern {
        pattern: String,
    },
    DuplicatePackage {
        name: String,
    },
    UnknownPackage {
        name: String,
        suggestion: Option<String>,
    },
    UnknownPipeline {
        name: String,
        suggestion: Option<String>,
    },
    InvalidTaskName {
        package: String,
        task: String,
    },
    InvalidTask {
        package: String,
        task: String,
        message: String,
    },
    MissingTask {
        package: String,
        task: String,
        suggestion: Option<String>,
    },
    InvalidTaskReference {
        reference: String,
        from: TaskNode,
    },
    TaskOutsideSelectedPackage {
        package: String,
        selected: String,
    },
    TaskCycle {
        path: Vec<TaskNode>,
    },
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "could not read {}: {source}", path.display()),
            Self::Parse { path, source } => {
                write!(f, "could not parse {}: {source}", path.display())
            }
            Self::MissingRoot { start } => write!(
                f,
                "could not find a root monorepo.toml from {}",
                start.display()
            ),
            Self::MissingPackageManifest { path } => {
                write!(f, "workspace member is missing {}", path.display())
            }
            Self::InvalidManifest { path, message } => {
                write!(f, "invalid manifest {}: {message}", path.display())
            }
            Self::InvalidWorkspace { message } => write!(f, "invalid workspace: {message}"),
            Self::InvalidMemberPattern { pattern } => {
                write!(f, "invalid workspace member pattern '{pattern}'")
            }
            Self::DuplicatePackage { name } => write!(f, "duplicate package name '{name}'"),
            Self::UnknownPackage { name, suggestion } => {
                write!(f, "unknown package '{name}'")?;
                if let Some(suggestion) = suggestion {
                    write!(f, ". Did you mean '{suggestion}'?")?;
                }
                Ok(())
            }
            Self::UnknownPipeline { name, suggestion } => {
                write!(f, "unknown pipeline '{name}'")?;
                if let Some(suggestion) = suggestion {
                    write!(f, ". Did you mean '{suggestion}'?")?;
                }
                Ok(())
            }
            Self::InvalidTaskName { package, task } => {
                write!(f, "invalid task name '{package}:{task}'")
            }
            Self::InvalidTask {
                package,
                task,
                message,
            } => write!(f, "invalid task '{package}:{task}': {message}"),
            Self::MissingTask {
                package,
                task,
                suggestion,
            } => {
                write!(f, "package '{package}' has no task '{task}'")?;
                if let Some(suggestion) = suggestion {
                    write!(f, ". Did you mean '{suggestion}'?")?;
                }
                Ok(())
            }
            Self::InvalidTaskReference { reference, from } => write!(
                f,
                "invalid task reference '{reference}' from '{}:{}'",
                from.package, from.task
            ),
            Self::TaskOutsideSelectedPackage { package, selected } => write!(
                f,
                "task root belongs to package '{package}', but package selection is '{selected}'; omit --package for workspace tasks or select a package task"
            ),
            Self::TaskCycle { path } => {
                let cycle = path
                    .iter()
                    .map(|node| format!("{}:{}", node.package, node.task))
                    .collect::<Vec<_>>()
                    .join(" -> ");
                write!(f, "task dependency cycle detected: {cycle}")
            }
        }
    }
}

impl StdError for WorkspaceError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    fn write_manifest(path: &Path, contents: &str) {
        fs::create_dir_all(path).expect("create package directory");
        fs::write(config_path(path), contents).expect("write manifest");
    }

    fn root_with_members(temp: &TempDir, members: &str) -> PathBuf {
        fs::write(
            config_path(temp.path()),
            format!(
                "[workspace]\nname = \"test\"\nmembers = [{members}]\ndefault_pipeline = \"ci\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n"
            ),
        )
        .expect("write root manifest");
        temp.path().to_path_buf()
    }

    fn standalone_root(temp: &TempDir) -> PathBuf {
        fs::write(
            config_path(temp.path()),
            "[package]\nname = \"app\"\n\n[pipelines.ci]\ntasks = [\"build\", \"test\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n\n[tasks.test]\ncommand = [\"echo\", \"test\"]\ndepends_on = [\"build\"]\n",
        )
        .expect("write standalone manifest");
        temp.path().to_path_buf()
    }

    #[test]
    fn loads_a_standalone_project_as_one_package() {
        let temp = TempDir::new();
        let root = standalone_root(&temp);
        let nested = root.join("src");
        fs::create_dir_all(&nested).expect("create nested source directory");

        let scope = Workspace::load(&nested).expect("standalone project loads");

        assert_eq!(scope.name, "app");
        assert_eq!(scope.packages.len(), 1);
        assert_eq!(scope.packages["app"].path, fs::canonicalize(root).unwrap());
        assert!(scope.workspace_tasks.is_empty());
    }

    #[test]
    fn workspace_root_wins_when_loaded_from_a_package_directory() {
        let temp = TempDir::new();
        let root = root_with_members(&temp, "\"packages/*\"");
        let package = root.join("packages/app");
        fs::create_dir_all(package.join("src")).expect("create package source directory");
        fs::write(
            config_path(&package),
            "[package]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n",
        )
        .expect("write package manifest");

        let scope = Workspace::load(package.join("src").as_path())
            .expect("workspace root is discovered from a package directory");

        assert_eq!(scope.name, "test");
        assert_eq!(scope.packages.len(), 1);
        assert_eq!(
            scope.packages["app"].path,
            fs::canonicalize(package).unwrap()
        );
    }

    #[test]
    fn plans_standalone_tasks_as_package_tasks_in_dependency_order() {
        let temp = TempDir::new();
        let root = standalone_root(&temp);
        let scope = Workspace::load(&root).expect("standalone project loads");

        let plan = scope
            .plan(None, None, &[])
            .expect("standalone plan succeeds");

        assert_eq!(
            plan.iter().map(|task| task.node()).collect::<Vec<_>>(),
            vec![
                TaskNode {
                    package: "app".to_owned(),
                    task: "build".to_owned(),
                },
                TaskNode {
                    package: "app".to_owned(),
                    task: "test".to_owned(),
                },
            ]
        );
        assert!(plan.iter().all(|task| task.package() == "app"));
    }

    #[test]
    fn rejects_a_manifest_that_declares_both_root_modes() {
        let temp = TempDir::new();
        fs::write(
            config_path(temp.path()),
            "[workspace]\nname = \"repo\"\n\n[package]\nname = \"app\"\n",
        )
        .expect("write ambiguous manifest");

        let error = Workspace::load(temp.path()).expect_err("ambiguous root must fail");

        assert!(error.to_string().contains("both [workspace] and [package]"));
    }

    #[test]
    fn discovers_members_and_orders_cross_package_tasks() {
        let temp = TempDir::new();
        let root = root_with_members(&temp, "\"packages/*\"");
        write_manifest(
            &root.join("packages/base"),
            "[package]\nname = \"base\"\n\n[tasks.build]\ncommand = [\"echo\", \"base\"]\n",
        );
        write_manifest(
            &root.join("packages/app"),
            "[package]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"echo\", \"app\"]\ndepends_on = [\"base:build\"]\n",
        );

        let workspace = Workspace::load(&root).expect("workspace loads");
        let plan = workspace.plan(None, None, &[]).expect("plan succeeds");

        assert_eq!(
            plan.iter().map(|task| task.node()).collect::<Vec<_>>(),
            vec![
                TaskNode {
                    package: "base".to_owned(),
                    task: "build".to_owned(),
                },
                TaskNode {
                    package: "app".to_owned(),
                    task: "build".to_owned(),
                }
            ]
        );
    }

    #[test]
    fn plans_workspace_tasks_once_after_package_dependencies() {
        let temp = TempDir::new();
        fs::write(
            config_path(temp.path()),
            "[workspace]\nname = \"test\"\nmembers = [\"packages/*\"]\ndefault_pipeline = \"release\"\n\n[pipelines.release]\ntasks = [\"workspace:release-verify\"]\n\n[tasks.release-verify]\ncommand = [\"echo\", \"release\"]\ndepends_on = [\"app:package\"]\n",
        )
        .expect("write root manifest");
        write_manifest(
            &temp.path().join("packages/app"),
            "[package]\nname = \"app\"\n\n[tasks.package]\ncommand = [\"echo\", \"package\"]\n",
        );

        let workspace = Workspace::load(temp.path()).expect("workspace loads");
        let plan = workspace.plan(None, None, &[]).expect("plan succeeds");

        assert_eq!(
            plan.iter().map(|task| task.node()).collect::<Vec<_>>(),
            vec![
                TaskNode {
                    package: "app".to_owned(),
                    task: "package".to_owned(),
                },
                TaskNode {
                    package: WORKSPACE_PACKAGE_NAME.to_owned(),
                    task: "release-verify".to_owned(),
                },
            ]
        );
        assert_eq!(
            plan[1].cwd(),
            &fs::canonicalize(temp.path()).expect("workspace root canonicalizes")
        );
    }

    #[test]
    fn workspace_task_dependencies_are_local_by_default() {
        let temp = TempDir::new();
        fs::write(
            config_path(temp.path()),
            "[workspace]\nname = \"test\"\nmembers = []\n\n[pipelines.ci]\ntasks = [\"workspace:release-verify\"]\n\n[tasks.generate]\ncommand = [\"echo\", \"generate\"]\n\n[tasks.release-verify]\ncommand = [\"echo\", \"release\"]\ndepends_on = [\"generate\"]\n",
        )
        .expect("write root manifest");

        let workspace = Workspace::load(temp.path()).expect("workspace loads");
        let plan = workspace.plan(None, None, &[]).expect("plan succeeds");

        assert_eq!(
            plan.iter().map(|task| task.node()).collect::<Vec<_>>(),
            vec![
                TaskNode {
                    package: WORKSPACE_PACKAGE_NAME.to_owned(),
                    task: "generate".to_owned(),
                },
                TaskNode {
                    package: WORKSPACE_PACKAGE_NAME.to_owned(),
                    task: "release-verify".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn rejects_a_package_named_workspace() {
        let temp = TempDir::new();
        let root = root_with_members(&temp, "\"packages/*\"");
        write_manifest(
            &root.join("packages/reserved"),
            "[package]\nname = \"workspace\"\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n",
        );

        let error = Workspace::load(&root).expect_err("workspace namespace must be reserved");
        assert!(error.to_string().contains("reserved for workspace tasks"));
    }

    #[test]
    fn selecting_a_package_includes_task_dependencies_only() {
        let temp = TempDir::new();
        let root = root_with_members(&temp, "\"packages/*\"");
        write_manifest(
            &root.join("packages/shared"),
            "[package]\nname = \"shared\"\n\n[tasks.build]\ncommand = [\"echo\", \"shared\"]\n",
        );
        write_manifest(
            &root.join("packages/app"),
            "[package]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"echo\", \"app\"]\ndepends_on = [\"shared:build\"]\n",
        );
        write_manifest(
            &root.join("packages/unrelated"),
            "[package]\nname = \"unrelated\"\n\n[tasks.build]\ncommand = [\"echo\", \"unrelated\"]\n",
        );

        let workspace = Workspace::load(&root).expect("workspace loads");
        let plan = workspace
            .plan(Some("app"), None, &[])
            .expect("plan succeeds");

        assert_eq!(
            plan.iter()
                .map(|task| task.package.as_str())
                .collect::<Vec<_>>(),
            vec!["shared", "app"]
        );
    }

    #[test]
    fn supports_custom_pipeline_task_names_and_task_environment() {
        let temp = TempDir::new();
        let root = root_with_members(&temp, "\"packages/*\"");
        fs::write(
            config_path(&root),
            "[workspace]\nname = \"test\"\nmembers = [\"packages/*\"]\ndefault_pipeline = \"docs\"\n\n[pipelines.docs]\ntasks = [\"generate\"]\n",
        )
        .expect("rewrite root manifest");
        write_manifest(
            &root.join("packages/docs"),
            "[package]\nname = \"docs\"\n\n[tasks.generate]\ncommand = [\"make\", \"docs\"]\ncwd = \"site\"\nenv = { MODE = \"check\" }\n",
        );
        fs::create_dir_all(root.join("packages/docs/site")).expect("create task cwd");

        let workspace = Workspace::load(&root).expect("workspace loads");
        let plan = workspace.plan(None, None, &[]).expect("plan succeeds");

        assert_eq!(plan[0].task(), "generate");
        assert_eq!(
            plan[0].cwd(),
            fs::canonicalize(root.join("packages/docs/site")).expect("canonical task cwd")
        );
        assert_eq!(plan[0].env()["MODE"], "check");
        assert_eq!(plan[0].timeout(), Duration::from_secs(600));
        assert_eq!(plan[0].resource_group(), None);
    }

    #[test]
    fn plans_timeout_and_resource_group_settings() {
        let temp = TempDir::new();
        let root = root_with_members(&temp, "\"packages/*\"");
        write_manifest(
            &root.join("packages/app"),
            "[package]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"echo\", \"app\"]\ntimeout_seconds = 30\nresource_group = \"integration\"\n",
        );

        let workspace = Workspace::load(&root).expect("workspace loads");
        let plan = workspace.plan(None, None, &[]).expect("plan succeeds");

        assert_eq!(plan[0].timeout(), Duration::from_secs(30));
        assert_eq!(plan[0].resource_group(), Some("integration"));
    }

    #[test]
    fn rejects_cacheable_tasks_without_inputs() {
        let temp = TempDir::new();
        let root = root_with_members(&temp, "\"packages/*\"");
        write_manifest(
            &root.join("packages/app"),
            "[package]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"echo\", \"app\"]\ncache = true\noutputs = [\"dist/**\"]\n",
        );

        let error = Workspace::load(&root).expect_err("cacheable task needs inputs");
        assert!(error.to_string().contains("input pattern"));
    }

    #[test]
    fn rejects_zero_timeout() {
        let temp = TempDir::new();
        let root = root_with_members(&temp, "\"packages/*\"");
        write_manifest(
            &root.join("packages/app"),
            "[package]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"echo\", \"app\"]\ntimeout_seconds = 0\n",
        );

        let error = Workspace::load(&root).expect_err("zero timeout must be rejected");
        assert!(error.to_string().contains("timeout_seconds"));
    }

    #[test]
    fn rejects_empty_resource_group() {
        let temp = TempDir::new();
        let root = root_with_members(&temp, "\"packages/*\"");
        write_manifest(
            &root.join("packages/app"),
            "[package]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"echo\", \"app\"]\nresource_group = \"\"\n",
        );

        let error = Workspace::load(&root).expect_err("empty resource group must be rejected");
        assert!(error.to_string().contains("resource_group"));
    }

    #[test]
    fn rejects_task_cycles_with_the_cycle_path() {
        let temp = TempDir::new();
        let root = root_with_members(&temp, "\"packages/*\"");
        write_manifest(
            &root.join("packages/app"),
            "[package]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"echo\", \"app\"]\ndepends_on = [\"test\"]\n\n[tasks.test]\ncommand = [\"echo\", \"app\"]\ndepends_on = [\"build\"]\n",
        );

        let error = Workspace::load(&root).expect_err("cycle must be rejected");
        assert!(matches!(error, WorkspaceError::TaskCycle { .. }));
        assert!(
            error
                .to_string()
                .contains("app:build -> app:test -> app:build")
        );
    }

    #[test]
    fn rejects_a_missing_task_working_directory_during_load() {
        let temp = TempDir::new();
        let root = root_with_members(&temp, "\"packages/*\"");
        write_manifest(
            &root.join("packages/app"),
            "[package]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"echo\", \"app\"]\ncwd = \"missing\"\n",
        );

        let error = Workspace::load(&root).expect_err("missing cwd must be rejected");
        assert!(error.to_string().contains("missing"));
    }

    #[test]
    fn rejects_process_values_containing_nul() {
        let temp = TempDir::new();
        let root = root_with_members(&temp, "\"packages/*\"");
        write_manifest(
            &root.join("packages/app"),
            "[package]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"echo\", \"bad\\u0000value\"]\n",
        );

        let error = Workspace::load(&root).expect_err("NUL command argument must be rejected");
        assert!(error.to_string().contains("NUL"));
    }

    #[test]
    fn rejects_a_qualified_root_outside_package_selection() {
        let temp = TempDir::new();
        let root = root_with_members(&temp, "\"packages/*\"");
        write_manifest(
            &root.join("packages/shared"),
            "[package]\nname = \"shared\"\n\n[tasks.build]\ncommand = [\"echo\", \"shared\"]\n",
        );
        write_manifest(
            &root.join("packages/app"),
            "[package]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"echo\", \"app\"]\n",
        );
        let workspace = Workspace::load(&root).expect("workspace loads");

        let error = workspace
            .plan(Some("app"), None, &["shared:build".to_owned()])
            .expect_err("selection conflict must be explicit");
        assert!(matches!(
            error,
            WorkspaceError::TaskOutsideSelectedPackage { .. }
        ));
    }
}
