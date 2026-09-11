//! End-to-end checks of the transport contract `main` owns: what the binary
//! prints and which exit code it maps each expected failure to.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// A unique directory that deletes itself when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("mono-cli-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
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

fn mono(args: &[&str], cwd: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mono"))
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run the mono binary")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn init_succeeds_with_exit_code_zero() {
    let temp = TempDir::new("init");

    let output = mono(&["init"], temp.path());

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(temp.path().join("mono.toml").is_file());
    assert!(stdout(&output).starts_with("initialized"));
}

#[test]
fn standalone_init_creates_a_valid_single_package_project() {
    let temp = TempDir::new("standalone-init");

    let output = mono(
        &[
            "init",
            "--standalone",
            "--command",
            "echo",
            "--command",
            "build",
        ],
        temp.path(),
    );

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let manifest =
        fs::read_to_string(temp.path().join("mono.toml")).expect("standalone manifest is readable");
    assert!(manifest.contains("[package]"));
    assert!(!manifest.contains("[workspace]"));
    assert!(!temp.path().join("apps").exists());
    assert!(!temp.path().join("packages").exists());

    let doctor = mono(&["doctor"], temp.path());
    assert!(doctor.status.success(), "stderr: {}", stderr(&doctor));
}

#[test]
fn standalone_init_requires_a_command() {
    let temp = TempDir::new("standalone-init-missing-command");

    let output = mono(&["init", "--standalone"], temp.path());

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("requires a non-empty --command"));
    assert!(!temp.path().join("mono.toml").exists());
}

#[test]
fn cache_clean_removes_local_entries_without_removing_the_workspace() {
    let temp = TempDir::new("cache-clean");
    assert!(mono(&["init"], temp.path()).status.success());
    let cache_entry = temp.path().join(".mono").join("cache").join("entry");
    fs::create_dir_all(&cache_entry).expect("create cache entry");
    fs::write(cache_entry.join("metadata.json"), "cache").expect("write cache metadata");

    let output = mono(&["cache", "clean"], temp.path());

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(!temp.path().join(".mono/cache").exists());
    assert!(temp.path().join("mono.toml").is_file());
}

#[test]
fn reinitializing_fails_with_a_nonzero_exit_code() {
    let temp = TempDir::new("reinit");
    assert!(mono(&["init"], temp.path()).status.success());

    let output = mono(&["init"], temp.path());

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("refusing to overwrite"));
}

#[test]
fn doctor_fails_with_a_nonzero_exit_code_without_a_root_manifest() {
    let temp = TempDir::new("doctor-missing-root");

    let output = mono(&["doctor"], temp.path());

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("could not find a root mono.toml"));
}

#[test]
fn doctor_accepts_the_manifest_created_by_init() {
    let temp = TempDir::new("doctor-valid");
    assert!(mono(&["init"], temp.path()).status.success());

    let output = mono(&["doctor"], temp.path());

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains("checked"));
}

#[test]
fn default_command_runs_the_default_pipeline() {
    let temp = TempDir::new("default-command");
    fs::write(
        temp.path().join("mono.toml"),
        "[package]\nname = \"app\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"building\"]\n",
    )
    .expect("write standalone manifest");

    let output = mono(&[], temp.path());

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains("summary: 1 completed"));
}

#[test]
fn list_describes_available_tasks_and_pipelines() {
    let temp = TempDir::new("list");
    fs::write(
        temp.path().join("mono.toml"),
        "[package]\nname = \"app\"\n\n[pipelines.ci]\ntasks = [\"test\"]\n\n[tasks.test]\ncommand = [\"echo\", \"testing\"]\n",
    )
    .expect("write standalone manifest");

    let output = mono(&["list"], temp.path());

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let listing = stdout(&output);
    assert!(listing.contains("Pipelines:"));
    assert!(listing.contains("ci  test"));
    assert!(listing.contains("app:test"));
}

#[test]
fn unknown_task_suggests_the_closest_task_name() {
    let temp = TempDir::new("task-suggestion");
    fs::write(
        temp.path().join("mono.toml"),
        "[package]\nname = \"app\"\n\n[pipelines.ci]\ntasks = [\"test\"]\n\n[tasks.test]\ncommand = [\"echo\", \"testing\"]\n",
    )
    .expect("write standalone manifest");

    let output = mono(&["task", "tests"], temp.path());

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("Did you mean 'test'?"));
}

