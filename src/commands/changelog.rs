//! Provider-neutral changelog commands.

use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::changelog::{Changelog, Request, today};

/// Default changelog filename used by the CLI.
pub const DEFAULT_PATH: &str = "CHANGELOG.md";
/// Default release-notes filename used by the CLI.
pub const DEFAULT_NOTES_PATH: &str = "RELEASE_NOTES.md";

pub fn validate(path: &Path) -> Result<String, ChangelogError> {
    let changelog = load(path)?;
    Ok(format!(
        "validated {} ({} entries)",
        path.display(),
        changelog.entry_count()
    ))
}

/// Scaffold an entry dated today.
pub fn scaffold(path: &Path, version: &str) -> Result<String, ChangelogError> {
    scaffold_on(path, version, &today())
}

/// Scaffold an entry with an explicit date.
///
/// The clock is a parameter rather than a call, so the rendered changelog is
/// deterministic and a test can assert the date.
pub fn scaffold_on(path: &Path, version: &str, date: &str) -> Result<String, ChangelogError> {
    let request = Request::parse(version).map_err(ChangelogError::Invalid)?;
    let text = read(path)?;
    let mut changelog = Changelog::parse(&text).map_err(|message| invalid(path, message))?;
    let action = changelog
        .scaffold(request, date, &[])
        .map_err(|message| invalid(path, message))?;
    write(path, &changelog.render())?;
    Ok(format!(
        "scaffolded {} in {}",
        action_name(&action),
        path.display()
    ))
}

pub fn notes(
    changelog_path: &Path,
    version: &str,
    output_path: &Path,
) -> Result<String, ChangelogError> {
    let request = Request::parse(version).map_err(ChangelogError::Invalid)?;
    let changelog = load(changelog_path)?;
    let heading = request.heading();
    let entry = changelog
        .entry(heading)
        .ok_or_else(|| invalid(changelog_path, format!("entry `{heading}` was not found")))?;
    let body = entry.body.trim();
    if body.is_empty() {
        return Err(invalid(
            changelog_path,
            format!("entry `{heading}` is empty"),
        ));
    }
    let heading_text = heading.heading();
    let title = heading_text.trim_start_matches("## ");
    let notes = format!("# {title}\n\n## Changelog\n\n{body}\n");
    write(output_path, &notes)?;
    Ok(format!("wrote {}", output_path.display()))
}

fn load(path: &Path) -> Result<Changelog, ChangelogError> {
    let text = read(path)?;
    Changelog::parse(&text).map_err(|message| invalid(path, message))
}

fn action_name(action: &crate::changelog::Action) -> &'static str {
    match action {
        crate::changelog::Action::Inserted => "new entry",
        crate::changelog::Action::Renamed { .. } => "entry",
    }
}

fn invalid(path: &Path, message: String) -> ChangelogError {
    ChangelogError::Invalid(format!("{}: {message}", path.display()))
}

fn read(path: &Path) -> Result<String, ChangelogError> {
    fs::read_to_string(path).map_err(|source| ChangelogError::Read {
        path: path.to_path_buf(),
        source,
    })
}

fn write(path: &Path, contents: &str) -> Result<(), ChangelogError> {
    crate::atomic_file::write(
        path,
        contents.as_bytes(),
        crate::atomic_file::WriteMode::Replace,
    )
    .map_err(|source| ChangelogError::Write {
        path: path.to_path_buf(),
        source,
    })
}

#[derive(Debug)]
pub enum ChangelogError {
    Read { path: PathBuf, source: io::Error },
    Write { path: PathBuf, source: io::Error },
    Invalid(String),
}

impl fmt::Display for ChangelogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(formatter, "failed to read {}: {source}", path.display())
            }
            Self::Write { path, source } => {
                write!(formatter, "failed to write {}: {source}", path.display())
            }
            Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

