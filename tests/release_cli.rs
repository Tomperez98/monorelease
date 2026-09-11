//! End-to-end checks for the provider-neutral changelog and release commands.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("mono-release-cli-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temp directory");
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

fn mono(args: &[&str], cwd: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mono"))
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run mono")
}

#[test]
fn changelog_commands_validate_scaffold_and_render_notes() {
    let temp = TempDir::new("changelog");
    fs::write(
        temp.path().join("CHANGELOG.md"),
        "# Changelog\n\n## 1.0.0\nReleased: 2026-09-11\n\n- Shipped.\n",
    )
    .unwrap();

    let validate = mono(&["changelog", "validate"], temp.path());
    assert!(validate.status.success(), "validate failed: {validate:?}");

    let notes = mono(
        &[
            "changelog",
            "notes",
            "--version",
            "v1.0.0",
            "--output",
            "notes.md",
        ],
        temp.path(),
    );
    assert!(notes.status.success(), "notes failed: {notes:?}");
    assert!(
        fs::read_to_string(temp.path().join("notes.md"))
            .unwrap()
            .contains("- Shipped.")
    );

    let scaffold = mono(
        &["changelog", "scaffold", "--version", "1.1.0"],
        temp.path(),
    );
    assert!(scaffold.status.success(), "scaffold failed: {scaffold:?}");
    assert!(
        fs::read_to_string(temp.path().join("CHANGELOG.md"))
            .unwrap()
            .contains("## 1.1.0")
    );
}

#[test]
fn release_commands_create_and_verify_file_artifacts() {
    let temp = TempDir::new("release");
    fs::create_dir_all(temp.path().join("dist/nested")).unwrap();
    fs::write(temp.path().join("dist/app.tar.gz"), b"app").unwrap();
    fs::write(temp.path().join("dist/nested/license"), b"license").unwrap();

    let manifest = mono(
        &[
            "release",
            "manifest",
            "--directory",
            "dist",
            "--tag",
            "v1.0.0",
            "--commit",
            "abc123",
        ],
        temp.path(),
    );
    assert!(manifest.status.success(), "manifest failed: {manifest:?}");
    assert!(temp.path().join("dist/BUILD-METADATA.json").is_file());
    assert!(temp.path().join("dist/SHA256SUMS").is_file());

    let verify = mono(
        &[
            "release",
            "verify",
            "--directory",
            "dist",
            "--tag",
            "v1.0.0",
            "--commit",
            "abc123",
        ],
        temp.path(),
    );
    assert!(verify.status.success(), "verify failed: {verify:?}");
}
