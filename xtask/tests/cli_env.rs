//! Regression: the xtask binary rejects environment-only RELEASE_TAG configuration.
//!
//! After the env-fallback removal, `verify` and `release-prepare` require an
//! explicit `--tag` even when `RELEASE_TAG` is set in the child environment.
//! These integration tests prove the old fallback is gone by setting the
//! variable and asserting Clap usage exit code 2.
//!
//! `release-stamp` is not covered here because its `--version` argument never
//! had a `RELEASE_TAG` environment fallback — the old codebase read `RELEASE_TAG`
//! only for commands that take `--tag`, not for the stamp version argument,
//! which already required an explicit positional or `--version` value
//! (or `--restore`).

use std::process::Command;

#[test]
fn verify_fails_with_env_only_release_tag() {
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("verify")
        .env("RELEASE_TAG", "v0.1.5")
        .output()
        .expect("xtask binary is available");

    assert_eq!(
        output.status.code(),
        Some(2),
        "verify without --tag must fail with Clap usage error 2; stdout: {}, stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--tag"),
        "error must mention --tag: {stderr}"
    );
}

#[test]
fn release_prepare_fails_with_env_only_release_tag() {
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("release-prepare")
        .env("RELEASE_TAG", "v0.1.5")
        .output()
        .expect("xtask binary is available");

    assert_eq!(
        output.status.code(),
        Some(2),
        "release-prepare without --tag must fail with Clap usage error 2; stdout: {}, stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--tag"),
        "error must mention --tag: {stderr}"
    );
}
