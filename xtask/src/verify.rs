//! The release gates.
//!
//! These run twice with the same code: before publishing, against the binary
//! just built, and after publishing, against the published artifact. Each gate
//! asserts something the release claims, so a claim that stops being true stops
//! the release instead of reaching users.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

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
            "build",
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
const PLAN_EXPECTED: &str = "workspace:release-verify";

pub fn run(tag: &Tag, binary: &Path, assets: Option<&Path>) -> Result<(), Error> {
    if !binary.exists() {
        return Err(Error::Invalid(format!(
            "{} does not exist; run `cargo build` or set MONORELEASE_BIN",
            binary.display()
        )));
    }

    identity(tag, binary)?;
    self_check(binary)?;
    if let Some(assets) = assets {
        checksums(assets)?;
    }
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
    let expected = format!("monorelease {}", tag.version);

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

/// Every file listed in `SHA256SUMS` must be present and hash to the recorded
/// digest, so a corrupted or replaced download cannot pass.
fn checksums(directory: &Path) -> Result<(), Error> {
    let sums_path = directory.join("SHA256SUMS");
    if !sums_path.exists() {
        return Err(Error::Invalid(format!(
            "{} is missing",
            sums_path.display()
        )));
    }

    let sums = crate::read_to_string(&sums_path)?;
    let mut verified = 0_usize;
    for (line_number, line) in sums.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((expected, name)) = parse_sum_line(line) else {
            return Err(Error::Invalid(format!(
                "{}:{}: expected `<sha256>  <file>`, found `{line}`",
                sums_path.display(),
                line_number + 1
            )));
        };

        let path = directory.join(name);
        let actual = digest(&path)?;
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(Error::Command(format!(
                "{name}: FAILED\n  expected {expected}\n  actual   {actual}"
            )));
        }
        println!("{name}: OK");
        verified += 1;
    }

    if verified == 0 {
        return Err(Error::Invalid(format!(
            "{} lists no files",
            sums_path.display()
        )));
    }

    println!("{PREFIX}: checksums in {} verified", directory.display());
    Ok(())
}

/// A `sha256sum` line: a hex digest, whitespace, then a file name, optionally
/// preceded by `*` for binary mode.
fn parse_sum_line(line: &str) -> Option<(&str, &str)> {
    let (digest, rest) = line.split_once(char::is_whitespace)?;
    let name = rest.trim_start().trim_start_matches('*');
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) || name.is_empty()
    {
        return None;
    }
    Some((digest, name))
}

fn digest(path: &Path) -> Result<String, Error> {
    let mut file = fs::File::open(path).map_err(|source| Error::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(|source| Error::Read {
        path: path.to_path_buf(),
        source,
    })?;

    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(hex)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_sha256sum_line_shapes() {
        let hex = "0".repeat(64);
        assert_eq!(
            parse_sum_line(&format!("{hex}  monorelease.tar.gz"))
                .unwrap()
                .1,
            "monorelease.tar.gz"
        );
        assert_eq!(
            parse_sum_line(&format!("{hex} *monorelease.zip"))
                .unwrap()
                .1,
            "monorelease.zip"
        );
    }

    #[test]
    fn rejects_a_line_that_is_not_a_checksum() {
        assert!(parse_sum_line("not a checksum").is_none());
        assert!(parse_sum_line(&format!("{}  file", "0".repeat(63))).is_none());
        assert!(parse_sum_line(&format!("{}  file", "z".repeat(64))).is_none());
        assert!(parse_sum_line(&format!("{}  ", "0".repeat(64))).is_none());
    }

    #[test]
    fn digests_the_empty_input_like_sha256sum_does() {
        let path = std::env::temp_dir().join("xtask-digest-test");
        fs::write(&path, "").unwrap();
        let digest = digest(&path).unwrap();
        fs::remove_file(&path).unwrap();

        assert_eq!(
            digest,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
