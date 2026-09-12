//! Release coordination that must happen before publishing.
//!
//! The workflow provides credentials and GitHub Pages actions. This module owns
//! the repository-local release boundary: stamp the release version, build the
//! versioned documentation, verify its output, and restore the committed
//! placeholders before returning.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use mono::Version;

use crate::Error;
use crate::stamp;

pub(crate) const COMPONENT: &str = "release-docs";
pub(crate) const PREPARE_COMPONENT: &str = "release-prepare";
pub(crate) const PUBLISH_COMPONENT: &str = "release-publish";
const ZENSICAL_VERSION: &str = "0.0.61";

/// Run the repository-local release gates from one stamped checkout. The
/// workflow supplies the toolchain; this coordinator owns source validation,
/// changelog notes, CI, release gates, packaging, and restoration.
pub(crate) fn prepare(root: &Path, tag: &str, version: Version) -> Result<(), Error> {
    let commit = git_output(root, &["rev-parse", "HEAD"])?;
    run_command(
        root,
        "cargo",
        [
            "run", "--locked", "--quiet", "--", "release", "source", "--tag", tag, "--commit",
            &commit,
        ],
    )?;
    stamp::apply(root, version)?;

    let result = (|| {
        run_command(
            root,
            "cargo",
            [
                "run",
                "--locked",
                "--",
                "ci",
                "--no-cache",
                "--output",
                "text",
                "--ui",
                "stream",
            ],
        )?;
        run_command(
            root,
            "cargo",
            [
                "run",
                "--locked",
                "--",
                "run",
                "release",
                "--no-cache",
                "--output",
                "text",
                "--ui",
                "stream",
            ],
        )?;
        run_command(root, "cargo", ["package", "--locked", "--allow-dirty"])?;
        if !root.join("RELEASE_NOTES.md").is_file() {
            return Err(Error::Invalid(
                "release gates completed without producing RELEASE_NOTES.md".to_owned(),
            ));
        }
        Ok(())
    })();

    let restore = stamp::restore(root);
    match (result, restore) {
        (Ok(()), Ok(())) => {
            println!("{PREPARE_COMPONENT}: release gates and package passed for {tag}");
            Ok(())
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(restore)) => Err(Error::Command(format!(
            "release preparation failed: {error}; restoring pinned files also failed: {restore}"
        ))),
    }
}

/// Build the release-tagged documentation without leaving release versions in
/// the checkout. The caller must provide a parsed release version; parsing
/// belongs at the CLI boundary.
pub(crate) fn docs(root: &Path, version: Version) -> Result<(), Error> {
    stamp::apply(root, version)?;

    let build_result = build_site(root, version);
    let restore_result = stamp::restore(root);

    match (build_result, restore_result) {
        (Ok(()), Ok(())) => {
            println!("{COMPONENT}: built versioned documentation in site/");
            Ok(())
        }
        (Err(build), Ok(())) => Err(build),
        (Ok(()), Err(restore)) => Err(restore),
        (Err(build), Err(restore)) => Err(Error::Command(format!(
            "documentation build failed: {build}; restoring pinned files also failed: {restore}"
        ))),
    }
}

/// Create or reuse the draft GitHub release and upload the assembled release
/// directory. A separate `finalize` invocation publishes it after Pages has
/// deployed the documentation.
pub(crate) fn publish(
    tag: &str,
    directory: &Path,
    notes: &Path,
    finalize: bool,
) -> Result<(), Error> {
    let repository = required_env("GITHUB_REPOSITORY")?;
    if finalize {
        run_gh([
            "release",
            "edit",
            tag,
            "--repo",
            &repository,
            "--title",
            tag,
            "--notes-file",
            notes.to_str().ok_or_else(|| {
                Error::Invalid(format!(
                    "release notes path is not valid UTF-8: {}",
                    notes.display()
                ))
            })?,
            "--draft=false",
            "--latest",
        ])?;
        println!("{PUBLISH_COMPONENT}: published {tag}");
        return Ok(());
    }

    create_or_reuse_draft(tag, &repository, notes)?;
    let artifacts = artifact_paths(directory)?;
    let artifact_args = artifacts
        .iter()
        .map(|path| {
            path.to_str().ok_or_else(|| {
                Error::Invalid(format!(
                    "release artifact path is not valid UTF-8: {}",
                    path.display()
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut upload_args = vec!["release", "upload", tag];
    upload_args.extend(artifact_args.iter().copied());
    upload_args.extend(["--repo", &repository, "--clobber"]);
    run_gh(upload_args)?;
    println!(
        "{PUBLISH_COMPONENT}: uploaded {} artifacts to draft {tag}",
        artifacts.len()
    );
    Ok(())
}

fn run_command<const N: usize>(root: &Path, program: &str, args: [&str; N]) -> Result<(), Error> {
    let status = Command::new(program)
        .args(args)
        .current_dir(root)
        .status()
        .map_err(|source| Error::Spawn {
            program: format!("{program} {}", args.join(" ")),
            source,
        })?;
    if status.success() {
        return Ok(());
    }
    Err(Error::Command(format!(
        "`{program} {}` failed with {status}",
        args.join(" ")
    )))
}

fn git_output(root: &Path, args: &[&str]) -> Result<String, Error> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|source| Error::Spawn {
            program: format!("git {}", args.join(" ")),
            source,
        })?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned());
    }
    Err(Error::Command(format!(
        "`git {}` failed with {}{}",
        args.join(" "),
        output.status,
        diagnostics(&output)
    )))
}

fn create_or_reuse_draft(tag: &str, repository: &str, notes: &Path) -> Result<(), Error> {
    let view = gh_output([
        "release", "view", tag, "--repo", repository, "--json", "isDraft", "--jq", ".isDraft",
    ])?;

    match view.trim() {
        "false" => Err(Error::Invalid(format!(
            "{tag} is already published; refusing to overwrite it"
        ))),
        "true" => {
            println!("{PUBLISH_COMPONENT}: reusing existing draft {tag}");
            Ok(())
        }
        "" => {
            let notes = notes.to_str().ok_or_else(|| {
                Error::Invalid(format!(
                    "release notes path is not valid UTF-8: {}",
                    notes.display()
                ))
            })?;
            run_gh([
                "release",
                "create",
                tag,
                "--repo",
                repository,
                "--verify-tag",
                "--draft",
                "--title",
                tag,
                "--notes-file",
                notes,
            ])?;
            println!("{PUBLISH_COMPONENT}: created draft {tag}");
            Ok(())
        }
        state => Err(Error::Invalid(format!(
            "unexpected `gh release view` state `{state}` for {tag}"
        ))),
    }
}

fn artifact_paths(directory: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut paths = fs::read_dir(directory)
        .map_err(|source| Error::Io {
            path: directory.to_path_buf(),
            source,
        })?
        .map(|entry| {
            let entry = entry.map_err(|source| Error::Io {
                path: directory.to_path_buf(),
                source,
            })?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|source| Error::Io {
                path: path.clone(),
                source,
            })?;
            if file_type.is_file() {
                Ok(Some(path))
            } else {
                Ok(None)
            }
        })
        .collect::<Result<Vec<_>, Error>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    paths.sort();
    if paths.is_empty() {
        return Err(Error::Invalid(format!(
            "release artifact directory {} contains no files",
            directory.display()
        )));
    }
    Ok(paths)
}

fn required_env(name: &str) -> Result<String, Error> {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::Invalid(format!("{name} is not set")))
}

