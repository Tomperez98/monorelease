//! The on-disk `monorepo.toml` schema, and the pure functions over it.
//!
//! No IO and no globals live here, so tests can throw arbitrary configs at
//! these functions.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Name of the configuration file, relative to a repository root.
pub const CONFIG_FILE_NAME: &str = "monorepo.toml";

/// Schema version written by [`crate::init`].
///
/// Bump this whenever the on-disk format changes, so that readers can
/// recognize a config they do not understand.
pub const CONFIG_VERSION: u8 = 1;

/// The on-disk `monorepo.toml` schema.
#[derive(Debug, Serialize, Deserialize)]
pub struct MonorepoConfig {
    pub version: u8,
}

impl MonorepoConfig {
    /// The config [`crate::init`] writes into a fresh repository.
    pub fn template() -> Self {
        Self {
            version: CONFIG_VERSION,
        }
    }
}

/// Path of the config file inside `dir`.
pub fn config_path(dir: &Path) -> PathBuf {
    dir.join(CONFIG_FILE_NAME)
}

/// Render a config as TOML.
///
/// Serializing the template is an invariant, not an expected failure:
/// `version` is a `u8`, so no value of [`MonorepoConfig`] can fail to
/// serialize. A panic here means the template was changed into something TOML
/// cannot express, which is a bug, not a caller problem.
pub fn render_config(config: &MonorepoConfig) -> String {
    toml::to_string_pretty(config).expect("MonorepoConfig must serialize to TOML")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_round_trips_through_toml() {
        let rendered = render_config(&MonorepoConfig::template());

        let parsed: MonorepoConfig = toml::from_str(&rendered).expect("rendered config parses");

        assert_eq!(parsed.version, CONFIG_VERSION);
    }

    #[test]
    fn config_path_joins_the_file_name_onto_the_directory() {
        assert_eq!(
            config_path(Path::new("repo")),
            Path::new("repo").join(CONFIG_FILE_NAME),
        );
    }
}