#[test]
fn standalone_project_runs_without_workspace_members() {
    let temp = TempDir::new("standalone");
    fs::create_dir_all(temp.path().join("src")).expect("create source directory");
    fs::write(
        temp.path().join("mono.toml"),
        "[package]\nname = \"app\"\n\n[pipelines.ci]\ntasks = [\"build\", \"test\"]\n\n[tasks.build]\ncommand = [\"echo\", \"building\"]\n\n[tasks.test]\ncommand = [\"echo\", \"testing\"]\ndepends_on = [\"build\"]\n",
    )
    .expect("write standalone manifest");

    let plan = mono(&["plan"], &temp.path().join("src"));

    assert!(plan.status.success(), "stderr: {}", stderr(&plan));
    let plan_output = stdout(&plan);
    assert!(plan_output.find("app:build").unwrap() < plan_output.find("app:test").unwrap());
    assert!(!plan_output.contains("workspace:build"));

    let run = mono(&["ci"], temp.path());
    assert!(run.status.success(), "stderr: {}", stderr(&run));
    assert!(stdout(&run).contains("summary: 2 completed"));
}

#[test]
fn ci_dry_run_discovers_a_package_manifest() {
    let temp = TempDir::new("ci-dry-run");
    assert!(mono(&["init"], temp.path()).status.success());

    let package = temp.path().join("packages").join("api");
    fs::create_dir_all(&package).expect("create package directory");
    fs::write(
        package.join("mono.toml"),
        "[package]\nname = \"api\"\n\n[tasks.build]\ncommand = [\"echo\", \"building-api\"]\n\n[tasks.test]\ncommand = [\"echo\", \"testing-api\"]\n",
    )
    .expect("write package manifest");

    let output = mono(&["ci", "--dry-run"], temp.path());

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains("would run api:build"));
    assert!(stdout(&output).contains("echo building-api"));
}

#[cfg(unix)]
#[test]
fn ci_reuses_cached_outputs_and_supports_cache_bypass_flags() {
    let temp = TempDir::new("ci-cache");
    fs::write(
        temp.path().join("mono.toml"),
        "[workspace]\nname = \"fixture\"\nmembers = [\"packages/*\"]\ndefault_pipeline = \"ci\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n",
    )
    .expect("write root manifest");
    let package = temp.path().join("packages").join("app");
    fs::create_dir_all(&package).expect("create package directory");
    fs::write(
        package.join("mono.toml"),
        "[package]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"sh\", \"-c\", \"n=$(cat count 2>/dev/null || echo 0); echo $((n + 1)) > count; cat seed > artifact\"]\ncache = true\ninputs = [\"seed\"]\noutputs = [\"artifact\"]\n",
    )
    .expect("write package manifest");
    fs::write(package.join("seed"), "hello").expect("write input");

    let first = mono(&["ci"], temp.path());
    assert!(first.status.success(), "stderr: {}", stderr(&first));
    let second = mono(&["ci"], temp.path());
    assert!(second.status.success(), "stderr: {}", stderr(&second));
    assert!(stderr(&second).contains("cache hit"));
    assert_eq!(
        fs::read_to_string(package.join("count")).expect("read count"),
        "1\n"
    );

    let forced = mono(&["ci", "--force"], temp.path());
    assert!(forced.status.success(), "stderr: {}", stderr(&forced));
    assert_eq!(
        fs::read_to_string(package.join("count")).expect("read count"),
        "2\n"
    );

    let without_cache = mono(&["ci", "--no-cache"], temp.path());
    assert!(
        without_cache.status.success(),
        "stderr: {}",
        stderr(&without_cache)
    );
    assert!(!stderr(&without_cache).contains("cache hit"));
    assert_eq!(
        fs::read_to_string(package.join("count")).expect("read count"),
        "3\n"
    );

    fs::remove_file(package.join("artifact")).expect("remove output");
    let restored = mono(&["ci"], temp.path());
    assert!(restored.status.success(), "stderr: {}", stderr(&restored));
    assert!(stderr(&restored).contains("cache hit"));
    assert_eq!(
        fs::read_to_string(package.join("artifact")).expect("read artifact"),
        "hello"
    );
}

#[test]
fn plan_includes_a_workspace_task_after_package_dependencies() {
    let temp = TempDir::new("workspace-task");
    fs::write(
        temp.path().join("mono.toml"),
        "[workspace]\nname = \"fixture\"\nmembers = [\"packages/*\"]\ndefault_pipeline = \"release\"\n\n[pipelines.release]\ntasks = [\"workspace:release-verify\"]\n\n[tasks.release-verify]\ncommand = [\"echo\", \"release\"]\ndepends_on = [\"api:package\"]\n",
    )
    .expect("write root manifest");
    let package = temp.path().join("packages").join("api");
    fs::create_dir_all(&package).expect("create package directory");
    fs::write(
        package.join("mono.toml"),
        "[package]\nname = \"api\"\n\n[tasks.package]\ncommand = [\"echo\", \"package\"]\n",
    )
    .expect("write package manifest");

    let output = mono(&["plan"], temp.path());

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let output = stdout(&output);
    assert!(output.find("api:package").unwrap() < output.find("workspace:release-verify").unwrap());
}

