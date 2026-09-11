use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use crate::cache::CacheError;

#[derive(Debug, Default)]
struct CollectedPaths {
    files: Vec<String>,
    first_symlink: Option<String>,
    matched_positive_patterns: BTreeSet<usize>,
}

/// Cache patterns compiled into path segments once, so a walk never re-splits
/// them for every entry it visits.
pub(super) struct CachePatterns<'a> {
    patterns: Vec<CachePattern<'a>>,
}

struct CachePattern<'a> {
    source: &'a str,
    exclude: bool,
    segments: Vec<&'a str>,
}

impl<'a> CachePatterns<'a> {
    pub(super) fn compile(patterns: &'a [String]) -> Self {
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
    pub(super) fn matches(&self, path: &str) -> bool {
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
pub(super) fn collect_files(
    root: &Path,
    patterns: &[String],
    kind: &str,
) -> Result<Vec<String>, CacheError> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_patterns_apply_the_last_matching_selection() {
        let binding = [
            "src/**".to_owned(),
            "!src/generated/**".to_owned(),
            "src/generated/keep.txt".to_owned(),
        ];
        let patterns = CachePatterns::compile(&binding);

        assert!(patterns.matches("src/main.rs"));
        assert!(!patterns.matches("src/generated/drop.txt"));
        assert!(patterns.matches("src/generated/keep.txt"));
    }

    #[test]
    fn double_star_matches_zero_or_more_path_segments() {
        let binding = ["src/**/Cargo.toml".to_owned()];
        let patterns = CachePatterns::compile(&binding);

        assert!(patterns.matches("src/Cargo.toml"));
        assert!(patterns.matches("src/a/b/Cargo.toml"));
        assert!(!patterns.matches("tests/Cargo.toml"));
    }

    #[test]
    fn directory_pruning_rejects_unreachable_subtrees() {
        let binding = ["src/**/*.rs".to_owned()];
        let patterns = CachePatterns::compile(&binding);

        assert!(patterns.could_match_below("src"));
        assert!(patterns.could_match_below("src/lib"));
        assert!(!patterns.could_match_below("target"));
    }
}
