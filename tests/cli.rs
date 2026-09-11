//! End-to-end checks of the CLI transport contract.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
        .expect("run mono")
}

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

#[cfg(unix)]
#[test]
fn cache_reuses_root_task_outputs_and_supports_bypass() {
    let temp = TempDir::new("cache");
    write_project(
        temp.path(),
        "[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"sh\", \"-c\", \"n=$(cat count 2>/dev/null || echo 0); echo $((n + 1)) > count; cat seed > artifact\"]\ncache = true\ninputs = [\"seed\"]\noutputs = [\"artifact\"]\n",
    );
    fs::write(temp.path().join("seed"), "hello").unwrap();
    assert!(mono(&["ci"], temp.path()).status.success());
    let second = mono(&["ci"], temp.path());
    assert!(second.status.success(), "{}", stderr(&second));
    assert!(stderr(&second).contains("cache hit"));
    assert_eq!(
        fs::read_to_string(temp.path().join("count")).unwrap(),
        "1\n"
    );
    assert!(mono(&["ci", "--force"], temp.path()).status.success());
    assert_eq!(
        fs::read_to_string(temp.path().join("count")).unwrap(),
        "2\n"
    );
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

#[cfg(unix)]
#[test]
fn json_output_contains_lifecycle_events() {
    let temp = TempDir::new("json");
    write_project(
        temp.path(),
        "[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"hello\"]\n",
    );
    let output = mono(&["ci", "--output", "json"], temp.path());
    assert!(output.status.success(), "{}", stderr(&output));
    let output_text = stdout(&output);
    let lines = output_text.lines().collect::<Vec<_>>();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("\"event\":\"task_started\""))
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("\"event\":\"task_finished\""))
    );
    assert!(
        lines
            .iter()
            .all(|line| serde_json::from_str::<serde_json::Value>(line).is_ok())
    );
    let events = lines
        .iter()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(events.iter().all(|event| event["run_id"].is_u64()));
    let sequences = events
        .iter()
        .map(|event| event["sequence"].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn old_workspace_shape_is_rejected() {
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
    assert!(document["tasks"][0].get("secret").is_none());
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
