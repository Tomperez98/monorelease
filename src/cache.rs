//! Local content-addressed task caching.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::runner::{CapturedOutput, TaskResult};
use crate::workspace::PlannedTask;

const CACHE_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheMode {
    ReadWrite,
    NoCache,
    Force,
}

#[derive(Debug, Clone)]
pub(crate) struct CacheStore {
    root: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheMetadata {
    version: u32,
    key: String,
    outputs: Vec<CachedOutput>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedOutput {
    path: String,
    #[serde(default)]
    mode: Option<u32>,
}

impl CacheStore {
    pub(crate) fn new(workspace_root: &Path) -> Self {
        Self {
            root: workspace_root.join(".monorelease").join("cache"),
        }
    }

    pub(crate) fn task_key(
        &self,
        workspace_root: &Path,
        task: &PlannedTask,
        dependency_keys: &[String],
    ) -> Result<String, CacheError> {
        let mut hasher = Sha256::new();
        hash_string(&mut hasher, "monorelease-cache");
        hash_string(&mut hasher, &CACHE_FORMAT_VERSION.to_string());
        hash_string(&mut hasher, task.package());
        hash_string(&mut hasher, task.task());
        hash_string(
            &mut hasher,
            &task
                .cwd()
                .strip_prefix(task.package_path())
                .map_err(|_| CacheError::Invalid {
                    message: format!(
                        "task '{}:{}' working directory is outside its package",
                        task.package(),
                        task.task()
                    ),
                })?
                .to_string_lossy(),
        );
        hash_strings(&mut hasher, task.command());
        hash_string(&mut hasher, &task.timeout().as_secs().to_string());
        hash_string(&mut hasher, task.resource_group().unwrap_or(""));
        hash_strings(&mut hasher, task.inputs());
        hash_strings(&mut hasher, task.outputs());
        hash_strings(&mut hasher, task.cache_env());
        hash_strings(&mut hasher, dependency_keys);

        hash_file(
            &mut hasher,
            "workspace-manifest",
            &workspace_root.join("monorepo.toml"),
        )?;
        if task.package_path() != workspace_root {
            hash_file(
                &mut hasher,
                "package-manifest",
                &task.package_path().join("monorepo.toml"),
            )?;
        }

        let mut environment = BTreeMap::new();
        if task.cache_env().iter().any(|variable| variable == "*") {
            environment.extend(std::env::vars());
            environment.extend(task.env().clone());
        } else {
            for variable in task.cache_env() {
                environment.insert(
                    variable.clone(),
                    task.env()
                        .get(variable)
                        .cloned()
                        .or_else(|| std::env::var(variable).ok())
                        .unwrap_or_else(|| "<unset>".to_owned()),
                );
            }
        }
        for (variable, value) in environment {
            hash_string(&mut hasher, &variable);
            hash_string(&mut hasher, &value);
        }

        let input_files = collect_files(task.package_path(), task.inputs())?;
        ensure_patterns_match(task.inputs(), &input_files, "input")?;
        reject_symlink_matches(task.package_path(), task.inputs(), "input")?;
        let input_files = input_files
            .into_iter()
            .filter(|path| !matches_patterns(path, task.outputs()))
            .collect::<Vec<_>>();
        for relative_path in input_files {
            hash_string(&mut hasher, &relative_path);
            hash_file(
                &mut hasher,
                &relative_path,
                &task.package_path().join(&relative_path),
            )?;
        }

        Ok(hex_digest(&hasher.finalize()))
    }

    pub(crate) fn lookup(
        &self,
        task: &PlannedTask,
        key: &str,
    ) -> Result<Option<TaskResult>, CacheError> {
        let started = Instant::now();
        let entry = self.entry_path(key);
        let metadata_path = entry.join("metadata.json");
        let metadata_contents = match fs::read(&metadata_path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(CacheError::io(metadata_path, source)),
        };
        let metadata: CacheMetadata = match serde_json::from_slice(&metadata_contents) {
            Ok(metadata) => metadata,
            Err(_) => return Ok(None),
        };
        if metadata.version != CACHE_FORMAT_VERSION || metadata.key != key {
            return Ok(None);
        }

        let stdout_path = entry.join("stdout");
        let stderr_path = entry.join("stderr");
        let stdout = match fs::read(&stdout_path) {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(CacheError::io(stdout_path, source)),
        };
        let stderr = match fs::read(&stderr_path) {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(CacheError::io(stderr_path, source)),
        };

        let mut outputs = Vec::new();
        for output in &metadata.outputs {
            let relative_path = validate_cached_path(&output.path)?;
            let source = entry.join("outputs").join(&relative_path);
            let source_metadata = match fs::metadata(&source) {
                Ok(metadata) if metadata.is_file() => metadata,
                Ok(_) => return Ok(None),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(source_error) => return Err(CacheError::io(source, source_error)),
            };
            let destination = task.package_path().join(&relative_path);
            ensure_inside(task.package_path(), &destination)?;
            outputs.push((relative_path, source, destination, source_metadata));
        }

        for (_, source, destination, _) in &outputs {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)
                    .map_err(|source_error| CacheError::io(parent.to_path_buf(), source_error))?;
            }
            fs::copy(source, destination)
                .map_err(|source_error| CacheError::io(destination.clone(), source_error))?;
        }
        for (metadata, (_, _, destination, _)) in metadata.outputs.iter().zip(&outputs) {
            if let Some(mode) = metadata.mode {
                set_mode(destination, mode)?;
            }
        }

        Ok(Some(TaskResult {
            output: CapturedOutput { stdout, stderr },
            elapsed: started.elapsed(),
            cached: true,
        }))
    }

