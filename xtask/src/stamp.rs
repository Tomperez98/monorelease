//! Stamp the pinned manifest placeholders with the release version.
//!
//! This repository keeps release-version placeholders in its release files:
//! `0.0.0` in `Cargo.toml`, in the `mono` package entry of `Cargo.lock`, and in
//! the displayed Zensical site name/footer. The release process rewrites all
//! three from the tag just before building. The committed files therefore never
//! drift, and the CHANGELOG heading (gated against `RELEASE_TAG` by the
//! `release-notes` task) is the only version source.
//!
//! Rewriting is paired with `--restore` so a local run leaves the working tree
//! clean, mirroring TigerBeetle's `backup_create`/`backup_restore`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use mono::Version;

use crate::Error;

pub(crate) const COMPONENT: &str = "release-stamp";

/// The committed placeholders. Each stamped file must carry its placeholder
/// exactly once, in the place this module expects; anything else is an error
/// rather than a silent partial rewrite.
const CARGO_PLACEHOLDER: &str = "0.0.0";
const CARGO_TOML: &str = "Cargo.toml";
const CARGO_LOCK: &str = "Cargo.lock";
const ZENSICAL_TOML: &str = "zensical.toml";
const ZENSICAL_PLACEHOLDER: &str = "v0.0.0";
/// The lockfile package whose version tracks the workspace release.
const LOCK_PACKAGE: &str = "mono";
const BACKUP_SUFFIX: &str = ".backup";

/// Stamp all release manifests with `version`, refusing to clobber an existing backup.
pub(crate) fn apply(root: &Path, version: Version) -> Result<(), Error> {
    let version_text = version.to_string();
    if version_text == CARGO_PLACEHOLDER {
        return Err(Error::Invalid(format!(
            "{CARGO_PLACEHOLDER} is the pinned placeholder, not a release version"
        )));
    }
    let mut staged: Vec<(PathBuf, String, String)> = Vec::with_capacity(3);

    for (relative, stamp) in targets() {
        let path = root.join(relative);
        let backup = backup_path(&path);
        if backup.exists() {
            return Err(Error::Invalid(format!(
                "{} already exists; run `release-stamp --restore` before stamping again",
                backup.display()
            )));
        }

        let original = read(&path)?;
        let stamped = stamp(&original, &version_text)
            .map_err(|message| Error::Invalid(format!("{}: {message}", path.display())))?;
        staged.push((path, original, stamped));
    }

    // Only touch the working tree once every file has stamped successfully, so a
    // failure cannot leave one manifest rewritten and the other untouched.
    for (path, original, stamped) in staged {
        if let Err(error) =
            write(&backup_path(&path), &original).and_then(|()| write(&path, &stamped))
        {
            let rollback = restore_backups(root);
            return if let Err(rollback) = rollback {
                Err(Error::Invalid(format!(
                    "stamping failed: {error}; rollback also failed: {rollback}"
                )))
            } else {
                Err(error)
            };
        }
    }

    println!(
        "{COMPONENT}: stamped {CARGO_TOML}, {CARGO_LOCK}, and {ZENSICAL_TOML} to {version_text}"
    );
    Ok(())
}

/// Restore the committed placeholders from their backups.
///
/// Missing backups are not an error: a fresh checkout has nothing to restore,
/// and a release build that never stamped is already in the pinned state.
pub(crate) fn restore(root: &Path) -> Result<(), Error> {
    let backups = targets()
        .iter()
        .filter(|(relative, _)| backup_path(&root.join(relative)).exists())
        .count();
    if backups != 0 && backups != targets().len() {
        return Err(Error::Invalid(format!(
            "incomplete stamp state: found {backups} of {} backups; refusing partial restore",
            targets().len()
        )));
    }
    let restored = restore_backups(root)?;

    if restored.is_empty() {
        println!("{COMPONENT}: no backups found; placeholders are already in place");
    } else {
        println!("{COMPONENT}: restored {} from backup", restored.join(", "));
    }
    Ok(())
}

