//! Root and package-member discovery for a manifest-driven workspace.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{MonorepoConfig, config_path};
use crate::workspace::WorkspaceError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RootKind {
    Workspace,
    Standalone,
}

#[derive(Debug)]
pub(crate) struct DiscoveredRoot {
    pub(crate) root: PathBuf,
    pub(crate) config: MonorepoConfig,
    pub(crate) kind: RootKind,
}

pub(crate) fn find_root(start: &Path) -> Result<DiscoveredRoot, WorkspaceError> {
    let start = fs::canonicalize(start).map_err(|source| WorkspaceError::Io {
        path: start.to_path_buf(),
        source,
    })?;
    let start_for_error = start.clone();
    let mut standalone_root = None;
    let mut current = if start.is_dir() {
        start
    } else {
        start.parent().unwrap_or(Path::new("/")).to_path_buf()
    };

    loop {
        let manifest_path = config_path(&current);
        if manifest_path.is_file() {
            let config = read_manifest(&manifest_path)?;
            match (config.workspace.is_some(), config.package.is_some()) {
                (true, true) => {
                    return Err(WorkspaceError::InvalidManifest {
                        path: manifest_path,
                        message: "root manifest cannot contain both [workspace] and [package]"
                            .to_owned(),
                    });
                }
                (true, false) => {
                    return Ok(DiscoveredRoot {
                        root: current,
                        config,
                        kind: RootKind::Workspace,
                    });
                }
                (false, true) if standalone_root.is_none() => {
                    standalone_root = Some(DiscoveredRoot {
                        root: current.clone(),
                        config,
                        kind: RootKind::Standalone,
                    });
                }
                (false, true) | (false, false) => {}
            }
        }

        if !current.pop() {
            break;
        }
    }

    standalone_root.ok_or(WorkspaceError::MissingRoot {
        start: start_for_error,
    })
}

pub(crate) fn read_manifest(path: &Path) -> Result<MonorepoConfig, WorkspaceError> {
    let contents = fs::read_to_string(path).map_err(|source| WorkspaceError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    MonorepoConfig::parse(&contents).map_err(|source| WorkspaceError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

pub(crate) fn expand_member_pattern(
    root: &Path,
    pattern: &str,
) -> Result<Vec<PathBuf>, WorkspaceError> {
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