    pub(crate) fn store(
        &self,
        task: &PlannedTask,
        key: &str,
        result: &TaskResult,
    ) -> Result<(), CacheError> {
        static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

        fs::create_dir_all(&self.root)
            .map_err(|source| CacheError::io(self.root.clone(), source))?;
        let temporary = self.root.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        if temporary.exists() {
            fs::remove_dir_all(&temporary)
                .map_err(|source| CacheError::io(temporary.clone(), source))?;
        }

        let result = self.store_in(&temporary, task, key, result);
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&temporary);
            return Err(error);
        }

        let entry = self.entry_path(key);
        if entry.exists() {
            let _ = fs::remove_dir_all(&temporary);
            return Ok(());
        }
        fs::rename(&temporary, &entry).map_err(|source| CacheError::io(entry, source))
    }

    fn store_in(
        &self,
        directory: &Path,
        task: &PlannedTask,
        key: &str,
        result: &TaskResult,
    ) -> Result<(), CacheError> {
        fs::create_dir_all(directory.join("outputs"))
            .map_err(|source| CacheError::io(directory.to_path_buf(), source))?;
        fs::write(directory.join("stdout"), &result.output.stdout)
            .map_err(|source| CacheError::io(directory.join("stdout"), source))?;
        fs::write(directory.join("stderr"), &result.output.stderr)
            .map_err(|source| CacheError::io(directory.join("stderr"), source))?;

        let output_paths = collect_files(task.package_path(), task.outputs())?;
        ensure_patterns_match(task.outputs(), &output_paths, "output")?;
        reject_symlink_matches(task.package_path(), task.outputs(), "output")?;
        let mut outputs = Vec::new();
        for relative_path in output_paths {
            let source = task.package_path().join(&relative_path);
            let destination = directory.join("outputs").join(&relative_path);
            ensure_inside(task.package_path(), &source)?;
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)
                    .map_err(|source_error| CacheError::io(parent.to_path_buf(), source_error))?;
            }
            fs::copy(&source, &destination)
                .map_err(|source_error| CacheError::io(source.clone(), source_error))?;
            outputs.push(CachedOutput {
                path: relative_path,
                mode: file_mode(&source)?,
            });
        }

        let metadata = CacheMetadata {
            version: CACHE_FORMAT_VERSION,
            key: key.to_owned(),
            outputs,
        };
        let contents = serde_json::to_vec_pretty(&metadata).map_err(|source| CacheError::Json {
            path: directory.join("metadata.json"),
            source,
        })?;
        fs::write(directory.join("metadata.json"), contents)
            .map_err(|source| CacheError::io(directory.join("metadata.json"), source))
    }

    fn entry_path(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }
}

#[derive(Debug, Default)]
struct CollectedPaths {
    files: BTreeSet<String>,
    symlinks: BTreeSet<String>,
}

