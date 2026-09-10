//! End-to-end checks of the transport contract `main` owns: what the binary
//! prints and which exit code it maps each expected failure to.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A unique directory that deletes itself when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("monorelease-cli-{}-{name}", std::process::id()));
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

fn monore(args: &[&str], cwd: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_monorelease"))
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run the monore binary")
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

    let output = monore(&["init"], temp.path());

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(temp.path().join("monorepo.toml").is_file());
    assert!(stdout(&output).starts_with("initialized"));
}

#[test]
fn reinitializing_fails_with_a_nonzero_exit_code() {
    let temp = TempDir::new("reinit");
    assert!(monore(&["init"], temp.path()).status.success());

    let output = monore(&["init"], temp.path());

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("refusing to overwrite"));
}

#[test]
fn doctor_fails_with_a_nonzero_exit_code_without_a_root_manifest() {
    let temp = TempDir::new("doctor-missing-root");

    let output = monore(&["doctor"], temp.path());

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("could not find a root monorepo.toml"));
}

#[test]
fn doctor_accepts_the_manifest_created_by_init() {
    let temp = TempDir::new("doctor-valid");
    assert!(monore(&["init"], temp.path()).status.success());

    let output = monore(&["doctor"], temp.path());

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains("checked"));
}

#[test]
fn ci_dry_run_discovers_a_package_manifest() {
    let temp = TempDir::new("ci-dry-run");
    assert!(monore(&["init"], temp.path()).status.success());

    let package = temp.path().join("packages").join("api");
    fs::create_dir_all(&package).expect("create package directory");
    fs::write(
        package.join("monorepo.toml"),
        "[package]\nname = \"api\"\n\n[tasks.build]\ncommand = [\"echo\", \"building-api\"]\n\n[tasks.test]\ncommand = [\"echo\", \"testing-api\"]\n",
    )
    .expect("write package manifest");

    let output = monore(&["ci", "--dry-run"], temp.path());

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains("would run api:build"));
    assert!(stdout(&output).contains("echo building-api"));
}

#[test]
fn plan_and_graph_commands_expose_task_dependencies() {
    let temp = TempDir::new("plan-graph");
    assert!(monore(&["init"], temp.path()).status.success());

    for (name, dependency) in [("shared", ""), ("web", "depends_on = [\"shared:build\"]\n")] {
        let package = temp.path().join("packages").join(name);
        fs::create_dir_all(&package).expect("create package directory");
        fs::write(
            package.join("monorepo.toml"),
            format!(
                "[package]\nname = \"{name}\"\n\n[tasks.build]\ncommand = [\"echo\", \"{name}\"]\n{dependency}\n[tasks.test]\ncommand = [\"echo\", \"{name}-test\"]\ndepends_on = [\"build\"]\n"
            ),
        )
        .expect("write package manifest");
    }

    let plan = monore(&["plan"], temp.path());
    assert!(plan.status.success(), "stderr: {}", stderr(&plan));
    assert!(stdout(&plan).find("shared:build").unwrap() < stdout(&plan).find("web:build").unwrap());

    let graph = monore(&["graph"], temp.path());
    assert!(graph.status.success(), "stderr: {}", stderr(&graph));
    assert!(stdout(&graph).contains("web:build <- shared:build"));
}