fn run_gh<'a>(args: impl IntoIterator<Item = &'a str>) -> Result<(), Error> {
    let args = args.into_iter().collect::<Vec<_>>();
    let output = Command::new("gh")
        .args(&args)
        .output()
        .map_err(|source| Error::Spawn {
            program: format!("gh {}", args.join(" ")),
            source,
        })?;
    if output.status.success() {
        return Ok(());
    }
    Err(Error::Command(format!(
        "`gh {}` failed with {}{}",
        args.join(" "),
        output.status,
        diagnostics(&output)
    )))
}

fn gh_output<'a>(args: impl IntoIterator<Item = &'a str>) -> Result<String, Error> {
    let args = args.into_iter().collect::<Vec<_>>();
    let output = Command::new("gh")
        .args(&args)
        .output()
        .map_err(|source| Error::Spawn {
            program: format!("gh {}", args.join(" ")),
            source,
        })?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned());
    }
    // A missing release is the normal first-run state. Treat it as an empty
    // state so the caller creates the draft; other failures are surfaced.
    if output.status.code() == Some(1) {
        return Ok(String::new());
    }
    Err(Error::Command(format!(
        "`gh {}` failed with {}{}",
        args.join(" "),
        output.status,
        diagnostics(&output)
    )))
}

fn diagnostics(output: &Output) -> String {
    let mut text = String::new();
    for stream in [&output.stdout, &output.stderr] {
        let stream = String::from_utf8_lossy(stream);
        for line in stream.lines().filter(|line| !line.trim().is_empty()) {
            text.push('\n');
            text.push_str("    ");
            text.push_str(line);
        }
    }
    text
}

fn build_site(root: &Path, version: Version) -> Result<(), Error> {
    let status = Command::new("uvx")
        .args([
            "--from",
            &format!("zensical=={ZENSICAL_VERSION}"),
            "zensical",
            "build",
            "--clean",
            "--config-file",
            "zensical.toml",
        ])
        .current_dir(root)
        .status()
        .map_err(|source| Error::Spawn {
            program: "uvx zensical build".to_owned(),
            source,
        })?;

    if !status.success() {
        return Err(Error::Command(format!(
            "`uvx --from zensical=={ZENSICAL_VERSION} zensical build` failed with {status} for {version}"
        )));
    }

    let index = root.join("site/index.html");
    if !index.is_file() {
        return Err(Error::Invalid(format!(
            "Zensical completed but did not produce {}",
            index.display()
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn artifact_paths_are_sorted_and_exclude_directories() {
        let temp = TempDir::new();
        fs::write(temp.path().join("z-last"), "last").unwrap();
        fs::write(temp.path().join("a-first"), "first").unwrap();
        fs::create_dir(temp.path().join("nested")).unwrap();

        let paths = artifact_paths(temp.path()).unwrap();
        let names = paths
            .iter()
            .map(|path| path.file_name().unwrap().to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, ["a-first", "z-last"]);
    }

    #[test]
    fn artifact_paths_reject_an_empty_directory() {
        let temp = TempDir::new();
        let error = artifact_paths(temp.path()).unwrap_err();
        assert!(error.to_string().contains("contains no files"), "{error}");
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "mono-xtask-release-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
