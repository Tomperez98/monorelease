//! Provider-neutral changelog commands.

use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::changelog::{Changelog, Request, is_valid_date, today};

/// Default changelog filename used by the CLI.
pub const DEFAULT_PATH: &str = "CHANGELOG.md";
/// Default release-notes filename used by the CLI.
pub const DEFAULT_NOTES_PATH: &str = "RELEASE_NOTES.md";
const GIT_LOG_BYTES_MAX: usize = 10 * 1024 * 1024;
const GIT_ERROR_BYTES_MAX: usize = 64 * 1024;

/// The release target supplied by the CLI or release environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReleaseNotesTarget {
    version: Option<Request>,
    release_tag: Option<Request>,
}

impl ReleaseNotesTarget {
    pub fn parse(version: Option<&str>, release_tag: Option<&str>) -> Result<Self, ChangelogError> {
        let version = version
            .map(Request::parse)
            .transpose()
            .map_err(ChangelogError::Invalid)?;
        let release_tag = release_tag
            .map(Request::parse)
            .transpose()
            .map_err(ChangelogError::Invalid)?;
        if let (Some(version), Some(release_tag)) = (version, release_tag)
            && version != release_tag
        {
            return Err(ChangelogError::Invalid(
                "requested version does not match --release-tag".to_owned(),
            ));
        }
        Ok(Self {
            version,
            release_tag,
        })
    }

    fn requested(self) -> Option<Request> {
        self.version.or(self.release_tag)
    }
}

pub fn validate(path: &Path) -> Result<String, ChangelogError> {
    let changelog = load(path)?;
    Ok(format!(
        "validated {} ({} entries)",
        path.display(),
        changelog.entry_count()
    ))
}

/// Prepare a new top entry, inferring the next patch version when omitted.
pub fn prepare(
    path: &Path,
    version: Option<&str>,
    date: Option<&str>,
) -> Result<String, ChangelogError> {
    let date = date.map_or_else(today, str::to_owned);
    prepare_on(path, version, &date)
}

/// Prepare an entry using a caller-supplied date.
///
/// Keeping the clock outside this function makes the workflow deterministic
/// without requiring a fake clock or global state in tests.
pub fn prepare_on(
    path: &Path,
    version: Option<&str>,
    date: &str,
) -> Result<String, ChangelogError> {
    prepare_with_bullets(path, version, date, &[])
}

/// Prepare a new top entry from first-parent merge commits in an explicit ref range.
///
/// Git state is read only: this function never fetches, switches branches, or writes
/// anything except the requested changelog file.
pub fn prepare_from_git(
    path: &Path,
    version: Option<&str>,
    date: Option<&str>,
    from: &str,
    to: &str,
    pull_request_url: Option<&str>,
) -> Result<String, ChangelogError> {
    prepare_from_git_with(
        path,
        version,
        date,
        from,
        to,
        pull_request_url,
        git_merge_log,
    )
}

/// Prepare a changelog entry with Git supplied by the caller.
///
/// The production wrapper above supplies the real Git process. Tests and other
/// callers can provide a scripted log or failure without spawning a process.
pub(crate) fn prepare_from_git_with<F>(
    path: &Path,
    version: Option<&str>,
    date: Option<&str>,
    from: &str,
    to: &str,
    pull_request_url: Option<&str>,
    mut merge_log: F,
) -> Result<String, ChangelogError>
where
    F: FnMut(&Path, &str, &str) -> Result<String, ChangelogError>,
{
    let date = date.map_or_else(today, str::to_owned);
    let (changelog, request) = load_prepare_input(path, version, &date)?;
    validate_ref(from)?;
    validate_ref(to)?;
    validate_pull_request_url(pull_request_url)?;
    let log = merge_log(path, from, to)?;
    let bullets = format_git_bullets(&log, pull_request_url);
    let mut message = write_prepared(path, changelog, request, &date, &bullets)?;
    if bullets.is_empty() {
        message.push_str(&format!(
            "; warning: no first-parent merge commits found in {from}..{to}"
        ));
    }
    Ok(message)
}

/// Scaffold an entry dated today. Kept as a compatibility wrapper for `prepare`.
pub fn scaffold(path: &Path, version: &str) -> Result<String, ChangelogError> {
    prepare(path, Some(version), None)
}