fn collect_files(root: &Path, patterns: &[String]) -> Result<Vec<String>, CacheError> {
    let mut paths = CollectedPaths::default();
    walk_paths(root, root, &mut paths)?;
    Ok(paths
        .files
        .into_iter()
        .filter(|path| matches_patterns(path, patterns))
        .collect())
}

fn reject_symlink_matches(root: &Path, patterns: &[String], kind: &str) -> Result<(), CacheError> {
    let mut paths = CollectedPaths::default();
    walk_paths(root, root, &mut paths)?;
    if let Some(path) = paths
        .symlinks
        .into_iter()
        .find(|path| matches_patterns(path, patterns))
    {
        return Err(CacheError::Invalid {
            message: format!("{kind} pattern matches unsupported symlink '{path}'"),
        });
    }
    Ok(())
}

fn ensure_patterns_match(
    patterns: &[String],
    files: &[String],
    kind: &str,
) -> Result<(), CacheError> {
    for pattern in patterns.iter().filter(|pattern| !pattern.starts_with('!')) {
        if !files.iter().any(|path| pattern_matches(path, pattern)) {
            return Err(CacheError::Invalid {
                message: format!("{kind} pattern '{pattern}' matched no files"),
            });
        }
    }
    Ok(())
}

fn walk_paths(root: &Path, current: &Path, paths: &mut CollectedPaths) -> Result<(), CacheError> {
    let entries =
        fs::read_dir(current).map_err(|source| CacheError::io(current.to_path_buf(), source))?;
    for entry in entries {
        let entry = entry.map_err(|source| CacheError::io(current.to_path_buf(), source))?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .expect("walked path must remain below its root");
        if relative
            .components()
            .any(|component| matches!(component, std::path::Component::Normal(name) if name == ".git" || name == ".monorelease"))
        {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|source| CacheError::io(path.clone(), source))?;
        if file_type.is_dir() {
            walk_paths(root, &path, paths)?;
        } else if file_type.is_file() {
            paths.files.insert(relative_path(relative));
        } else if file_type.is_symlink() {
            paths.symlinks.insert(relative_path(relative));
        }
    }
    Ok(())
}

fn matches_patterns(path: &str, patterns: &[String]) -> bool {
    let mut selected = false;
    for pattern in patterns {
        let (exclude, pattern) = pattern
            .strip_prefix('!')
            .map_or((false, pattern.as_str()), |pattern| (true, pattern));
        if pattern_matches(path, pattern) {
            selected = !exclude;
        }
    }
    selected
}

fn pattern_matches(path: &str, pattern: &str) -> bool {
    let path = path.split('/').collect::<Vec<_>>();
    let pattern = pattern.split('/').collect::<Vec<_>>();
    match_segments(&path, &pattern)
}

fn match_segments(path: &[&str], pattern: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((segment, rest)) if *segment == "**" => {
            match_segments(path, rest)
                || path
                    .split_first()
                    .is_some_and(|(_, remaining)| match_segments(remaining, pattern))
        }
        Some((segment, rest)) => path.split_first().is_some_and(|(value, remaining)| {
            segment_matches(value, segment) && match_segments(remaining, rest)
        }),
    }
}

fn segment_matches(value: &str, pattern: &str) -> bool {
    fn matches(value: &[u8], pattern: &[u8]) -> bool {
        match pattern.split_first() {
            None => value.is_empty(),
            Some((b'*', rest)) => {
                matches(value, rest)
                    || value
                        .split_first()
                        .is_some_and(|(_, remaining)| matches(remaining, pattern))
            }
            Some((character, rest)) => {
                value
                    .split_first()
                    .is_some_and(|(value_character, remaining)| {
                        character == value_character && matches(remaining, rest)
                    })
            }
        }
    }

    matches(value.as_bytes(), pattern.as_bytes())
}

fn relative_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn validate_cached_path(path: &str) -> Result<PathBuf, CacheError> {
    let relative = Path::new(path);
    if path.is_empty()
        || relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::RootDir
            )
        })
    {
        return Err(CacheError::Invalid {
            message: format!("cache entry contains unsafe output path '{path}'"),
        });
    }
    Ok(relative.to_path_buf())
}

fn ensure_inside(root: &Path, path: &Path) -> Result<(), CacheError> {
    if path.starts_with(root) {
        Ok(())
    } else {
        Err(CacheError::Invalid {
            message: format!("cache path escapes package root: {}", path.display()),
        })
    }
}

