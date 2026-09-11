//! The fixed, language-agnostic root `mono.toml` schema.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Name of the configuration file at a project root.
pub const CONFIG_FILE_NAME: &str = "mono.toml";
/// Version of the on-disk manifest schema.
pub const SUPPORTED_SCHEMA: u32 = 1;

/// One root manifest describes one complete execution graph.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MonoConfig {
    #[serde(default = "default_schema")]
    pub schema: u32,
    pub project: ProjectConfig,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tasks: BTreeMap<String, TaskConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pipelines: BTreeMap<String, PipelineConfig>,
}

impl MonoConfig {
    /// A valid root-only project template.
    pub fn template() -> Self {
        let mut tasks = BTreeMap::new();
        tasks.insert(
            "check".to_owned(),
            TaskConfig {
                command: vec!["echo".to_owned(), "configure this task".to_owned()],
                timeout_seconds: 600,
                max_output_bytes: 16 * 1024 * 1024,
                ..TaskConfig::default()
            },
        );
        let mut pipelines = BTreeMap::new();
        pipelines.insert(
            "ci".to_owned(),
            PipelineConfig {
                tasks: vec!["check".to_owned()],
                finally: Vec::new(),
            },
        );
        Self {
            schema: SUPPORTED_SCHEMA,
            project: ProjectConfig {
                name: "project".to_owned(),
                default_pipeline: "ci".to_owned(),
            },
            tasks,
            pipelines,
        }
    }

    /// Parse a root manifest from TOML.
    pub fn parse(contents: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(contents)
    }
}

/// Project identity and the default pipeline.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    pub name: String,
    #[serde(default = "default_pipeline")]
    pub default_pipeline: String,
}

/// How a task receives standard input.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum StdinMode {
    #[default]
    Null,
    Inherit,
}

impl StdinMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Inherit => "inherit",
        }
    }
}

/// A task command and orchestration metadata. Every path is relative to the
/// project root unless the command itself receives an absolute value.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskConfig {
    pub command: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub stdin: StdinMode,
    #[serde(default)]
    pub cache: bool,
    #[serde(default)]
    pub inputs: Vec<String>,
    #[serde(default)]
    pub outputs: Vec<String>,
    #[serde(default)]
    pub cache_env: Vec<String>,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: u64,
    #[serde(default)]
    pub resource_group: Option<String>,
    #[serde(default)]
    pub retries: u32,
    #[serde(default)]
    pub retry_backoff_seconds: u64,
    #[serde(default)]
    pub matrix: BTreeMap<String, Vec<String>>,
}

/// A named root task pipeline.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PipelineConfig {
    pub tasks: Vec<String>,
    #[serde(default)]
    pub finally: Vec<String>,
}

fn default_schema() -> u32 {
    SUPPORTED_SCHEMA
}

fn default_pipeline() -> String {
    "ci".to_owned()
}

fn default_timeout_seconds() -> u64 {
    600
}

fn default_max_output_bytes() -> u64 {
    16 * 1024 * 1024
}

/// Path of the root config inside `dir`.
pub fn config_path(dir: &Path) -> PathBuf {
    dir.join(CONFIG_FILE_NAME)
}

/// Render a config as TOML.
pub fn render_config(config: &MonoConfig) -> String {
    toml::to_string_pretty(config).expect("MonoConfig must serialize to TOML")
}

/// Return a validation message when a value cannot be passed to a child
/// process safely.
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
        let rendered = render_config(&MonoConfig::template());
        let parsed = MonoConfig::parse(&rendered).expect("rendered config parses");

        assert_eq!(parsed, MonoConfig::template());
        assert_eq!(parsed.schema, 1);
    }

    #[test]
    fn config_path_joins_the_file_name_onto_the_directory() {
        assert_eq!(
            config_path(Path::new("repo")),
            Path::new("repo").join(CONFIG_FILE_NAME)
        );
    }

    #[test]
    fn task_defaults_to_no_dependencies_or_environment() {
        let parsed = MonoConfig::parse(
            "[project]\nname = \"worker\"\n\n[tasks.build]\ncommand = [\"make\", \"build\"]\n",
        )
        .expect("project parses");

        assert_eq!(parsed.tasks["build"].timeout_seconds, 600);
        assert_eq!(parsed.tasks["build"].max_output_bytes, 16 * 1024 * 1024);
        assert_eq!(parsed.tasks["build"].stdin, StdinMode::Null);
        assert!(parsed.tasks["build"].depends_on.is_empty());
    }

    #[test]
    fn task_can_inherit_standard_input() {
        let parsed = MonoConfig::parse(
            "[project]\nname = \"worker\"\n\n[tasks.login]\ncommand = [\"login\"]\nstdin = \"inherit\"\n",
        )
        .expect("project parses");

        assert_eq!(parsed.tasks["login"].stdin, StdinMode::Inherit);
    }

    #[test]
    fn schema_defaults_to_version_one() {
        let parsed = MonoConfig::parse("[project]\nname = \"worker\"\n").unwrap();
        assert_eq!(parsed.schema, 1);
    }

    #[test]
    fn non_project_manifest_shapes_are_rejected() {
        assert!(MonoConfig::parse("[workspace]\nname = \"repo\"\n").is_err());
        assert!(MonoConfig::parse("[package]\nname = \"app\"\n").is_err());
    }
}
