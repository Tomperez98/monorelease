//! Create and push the annotated tag that starts the release workflow.

use std::process::Command;

use mono::Version;

use crate::Error;

pub(crate) const COMPONENT: &str = "release-tag";
const REMOTE: &str = "origin";

/// Create and push the release tag.
///
/// The repository's `release.yml` is triggered by a tag push, so this command
/// deliberately does not dispatch GitHub Actions directly. The tag points at
/// the current `HEAD`, and the pushed tag is the immutable release source.
pub(crate) fn run(tag: &str) -> Result<(), Error> {
    validate_tag(tag)?;

    let action = match local_tag_commit(tag)? {
        Some(tag_commit) => {
            let head = git_output(&["rev-parse", "HEAD"])?;
            if tag_commit != head {
                return Err(Error::Invalid(format!(
                    "release tag {tag} already points at {tag_commit}, not HEAD {head}"
                )));
            }
            "reused"
        }
        None => {
            run_git(&[
                "tag",
                "--annotate",
                tag,
                "--message",
                &format!("Release {tag}"),
            ])?;
            "created"
        }
    };

    run_git(&["push", REMOTE, tag])?;

    println!("{COMPONENT}: {action} and pushed {tag}");
    Ok(())
}

fn local_tag_commit(tag: &str) -> Result<Option<String>, Error> {
    let listed = git_output(&["tag", "--list", tag])?;
    if listed.is_empty() {
        return Ok(None);
    }

    let commit_ref = format!("refs/tags/{tag}^{{commit}}");
    Ok(Some(git_output(&["rev-parse", "--verify", &commit_ref])?))
}

fn validate_tag(tag: &str) -> Result<(), Error> {
    let version = tag.strip_prefix('v').ok_or_else(|| invalid_tag(tag))?;
    if Version::parse(version).is_none() {
        return Err(invalid_tag(tag));
    }
    Ok(())
}

fn invalid_tag(tag: &str) -> Error {
    Error::Invalid(format!(
        "release tag must match `v<major>.<minor>.<patch>`, got `{tag}`"
    ))
}

fn run_git(args: &[&str]) -> Result<(), Error> {
    let status = Command::new("git")
        .args(args)
        .status()
        .map_err(|source| Error::Spawn {
            program: format!("git {}", args.join(" ")),
            source,
        })?;

    if !status.success() {
        return Err(Error::Command(format!(
            "`git {}` failed with {status}",
            args.join(" ")
        )));
    }
    Ok(())
}

fn git_output(args: &[&str]) -> Result<String, Error> {
    let output = Command::new("git")
        .args(args)
        .output()
        .map_err(|source| Error::Spawn {
            program: format!("git {}", args.join(" ")),
            source,
        })?;

    if !output.status.success() {
        return Err(Error::Command(format!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_version_tags() {
        for tag in ["v0.1.3", "v1.20.300"] {
            assert!(validate_tag(tag).is_ok(), "rejected {tag}");
        }

        for tag in ["0.1.3", "v1.2", "v1.2.3-rc1", "v1 2 3", "v"] {
            assert!(validate_tag(tag).is_err(), "accepted {tag}");
        }
    }
}