#[test]
fn doctor_rejects_a_missing_task_working_directory() {
    let temp = TempDir::new("doctor-missing-cwd");
    assert!(mono(&["init"], temp.path()).status.success());

    let package = temp.path().join("packages").join("api");
    fs::create_dir_all(&package).expect("create package directory");
    fs::write(
        package.join("mono.toml"),
        "[package]\nname = \"api\"\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\ncwd = \"missing\"\n\n[tasks.test]\ncommand = [\"echo\", \"test\"]\n",
    )
    .expect("write package manifest");

    let output = mono(&["doctor"], temp.path());

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("missing"));
}

#[cfg(unix)]
#[test]
fn ci_keeps_task_output_and_status_lines_separate() {
    let temp = TempDir::new("ci-output-boundaries");
    assert!(mono(&["init"], temp.path()).status.success());

    let package = temp.path().join("packages").join("api");
    fs::create_dir_all(&package).expect("create package directory");
    fs::write(
        package.join("mono.toml"),
        "[package]\nname = \"api\"\n\n[tasks.build]\ncommand = [\"sh\", \"-c\", \"printf task-output; printf task-error >&2\"]\n\n[tasks.test]\ncommand = [\"echo\", \"test\"]\n",
    )
    .expect("write package manifest");

    let output = mono(&["ci", "--task", "build"], temp.path());

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(
        stdout(&output)
            .contains("task-output\nsummary: 1 completed, 0 cached, 0 failed, 0 blocked")
    );
    assert!(stderr(&output).contains("▶ api:build"));
    assert!(stderr(&output).contains("task-error\napi:build: completed in "));
}

#[cfg(unix)]
#[test]
fn github_actions_output_is_explicit() {
    let temp = TempDir::new("github-actions-output");
    fs::write(
        temp.path().join("mono.toml"),
        "[package]\nname = \"app\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"building\"]\n",
    )
    .expect("write standalone manifest");

    let output = mono(&["ci", "--output", "github-actions"], temp.path());

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains("::group::app:build"));
    assert!(stdout(&output).contains("building"));
    assert!(stdout(&output).contains("::endgroup::"));
}

#[cfg(unix)]
#[test]
fn ci_reports_failed_task_output_and_context() {
    let temp = TempDir::new("ci-failure");
    assert!(mono(&["init"], temp.path()).status.success());

    let package = temp.path().join("packages").join("api");
    fs::create_dir_all(&package).expect("create package directory");
    fs::write(
        package.join("mono.toml"),
        "[package]\nname = \"api\"\n\n[tasks.build]\ncommand = [\"sh\", \"-c\", \"printf boom >&2; exit 3\"]\n\n[tasks.test]\ncommand = [\"sh\", \"-c\", \"printf should-not-run\"]\n",
    )
    .expect("write package manifest");

    // `--jobs 1` keeps this test about failure blocking: with the default
    // worker count the independent `api:test` is dispatched before `api:build`
    // reports its failure, so it would not be counted as blocked.
    let output = mono(&["ci", "--jobs", "1"], temp.path());

    // A task that exits non-zero is a failed request (1), not a failed tool (3).
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("boom"));
    assert!(stderr(&output).contains("api/build"));
    assert!(stderr(&output).contains("api:build: failed in "));
    assert!(stderr(&output).contains("summary: 0 completed, 0 cached, 1 failed, 1 blocked"));
    assert!(!stderr(&output).contains("should-not-run"));
}

#[cfg(unix)]
#[test]
fn ci_reports_task_start_progress() {
    let temp = TempDir::new("ci-progress");
    fs::write(
        temp.path().join("mono.toml"),
        "[workspace]\nname = \"fixture\"\nmembers = [\"packages/*\"]\ndefault_pipeline = \"ci\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n",
    )
    .expect("write root manifest");
    for name in ["a", "b"] {
        let package = temp.path().join("packages").join(name);
        fs::create_dir_all(&package).expect("create package directory");
        fs::write(
            package.join("mono.toml"),
            format!(
                "[package]\nname = \"{name}\"\n\n[tasks.build]\ncommand = [\"sh\", \"-c\", \"sleep 0.05; echo {name}\"]\n"
            ),
        )
        .expect("write package manifest");
    }

    let output = mono(&["ci", "--jobs", "2"], temp.path());

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(stderr(&output).contains("▶ a:build"));
    assert!(stderr(&output).contains("▶ b:build"));
    assert!(stderr(&output).contains("a:build: completed in "));
    assert!(stderr(&output).contains("b:build: completed in "));
}

#[cfg(unix)]
#[test]
fn jobs_defaults_to_machine_parallelism() {
    let temp = TempDir::new("jobs-default");

    let output = mono(&["run", "--help"], temp.path());

    let expected = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1);
    assert!(
        stdout(&output).contains(&format!("[default: {expected}]")),
        "expected the help to report the machine's parallelism: {}",
        stdout(&output)
    );
}