/// Scaffold an entry with an explicit date.
///
/// The clock is a parameter rather than a call, so the rendered changelog is
/// deterministic and a test can assert the date.
pub fn scaffold_on(path: &Path, version: &str, date: &str) -> Result<String, ChangelogError> {
    prepare_on(path, Some(version), date)
}

fn prepare_with_bullets(
    path: &Path,
    version: Option<&str>,
    date: &str,
    bullets: &[String],
) -> Result<String, ChangelogError> {
    let (changelog, request) = load_prepare_input(path, version, date)?;
    write_prepared(path, changelog, request, date, bullets)
}

fn load_prepare_input(
    path: &Path,
    version: Option<&str>,
    date: &str,
) -> Result<(Changelog, Request), ChangelogError> {
    let text = read(path)?;
    let changelog = Changelog::parse(&text).map_err(|message| invalid(path, message))?;
    if !is_valid_date(date) {
        return Err(invalid(
            path,
            format!("invalid release date `{date}`, expected `<yyyy-mm-dd>`"),
        ));
    }
    let request = match version {
        Some(version) => Request::parse(version).map_err(ChangelogError::Invalid)?,
        None => Request::Version(
            changelog
                .next_version()
                .map_err(|message| invalid(path, message))?,
        ),
    };
    Ok((changelog, request))
}

fn write_prepared(
    path: &Path,
    mut changelog: Changelog,
    request: Request,
    date: &str,
    bullets: &[String],
) -> Result<String, ChangelogError> {
    changelog
        .scaffold(request, date, bullets)
        .map_err(|message| invalid(path, message))?;
    write(path, &changelog.render())?;
    let prepared = request.heading();
    let suffix = if bullets.is_empty() {
        String::new()
    } else {
        format!(" ({} Git changes)", bullets.len())
    };
    Ok(format!(
        "prepared {prepared} in {}{}; edit the changelog content",
        path.display(),
        suffix
    ))
}

/// Extract the requested entry without imposing the top-entry release contract.
///
/// This compatibility API remains useful for historical notes. Release workflows
/// should use [`release_notes`] instead.
pub fn notes(
    changelog_path: &Path,
    version: &str,
    output_path: &Path,
) -> Result<String, ChangelogError> {
    let request = Request::parse(version).map_err(ChangelogError::Invalid)?;
    let changelog = load(changelog_path)?;
    let notes =
        render_notes(&changelog, request).map_err(|message| invalid(changelog_path, message))?;
    write(output_path, &notes)?;
    Ok(format!("wrote {}", output_path.display()))
}

/// Extract release notes from the newest entry and enforce tag agreement.
pub fn release_notes(
    changelog_path: &Path,
    target: ReleaseNotesTarget,
    output_path: &Path,
) -> Result<String, ChangelogError> {
    let changelog = load(changelog_path)?;
    let top = changelog.top().heading;
    if top == crate::changelog::Heading::Unreleased {
        return Err(invalid(
            changelog_path,
            "the newest entry is `(unreleased)`; promote it before releasing".to_owned(),
        ));
    }
    let request = target.requested().unwrap_or_else(|| match top {
        crate::changelog::Heading::Version(version) => Request::Version(version),
        crate::changelog::Heading::Unreleased => unreachable!("checked above"),
    });
    if request.heading() != top {
        return Err(invalid(
            changelog_path,
            format!(
                "release version `{}` must match the newest changelog entry `{top}`",
                request.heading()
            ),
        ));
    }
    let notes =
        render_notes(&changelog, request).map_err(|message| invalid(changelog_path, message))?;
    write(output_path, &notes)?;
    Ok(format!("wrote {}", output_path.display()))
}

fn render_notes(changelog: &Changelog, request: Request) -> Result<String, String> {
    let heading = request.heading();
    let entry = changelog
        .entry(heading)
        .ok_or_else(|| format!("entry `{heading}` was not found"))?;
    let body = substantive_body(entry).ok_or_else(|| {
        format!("entry `{heading}` is empty or contains only its `Released:` line")
    })?;
    let title = heading.heading();
    let title = title.trim_start_matches("## ");
    Ok(format!("# {title}\n\n## Changelog\n\n{body}\n"))
}

