//! End-to-end checks for the provider-neutral changelog and release commands.

use std::fs;

mod support;

use support::{TempDir, mono, mono_with_env};

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
