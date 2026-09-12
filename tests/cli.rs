//! End-to-end checks of the CLI transport contract.

use std::fs;
use std::path::Path;
#[cfg(unix)]
use std::process::Command;
use std::process::Output;

mod support;

use support::{TempDir, mono};

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn write_project(root: &Path, body: &str) {
    fs::write(
        root.join("mono.toml"),
        format!("[project]\nname = \"fixture\"\n\n{body}"),
    )
    .expect("write project manifest");
}

#[test]
fn init_creates_a_valid_root_project() {
    let temp = TempDir::new("init");
    let output = mono(&["init"], temp.path());
    assert!(output.status.success(), "{}", stderr(&output));
    let manifest = fs::read_to_string(temp.path().join("mono.toml")).unwrap();
    assert!(manifest.contains("schema = 1"));
    assert!(manifest.contains("[project]"));
    assert!(!manifest.contains("[workspace]"));
    assert!(!manifest.contains("[package]"));
    assert!(mono(&["check"], temp.path()).status.success());
}

#[test]
fn init_has_no_standalone_mode() {
    let temp = TempDir::new("no-standalone");
    let output = mono(&["init", "--standalone"], temp.path());
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn reinitializing_refuses_to_overwrite() {
    let temp = TempDir::new("reinit");
    assert!(mono(&["init"], temp.path()).status.success());
    let output = mono(&["init"], temp.path());
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("refusing to overwrite"));
}

#[test]
fn nested_invocation_uses_the_ancestor_root() {
    let temp = TempDir::new("nested");
    fs::create_dir(temp.path().join("services")).unwrap();
    write_project(
        temp.path(),
        "[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"ok\"]\n",
    );
    let output = mono(&["plan"], &temp.path().join("services"));
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("would run build"));
}

#[test]
fn default_pipeline_runs_global_tasks_in_dependency_order() {
    let temp = TempDir::new("run");
    write_project(
        temp.path(),
        "[pipelines.ci]\ntasks = [\"app-build\"]\n\n[tasks.base-build]\ncommand = [\"echo\", \"base\"]\n\n[tasks.app-build]\ncommand = [\"echo\", \"app\"]\ndepends_on = [\"base-build\"]\n",
    );
    let output = mono(&["run", "--no-cache"], temp.path());
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).matches("summary: 2 completed").count(), 1);
    assert!(!stderr(&output).contains("summary:"));
    assert!(stderr(&output).contains("app-build"));
}

#[test]
fn list_describes_one_project_and_global_tasks() {
    let temp = TempDir::new("list");
    write_project(
        temp.path(),
        "[pipelines.ci]\ntasks = [\"test\"]\n\n[tasks.test]\ncommand = [\"echo\", \"testing\"]\n",
    );
    let output = mono(&["list"], temp.path());
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("project fixture"));
    assert!(stdout(&output).contains("test: echo testing"));
}

#[test]
fn unknown_task_suggests_the_closest_global_task() {
    let temp = TempDir::new("suggestion");
    write_project(
        temp.path(),
        "[pipelines.ci]\ntasks = [\"test\"]\n\n[tasks.test]\ncommand = [\"echo\", \"testing\"]\n",
    );
    let output = mono(&["task", "tests"], temp.path());
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("Did you mean 'test'?"));
}

#[test]
fn cacheable_tasks_cannot_inherit_standard_input() {
    let temp = TempDir::new("cache-stdin");
    write_project(
        temp.path(),
        "[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\ncache = true\ninputs = [\"input.txt\"]\nstdin = \"inherit\"\n",
    );
    fs::write(temp.path().join("input.txt"), "input").unwrap();
    let output = mono(&["check"], temp.path());
    assert!(!output.status.success());
    assert!(stderr(&output).contains("cannot inherit standard input"));
}

