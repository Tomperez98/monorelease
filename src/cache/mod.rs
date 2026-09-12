//! Local content-addressed task caching.

use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::fmt;
use std::fs;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::config_path;
use crate::project::PlannedTask;
use crate::runner::{CapturedOutput, TaskResult};

mod hash;
mod pattern;

use hash::{
    ensure_inside, ensure_no_symlink_components, file_digest, file_mode, hash_bytes, hash_file,
    hash_string, hash_strings, hex_digest, read_file, set_mode, validate_cached_path,
};
use pattern::{CachePatterns, collect_files};

const CACHE_FORMAT_VERSION: u32 = 3;
const CACHE_GITIGNORE: &str = "*\n!.gitignore\n";

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

/// Immutable cache state shared by every task in one execution.
///
/// Manifests, the ambient environment, and cache-directory setup are project
/// state, not task state. Keeping them here prevents every cacheable task from
/// repeating the same filesystem work and keeps `std::env` out of the hasher.
#[derive(Debug, Clone)]
pub(crate) struct CacheSession {
    project_manifest: Vec<u8>,
    environment: BTreeMap<String, String>,
}

/// The cache interface consumed by the scheduler.
///
/// Production: [`CacheStore`].  Test: [`ScriptedCache`].
pub(crate) trait CacheBackend: Send + Sync {
    fn prepare(
        &self,
        project_root: &Path,
        environment: BTreeMap<String, String>,
    ) -> Result<CacheSession, CacheError>;

    fn task_key(
        &self,
        session: &CacheSession,
        task: &PlannedTask,
        dependency_keys: &[String],
    ) -> Result<String, CacheError>;

    fn lookup(&self, task: &PlannedTask, key: &str) -> Result<Option<TaskResult>, CacheError>;

    fn store(&self, task: &PlannedTask, key: &str, result: &TaskResult) -> Result<(), CacheError>;
}

impl CacheBackend for CacheStore {
    fn prepare(
        &self,
        project_root: &Path,
        environment: BTreeMap<String, String>,
    ) -> Result<CacheSession, CacheError> {
        self.prepare(project_root, environment)
    }

    fn task_key(
        &self,
        session: &CacheSession,
        task: &PlannedTask,
        dependency_keys: &[String],
    ) -> Result<String, CacheError> {
        self.task_key_with_session(session, task, dependency_keys)
    }

    fn lookup(&self, task: &PlannedTask, key: &str) -> Result<Option<TaskResult>, CacheError> {
        self.lookup(task, key)
    }

    fn store(&self, task: &PlannedTask, key: &str, result: &TaskResult) -> Result<(), CacheError> {
        self.store(task, key, result)
    }
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
    sha256: String,
    #[serde(default)]
    mode: Option<u32>,
}

fn select_cache_environment(
    task: &PlannedTask,
    ambient: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    if task.cache_env().iter().any(|variable| variable == "*") {
        let mut environment = ambient.clone();
        environment.extend(task.env().clone());
        return environment;
    }

    task.cache_env()
        .iter()
        .map(|variable| {
            let value = task
                .env()
                .get(variable)
                .cloned()
                .or_else(|| ambient.get(variable).cloned())
                .unwrap_or_else(|| "<unset>".to_owned());
            (variable.clone(), value)
        })
        .collect()
}

fn valid_metadata(metadata: &CacheMetadata, key: &str) -> bool {
    metadata.version == CACHE_FORMAT_VERSION && metadata.key == key
}

impl CacheStore {
    pub(crate) fn new(project_root: &Path) -> Self {
        Self {
            root: project_root.join(".mono").join("cache"),
        }
    }

    pub(crate) fn prepare(
        &self,
        project_root: &Path,
        environment: BTreeMap<String, String>,
    ) -> Result<CacheSession, CacheError> {
        self.ensure_gitignore()?;
        let project_manifest = read_file(&config_path(project_root))?;
        Ok(CacheSession {
            project_manifest,
            environment,
        })
    }

