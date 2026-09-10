# Standalone Projects and Invariant Guards Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Allow `monorelease` to operate on a single project with one root `monorepo.toml`, while preserving monorepo behavior and making broken internal assumptions fail fast.

**Architecture:** Keep `Workspace` as the execution-scope abstraction, but add an explicit `ScopeKind` so standalone mode is not inferred from package count. Standalone loading creates one package whose path is the root and whose tasks come from the root manifest; the existing graph, runner, scheduler, and cache operate on that normalized representation. User/configuration errors continue to return `Result` errors, while assertions guard states that are impossible after validation.

**Tech Stack:** Rust 2024, `serde`, TOML parsing, existing `Workspace`/`PlannedTask` graph planner, scheduler, runner, and CLI tests.

---

## Task 1: Add explicit root modes and standalone discovery

**Files:**
- Modify: `src/discovery.rs`
- Modify: `src/workspace.rs`
- Test: `src/discovery.rs` and `src/workspace.rs`

- [ ] **Step 1: Add a discovered-root mode.**

Add a private discovery type:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RootKind {
    Workspace,
    Standalone,
}

pub(crate) struct DiscoveredRoot {
    pub(crate) root: PathBuf,
    pub(crate) config: MonorepoConfig,
    pub(crate) kind: RootKind,
}
```

Change `find_root` to return `DiscoveredRoot` instead of `(PathBuf, MonorepoConfig)`.

- [ ] **Step 2: Make discovery prefer an ancestor workspace root.**

While walking from the starting directory toward `/`, remember the nearest valid standalone `[package]` manifest but continue searching for a `[workspace]` manifest. Return the workspace root if found; otherwise return the remembered standalone root.

Reject a manifest containing both `[workspace]` and `[package]` with `WorkspaceError::InvalidManifest`. Do not treat a manifest containing neither section as a root candidate.

The key behavior must be:

```text
repo/packages/app/src
  repo/packages/app/monorepo.toml -> standalone candidate
  repo/monorepo.toml             -> workspace root wins
```

- [ ] **Step 3: Add discovery tests.**

Cover:

```rust
#[test]
fn discovers_a_standalone_root_from_a_nested_directory() {
    let temp = TempDir::new();
    fs::create_dir_all(temp.path().join("src")).expect("create source directory");
    fs::write(
        config_path(temp.path()),
        "[package]\nname = \"app\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n",
    )
    .expect("write standalone manifest");

    let discovered = find_root(&temp.path().join("src")).expect("root is discovered");

    assert_eq!(discovered.kind, RootKind::Standalone);
    assert_eq!(discovered.root, fs::canonicalize(temp.path()).expect("root canonicalizes"));
}

#[test]
fn workspace_root_wins_over_a_nested_package_manifest() {
    let temp = TempDir::new();
    let package = temp.path().join("packages/app");
    fs::create_dir_all(package.join("src")).expect("create package source directory");
    fs::write(
        config_path(temp.path()),
        "[workspace]\nname = \"repo\"\nmembers = [\"packages/*\"]\n\n[pipelines.ci]\ntasks = [\"build\"]\n",
    )
    .expect("write workspace manifest");
    fs::write(
        config_path(&package),
        "[package]\nname = \"app\"\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n",
    )
    .expect("write package manifest");

    let discovered = find_root(&package.join("src")).expect("root is discovered");

    assert_eq!(discovered.kind, RootKind::Workspace);
    assert_eq!(discovered.root, fs::canonicalize(temp.path()).expect("root canonicalizes"));
}