#[test]
fn plan_redacts_environment_values() {
    let temp = TempDir::new("env");
    write_project(
        temp.path(),
        "[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\nenv = { API_TOKEN = \"secret\", MODE = \"check\" }\n",
    );
    let output = mono(&["plan"], temp.path());
    assert!(output.status.success());
    let text = stdout(&output);
    assert!(text.contains("API_TOKEN=<redacted>"));
    assert!(!text.contains("secret"));
    assert!(!text.contains("check"));
}

#[test]
fn non_project_manifest_shape_is_rejected() {
    let temp = TempDir::new("old-shape");
    fs::write(
        temp.path().join("mono.toml"),
        "[workspace]\nname = \"old\"\n",
    )
    .unwrap();
    let output = mono(&["check"], temp.path());
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("could not parse"));
}

#[test]
fn legacy_package_selection_is_rejected_by_the_cli() {
    let temp = TempDir::new("legacy-selection-flag");
    assert!(mono(&["init"], temp.path()).status.success());
    let output = mono(&["run", "--package", "api"], temp.path());
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn plan_json_is_a_stable_machine_document() {
    let temp = TempDir::new("plan-json");
    write_project(
        temp.path(),
        "[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\nenv = { TOKEN = \"secret\" }\n",
    );
    let output = mono(&["--output", "json", "plan"], temp.path());
    assert!(output.status.success(), "{}", stderr(&output));
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["schema"], 1);
    assert_eq!(document["kind"], "plan");
    assert_eq!(document["tasks"][0]["id"], "build");
    assert_eq!(document["tasks"][0]["env"][0], "TOKEN");
    assert_eq!(document["tasks"][0]["stdin"], "null");
    assert!(document["tasks"][0].get("secret").is_none());

    let dry_run = mono(&["--output", "json", "run", "--dry-run"], temp.path());
    assert!(dry_run.status.success(), "{}", stderr(&dry_run));
    let dry_run_document: serde_json::Value = serde_json::from_slice(&dry_run.stdout).unwrap();
    assert_eq!(dry_run_document["kind"], "plan");
}

#[test]
fn list_json_contains_pipelines_and_tasks() {
    let temp = TempDir::new("list-json");
    write_project(
        temp.path(),
        "[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n",
    );
    let output = mono(&["--output", "json", "list"], temp.path());
    assert!(output.status.success(), "{}", stderr(&output));
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["schema"], 1);
    assert_eq!(document["kind"], "list");
    assert_eq!(document["pipelines"][0]["name"], "ci");
    assert_eq!(document["tasks"][0]["id"], "build");
}

#[test]
fn json_success_documents_cover_non_execution_commands() {
    let temp = TempDir::new("json-success");
    let init = mono(&["--output", "json", "init"], temp.path());
    assert!(init.status.success(), "{}", stderr(&init));
    let init_document: serde_json::Value = serde_json::from_slice(&init.stdout).unwrap();
    assert_eq!(init_document["schema"], 1);
    assert_eq!(init_document["kind"], "init");

    let clean = mono(&["--output", "json", "cache", "clean"], temp.path());
    assert!(clean.status.success(), "{}", stderr(&clean));
    let clean_document: serde_json::Value = serde_json::from_slice(&clean.stdout).unwrap();
    assert_eq!(clean_document["kind"], "cache_clean");
}

/// Values whose shape mono can judge on its own are refused while parsing, so
/// they name the flag and exit `2`. A value only the project can judge exits
/// `1` instead — `release_notes_reject_a_tag_that_is_not_the_newest_entry` pins
/// that half of the split.
#[test]
fn malformed_flag_values_are_usage_errors() {
    let temp = TempDir::new("usage-values");

    for args in [
        vec!["run", "--jobs", "0"],
        vec!["run", "--jobs", "many"],
        vec!["changelog", "prepare", "banana"],
        vec!["changelog", "prepare", "--date", "2001-13-45"],
        vec!["changelog", "prepare", "--date", "03/02/2001"],
        vec![
            "changelog",
            "prepare",
            "--pull-request-url",
            "https://example.test/pull/",
        ],
        vec!["changelog", "release-notes", "--release-tag", "banana"],
    ] {
        let output = mono(&args, temp.path());
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?} must be a usage error: {}",
            stderr(&output)
        );
    }
}

