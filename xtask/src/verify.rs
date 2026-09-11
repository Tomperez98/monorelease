//! Project-specific release gates.
//!
//! These gates assert claims made by this repository's release. Generic
//! changelog, release-source, checksum, and artifact-manifest checks are provided by
//! the published `mono` CLI instead of being duplicated here.

use std::path::Path;

use crate::process::capture;
use crate::{Error, Tag};

const PREFIX: &str = "release-verify";

/// Plans the repository's real pipelines without executing the release
/// pipeline recursively. The release pipeline invokes this xtask, so running
/// it from here would recurse forever.
const PLANS: &[(&str, &[&str], &str)] = &[
    ("the root CI pipeline", &["plan", "ci"], "clippy"),
    (
        "the root release pipeline",
        &["plan", "release"],
        "release-verify",
    ),
];

pub fn run(tag: &Tag, binary: &Path) -> Result<(), Error> {
    if !binary.exists() {
        return Err(Error::Invalid(format!(
            "{} does not exist; run `cargo build` or set MONO_BIN",
            binary.display()
        )));
    }

    identity(tag, binary)?;
    self_check(binary)?;
    plans(binary)?;

    println!("{PREFIX}: release gates passed for {}", tag.name);
    Ok(())
}

/// The artifact must be the release it claims to be, and must know its own name:
/// a binary that reports the wrong version is a mis-built artifact, not a typo.
fn identity(tag: &Tag, binary: &Path) -> Result<(), Error> {
    let output = capture(binary, &["--version"])?;
    let reported = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let expected = format!("mono {}", tag.version);

    if !output.status.success() || reported != expected {
        return Err(Error::Command(format!(
            "{} reports `{reported}`, expected `{expected}`\n{}",
            binary.display(),
            crate::process::diagnostics(&output)
        )));
    }

    println!("{PREFIX}: {} reports `{reported}`", binary.display());
    Ok(())
}

/// The repository's own manifests, including the pipeline running these gates.
fn self_check(binary: &Path) -> Result<(), Error> {
    let output = capture(binary, &["check"])?;
    if !output.status.success() {
        return Err(Error::Command(format!(
            "`{} check` rejected this repository's manifests\n{}",
            binary.display(),
            crate::process::diagnostics(&output)
        )));
    }
    Ok(())
}

/// Planning is what a user reads before running anything, so it has to resolve
/// the repository's real graphs without entering a recursive release run.
fn plans(binary: &Path) -> Result<(), Error> {
    for (description, args, expected) in PLANS {
        let output = capture(binary, args)?;
        let stdout = String::from_utf8_lossy(&output.stdout);

        if !output.status.success() || !stdout.contains(expected) {
            return Err(Error::Command(format!(
                "`{} {}` does not plan {expected}\n{}",
                binary.display(),
                args.join(" "),
                crate::process::diagnostics(&output)
            )));
        }

        println!("{PREFIX}: {description} plans successfully");
    }
    Ok(())
}