#[test]
fn rejects_a_manifest_that_declares_both_root_modes() {
    let temp = TempDir::new();
    fs::write(
        config_path(temp.path()),
        "[workspace]\nname = \"repo\"\n\n[package]\nname = \"app\"\n",
    )
    .expect("write ambiguous manifest");

    let error = find_root(temp.path()).expect_err("ambiguous root must fail");

    assert!(error.to_string().contains("both [workspace] and [package]"));
}
```

The standalone fixture should contain `[package] name = "app"`, a `ci` pipeline, and one task.

- [ ] **Step 4: Run focused tests.**

Run:

```bash
cargo test discovery workspace
```

Expected: existing workspace tests pass; the new discovery tests pass.

---

## Task 2: Normalize standalone configuration into one package

**Files:**
- Modify: `src/workspace.rs`
- Modify: `src/config.rs` only if a standalone default constant is needed
- Test: `src/workspace.rs`

- [ ] **Step 1: Add `ScopeKind` to `Workspace`.**

Add a private field:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
    Workspace,
    Standalone,
}

pub struct Workspace {
    root: PathBuf,
    name: String,
    default_pipeline: String,
    pipelines: BTreeMap<String, PipelineConfig>,
    workspace_tasks: BTreeMap<String, TaskConfig>,
    packages: BTreeMap<String, Package>,
    kind: ScopeKind,
}
```

Use the discovered root kind when constructing it.

- [ ] **Step 2: Load standalone roots as singleton packages.**

In `Workspace::load`, branch after reading the discovered root:

```rust
match discovered.kind {
    RootKind::Workspace => load_workspace_root(...),
    RootKind::Standalone => load_standalone_root(...),
}
```

The standalone branch must:

1. Require `[package]`.
2. Reject `[workspace]`.
3. Validate the package name and root task map with `validate_package_config`.
4. Validate all pipeline names and task references.
5. Require at least one pipeline.
6. Use `ci` as the standalone default pipeline.
7. Require that the `ci` pipeline exists.
8. Insert exactly one package into `packages`:

```rust
let package = Package::from_config(root.clone(), package_config, root_config.tasks);
let package_name = package.name.clone();
let packages = BTreeMap::from([(package_name.clone(), package)]);
```

Set `workspace_tasks` to an empty map and set `name` to the package name.

Do not add a second standalone package discovery path.

- [ ] **Step 3: Preserve existing monorepo loading.**

The workspace branch must retain the existing behavior:

- `[workspace]` is required.
- root `[package]` is rejected.
- root tasks remain `workspace:*` tasks.
- members are discovered from member patterns.
- package manifests cannot contain pipelines or workspace settings.

- [ ] **Step 4: Add standalone planning tests.**

Add tests covering:

```rust
#[test]
fn loads_a_standalone_project_as_one_package() {
    let temp = TempDir::new();
    write_standalone_manifest(&temp);

    let scope = Workspace::load(temp.path()).expect("standalone scope loads");

    assert_eq!(scope.packages.len(), 1);
    assert_eq!(scope.packages["app"].path, fs::canonicalize(temp.path()).unwrap());
}

#[test]
fn plans_standalone_pipeline_tasks_in_dependency_order() {
    let temp = TempDir::new();
    write_standalone_manifest(&temp);

    let scope = Workspace::load(temp.path()).expect("standalone scope loads");
    let plan = scope.plan(None, None, &[]).expect("pipeline plans");

    assert_eq!(plan[0].node(), TaskNode { package: "app".into(), task: "build".into() });
    assert_eq!(plan[1].node(), TaskNode { package: "app".into(), task: "test".into() });
}

#[test]
fn standalone_root_tasks_are_package_tasks_not_workspace_tasks() {
    let temp = TempDir::new();
    write_standalone_manifest(&temp);

    let scope = Workspace::load(temp.path()).expect("standalone scope loads");
    let plan = scope.plan(None, None, &[]).expect("pipeline plans");

    assert!(plan.iter().all(|task| task.package() == "app"));
    assert!(scope.workspace_tasks.is_empty());
}

#[test]
fn standalone_tasks_can_use_a_root_relative_cwd() {
    let temp = TempDir::new();
    fs::create_dir(temp.path().join("src")).expect("create source directory");
    write_standalone_manifest_with_cwd(&temp);

    let scope = Workspace::load(temp.path()).expect("standalone scope loads");
    let plan = scope.plan(None, None, &[]).expect("pipeline plans");

    assert_eq!(plan[0].cwd(), &fs::canonicalize(temp.path().join("src")).unwrap());
}
```

