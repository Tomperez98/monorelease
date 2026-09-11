# Testability and Code Improvements Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give `mono`'s two most complex modules (`project.rs`, `scheduler.rs`) direct tests, replace process-global state with injected state at the three places it hurts testability, and remove the duplicated/overloaded code those seams expose.

**Architecture:** Keep the existing shape — `main.rs` is the only transport edge, each command owns one error vocabulary, and `lib.rs` composes them. Every change below either (a) adds tests to pure functions that are already there, (b) replaces a process-global (`stdout`, `env::vars`, `Command::new("git")`) with a parameter, or (c) deletes duplication. No new dependencies. No behavior change except Task 7, which makes exit codes exact.

**Tech Stack:** Rust 2024, Clap, Serde/JSON, TOML, `sha2`, platform process APIs, `ctrlc`.

---

## Analysis

Baseline is green: `cargo test --workspace --all-targets --all-features` passes (66 tests), `cargo clippy -- -D warnings` is clean, `cargo fmt --check` is clean. This plan is about quality, not breakage.

### What is already right (do not touch)

- **Parse at the edge.** `main.rs` validates every argument in `clap`; the library only sees well-formed commands (`parse_jobs`, `NonEmptyStringValueParser`, `conflicts_with`).
- **One error vocabulary per boundary.** Each command owns an error enum; `lib::Error` unions them; `main.rs::exit_code` is the single transport mapping.
- **Fail-fast invariants.** `cache_mode`'s assert, `Runner::run_with_options`'s four asserts, `Project::assert_invariants`, `execute_plan`'s worker-limit and accounting asserts, `matrix_size > 1024` bound, `validate_cache_pattern`/`valid_relative_path` rejecting `..`.
- **Determinism where it counts.** `format_plan`, `format_command`, the `BTree*` collections, and the ordered pattern matcher are all deterministic and already covered.

### Findings, worst first

| # | Violation (REFACTOR.md checklist) | Where | Why it matters |
| --- | --- | --- | --- |
| F1 | **Zero tests on the domain core** | `src/project.rs` (1079 lines) — no `mod tests` | 12 `ProjectError` variants, DFS cycle detection, matrix expansion, interpolation, edit-distance suggestions: all untested directly. Ten pure helpers sit there with no test at all. |
| F2 | **Zero tests on the concurrency core** | `src/scheduler.rs` (584 lines) — no `mod tests` | Finalizer gating, resource-group serialization, blocked/cancelled accounting, first-error precedence: only reachable through a subprocess or a real `sh -c`. The loop's asserts can never fire in a test. |
| F3 | **IO interleaved with logic / no seam** | `src/output.rs` | The renderer is the largest surface (4 modes × 7 events) and writes straight to `io::stdout()`/`io::stderr()`. Tests can only reach JSON mode, and only through a `#[cfg(test)] json_lines` field that makes the test struct literal differ from production construction. Terminal, GitHub Actions, and Live rendering have **no** test. |
| F4 | **Global state read deep inside** | `cache.rs::task_key_with_session` reads `std::env::vars()` / `std::env::var` | The cache key depends on process environment read from the middle of a hash computation. In Rust 2024 `std::env::set_var` is `unsafe`, so a test cannot influence this safely. There is no `cache_env` test at all. |
| F5 | **No seam** | `release.rs::verify_source` calls `Command::new("git")` directly | The one function in `release.rs` with zero tests, because it needs a real git repo. |
| F6 | **Overloaded error variant** | `ProjectError::Io` | `main.rs::exit_code` carries a comment admitting the exit code cannot distinguish "cannot read the manifest" (tool failure, 3) from "a declared `cwd` is missing" (request failure, 1). Two different failure kinds share one variant, so the exit-code contract is wrong for one of them. |
| F7 | **Duplicated logic** | `release.rs::write_file`/`replace_file`, `commands/changelog.rs::write`/`replace_file`, `commands/init.rs::write_config` | Three copies of same-directory-temp + `sync_all` + replace, with **divergent** Windows fallbacks. Three places to get wrong. |
| F8 | **Non-determinism in a command** | `commands/changelog.rs::scaffold` calls `today()` | The scaffold date comes from the wall clock inside the command, so a test can never assert the rendered date. |
| F9 | **Hand-built JSON** | `main.rs::run_check` uses `format!`; `success_document`/`emit_error` use `serde_json::json!` | The versioned machine contract is assembled three different ways while `plan`/`list` use typed structs. |
| F10 | **Dead code and dead parameters** | `output.rs::write_status`, `output.rs::present_summary`, `process.rs::ManagedChild::child`, `runner.rs::Runner::run`, `cache.rs::prepare(_plan)`, `cache.rs::task_key_with_session(_project_root)` | Four `#[allow(dead_code)]` markers and two parameters that exist only to be ignored. |

### Deferred, with reasons

