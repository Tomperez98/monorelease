//! The fixed, language-agnostic `monorepo.toml` schema.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Name of the configuration file, relative to a repository or package root.
pub const CONFIG_FILE_NAME: &str = "monorepo.toml";

/// The fixed on-disk `monorepo.toml` schema.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MonorepoConfig {
    #[serde(default)]
    pub workspace: Option<WorkspaceConfig>,
    #[serde(default)]
    pub package: Option<PackageConfig>,
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

/// Path of the config file inside `dir`.
pub fn config_path(dir: &Path) -> PathBuf {
    dir.join(CONFIG_FILE_NAME)
}

/// Render a config as TOML.
pub fn render_config(config: &MonorepoConfig) -> String {
    toml::to_string_pretty(config).expect("MonorepoConfig must serialize to TOML")
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