The expected task nodes for a package named `app` must be `app:build`, not `workspace:build`.

- [ ] **Step 5: Run focused tests.**

Run:

```bash
cargo test workspace::tests
```

Expected: all existing monorepo tests and new standalone tests pass.

---

## Task 3: Guard validated workspace and planned-task invariants

**Files:**
- Modify: `src/workspace.rs`
- Test: `src/workspace.rs`

- [ ] **Step 1: Add a fail-fast workspace invariant guard.**

Add a private method called after constructing and validating a `Workspace`:

```rust
fn assert_invariants(&self) {
    assert!(self.root.is_absolute(), "workspace root must be absolute");
    assert!(self.root.is_dir(), "workspace root must be a directory");
    assert!(!self.name.is_empty(), "workspace name must not be empty");
    assert!(
        self.pipelines.contains_key(&self.default_pipeline),
        "default pipeline must exist"
    );

    for package in self.packages.values() {
        assert!(!package.name.is_empty(), "package name must not be empty");
        assert!(
            package.path.starts_with(&self.root),
            "package path must remain inside the workspace root"
        );
        assert!(package.path.is_absolute(), "package path must be absolute");
    }

    match self.kind {
        ScopeKind::Standalone => {
            assert_eq!(self.packages.len(), 1, "standalone scope must have one package");
            let package = self.packages.values().next().expect("singleton package exists");
            assert_eq!(package.path, self.root, "standalone package must be rooted at scope root");
            assert!(self.workspace_tasks.is_empty(), "standalone scope has no workspace tasks");
        }
        ScopeKind::Workspace => {
            assert!(
                self.packages.values().all(|package| package.name != WORKSPACE_PACKAGE_NAME),
                "workspace namespace cannot be a real package"
            );
        }
    }
}
```

This guard is for internal bugs. Invalid user input must still be rejected before this method with `WorkspaceError`.

- [ ] **Step 2: Assert planned-task assumptions.**

At the end of `planned_task`, assert:

```rust
assert!(!command.is_empty(), "planned task command must not be empty");
assert!(package_path.starts_with(&self.root));
assert!(cwd_path.starts_with(package_path));
assert!(cwd_path.is_dir());
assert!(timeout > Duration::ZERO);
```

Keep the existing validation errors for malformed manifests; these assertions guard only post-validation construction.

- [ ] **Step 3: Remove silent impossible-state fallbacks.**

In cycle detection, replace:

```rust
.position(|current| current == node)
.unwrap_or(0)
```

with:

```rust
.position(|current| current == node)
.expect("visiting task must be present on the DFS stack")
```

- [ ] **Step 4: Add graph assertions after planning.**

Before returning a plan, assert that each `TaskNode` appears once and that every dependency is present in the plan or was already validated as a reachable dependency. Do not replace user-facing missing-task errors with assertions.

- [ ] **Step 5: Run tests.**

Run:

```bash
cargo test workspace::tests config::tests
```

Expected: PASS.

---

## Task 4: Guard scheduler and runner invariants

**Files:**
- Modify: `src/scheduler.rs`
- Modify: `src/runner.rs`
- Test: `src/scheduler.rs` and `src/runner.rs`

- [ ] **Step 1: Assert scheduler input invariants.**

At the start of `execute_plan`, add:

```rust
assert!(jobs > 0, "scheduler requires at least one worker");
```

Build the task map explicitly so duplicate nodes panic instead of silently overwriting:

```rust
let mut tasks = BTreeMap::new();
for task in plan {
    let node = task.node();
    assert!(tasks.insert(node, task.clone()).is_none(), "plan contains duplicate task nodes");
}
```

- [ ] **Step 2: Assert scheduler bounds and accounting.**

