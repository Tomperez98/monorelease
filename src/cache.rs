//! Local content-addressed task caching.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::runner::{CapturedOutput, TaskResult};
use crate::workspace::PlannedTask;

const CACHE_FORMAT_VERSION: u32 = 3;
const CACHE_GITIGNORE: &str = "*\n!.gitignore\n";
const HASH_BUFFER_SIZE: usize = 64 * 1024;
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

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
/// Manifests and cache-directory setup are workspace state, not task state. Keeping
/// them here prevents every cacheable task from repeating the same filesystem work.
#[derive(Debug, Clone)]
pub(crate) struct CacheSession {
    project_manifest: Vec<u8>,
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

impl CacheStore {
    pub(crate) fn new(workspace_root: &Path) -> Self {
        Self {
            root: workspace_root.join(".mono").join("cache"),
        }
    }

    pub(crate) fn prepare(
        &self,
        workspace_root: &Path,
        _plan: &[PlannedTask],
    ) -> Result<CacheSession, CacheError> {
        self.ensure_gitignore()?;
        let project_manifest = read_file(&workspace_root.join("mono.toml"))?;
        Ok(CacheSession { project_manifest })
    }

    /// Compatibility helper for tests and callers that key one task outside a run.
    #[cfg(test)]
    pub(crate) fn task_key(
        &self,
        workspace_root: &Path,
        task: &PlannedTask,
        dependency_keys: &[String],
    ) -> Result<String, CacheError> {
        let session = self.prepare(workspace_root, std::slice::from_ref(task))?;
        self.task_key_with_session(&session, workspace_root, task, dependency_keys)
    }