impl StdError for ChangelogError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Read { source, .. } | Self::Write { source, .. } => Some(source),
            Self::Invalid(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn validates_a_changelog() {
        let temp = TempDir::new();
        let path = temp.path().join(DEFAULT_PATH);
        fs::write(&path, "# Changelog\n\n## 1.0.0\nReleased: 2026-09-11\n").unwrap();

        let output = validate(&path).unwrap();
        assert!(output.contains("validated"));
        assert!(output.contains("1 entries"));
    }

    #[test]
    fn writes_notes_for_the_requested_entry() {
        let temp = TempDir::new();
        let changelog = temp.path().join(DEFAULT_PATH);
        let notes_path = temp.path().join(DEFAULT_NOTES_PATH);
        fs::write(
            &changelog,
            "# Changelog\n\n## 1.0.0\nReleased: 2026-09-11\n\n- Shipped.\n",
        )
        .unwrap();

        notes(&changelog, "v1.0.0", &notes_path).unwrap();
        let output = fs::read_to_string(notes_path).unwrap();
        assert!(output.contains("# 1.0.0"));
        assert!(output.contains("- Shipped."));
    }

    #[test]
    fn scaffold_refuses_a_duplicate_version() {
        let temp = TempDir::new();
        let path = temp.path().join(DEFAULT_PATH);
        fs::write(&path, "# Changelog\n\n## 1.0.0\nReleased: 2026-09-11\n").unwrap();

        let error = scaffold(&path, "1.0.0").unwrap_err();
        assert!(error.to_string().contains("already exists"));
    }

    #[test]
    fn scaffold_replaces_changelog_without_leaving_temporary_files() {
        let temp = TempDir::new();
        let path = temp.path().join(DEFAULT_PATH);
        fs::write(&path, "# Changelog\n\n## 1.0.0\nReleased: 2026-09-11\n").unwrap();

        scaffold(&path, "1.1.0").expect("scaffold succeeds");

        assert!(fs::read_to_string(&path).unwrap().contains("## 1.1.0"));
        assert!(
            fs::read_dir(temp.path())
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| !entry.file_name().to_string_lossy().ends_with(".tmp"))
        );
    }

    #[test]
    fn notes_rejects_an_empty_entry_without_writing_the_output_file() {
        let temp = TempDir::new();
        let changelog = temp.path().join(DEFAULT_PATH);
        let notes_path = temp.path().join(DEFAULT_NOTES_PATH);
        fs::write(&changelog, "# Changelog\n\n## (unreleased)\n").unwrap();

        let error = notes(&changelog, "unreleased", &notes_path).expect_err("empty entry fails");

        assert!(
            error
                .to_string()
                .contains("entry `## (unreleased)` is empty")
        );
        assert!(!notes_path.exists());
    }

    #[test]
    fn scaffold_on_writes_the_supplied_date() {
        let temp = TempDir::new();
        let path = temp.path().join(DEFAULT_PATH);
        fs::write(&path, "# Changelog\n\n## 1.0.0\nReleased: 2026-01-01\n").unwrap();

        // Deliberately not today's date. If this were the current date, the test
        // could not tell `scaffold_on` using the parameter apart from
        // `scaffold_on` ignoring it and calling `today()`, and would pass for
        // the wrong reason until the clock moved on.
        scaffold_on(&path, "1.1.0", "2001-02-03").expect("scaffold succeeds");

        let rendered = fs::read_to_string(&path).unwrap();
        assert!(rendered.contains("## 1.1.0\n"), "{rendered}");
        assert!(rendered.contains("Released: 2001-02-03\n"), "{rendered}");
        assert!(
            !rendered.contains(&format!("Released: {}\n", today())),
            "scaffold_on must not fall back to the wall clock: {rendered}"
        );
    }

    #[test]
    fn changelog_errors_expose_a_source_only_for_read_and_write() {
        let read = ChangelogError::Read {
            path: PathBuf::from("CHANGELOG.md"),
            source: io::Error::new(io::ErrorKind::NotFound, "missing"),
        };
        let invalid = ChangelogError::Invalid("no entries".to_owned());

        assert!(read.source().is_some(), "{read}");
        assert!(invalid.source().is_none(), "{invalid}");
        assert!(!invalid.to_string().is_empty());
    }
}