- **Splitting `project.rs`/`cache.rs` by responsibility** — real, but mechanical churn; it is Task 12, last, and each split stands alone.
- **A `Git` trait object instead of a closure** — Task 6 uses the narrowest seam that makes the failure space testable (one `Fn(&[&str])`); a trait would add indirection for one call site (WRITE.md #10).
- **Extracting a full `SchedulerState` struct from `execute_plan`** — Task 4 extracts the two pure *decisions* (`next_ready`, `finalizers_allowed`) and tests them. Restructuring the channel/worker driver is a larger change with no new tests to show for it; do it only if Task 4 leaves pain.
- **`SchedulerError::UnresolvedDependency`/`NoReadyWork` are arguably broken invariants that should panic.** They are public variants in the exported `SchedulerError`, so converting them is a breaking API change for a pre-1.0 crate. Not in this plan.
- **The `O(n)` `finalizers_allowed` scan per dispatch.** Real control work in the scheduling loop, but n is the task count (hundreds at most) and there is no profile showing it. Leave it.

### Sequencing

Tasks 1–3 add tests to things that already exist and change no behavior — do them first, they are pure upside. Tasks 4–5 add the missing vocabulary mapping and make the two scheduler decisions testable. Tasks 6–7 add the two remaining seams. Tasks 8–11 are contract fixes. Tasks 12–13 are cleanup and are safe to defer.

---

## Conventions

- **Branch:** create one branch per task, named `testability/<task-slug>`, unless a task says otherwise.
- **TDD loop:** write the failing test, run it and read the failure, make it pass, run the focused tests plus `cargo test --workspace --all-targets --all-features`.
- **Commits:** this repository is a GitButler workspace, so use `but`, never `git add`/`git commit`. For each commit step: run `but diff`, copy the file/hunk IDs, then `but commit -b <branch> -m "<message>" <ids>`.
- **Focused test command shape:** `cargo test --lib project::tests::` (unit tests in `src/*.rs`) or `cargo test --test cli` (integration tests in `tests/`).
- **rustfmt owns the final layout.** The code blocks below were written for clarity and are **not** guaranteed to be rustfmt-stable. After transcribing any block, run `cargo fmt` (not just `--check`) and accept rustfmt's reflowing. Every task's validation includes `cargo fmt --check`, which CI enforces through the manifest's `fmt` task (`command = ["cargo", "fmt", "--check"]`, `mono.toml:17-19`). Do not hand-fight rustfmt, and do not treat a passing test run as sufficient validation.
- **Do not** add dependencies. `serde`/`serde_json`/`toml`/`sha2`/`clap`/`ctrlc`/`libc`/`windows-sys` are the whole set.

---

## Phase A — Test what already exists (no behavior change)

### Task 1: Unit-test `project.rs`'s pure helpers

Ten private helpers in `src/project.rs` do all the parsing, expansion, interpolation, matching, and suggestion work, and none of them is tested. They are pure functions over owned data, so this task needs no filesystem and no subprocess.

**Files:**
- Modify: `src/project.rs` (append a `#[cfg(test)] mod tests` at the end of the file)

- [ ] **Step 1: Write the failing tests**

Append to `src/project.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_without_dimensions_is_its_own_base() {
        assert_eq!(base_task_name("build"), Ok("build"));
    }

    #[test]
    fn a_matrix_instance_base_is_the_name_before_the_bracket() {
        assert_eq!(base_task_name("build[os=linux]"), Ok("build"));
        assert_eq!(base_task_name("build[os=linux,arch=arm]"), Ok("build"));
    }

    #[test]
    fn an_unterminated_or_leading_bracket_is_not_a_task_name() {
        assert_eq!(base_task_name("build[os=linux"), Err(()));
        assert_eq!(base_task_name("[os=linux]"), Err(()));
    }

    #[test]
    fn a_plain_task_name_has_no_dimensions() {
        assert!(task_dimensions("build").unwrap().is_empty());
    }

    #[test]
    fn dimensions_parse_into_a_sorted_map() {
        let dimensions = task_dimensions("build[os=linux,arch=arm]").unwrap();

        assert_eq!(dimensions.get("os").map(String::as_str), Some("linux"));
        assert_eq!(dimensions.get("arch").map(String::as_str), Some("arm"));
        assert_eq!(dimensions.len(), 2);
    }

    #[test]
    fn malformed_dimensions_are_rejected() {
        for reference in [
            "build[",
            "build[]",
            "build[os]",
            "build[os=]",
            "build[=linux]",
            "build[os=linux,os=mac]",
        ] {
            assert_eq!(task_dimensions(reference), Err(()), "{reference}");
        }
    }

    #[test]
    fn an_instance_is_a_format_parse_round_trip() {
        let dimensions = task_dimensions("build[arch=x64,os=linux]").unwrap();

        assert_eq!(
            format_task_instance("build", &dimensions),
            "build[arch=x64,os=linux]"
        );
        assert_eq!(
            task_dimensions(&format_task_instance("build", &dimensions)).unwrap(),
            dimensions
        );
    }

    #[test]
    fn matrix_instances_are_the_cartesian_product_in_key_order() {
        let matrix = BTreeMap::from([
            ("os".to_owned(), vec!["linux".to_owned(), "mac".to_owned()]),
            ("arch".to_owned(), vec!["x64".to_owned()]),
        ]);

        let instances = matrix_instances(&matrix, &BTreeMap::new()).unwrap();

        assert_eq!(
            instances
                .iter()
                .map(|instance| format_task_instance("build", instance))
                .collect::<Vec<_>>(),
            vec!["build[arch=x64,os=linux]", "build[arch=x64,os=mac]"]
        );
    }

    #[test]
    fn matrix_instances_pin_a_fixed_dimension() {
        let matrix = BTreeMap::from([(
            "os".to_owned(),
            vec!["linux".to_owned(), "mac".to_owned()],
        )]);
        let fixed = BTreeMap::from([("os".to_owned(), "mac".to_owned())]);

        let instances = matrix_instances(&matrix, &fixed).unwrap();

        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0]["os"], "mac");
    }

    #[test]
    fn a_fixed_matrix_value_must_exist_in_the_dimension() {
        let matrix = BTreeMap::from([(
            "os".to_owned(),
            vec!["linux".to_owned(), "mac".to_owned()],
        )]);
        let fixed = BTreeMap::from([("os".to_owned(), "windows".to_owned())]);

        assert_eq!(
            matrix_instances(&matrix, &fixed),
            Err("matrix dimension 'os' has no value 'windows'".to_owned())
        );
    }

    #[test]
    fn a_matrix_reference_must_name_every_dimension() {
        let matrix = BTreeMap::from([("os".to_owned(), vec!["linux".to_owned()])]);

        assert_eq!(
            validate_matrix_instance(&matrix, &BTreeMap::new()),
            Err("matrix task references must specify every dimension".to_owned())
        );
    }

    #[test]
    fn a_non_matrix_task_cannot_be_referenced_with_dimensions() {
        let dimensions = BTreeMap::from([("os".to_owned(), "linux".to_owned())]);

        assert_eq!(
            validate_matrix_instance(&BTreeMap::new(), &dimensions),
            Err("task is not matrix-parameterized".to_owned())
        );
    }

    #[test]
    fn placeholders_are_replaced_from_the_instance() {
        let dimensions = BTreeMap::from([("os".to_owned(), "linux".to_owned())]);

        assert_eq!(
            interpolate_value("bin/${os}/app", &dimensions).unwrap(),
            "bin/linux/app"
        );
        assert_eq!(interpolate_value("plain", &dimensions).unwrap(), "plain");
    }

    #[test]
    fn an_unknown_or_unterminated_placeholder_is_rejected() {
        let dimensions = BTreeMap::from([("os".to_owned(), "linux".to_owned())]);

        assert!(interpolate_value("${windows}", &dimensions).is_err());
        assert!(interpolate_value("${os", &dimensions).is_err());
    }

    #[test]
    fn edit_distance_counts_substitutions_insertions_and_deletions() {
        assert_eq!(edit_distance("", ""), 0);
        assert_eq!(edit_distance("abc", "abc"), 0);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("build", "builds"), 1);
    }

    #[test]
    fn a_close_name_is_suggested_only_within_the_distance_bound() {
        let candidates = [
            "test".to_owned(),
            "build".to_owned(),
            "release".to_owned(),
        ];

        assert_eq!(closest_name("tests", &candidates), Some("test".to_owned()));
        assert_eq!(closest_name("test", &candidates), None);
        assert_eq!(closest_name("completely-different", &candidates), None);
    }

    #[test]
    fn relative_paths_reject_parent_and_absolute_components() {
        assert!(valid_relative_path("services/api"));
        assert!(valid_relative_path("./api"));
        assert!(!valid_relative_path(""));
        assert!(!valid_relative_path(".."));
        assert!(!valid_relative_path("../api"));
        assert!(!valid_relative_path("/api"));
    }

    #[test]
    fn cache_patterns_are_relative_and_free_of_empty_segments() {
        assert!(validate_cache_pattern("src/**").is_ok());
        assert!(validate_cache_pattern("!src/**").is_ok());
        assert!(validate_cache_pattern("").is_err());
        assert!(validate_cache_pattern("!").is_err());
        assert!(validate_cache_pattern("/src/**").is_err());
        assert!(validate_cache_pattern("src//app").is_err());
        assert!(validate_cache_pattern("src/../app").is_err());
    }
}
```

- [ ] **Step 2: Run the tests**

Run: `cargo test --lib project::tests::`
Expected: all 18 tests PASS. If any fails, the test is wrong about intended behavior — fix the test to match the code and note it, because this task pins current behavior.

- [ ] **Step 3: Confirm no production code changed**

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
but diff
but commit -b testability/project-unit-tests -m "test: cover project manifest helpers directly" <ids>
```

---

### Task 2: Test `Project::load`'s failure vocabulary and `Project::plan`'s semantics

`ProjectError` has 12 variants and `Project::load` is the only thing that produces most of them. Every test here goes through a real `TempDir` because `load` reads a manifest and canonicalizes `cwd` — that is the module's actual boundary, so it is the right seam.

**Files:**
- Modify: `src/project.rs` (extend the `mod tests` added in Task 1)

- [ ] **Step 1: Add the fixtures and the failing tests**

Add inside `mod tests`:

```rust
    use crate::testing::TempDir;
    use std::fs;

    fn load(manifest: &str) -> (TempDir, Project) {
        let temp = TempDir::new();
        fs::write(config_path(temp.path()), manifest).expect("write manifest");
        let project = Project::load(temp.path()).expect("project loads");
        (temp, project)
    }

    fn reject(manifest: &str) -> ProjectError {
        let temp = TempDir::new();
        fs::write(config_path(temp.path()), manifest).expect("write manifest");
        Project::load(temp.path()).expect_err("manifest must be rejected")
    }

    const FINALIZER_MANIFEST: &str = "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\nfinally = [\"cleanup\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n\n[tasks.cleanup]\ncommand = [\"echo\", \"cleanup\"]\n";

    #[test]
    fn load_orders_dependencies_before_dependents() {
        let (_temp, project) = load(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"app\"]\n\n[tasks.base]\ncommand = [\"echo\", \"base\"]\n\n[tasks.app]\ncommand = [\"echo\", \"app\"]\ndepends_on = [\"base\"]\n",
        );

        let plan = project.plan(None, &[]).expect("plan succeeds");

        assert_eq!(
            plan.iter().map(PlannedTask::task).collect::<Vec<_>>(),
            vec!["base", "app"]
        );
    }

    #[test]
    fn a_dependency_cycle_is_reported_with_its_path() {
        let error = reject(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"app\"]\n\n[tasks.base]\ncommand = [\"echo\", \"base\"]\ndepends_on = [\"app\"]\n\n[tasks.app]\ncommand = [\"echo\", \"app\"]\ndepends_on = [\"base\"]\n",
        );

        assert!(matches!(error, ProjectError::TaskCycle { .. }), "{error}");
        assert!(error.to_string().contains("app -> base -> app"), "{error}");
    }

    #[test]
    fn an_unknown_default_pipeline_is_rejected() {
        let error = reject(
            "[project]\nname = \"fixture\"\ndefault_pipeline = \"missing\"\n\n[pipelines.ci]\ntasks = [\"app\"]\n\n[tasks.app]\ncommand = [\"echo\", \"app\"]\n",
        );

        assert!(
            matches!(error, ProjectError::UnknownPipeline { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_pipeline_reference_to_an_unknown_task_suggests_the_closest_name() {
        let error = reject(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"tests\"]\n\n[tasks.test]\ncommand = [\"echo\", \"test\"]\n",
        );

        assert!(
            matches!(
                error,
                ProjectError::MissingTask {
                    suggestion: Some(_),
                    ..
                }
            ),
            "{error}"
        );
        assert!(error.to_string().contains("Did you mean 'test'?"), "{error}");
    }

    #[test]
    fn a_matrix_task_expands_to_one_instance_per_combination() {
        let (_temp, project) = load(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"${os}\"]\nmatrix.os = [\"linux\", \"mac\"]\n",
        );

        let plan = project.plan(None, &[]).expect("plan succeeds");

        assert_eq!(
            plan.iter().map(PlannedTask::task).collect::<Vec<_>>(),
            vec!["build[os=linux]", "build[os=mac]"]
        );
        assert_eq!(
            plan[0].command(),
            ["echo".to_owned(), "linux".to_owned()]
        );
    }

    #[test]
    fn an_explicit_matrix_instance_selects_one_combination() {
        let (_temp, project) = load(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build[os=linux]\"]\n\n[tasks.build]\ncommand = [\"echo\", \"${os}\"]\nmatrix.os = [\"linux\", \"mac\"]\n",
        );

        let plan = project.plan(None, &[]).expect("plan succeeds");

        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].task(), "build[os=linux]");
    }

    #[test]
    fn an_explicit_matrix_value_outside_the_dimension_is_rejected() {
        let error = reject(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build[os=windows]\"]\n\n[tasks.build]\ncommand = [\"echo\", \"${os}\"]\nmatrix.os = [\"linux\", \"mac\"]\n",
        );

        assert!(matches!(error, ProjectError::InvalidTask { .. }), "{error}");
    }

    #[test]
    fn a_matrix_dependent_inherits_the_current_dimensions() {
        let (_temp, project) = load(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"package\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\nmatrix.os = [\"linux\", \"mac\"]\n\n[tasks.package]\ncommand = [\"echo\", \"package\"]\ndepends_on = [\"build\"]\nmatrix.os = [\"linux\", \"mac\"]\n",
        );

        let plan = project.plan(None, &[]).expect("plan succeeds");
        let package = plan
            .iter()
            .find(|task| task.task() == "package[os=linux]")
            .expect("package instance exists");

        assert_eq!(
            package
                .depends_on()
                .iter()
                .map(TaskNode::id)
                .collect::<Vec<_>>(),
            vec!["build[os=linux]"]
        );
    }

    #[test]
    fn pipeline_finalizers_are_marked_on_the_planned_tasks() {
        let (_temp, project) = load(FINALIZER_MANIFEST);

        let plan = project.plan(None, &[]).expect("plan succeeds");

        assert!(!plan[0].is_finalizer());
        assert!(plan[1].is_finalizer());
    }

    #[test]
    fn a_cacheable_finalizer_is_rejected() {
        let error = reject(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\nfinally = [\"cleanup\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n\n[tasks.cleanup]\ncommand = [\"echo\", \"cleanup\"]\ncache = true\ninputs = [\"src/**\"]\n",
        );

        assert!(
            error.to_string().contains("finalizer tasks cannot be cached"),
            "{error}"
        );
    }

    #[test]
    fn a_cacheable_task_must_declare_a_positive_input_pattern() {
        let error = reject(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\ncache = true\ninputs = [\"!vendor/**\"]\n",
        );

        assert!(
            error
                .to_string()
                .contains("at least one positive input pattern"),
            "{error}"
        );
    }

    #[test]
    fn a_cwd_escaping_the_project_root_is_rejected() {
        let error = reject(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\ncwd = \"../escape\"\n",
        );

        assert!(matches!(error, ProjectError::InvalidTask { .. }), "{error}");
    }

    #[test]
    fn a_missing_cwd_directory_cannot_be_resolved() {
        let error = reject(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\ncwd = \"missing\"\n",
        );

        // Task 7 replaces this variant with `TaskDirectory`; this test is the
        // one that changes there.
        assert!(matches!(error, ProjectError::Io { .. }), "{error}");
    }

    #[test]
    fn zero_timeout_and_zero_output_limit_are_rejected() {
        for (field, message) in [
            (
                "timeout_seconds = 0",
                "timeout_seconds must be greater than zero",
            ),
            ("max_output_bytes = 0", "max_output_bytes must be positive"),
        ] {
            let error = reject(&format!(
                "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n{field}\n"
            ));
            assert!(error.to_string().contains(message), "{error}");
        }
    }

    #[test]
    fn an_unsupported_schema_is_rejected() {
        let error = reject(
            "schema = 2\n\n[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n",
        );

        assert!(
            matches!(error, ProjectError::UnsupportedSchema { found: 2, .. }),
            "{error}"
        );
    }

    #[test]
    fn a_matrix_larger_than_the_bound_is_rejected() {
        let values = (0..1025)
            .map(|index| format!("\"v{index}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let error = reject(&format!(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\nmatrix.os = [{values}]\n"
        ));

        assert!(
            error.to_string().contains("more than 1024 instances"),
            "{error}"
        );
    }
```

- [ ] **Step 2: Run the tests**

Run: `cargo test --lib project::tests::`
Expected: all tests PASS (34 total in the module). The `a_missing_cwd_directory_cannot_be_resolved` case must currently match `ProjectError::Io` — if it does not, stop and read `validate_task_config` before changing anything.

- [ ] **Step 3: Run the full suite**

Run: `cargo test --workspace --all-targets --all-features`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
but diff
but commit -b testability/project-behavior-tests -m "test: cover Project::load errors and plan semantics" <ids>
```

---

### Task 3: Inject writers into `OutputSink`

Today `render_event` writes to the process-global handles, and the only testable mode is JSON via a `#[cfg(test)] json_lines` field. Inject the two sinks, delete the test-only field, and render every mode into captured buffers.

Behavior change: none. The JSON test path stops being special-cased, so it starts exercising the real `write_json_event_with_identity` plus the real `run_id`/`sequence` counters.

**Files:**
- Modify: `src/output.rs`
- Modify: `src/commands/ci.rs:70` (only if the constructor name changes — it does not)

- [ ] **Step 1: Write the failing tests**

Replace the whole `#[cfg(test)] mod tests` block in `src/output.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::ExecutionSummary;
    use std::sync::{Arc, Mutex};

    /// A `Write` handle over a buffer the test can read back.
    #[derive(Clone)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("shared writer lock")
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    type Captured = (OutputSink, Arc<Mutex<Vec<u8>>>, Arc<Mutex<Vec<u8>>>);

    /// A sink with a pinned run identity, so CI output is byte-for-byte assertable.
    fn captured(mode: OutputMode, run_id: u64) -> Captured {
        let out = Arc::new(Mutex::new(Vec::new()));
        let err = Arc::new(Mutex::new(Vec::new()));
        let sink = OutputSink::test_sink(
            mode,
            run_id,
            Box::new(SharedWriter(Arc::clone(&out))),
            Box::new(SharedWriter(Arc::clone(&err))),
        );
        (sink, out, err)
    }

    fn text(buffer: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8(buffer.lock().expect("buffer lock").clone())
            .expect("rendered output is UTF-8")
    }

    fn result(stdout: &[u8], cached: bool) -> TaskResult {
        TaskResult {
            output: crate::runner::CapturedOutput {
                stdout: stdout.to_vec(),
                stderr: Vec::new(),
            },
            elapsed: std::time::Duration::from_millis(42),
            cached,
        }
    }

    #[test]
    fn terminal_renders_a_golden_transcript() {
        let (sink, out, err) = captured(OutputMode::Terminal, 1);
        let node = TaskNode::new("build");

        sink.present_start(&node).expect("start renders");
        sink.present_attempt(&node, 2, 3).expect("attempt renders");
        sink.present_success(&node, &result(b"hello\n", false))
            .expect("success renders");

        assert_eq!(text(&out), "hello\n");
        assert_eq!(
            text(&err),
            "\u{25b6} build\n\u{21bb} build: retry 2/3\nbuild: completed in 42ms\n"
        );
    }

    #[test]
    fn terminal_marks_a_cache_hit() {
        let (sink, out, err) = captured(OutputMode::Terminal, 1);
        let node = TaskNode::new("build");

        sink.present_success(&node, &result(b"cached\n", true))
            .expect("success renders");

        assert_eq!(text(&out), "cached\n");
        assert_eq!(text(&err), "build: cache hit in 42ms\n");
    }

    #[test]
    fn live_streams_bytes_once_and_keeps_status_on_stderr() {
        let (sink, out, err) = captured(OutputMode::Live, 1);
        let node = TaskNode::new("build");

        sink.present_start(&node).expect("start renders");
        sink.present_live_output(&node, TaskStream::Stdout, b"streamed".to_vec())
            .expect("live output renders");
        sink.present_success(&node, &result(b"streamed", false))
            .expect("success does not repeat live bytes");

        assert_eq!(text(&out), "streamed");
        assert_eq!(
            text(&err),
            "\u{25b6} build\nbuild: completed in 42ms\n"
        );
    }

    #[test]
    fn github_actions_wraps_task_output_in_stop_and_resume_markers() {
        let (sink, out, _err) = captured(OutputMode::GithubActions, 7);
        let node = TaskNode::new("build");

        sink.present_start(&node).expect("start renders");
        sink.present_success(&node, &result(b"::error:: injected\n", false))
            .expect("success renders");

        let rendered = text(&out);
        assert!(rendered.starts_with("::group::build\n"), "{rendered}");
        assert!(rendered.contains("::stop-commands::"), "{rendered}");
        assert!(rendered.contains("::mono_output_7_"), "{rendered}");
        assert!(rendered.contains("::error:: injected\n"), "{rendered}");
        assert!(rendered.ends_with("build: completed in 42ms\n::endgroup::\n"), "{rendered}");
    }

    #[test]
    fn json_writes_one_identified_object_per_line_through_the_production_path() {
        let (sink, out, err) = captured(OutputMode::Json, 7);
        let node = TaskNode::new("build");

        sink.present_run_start(Path::new("/workspace"), 1)
            .expect("run start renders");
        sink.present_start(&node).expect("start renders");
        sink.present_success(&node, &result(b"hello\n", false))
            .expect("success renders");

        assert!(text(&err).is_empty(), "JSON writes nothing to stderr");
        let lines = text(&out);
        let events = lines
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("valid JSON"))
            .collect::<Vec<_>>();

        // run_started, task_started, task_output, task_finished
        assert_eq!(events.len(), 4);
        assert_eq!(events[0]["event"], "run_started");
        assert_eq!(events[1]["event"], "task_started");
        assert_eq!(events[2]["event"], "task_output");
        assert_eq!(events[3]["event"], "task_finished");
        assert!(events.iter().all(|event| event["run_id"] == 7));
        assert_eq!(
            events
                .iter()
                .map(|event| event["sequence"].as_u64().expect("sequence"))
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
    }

    #[test]
    fn json_run_finished_carries_the_summary_counts() {
        let (sink, out, _err) = captured(OutputMode::Json, 7);
        let summary = ExecutionSummary {
            completed: 2,
            cached: 1,
            failed: 0,
            cancelled: 0,
            blocked: 0,
        };

        sink.present_run_finished(&summary).expect("summary renders");

        let events = text(&out)
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("valid JSON"))
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event"], "run_finished");
        assert_eq!(events[0]["completed"], 2);
        assert_eq!(events[0]["cached"], 1);
    }

    #[test]
    fn json_preserves_non_utf8_task_output_as_bytes() {
        let (sink, out, _err) = captured(OutputMode::Json, 7);
        let node = TaskNode::new("build");

        sink.present_success(&node, &result(&[0u8, 255u8, 10u8], false))
            .expect("success renders");

        let first = text(&out);
        let event: serde_json::Value = serde_json::from_str(
            first.lines().next().expect("a first event line"),
        )
        .expect("valid JSON");
        let bytes = event["bytes"]
            .as_array()
            .expect("bytes array")
            .iter()
            .map(|value| value.as_u64().expect("byte") as u8)
            .collect::<Vec<_>>();

        assert_eq!(bytes, vec![0u8, 255u8, 10u8]);
    }

    #[test]
    fn every_mode_renders_a_blocked_task() {
        let node = TaskNode::new("build");

        for mode in [
            OutputMode::Terminal,
            OutputMode::Json,
            OutputMode::GithubActions,
            OutputMode::Live,
        ] {
            let (sink, out, err) = captured(mode, 1);
            sink.present_blocked(&node).expect("blocked renders");
            let rendered = format!("{}{}", text(&out), text(&err));
            assert!(rendered.contains("blocked"), "{mode:?}: {rendered}");
        }
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib output::tests::`
Expected: FAIL to compile — `OutputSink::test_sink` does not exist, `TaskNode`/`Duration` imports may be missing, and `result()` shadows the old test fixtures. This is the red state.

- [ ] **Step 3: Inject the writers**

In `src/output.rs`, replace the `OutputSink` definition and its `new`/`render_event` plumbing:

```rust
/// Owns task presentation so worker threads never write directly to the
/// process-global stdout or stderr handles.
///
/// The two write handles are fields rather than process globals so every
/// output mode is renderable into a captured buffer.
pub(crate) struct OutputSink {
    mode: OutputMode,
    writers: Mutex<Writers>,
    run_id: u64,
    sequence: AtomicU64,
}

struct Writers {
    out: Box<dyn Write + Send>,
    err: Box<dyn Write + Send>,
}

impl std::fmt::Debug for OutputSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OutputSink")
            .field("mode", &self.mode)
            .field("run_id", &self.run_id)
            .finish_non_exhaustive()
    }
}

impl OutputSink {
    /// Render to the real process handles.
    pub(crate) fn new(mode: OutputMode) -> Self {
        Self::with_writers(mode, Box::new(io::stdout()), Box::new(io::stderr()))
    }

    /// Render to caller-owned handles. A test passes buffers; the CLI passes
    /// the process stdio.
    pub(crate) fn with_writers(
        mode: OutputMode,
        out: Box<dyn Write + Send>,
        err: Box<dyn Write + Send>,
    ) -> Self {
        Self::with_identity(mode, run_identity(), out, err)
    }

    fn with_identity(
        mode: OutputMode,
        run_id: u64,
        out: Box<dyn Write + Send>,
        err: Box<dyn Write + Send>,
    ) -> Self {
        Self {
            mode,
            writers: Mutex::new(Writers { out, err }),
            run_id,
            sequence: AtomicU64::new(0),
        }
    }

    /// A sink with a pinned run identity, so CI output is deterministic.
    #[cfg(test)]
    pub(crate) fn test_sink(
        mode: OutputMode,
        run_id: u64,
        out: Box<dyn Write + Send>,
        err: Box<dyn Write + Send>,
    ) -> Self {
        Self::with_identity(mode, run_id, out, err)
    }
```

Then change every `present_*` method from `let _guard = self.lock.lock()...` to `let mut writers = self.writers.lock().expect("output lock is not poisoned");` and pass `&mut writers` to `render_event`. `render_event` and the three renderers take `writers: &mut Writers`:

```rust
    fn render_event(&self, writers: &mut Writers, event: &ExecutionEvent) -> io::Result<()> {
        match self.mode {
            OutputMode::Json => write_json_event_with_identity(
                event,
                self.run_id,
                self.sequence.fetch_add(1, Ordering::Relaxed),
                &mut *writers.out,
            ),
            OutputMode::Terminal => self.render_terminal(writers, event),
            OutputMode::GithubActions => self.render_github_actions(writers, event),
            OutputMode::Live => self.render_live(writers, event),
        }
    }
```

In `render_terminal`, `render_live`, and `render_github_actions`, replace every `io::stderr().lock()` with `&mut *writers.err` and every `io::stdout().lock()` with `&mut *writers.out`. `write_bytes` becomes `write_bytes(writers, bytes, to_stderr)`:

```rust
fn write_bytes(writers: &mut Writers, bytes: &[u8], to_stderr: bool) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    let handle: &mut dyn Write = if to_stderr {
        &mut *writers.err
    } else {
        &mut *writers.out
    };
    write_line_terminated(handle, bytes)?;
    handle.flush()
}
```

Delete `start_section` (it is a no-op), `present_summary` (`#[allow(dead_code)]`, duplicate of `present_run_finished`), and `write_status` (`#[allow(dead_code)]`, unreferenced). Delete the now-unused `#[cfg(test)] write_json_event` helper and the `json_lines` field and its initialization in `new`.

`run_identity()` stays as it is. `write_line_terminated` and `write_json_event_with_identity` keep their bodies but **must take `&mut (impl Write + ?Sized)`** instead of `&mut impl Write`: the call sites now pass `&mut *writers.out`, which is `&mut (dyn Write + Send)`, and an unsized `&mut dyn Write` does not satisfy a `Sized` `impl Write` parameter (`E0277: the size for values of type dyn std::io::Write + Send cannot be known at compilation time`). This was verified with a minimal `rustc` reproduction during execution; the plan's original literal snippets did not compile.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib output::tests::`
Expected: all 8 tests PASS.

Then run the full suite, because `commands/ci.rs` and `scheduler.rs` use `present_*`:
Run: `cargo test --workspace --all-targets --all-features`
Expected: PASS, including `tests/cli.rs::json_output_contains_lifecycle_events` and `tests/cli.rs::github_actions_output_disables_command_processing_for_task_output`.

- [ ] **Step 5: Confirm no process globals remain in the renderer**

Run: `grep -n "io::stdout()\|io::stderr()" src/output.rs`
Expected: exactly two hits, both inside `OutputSink::new`. rustfmt may place both on one line.

**As-built notes (what execution actually required, beyond the snippets above):**

1. **`?Sized` bound (required).** `write_line_terminated` and `write_json_event_with_identity` must be declared `&mut (impl Write + ?Sized)`. See the note at the end of Step 3.
2. **`present_failure` collapses to a let-chain (required).** Removing the `start_section(node)?;` call leaves `else { if let Some(output) = error.output() { … } }`, which clippy rejects as `collapsible_if` under `-D warnings`. It must become:

```rust
        } else if self.mode != OutputMode::Live
            && let Some(output) = error.output()
        {
            write_bytes(&mut writers, &output.stdout, false)?;
            write_bytes(&mut writers, &output.stderr, true)?;
        }
```

   `present_success` similarly becomes `} else if self.mode != OutputMode::Live || result.cached {`. Control flow is otherwise identical, because `start_section` was a no-op.
3. **Accepted residual risk, deliberately not fixed.** `Writers.out` holds a `Box<io::Stdout>` rather than a `StdoutLock` held across one render, so the process stdout lock is now taken per write rather than per event. This is safe here because every task-output write goes through the sink's single `Mutex<Writers>`, and the only other writers — `main::emit_summary` and `main::emit_error` — run strictly after `execute_plan` returns. No interleaving is reachable without a second thread writing to process stdout mid-render, which the CLI never does. If a future caller renders to process stdout concurrently with a pipeline, this becomes a real bug and the fix is to hold the lock for the render's duration.

- [ ] **Step 6: Commit**

```bash
but diff
but commit -b testability/output-writers -m "refactor: render every output mode into injected writers" <ids>
```

---

## Phase B — Add the missing vocabulary and seams

### Task 4: Map `RunnerError` to `TaskStatus` in one place

`output.rs::present_failure` decides the lifecycle status inline, and `execute_plan` decides the same thing a second time when it counts the summary. Two mappings can disagree; the JSON `status` field and the run summary would then tell different stories. Make it one function next to the error, then use it for both.

**Files:**
- Modify: `src/events.rs` (`TaskStatus` derives `PartialEq, Eq`)
- Modify: `src/runner.rs` (add `RunnerError::status`)
- Modify: `src/output.rs` (`present_failure` uses it)
- Modify: `src/scheduler.rs` (add `classify`, use it in `execute_plan`)

- [ ] **Step 1: Write the failing tests**

In `src/events.rs`, change the derive on `TaskStatus`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
```

Add to the `mod tests` in `src/runner.rs`:

```rust
    #[test]
    fn a_failure_maps_onto_exactly_one_lifecycle_status() {
        use crate::events::TaskStatus;

        assert_eq!(
            RunnerError::Cancelled(Box::new(CancelledTask {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
                output: CapturedOutput::default(),
                elapsed: Duration::ZERO,
            }))
            .status(),
            TaskStatus::Cancelled
        );
        assert_eq!(
            RunnerError::TimedOut(Box::new(TimedOutTask {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
                command: vec!["sleep".to_owned()],
                cwd: PathBuf::from("/tmp"),
                timeout: Duration::from_secs(1),
                output: CapturedOutput::default(),
                elapsed: Duration::ZERO,
            }))
            .status(),
            TaskStatus::TimedOut
        );
        assert_eq!(
            RunnerError::OutputLimit(Box::new(OutputLimitTask {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
                stream: "stdout",
                limit: 8,
                output: CapturedOutput::default(),
                elapsed: Duration::ZERO,
            }))
            .status(),
            TaskStatus::OutputLimit
        );
        assert_eq!(
            RunnerError::EmptyCommand {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
            }
            .status(),
            TaskStatus::Failed
        );
    }
```

Add a new `mod tests` at the end of `src/scheduler.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::config_path;
    use crate::events::TaskStatus;
    use crate::project::Project;
    use crate::runner::{CapturedOutput, RunnerError, TaskResult};
    use crate::testing::TempDir;
    use std::fs;
    use std::time::Duration;

    fn nodes(ids: &[&str]) -> Vec<TaskNode> {
        ids.iter().map(|id| TaskNode::new(*id)).collect()
    }

    fn planned(manifest: &str) -> Vec<PlannedTask> {
        let temp = TempDir::new();
        fs::write(config_path(temp.path()), manifest).expect("write manifest");
        let project = Project::load(temp.path()).expect("project loads");
        project.plan(None, &[]).expect("plan succeeds")
    }

    fn task_map(plan: &[PlannedTask]) -> BTreeMap<TaskNode, Arc<PlannedTask>> {
        plan.iter()
            .map(|task| (task.node(), Arc::new(task.clone())))
            .collect()
    }

    fn succeeded() -> Result<TaskResult, RunnerError> {
        Ok(TaskResult {
            output: CapturedOutput::default(),
            elapsed: Duration::ZERO,
            cached: false,
        })
    }

    #[test]
    fn a_finished_plan_accounts_for_every_task() {
        let plan = nodes(&["a", "b", "c"]);

        let (summary, first_error) = classify(&plan, |node| match node.id() {
            "a" => Some(TaskStatus::Completed),
            "b" => Some(TaskStatus::Cached),
            _ => None,
        });

        assert_eq!(
            summary,
            ExecutionSummary {
                completed: 1,
                cached: 1,
                failed: 0,
                cancelled: 0,
                blocked: 1,
            }
        );
        assert!(first_error.is_none());
    }

    #[test]
    fn every_failure_kind_counts_as_failed_and_the_first_one_wins() {
        let plan = nodes(&["a", "b", "c"]);

        let (summary, first_error) = classify(&plan, |node| match node.id() {
            "a" => Some(TaskStatus::TimedOut),
            "b" => Some(TaskStatus::Failed),
            _ => Some(TaskStatus::OutputLimit),
        });

        assert_eq!(summary.failed, 3);
        assert_eq!(
            first_error.map(|node| node.id().to_owned()),
            Some("a".to_owned())
        );
    }

    #[test]
    fn a_cancelled_task_is_counted_separately_and_still_reported() {
        let plan = nodes(&["a", "b"]);

        let (summary, first_error) = classify(&plan, |node| match node.id() {
            "a" => Some(TaskStatus::Cancelled),
            _ => Some(TaskStatus::Failed),
        });

        assert_eq!(summary.cancelled, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(
            first_error.map(|node| node.id().to_owned()),
            Some("a".to_owned()),
            "an interrupted run must still report an error, or Ctrl-C would exit 0"
        );
    }

    #[test]
    fn a_plan_of_only_cancelled_tasks_still_reports_an_error() {
        let plan = nodes(&["a", "b"]);

        let (summary, first_error) = classify(&plan, |_| Some(TaskStatus::Cancelled));

        assert_eq!(summary.cancelled, 2);
        assert_eq!(summary.failed, 0);
        assert_eq!(
            first_error.map(|node| node.id().to_owned()),
            Some("a".to_owned())
        );
    }

    #[test]
    fn a_held_resource_group_blocks_its_next_task() {
        let plan = planned(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"a\", \"b\"]\n\n[tasks.a]\ncommand = [\"echo\", \"a\"]\nresource_group = \"db\"\n\n[tasks.b]\ncommand = [\"echo\", \"b\"]\nresource_group = \"db\"\n",
        );
        let tasks = task_map(&plan);
        let ready = plan.iter().map(PlannedTask::node).collect::<BTreeSet<_>>();

        assert_eq!(
            next_ready(
                &ready,
                &tasks,
                &HashSet::from(["db".to_owned()]),
                false,
                true
            ),
            None
        );
        assert_eq!(
            next_ready(&ready, &tasks, &HashSet::new(), false, true),
            Some(TaskNode::new("a"))
        );
    }

    #[test]
    fn stopping_admits_only_finalizers() {
        let plan = planned(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\nfinally = [\"cleanup\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n\n[tasks.cleanup]\ncommand = [\"echo\", \"cleanup\"]\n",
        );
        let tasks = task_map(&plan);
        let ready = plan.iter().map(PlannedTask::node).collect::<BTreeSet<_>>();

        assert_eq!(
            next_ready(&ready, &tasks, &HashSet::new(), true, true),
            Some(TaskNode::new("cleanup"))
        );
    }

    #[test]
    fn a_finalizer_waits_until_normal_work_is_done() {
        let plan = planned(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\nfinally = [\"cleanup\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n\n[tasks.cleanup]\ncommand = [\"echo\", \"cleanup\"]\n",
        );
        let tasks = task_map(&plan);
        let ready = BTreeSet::from([TaskNode::new("cleanup")]);

        assert_eq!(
            next_ready(&ready, &tasks, &HashSet::new(), false, false),
            None
        );
        assert_eq!(
            next_ready(&ready, &tasks, &HashSet::new(), false, true),
            Some(TaskNode::new("cleanup"))
        );
    }

    #[test]
    fn a_ready_finalizer_is_detected() {
        let plan = planned(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\nfinally = [\"cleanup\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n\n[tasks.cleanup]\ncommand = [\"echo\", \"cleanup\"]\n",
        );
        let tasks = task_map(&plan);

        assert!(!has_ready_finalizer(
            &BTreeSet::from([TaskNode::new("build")]),
            &tasks
        ));
        assert!(has_ready_finalizer(
            &BTreeSet::from([TaskNode::new("cleanup")]),
            &tasks
        ));
    }

    #[test]
    fn finalizers_wait_for_every_normal_task_while_running() {
        let plan = planned(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\nfinally = [\"cleanup\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n\n[tasks.cleanup]\ncommand = [\"echo\", \"cleanup\"]\n",
        );
        let tasks = task_map(&plan);
        let results = BTreeMap::new();

        assert!(!finalizers_allowed(&tasks, &results, &HashSet::new(), false));

        let mut done = BTreeMap::new();
        done.insert(TaskNode::new("build"), succeeded());
        assert!(finalizers_allowed(&tasks, &done, &HashSet::new(), false));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib scheduler::tests::`
Expected: FAIL to compile — `classify` and `finalizers_allowed` do not exist.

- [ ] **Step 3: Add `RunnerError::status`**

In `src/runner.rs`, inside `impl RunnerError`, next to `output()`/`elapsed()`:

```rust
    /// The lifecycle status this failure presents as.
    ///
    /// One mapping, next to the error vocabulary, so the JSON `status` field
    /// and the run summary can never disagree about what a failure was.
    pub(crate) fn status(&self) -> crate::events::TaskStatus {
        use crate::events::TaskStatus;

        match self {
            Self::TimedOut(_) => TaskStatus::TimedOut,
            Self::OutputLimit(_) => TaskStatus::OutputLimit,
            Self::Cancelled(_) => TaskStatus::Cancelled,
            Self::EmptyCommand { .. }
            | Self::Spawn { .. }
            | Self::Wait { .. }
            | Self::OutputRead { .. }
            | Self::Terminate { .. }
            | Self::Failed(_) => TaskStatus::Failed,
        }
    }
```

In `src/output.rs::present_failure`, replace the status match with:

```rust
        let status = error.status();
```

- [ ] **Step 4: Add `classify` and `finalizers_allowed` to `scheduler.rs`, and use them**

Add these functions next to `next_ready`:

```rust
/// Whether a finalizer may start now.
///
/// While normal work is running, finalizers wait until every normal task has
/// a result. Once the run is stopping, they wait only for the normal tasks
/// still in flight.
fn finalizers_allowed(
    tasks: &BTreeMap<TaskNode, Arc<PlannedTask>>,
    results: &BTreeMap<TaskNode, Result<TaskResult, RunnerError>>,
    active: &HashSet<TaskNode>,
    stopping: bool,
) -> bool {
    if stopping {
        !active.iter().any(|node| {
            !tasks
                .get(node)
                .expect("active task must exist in the validated plan")
                .is_finalizer()
        })
    } else {
        tasks
            .iter()
            .filter(|(_, task)| !task.is_finalizer())
            .all(|(node, _)| results.contains_key(node))
    }
}

/// Classify a completed plan in plan order.
///
/// `status` answers what happened to a node, or `None` when the node never
/// ran. The summary parts always add up to the plan length, and the returned
/// node is the first one that reported an error in plan order.
///
/// A cancelled task is counted in `cancelled`, not in `failed`, but it still
/// counts as the reported error. That distinction is load-bearing: an
/// interrupted run must fail, not report success. Dropping it makes Ctrl-C
/// exit `0`, because `execute_plan` then falls through to its `Ok(summary)`
/// tail instead of returning `SchedulerError::Task`.
fn classify(
    nodes: &[TaskNode],
    status: impl Fn(&TaskNode) -> Option<crate::events::TaskStatus>,
) -> (ExecutionSummary, Option<TaskNode>) {
    use crate::events::TaskStatus;

    let mut summary = ExecutionSummary::default();
    let mut first_error = None;
    for node in nodes {
        match status(node) {
            None | Some(TaskStatus::Blocked) => summary.blocked += 1,
            Some(TaskStatus::Cached) => summary.cached += 1,
            Some(TaskStatus::Completed) => summary.completed += 1,
            Some(TaskStatus::Cancelled) => {
                summary.cancelled += 1;
                first_error.get_or_insert_with(|| node.clone());
            }
            Some(TaskStatus::Failed | TaskStatus::TimedOut | TaskStatus::OutputLimit) => {
                summary.failed += 1;
                first_error.get_or_insert_with(|| node.clone());
            }
        }
    }
    assert_eq!(
        summary.completed + summary.cached + summary.failed + summary.cancelled + summary.blocked,
        nodes.len(),
        "scheduler result accounting must cover the entire plan"
    );
    (summary, first_error)
}
```

In `execute_plan`, replace the inline `finalizers_allowed` computation:

```rust
            let finalizers_ready =
                finalizers_allowed(&tasks, &results, &active, stopping);
```

The local is deliberately named `finalizers_ready`, not `finalizers_allowed`: shadowing the function name with a `bool` compiles (the call resolves before the binding) but breaks the moment anyone adds a second call in the same block. Pass `finalizers_ready` to `next_ready` in place of the old `finalizers_allowed` argument.

and replace the accounting block (from `let mut first_error = None;` through the `assert_eq!`) with:

```rust
    let nodes = plan.iter().map(PlannedTask::node).collect::<Vec<_>>();
    let (summary, first_error_node) = classify(&nodes, |node| match results.get(node) {
        None => None,
        Some(Ok(result)) if result.cached => Some(crate::events::TaskStatus::Cached),
        Some(Ok(_)) => Some(crate::events::TaskStatus::Completed),
        Some(Err(error)) => Some(error.status()),
    });
    for task in plan {
        let node = task.node();
        match results.get(&node) {
            None => output
                .present_blocked(&node)
                .map_err(SchedulerError::Output)?,
            Some(Ok(result)) => output
                .present_success(&node, result)
                .map_err(SchedulerError::Output)?,
            Some(Err(error)) => output
                .present_failure(&node, error)
                .map_err(SchedulerError::Output)?,
        }
    }
    let first_error = first_error_node.and_then(|node| match results.remove(&node) {
        Some(Err(error)) => Some(error),
        _ => None,
    });
```

Delete the old `let mut first_error = None; let mut summary = ...; for task in plan { ... } assert_eq!(...)` accounting block. `results` keeps its `mut` binding because the `first_error` extraction consumes the failing entry.

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib scheduler::tests:: runner::tests::`
Expected: all PASS.

Run: `cargo test --workspace --all-targets --all-features`
Expected: PASS. `tests/cli.rs::json_output_contains_lifecycle_events` proves the JSON status field still comes out right.

**As-built note — a plan bug found in execution, and the fix (required).**

The version of `classify` originally specified here set its first-error node only for `Failed`/`TimedOut`/`OutputLimit`, and counted `Cancelled` without recording it. That silently changed behavior. The original inline loop ran `first_error.get_or_insert(error)` for **every** `Err`, including `RunnerError::Cancelled`:

```rust
            Err(error) => {
                if matches!(error, RunnerError::Cancelled(_)) {
                    summary.cancelled += 1;
                } else {
                    summary.failed += 1;
                }
                output.present_failure(&node, &error).map_err(SchedulerError::Output)?;
                first_error.get_or_insert(error);   // runs for Cancelled too
            }
```

With the original (buggy) `classify`, `first_error` stayed `None` for a cancelled run, so `execute_plan` skipped its `Err(SchedulerError::Task(..))` tail and reached `if cancellation.is_cancelled() && summary.cancelled == 0 && summary.blocked > 0` — false, because `summary.cancelled > 0` — and returned `Ok(summary)`. **A Ctrl-C'd pipeline would exit `0` with a green summary.** No test failed, because nothing covered the cancellation path.

The fix, now reflected in the `classify` code above: a cancelled node increments `cancelled` **and** claims the first-error slot. The two summary counters stay separate (`cancelled` is not `failed`), but both claim the reported error. Verified end to end by signalling a running `mono`:

```console
$ ./target/debug/mono --dir "$tmp" ci --no-cache &   # task: sleep 30
$ sleep 2; kill -INT $!
$ wait $!; echo $?
1
▶ slow
slow: cancelled in 1739ms
mono: cancel/slow was cancelled
```

`tests/cli.rs::an_interrupted_run_does_not_report_success` now pins this: it spawns `mono`, sends `SIGINT`, and asserts the exit code is not `0`. It asserts "not success" rather than `Some(1)` so it still means something if the signal lands before the handler is installed and the process dies by signal.

**Generalizable rule for the remaining tasks:** when extracting a pure function out of a loop, the burden of proof is on the extraction to reproduce *every* side effect of the original, including the ones the summary counters do not name. Diff the old branch-by-branch, not just the totals.

- [ ] **Step 6: Commit**

```bash
but diff
but commit -b testability/scheduler-decisions -m "refactor: derive task status and run summary from one mapping" <ids>
```

---

### Task 5: Gather the environment at the edge

`cache.rs::task_key_with_session` calls `std::env::vars()` and `std::env::var` while computing a hash. The cache key therefore depends on ambient process state, and a test cannot change it safely — `std::env::set_var` is `unsafe` in Rust 2024. Read the environment once in `execute_plan`, pass it into `CacheSession`, and delete the `prepare(_plan)` and `task_key_with_session(_project_root)` parameters that exist only to be ignored.

**Files:**
- Modify: `src/cache.rs`
- Modify: `src/scheduler.rs:65` and `src/scheduler.rs:438`

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` in `src/cache.rs`:

```rust
    fn project_with_cache_env(temp: &TempDir, cache_env: &str, task_env: &str) -> Project {
        fs::write(
            config_path(temp.path()),
            format!(
                "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"sh\", \"-c\", \"cat input.txt > output.txt\"]\ncache = true\ninputs = [\"input.txt\"]\noutputs = [\"output.txt\"]\ncache_env = [{cache_env}]\n{task_env}"
            ),
        )
        .expect("write root manifest");
        fs::write(temp.path().join("input.txt"), "input").expect("write input");
        Project::load(temp.path()).expect("project loads")
    }

    fn key_with_environment(
        store: &CacheStore,
        project: &Project,
        task: &PlannedTask,
        environment: BTreeMap<String, String>,
    ) -> String {
        let session = store
            .prepare(&project.root, environment)
            .expect("cache session prepares");
        store
            .task_key_with_session(&session, task, &[])
            .expect("key succeeds")
    }

    #[test]
    fn a_declared_cache_environment_variable_changes_the_key() {
        let temp = TempDir::new();
        let project = project_with_cache_env(&temp, "\"MODE\"", "");
        let task = only_task(&project);
        let store = CacheStore::new(&project.root);

        let debug = key_with_environment(
            &store,
            &project,
            &task,
            BTreeMap::from([("MODE".to_owned(), "debug".to_owned())]),
        );
        let release = key_with_environment(
            &store,
            &project,
            &task,
            BTreeMap::from([("MODE".to_owned(), "release".to_owned())]),
        );

        assert_ne!(debug, release);
    }

    #[test]
    fn an_unset_cache_environment_variable_hashes_as_unset() {
        let temp = TempDir::new();
        let project = project_with_cache_env(&temp, "\"MODE\"", "");
        let task = only_task(&project);
        let store = CacheStore::new(&project.root);

        let first = key_with_environment(&store, &project, &task, BTreeMap::new());
        let second = key_with_environment(&store, &project, &task, BTreeMap::new());

        assert_eq!(first, second);
    }

    #[test]
    fn a_task_environment_value_overrides_the_process_environment() {
        let temp = TempDir::new();
        let project = project_with_cache_env(&temp, "\"MODE\"", "env = { MODE = \"check\" }\n");
        let task = only_task(&project);
        let store = CacheStore::new(&project.root);

        let from_task = key_with_environment(
            &store,
            &project,
            &task,
            BTreeMap::from([("MODE".to_owned(), "debug".to_owned())]),
        );
        let from_task_again = key_with_environment(
            &store,
            &project,
            &task,
            BTreeMap::from([("MODE".to_owned(), "release".to_owned())]),
        );

        assert_eq!(
            from_task, from_task_again,
            "the task's own env must win over the ambient environment"
        );
    }

    #[test]
    fn a_wildcard_cache_environment_includes_every_variable() {
        let temp = TempDir::new();
        let project = project_with_cache_env(&temp, "\"*\"", "");
        let task = only_task(&project);
        let store = CacheStore::new(&project.root);

        let one = key_with_environment(
            &store,
            &project,
            &task,
            BTreeMap::from([("UNRELATED".to_owned(), "one".to_owned())]),
        );
        let two = key_with_environment(
            &store,
            &project,
            &task,
            BTreeMap::from([("UNRELATED".to_owned(), "two".to_owned())]),
        );

        assert_ne!(one, two);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib cache::tests::`
Expected: FAIL to compile — `prepare` takes `&[PlannedTask]`, `task_key_with_session` takes `_project_root`, and `key_with_environment` does not exist yet.

- [ ] **Step 3: Put the environment in the session**

In `src/cache.rs`, add the import and change `CacheSession`, `prepare`, `task_key`, `task_key_with_session`:

```rust
use crate::config::config_path;
```

```rust
/// Immutable cache state shared by every task in one execution.
///
/// Manifests, the ambient environment, and cache-directory setup are project
/// state, not task state. Keeping them here prevents every cacheable task from
/// repeating the same filesystem work and keeps `std::env` out of the hasher.
#[derive(Debug, Clone)]
pub(crate) struct CacheSession {
    project_manifest: Vec<u8>,
    environment: BTreeMap<String, String>,
}
```

```rust
    pub(crate) fn prepare(
        &self,
        project_root: &Path,
        environment: BTreeMap<String, String>,
    ) -> Result<CacheSession, CacheError> {
        self.ensure_gitignore()?;
        let project_manifest = read_file(&config_path(project_root))?;
        Ok(CacheSession {
            project_manifest,
            environment,
        })
    }
```

```rust
    /// Compatibility helper for tests and callers that key one task outside a run.
    #[cfg(test)]
    pub(crate) fn task_key(
        &self,
        project_root: &Path,
        task: &PlannedTask,
        dependency_keys: &[String],
    ) -> Result<String, CacheError> {
        let session = self.prepare(project_root, BTreeMap::new())?;
        self.task_key_with_session(&session, task, dependency_keys)
    }

    pub(crate) fn task_key_with_session(
        &self,
        session: &CacheSession,
        task: &PlannedTask,
        dependency_keys: &[String],
    ) -> Result<String, CacheError> {
```

and inside it, replace the environment block:

```rust
        let mut environment = BTreeMap::new();
        if task.cache_env().iter().any(|variable| variable == "*") {
            environment.clone_from(&session.environment);
            environment.extend(task.env().clone());
        } else {
            for variable in task.cache_env() {
                environment.insert(
                    variable.clone(),
                    task.env()
                        .get(variable)
                        .cloned()
                        .or_else(|| session.environment.get(variable).cloned())
                        .unwrap_or_else(|| "<unset>".to_owned()),
                );
            }
        }
```

- [ ] **Step 4: Update the callers**

In `src/scheduler.rs`, replace

```rust
    let cache_session =
        if !matches!(cache_mode, CacheMode::NoCache) && plan.iter().any(PlannedTask::cache) {
            Some(Arc::new(
                cache.prepare(&root, plan).map_err(SchedulerError::Cache)?,
            ))
        } else {
            None
        };
```

with

```rust
    // Read the ambient environment once, at the edge of the run. The cache
    // hasher receives it as data, so a cache key never depends on process
    // state it cannot be told about.
    let cache_session =
        if !matches!(cache_mode, CacheMode::NoCache) && plan.iter().any(PlannedTask::cache) {
            let environment = std::env::vars_os()
                .map(|(key, value)| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
                .collect::<BTreeMap<_, _>>();
            Some(Arc::new(
                cache
                    .prepare(&root, environment)
                    .map_err(SchedulerError::Cache)?,
            ))
        } else {
            None
        };
```

and in `execute_task`, replace

```rust
        match cache.task_key_with_session(session.as_ref(), &root, &task, &dependency_keys) {
```

with

```rust
        match cache.task_key_with_session(session.as_ref(), &task, &dependency_keys) {
```

`root` is still used by `runner.run_with_options`, so do not remove it from `WorkerJob`.

Also update the two call sites in `src/cache.rs::tests` at the existing `changing_the_output_limit_changes_the_key` test:

```rust
        let session = store
            .prepare(&project.root, BTreeMap::new())
            .expect("cache session prepares");
        let first = store
            .task_key_with_session(&session, &task, &[])
            .expect("key succeeds");
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib cache::tests::`
Expected: all PASS (17 tests in the module).

Run: `grep -rn "std::env::var" src/`
Expected: exactly one hit, in `src/scheduler.rs`. See the as-built note below for why it must be `vars_os`, not `vars`.

Run: `cargo test --workspace --all-targets --all-features`
Expected: PASS.

**As-built note — a plan bug found in execution, and the fix (required).**

The version of the environment collection originally specified here was `std::env::vars().collect::<BTreeMap<_, _>>()`. That is a regression, and it is subtle enough to be worth stating precisely.

`std::env::vars()` **panics** if any ambient variable is not valid Unicode. The original code only reached it on the wildcard path:

```rust
        if task.cache_env().iter().any(|variable| variable == "*") {
            environment.extend(std::env::vars());          // panics on non-Unicode
        } else {
            for variable in task.cache_env() {
                ...
                        .or_else(|| std::env::var(variable).ok())   // no panic; None
                        .unwrap_or_else(|| "<unset>".to_owned()),
```

Because `prepare` gathers the whole environment eagerly whenever *any* task is cacheable, `vars()` moves that panic onto **every** cached run — a project using only named `cache_env` entries would abort because of an entirely unrelated non-Unicode variable. That is the wrong side of the panic/return line: a non-Unicode var is an expected environmental condition, not a broken invariant.

The fix, now reflected in the snippet above: collect with `std::env::vars_os()` and convert lossily. Neither path panics any more, and a non-Unicode variable still contributes a stable value to the key instead of collapsing to `"<unset>"` (which previously made every non-Unicode value hash identically — a latent cache-correctness bug).

`tests/cli.rs::a_non_unicode_environment_variable_does_not_abort_a_cached_run` pins it: it spawns `mono` with `MONO_TEST_NON_UNICODE` set to invalid UTF-8 and asserts a clean exit. Verified by mutation — reverting to `vars()` makes the child panic at `library/std/src/env.rs:168` and the test fail. The variable is set on the child rather than via `set_var`, which is `unsafe` in Rust 2024 and racy under parallel tests.

- [ ] **Step 6: Commit**

```bash
but diff
but commit -b testability/cache-environment -m "refactor: pass the environment into the cache key instead of reading it" <ids>
```

---

### Task 6: Give `verify_source` a git seam

`release.rs::verify_source` is the only function in the module with no test, because it shells out to `git` directly. Its failure space is four variants and a mismatch check; all of it is reachable with a recorded `git` runner.

**Files:**
- Modify: `src/release.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` in `src/release.rs`:

```rust
    /// `verify_source_with` calls `rev-parse` twice, so a stub keyed only by
    /// subcommand is not enough; these stubs answer from a scripted queue.
    #[test]
    fn a_matching_checkout_tag_and_commit_verify() {
        let mut answers = vec!["abc123".to_owned(), "abc123".to_owned()];
        let mut git = move |args: &[&str]| -> Result<String, ReleaseError> {
            if args[0] == "check-ref-format" {
                return Ok(String::new());
            }
            Ok(answers.remove(0))
        };

        assert!(verify_source_with(&mut git, "v1.0.0", "abc123").is_ok());
    }

    #[test]
    fn a_checkout_that_does_not_match_the_tag_is_rejected() {
        let mut answers = vec!["abc123".to_owned(), "def456".to_owned()];
        let mut git = move |args: &[&str]| -> Result<String, ReleaseError> {
            if args[0] == "check-ref-format" {
                return Ok(String::new());
            }
            Ok(answers.remove(0))
        };

        let error = verify_source_with(&mut git, "v1.0.0", "abc123").unwrap_err();

        assert!(error.to_string().contains("does not match"), "{error}");
    }

    #[test]
    fn a_checkout_that_does_not_match_the_expected_commit_is_rejected() {
        let mut answers = vec!["abc123".to_owned(), "abc123".to_owned()];
        let mut git = move |args: &[&str]| -> Result<String, ReleaseError> {
            if args[0] == "check-ref-format" {
                return Ok(String::new());
            }
            Ok(answers.remove(0))
        };

        let error = verify_source_with(&mut git, "v1.0.0", "other").unwrap_err();

        assert!(error.to_string().contains("does not match"), "{error}");
    }

    #[test]
    fn a_git_failure_propagates_as_a_command_error() {
        let mut git = |args: &[&str]| -> Result<String, ReleaseError> {
            if args[0] == "check-ref-format" {
                return Ok(String::new());
            }
            Err(ReleaseError::Command("git exploded".to_owned()))
        };

        let error = verify_source_with(&mut git, "v1.0.0", "abc123").unwrap_err();

        assert!(matches!(error, ReleaseError::Command(_)), "{error}");
    }

    #[test]
    fn an_empty_or_unsafe_identity_is_rejected_before_running_git() {
        let mut git = |args: &[&str]| -> Result<String, ReleaseError> {
            panic!("git must not run: {args:?}")
        };

        for (tag, commit) in [
            ("", "abc123"),
            ("v1.0.0", ""),
            ("--force", "abc123"),
            ("v1 0 0", "abc123"),
            ("v1.0.0", "abc 123"),
        ] {
            let error = verify_source_with(&mut git, tag, commit).unwrap_err();
            assert!(matches!(error, ReleaseError::Invalid(_)), "{tag}/{commit}: {error}");
        }
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib release::tests::`
Expected: FAIL to compile — `verify_source_with` does not exist.

- [ ] **Step 3: Split the workflow from the subprocess**

In `src/release.rs`, replace `verify_source` with a thin wrapper plus the testable workflow:

```rust
/// Verify that `repository` is checked out at exactly `tag` and `expected_commit`.
pub fn verify_source(
    repository: &Path,
    tag: &str,
    expected_commit: &str,
) -> Result<(), ReleaseError> {
    let mut run = |args: &[&str]| git(repository, args);
    verify_source_with(&mut run, tag, expected_commit)
}

/// The verification workflow, with `run_git` as the only side effect.
///
/// `run_git` receives the argument vector after `git` — for example
/// `["rev-parse", "HEAD"]` — so a test can script the answers without a
/// repository and without spawning a process.
///
/// It is `&mut dyn FnMut`, NOT `&dyn Fn`: the scripted stubs consume a queue of
/// answers, which makes them `FnMut`, and `&dyn Fn` rejects them with `E0525`
/// ("expected a closure that implements the `Fn` trait, but this closure only
/// implements `FnMut`"). Mutating closures also need a `mut` binding so the
/// reborrow as `&mut` is legal.
fn verify_source_with(
    run_git: &mut dyn FnMut(&[&str]) -> Result<String, ReleaseError>,
    tag: &str,
    expected_commit: &str,
) -> Result<(), ReleaseError> {
    if tag.is_empty() || expected_commit.is_empty() {
        return Err(ReleaseError::Invalid(
            "release source verification requires a non-empty tag and commit".to_owned(),
        ));
    }
    if tag.starts_with('-') || tag.chars().any(char::is_whitespace) || tag.contains('\0') {
        return Err(ReleaseError::Invalid(format!(
            "release tag `{tag}` is not a safe Git ref"
        )));
    }
    if expected_commit.chars().any(char::is_whitespace) || expected_commit.contains('\0') {
        return Err(ReleaseError::Invalid(
            "expected release commit is not a safe Git object name".to_owned(),
        ));
    }

    let tag_ref = format!("refs/tags/{tag}");
    run_git(&["check-ref-format", "--allow-onelevel", &tag_ref])?;
    let checkout_commit = run_git(&["rev-parse", "HEAD"])?;
    let tag_commit_ref = format!("{tag_ref}^{{commit}}");
    let tag_commit = run_git(&["rev-parse", "--verify", &tag_commit_ref])?;
    if checkout_commit != tag_commit || checkout_commit != expected_commit {
        return Err(ReleaseError::Invalid(format!(
            "release source does not match: checkout {checkout_commit}, tag {tag_commit}, expected {expected_commit}"
        )));
    }
    Ok(())
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib release::tests::`
Expected: all PASS (13 tests in the module).

Run: `cargo test --workspace --all-targets --all-features`
Expected: PASS, including `tests/release_cli.rs`.

**As-built note.** Two things execution added beyond the snippets above, both kept:

1. **`&mut dyn FnMut`, not `&dyn Fn` (required).** The scripted stubs consume a queue of answers, so they implement `FnMut`; `&dyn Fn` rejects them with `E0525`. The stubs also need `mut` bindings so `&mut git` is a legal reborrow. Both were reproduced with a standalone `rustc` check before this task ran. See Step 3.
2. **The doc comment on `verify_source` was wrong and is now fixed.** It read `/// Verify metadata, checksums, and the exact regular-file inventory.` — a description of `verify_manifest`, not of source verification. It now reads `/// Verify that \`repository\` is checked out at exactly \`tag\` and \`expected_commit\`.`. This is a pre-existing documentation defect, not a behavior change, but it is worth knowing that it was there.

**Residual gap, deliberately not closed:** the seam tests `verify_source`'s workflow logic against scripted `git` answers. It does not test `git()` itself, so nothing in the suite proves the real subprocess call still works end to end. `git()` is 15 lines and untouched by this task; closing the gap would need a `git init` fixture and a machine with `git` installed, which is a separate decision.

- [ ] **Step 5: Commit**

```bash
but diff
but commit -b testability/git-seam -m "test: cover release source verification without a repository" <ids>
```

---

## Phase C — Contract fixes

### Task 7: Split `ProjectError::Io` so exit codes are exact

`ProjectError::Io` currently means both "could not read the manifest" (a tool failure, exit 3) and "a declared `cwd` does not resolve" (a rejected request, exit 1). `main.rs::exit_code` even documents the ambiguity. Split the variant, then map both kinds.

**Files:**
- Modify: `src/project.rs`
- Modify: `src/main.rs`
- Modify: `tests/cli.rs` (nothing to change unless the CLI test asserts an exit code for a missing `cwd`; it does not)

- [ ] **Step 1: Write the failing tests**

In `src/main.rs`, change the `missing_declared_directory` fixture and add the new case:

```rust
    fn missing_declared_directory() -> ProjectError {
        // A task `cwd` that the manifest declares but the worktree lacks.
        ProjectError::TaskDirectory {
            task: "api-test".to_owned(),
            path: PathBuf::from("packages/api/missing"),
            source: io::Error::new(io::ErrorKind::NotFound, "missing"),
        }
    }

    fn unreadable_manifest() -> ProjectError {
        // The manifest itself could not be read: an environment failure.
        ProjectError::Io {
            path: PathBuf::from("mono.toml"),
            source: io::Error::new(io::ErrorKind::PermissionDenied, "denied"),
        }
    }
```

Add to `requests_that_were_understood_fail_with_one`'s array:

```rust
            Error::Ci(CiError::Project(missing_declared_directory())),
            Error::Doctor(DoctorError::Project(missing_declared_directory())),
            Error::List(ListError::Project(missing_declared_directory())),
```

Add to `environment_failures_exit_with_three`'s array:

```rust
            Error::Ci(CiError::Project(unreadable_manifest())),
            Error::Doctor(DoctorError::Project(unreadable_manifest())),
```

In `src/project.rs`, change the Task 2 test:

```rust
    #[test]
    fn a_missing_cwd_directory_is_a_task_directory_failure() {
        let error = reject(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\ncwd = \"missing\"\n",
        );

        assert!(
            matches!(
                error,
                ProjectError::TaskDirectory {
                    ref task,
                    ..
                } if task == "build"
            ),
            "{error}"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib main::tests:: project::tests::`
Expected: FAIL to compile — `ProjectError::TaskDirectory` does not exist.

- [ ] **Step 3: Add the variant and use it**

In `src/project.rs`, add to `enum ProjectError`:

```rust
    /// A `cwd` the manifest declares could not be resolved to a directory
    /// inside the project root. The request was understood and is rejected;
    /// this is not an environment failure.
    TaskDirectory {
        task: String,
        path: PathBuf,
        source: std::io::Error,
    },
```

Add its `Display` arm:

```rust
            Self::TaskDirectory {
                task,
                path,
                source,
            } => write!(
                f,
                "could not resolve task '{task}' directory {}: {source}",
                path.display()
            ),
```

Add its `source` arm:

```rust
            Self::TaskDirectory { source, .. } => Some(source),
```

In `validate_task_config`, change:

```rust
        if !cwd.contains("${") {
            let cwd_path = root.join(cwd);
            let canonical = fs::canonicalize(&cwd_path).map_err(|source| {
                ProjectError::TaskDirectory {
                    task: task_name.to_owned(),
                    path: cwd_path.clone(),
                    source,
                }
            })?;
```

In `planned_task`, change:

```rust
        let cwd_path = fs::canonicalize(self.root.join(&cwd)).map_err(|source| {
            ProjectError::TaskDirectory {
                task: node.id.clone(),
                path: self.root.join(&cwd),
                source,
            }
        })?;
```

- [ ] **Step 4: Map both kinds in `exit_code`**

In `src/main.rs`, replace the body of `exit_code` and add `project_exit_code`:

```rust
    match error {
        Error::Init(error) => init_exit_code(error),
        Error::Ci(CiError::InvalidJobs) => EXIT_USAGE,
        Error::Ci(CiError::Scheduler(error)) => scheduler_exit_code(error),
        Error::Ci(CiError::Json { .. }) => EXIT_TOOL,
        Error::Ci(CiError::Project(error)) => project_exit_code(error),
        Error::Changelog(error) => changelog_exit_code(error),
        Error::Release(ReleaseCommandError::Release(error)) => release_exit_code(error),
        Error::Doctor(DoctorError::Project(error)) => project_exit_code(error),
        Error::List(ListError::Project(error)) => project_exit_code(error),
        Error::Doctor(_) | Error::List(_) => EXIT_FAILED,
    }
}

/// Map a project failure onto the transport's exit codes.
///
/// A manifest that cannot be read is a `mono` or environment failure (`3`).
/// A manifest that was read and rejected — including a `cwd` that does not
/// resolve — is a request the caller can fix (`1`).
fn project_exit_code(error: &mono::ProjectError) -> u8 {
    match error {
        mono::ProjectError::Io { .. } => EXIT_TOOL,
        mono::ProjectError::Parse { .. }
        | mono::ProjectError::MissingRoot { .. }
        | mono::ProjectError::InvalidManifest { .. }
        | mono::ProjectError::InvalidProject { .. }
        | mono::ProjectError::UnknownPipeline { .. }
        | mono::ProjectError::InvalidTaskName { .. }
        | mono::ProjectError::InvalidTask { .. }
        | mono::ProjectError::MissingTask { .. }
        | mono::ProjectError::InvalidTaskReference { .. }
        | mono::ProjectError::TaskCycle { .. }
        | mono::ProjectError::UnsupportedSchema { .. }
        | mono::ProjectError::TaskDirectory { .. } => EXIT_FAILED,
    }
}
```

Remove the now-wrong comment block above the old `Error::Doctor(_) | Error::List(_) | ...` arm.

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib main::tests:: project::tests::`
Expected: all PASS.

Run: `cargo test --workspace --all-targets --all-features`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
but diff
but commit -b fix/project-error-vocabulary -m "fix: separate an unreadable manifest from a rejected task directory" <ids>
```

---

### Task 8: One atomic-write helper

Three copies of "same-directory temp file, `sync_all`, replace" exist with divergent Windows fallbacks: `release.rs::write_file`/`replace_file`, `commands/changelog.rs::write`/`replace_file`, `commands/init.rs::write_config`. Extract one helper with two modes — replace, and create-new — and let each caller map the `io::Error` into its own vocabulary.

**Files:**
- Create: `src/atomic_file.rs`
- Modify: `src/lib.rs` (add `mod atomic_file;`)
- Modify: `src/release.rs`
- Modify: `src/commands/changelog.rs`
- Modify: `src/commands/init.rs`
- Modify: `tests/cli.rs` (add one end-to-end assertion for `init`)

- [ ] **Step 1: Write the failing tests**

Create `src/atomic_file.rs`:

```rust
//! Crash-safe file publication.
//!
//! Both modes write a same-directory temporary file, flush it to disk, and
//! then publish it. `Replace` can overwrite an existing destination; `New`
//! refuses to, and reports `AlreadyExists` so the caller can name that
//! failure in its own vocabulary.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// How the temporary file is published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteMode {
    /// Atomically replace the destination, if it exists.
    Replace,
    /// Create the destination, failing if it already exists.
    New,
}

/// Publish `contents` at `path`.
pub(crate) fn write(path: &Path, contents: &[u8], mode: WriteMode) -> io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("mono");
    let temporary = path.with_file_name(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
    ));

    let result = write_temporary(&temporary, contents).and_then(|()| match mode {
        WriteMode::Replace => replace_file(&temporary, path),
        WriteMode::New => create_new(&temporary, path),
    });
    let _ = fs::remove_file(&temporary);
    result
}

fn write_temporary(temporary: &Path, contents: &[u8]) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temporary)?;
    file.write_all(contents)?;
    file.sync_all()
}

fn replace_file(temporary: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::rename(temporary, destination)
    }

    #[cfg(not(unix))]
    {
        match fs::rename(temporary, destination) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                fs::remove_file(destination)?;
                fs::rename(temporary, destination)
            }
            Err(error) => Err(error),
        }
    }
}

/// Publish without replacing.
///
/// A hard link is the atomic create-if-absent primitive on every platform
/// this crate targets; it fails with `AlreadyExists` when the destination is
/// taken.
fn create_new(temporary: &Path, destination: &Path) -> io::Result<()> {
    fs::hard_link(temporary, destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn replace_writes_a_new_file() {
        let temp = TempDir::new();
        let path = temp.path().join("artifact");

        write(&path, b"content", WriteMode::Replace).expect("replace writes");

        assert_eq!(fs::read_to_string(&path).unwrap(), "content");
    }

    #[test]
    fn replace_overwrites_an_existing_file() {
        let temp = TempDir::new();
        let path = temp.path().join("artifact");
        fs::write(&path, b"before").unwrap();

        write(&path, b"after", WriteMode::Replace).expect("replace overwrites");

        assert_eq!(fs::read_to_string(&path).unwrap(), "after");
    }

    #[test]
    fn new_refuses_to_replace_an_existing_file() {
        let temp = TempDir::new();
        let path = temp.path().join("artifact");
        fs::write(&path, b"before").unwrap();

        let error = write(&path, b"after", WriteMode::New).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&path).unwrap(), "before");
    }

    #[test]
    fn new_writes_when_the_destination_is_absent() {
        let temp = TempDir::new();
        let path = temp.path().join("artifact");

        write(&path, b"content", WriteMode::New).expect("new writes");

        assert_eq!(fs::read_to_string(&path).unwrap(), "content");
    }

    #[test]
    fn no_temporary_file_survives_a_success_or_a_refusal() {
        let temp = TempDir::new();
        let path = temp.path().join("artifact");

        write(&path, b"content", WriteMode::Replace).expect("replace writes");
        let _ = write(&path, b"content", WriteMode::New);

        assert!(
            fs::read_dir(temp.path())
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| !entry.file_name().to_string_lossy().ends_with(".tmp")),
            "a temporary file was left behind"
        );
    }
}
```

- [ ] **Step 2: Register the module and run the tests**

Add `mod atomic_file;` to `src/lib.rs`, next to the other private modules.

Run: `cargo test --lib atomic_file::tests::`
Expected: FAIL to compile until the module is registered; then all 5 tests PASS.

- [ ] **Step 3: Point the three callers at the helper**

In `src/release.rs`, replace the body of `write_file` and delete `replace_file`:

```rust
fn write_file(path: &Path, contents: &str) -> Result<(), ReleaseError> {
    crate::atomic_file::write(
        path,
        contents.as_bytes(),
        crate::atomic_file::WriteMode::Replace,
    )
    .map_err(|source| ReleaseError::Write {
        path: path.to_path_buf(),
        source,
    })
}
```

Delete the now-unused `AtomicU64`/`Ordering` imports if nothing else in the file uses them, and the `std::io::Write` import if it is no longer needed.

In `src/commands/changelog.rs`, replace `write` and delete `replace_file`:

```rust
fn write(path: &Path, contents: &str) -> Result<(), ChangelogError> {
    crate::atomic_file::write(
        path,
        contents.as_bytes(),
        crate::atomic_file::WriteMode::Replace,
    )
    .map_err(|source| ChangelogError::Write {
        path: path.to_path_buf(),
        source,
    })
}
```

In `src/commands/init.rs`, replace the temporary-file machinery in `write_config`:

```rust
fn write_config(dir: &Path, contents: String) -> Result<PathBuf, InitError> {
    fs::create_dir_all(dir).map_err(|source| InitError::CreateDir {
        dir: dir.to_path_buf(),
        source,
    })?;
    let path = config_path(dir);
    crate::atomic_file::write(
        &path,
        contents.as_bytes(),
        crate::atomic_file::WriteMode::New,
    )
    .map_err(|source| match source.kind() {
        io::ErrorKind::AlreadyExists => InitError::AlreadyInitialized(path.clone()),
        _ => InitError::WriteConfig {
            path: path.clone(),
            source,
        },
    })?;
    Ok(path)
}
```

Delete the now-unused imports from `commands/init.rs`: `Write`, `AtomicU64`, `Ordering`, `SystemTime`, `UNIX_EPOCH`. Keep `fs`, `io`, `Path`, `PathBuf`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib`
Expected: PASS, including the existing `commands::changelog::tests::scaffold_replaces_changelog_without_leaving_temporary_files` and `release::tests::replaces_existing_release_metadata_without_leaving_temporary_files`, which now exercise the shared helper.

Run: `cargo test --workspace --all-targets --all-features`
Expected: PASS. `tests/cli.rs::init_creates_a_valid_root_project` and `reinitializing_refuses_to_overwrite` prove the `New` mode still maps `AlreadyExists` to exit 1.

- [ ] **Step 5: Confirm the duplication is gone**

Run: `grep -rn "create_new(true)\|replace_file" src/`
Expected: exactly one `create_new(true)` in `src/atomic_file.rs` and no `replace_file` outside it.

- [ ] **Step 6: Commit**

```bash
but diff
but commit -b refactor/atomic-write -m "refactor: share one atomic write helper across release, changelog, and init" <ids>
```

---

### Task 9: Give `changelog scaffold` a deterministic date

`commands::changelog::scaffold` reads the wall clock via `today()`, so no test can assert the rendered date. `Changelog::scaffold` already takes the date as a parameter; expose that through the command.

**Files:**
- Modify: `src/commands/changelog.rs`
- Modify: `src/lib.rs` (export the new function)

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/commands/changelog.rs`:

```rust
    #[test]
    fn scaffold_on_writes_the_supplied_date() {
        let temp = TempDir::new();
        let path = temp.path().join(DEFAULT_PATH);
        fs::write(&path, "# Changelog\n\n## 1.0.0\nReleased: 2026-01-01\n").unwrap();

        scaffold_on(&path, "1.1.0", "2026-09-11").expect("scaffold succeeds");

        let rendered = fs::read_to_string(&path).unwrap();
        assert!(rendered.contains("## 1.1.0\n"), "{rendered}");
        assert!(rendered.contains("Released: 2026-09-11\n"), "{rendered}");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib commands::changelog::tests::`
Expected: FAIL to compile — `scaffold_on` does not exist.

- [ ] **Step 3: Split the clock out of the command**

In `src/commands/changelog.rs`, replace `scaffold`:

```rust
/// Scaffold an entry dated today.
pub fn scaffold(path: &Path, version: &str) -> Result<String, ChangelogError> {
    scaffold_on(path, version, &today())
}

/// Scaffold an entry with an explicit date.
///
/// The clock is a parameter rather than a call, so the rendered changelog is
/// deterministic and a test can assert the date.
pub fn scaffold_on(path: &Path, version: &str, date: &str) -> Result<String, ChangelogError> {
    let request = Request::parse(version).map_err(ChangelogError::Invalid)?;
    let text = read(path)?;
    let mut changelog = Changelog::parse(&text).map_err(|message| invalid(path, message))?;
    let action = changelog
        .scaffold(request, date, &[])
        .map_err(|message| invalid(path, message))?;
    write(path, &changelog.render())?;
    Ok(format!(
        "scaffolded {} in {}",
        action_name(&action),
        path.display()
    ))
}
```

In `src/lib.rs`, extend the export:

```rust
pub use commands::changelog::{
    ChangelogError, DEFAULT_NOTES_PATH as DEFAULT_RELEASE_NOTES_PATH,
    DEFAULT_PATH as DEFAULT_CHANGELOG_PATH, notes as changelog_notes,
    scaffold as changelog_scaffold, scaffold_on as changelog_scaffold_on,
    validate as changelog_validate,
};
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib commands::changelog::tests::`
Expected: all 5 tests PASS.

Run: `cargo test --workspace --all-targets --all-features`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
but diff
but commit -b testability/changelog-clock -m "refactor: take the changelog date as a parameter" <ids>
```

---

### Task 10: Build every JSON document with a typed struct

`main.rs::run_check` hand-builds JSON with `format!` and an `.expect("path is serializable")`, while `success_document`/`emit_error` use `serde_json::json!` and `plan`/`list` use typed structs. The versioned contract should be assembled one way. The keys and values do not change — only how they are produced.

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/main.rs`:

```rust
    #[test]
    fn success_documents_have_the_documented_shape() {
        let document = success_document(OutputMode::Json, "cache_clean", "removed cache".to_owned());
        let value: serde_json::Value = serde_json::from_str(&document).expect("valid JSON");

        assert_eq!(value["schema"], mono::JSON_OUTPUT_SCHEMA);
        assert_eq!(value["kind"], "cache_clean");
        assert_eq!(value["status"], "ok");
        assert_eq!(value["message"], "removed cache");
    }

    #[test]
    fn a_check_document_reports_the_project_root() {
        let document = check_document(OutputMode::Json, Path::new("/workspace"));
        let value: serde_json::Value = serde_json::from_str(&document).expect("valid JSON");

        assert_eq!(value["schema"], mono::JSON_OUTPUT_SCHEMA);
        assert_eq!(value["kind"], "check");
        assert_eq!(value["status"], "ok");
        assert_eq!(value["project"], "/workspace");
    }

    #[test]
    fn error_documents_have_the_documented_shape() {
        let error = Error::Ci(CiError::InvalidJobs);
        let mut sink = Vec::new();

        emit_error(&mut sink, &error, OutputMode::Json);

        let value: serde_json::Value =
            serde_json::from_slice(&sink).expect("valid JSON");
        assert_eq!(value["schema"], mono::JSON_OUTPUT_SCHEMA);
        assert_eq!(value["kind"], "error");
        assert_eq!(value["code"], EXIT_USAGE);
        assert!(value["message"].as_str().unwrap().contains("--jobs"));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib main::tests::`
Expected: FAIL to compile — `check_document` does not exist.

- [ ] **Step 3: Replace hand-built JSON with structs**

In `src/main.rs`, add the document types near `success_document`:

```rust
/// The success document every non-execution command returns in JSON mode.
#[derive(serde::Serialize)]
struct SuccessDocument {
    schema: u32,
    kind: &'static str,
    status: &'static str,
    message: String,
}

/// The `check` document, which reports the resolved project root.
#[derive(serde::Serialize)]
struct CheckDocument {
    schema: u32,
    kind: &'static str,
    status: &'static str,
    project: String,
}

/// The failure document for JSON mode.
#[derive(serde::Serialize)]
struct ErrorDocument<'a> {
    schema: u32,
    kind: &'static str,
    code: u8,
    message: &'a str,
}
```

Replace `success_document` and `run_check`'s JSON branch:

```rust
fn success_document(output: OutputMode, kind: &'static str, message: String) -> String {
    if output == OutputMode::Json {
        serialize(&SuccessDocument {
            schema: mono::JSON_OUTPUT_SCHEMA,
            kind,
            status: "ok",
            message,
        })
    } else {
        message
    }
}

fn check_document(output: OutputMode, root: &Path) -> String {
    if output == OutputMode::Json {
        serialize(&CheckDocument {
            schema: mono::JSON_OUTPUT_SCHEMA,
            kind: "check",
            status: "ok",
            project: root.display().to_string(),
        })
    } else {
        format!("checked {}", root.display())
    }
}
```

Change `run_check` to:

```rust
fn run_check(root: &Path, output: OutputMode) -> Result<String, Error> {
    let project = mono::Project::load(root).map_err(mono::DoctorError::from)?;
    Ok(check_document(output, &project.root))
}
```

Replace `emit_error`'s JSON branch:

```rust
fn emit_error(sink: &mut impl Write, error: &Error, output: OutputMode) -> u8 {
    let code = exit_code(error);
    if output == OutputMode::Json {
        let _ = sink.write_all(
            serialize(&ErrorDocument {
                schema: mono::JSON_OUTPUT_SCHEMA,
                kind: "error",
                code,
                message: &error.to_string(),
            })
            .as_bytes(),
        );
        let _ = sink.write_all(b"\n");
    } else {
        let _ = writeln!(sink, "mono: {error}");
    }
    code
}
```

Add the serializer helper:

```rust
/// Serialize a document that contains only serializable fields.
fn serialize(document: &impl serde::Serialize) -> String {
    serde_json::to_string(document).expect("output documents contain only serializable fields")
}
```

`run_init`, `run_cache`, and `run_changelog`/`run_release` pass `&'static str` kinds already, so no change is needed beyond the `success_document` signature.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib main::tests::`
Expected: all 16 tests PASS.

Run: `cargo test --workspace --all-targets --all-features`
Expected: PASS. `tests/cli.rs::plan_json_is_a_stable_machine_document`, `list_json_contains_pipelines_and_tasks`, `json_success_documents_cover_non_execution_commands`, and `json_errors_are_documents_on_stdout` all parse the documents, so key order is not asserted — only key presence and value, which is unchanged.

- [ ] **Step 5: Confirm no hand-built JSON remains**

Run: `grep -n "serde_json::json!\|format!(\"{{\\\"schema\\\"" src/main.rs`
Expected: no hits.

- [ ] **Step 6: Commit**

```bash
but diff
but commit -b refactor/typed-json-documents -m "refactor: build CLI JSON documents from typed structs" <ids>
```

---

## Phase D — Cleanup

### Task 11: Delete dead code and dead parameters

Four `#[allow(dead_code)]` markers and two self-documenting-as-ignored parameters. Removing them is safe: the compiler proves it.

**Files:**
- Modify: `src/output.rs`
- Modify: `src/process.rs`
- Modify: `src/runner.rs`
- Modify: `src/cache.rs` (already covered by Task 5, but re-verify)

- [ ] **Step 1: Remove each item and prove nothing uses it**

Delete `ManagedChild::child` from `src/process.rs`, including its `#[allow(dead_code)]` attribute and its doc comment.

Change `Runner::run` in `src/runner.rs` to be explicitly test-only:

```rust
    /// Execute one planned task and return its output and timing.
    ///
    /// A convenience for tests and for callers that want one task without a
    /// plan; the scheduler always uses [`run_with_options`](Self::run_with_options).
    #[cfg(test)]
    pub fn run(
        &self,
        project_root: &Path,
        planned: &PlannedTask,
    ) -> Result<TaskResult, RunnerError> {
        self.run_with_options(project_root, planned, None, None)
    }
```

In `src/output.rs`, `present_summary` and `write_status` were already deleted in Task 3. Verify with:

Run: `grep -rn "allow(dead_code)" src/`
Expected: no hits.

- [ ] **Step 2: Run the tests**

Run: `cargo test --workspace --all-targets --all-features`
Expected: PASS.

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`
Expected: clean. A `dead_code` warning here means the item was still reachable and the removal was wrong — restore it and read the call sites before retrying.

- [ ] **Step 3: Commit**

```bash
but diff
but commit -b chore/remove-dead-code -m "chore: remove unreachable helpers and make test-only code explicit" <ids>
```

---

### Task 12: Split `project.rs` and `cache.rs` by responsibility

Both files carry three unrelated concerns. This is mechanical — move code, fix `use` paths, keep every test passing. Do not change any behavior.

**Files:**
- Create: `src/project/mod.rs`, `src/project/matrix.rs`, `src/project/validate.rs`, `src/project/suggest.rs`, `src/project/tests.rs`
- Create: `src/cache/mod.rs`, `src/cache/pattern.rs`, `src/cache/hash.rs`
- Modify: `src/lib.rs`
- Delete: `src/project.rs`, `src/cache.rs`

- [ ] **Step 1: Split `project.rs`**

Move the declarations from `src/project.rs` into `src/project/mod.rs`, and:

- `src/project/matrix.rs` — `base_task_name`, `task_dimensions`, `validate_matrix_instance`, `format_task_instance`, `matrix_instances`, `interpolate_value`. Make each `pub(super)`, and import `std::collections::BTreeMap`.
- `src/project/validate.rs` — `validate_schema`, `validate_task_config`, `validate_identifier`, `validate_task_reference`, `valid_relative_path`, `validate_cache_pattern`. Make the cross-module ones `pub(super)`.
- `src/project/suggest.rs` — `closest_name`, `edit_distance`. Make `closest_name` `pub(super)`.
- `src/project/tests.rs` — the entire `#[cfg(test)] mod tests` block from Task 1 and Task 2, re-labelled as a module (`use super::*;` becomes `use crate::project::*;` plus the explicit imports it already has).

In `src/project/mod.rs`, add:

```rust
mod matrix;
mod suggest;
#[cfg(test)]
mod tests;
mod validate;

use matrix::{
    base_task_name, format_task_instance, interpolate_value, matrix_instances, task_dimensions,
    validate_matrix_instance,
};
use suggest::closest_name;
use validate::{validate_cache_pattern, validate_identifier, validate_task_config};
pub(crate) use validate::validate_schema;
```

Keep `Project`, `PlannedTask`, `TaskNode`, `ProjectError`, and `VisitState` in `mod.rs`, since they are the module's interface.

- [ ] **Step 2: Split `cache.rs`**

- `src/cache/mod.rs` — `CacheMode`, `CacheStore`, `CacheSession`, `CacheMetadata`, `CachedOutput`, `CacheError`, and the `#[cfg(test)] mod tests`. Add `mod hash; mod pattern;` and explicit `use` lines.
- `src/cache/pattern.rs` — `CollectedPaths`, `CachePatterns`, `CachePattern`, `collect_files`, `walk_matched`, `prefix_matches`, `match_segments`, `match_path_segments`, `segment_matches`, `relative_path`. Make `CachePatterns` and `collect_files` `pub(super)`.
- `src/cache/hash.rs` — `hash_strings`, `hash_string`, `hash_file`, `file_digest`, `read_file`, `hash_bytes`, `hex_digest`, `file_mode`, `set_mode`, `validate_cached_path`, `ensure_inside`, `ensure_no_symlink_components`, and the `HASH_BUFFER_SIZE`/`HEX_DIGITS` constants. Make the cross-module ones `pub(super)`.

Keep `CacheStore::lookup`/`store`/`store_in`/`entry_path`/`ensure_gitignore` in `mod.rs` — they are the public store behavior — and have them call `pattern::collect_files` and `hash::*`.

- [ ] **Step 3: Update `lib.rs`**

`src/lib.rs` already declares `mod cache;` and `mod project;`, which now resolve to the directories. No change needed unless a path was declared as `mod cache` with an explicit file; verify with `cargo check`.

- [ ] **Step 4: Prove it is a pure move**

Run: `cargo test --workspace --all-targets --all-features`
Expected: PASS, same test count as before the split.

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`
Expected: clean.

Run: `cargo fmt --check`
Expected: clean. Run `cargo fmt` first if the moves reflow lines.

- [ ] **Step 5: Commit**

```bash
but diff
but commit -b chore/split-modules -m "chore: split project and cache into responsibility-focused modules" <ids>
```

---

## Self-Review

**Spec coverage.** Each finding has a task: F1→1,2; F2→4; F3→3; F4→5; F5→6; F6→7; F7→8; F8→9; F9→10; F10→11. The deferred items from the Analysis section ("Deferred, with reasons") have no task by design and are listed there with why.

**Placeholder scan.** Every code step contains the code to write. Every test step names the exact filter (`cargo test --lib project::tests::`). No "add error handling" / "write tests for the above" steps remain, and no step refers to code that another step was supposed to have deleted.

**Type consistency across tasks.** `OutputSink::with_writers`, `OutputSink::test_sink`, `SharedWriter`, `captured`, `text`, `result` are introduced in Task 3 and used only there. `fn captured(...)` in Task 3's test module shadows nothing else — the old `json_lines` fixtures are replaced wholesale. `classify`, `finalizers_allowed`, `nodes`, `planned`, `task_map`, `succeeded` are introduced in Task 4 and used only in `scheduler::tests`. `TaskStatus` gains `PartialEq, Eq` in Task 4 and is asserted with `assert_eq!` in the same task. `key_with_environment` and `project_with_cache_env` are introduced and used in Task 5 only. `verify_source_with` is introduced and used in Task 6 only. `ProjectError::TaskDirectory` is introduced and matched in Task 7, and the Task 2 test that pins it is updated in the same task. `WriteMode` and `atomic_file::write` are introduced and used in Task 8 only. `scaffold_on` is introduced in Task 9 and exported as `changelog_scaffold_on`. `SuccessDocument`/`CheckDocument`/`ErrorDocument`/`serialize`/`check_document` are introduced and used in Task 10 only.

**Ordering dependencies.** Task 4 modifies `present_failure`, which Task 3 rewrote — do Task 3 first. Task 5 renames `prepare`/`task_key_with_session`, which Task 11 re-verifies — do Task 5 first. Task 8's `write` helper replaces code that Task 3 does not touch. Task 12 moves the tests added by Tasks 1, 2, and 5, so it must come after them.

**Known follow-ups not in this plan.** A true `SchedulerState` extraction, converting `SchedulerError::UnresolvedDependency`/`NoReadyWork` into panics, pinning `run_id` in production for reproducible JSON, removing the duplicate `PlannedTask::id()`/`task()` accessors, and the per-dispatch `finalizers_allowed` scan.

One more inconsistency worth recording: `CiError::Json` maps to exit `3` (`EXIT_TOOL`) in `main.rs`, but `ListError::Json` — the same kind of serde serialization failure — falls through to `EXIT_FAILED` (`1`). Task 7 preserves that existing behavior deliberately rather than widening its scope; a follow-up should give both variants one classification.

---

## Out of Scope

- Publishing, registries, package managers, language detection — deliberate non-goals of the tool.
- The GitHub Actions workflows under `.github/workflows/` and `xtask`.
- The deleted `examples/` and `docs/json-contract.md`/`docs/release-conventions.md` in the working tree. If those deletions were accidental, restore them before Task 12, which does not touch either path.