    pub(crate) fn task_key_with_session(
        &self,
        session: &CacheSession,
        _workspace_root: &Path,
        task: &PlannedTask,
        dependency_keys: &[String],
    ) -> Result<String, CacheError> {
        let mut hasher = Sha256::new();
        hash_string(&mut hasher, "mono-cache");
        hash_string(&mut hasher, &CACHE_FORMAT_VERSION.to_string());
        hash_string(&mut hasher, std::env::consts::OS);
        hash_string(&mut hasher, std::env::consts::ARCH);
        hash_string(&mut hasher, task.task());
        hash_string(
            &mut hasher,
            &task
                .cwd()
                .strip_prefix(task.root())
                .map_err(|_| CacheError::Invalid {
                    message: format!(
                        "task '{}' working directory is outside the project root",
                        task.task()
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

#[derive(Debug, Default)]
struct CollectedPaths {
    files: Vec<String>,
    first_symlink: Option<String>,
    matched_positive_patterns: BTreeSet<usize>,
}

/// Cache patterns compiled into path segments once, so a walk never re-splits
/// them for every entry it visits.
struct CachePatterns<'a> {
    patterns: Vec<CachePattern<'a>>,
}

struct CachePattern<'a> {
    source: &'a str,
    exclude: bool,
    segments: Vec<&'a str>,
}

impl<'a> CachePatterns<'a> {
    fn compile(patterns: &'a [String]) -> Self {
        let patterns = patterns
            .iter()
            .map(|pattern| match pattern.strip_prefix('!') {
                Some(source) => CachePattern {
                    source,
                    exclude: true,
                    segments: source.split('/').collect(),
                },
                None => CachePattern {
                    source: pattern.as_str(),
                    exclude: false,
                    segments: pattern.split('/').collect(),
                },
            })
            .collect();
        Self { patterns }
    }

    /// Select a path the same way the manifest's ordered pattern list does.
    fn matches(&self, path: &str) -> bool {
        let path = path.split('/').collect::<Vec<_>>();
        let mut selected = false;
        for pattern in &self.patterns {
            if match_segments(&path, &pattern.segments) {
                selected = !pattern.exclude;
            }
        }
        selected
    }

    fn matches_and_record(
        &self,
        path: &str,
        matched_positive_patterns: &mut BTreeSet<usize>,
    ) -> bool {
        let path = path.split('/').collect::<Vec<_>>();
        let mut selected = false;
        for (index, pattern) in self.patterns.iter().enumerate() {
            if match_segments(&path, &pattern.segments) {
                selected = !pattern.exclude;
                if !pattern.exclude {
                    matched_positive_patterns.insert(index);
                }
            }
        }
        selected
    }

    /// Whether any positive pattern can still match a path below `directory`.
    ///
    /// Reaching a path requires every one of its leading segments to line up
    /// with a pattern, so a directory no positive pattern can reach is never
    /// walked. A project full of unrelated build output then costs one
    /// `read_dir` per pattern prefix instead of a full-tree traversal.
    fn could_match_below(&self, directory: &str) -> bool {
        let directory = directory.split('/').collect::<Vec<_>>();
        self.patterns
            .iter()
            .any(|pattern| !pattern.exclude && prefix_matches(&pattern.segments, &directory))
    }
}

/// Collect every file under `root` that `patterns` select, rejecting the
/// symlink matches that content addressing cannot represent.
///
/// One traversal enforces the whole pattern contract: the same "matched no
/// files" error and the same "matched a symlink" error a caller would get
/// from collecting, checking, and rejecting separately.
fn collect_files(root: &Path, patterns: &[String], kind: &str) -> Result<Vec<String>, CacheError> {
    let patterns = CachePatterns::compile(patterns);
    let mut paths = CollectedPaths::default();
    walk_matched(root, root, &patterns, &mut paths)?;
    paths.files.sort_unstable();
    paths.files.dedup();
    for (index, pattern) in patterns.patterns.iter().enumerate() {
        if !pattern.exclude && !paths.matched_positive_patterns.contains(&index) {
            return Err(CacheError::Invalid {
                message: format!("{kind} pattern '{}' matched no files", pattern.source),
            });
        }
    }
    if let Some(symlink) = paths.first_symlink {
        return Err(CacheError::Invalid {
            message: format!("{kind} pattern matches unsupported symlink '{symlink}'"),
        });
    }
    Ok(paths.files)
}

fn walk_matched(
    root: &Path,
    current: &Path,
    patterns: &CachePatterns<'_>,
    paths: &mut CollectedPaths,
) -> Result<(), CacheError> {
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
            .any(|component| matches!(component, std::path::Component::Normal(name) if name == ".git" || name == ".mono"))
        {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|source| CacheError::io(path.clone(), source))?;
        if file_type.is_dir() {
            let directory = relative_path(relative);
            if patterns.could_match_below(&directory) {
                walk_matched(root, &path, patterns, paths)?;
            }
        } else if file_type.is_file() {
            let relative = relative_path(relative);
            let selected =
                patterns.matches_and_record(&relative, &mut paths.matched_positive_patterns);
            if selected {
                paths.files.push(relative);
            }
        } else if file_type.is_symlink() {
            let relative = relative_path(relative);
            if patterns.matches(&relative)
                && paths
                    .first_symlink
                    .as_ref()
                    .is_none_or(|current| relative.as_str() < current.as_str())
            {
                paths.first_symlink = Some(relative);
            }
        }
    }
    Ok(())
}

/// Whether `directory` can be the leading part of a path `pattern` matches.
fn prefix_matches(pattern: &[&str], directory: &[&str]) -> bool {
    match_path_segments(directory, pattern, true)
}

fn match_segments(path: &[&str], pattern: &[&str]) -> bool {
    match_path_segments(path, pattern, false)
}

/// Match path segments with `**` using greedy backtracking rather than
/// recursive branching. `prefix` allows a directory to end before the pattern
/// does, because the remaining pattern may match descendants.
fn match_path_segments(path: &[&str], pattern: &[&str], prefix: bool) -> bool {
    let mut path_index = 0;
    let mut pattern_index = 0;
    let mut star_pattern = None;
    let mut star_path = 0;

    while path_index < path.len() {
        if pattern_index < pattern.len()
            && pattern[pattern_index] != "**"
            && segment_matches(path[path_index], pattern[pattern_index])
        {
            path_index += 1;
            pattern_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == "**" {
            star_pattern = Some(pattern_index);
            star_path = path_index;
            pattern_index += 1;
        } else if let Some(star_pattern_index) = star_pattern {
            star_path += 1;
            path_index = star_path;
            pattern_index = star_pattern_index + 1;
        } else {
            return false;
        }
    }

    prefix
        || pattern_index == pattern.len()
        || pattern[pattern_index..]
            .iter()
            .all(|segment| *segment == "**")
}

fn segment_matches(value: &str, pattern: &str) -> bool {
    let value = value.as_bytes();
    let pattern = pattern.as_bytes();
    let mut value_index = 0;
    let mut pattern_index = 0;
    let mut star_pattern = None;
    let mut star_value = 0;

    while value_index < value.len() {
        if pattern_index < pattern.len() && pattern[pattern_index] == value[value_index] {
            value_index += 1;
            pattern_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star_pattern = Some(pattern_index);
            star_value = value_index;
            pattern_index += 1;
        } else if let Some(star_pattern_index) = star_pattern {
            star_value += 1;
            value_index = star_value;
            pattern_index = star_pattern_index + 1;
        } else {
            return false;
        }
    }

    pattern[pattern_index..]
        .iter()
        .all(|character| *character == b'*')
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
            message: format!("cache path escapes project root: {}", path.display()),
        })
    }
}

/// Reject cache output restoration through any symlink component from the
/// project root down to (and including) the destination path.
///
/// Uses `symlink_metadata` instead of `metadata` so a symlink is detected
/// rather than followed.  The path may not exist yet — that is only the
/// destination file, not an intermediate directory, but we stop scanning at
/// the first missing component since earlier components must exist.
fn ensure_no_symlink_components(root: &Path, destination: &Path) -> Result<(), CacheError> {
    ensure_inside(root, destination)?;

    let relative = destination
        .strip_prefix(root)
        .map_err(|_| CacheError::Invalid {
            message: format!(
                "cache destination escapes project root: {}",
                destination.display()
            ),
        })?;

    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        let meta = match fs::symlink_metadata(&current) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => break,
            Err(source) => return Err(CacheError::io(current, source)),
        };
        if meta.file_type().is_symlink() {
            return Err(CacheError::Invalid {
                message: format!(
                    "cache output restoration refuses symlink component {}",
                    current.display()
                ),
            });
        }
    }
    Ok(())
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

/// Stream a file into the hasher so an oversized input never has to fit in
/// memory at once.
fn hash_file(hasher: &mut Sha256, label: &str, path: &Path) -> Result<(), CacheError> {
    let mut file =
        fs::File::open(path).map_err(|source| CacheError::io(path.to_path_buf(), source))?;
    let metadata = file
        .metadata()
        .map_err(|source| CacheError::io(path.to_path_buf(), source))?;
    hash_string(hasher, label);
    hasher.update(metadata.len().to_le_bytes());
    if let Some(mode) = file_mode(path)? {
        hasher.update(mode.to_le_bytes());
    }

    let mut buffer = vec![0u8; HASH_BUFFER_SIZE];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| CacheError::io(path.to_path_buf(), source))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(())
}