fn restore_file(backup: &Path, path: &Path) -> Result<(), Error> {
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
    }
    fs::rename(backup, path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn restore_backups(root: &Path) -> Result<Vec<&'static str>, Error> {
    let mut restored = Vec::new();
    for (relative, _) in targets() {
        let path = root.join(relative);
        let backup = backup_path(&path);
        if !backup.exists() {
            continue;
        }
        restore_file(&backup, &path)?;
        restored.push(relative);
    }
    Ok(restored)
}

/// The repository root, independent of the caller's working directory.
pub(crate) fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask is a workspace member with a parent directory")
        .to_path_buf()
}

/// A pure transformation from committed manifest text to stamped manifest text.
type Stamp = fn(&str, &str) -> Result<String, String>;

/// The files to stamp and the pure transformation each one uses.
fn targets() -> [(&'static str, Stamp); 3] {
    [
        (CARGO_TOML, stamped_cargo_toml),
        (CARGO_LOCK, stamped_cargo_lock),
        (ZENSICAL_TOML, stamped_zensical_toml),
    ]
}

/// `version` in the root `[package]` table, which is the only unqualified
/// `version = "0.0.0"` line in the manifest. Dependency versions sit inside
/// inline tables, so they never match a whole line.
fn stamped_cargo_toml(text: &str, version: &str) -> Result<String, String> {
    replace_placeholder_lines(text, version, CARGO_PLACEHOLDER, |_| true, CARGO_TOML)
}

/// The `mono` entry of the lockfile. Other packages (for example `xtask`) may
/// legitimately keep `0.0.0`, so only the block named `mono` is rewritten.
fn stamped_cargo_lock(text: &str, version: &str) -> Result<String, String> {
    let mut current_package: Option<String> = None;
    replace_placeholder_lines(
        text,
        version,
        CARGO_PLACEHOLDER,
        |trimmed| {
            if trimmed == "[[package]]" {
                current_package = None;
            } else if let Some(name) = trimmed.strip_prefix("name = ") {
                current_package = Some(name.trim().trim_matches('"').to_owned());
            }
            current_package.as_deref() == Some(LOCK_PACKAGE)
        },
        CARGO_LOCK,
    )
}

/// Stamp the version displayed in the Zensical header and footer.
fn stamped_zensical_toml(text: &str, version: &str) -> Result<String, String> {
    let replacement = format!("v{version}");
    let mut output = String::with_capacity(text.len());
    let mut replacements = 0usize;

    for line in text.split_inclusive('\n') {
        let body = line_body(line);
        let trimmed = body.trim();
        let in_scope = trimmed.starts_with("site_name = \"Mono ")
            || trimmed.starts_with("copyright = \"Mono ");

        if in_scope && body.matches(ZENSICAL_PLACEHOLDER).count() == 1 {
            output.push_str(&body.replace(ZENSICAL_PLACEHOLDER, &replacement));
            output.push_str(line_ending(line));
            replacements += 1;
        } else {
            output.push_str(line);
        }
    }

    match replacements {
        2 => Ok(output),
        0 => Err(format!(
            "no `{ZENSICAL_PLACEHOLDER}` lines to stamp; {ZENSICAL_TOML} is already stamped or the placeholders changed"
        )),
        count => Err(format!(
            "found {count} `{ZENSICAL_PLACEHOLDER}` lines; expected 2 in {ZENSICAL_TOML}"
        )),
    }
}

/// Replace every whole line equal to `version = "<placeholder>"` for which
/// `in_scope` says the line belongs to the target. The function is called for
/// each line in order, before the decision to replace, so callers can track
/// section state.
///
/// Returns an error unless exactly one line is replaced: zero means the file is
/// already stamped (or the placeholder was edited away), and more than one means
/// the manifest layout changed and rewriting it would be guesswork.
fn replace_placeholder_lines(
    text: &str,
    version: &str,
    placeholder: &str,
    mut in_scope: impl FnMut(&str) -> bool,
    label: &str,
) -> Result<String, String> {
    let needle = format!("version = \"{placeholder}\"");
    let replacement = format!("version = \"{version}\"");
    let mut output = String::with_capacity(text.len());
    let mut replacements = 0usize;

    for line in text.split_inclusive('\n') {
        let body = line_body(line);
        let trimmed = body.trim();
        let scope = in_scope(trimmed);
        if scope && trimmed == needle {
            let indent = &body[..body.len() - body.trim_start().len()];
            output.push_str(indent);
            output.push_str(&replacement);
            output.push_str(line_ending(line));
            replacements += 1;
        } else {
            output.push_str(line);
        }
    }

    match replacements {
        1 => Ok(output),
        0 => Err(format!(
            "no `{needle}` line to stamp; {label} is already stamped or the placeholder changed"
        )),
        count => Err(format!(
            "found {count} `{needle}` lines; refusing to guess which one names the release"
        )),
    }
}

fn line_body(line: &str) -> &str {
    line.trim_end_matches(['\n', '\r'])
}

fn line_ending(line: &str) -> &str {
    &line[line_body(line).len()..]
}

fn backup_path(path: &Path) -> PathBuf {
    let mut backup = path.as_os_str().to_owned();
    backup.push(BACKUP_SUFFIX);
    PathBuf::from(backup)
}

fn read(path: &Path) -> Result<String, Error> {
    fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn write(path: &Path, contents: &str) -> Result<(), Error> {
    fs::write(path, contents).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    const MANIFEST: &str = "\
[package]
name = \"mono\"
version = \"0.0.0\"
edition = \"2024\"

[dependencies]
clap = { version = \"4.6.6\" }
xtask = { version = \"0.0.0\", path = \"xtask\" }
";

    const LOCKFILE: &str = "\
version = 4

[[package]]
name = \"mono\"
version = \"0.0.0\"
dependencies = [
 \"clap\",
]

[[package]]
name = \"xtask\"
version = \"0.0.0\"
dependencies = [
 \"mono\",
]
";

    const ZENSICAL: &str = "\
[project]
site_name = \"Mono v0.0.0\"
copyright = \"Mono v0.0.0 · Apache-2.0 License\"
";

    #[test]
    fn stamps_only_the_package_line() {
        let stamped = stamped_cargo_toml(MANIFEST, "1.2.3").unwrap();
        assert!(stamped.contains("version = \"1.2.3\"\n"));
        // The dependency keeps its placeholder: only the `[package]` line moved.
        assert!(stamped.contains("xtask = { version = \"0.0.0\", path = \"xtask\" }"));
        assert_eq!(stamped.matches("version = \"1.2.3\"").count(), 1);
    }

    #[test]
    fn stamps_only_the_mono_lock_entry() {
        let stamped = stamped_cargo_lock(LOCKFILE, "1.2.3").unwrap();
        assert_eq!(stamped.matches("version = \"1.2.3\"").count(), 1);
        assert!(stamped.contains("name = \"xtask\"\nversion = \"0.0.0\""));
    }

    #[test]
    fn stamps_the_zensical_header_and_footer() {
        let stamped = stamped_zensical_toml(ZENSICAL, "1.2.3").unwrap();
        assert_eq!(stamped.matches("Mono v1.2.3").count(), 2);
        assert!(!stamped.contains(ZENSICAL_PLACEHOLDER));
    }

    #[test]
    fn preserves_crlf_line_endings() {
        let manifest = MANIFEST.replace('\n', "\r\n");
        let stamped = stamped_cargo_toml(&manifest, "1.2.3").unwrap();
        assert!(stamped.contains("version = \"1.2.3\"\r\n"), "{stamped}");
        assert!(!stamped.contains("version = \"1.2.3\"\n\n"), "{stamped}");
    }

    #[test]
    fn rejects_an_already_stamped_manifest() {
        let stamped = stamped_cargo_toml(MANIFEST, "1.2.3").unwrap();
        let error = stamped_cargo_toml(&stamped, "1.2.4").unwrap_err();
        assert!(error.contains("already stamped"), "{error}");
    }

    #[test]
    fn rejects_the_placeholder_as_a_release_version() {
        let root = TempDir::new();
        fs::write(root.path().join(CARGO_TOML), MANIFEST).unwrap();
        fs::write(root.path().join(CARGO_LOCK), LOCKFILE).unwrap();
        fs::write(root.path().join(ZENSICAL_TOML), ZENSICAL).unwrap();

        let error = apply(root.path(), Version::parse(CARGO_PLACEHOLDER).unwrap()).unwrap_err();
        assert!(error.to_string().contains("placeholder"), "{error}");
        assert!(!backup_path(&root.path().join(CARGO_TOML)).exists());
    }

    #[test]
    fn rejects_an_ambiguous_manifest() {
        let doubled = MANIFEST.replace("[dependencies]", "version = \"0.0.0\"\n\n[dependencies]");
        let error = stamped_cargo_toml(&doubled, "1.2.3").unwrap_err();
        assert!(error.contains("found 2"), "{error}");
    }

    #[test]
    fn apply_and_restore_round_trip() {
        let root = TempDir::new();
        fs::write(root.path().join(CARGO_TOML), MANIFEST).unwrap();
        fs::write(root.path().join(CARGO_LOCK), LOCKFILE).unwrap();
        fs::write(root.path().join(ZENSICAL_TOML), ZENSICAL).unwrap();

        apply(root.path(), Version::parse("1.2.3").unwrap()).unwrap();
        assert!(
            fs::read_to_string(root.path().join(CARGO_TOML))
                .unwrap()
                .contains("version = \"1.2.3\"")
        );
        assert!(backup_path(&root.path().join(CARGO_TOML)).exists());

        restore(root.path()).unwrap();
        assert_eq!(
            fs::read_to_string(root.path().join(CARGO_TOML)).unwrap(),
            MANIFEST
        );
        assert_eq!(
            fs::read_to_string(root.path().join(CARGO_LOCK)).unwrap(),
            LOCKFILE
        );
        assert_eq!(
            fs::read_to_string(root.path().join(ZENSICAL_TOML)).unwrap(),
            ZENSICAL
        );
        assert!(!backup_path(&root.path().join(CARGO_LOCK)).exists());
    }

    #[test]
    fn apply_refuses_to_clobber_a_backup() {
        let root = TempDir::new();
        fs::write(root.path().join(CARGO_TOML), MANIFEST).unwrap();
        fs::write(root.path().join(CARGO_LOCK), LOCKFILE).unwrap();
        fs::write(root.path().join(ZENSICAL_TOML), ZENSICAL).unwrap();

        apply(root.path(), Version::parse("1.2.3").unwrap()).unwrap();
        let error = apply(root.path(), Version::parse("1.2.4").unwrap()).unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error}");

        // The first stamp is untouched by the rejected second one.
        assert_eq!(
            fs::read_to_string(root.path().join(CARGO_TOML))
                .unwrap()
                .matches("version = \"1.2.3\"")
                .count(),
            1
        );
    }

    #[test]
    fn a_failed_stamp_leaves_both_files_untouched() {
        let root = TempDir::new();
        // A lockfile without a `mono` entry cannot be stamped, so no file may
        // be modified.
        fs::write(root.path().join(CARGO_TOML), MANIFEST).unwrap();
        fs::write(root.path().join(CARGO_LOCK), "version = 4\n").unwrap();
        fs::write(root.path().join(ZENSICAL_TOML), ZENSICAL).unwrap();

        apply(root.path(), Version::parse("1.2.3").unwrap()).unwrap_err();

        assert_eq!(
            fs::read_to_string(root.path().join(CARGO_TOML)).unwrap(),
            MANIFEST
        );
        assert!(!backup_path(&root.path().join(CARGO_TOML)).exists());
    }

    #[test]
    fn restore_without_backups_is_a_no_op() {
        let root = TempDir::new();
        fs::write(root.path().join(CARGO_TOML), MANIFEST).unwrap();
        restore(root.path()).unwrap();
        assert_eq!(
            fs::read_to_string(root.path().join(CARGO_TOML)).unwrap(),
            MANIFEST
        );
    }

    /// A temp directory that removes itself; `mono::testing` is private to the
    /// library crate, so the xtask carries its own minimal copy.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                env::temp_dir().join(format!("mono-xtask-stamp-{}-{unique}", std::process::id()));
            fs::create_dir_all(&path).expect("create temp dir");
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