/// A run in JSON mode reports itself through events, so the transport has no
/// summary left to print. A blank line where the summary would have gone would
/// silently break a consumer parsing one document per line.
#[test]
fn a_json_run_emits_events_and_no_summary_line() {
    let temp = TempDir::new("run-json");
    write_project(
        temp.path(),
        "[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n",
    );

    let output = mono(&["--output", "json", "run", "--no-cache"], temp.path());
    assert!(output.status.success(), "{}", stderr(&output));

    let events = stdout(&output)
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("one document per line"))
        .collect::<Vec<_>>();
    assert_eq!(events.first().unwrap()["event"], "run_started");
    assert_eq!(events.last().unwrap()["event"], "run_finished");
    assert!(stderr(&output).is_empty(), "{}", stderr(&output));
}

#[test]
fn json_errors_are_documents_on_stdout() {
    let temp = TempDir::new("error-json");
    write_project(
        temp.path(),
        "[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n",
    );
    let output = mono(&["--output", "json", "task", "missing"], temp.path());
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stderr.is_empty());
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["schema"], 1);
    assert_eq!(document["kind"], "error");
    assert_eq!(document["code"], 1);
    assert!(document["message"].as_str().unwrap().contains("missing"));
}

/// A refactor once made `scheduler::classify` stop treating a cancelled task as
/// the reported error, so an interrupted run fell through to `Ok(summary)` and
/// exited `0`. Nothing failed, because no test covered the Ctrl-C path. This
/// pins it. The exit code is asserted as "not success" rather than `Some(1)` so
/// the test still means something if the signal lands before the handler is
/// installed and the process dies by signal (`code() == None`).
#[cfg(unix)]
#[test]
fn an_interrupted_run_does_not_report_success() {
    let temp = TempDir::new("cancel");
    write_project(
        temp.path(),
        "[pipelines.ci]\ntasks = [\"slow\"]\n\n[tasks.slow]\ncommand = [\"sleep\", \"30\"]\n",
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_mono"))
        .args(["ci", "--no-cache"])
        .current_dir(temp.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn mono");

    // Long enough for the Ctrl-C handler to be installed and the task to start.
    std::thread::sleep(std::time::Duration::from_millis(2000));
    let signaled = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("send SIGINT");
    assert!(signaled.success(), "could not signal mono");

    let status = child.wait().expect("wait for mono");

    assert_ne!(
        status.code(),
        Some(0),
        "an interrupted run reported success; a cancelled task must still be the \
         reported error in scheduler::classify"
    );
}

/// `std::env::vars()` panics when any ambient variable is not valid Unicode. The
/// cache session gathers the environment once, so calling `vars()` there aborts
/// the whole run because of an unrelated variable — `vars_os` plus a lossy
/// conversion does not. The variable is set on the child process rather than
/// with `set_var`, which is `unsafe` in Rust 2024 and racy under parallel tests.
#[cfg(unix)]
#[test]
fn a_non_unicode_environment_variable_does_not_abort_a_cached_run() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let temp = TempDir::new("non-unicode-env");
    write_project(
        temp.path(),
        "[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"sh\", \"-c\", \"cat seed > artifact\"]\ncache = true\ninputs = [\"seed\"]\noutputs = [\"artifact\"]\ncache_env = [\"*\"]\n",
    );
    fs::write(temp.path().join("seed"), "hello").unwrap();

    // Deliberately no `--no-cache`: the cache session only prepares when the
    // cache is enabled, and preparing is where the environment is gathered.
    let output = Command::new(env!("CARGO_BIN_EXE_mono"))
        .args(["ci"])
        .current_dir(temp.path())
        .env(
            "MONO_TEST_NON_UNICODE",
            OsString::from_vec(vec![0xff, 0xfe]),
        )
        .output()
        .expect("run mono");

    assert!(
        output.status.success(),
        "a non-UTF-8 environment variable aborted the run: {}",
        stderr(&output)
    );
}
