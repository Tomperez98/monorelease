//! Root manifest discovery for a single-project execution graph.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{MonoConfig, config_path};
use crate::project::ProjectError;

#[derive(Debug)]
pub(crate) struct DiscoveredRoot {
    pub(crate) root: PathBuf,
    pub(crate) config: MonoConfig,
}

/// Find the nearest root `mono.toml`, walking upward from `start`.
pub(crate) fn find_root(start: &Path) -> Result<DiscoveredRoot, ProjectError> {
    let start = fs::canonicalize(start).map_err(|source| ProjectError::Io {
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
            return Ok(DiscoveredRoot {
                root: current,
                config,
            });
        }
        if !current.pop() {
            break;
        }
    }

    Err(ProjectError::MissingRoot {
        start: start_for_error,
    })
}

pub(crate) fn read_manifest(path: &Path) -> Result<MonoConfig, ProjectError> {
    let contents = fs::read_to_string(path).map_err(|source| ProjectError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    MonoConfig::parse(&contents).map_err(|source| ProjectError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;
    use std::fs;

    fn write_root(temp: &TempDir) {
        fs::write(
            config_path(temp.path()),
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n",
        )
        .expect("write root manifest");
    }

    #[test]
    fn a_nested_directory_finds_the_ancestor_manifest() {
        let temp = TempDir::new();
        write_root(&temp);
        let nested = temp.path().join("services/api");
        fs::create_dir_all(&nested).expect("create nested directory");

        let discovered = find_root(&nested).expect("root is found");

        assert_eq!(discovered.config.project.name, "fixture");
        assert_eq!(
            discovered.root,
            fs::canonicalize(temp.path()).expect("temp path canonicalizes")
        );
    }

    #[test]
    fn a_file_start_walks_up_from_its_parent() {
        let temp = TempDir::new();
        write_root(&temp);
        let file = temp.path().join("services.txt");
        fs::write(&file, "content").expect("write file");

        let discovered = find_root(&file).expect("root is found from a file");

        assert_eq!(
            discovered.root,
            fs::canonicalize(temp.path()).expect("temp path canonicalizes")
        );
    }

    #[test]
    fn a_start_with_no_ancestor_manifest_is_the_missing_root_failure() {
        let temp = TempDir::new();
        // A fresh temp dir has no ancestor manifest it owns; walk to the
        // filesystem root. `/tmp` is not a project root in the test sandbox, so
        // the walk fails rather than finding an unrelated one.
        let error = match find_root(temp.path()) {
            Ok(discovered) => {
                // A developer machine could legitimately have a manifest above
                // the temp dir. Skip rather than assert a wrong invariant.
                eprintln!(
                    "skipped: found {} above temp dir",
                    discovered.root.display()
                );
                return;
            }
            Err(error) => error,
        };

        assert!(matches!(error, ProjectError::MissingRoot { .. }), "{error}");
    }

    #[test]
    fn an_unsupported_schema_is_rejected_while_discovering() {
        let temp = TempDir::new();
        fs::write(
            config_path(temp.path()),
            "schema = 2\n\n[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n",
        )
        .expect("write manifest");

        let error = crate::project::Project::load(temp.path()).expect_err("schema 2 is rejected");

        assert!(
            matches!(error, ProjectError::UnsupportedSchema { found: 2, .. }),
            "{error}"
        );
    }

    #[test]
    fn unparseable_manifest_contents_are_a_parse_failure() {
        let temp = TempDir::new();
        fs::write(config_path(temp.path()), "this is not toml = = =").expect("write manifest");

        let error = read_manifest(&config_path(temp.path())).expect_err("parse fails");

        assert!(matches!(error, ProjectError::Parse { .. }), "{error}");
    }
}