fn hash_strings(hasher: &mut Sha256, values: &[String]) {
    hash_string(hasher, &values.len().to_string());
    for value in values {
        hash_string(hasher, value);
    }
}

fn hash_string(hasher: &mut Sha256, value: &str) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}

fn hash_file(hasher: &mut Sha256, label: &str, path: &Path) -> Result<(), CacheError> {
    let contents = fs::read(path).map_err(|source| CacheError::io(path.to_path_buf(), source))?;
    hash_string(hasher, label);
    hasher.update((contents.len() as u64).to_le_bytes());
    hasher.update(contents);
    Ok(())
}

fn hex_digest(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn file_mode(path: &Path) -> Result<Option<u32>, CacheError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(Some(
            fs::metadata(path)
                .map_err(|source| CacheError::io(path.to_path_buf(), source))?
                .permissions()
                .mode(),
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
}

fn set_mode(path: &Path, mode: u32) -> Result<(), CacheError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = fs::Permissions::from_mode(mode);
        fs::set_permissions(path, permissions)
            .map_err(|source| CacheError::io(path.to_path_buf(), source))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

#[derive(Debug)]
pub enum CacheError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    Invalid {
        message: String,
    },
}

impl CacheError {
    fn io(path: PathBuf, source: std::io::Error) -> Self {
        Self::Io { path, source }
    }
}

impl fmt::Display for CacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "cache could not access {}: {source}", path.display())
            }
            Self::Json { path, source } => {
                write!(f, "cache could not parse {}: {source}", path.display())
            }
            Self::Invalid { message } => write!(f, "invalid cache entry: {message}"),
        }
    }
}

impl StdError for CacheError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Json { source, .. } => Some(source),
            Self::Invalid { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::config_path;
    use crate::runner::Runner;
    use crate::testing::TempDir;
    use crate::workspace::Workspace;
    use std::fs;

    fn workspace_with_task(temp: &TempDir) -> Workspace {
        fs::write(
            config_path(temp.path()),
            "[workspace]\nname = \"fixture\"\nmembers = [\"packages/*\"]\n\n[pipelines.ci]\ntasks = [\"build\"]\n",
        )
        .expect("write root manifest");
        let package = temp.path().join("packages/app");
        fs::create_dir_all(&package).expect("create package");
        fs::write(
            config_path(&package),
            "[package]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"sh\", \"-c\", \"cat input.txt > output.txt\"]\ncache = true\ninputs = [\"input.txt\"]\noutputs = [\"output.txt\"]\n",
        )
        .expect("write package manifest");
        fs::write(package.join("input.txt"), "input").expect("write input");
        Workspace::load(temp.path()).expect("workspace loads")
    }

    #[cfg(unix)]
    #[test]
    fn stores_and_restores_a_successful_task() {
        let temp = TempDir::new();
        let workspace = workspace_with_task(&temp);
        let task = workspace
            .plan(None, None, &[])
            .expect("plan succeeds")
            .remove(0);
        let store = CacheStore::new(&workspace.root);
        let key = store
            .task_key(&workspace.root, &task, &[])
            .expect("key succeeds");
        let result = Runner::new()
            .run(&workspace.root, &task)
            .expect("task succeeds");
        store.store(&task, &key, &result).expect("store succeeds");
        fs::remove_file(task.package_path().join("output.txt")).expect("remove output");

        let restored = store
            .lookup(&task, &key)
            .expect("lookup succeeds")
            .expect("cache hit");
        assert_eq!(restored.output.stdout, result.output.stdout);
        assert_eq!(
            fs::read_to_string(task.package_path().join("output.txt")).expect("read output"),
            "input"
        );
    }

    #[test]
    fn changing_an_input_changes_the_key() {
        let temp = TempDir::new();
        let workspace = workspace_with_task(&temp);
        let task = workspace
            .plan(None, None, &[])
            .expect("plan succeeds")
            .remove(0);
        let store = CacheStore::new(&workspace.root);
        let first = store
            .task_key(&workspace.root, &task, &[])
            .expect("key succeeds");
        fs::write(task.package_path().join("input.txt"), "changed").expect("change input");
        let second = store
            .task_key(&workspace.root, &task, &[])
            .expect("key succeeds");
        assert_ne!(first, second);
    }
}
