//! The fixed, language-agnostic `monorepo.toml` schema.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Name of the configuration file, relative to a repository or package root.
pub const CONFIG_FILE_NAME: &str = "monorepo.toml";
/// Namespace used by task references for tasks declared in the root manifest.
pub const WORKSPACE_PACKAGE_NAME: &str = "workspace";

/// The fixed on-disk `monorepo.toml` schema.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MonorepoConfig {
    #[serde(default)]
    pub workspace: Option<WorkspaceConfig>,
    #[serde(default)]
    pub package: Option<PackageConfig>,
    /// Tasks declared at the workspace root run once from the workspace root.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tasks: BTreeMap<String, TaskConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pipelines: BTreeMap<String, PipelineConfig>,
}

impl MonorepoConfig {
    /// The root config [`crate::init`] writes into a fresh repository.
    pub fn template() -> Self {
        let mut pipelines = BTreeMap::new();
        pipelines.insert("ci".to_owned(), PipelineConfig::ci_template());

        Self {
            workspace: Some(WorkspaceConfig::template()),
            package: None,
            tasks: BTreeMap::new(),
            pipelines,
        }
    }

    /// A valid standalone project config using one caller-supplied command.
    pub fn standalone_template(name: String, command: Vec<String>) -> Self {
        assert!(
            !name.is_empty(),
            "standalone package name must not be empty"
        );
        assert!(
            !name.contains(':'),
            "standalone package name cannot contain ':'"
        );
        assert!(
            !command.is_empty(),
            "standalone build command must not be empty"
        );
        assert!(
            !command[0].is_empty(),
            "standalone build command executable must not be empty"
        );

        let mut tasks = BTreeMap::new();
        tasks.insert(
            "build".to_owned(),
            TaskConfig {
                command,
                depends_on: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                cache: false,
                inputs: Vec::new(),
                outputs: Vec::new(),
                cache_env: Vec::new(),
                timeout_seconds: default_timeout_seconds(),
                resource_group: None,
            },
        );
        let mut pipelines = BTreeMap::new();
        pipelines.insert(
            "ci".to_owned(),
            PipelineConfig {
                tasks: vec!["build".to_owned()],
            },
        );

        Self {
            workspace: None,
            package: Some(PackageConfig { name }),
            tasks,
            pipelines,
        }
    }

    /// Parse a manifest from TOML.
    pub fn parse(contents: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(contents)
    }
}

/// Root-workspace settings.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub name: String,
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default = "default_pipeline")]
    pub default_pipeline: String,
}

impl WorkspaceConfig {
    pub fn template() -> Self {
        Self {
            name: "monorepo".to_owned(),
            members: vec!["apps/*".to_owned(), "packages/*".to_owned()],
            default_pipeline: default_pipeline(),
        }
    }
}

/// Package identity. Build behavior belongs entirely to [`TaskConfig`].
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PackageConfig {
    pub name: String,
}

/// A task command and its orchestration metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskConfig {
    pub command: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Whether successful task results may be reused from the local cache.
    #[serde(default)]
    pub cache: bool,
    /// Relative file globs included in the task fingerprint.
    #[serde(default)]
    pub inputs: Vec<String>,
    /// Relative file globs copied into and restored from the cache.
    #[serde(default)]
    pub outputs: Vec<String>,
    /// Environment variables whose values affect the task fingerprint.
    #[serde(default)]
    pub cache_env: Vec<String>,
    /// Maximum runtime for one invocation, in seconds.
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    /// Tasks sharing a resource group never run concurrently.
    #[serde(default)]
    pub resource_group: Option<String>,
}

/// A named workspace pipeline made of task names.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PipelineConfig {
    pub tasks: Vec<String>,
}

impl PipelineConfig {
    pub fn ci_template() -> Self {
        Self {
            tasks: vec!["build".to_owned(), "test".to_owned()],
        }
    }
}

fn default_pipeline() -> String {
    "ci".to_owned()
}

fn default_timeout_seconds() -> u64 {
    600
}

/// Path of the config file inside `dir`.
pub fn config_path(dir: &Path) -> PathBuf {
    dir.join(CONFIG_FILE_NAME)
}

/// Render a config as TOML.
pub fn render_config(config: &MonorepoConfig) -> String {
    toml::to_string_pretty(config).expect("MonorepoConfig must serialize to TOML")
}

/// Return a manifest validation message when a value cannot be passed to a
/// child process safely.
pub(crate) fn validate_process_value(value: &str, field: &str) -> Result<(), String> {
    if value.contains('\0') {
        return Err(format!("{field} must not contain a NUL byte"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_round_trips_through_toml() {
        let rendered = render_config(&MonorepoConfig::template());
        let parsed = MonorepoConfig::parse(&rendered).expect("rendered config parses");

        assert_eq!(parsed, MonorepoConfig::template());
    }

    #[test]
    fn standalone_template_round_trips_through_toml() {
        let config = MonorepoConfig::standalone_template(
            "app".to_owned(),
            vec!["cargo".to_owned(), "build".to_owned()],
        );
        let rendered = render_config(&config);
        let parsed = MonorepoConfig::parse(&rendered).expect("standalone config parses");

        assert_eq!(parsed, config);
    }

    #[test]
    fn config_path_joins_the_file_name_onto_the_directory() {
        assert_eq!(
            config_path(Path::new("repo")),
            Path::new("repo").join(CONFIG_FILE_NAME),
        );
    }

    #[test]
    fn task_defaults_to_no_dependencies_or_environment() {
        let parsed = MonorepoConfig::parse(
            "[package]\nname = \"worker\"\n\n[tasks.build]\ncommand = [\"make\", \"build\"]\n",
        )
        .expect("package parses");

        assert_eq!(
            parsed.tasks["build"],
            TaskConfig {
                command: vec!["make".to_owned(), "build".to_owned()],
                depends_on: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                cache: false,
                inputs: Vec::new(),
                outputs: Vec::new(),
                cache_env: Vec::new(),
                timeout_seconds: 600,
                resource_group: None,
            }
        );
    }

    #[test]
    fn rejects_schema_version_headers() {
        let error = MonorepoConfig::parse("version = 1\n\n[workspace]\nname = \"repo\"\n")
            .expect_err("version headers are not part of the fixed schema");

        assert!(error.to_string().contains("unknown field"));
    }
}
