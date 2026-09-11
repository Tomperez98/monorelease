//! Project-specific release gates.
//!
//! These gates assert claims made by this repository's release. Generic
//! changelog, source, checksum, and artifact-manifest checks are provided by
//! the published `mono` CLI instead of being duplicated here.

use std::path::Path;

use crate::process::capture;
use crate::{Error, Tag};

const PREFIX: &str = "release-verify";

/// What a working binary must still do, run through `--no-cache` so every
/// result comes from this run rather than from a previous one.
const WORKLOADS: &[(&str, &[&str])] = &[
    (
        "examples/echo builds in dependency order",
        &[
            "--dir",
            "examples/echo",
            "task",
            "app-build",
            "--jobs",
            "2",
            "--no-cache",
        ],
    ),
    (
        "examples/release-gate runs its release pipeline",
        &[
            "--dir",
            "examples/release-gate",
            "run",
            "release",
            "--jobs",
            "3",
            "--no-cache",
        ],
    ),
];

const PLAN: &[&str] = &["--dir", "examples/release-gate", "plan", "release"];
const PLAN_EXPECTED: &str = "release-verify";

pub fn run(tag: &Tag, binary: &Path) -> Result<(), Error> {
    if !binary.exists() {
        return Err(Error::Invalid(format!(
            "{} does not exist; run `cargo build` or set MONO_BIN",
            binary.display()
        )));
    }

    identity(tag, binary)?;
    self_check(binary)?;
    workloads(binary)?;
    plan(binary)?;

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

fn workloads(binary: &Path) -> Result<(), Error> {
    for (description, args) in WORKLOADS {
        let output = capture(binary, args)?;
        let stdout = String::from_utf8_lossy(&output.stdout);

        // A task graph that failed, or that reports a failure in its summary,
        // means the binary cannot build its own examples.
        if !output.status.success() || !stdout.contains("0 failed") {
            return Err(Error::Command(format!(
                "`{} {}` did not report `0 failed`\n{}",
                binary.display(),
                args.join(" "),
                crate::process::diagnostics(&output)
            )));
        }

        println!("{PREFIX}: {description}");
    }
    Ok(())
}

/// Planning is what a user reads before running anything, so it has to resolve
/// the graph the documentation promises.
fn plan(binary: &Path) -> Result<(), Error> {
    let output = capture(binary, PLAN)?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    if !output.status.success() || !stdout.contains(PLAN_EXPECTED) {
        return Err(Error::Command(format!(
            "`{} {}` does not plan {PLAN_EXPECTED}\n{}",
            binary.display(),
            PLAN.join(" "),
            crate::process::diagnostics(&output)
        )));
    }

    println!("{PREFIX}: examples/release-gate plans {PLAN_EXPECTED}");
    Ok(())
}
