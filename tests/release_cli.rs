//! End-to-end checks for the provider-neutral changelog and release commands.

use std::fs;
use std::process::Command;

mod support;

use support::{TempDir, mono, mono_with_env};

fn git(temp: &TempDir, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(temp.path())
        .output()
        .expect("git is installed");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
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
            "--output-file",
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
fn clean_changelog_workflow_infers_versions_and_uses_the_top_entry() {
    let temp = TempDir::new("clean-changelog");
    fs::write(
        temp.path().join("CHANGELOG.md"),
        "# Changelog\n\n## 1.0.0\nReleased: 2026-09-11\n\n- Shipped.\n",
    )
    .unwrap();

    let prepare = mono(
        &["changelog", "prepare", "--date", "2001-02-03"],
        temp.path(),
    );
    assert!(prepare.status.success(), "prepare failed: {prepare:?}");
    let changelog = fs::read_to_string(temp.path().join("CHANGELOG.md")).unwrap();
    assert!(changelog.contains("## 1.0.1\n"));
    assert!(changelog.contains("Released: 2001-02-03\n"));

    let notes = mono(&["changelog", "release-notes"], temp.path());
    assert!(notes.status.success(), "notes failed: {notes:?}");
    assert!(
        fs::read_to_string(temp.path().join("RELEASE_NOTES.md"))
            .unwrap()
            .contains("-\n")
    );
}

#[test]
fn prepare_can_seed_editable_bullets_from_a_git_ref_range() {
    let temp = TempDir::new("changelog-git");
    fs::write(
        temp.path().join("CHANGELOG.md"),
        "# Changelog\n\n## 1.0.0\nReleased: 2026-09-11\n",
    )
    .unwrap();

    git(&temp, &["init"]);
    git(&temp, &["config", "user.email", "test@example.com"]);
    git(&temp, &["config", "user.name", "Mono Test"]);
    git(&temp, &["add", "CHANGELOG.md"]);
    git(&temp, &["commit", "-m", "base"]);
    git(&temp, &["branch", "-M", "main"]);
    git(&temp, &["checkout", "-b", "feature"]);
    fs::write(temp.path().join("feature.txt"), "feature").unwrap();
    git(&temp, &["add", "feature.txt"]);
    git(&temp, &["commit", "-m", "feature"]);
    git(&temp, &["checkout", "main"]);
    git(
        &temp,
        &[
            "merge",
            "--no-ff",
            "feature",
            "-m",
            "Merge pull request #42 from test/feature\n\nAdd the feature",
        ],
    );

    let prepare = mono(
        &[
            "changelog",
            "prepare",
            "--from",
            "HEAD~1",
            "--to",
            "HEAD",
            "--pull-request-url",
            "https://github.com/example/project/pull/{number}",
            "--date",
            "2001-02-03",
        ],
        temp.path(),
    );
    assert!(prepare.status.success(), "prepare failed: {prepare:?}");
    let changelog = fs::read_to_string(temp.path().join("CHANGELOG.md")).unwrap();
    assert!(
        changelog.contains("[#42](https://github.com/example/project/pull/42)"),
        "{changelog}"
    );
    assert!(changelog.contains("Add the feature"), "{changelog}");
}

#[test]
fn release_notes_reject_a_tag_that_is_not_the_newest_entry() {
    let temp = TempDir::new("changelog-tag-mismatch");
    fs::write(
        temp.path().join("CHANGELOG.md"),
        "# Changelog\n\n## 1.1.0\nReleased: 2026-09-12\n\n- New.\n\n## 1.0.0\nReleased: 2026-09-11\n\n- Old.\n",
    )
    .unwrap();

    let notes = mono_with_env(
        &["changelog", "release-notes"],
        temp.path(),
        &[("RELEASE_TAG", "v1.0.0")],
    );

    assert_eq!(notes.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&notes.stderr).contains("newest changelog entry"));
    assert!(!temp.path().join("RELEASE_NOTES.md").exists());
}

#[test]
fn changelog_help_hides_execution_only_ui_flags() {
    let temp = TempDir::new("changelog-help");
    let help = mono(&["changelog", "--help"], temp.path());

    assert!(help.status.success());
    assert!(!String::from_utf8_lossy(&help.stdout).contains("--ui"));
    assert!(String::from_utf8_lossy(&help.stdout).contains("release-notes"));
}