fn file_digest(path: &Path) -> Result<String, CacheError> {
    let mut file =
        fs::File::open(path).map_err(|source| CacheError::io(path.to_path_buf(), source))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; HASH_BUFFER_SIZE];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| CacheError::io(path.to_path_buf(), source))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn read_file(path: &Path) -> Result<Vec<u8>, CacheError> {
    fs::read(path).map_err(|source| CacheError::io(path.to_path_buf(), source))
}

fn hash_bytes(hasher: &mut Sha256, label: &str, bytes: &[u8]) {
    hash_string(hasher, label);
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn hex_digest(digest: &[u8]) -> String {
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push(HEX_DIGITS[usize::from(byte >> 4)] as char);
        hex.push(HEX_DIGITS[usize::from(byte & 0x0f)] as char);
    }
    hex
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
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"sh\", \"-c\", \"cat input.txt > output.txt\"]\ncache = true\ninputs = [\"input.txt\"]\noutputs = [\"output.txt\"]\n",
        )
        .expect("write root manifest");
        fs::write(temp.path().join("input.txt"), "input").expect("write input");
        Workspace::load(temp.path()).expect("project loads")
    }

    #[cfg(unix)]
    #[test]
    fn stores_and_restores_a_successful_task() {
        let temp = TempDir::new();
        let workspace = workspace_with_task(&temp);
        let task = workspace.plan(None, &[]).expect("plan succeeds").remove(0);
        let store = CacheStore::new(&workspace.root);
        let key = store
            .task_key(&workspace.root, &task, &[])
            .expect("key succeeds");
        let result = Runner::new()
            .run(&workspace.root, &task)
            .expect("task succeeds");
        store.store(&task, &key, &result).expect("store succeeds");
        fs::remove_file(task.root().join("output.txt")).expect("remove output");

        let restored = store
            .lookup(&task, &key)
            .expect("lookup succeeds")
            .expect("cache hit");
        assert_eq!(restored.output.stdout, result.output.stdout);
        assert_eq!(
            fs::read_to_string(task.root().join("output.txt")).expect("read output"),
            "input"
        );
    }

    #[test]
    fn creates_a_gitignore_for_cache_storage() {
        let temp = TempDir::new();
        let workspace = workspace_with_task(&temp);
        let task = workspace.plan(None, &[]).expect("plan succeeds").remove(0);
        let store = CacheStore::new(&workspace.root);

        store
            .task_key(&workspace.root, &task, &[])
            .expect("key succeeds");

        assert_eq!(
            fs::read_to_string(workspace.root.join(".mono/.gitignore"))
                .expect("gitignore is created"),
            "*\n!.gitignore\n"
        );
    }

    #[test]
    fn changing_an_input_changes_the_key() {
        let temp = TempDir::new();
        let workspace = workspace_with_task(&temp);
        let task = workspace.plan(None, &[]).expect("plan succeeds").remove(0);
        let store = CacheStore::new(&workspace.root);
        let first = store
            .task_key(&workspace.root, &task, &[])
            .expect("key succeeds");
        fs::write(task.root().join("input.txt"), "changed").expect("change input");
        let second = store
            .task_key(&workspace.root, &task, &[])
            .expect("key succeeds");
        assert_ne!(first, second);
    }

    fn workspace_with_manifest(temp: &TempDir, manifest: &str) -> Workspace {
        let task_manifest = manifest.replacen("[project]\nname = \"app\"\n\n", "", 1);
        let contents = format!(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n{task_manifest}"
        );
        fs::write(config_path(temp.path()), contents).expect("write project manifest");
        Workspace::load(temp.path()).expect("project loads")
    }

    fn only_task(workspace: &Workspace) -> PlannedTask {
        workspace.plan(None, &[]).expect("plan succeeds").remove(0)
    }

    #[test]
    fn changing_the_output_limit_changes_the_key() {
        let temp = TempDir::new();
        let mut workspace = workspace_with_task(&temp);
        let task = only_task(&workspace);
        let store = CacheStore::new(&workspace.root);
        let session = store
            .prepare(&workspace.root, std::slice::from_ref(&task))
            .expect("cache session prepares");
        let first = store
            .task_key_with_session(&session, &workspace.root, &task, &[])
            .expect("key succeeds");
        workspace
            .tasks
            .get_mut("build")
            .expect("fixture task exists")
            .max_output_bytes += 1;
        let changed_task = only_task(&workspace);
        let second = store
            .task_key_with_session(&session, &workspace.root, &changed_task, &[])
            .expect("key succeeds");
        assert_ne!(first, second);
    }

    #[test]
    fn ignores_files_outside_the_input_patterns() {
        let temp = TempDir::new();
        let workspace = workspace_with_manifest(
            &temp,
            "[project]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"echo\", \"app\"]\ncache = true\ninputs = [\"src/**\"]\noutputs = [\"dist/**\"]\n",
        );
        let task = only_task(&workspace);
        let project = task.root().to_path_buf();
        fs::create_dir_all(project.join("src")).expect("create src");
        fs::create_dir_all(project.join("dist")).expect("create dist");
        fs::create_dir_all(project.join("target/build")).expect("create unrelated tree");
        fs::write(project.join("src/input.txt"), "input").expect("write input");
        fs::write(project.join("dist/output.txt"), "output").expect("write output");
        let store = CacheStore::new(&workspace.root);
        let first = store
            .task_key(&workspace.root, &task, &[])
            .expect("key succeeds");

        fs::write(project.join("target/build/object.o"), "unrelated")
            .expect("write unrelated file");

        let second = store
            .task_key(&workspace.root, &task, &[])
            .expect("key succeeds");
        assert_eq!(first, second);
    }

    #[test]
    fn honours_negated_input_patterns() {
        let temp = TempDir::new();
        let workspace = workspace_with_manifest(
            &temp,
            "[project]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"echo\", \"app\"]\ncache = true\ninputs = [\"src/**\", \"!src/skip.txt\"]\noutputs = [\"dist/**\"]\n",
        );
        let task = only_task(&workspace);
        let project = task.root().to_path_buf();
        fs::create_dir_all(project.join("src")).expect("create src");
        fs::create_dir_all(project.join("dist")).expect("create dist");
        fs::write(project.join("src/kept.txt"), "kept").expect("write input");
        fs::write(project.join("src/skip.txt"), "first").expect("write excluded input");
        let store = CacheStore::new(&workspace.root);
        let first = store
            .task_key(&workspace.root, &task, &[])
            .expect("key succeeds");

        fs::write(project.join("src/skip.txt"), "second").expect("change excluded input");

        let second = store
            .task_key(&workspace.root, &task, &[])
            .expect("key succeeds");
        assert_eq!(first, second);
    }

    #[test]
    fn excludes_outputs_from_the_input_fingerprint() {
        let temp = TempDir::new();
        let workspace = workspace_with_manifest(
            &temp,
            "[project]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"echo\", \"app\"]\ncache = true\ninputs = [\"**\"]\noutputs = [\"dist/**\"]\n",
        );
        let task = only_task(&workspace);
        let project = task.root().to_path_buf();
        fs::create_dir_all(project.join("src")).expect("create src");
        fs::create_dir_all(project.join("dist")).expect("create dist");
        fs::write(project.join("src/input.txt"), "input").expect("write input");
        fs::write(project.join("dist/output.txt"), "first").expect("write output");
        let store = CacheStore::new(&workspace.root);
        let first = store
            .task_key(&workspace.root, &task, &[])
            .expect("key succeeds");

        fs::write(project.join("dist/output.txt"), "second").expect("rewrite output");

        let second = store
            .task_key(&workspace.root, &task, &[])
            .expect("key succeeds");
        assert_eq!(first, second);
    }

    #[test]
    fn reports_an_input_pattern_that_matches_no_files() {
        let temp = TempDir::new();
        let workspace = workspace_with_manifest(
            &temp,
            "[project]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"echo\", \"app\"]\ncache = true\ninputs = [\"src/**\", \"missing/**\"]\noutputs = [\"dist/**\"]\n",
        );
        let task = only_task(&workspace);
        let project = task.root().to_path_buf();
        fs::create_dir_all(project.join("src")).expect("create src");
        fs::create_dir_all(project.join("dist")).expect("create dist");
        fs::write(project.join("src/input.txt"), "input").expect("write input");
        let store = CacheStore::new(&workspace.root);

        let error = store
            .task_key(&workspace.root, &task, &[])
            .expect_err("an unmatched pattern must fail");
        assert!(error.to_string().contains("missing/**"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_symlink_input_match() {
        let temp = TempDir::new();
        let workspace = workspace_with_manifest(
            &temp,
            "[project]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"echo\", \"app\"]\ncache = true\ninputs = [\"src/**\"]\noutputs = [\"dist/**\"]\n",
        );
        let task = only_task(&workspace);
        let project = task.root().to_path_buf();
        fs::create_dir_all(project.join("src")).expect("create src");
        fs::create_dir_all(project.join("dist")).expect("create dist");
        fs::write(project.join("src/input.txt"), "input").expect("write input");
        std::os::unix::fs::symlink("input.txt", project.join("src/link.txt"))
            .expect("create symlink");
        let store = CacheStore::new(&workspace.root);

        let error = store
            .task_key(&workspace.root, &task, &[])
            .expect_err("symlinks cannot be fingerprinted");
        assert!(error.to_string().contains("unsupported symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn stores_and_restores_nested_outputs() {
        let temp = TempDir::new();
        let workspace = workspace_with_manifest(
            &temp,
            "[project]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"sh\", \"-c\", \"mkdir -p dist/nested && cp src/input.txt dist/nested/artifact.txt\"]\ncache = true\ninputs = [\"src/**\"]\noutputs = [\"dist/**\"]\n",
        );
        let task = only_task(&workspace);
        let project = task.root().to_path_buf();
        fs::create_dir_all(project.join("src")).expect("create src");
        fs::write(project.join("src/input.txt"), "nested").expect("write input");
        let store = CacheStore::new(&workspace.root);
        let key = store
            .task_key(&workspace.root, &task, &[])
            .expect("key succeeds");
        let result = Runner::new()
            .run(&workspace.root, &task)
            .expect("task succeeds");
        store.store(&task, &key, &result).expect("store succeeds");
        fs::remove_dir_all(project.join("dist")).expect("remove outputs");

        store
            .lookup(&task, &key)
            .expect("lookup succeeds")
            .expect("cache hit");

        assert_eq!(
            fs::read_to_string(project.join("dist/nested/artifact.txt")).expect("read output"),
            "nested"
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_to_restore_through_a_destination_symlink() {
        let temp = TempDir::new();
        let workspace = workspace_with_task(&temp);
        let task = only_task(&workspace);
        let project = task.root().to_path_buf();
        let store = CacheStore::new(&workspace.root);

        let key = store
            .task_key(&workspace.root, &task, &[])
            .expect("key succeeds");
        let result = Runner::new()
            .run(&workspace.root, &task)
            .expect("task succeeds");
        store.store(&task, &key, &result).expect("store succeeds");

        fs::remove_file(project.join("output.txt")).expect("remove generated output");
        fs::write(project.join("outside.txt"), "must remain unchanged")
            .expect("write outside target");
        std::os::unix::fs::symlink("outside.txt", project.join("output.txt"))
            .expect("create destination symlink");

        let error = store
            .lookup(&task, &key)
            .expect_err("symlink destination must be rejected");

        assert!(error.to_string().contains("symlink"));
        assert_eq!(
            fs::read_to_string(project.join("outside.txt")).unwrap(),
            "must remain unchanged"
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_to_restore_through_a_symlinked_output_parent() {
        let temp = TempDir::new();
        let workspace = workspace_with_manifest(
            &temp,
            "[project]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"sh\", \"-c\", \"mkdir -p dist/nested && cp src/input.txt dist/nested/artifact.txt\"]\ncache = true\ninputs = [\"src/**\"]\noutputs = [\"dist/**\"]\n",
        );
        let task = only_task(&workspace);
        let project = task.root().to_path_buf();
        let store = CacheStore::new(&workspace.root);

        fs::create_dir_all(project.join("src")).expect("create source directory");
        fs::write(project.join("src/input.txt"), "cached").expect("write source");
        let key = store
            .task_key(&workspace.root, &task, &[])
            .expect("key succeeds");
        let result = Runner::new()
            .run(&workspace.root, &task)
            .expect("task succeeds");
        store.store(&task, &key, &result).expect("store succeeds");

        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside).expect("create outside directory");
        fs::write(outside.join("artifact.txt"), "must remain unchanged")
            .expect("write outside sentinel");
        fs::remove_dir_all(project.join("dist")).expect("remove real output directory");
        std::os::unix::fs::symlink(&outside, project.join("dist"))
            .expect("create symlinked output parent");

        let error = store
            .lookup(&task, &key)
            .expect_err("symlinked parent must be rejected");

        assert!(error.to_string().contains("symlink"));
        assert_eq!(
            fs::read_to_string(outside.join("artifact.txt")).unwrap(),
            "must remain unchanged"
        );
    }

    #[cfg(unix)]
    #[test]
    fn preflight_validates_all_outputs_before_copying_any() {
        let temp = TempDir::new();
        let workspace = workspace_with_manifest(
            &temp,
            "[project]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"sh\", \"-c\", \"mkdir -p deep && echo first > output1.txt && echo second > deep/output2.txt\"]\ncache = true\ninputs = [\"input.txt\"]\noutputs = [\"output1.txt\", \"deep/output2.txt\"]\n",
        );
        let task = only_task(&workspace);
        let project = task.root().to_path_buf();
        let store = CacheStore::new(&workspace.root);

        fs::write(project.join("input.txt"), "input").expect("write input");

        let key = store
            .task_key(&workspace.root, &task, &[])
            .expect("key succeeds");
        let result = Runner::new()
            .run(&workspace.root, &task)
            .expect("task succeeds");
        store.store(&task, &key, &result).expect("store succeeds");

        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside).expect("create outside directory");
        fs::write(outside.join("sentinel.txt"), "must remain unchanged")
            .expect("write outside sentinel");

        // First output: replace with a regular file so we can verify it was
        // NOT overwritten by a partial restore.
        fs::write(project.join("output1.txt"), "old content before restore")
            .expect("write sentinel to first output");

        // Second output: replace the real directory tree with a symlink.
        fs::remove_dir_all(project.join("deep")).expect("remove deep directory");
        fs::create_dir_all(project.join("deep")).expect("recreate deep directory");
        std::os::unix::fs::symlink("../outside/sentinel.txt", project.join("deep/output2.txt"))
            .expect("create symlink for later output");

        let error = store
            .lookup(&task, &key)
            .expect_err("symlink in a later output must be rejected");

        assert!(error.to_string().contains("symlink"));

        // The first output was NOT restored because the preflight caught the
        // symlink before any copy started.
        assert_eq!(
            fs::read_to_string(project.join("output1.txt")).unwrap(),
            "old content before restore"
        );
        assert_eq!(
            fs::read_to_string(outside.join("sentinel.txt")).unwrap(),
            "must remain unchanged"
        );
    }
}