After scheduling and after removing a task from `active`, assert:

```rust
assert!(active.len() <= jobs, "scheduler exceeded worker limit");
```

When a resource group is removed, assert that it was active if the task was actually running. At the end, assert:

```rust
assert_eq!(
    summary.completed + summary.cached + summary.failed + summary.blocked,
    plan.len(),
    "scheduler result accounting must cover the entire plan"
);
```

Keep `UnresolvedDependency` as a returned `SchedulerError`, since it represents an invalid scheduler input rather than an impossible internal state at that boundary.

- [ ] **Step 3: Guard runner assumptions.**

In `Runner::run`, retain the existing timeout and package-root assertions and add:

```rust
assert!(planned.cwd().is_absolute(), "planned cwd must be absolute");
assert!(
    planned.cwd().starts_with(planned.package_path()),
    "planned cwd must remain inside the package"
);
```

- [ ] **Step 4: Add focused invariant tests.**

Add a scheduler unit test that constructs a duplicate-node plan only if the test module can access `PlannedTask`; otherwise test the public planner to prove duplicate roots are deduplicated and reserve the assertion for the internal construction boundary.

Add a runner test that confirms a valid standalone planned task runs with the project root as its package path.

- [ ] **Step 5: Run scheduler and runner tests.**

Run:

```bash
cargo test scheduler runner
```

Expected: PASS.

---

## Task 5: Add standalone CLI coverage

**Files:**
- Modify: `tests/cli.rs`
- Modify: `README.md`

- [ ] **Step 1: Add a standalone CLI fixture.**

Create a temporary project with:

```toml
[package]
name = "app"

[pipelines.ci]
tasks = ["build", "test"]

[tasks.build]
command = ["echo", "build"]

[tasks.test]
command = ["echo", "test"]
depends_on = ["build"]
```

- [ ] **Step 2: Test nested-directory invocation.**

Run `monore ci --dry-run` with the command current directory set to the fixture's `src/` directory. Assert that it succeeds and prints `app:build` before `app:test`.

- [ ] **Step 3: Test standalone commands.**

Cover:

```text
monore doctor
monore plan
monore graph
monore ci
monore cache clean
```

Assert that the output reports one package and no `workspace:*` task nodes.

- [ ] **Step 4: Document the standalone manifest.**

Add a README section explaining that standalone mode still uses a root `monorepo.toml`, but does not need:

```text
apps/*
packages/*
child monorepo.toml files
```

Document that `cwd = "src"` is optional task configuration, not a special standalone mode.

- [ ] **Step 5: Run the full test suite.**

Run:

```bash
cargo test
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: all commands pass.

---

## Task 6: Decide and implement standalone initialization

**Files:**
- Modify: `src/config.rs`
- Modify: `src/commands/init.rs`
- Modify: `src/main.rs`
- Modify: `tests/cli.rs`
- Modify: `README.md`

- [ ] **Step 1: Add a standalone init option.**

Add a CLI flag:

```rust
Init {
    #[arg(long)]
    standalone: bool,
    #[arg(default_value = ".")]
    path: PathBuf,
}
```

Pass the selected mode to initialization.

- [ ] **Step 2: Generate a valid standalone scaffold.**

Add `MonorepoConfig::standalone_template(name: String, command: Vec<String>)` and make it write a root `[package]` manifest with one `ci` pipeline and one `build` task using the supplied command. Add a required repeated `--command` option to `init --standalone`; reject an empty command before writing. This keeps initialization language-agnostic while ensuring the generated manifest is immediately valid.

The initializer must not silently generate a monorepo member layout.

- [ ] **Step 3: Test standalone initialization.**

Run:

```bash
monore init --standalone /tmp/project
```

Assert that the resulting manifest has `[package]`, has no `[workspace]`, and does not create `apps/` or `packages/` directories.

- [ ] **Step 4: Run the full suite again.**

Run:

```bash
cargo test
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: PASS.