fn substantive_body(entry: &crate::changelog::Entry) -> Option<&str> {
    let body = entry.body.trim();
    let mut lines = body.lines();
    let first = lines.next()?;
    if first.starts_with("Released: ") && !lines.any(|line| !line.trim().is_empty()) {
        return None;
    }
    Some(body)
}

fn load(path: &Path) -> Result<Changelog, ChangelogError> {
    let text = read(path)?;
    Changelog::parse(&text).map_err(|message| invalid(path, message))
}

fn validate_ref(reference: &str) -> Result<(), ChangelogError> {
    if reference.is_empty()
        || reference.starts_with('-')
        || reference.chars().any(char::is_whitespace)
        || reference.contains('\0')
    {
        return Err(ChangelogError::Invalid(format!(
            "invalid Git ref `{reference}`"
        )));
    }
    Ok(())
}

fn git_merge_log(path: &Path, from: &str, to: &str) -> Result<String, ChangelogError> {
    let workdir = path.parent().unwrap_or_else(|| Path::new("."));
    let range = format!("{from}..{to}");
    let command = format!("git log --merges --first-parent {range}");
    let output = Command::new("git")
        .current_dir(workdir)
        .args([
            "log",
            "--merges",
            "--first-parent",
            "--format=%s%x1f%b%x1e",
            "--end-of-options",
            &range,
        ])
        .output()
        .map_err(|source| ChangelogError::Command {
            command: command.clone(),
            source,
        })?;
    if !output.status.success() {
        return Err(ChangelogError::CommandFailed {
            command,
            status: output.status.code(),
            stderr: bounded_text(&output.stderr, GIT_ERROR_BYTES_MAX),
        });
    }
    if output.stdout.len() > GIT_LOG_BYTES_MAX {
        return Err(ChangelogError::Invalid(format!(
            "Git merge log exceeds the {} MiB safety limit",
            GIT_LOG_BYTES_MAX / (1024 * 1024)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn bounded_text(bytes: &[u8], limit: usize) -> String {
    let truncated = bytes.len() > limit;
    let mut text = String::from_utf8_lossy(&bytes[..bytes.len().min(limit)])
        .trim()
        .to_owned();
    if truncated {
        text.push('…');
    }
    text
}

fn validate_pull_request_url(template: Option<&str>) -> Result<(), ChangelogError> {
    if let Some(template) = template
        && !template.contains("{number}")
    {
        return Err(ChangelogError::Invalid(
            "pull-request URL must contain the `{number}` placeholder".to_owned(),
        ));
    }
    Ok(())
}

fn format_git_bullets(log: &str, pull_request_url: Option<&str>) -> Vec<String> {
    log.split('\x1e')
        .filter_map(|record| {
            let record = record.trim();
            if record.is_empty() {
                return None;
            }
            let (subject, body) = record.split_once('\x1f').unwrap_or((record, ""));
            let subject = subject.trim();
            let pull_request = subject
                .strip_prefix("Merge pull request #")
                .and_then(|rest| rest.split_once(" from "))
                .and_then(|(number, branch)| {
                    number.parse::<u64>().ok().map(|number| (number, branch))
                });
            let summary = body
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .or_else(|| {
                    pull_request.map(|(_, branch)| branch.rsplit('/').next().unwrap_or(branch))
                })
                .unwrap_or(subject);
            let bullet = pull_request
                .map(|(number, _)| {
                    let reference = pull_request_url
                        .map(|template| {
                            format!(
                                "[#{}]({})",
                                number,
                                template.replace("{number}", &number.to_string())
                            )
                        })
                        .unwrap_or_else(|| format!("#{number}"));
                    format!("- {reference}\n\n  {summary}")
                })
                .unwrap_or_else(|| format!("- {summary}"));
            Some(bullet)
        })
        .collect()
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
    Read {
        path: PathBuf,
        source: io::Error,
    },
    Write {
        path: PathBuf,
        source: io::Error,
    },
    Command {
        command: String,
        source: io::Error,
    },
    CommandFailed {
        command: String,
        status: Option<i32>,
        stderr: String,
    },
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
            Self::Command { command, source } => {
                write!(formatter, "failed to run `{command}`: {source}")
            }
            Self::CommandFailed {
                command,
                status,
                stderr,
            } => {
                write!(formatter, "`{command}` failed")?;
                if let Some(status) = status {
                    write!(formatter, " with exit status {status}")?;
                }
                if !stderr.is_empty() {
                    write!(formatter, ": {stderr}")?;
                }
                Ok(())
            }
            Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

impl StdError for ChangelogError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Read { source, .. }
            | Self::Write { source, .. }
            | Self::Command { source, .. } => Some(source),
            Self::CommandFailed { .. } | Self::Invalid(_) => None,
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
    fn prepare_infers_the_next_patch_version_and_uses_the_supplied_date() {
        let temp = TempDir::new();
        let path = temp.path().join(DEFAULT_PATH);
        fs::write(&path, "# Changelog\n\n## 1.0.0\nReleased: 2026-01-01\n").unwrap();

        let message = prepare(&path, None, Some("2001-02-03")).unwrap();
        let rendered = fs::read_to_string(path).unwrap();

        assert!(message.contains("prepared ## 1.0.1"), "{message}");
        assert!(rendered.contains("## 1.0.1\n"), "{rendered}");
        assert!(rendered.contains("Released: 2001-02-03\n"), "{rendered}");
    }

    #[test]
    fn prepare_on_is_deterministic_without_a_clock() {
        let temp = TempDir::new();
        let path = temp.path().join(DEFAULT_PATH);
        fs::write(&path, "# Changelog\n\n## 1.0.0\nReleased: 2026-01-01\n").unwrap();

        prepare_on(&path, None, "2001-02-03").unwrap();

        assert!(
            fs::read_to_string(path)
                .unwrap()
                .contains("Released: 2001-02-03\n")
        );
    }

    #[test]
    fn prepare_rejects_an_invalid_date_without_writing() {
        let temp = TempDir::new();
        let path = temp.path().join(DEFAULT_PATH);
        let original = "# Changelog\n\n## 1.0.0\nReleased: 2026-01-01\n";
        fs::write(&path, original).unwrap();

        let error = prepare(&path, None, Some("2026-02-30")).unwrap_err();

        assert!(error.to_string().contains("invalid release date"));
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn prepare_can_start_an_unreleased_entry() {
        let temp = TempDir::new();
        let path = temp.path().join(DEFAULT_PATH);
        fs::write(&path, "# Changelog\n\n## 1.0.0\nReleased: 2026-01-01\n").unwrap();

        prepare(&path, Some("unreleased"), Some("2001-02-03")).unwrap();

        let rendered = fs::read_to_string(path).unwrap();
        assert!(rendered.starts_with("# Changelog\n\n## (unreleased)\n"));
        assert!(rendered.contains("Released: 2001-02-03\n"));
    }

    #[test]
    fn render_notes_is_pure_and_preserves_the_entry_body() {
        let changelog =
            Changelog::parse("# Changelog\n\n## 1.0.0\nReleased: 2026-09-11\n\n- Shipped.\n")
                .unwrap();

        let notes = render_notes(&changelog, Request::parse("1.0.0").unwrap()).unwrap();

        assert_eq!(
            notes,
            "# 1.0.0\n\n## Changelog\n\nReleased: 2026-09-11\n\n- Shipped.\n"
        );
    }

    #[test]
    fn release_notes_target_rejects_disagreeing_inputs() {
        let error = ReleaseNotesTarget::parse(Some("1.0.0"), Some("v1.0.1")).unwrap_err();

        assert!(error.to_string().contains("does not match --release-tag"));
    }

    #[test]
    fn release_notes_require_the_newest_entry() {
        let temp = TempDir::new();
        let changelog = temp.path().join(DEFAULT_PATH);
        let notes_path = temp.path().join(DEFAULT_NOTES_PATH);
        fs::write(
            &changelog,
            "# Changelog\n\n## 1.1.0\nReleased: 2026-09-12\n\n- New.\n\n## 1.0.0\nReleased: 2026-09-11\n\n- Old.\n",
        )
        .unwrap();

        let target = ReleaseNotesTarget::parse(Some("1.0.0"), None).unwrap();
        let error = release_notes(&changelog, target, &notes_path).unwrap_err();

        assert!(error.to_string().contains("must match the newest"));
        assert!(!notes_path.exists());
    }

    #[test]
    fn release_notes_default_to_the_newest_entry() {
        let temp = TempDir::new();
        let changelog = temp.path().join(DEFAULT_PATH);
        let notes_path = temp.path().join(DEFAULT_NOTES_PATH);
        fs::write(
            &changelog,
            "# Changelog\n\n## 1.1.0\nReleased: 2026-09-12\n\n- New.\n\n## 1.0.0\nReleased: 2026-09-11\n\n- Old.\n",
        )
        .unwrap();

        let target = ReleaseNotesTarget::parse(None, None).unwrap();
        release_notes(&changelog, target, &notes_path).unwrap();

        assert!(fs::read_to_string(notes_path).unwrap().contains("- New."));
    }

    #[test]
    fn git_merge_subjects_become_editable_bullets() {
        let log = "Merge pull request #42 from team/feature\x1f\nAdd the feature\x1eMerge branch fix\x1f\nFix the bug\x1e";

        assert_eq!(
            format_git_bullets(log, None),
            vec![
                "- #42\n\n  Add the feature".to_owned(),
                "- Fix the bug".to_owned(),
            ]
        );
        assert_eq!(
            format_git_bullets(
                log,
                Some("https://github.com/example/project/pull/{number}")
            ),
            vec![
                "- [#42](https://github.com/example/project/pull/42)\n\n  Add the feature"
                    .to_owned(),
                "- Fix the bug".to_owned(),
            ]
        );
    }

    #[test]
    fn bounded_git_errors_are_limited_and_marked() {
        let text = bounded_text(b"abcdefghijklmnopqrstuvwxyz", 8);

        assert_eq!(text, "abcdefgh…");
    }

    #[test]
    fn prepare_validates_local_input_before_running_git() {
        let temp = TempDir::new();
        let path = temp.path().join(DEFAULT_PATH);
        fs::write(&path, "not a changelog").unwrap();

        let error = prepare_from_git(&path, None, None, "bad ref", "HEAD", None).unwrap_err();

        assert!(error.to_string().contains("expected the file to start"));
    }

    #[test]
    fn scripted_git_log_prepares_without_spawning_git() {
        let temp = TempDir::new();
        let path = temp.path().join(DEFAULT_PATH);
        fs::write(&path, "# Changelog\n\n## 1.0.0\nReleased: 2026-01-01\n").unwrap();

        let message = prepare_from_git_with(
            &path,
            None,
            Some("2001-02-03"),
            "FROM",
            "TO",
            None,
            |_path, from, to| {
                assert_eq!((from, to), ("FROM", "TO"));
                Ok("Merge pull request #42 from team/feature\x1f\nAdd the feature\x1e".to_owned())
            },
        )
        .expect("scripted Git log succeeds");

        assert!(message.contains("1 Git changes"), "{message}");
        assert!(fs::read_to_string(path).unwrap().contains("#42"));
    }

    #[test]
    fn scripted_git_failure_is_returned_without_writing() {
        let temp = TempDir::new();
        let path = temp.path().join(DEFAULT_PATH);
        let original = "# Changelog\n\n## 1.0.0\nReleased: 2026-01-01\n";
        fs::write(&path, original).unwrap();

        let error = prepare_from_git_with(
            &path,
            None,
            Some("2001-02-03"),
            "FROM",
            "TO",
            None,
            |_path, _from, _to| {
                Err(ChangelogError::CommandFailed {
                    command: "git log".to_owned(),
                    status: Some(128),
                    stderr: "bad ref".to_owned(),
                })
            },
        )
        .expect_err("scripted Git failure is returned");

        assert!(matches!(error, ChangelogError::CommandFailed { .. }));
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn git_failure_leaves_the_changelog_untouched() {
        let temp = TempDir::new();
        let path = temp.path().join(DEFAULT_PATH);
        let original = "# Changelog\n\n## 1.0.0\nReleased: 2026-01-01\n";
        fs::write(&path, original).unwrap();

        let error = prepare_from_git(&path, None, None, "HEAD", "HEAD", None).unwrap_err();

        assert!(matches!(error, ChangelogError::CommandFailed { .. }));
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn pull_request_url_templates_require_the_number_placeholder() {
        let error = validate_pull_request_url(Some("https://example.test/pull")).unwrap_err();

        assert!(error.to_string().contains("{number}"));
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
