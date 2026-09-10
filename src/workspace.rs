//! Discovery, validation, and task-graph planning for a manifest-driven monorepo.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::config::{MonorepoConfig, PackageConfig, PipelineConfig, TaskConfig, config_path};

/// A validated package in a workspace.
#[derive(Debug, Clone)]
pub struct Package {
    pub name: String,
    pub path: PathBuf,
    pub tasks: BTreeMap<String, TaskConfig>,
}

/// A task node uniquely identifies one package task.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TaskNode {
    pub package: String,
    pub task: String,
}

/// A complete, validated workspace.
#[derive(Debug, Clone)]
pub struct Workspace {
    pub root: PathBuf,
    pub name: String,
    pub default_pipeline: String,
    pub pipelines: BTreeMap<String, PipelineConfig>,
    pub packages: BTreeMap<String, Package>,
}

/// One executable task in a deterministic task-DAG plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedTask {
    pub package: String,
    pub package_path: PathBuf,
    pub task: String,
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub depends_on: Vec<TaskNode>,
}

impl PlannedTask {
    pub fn node(&self) -> TaskNode {
        TaskNode {
            package: self.package.clone(),
            task: self.task.clone(),
        }
    }
}

impl Workspace {
    /// Find and load the root workspace at `start` or one of its ancestors.
    pub fn load(start: &Path) -> Result<Self, WorkspaceError> {
        let (root, root_config) = find_root(start)?;
        let workspace_config =
            root_config
                .workspace
                .ok_or_else(|| WorkspaceError::InvalidManifest {
                    path: config_path(&root),
                    message: "root manifest is missing [workspace]".to_owned(),
                })?;

        if root_config.package.is_some() || !root_config.tasks.is_empty() {
            return Err(WorkspaceError::InvalidManifest {
                path: config_path(&root),
                message: "root manifests may only contain [workspace] and [pipelines]".to_owned(),
            });
        }
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

                validate_package_config(&manifest_path, &package_config, &config.tasks)?;
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
            packages,
        };
        workspace.validate_graph()?;
        let pipeline_names = workspace.pipelines.keys().cloned().collect::<Vec<_>>();
        for pipeline_name in pipeline_names {
            workspace.plan(None, Some(&pipeline_name), &[])?;
        }
        Ok(workspace)
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
                if !package_names.contains(package) && selected_package.is_some() {
                    continue;
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
        for root in roots {
            self.visit_task(&root, &mut state, &mut ordered_nodes)?;
        }

        ordered_nodes
            .into_iter()
            .map(|node| self.planned_task(node))
            .collect()
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
        for package in self.packages.values() {
            for task_name in package.tasks.keys() {
                if task_name.is_empty() || task_name.contains(':') {
                    return Err(WorkspaceError::InvalidTaskName {
                        package: package.name.clone(),
                        task: task_name.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    fn selected_packages(
        &self,
        selected_package: Option<&str>,
    ) -> Result<BTreeSet<String>, WorkspaceError> {
        if let Some(selected) = selected_package {
            if !self.packages.contains_key(selected) {
                return Err(WorkspaceError::UnknownPackage {
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
        order: &mut Vec<TaskNode>,
    ) -> Result<(), WorkspaceError> {
        match state.get(node) {
            Some(VisitState::Visited) => return Ok(()),
            Some(VisitState::Visiting) => {
                return Err(WorkspaceError::TaskCycle { node: node.clone() });
            }
            None => {}
        }

        let task = self.task_config(node)?;
        state.insert(node.clone(), VisitState::Visiting);
        let dependencies = task
            .depends_on
            .iter()
            .map(|dependency| self.resolve_task_ref(node, dependency))
            .collect::<Result<Vec<_>, _>>()?;
        for dependency in &dependencies {
            self.visit_task(dependency, state, order)?;
        }
        state.insert(node.clone(), VisitState::Visited);
        order.push(node.clone());
        Ok(())
    }

    fn planned_task(&self, node: TaskNode) -> Result<PlannedTask, WorkspaceError> {
        let package =
            self.packages
                .get(&node.package)
                .ok_or_else(|| WorkspaceError::UnknownPackage {
                    name: node.package.clone(),
                })?;
        let task = package
            .tasks
            .get(&node.task)
            .ok_or_else(|| WorkspaceError::MissingTask {
                package: node.package.clone(),
                task: node.task.clone(),
            })?;
        let cwd = task.cwd.as_deref().unwrap_or(".");
        let cwd_path = if cwd == "." {
            package.path.clone()
        } else {
            package.path.join(cwd)
        };
        let depends_on = task
            .depends_on
            .iter()
            .map(|dependency| self.resolve_task_ref(&node, dependency))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(PlannedTask {
            package: package.name.clone(),
            package_path: package.path.clone(),
            task: node.task,
            command: task.command.clone(),
            cwd: cwd_path,
            env: task.env.clone(),
            depends_on,
        })
    }

    fn task_config(&self, node: &TaskNode) -> Result<&TaskConfig, WorkspaceError> {
        let package =
            self.packages
                .get(&node.package)
                .ok_or_else(|| WorkspaceError::UnknownPackage {
                    name: node.package.clone(),
                })?;
        package
            .tasks
            .get(&node.task)
            .ok_or_else(|| WorkspaceError::MissingTask {
                package: node.package.clone(),
                task: node.task.clone(),
            })
    }

    fn resolve_task_ref(
        &self,
        current: &TaskNode,
        reference: &str,
    ) -> Result<TaskNode, WorkspaceError> {
        if reference.is_empty() {
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
    config: &PackageConfig,
    tasks: &BTreeMap<String, TaskConfig>,
) -> Result<(), WorkspaceError> {
    if config.name.is_empty() || config.name.contains(':') {
        return Err(WorkspaceError::InvalidManifest {
            path: manifest_path.to_path_buf(),
            message: "package name must be non-empty and cannot contain ':'".to_owned(),
        });
    }

    for (task_name, task) in tasks {
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
        if task
            .cwd
            .as_deref()
            .is_some_and(|cwd| !valid_relative_path(cwd))
        {
            return Err(WorkspaceError::InvalidTask {
                package: config.name.clone(),
                task: task_name.clone(),
                message: "cwd must be a relative path without '..'".to_owned(),
            });
        }
    }
    Ok(())
}

fn valid_relative_path(path: &str) -> bool {
    let path = Path::new(path);
    !path.is_absolute()
        && path
            .components()
            .all(|component| !matches!(component, Component::ParentDir | Component::RootDir))
}

fn split_task_ref(reference: &str) -> Option<(&str, &str)> {
    reference.split_once(':')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VisitState {
    Visiting,
    Visited,
}

fn find_root(start: &Path) -> Result<(PathBuf, MonorepoConfig), WorkspaceError> {
    let start = fs::canonicalize(start).map_err(|source| WorkspaceError::Io {
        path: start.to_path_buf(),
        source,
    })?;
    let start_for_error = start.clone();
    let mut current = if start.is_dir() {
        start
    } else {
        start.parent().unwrap_or(Path::new("/")).to_path_buf()
    };

    loop {
        let manifest_path = config_path(&current);
        if manifest_path.is_file() {
            let config = read_manifest(&manifest_path)?;
            if config.workspace.is_some() {
                return Ok((current, config));
            }
        }

        if !current.pop() {
            break;
        }
    }

    Err(WorkspaceError::MissingRoot {
        start: start_for_error,
    })
}

fn read_manifest(path: &Path) -> Result<MonorepoConfig, WorkspaceError> {
    let contents = fs::read_to_string(path).map_err(|source| WorkspaceError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    MonorepoConfig::parse(&contents).map_err(|source| WorkspaceError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

fn expand_member_pattern(root: &Path, pattern: &str) -> Result<Vec<PathBuf>, WorkspaceError> {
    if pattern.is_empty() || pattern.starts_with('/') {
        return Err(WorkspaceError::InvalidMemberPattern {
            pattern: pattern.to_owned(),
        });
    }

    let segments: Vec<&str> = pattern
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.is_empty() || segments.contains(&"..") {
        return Err(WorkspaceError::InvalidMemberPattern {
            pattern: pattern.to_owned(),
        });
    }

    let mut matches = Vec::new();
    expand_segments(root, &segments, 0, &mut matches)?;
    matches.sort();
    matches.dedup();
    Ok(matches)
}

fn expand_segments(
    current: &Path,
    segments: &[&str],
    index: usize,
    matches: &mut Vec<PathBuf>,
) -> Result<(), WorkspaceError> {
    if index == segments.len() {
        if current.is_dir() {
            matches.push(current.to_path_buf());
        }
        return Ok(());
    }

    let segment = segments[index];
    if segment == "**" {
        expand_segments(current, segments, index + 1, matches)?;
        for entry in read_directories(current)? {
            expand_segments(&entry, segments, index, matches)?;
        }
        return Ok(());
    }

    if segment.contains('*') {
        for entry in read_directories(current)? {
            let name = entry
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if wildcard_matches(segment, name) {
                expand_segments(&entry, segments, index + 1, matches)?;
            }
        }
    } else {
        let next = current.join(segment);
        if next.is_dir() {
            expand_segments(&next, segments, index + 1, matches)?;
        }
    }
    Ok(())
}

fn read_directories(path: &Path) -> Result<Vec<PathBuf>, WorkspaceError> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(WorkspaceError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let mut directories = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| WorkspaceError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if entry
            .file_type()
            .map_err(|source| WorkspaceError::Io {
                path: entry.path(),
                source,
            })?
            .is_dir()
        {
            directories.push(entry.path());
        }
    }
    directories.sort();
    Ok(directories)
}

fn wildcard_matches(pattern: &str, value: &str) -> bool {
    fn matches(pattern: &[u8], value: &[u8]) -> bool {
        match pattern.split_first() {
            None => value.is_empty(),
            Some((b'*', rest)) => {
                matches(rest, value)
                    || value
                        .split_first()
                        .is_some_and(|(_, value_rest)| matches(pattern, value_rest))
            }
            Some((pattern_char, rest)) => {
                value.split_first().is_some_and(|(value_char, value_rest)| {
                    pattern_char == value_char && matches(rest, value_rest)
                })
            }
        }
    }

    matches(pattern.as_bytes(), value.as_bytes())
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
    },
    UnknownPipeline {
        name: String,
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
    },
    InvalidTaskReference {
        reference: String,
        from: TaskNode,
    },
    TaskCycle {
        node: TaskNode,
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
            Self::UnknownPackage { name } => write!(f, "unknown package '{name}'"),
            Self::UnknownPipeline { name } => write!(f, "unknown pipeline '{name}'"),
            Self::InvalidTaskName { package, task } => {
                write!(f, "invalid task name '{package}:{task}'")
            }
            Self::InvalidTask {
                package,
                task,
                message,
            } => write!(f, "invalid task '{package}:{task}': {message}"),
            Self::MissingTask { package, task } => {
                write!(f, "package '{package}' has no task '{task}'")
            }
            Self::InvalidTaskReference { reference, from } => write!(
                f,
                "invalid task reference '{reference}' from '{}:{}'",
                from.package, from.task
            ),
            Self::TaskCycle { node } => {
                write!(
                    f,
                    "task dependency cycle detected at '{}:{}'",
                    node.package, node.task
                )
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

        assert_eq!(plan[0].task, "generate");
        assert_eq!(
            plan[0].cwd,
            fs::canonicalize(root.join("packages/docs/site")).expect("canonical task cwd")
        );
        assert_eq!(plan[0].env["MODE"], "check");
    }

    #[test]
    fn rejects_task_cycles() {
        let temp = TempDir::new();
        let root = root_with_members(&temp, "\"packages/*\"");
        write_manifest(
            &root.join("packages/app"),
            "[package]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"echo\", \"app\"]\ndepends_on = [\"test\"]\n\n[tasks.test]\ncommand = [\"echo\", \"app\"]\ndepends_on = [\"build\"]\n",
        );

        assert!(matches!(
            Workspace::load(&root),
            Err(WorkspaceError::TaskCycle { .. })
        ));
    }
}