#[test]
fn plan_redacts_task_environment_values() {
    let temp = TempDir::new("plan-redacts-env");
    fs::write(
        temp.path().join("mono.toml"),
        "[workspace]\nname = \"fixture\"\nmembers = [\"packages/*\"]\ndefault_pipeline = \"ci\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n",
    )
    .expect("write root manifest");
    let package = temp.path().join("packages").join("app");
    fs::create_dir_all(&package).expect("create package directory");
    fs::write(
        package.join("mono.toml"),
        "[package]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\nenv = { API_TOKEN = \"super-secret\", MODE = \"check\" }\ninputs = [\"input.txt\"]\noutputs = [\"dist/**\"]\n",
    )
    .expect("write package manifest");

    let output = mono(&["plan"], temp.path());
    let output = stdout(&output);

    assert!(output.contains("[inputs=input.txt]"));
    assert!(output.contains("[outputs=dist/**]"));
    assert!(output.contains("[env API_TOKEN=<redacted>]"));
    assert!(output.contains("[env MODE=<redacted>]"));
    assert!(!output.contains("super-secret"));
    assert!(!output.contains("check"));
}

#[test]
fn plan_and_graph_commands_expose_task_dependencies() {
    let temp = TempDir::new("plan-graph");
    assert!(mono(&["init"], temp.path()).status.success());

    for (name, dependency) in [("shared", ""), ("web", "depends_on = [\"shared:build\"]\n")] {
        let package = temp.path().join("packages").join(name);
        fs::create_dir_all(&package).expect("create package directory");
        fs::write(
            package.join("mono.toml"),
            format!(
                "[package]\nname = \"{name}\"\n\n[tasks.build]\ncommand = [\"echo\", \"{name}\"]\n{dependency}\n[tasks.test]\ncommand = [\"echo\", \"{name}-test\"]\ndepends_on = [\"build\"]\n"
            ),
        )
        .expect("write package manifest");
    }

    let plan = mono(&["plan"], temp.path());
    assert!(plan.status.success(), "stderr: {}", stderr(&plan));
    assert!(stdout(&plan).find("shared:build").unwrap() < stdout(&plan).find("web:build").unwrap());

    let graph = mono(&["graph"], temp.path());
    assert!(graph.status.success(), "stderr: {}", stderr(&graph));
    assert!(stdout(&graph).contains("web:build <- shared:build"));
}

#[test]
fn a_consumer_that_hangs_up_is_not_a_failure() {
    let temp = TempDir::new("broken-pipe");
    assert!(mono(&["init"], temp.path()).status.success());

    let mut child = Command::new(env!("CARGO_BIN_EXE_mono"))
        .arg("list")
        .current_dir(temp.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mono");
    // Close the read end the way `mono list | head` would. The child is
    // still parsing and loading the workspace, so its write loses the race.
    drop(child.stdout.take());

    let output = child.wait_with_output().expect("wait for mono");

    assert!(
        output.status.success(),
        "a hung-up consumer must not be a failure: {output:?}"
    );
    assert!(
        !stderr(&output).contains("panicked"),
        "a hung-up consumer must not panic: {}",
        stderr(&output)
    );
}

#[test]
fn an_empty_argument_is_rejected_while_parsing() {
    let temp = TempDir::new("empty-argument");
    assert!(mono(&["init"], temp.path()).status.success());

    // Without edge validation `task ""` ran an empty plan and reported success:
    // a green build that did nothing.
    for args in [
        vec!["task", ""],
        vec!["run", "--task", ""],
        vec!["run", ""],
        vec!["plan", "--package", ""],
    ] {
        let output = mono(&args, temp.path());

        assert_eq!(output.status.code(), Some(2), "args: {args:?}");
    }
}

#[test]
fn an_unusable_worker_count_is_rejected_while_parsing() {
    let temp = TempDir::new("jobs-invalid");
    assert!(mono(&["init"], temp.path()).status.success());

    // The scheduler requires at least one worker, so the bound belongs on the
    // flag rather than surfacing as a failed run.
    for jobs in ["0", "many"] {
        let output = mono(&["run", "--jobs", jobs], temp.path());

        assert_eq!(output.status.code(), Some(2), "jobs: {jobs}");
    }
}

#[test]
fn a_filesystem_failure_exits_with_three() {
    let temp = TempDir::new("tool-failure");
    // The target is a file, so the workspace root cannot be created: the command
    // was understood and the environment refused to carry it out.
    let occupied = temp.path().join("occupied");
    fs::write(&occupied, "not a directory").expect("write occupying file");

    let output = mono(&["--dir", occupied.to_str().unwrap(), "init"], temp.path());

    assert_eq!(output.status.code(), Some(3));
    assert!(stderr(&output).contains("could not create directory"));
}