#[test]
fn release_commands_create_and_verify_file_artifacts() {
    let temp = TempDir::new("release");
    fs::create_dir_all(temp.path().join("dist/nested")).unwrap();
    fs::write(temp.path().join("dist/app.tar.gz"), b"app").unwrap();
    fs::write(temp.path().join("dist/nested/license"), b"license").unwrap();

    let manifest = mono(
        &[
            "release", "manifest", "--dist", "dist", "--tag", "v1.0.0", "--commit", "abc123",
        ],
        temp.path(),
    );
    assert!(manifest.status.success(), "manifest failed: {manifest:?}");
    assert!(temp.path().join("dist/BUILD-METADATA.json").is_file());
    assert!(temp.path().join("dist/SHA256SUMS").is_file());

    let verify = mono(
        &[
            "release", "verify", "--dist", "dist", "--tag", "v1.0.0", "--commit", "abc123",
        ],
        temp.path(),
    );
    assert!(verify.status.success(), "verify failed: {verify:?}");
}

/// `release.yml` exports `RELEASE_TAG_OBJECT` unconditionally, so a lightweight
/// tag reaches the CLI as the empty string. That has to mean "no tag object",
/// not a tag object that happens to be empty.
#[test]
fn release_commands_enforce_an_expected_artifact_inventory() {
    let temp = TempDir::new("expected-inventory");
    fs::create_dir_all(temp.path().join("dist")).unwrap();
    fs::write(temp.path().join("dist/app.tar.gz"), b"app").unwrap();
    fs::write(temp.path().join("dist/extra.txt"), b"extra").unwrap();
    fs::write(temp.path().join("expected.txt"), "app.tar.gz\n").unwrap();

    let manifest = mono(
        &[
            "release",
            "manifest",
            "--dist",
            "dist",
            "--expected",
            "expected.txt",
            "--tag",
            "v1.0.0",
        ],
        temp.path(),
    );
    assert!(
        !manifest.status.success(),
        "unexpected artifact passed inventory"
    );

    fs::write(temp.path().join("expected.txt"), "app.tar.gz\nextra.txt\n").unwrap();
    let manifest = mono(
        &[
            "release",
            "manifest",
            "--dist",
            "dist",
            "--expected",
            "expected.txt",
            "--tag",
            "v1.0.0",
        ],
        temp.path(),
    );
    assert!(
        manifest.status.success(),
        "expected inventory failed: {manifest:?}"
    );

    let verify = mono(
        &[
            "release",
            "verify",
            "--dist",
            "dist",
            "--expected",
            "expected.txt",
            "--tag",
            "v1.0.0",
        ],
        temp.path(),
    );
    assert!(
        verify.status.success(),
        "expected verification failed: {verify:?}"
    );
}

#[test]
fn a_lightweight_tag_records_no_tag_object() {
    let temp = TempDir::new("lightweight-tag");
    fs::create_dir_all(temp.path().join("dist")).unwrap();
    fs::write(temp.path().join("dist/artifact.tar.gz"), b"artifact").unwrap();

    let manifest = mono_with_env(
        &["release", "manifest", "--dist", "dist", "--tag", "v1.0.0"],
        temp.path(),
        &[("RELEASE_TAG_OBJECT", "")],
    );
    assert!(manifest.status.success(), "manifest failed: {manifest:?}");

    let written: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(temp.path().join("dist/BUILD-METADATA.json")).unwrap(),
    )
    .unwrap();
    assert!(
        written.get("tag_object").is_none(),
        "a lightweight tag must not be recorded as an empty tag object: {written}"
    );

    // The validator compares the field whenever it is given one, so an absent
    // field against an expected object is a mismatch, not a silent pass.
    let verify = mono(
        &[
            "release",
            "verify",
            "--dist",
            "dist",
            "--tag",
            "v1.0.0",
            "--tag-object",
            "def456",
        ],
        temp.path(),
    );
    assert!(!verify.status.success(), "verify unexpectedly passed");
    assert!(
        String::from_utf8_lossy(&verify.stderr).contains("tag object"),
        "unexpected error: {verify:?}"
    );
}

#[test]
fn an_annotated_tag_object_round_trips_through_the_cli() {
    let temp = TempDir::new("annotated-tag");
    fs::create_dir_all(temp.path().join("dist")).unwrap();
    fs::write(temp.path().join("dist/artifact.tar.gz"), b"artifact").unwrap();

    let manifest = mono(
        &[
            "release",
            "manifest",
            "--dist",
            "dist",
            "--tag",
            "v1.0.0",
            "--tag-object",
            "def456",
        ],
        temp.path(),
    );
    assert!(manifest.status.success(), "manifest failed: {manifest:?}");

    let verify = mono(
        &[
            "release",
            "verify",
            "--dist",
            "dist",
            "--tag",
            "v1.0.0",
            "--tag-object",
            "def456",
        ],
        temp.path(),
    );
    assert!(verify.status.success(), "verify failed: {verify:?}");
}
