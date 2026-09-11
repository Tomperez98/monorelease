//! Asking git about the repository.
//!
//! Everything here shells out to `git` and parses its output. The parsing is
//! split into pure functions so it can be tested without a repository.

use std::path::Path;
use std::process::Command;

use crate::Error;

/// Field and record separators for `git log`. They cannot occur in a commit
/// subject or body, which makes the output unambiguous even when a merge commit
/// has a multi-line body.
const FIELD: char = '\u{1f}';
const RECORD: char = '\u{1e}';

/// A pull request merged since the last release.
#[derive(Debug, PartialEq, Eq)]
pub struct Bullet {
    pub pull_request: u32,
    pub title: String,
}

impl Bullet {
    /// A changelog line linking the pull request and summarising it.
    pub fn line(&self, slug: &str) -> String {
        format!(
            "- [#{}](https://github.com/{}/pull/{})\n  {}",
            self.pull_request, slug, self.pull_request, self.title
        )
    }
}

/// The commits to describe: since the last release tag, or all of them.
pub fn range() -> Result<String, Error> {
    Ok(match last_tag()? {
        Some(tag) => format!("{tag}..HEAD"),
        None => "HEAD".to_owned(),
    })
}

/// The most recent release tag reachable from `HEAD`.
pub fn last_tag() -> Result<Option<String>, Error> {
    let output = git(&["describe", "--tags", "--abbrev=0"])?;
    // No tag yet is not an error: the first release has no history to describe.
    if !output.status.success() {
        return Ok(None);
    }
    let tag = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok(Some(tag).filter(|tag| !tag.is_empty()))
}

/// The merge commits in `range`, as `(subject, body)` records.
pub fn merges(range: &str) -> Result<Vec<Bullet>, Error> {
    let format = format!("tformat:%s{FIELD}%b{RECORD}");
    let output = git(&[
        "log",
        "--merges",
        "--first-parent",
        &format!("--pretty={format}"),
        range,
    ])?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Command(format!(
            "`git log {range}` failed: {}",
            stderr.trim()
        )));
    }
    Ok(parse_merges(&String::from_utf8_lossy(&output.stdout)))
}

/// `owner/name` of the `origin` remote, used to link pull requests.
pub fn remote_slug() -> Result<String, Error> {
    let output = git(&["config", "--get", "remote.origin.url"])?;
    if !output.status.success() {
        return Err(Error::Command(
            "`git config remote.origin.url` found no origin remote".to_owned(),
        ));
    }
    let url = String::from_utf8_lossy(&output.stdout);
    parse_slug(url.trim()).ok_or_else(|| {
        Error::Command(format!(
            "cannot derive a repository from the origin url `{}`",
            url.trim()
        ))
    })
}

fn git(args: &[&str]) -> Result<std::process::Output, Error> {
    Command::new("git")
        .args(args)
        .current_dir(Path::new("."))
        .output()
        .map_err(|source| Error::Spawn {
            program: format!("git {}", args.join(" ")),
            source,
        })
}

/// `git@github.com:owner/name.git` and `https://github.com/owner/name` both
/// describe `owner/name`.
fn parse_slug(url: &str) -> Option<String> {
    let path = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/')?.1,
        None => url.split_once(':')?.1,
    };
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    (!owner.is_empty() && !name.is_empty() && !name.contains('/'))
        .then(|| format!("{owner}/{name}"))
}

/// Split `git log` output into one bullet per merge.
///
/// A merge whose subject names no pull request is skipped: it is a local merge,
/// and inventing a bullet for it would put noise in the changelog.
fn parse_merges(output: &str) -> Vec<Bullet> {
    output
        .split(RECORD)
        .filter_map(|record| {
            let (subject, body) = record.split_once(FIELD)?;
            let pull_request = pull_request_number(subject)?;
            let title = body
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or_else(|| subject.trim());
            Some(Bullet {
                pull_request,
                title: title.to_owned(),
            })
        })
        .collect()
}

/// The first `#123` in a subject, which is how GitHub names a pull request both
/// for merge commits and for squash merges.
fn pull_request_number(subject: &str) -> Option<u32> {
    let rest = subject.split_once('#')?.1;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_merge_commits_into_bullets() {
        let log = format!(
            "Merge pull request #5 from o/branch{FIELD}Fix the thing{RECORD}\
             Merge pull request #4 from o/other{FIELD}{RECORD}\
             Merge branch 'main' into feature{FIELD}Local merge{RECORD}"
        );
        let bullets = parse_merges(&log);

        assert_eq!(bullets.len(), 2);
        assert_eq!(
            bullets[0],
            Bullet {
                pull_request: 5,
                title: "Fix the thing".to_owned()
            }
        );
        // An empty body falls back to the subject rather than an empty line.
        assert_eq!(bullets[1].title, "Merge pull request #4 from o/other");
    }

    #[test]
    fn parses_a_multi_line_body_from_its_first_line() {
        let log = format!(
            "Merge pull request #9 from o/b{FIELD}The title\n\nA longer explanation.{RECORD}"
        );
        assert_eq!(parse_merges(&log)[0].title, "The title");
    }

    #[test]
    fn parses_squash_merge_subjects() {
        assert_eq!(pull_request_number("Add a cache (#412)"), Some(412));
        assert_eq!(
            pull_request_number("Merge pull request #7 from o/b"),
            Some(7)
        );
        assert_eq!(pull_request_number("No pull request here"), None);
        // A truncated number is not a number.
        assert_eq!(pull_request_number("Fix #12 and more"), Some(12));
        assert_eq!(pull_request_number("Issue #"), None);
    }

    #[test]
    fn renders_a_bullet_like_the_changelog_expects() {
        let bullet = Bullet {
            pull_request: 7,
            title: "Fix the thing".to_owned(),
        };
        assert_eq!(
            bullet.line("Tomperez98/monorelease"),
            "- [#7](https://github.com/Tomperez98/monorelease/pull/7)\n  Fix the thing"
        );
    }

    #[test]
    fn derives_a_slug_from_both_remote_url_forms() {
        assert_eq!(
            parse_slug("git@github.com:Tomperez98/monorelease.git").unwrap(),
            "Tomperez98/monorelease"
        );
        assert_eq!(
            parse_slug("https://github.com/Tomperez98/monorelease").unwrap(),
            "Tomperez98/monorelease"
        );
        assert_eq!(
            parse_slug("https://github.com/Tomperez98/monorelease.git").unwrap(),
            "Tomperez98/monorelease"
        );
        assert!(parse_slug("not a url").is_none());
    }
}