    /// Compatibility helper for tests and callers that key one task outside a run.
    #[cfg(test)]
    pub(crate) fn task_key(
        &self,
        project_root: &Path,
        task: &PlannedTask,
        dependency_keys: &[String],
    ) -> Result<String, CacheError> {
        let session = self.prepare(project_root, BTreeMap::new())?;
        self.task_key_with_session(&session, task, dependency_keys)
    }

    pub(crate) fn task_key_with_session(
        &self,
        session: &CacheSession,
        task: &PlannedTask,
        dependency_keys: &[String],
    ) -> Result<String, CacheError> {
        let mut hasher = Sha256::new();
        hash_string(&mut hasher, "mono-cache");
        hash_string(&mut hasher, &CACHE_FORMAT_VERSION.to_string());
        hash_string(&mut hasher, std::env::consts::OS);
        hash_string(&mut hasher, std::env::consts::ARCH);
        hash_string(&mut hasher, task.id());
        hash_string(
            &mut hasher,
            &task
                .cwd()
                .strip_prefix(task.root())
                .map_err(|_| CacheError::Invalid {
                    message: format!(
                        "task '{}' working directory is outside the project root",
                        task.id()
                    ),
                })?
                .to_string_lossy(),
        );
        hash_strings(&mut hasher, task.command());
        hash_string(&mut hasher, &task.timeout().as_secs().to_string());
        hash_string(&mut hasher, &task.max_output_bytes().to_string());
        hash_string(&mut hasher, &task.retries().to_string());
        hash_string(&mut hasher, &task.retry_backoff().as_secs().to_string());
        hash_string(&mut hasher, task.resource_group().unwrap_or(""));
        hash_strings(&mut hasher, task.inputs());
        hash_strings(&mut hasher, task.outputs());
        hash_strings(&mut hasher, task.cache_env());
        hash_strings(&mut hasher, dependency_keys);

        hash_bytes(&mut hasher, "project-manifest", &session.project_manifest);

        for (variable, value) in select_cache_environment(task, &session.environment) {
            hash_string(&mut hasher, &variable);
            hash_string(&mut hasher, &value);
        }

        let input_files = collect_files(task.root(), task.inputs(), "input")?;
        let outputs = CachePatterns::compile(task.outputs());
        let input_files = input_files
            .into_iter()
            .filter(|path| !outputs.matches(path))
            .collect::<Vec<_>>();
        for relative_path in input_files {
            hash_string(&mut hasher, &relative_path);
            hash_file(
                &mut hasher,
                &relative_path,
                &task.root().join(&relative_path),
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
        if !valid_metadata(&metadata, key) {
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
            if file_digest(&source)? != output.sha256 {
                return Ok(None);
            }
            let destination = task.root().join(&relative_path);
            ensure_inside(task.root(), &destination)?;
            ensure_no_symlink_components(task.root(), &destination)?;
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
        match fs::rename(&temporary, &entry) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_dir_all(&temporary);
                Ok(())
            }
            Err(source) => Err(CacheError::io(entry, source)),
        }
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

        let output_paths = collect_files(task.root(), task.outputs(), "output")?;
        let mut outputs = Vec::new();
        for relative_path in output_paths {
            let source = task.root().join(&relative_path);
            let destination = directory.join("outputs").join(&relative_path);
            ensure_inside(task.root(), &source)?;
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)
                    .map_err(|source_error| CacheError::io(parent.to_path_buf(), source_error))?;
            }
            fs::copy(&source, &destination)
                .map_err(|source_error| CacheError::io(source.clone(), source_error))?;
            outputs.push(CachedOutput {
                path: relative_path,
                sha256: file_digest(&source)?,
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

    fn ensure_gitignore(&self) -> Result<(), CacheError> {
        let directory = self.root.parent().expect("cache root has a .mono parent");
        fs::create_dir_all(directory)
            .map_err(|source| CacheError::io(directory.to_path_buf(), source))?;

        let path = directory.join(".gitignore");
        match fs::read(&path) {
            Ok(contents) if contents == CACHE_GITIGNORE.as_bytes() => Ok(()),
            Ok(_) => {
                fs::write(&path, CACHE_GITIGNORE).map_err(|source| CacheError::io(path, source))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::write(&path, CACHE_GITIGNORE).map_err(|source| CacheError::io(path, source))
            }
            Err(source) => Err(CacheError::io(path, source)),
        }
    }
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
impl CacheSession {
    /// A session that requires no filesystem access.
    ///
    /// Useful for fake-cache implementations that must return a valid session
    /// without reading a real manifest or environment.
    pub(crate) fn for_test() -> Self {
        Self {
            project_manifest: Vec::new(),
            environment: BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::config_path;
    use crate::project::Project;
    use crate::testing::{TempDir, create_symlink_or_skip};
    use std::fs;

    fn project_with_manifest(temp: &TempDir, manifest: &str) -> Project {
        let task_manifest = manifest.replacen("[project]\nname = \"app\"\n\n", "", 1);
        let contents = format!(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n{task_manifest}"
        );
        fs::write(config_path(temp.path()), contents).expect("write project manifest");
        Project::load(temp.path()).expect("project loads")
    }

    fn only_task(project: &Project) -> PlannedTask {
        project.plan(None, &[]).expect("plan succeeds").remove(0)
    }

    #[test]
    fn cache_environment_selection_preserves_override_and_unset_rules() {
        let temp = TempDir::new();
        let project = project_with_cache_env(
            &temp,
            "\"MODE\", \"MISSING\"",
            "env = { MODE = \"task\" }\n",
        );
        let task = only_task(&project);
        let ambient = BTreeMap::from([("MODE".to_owned(), "ambient".to_owned())]);

        assert_eq!(
            select_cache_environment(&task, &ambient),
            BTreeMap::from([
                ("MODE".to_owned(), "task".to_owned()),
                ("MISSING".to_owned(), "<unset>".to_owned()),
            ])
        );
    }

    #[test]
    fn wildcard_cache_environment_merges_ambient_values_and_task_overrides() {
        let temp = TempDir::new();
        let project = project_with_cache_env(&temp, "\"*\"", "env = { MODE = \"task\" }\n");
        let task = only_task(&project);
        let ambient = BTreeMap::from([
            ("MODE".to_owned(), "ambient".to_owned()),
            ("CI".to_owned(), "true".to_owned()),
        ]);

        assert_eq!(
            select_cache_environment(&task, &ambient),
            BTreeMap::from([
                ("CI".to_owned(), "true".to_owned()),
                ("MODE".to_owned(), "task".to_owned()),
            ])
        );
    }

    #[test]
    fn cache_metadata_requires_the_current_version_and_requested_key() {
        let valid = CacheMetadata {
            version: CACHE_FORMAT_VERSION,
            key: "key".to_owned(),
            outputs: Vec::new(),
        };
        let wrong_version = CacheMetadata {
            version: CACHE_FORMAT_VERSION + 1,
            key: "key".to_owned(),
            outputs: Vec::new(),
        };
        let wrong_key = CacheMetadata {
            version: CACHE_FORMAT_VERSION,
            key: "other".to_owned(),
            outputs: Vec::new(),
        };

        assert!(valid_metadata(&valid, "key"));
        assert!(!valid_metadata(&valid, "other"));
        assert!(!valid_metadata(&wrong_version, "key"));
        assert!(!valid_metadata(&wrong_key, "key"));
    }

    fn project_with_cache_env(temp: &TempDir, cache_env: &str, task_env: &str) -> Project {
        fs::write(
            config_path(temp.path()),
            format!(
                "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"sh\", \"-c\", \"cat input.txt > output.txt\"]\ncache = true\ninputs = [\"input.txt\"]\noutputs = [\"output.txt\"]\ncache_env = [{cache_env}]\n{task_env}"
            ),
        )
        .expect("write root manifest");
        fs::write(temp.path().join("input.txt"), "input").expect("write input");
        Project::load(temp.path()).expect("project loads")
    }

    fn key_with_environment(
        store: &CacheStore,
        project: &Project,
        task: &PlannedTask,
        environment: BTreeMap<String, String>,
    ) -> String {
        let session = store
            .prepare(&project.root, environment)
            .expect("cache session prepares");
        store
            .task_key_with_session(&session, task, &[])
            .expect("key succeeds")
    }

    #[test]
    fn a_declared_cache_environment_variable_changes_the_key() {
        let temp = TempDir::new();
        let project = project_with_cache_env(&temp, "\"MODE\"", "");
        let task = only_task(&project);
        let store = CacheStore::new(&project.root);

        let debug = key_with_environment(
            &store,
            &project,
            &task,
            BTreeMap::from([("MODE".to_owned(), "debug".to_owned())]),
        );
        let release = key_with_environment(
            &store,
            &project,
            &task,
            BTreeMap::from([("MODE".to_owned(), "release".to_owned())]),
        );

        assert_ne!(debug, release);
    }

    #[test]
    fn an_unset_cache_environment_variable_hashes_as_unset() {
        let temp = TempDir::new();
        let project = project_with_cache_env(&temp, "\"MODE\"", "");
        let task = only_task(&project);
        let store = CacheStore::new(&project.root);

        let first = key_with_environment(&store, &project, &task, BTreeMap::new());
        let second = key_with_environment(&store, &project, &task, BTreeMap::new());

        assert_eq!(first, second);
    }

    #[test]
    fn a_task_environment_value_overrides_the_process_environment() {
        let temp = TempDir::new();
        let project = project_with_cache_env(&temp, "\"MODE\"", "env = { MODE = \"check\" }\n");
        let task = only_task(&project);
        let store = CacheStore::new(&project.root);

        let from_task = key_with_environment(
            &store,
            &project,
            &task,
            BTreeMap::from([("MODE".to_owned(), "debug".to_owned())]),
        );
        let from_task_again = key_with_environment(
            &store,
            &project,
            &task,
            BTreeMap::from([("MODE".to_owned(), "release".to_owned())]),
        );

        assert_eq!(
            from_task, from_task_again,
            "the task's own env must win over the ambient environment"
        );
    }

    #[test]
    fn a_wildcard_cache_environment_includes_every_variable() {
        let temp = TempDir::new();
        let project = project_with_cache_env(&temp, "\"*\"", "");
        let task = only_task(&project);
        let store = CacheStore::new(&project.root);

        let one = key_with_environment(
            &store,
            &project,
            &task,
            BTreeMap::from([("UNRELATED".to_owned(), "one".to_owned())]),
        );
        let two = key_with_environment(
            &store,
            &project,
            &task,
            BTreeMap::from([("UNRELATED".to_owned(), "two".to_owned())]),
        );

        assert_ne!(one, two);
    }

    #[test]
    fn ignores_files_outside_the_input_patterns() {
        let temp = TempDir::new();
        let project = project_with_manifest(
            &temp,
            "[project]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"echo\", \"app\"]\ncache = true\ninputs = [\"src/**\"]\noutputs = [\"dist/**\"]\n",
        );
        let task = only_task(&project);
        let project_root = task.root().to_path_buf();
        fs::create_dir_all(project_root.join("src")).expect("create src");
        fs::create_dir_all(project_root.join("dist")).expect("create dist");
        fs::create_dir_all(project_root.join("target/build")).expect("create unrelated tree");
        fs::write(project_root.join("src/input.txt"), "input").expect("write input");
        fs::write(project_root.join("dist/output.txt"), "output").expect("write output");
        let store = CacheStore::new(&project.root);
        let first = store
            .task_key(&project.root, &task, &[])
            .expect("key succeeds");

        fs::write(project_root.join("target/build/object.o"), "unrelated")
            .expect("write unrelated file");

        let second = store
            .task_key(&project.root, &task, &[])
            .expect("key succeeds");
        assert_eq!(first, second);
    }

    #[test]
    fn honours_negated_input_patterns() {
        let temp = TempDir::new();
        let project = project_with_manifest(
            &temp,
            "[project]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"echo\", \"app\"]\ncache = true\ninputs = [\"src/**\", \"!src/skip.txt\"]\noutputs = [\"dist/**\"]\n",
        );
        let task = only_task(&project);
        let project_root = task.root().to_path_buf();
        fs::create_dir_all(project_root.join("src")).expect("create src");
        fs::create_dir_all(project_root.join("dist")).expect("create dist");
        fs::write(project_root.join("src/kept.txt"), "kept").expect("write input");
        fs::write(project_root.join("src/skip.txt"), "first").expect("write excluded input");
        let store = CacheStore::new(&project.root);
        let first = store
            .task_key(&project.root, &task, &[])
            .expect("key succeeds");

        fs::write(project_root.join("src/skip.txt"), "second").expect("change excluded input");

        let second = store
            .task_key(&project.root, &task, &[])
            .expect("key succeeds");
        assert_eq!(first, second);
    }

    #[test]
    fn excludes_outputs_from_the_input_fingerprint() {
        let temp = TempDir::new();
        let project = project_with_manifest(
            &temp,
            "[project]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"echo\", \"app\"]\ncache = true\ninputs = [\"**\"]\noutputs = [\"dist/**\"]\n",
        );
        let task = only_task(&project);
        let project_root = task.root().to_path_buf();
        fs::create_dir_all(project_root.join("src")).expect("create src");
        fs::create_dir_all(project_root.join("dist")).expect("create dist");
        fs::write(project_root.join("src/input.txt"), "input").expect("write input");
        fs::write(project_root.join("dist/output.txt"), "first").expect("write output");
        let store = CacheStore::new(&project.root);
        let first = store
            .task_key(&project.root, &task, &[])
            .expect("key succeeds");

        fs::write(project_root.join("dist/output.txt"), "second").expect("rewrite output");

        let second = store
            .task_key(&project.root, &task, &[])
            .expect("key succeeds");
        assert_eq!(first, second);
    }

    #[test]
    fn reports_an_input_pattern_that_matches_no_files() {
        let temp = TempDir::new();
        let project = project_with_manifest(
            &temp,
            "[project]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"echo\", \"app\"]\ncache = true\ninputs = [\"src/**\", \"missing/**\"]\noutputs = [\"dist/**\"]\n",
        );
        let task = only_task(&project);
        let project_root = task.root().to_path_buf();
        fs::create_dir_all(project_root.join("src")).expect("create src");
        fs::create_dir_all(project_root.join("dist")).expect("create dist");
        fs::write(project_root.join("src/input.txt"), "input").expect("write input");
        let store = CacheStore::new(&project.root);

        let error = store
            .task_key(&project.root, &task, &[])
            .expect_err("an unmatched pattern must fail");
        assert!(error.to_string().contains("missing/**"));
    }

    #[test]
    fn rejects_a_symlink_input_match() {
        let temp = TempDir::new();
        let project = project_with_manifest(
            &temp,
            "[project]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"echo\", \"app\"]\ncache = true\ninputs = [\"src/**\"]\noutputs = [\"dist/**\"]\n",
        );
        let task = only_task(&project);
        let project_root = task.root().to_path_buf();
        fs::create_dir_all(project_root.join("src")).expect("create src");
        fs::create_dir_all(project_root.join("dist")).expect("create dist");
        fs::write(project_root.join("src/input.txt"), "input").expect("write input");
        if !create_symlink_or_skip(Path::new("input.txt"), &project_root.join("src/link.txt")) {
            return;
        }
        let store = CacheStore::new(&project.root);

        let error = store
            .task_key(&project.root, &task, &[])
            .expect_err("symlinks cannot be fingerprinted");
        assert!(error.to_string().contains("unsupported symlink"));
    }

    #[test]
    fn cache_errors_expose_a_source_exactly_when_they_wrap_one() {
        let io_error = CacheError::io(
            PathBuf::from("entry"),
            std::io::Error::new(std::io::ErrorKind::NotFound, "missing"),
        );
        let json_error = CacheError::Json {
            path: PathBuf::from("metadata.json"),
            source: serde_json::from_str::<serde_json::Value>("{").unwrap_err(),
        };
        let invalid = CacheError::Invalid {
            message: "unsafe path".to_owned(),
        };

        assert!(io_error.source().is_some());
        assert!(json_error.source().is_some());
        assert!(invalid.source().is_none());
        assert!(!invalid.to_string().is_empty());
    }
}
