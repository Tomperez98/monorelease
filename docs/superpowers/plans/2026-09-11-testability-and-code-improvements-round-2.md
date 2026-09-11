# Testability and Code Improvements — Round 2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Finish making `mono`'s execution core testable — give the scheduler loop and the runner poll loop a seam and a direct test, pin the machine contracts that currently have none, and delete the duplication and dead renderer paths they expose.

**Architecture:** Keep the existing shape: `main.rs` is the only transport edge, each command owns one error vocabulary, `lib.rs` composes them, and pure decisions are extracted from side-effecting drivers. This round follows the same three move types as round 1: (a) add a seam so a side effect becomes injectable, (b) add tests to things that already exist, (c) delete duplication and unreachable code. No new dependencies.

**Tech Stack:** Rust 2024, Clap, Serde/JSON, TOML, `sha2`, platform process APIs, `ctrlc`.

---

## Analysis

Baseline is green. `cargo test --workspace --all-targets --all-features` → **163 tests pass** (121 lib, 16 bin, 21 `tests/cli.rs`, 5 `tests/release_cli.rs`); `cargo clippy --workspace --all-targets --all-features -- -D warnings` and `cargo fmt --check` are clean.

Round 1 (`docs/superpowers/plans/2026-09-11-testability-and-code-improvements.md`, currently deleted in the GitButler working tree but present in `HEAD`) landed successfully: project/cache modules were split, `OutputSink` writers are injected, the cache environment is a parameter, `verify_source` has a git closure, `ProjectError::TaskDirectory` split `Io`, `atomic_file` is the one write helper, `scaffold_on` takes the date, JSON is built from typed structs, and dead parameters are gone. This plan is the follow-up it deferred.

### What is already right (do not touch)

- **Parse at the edge.** `main.rs` validates every argument in `clap`; the library only sees well-formed commands.
- **One error vocabulary per boundary.** Each command owns an error enum; `lib::Error` unions them; `main.rs::exit_code` is the single transport mapping.
- **Fail-fast invariants.** `cache_mode`'s assert, `Runner::run_with_options`'s five asserts, `Project::assert_invariants`, `execute_plan`'s worker-limit and accounting asserts, the `matrix_size > 1024` bound, and the `..`/absolute-path rejections in `validate_cache_pattern`, `valid_relative_path`, `validate_cached_path`.
- **Determinism where it counts.** `format_plan`, `format_command`, the `BTree*` collections, and the ordered pattern matcher are deterministic and already covered.
- **Pure decisions already extracted.** `scheduler::classify`, `next_ready`, `finalizers_allowed`, `has_ready_finalizer`, and `RunnerError::status` are tested directly.

### Findings, worst first

| # | Violation (REFACTOR.md checklist) | Where | Why it matters |
| --- | --- | --- | --- |
| F1 | **No seam around the worker** | `scheduler.rs::execute_plan` takes `runner: &Runner`; `WorkerJob` holds a concrete `Runner` | The single most complex loop in the crate (dispatch, resource groups, finalizer gating, first-error precedence, cancellation, cache/output failure) has **no end-to-end unit test**. Its asserts can never fire in a test, and every failure path is reachable only through a subprocess. |
| F2 | **IO interleaved with logic** | `runner.rs` poll loop (the `let status = loop { … }` block) | Cancellation, output-limit, timeout, child-exit, and wait-error are five interleaved `if`/`match` branches over live `Instant`/`Child` state. Only subprocess tests reach them, and they are timing-sensitive (`sleep`, 50 ms poll start, 100 ms SIGTERM grace). The decision table is not testable. |
| F3 | **Machine contract pinned nowhere** | `events.rs` | `TaskStatus`'s snake_case tokens and `ExecutionEvent`'s `"event"` tag are the versioned JSON contract CI consumes, but no test asserts them directly. A stray `#[serde(rename)]` compiles and silently changes the contract. |
| F4 | **Zero direct tests** | `discovery.rs::find_root` / `read_manifest` | The upward walk, the file-vs-directory start, and `MissingRoot` are only reached incidentally through `Project::load`. |
| F5 | **Duplicated logic + unreachable branch** | `output.rs` terminal/GitHub `RunFinished` arms vs `commands/ci.rs::format_summary` | The run-summary string is written twice (`output.rs:311`, `output.rs:405`, `ci.rs:293`). The two renderer arms are **unreachable**: `present_run_finished` emits `RunFinished` only in `Json` mode, so terminal/GitHub/Live summary rendering is dead code that can drift from the live one. |
| F6 | **Dead/untested public API** | `release.rs::verify_checksums` | Exported from `lib.rs`, never called anywhere in the crate or tests. The legacy `SHA256SUMS` parsing errors (`read_checksums`) have no test at all. |
| F7 | **Untested clock math** | `changelog.rs::civil_from_days` / `today` | Calendar math is pure but private and unasserted; a leap-year regression could not fail a test. |
| F8 | **Untested command + error contract** | `commands/init.rs` (no `mod tests`) | `InitError`'s three variants have no direct test; `Display`/`source` are unverified. |
| F9 | **Fault model unverified** | every error enum (`ProjectError`, `RunnerError`, `SchedulerError`, `CacheError`, `ChangelogError`, `InitError`, `CiError`, `ListError`, `ReleaseError`, `lib::Error`) | Which variants carry a `source()` is part of "one error vocabulary per boundary", but the impls are only incidentally exercised. `Into<String>`/`Display` for most variants is unasserted. |
| F10 | **Inconsistent exit classification** | `main.rs::exit_code` | `CiError::Json` maps to `EXIT_TOOL` (3) but the identical `ListError::Json` serialization failure falls through `Error::List(_) => EXIT_FAILED` (1). |
| F11 | **Accessor duplication** | `PlannedTask::id()` and `PlannedTask::task()` both return `&self.id` | Two names for one thing across 39 call sites; the mental model is split. |

### Findings explicitly deferred (with reasons)

- **Extracting a `Project::from_config(root, config)` seam.** Real — it would name the validation phases and let tests build a `MonoConfig` value instead of a TOML string — but `assert_invariants` requires `root.is_dir()` and `validate_task_config` canonicalizes `cwd`, so it does not remove the `TempDir` from any test. Low payoff; revisit only when the manifest gains a second consumer.
- **Converting `SchedulerError::UnresolvedDependency` / `NoReadyWork` into panics.** Both are broken invariants on a validated plan, but they are public variants of an exported enum and this is a pre-1.0 breaking change. Round 1 deferred this; keep deferring until a 0.2 breaking window.
- **The `O(n)` `finalizers_allowed` scan per dispatch.** Real control work in the loop, but `n` is the task count (hundreds at most) and there is no profile. Leave it.
- **Pinning `run_id` in production JSON.** `run_identity()` mixes wall clock and PID so the stream is unique per run; reproducible golden files would need an env override. No consumer needs it today.
- **Direct `process.rs` / `ManagedChild` tests (including `Drop`).** The behaviour is exercised end-to-end by runner tests; a direct test would need to spawn and signal real children, which is exactly what those tests already do.
- **A deterministic `--jobs > 1` overlap test.** Asserting real interleaving requires sleeps or barriers and reintroduces flakiness. The loop's `assert!(active.len() <= jobs)` is the guard; test serialization with `jobs = 1`.

### Sequencing

Tasks 1–5 add tests to things that already exist and change no behavior — pure upside, do them first. Tasks 6–8 delete duplication and dead paths and fix a classification. Task 9 adds the runner's decision seam. Task 10 is the largest change (the scheduler executor seam) and depends on nothing else; do it last so the contract and cleanup work is banked first.

---

## Conventions

- **Branch:** one branch per task, named `testability/<task-slug>`, unless a task says otherwise.
- **TDD loop:** write the failing test, run it and read the failure, make it pass, run the focused tests plus `cargo test --workspace --all-targets --all-features`.
- **Commits:** this repository is a GitButler workspace, so use `but`, never `git add`/`git commit`. For each commit step: run `but diff`, copy the file/hunk IDs, then `but commit -b <branch> -m "<message>" <ids>`.
- **Focused test commands:** library unit tests → `cargo test --lib <module>::tests::`; binary unit tests (`main.rs`) → `cargo test --bin mono`; integration → `cargo test --test cli`.
- **rustfmt owns the final layout.** The code blocks below are written for clarity and are **not** guaranteed rustfmt-stable. After transcribing any block, run `cargo fmt` (not just `--check`) and accept rustfmt's reflowing. Every task's validation includes `cargo fmt --check`.
- **Do not** add dependencies.
- **Working-tree note:** round 1's plan, `docs/json-contract.md`, `docs/release-conventions.md`, and `examples/` are deleted in the GitButler working tree. Those deletions are not this plan's concern and none of these tasks touches those paths. Do not restore them as part of this plan.

---

## Phase A — Lock the contracts that already exist (no behavior change)

### Task 1: Pin the execution-event JSON contract

`events.rs` defines the versioned, newline-delimited contract the scheduler and every CI consumer read. Only `output.rs` tests parse JSON; nothing asserts the token strings.

**Files:**
- Modify: `src/events.rs` (append a `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing tests**

Append to `src/events.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// These strings are the machine contract a CI consumer reads. Renaming a
    /// variant must break this test, not silently change the JSON stream.
    #[test]
    fn task_status_serializes_to_its_documented_token() {
        for (status, token) in [
            (TaskStatus::Completed, "completed"),
            (TaskStatus::Cached, "cached"),
            (TaskStatus::Failed, "failed"),
            (TaskStatus::TimedOut, "timed_out"),
            (TaskStatus::OutputLimit, "output_limit"),
            (TaskStatus::Cancelled, "cancelled"),
            (TaskStatus::Blocked, "blocked"),
        ] {
            assert_eq!(
                serde_json::to_value(status).expect("status serializes"),
                serde_json::json!(token)
            );
        }
    }

    #[test]
    fn task_stream_serializes_to_its_documented_token() {
        assert_eq!(
            serde_json::to_value(TaskStream::Stdout).unwrap(),
            serde_json::json!("stdout")
        );
        assert_eq!(
            serde_json::to_value(TaskStream::Stderr).unwrap(),
            serde_json::json!("stderr")
        );
    }

    #[test]
    fn every_event_carries_its_name_tag_and_schema() {
        let node = TaskNode::new("build");
        let events = [
            (
                ExecutionEvent::run_started(PathBuf::from("/workspace"), 2),
                "run_started",
            ),
            (ExecutionEvent::task_started(&node), "task_started"),
            (
                ExecutionEvent::task_output(&node, TaskStream::Stdout, b"hi".to_vec()),
                "task_output",
            ),
            (
                ExecutionEvent::task_attempt_started(&node, 2, 3),
                "task_attempt_started",
            ),
            (
                ExecutionEvent::task_finished(&node, TaskStatus::Completed, Duration::ZERO),
                "task_finished",
            ),
            (ExecutionEvent::run_finished(1, 0, 0, 0, 0), "run_finished"),
        ];

        for (event, name) in events {
            let value = serde_json::to_value(&event).expect("event serializes");
            assert_eq!(value["event"], name, "{value}");
            assert_eq!(value["schema"], EXECUTION_EVENT_SCHEMA, "{value}");
        }
    }

    #[test]
    fn status_labels_are_distinct_and_non_empty() {
        let labels = [
            TaskStatus::Completed,
            TaskStatus::Cached,
            TaskStatus::Failed,
            TaskStatus::TimedOut,
            TaskStatus::OutputLimit,
            TaskStatus::Cancelled,
            TaskStatus::Blocked,
        ]
        .map(TaskStatus::label);

        for label in labels {
            assert!(!label.is_empty());
        }
        let mut sorted = labels.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len(), "two statuses share a label");
    }
}
```

- [ ] **Step 2: Run the tests to verify they pass**

Run: `cargo test --lib events::tests::`
Expected: 4 tests PASS. (There is no implementation change: this task pins current behavior. If any fails, the test encodes the wrong expectation — fix the test to match the code and note it.)

- [ ] **Step 3: Confirm no production code changed and commit**

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings` → clean.
Run: `cargo fmt`, then `cargo fmt --check` → clean.

```bash
but diff
but commit -b testability/events-contract -m "test: pin the execution event JSON contract" <ids>
```

---

### Task 2: Unit-test root discovery

`discovery.rs` has no test module. Every `Project::load` test exercises it incidentally, but the walk, the file start, and `MissingRoot` are never asserted.

**Files:**
- Modify: `src/discovery.rs` (append a `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing tests**

Append to `src/discovery.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;
    use std::fs;

    fn write_root(temp: &TempDir) {
        fs::write(
            config_path(temp.path()),
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n",
        )
        .expect("write root manifest");
    }

    #[test]
    fn a_nested_directory_finds_the_ancestor_manifest() {
        let temp = TempDir::new();
        write_root(&temp);
        let nested = temp.path().join("services/api");
        fs::create_dir_all(&nested).expect("create nested directory");

        let discovered = find_root(&nested).expect("root is found");

        assert_eq!(discovered.config.project.name, "fixture");
        assert_eq!(
            discovered.root,
            fs::canonicalize(temp.path()).expect("temp path canonicalizes")
        );
    }

    #[test]
    fn a_file_start_walks_up_from_its_parent() {
        let temp = TempDir::new();
        write_root(&temp);
        let file = temp.path().join("services.txt");
        fs::write(&file, "content").expect("write file");

        let discovered = find_root(&file).expect("root is found from a file");

        assert_eq!(
            discovered.root,
            fs::canonicalize(temp.path()).expect("temp path canonicalizes")
        );
    }

    #[test]
    fn a_start_with_no_ancestor_manifest_is_the_missing_root_failure() {
        let temp = TempDir::new();
        // A fresh temp dir has no ancestor manifest it owns; walk to the
        // filesystem root. `/tmp` is not a project root in the test sandbox, so
        // the walk fails rather than finding an unrelated one.
        let error = match find_root(temp.path()) {
            Ok(discovered) => {
                // A developer machine could legitimately have a manifest above
                // the temp dir. Skip rather than assert a wrong invariant.
                eprintln!("skipped: found {} above temp dir", discovered.root.display());
                return;
            }
            Err(error) => error,
        };

        assert!(
            matches!(error, ProjectError::MissingRoot { .. }),
            "{error}"
        );
    }

    #[test]
    fn an_unsupported_schema_is_rejected_while_discovering() {
        let temp = TempDir::new();
        fs::write(
            config_path(temp.path()),
            "schema = 2\n\n[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n",
        )
        .expect("write manifest");

        let error = find_root(temp.path()).expect_err("schema 2 is rejected");

        assert!(
            matches!(error, ProjectError::UnsupportedSchema { found: 2, .. }),
            "{error}"
        );
    }

    #[test]
    fn unparseable_manifest_contents_are_a_parse_failure() {
        let temp = TempDir::new();
        fs::write(config_path(temp.path()), "this is not toml = = =").expect("write manifest");

        let error = read_manifest(&config_path(temp.path())).expect_err("parse fails");

        assert!(matches!(error, ProjectError::Parse { .. }), "{error}");
    }
}
```

- [ ] **Step 2: Run the tests**

Run: `cargo test --lib discovery::tests::`
Expected: 5 tests PASS (the missing-root test passes either by asserting `MissingRoot` or by taking the skip branch).

- [ ] **Step 3: Confirm no production code changed and commit**

Run: `cargo fmt`, `cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`.

```bash
but diff
but commit -b testability/discovery-tests -m "test: cover root manifest discovery" <ids>
```

---

### Task 3: Make the changelog clock arithmetic assertable

`today()` reads the wall clock; `civil_from_days` is the pure calendar conversion underneath it and has no test. Split the clock read from the conversion so the conversion can be pinned.

**Files:**
- Modify: `src/changelog.rs`

- [ ] **Step 1: Write the failing tests**

Append inside the existing `#[cfg(test)] mod tests` in `src/changelog.rs`:

```rust
    #[test]
    fn epoch_and_known_dates_convert_exactly() {
        // 0, 86_400, 951_782_400 (2000-02-29), and 1_789_084_800 (2026-09-11)
        // are checked against `date -u -r <seconds> +%Y-%m-%d`.
        assert_eq!(date_from_unix_seconds(0), "1970-01-01");
        assert_eq!(date_from_unix_seconds(86_400), "1970-01-02");
        assert_eq!(date_from_unix_seconds(951_782_400), "2000-02-29");
        assert_eq!(date_from_unix_seconds(1_789_084_800), "2026-09-11");
    }

    #[test]
    fn leap_day_rules_are_gregorian() {
        assert_eq!(date_from_unix_seconds(1_709_164_800), "2024-02-29");
        // 1900 is not a leap year under the Gregorian rule; 2000 is. Verify the
        // century rule through the pure days conversion.
        assert_eq!(civil_from_days(-25_508), (1900, 3, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib changelog::tests::`
Expected: FAIL with "cannot find function `date_from_unix_seconds`".

- [ ] **Step 3: Split the clock from the conversion**

In `src/changelog.rs`, replace the body of `today` and add the pure function:

```rust
/// Today's UTC date in `yyyy-mm-dd` form.
pub fn today() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    date_from_unix_seconds(seconds)
}

/// The UTC date `seconds` after the Unix epoch, in `yyyy-mm-dd` form.
///
/// Pure: the wall clock is read once, in `today`, so the calendar conversion
/// can be asserted without a clock.
fn date_from_unix_seconds(seconds: u64) -> String {
    let (year, month, day) = civil_from_days((seconds / 86_400) as i64);
    format!("{year:04}-{month:02}-{day:02}")
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib changelog::tests::`
Expected: PASS.

- [ ] **Step 5: Commit**

Run: `cargo fmt`, `cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`.

```bash
but diff
but commit -b testability/changelog-date-math -m "refactor: make the changelog date conversion pure and tested" <ids>
```

---

### Task 4: Unit-test `commands::init` and its error contract

`commands/init.rs` has no test module; only the CLI integration test covers `AlreadyInitialized`.

**Files:**
- Modify: `src/commands/init.rs` (append a `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing tests**

Append to `src/commands/init.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn init_writes_a_manifest_that_parses_back() {
        let temp = TempDir::new();

        let written = init(temp.path()).expect("init succeeds");

        assert_eq!(written, config_path(temp.path()));
        let contents = fs::read_to_string(&written).expect("manifest is written");
        let config = MonoConfig::parse(&contents).expect("written manifest parses");
        assert_eq!(config, MonoConfig::template());
    }

    #[test]
    fn init_creates_a_missing_directory() {
        let temp = TempDir::new();
        let nested = temp.path().join("a/b/c");

        let written = init(&nested).expect("init creates the directory");

        assert!(written.is_file(), "{}", written.display());
    }

    #[test]
    fn a_second_init_refuses_to_overwrite() {
        let temp = TempDir::new();
        init(temp.path()).expect("first init succeeds");

        let error = init(temp.path()).expect_err("second init refuses");

        assert!(
            matches!(error, InitError::AlreadyInitialized(ref path) if path == &config_path(temp.path())),
            "{error}"
        );
        assert!(error.to_string().contains("refusing to overwrite"), "{error}");
        assert!(error.source().is_none(), "a refusal has no underlying cause");
    }

    #[test]
    fn a_directory_that_cannot_be_created_is_a_tool_failure() {
        let temp = TempDir::new();
        // A file where the directory should be makes `create_dir_all` fail.
        let blocked = temp.path().join("blocked");
        fs::write(&blocked, "not a directory").expect("write blocker file");

        let error = init(&blocked.join("nested")).expect_err("init fails");

        assert!(matches!(error, InitError::CreateDir { .. }), "{error}");
        assert!(error.source().is_some(), "a filesystem cause is exposed");
        assert!(error.to_string().contains("could not create directory"));
    }
}
```

- [ ] **Step 2: Run the tests**

Run: `cargo test --lib commands::init::tests::`
Expected: 4 tests PASS.

- [ ] **Step 3: Commit**

Run: `cargo fmt`, `cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`.

```bash
but diff
but commit -b testability/init-tests -m "test: cover init and its error contract" <ids>
```

---

### Task 5: Pin the fault model of every error vocabulary

"One error vocabulary per boundary" (WRITE.md #5) is only half true if `source()` is not asserted: a caller distinguishes a wrapped cause from a bare refusal by that method. Add one table test per error enum that owns a `source()` impl.

**Files:**
- Modify: `src/project/tests.rs` (add a `ProjectError` fault-model test)
- Modify: `src/runner.rs` (add a `RunnerError` fault-model test)
- Modify: `src/scheduler.rs` (add a `SchedulerError` fault-model test)
- Modify: `src/cache/mod.rs` (add a `CacheError` fault-model test)
- Modify: `src/changelog.rs` (add a `ChangelogError` fault-model test)
- Modify: `src/lib.rs` (add an `Error` union test)
- Modify: `src/commands/changelog.rs` (add a `ChangelogError::Invalid` disposition test if not covered above)

- [ ] **Step 1: Add the `ProjectError` fault-model test**

Append to `src/project/tests.rs`:

```rust
// --- Fault model: which failures wrap a cause ---

#[test]
fn project_errors_expose_a_source_exactly_when_they_wrap_one() {
    let io = || std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");

    let with_source = [
        ProjectError::Io {
            path: PathBuf::from("mono.toml"),
            source: io(),
        },
        ProjectError::TaskDirectory {
            task: "build".to_owned(),
            path: PathBuf::from("missing"),
            source: io(),
        },
    ];
    for error in &with_source {
        assert!(error.source().is_some(), "{error}");
        assert!(!error.to_string().is_empty(), "{error}");
    }

    let bare = [
        ProjectError::InvalidProject {
            message: "no pipelines".to_owned(),
        },
        ProjectError::UnknownPipeline {
            name: "missing".to_owned(),
            suggestion: Some("ci".to_owned()),
        },
        ProjectError::InvalidTaskName {
            task: "bad[".to_owned(),
        },
        ProjectError::InvalidTask {
            task: "build".to_owned(),
            message: "empty command".to_owned(),
        },
        ProjectError::MissingTask {
            task: "missing".to_owned(),
            suggestion: None,
        },
        ProjectError::InvalidTaskReference {
            reference: "bad[".to_owned(),
            from: TaskNode::new("build"),
        },
        ProjectError::TaskCycle {
            path: vec![TaskNode::new("a"), TaskNode::new("b"), TaskNode::new("a")],
        },
        ProjectError::MissingRoot {
            start: PathBuf::from("."),
        },
    ];
    for error in &bare {
        assert!(error.source().is_none(), "{error}");
        assert!(!error.to_string().is_empty(), "{error}");
    }
}
```

`Parse` and `UnsupportedSchema` carry a `toml::de::Error`/`PathBuf`; assert them in the same test by parsing a bad string:

```rust
    let parse_error = ProjectError::Parse {
        path: PathBuf::from("mono.toml"),
        source: toml::from_str::<crate::config::MonoConfig>("= =").unwrap_err(),
    };
    assert!(parse_error.source().is_some(), "{parse_error}");

    let schema_error = ProjectError::UnsupportedSchema {
        path: PathBuf::from("mono.toml"),
        found: 2,
        supported: 1,
    };
    assert!(schema_error.source().is_none(), "{schema_error}");
```

Add `use std::path::PathBuf;` if the test module does not already have it via `use super::*;` (`project/mod.rs` imports `PathBuf`, so `super::*` provides it).

- [ ] **Step 2: Add the `RunnerError` fault-model test**

Append inside `mod tests` in `src/runner.rs`:

```rust
    #[test]
    fn runner_errors_expose_a_source_exactly_when_they_wrap_one() {
        let io = || io::Error::new(io::ErrorKind::NotFound, "missing");

        let with_source = [
            RunnerError::Spawn {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
                command: vec!["tool".to_owned()],
                cwd: PathBuf::from("/tmp"),
                source: io(),
            },
            RunnerError::Wait {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
                source: io(),
            },
            RunnerError::OutputRead {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
                stream: "stdout",
                source: io(),
            },
            RunnerError::Terminate {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
                operation: "timeout",
                source: io(),
            },
        ];
        for error in &with_source {
            assert!(error.source().is_some(), "{error}");
        }

        let bare = [
            RunnerError::EmptyCommand {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
            },
            RunnerError::Failed(Box::new(FailedTask {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
                command: vec!["tool".to_owned()],
                cwd: PathBuf::from("/tmp"),
                code: Some(1),
                output: CapturedOutput::default(),
                elapsed: Duration::ZERO,
            })),
            RunnerError::TimedOut(Box::new(TimedOutTask {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
                command: vec!["tool".to_owned()],
                cwd: PathBuf::from("/tmp"),
                timeout: Duration::from_secs(1),
                output: CapturedOutput::default(),
                elapsed: Duration::ZERO,
            })),
            RunnerError::OutputLimit(Box::new(OutputLimitTask {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
                stream: "stdout",
                limit: 8,
                output: CapturedOutput::default(),
                elapsed: Duration::ZERO,
            })),
            RunnerError::Cancelled(Box::new(CancelledTask {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
                output: CapturedOutput::default(),
                elapsed: Duration::ZERO,
            })),
        ];
        for error in &bare {
            assert!(error.source().is_none(), "{error}");
            assert!(!error.to_string().is_empty(), "{error}");
        }
    }
```

- [ ] **Step 3: Add the `SchedulerError`, `CacheError`, and `ChangelogError` fault-model tests**

Append inside the existing test modules:

`src/scheduler.rs`:

```rust
    #[test]
    fn scheduler_errors_expose_a_source_exactly_when_they_wrap_one() {
        let with_source = [
            SchedulerError::Task(Box::new(RunnerError::EmptyCommand {
                project: "fixture".to_owned(),
                task: "build".to_owned(),
            })),
            SchedulerError::Cache(CacheError::Invalid {
                message: "bad entry".to_owned(),
            }),
            SchedulerError::Output(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "closed",
            )),
        ];
        for error in &with_source {
            assert!(error.source().is_some(), "{error}");
        }

        let bare = [
            SchedulerError::UnresolvedDependency {
                task: TaskNode::new("app"),
                dependency: TaskNode::new("base"),
            },
            SchedulerError::NoReadyWork,
            SchedulerError::Cancelled,
        ];
        for error in &bare {
            assert!(error.source().is_none(), "{error}");
            assert!(!error.to_string().is_empty(), "{error}");
        }
    }
```

`src/cache/mod.rs`:

```rust
    #[test]
    fn cache_errors_expose_a_source_exactly_when_they_wrap_one() {
        let io_error = CacheError::io(
            PathBuf::from("entry"),
            std::io::Error::new(std::io::ErrorKind::NotFound, "missing"),
        );
        let json_error = CacheError::Json {
            path: PathBuf::from("metadata.json"),
            source: serde_json::from_str::<serde_json::Value>("{").unwrap_err(),
        };
        let invalid = CacheError::Invalid {
            message: "unsafe path".to_owned(),
        };

        assert!(io_error.source().is_some());
        assert!(json_error.source().is_some());
        assert!(invalid.source().is_none());
        assert!(!invalid.to_string().is_empty());
    }
```

`src/changelog.rs` already tests parse failures; add the `ChangelogError` disposition in `src/commands/changelog.rs`:

```rust
    #[test]
    fn changelog_errors_expose_a_source_only_for_read_and_write() {
        let read = ChangelogError::Read {
            path: PathBuf::from("CHANGELOG.md"),
            source: io::Error::new(io::ErrorKind::NotFound, "missing"),
        };
        let invalid = ChangelogError::Invalid("no entries".to_owned());

        assert!(read.source().is_some(), "{read}");
        assert!(invalid.source().is_none(), "{invalid}");
        assert!(!invalid.to_string().is_empty());
    }
```

- [ ] **Step 4: Add the `lib::Error` union test**

Append inside `mod tests` in `src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    fn builders() -> [(Error, bool); 6] {
        [
            (
                Error::Init(InitError::AlreadyInitialized(std::path::PathBuf::from("mono.toml"))),
                false,
            ),
            (
                Error::Doctor(DoctorError::Project(ProjectError::MissingRoot {
                    start: std::path::PathBuf::from("."),
                })),
                true,
            ),
            (Error::Ci(CiError::InvalidJobs), false),
            (
                Error::List(ListError::Project(ProjectError::MissingRoot {
                    start: std::path::PathBuf::from("."),
                })),
                true,
            ),
            (
                Error::Changelog(ChangelogError::Invalid("bad".to_owned())),
                false,
            ),
            (
                Error::Release(ReleaseCommandError::Release(ReleaseError::Write {
                    path: std::path::PathBuf::from("out"),
                    source: io::Error::new(io::ErrorKind::PermissionDenied, "denied"),
                })),
                true,
            ),
        ]
    }

    /// The union is the command failure space, so its `source` must forward to
    /// the wrapped error rather than swallow it.
    #[test]
    fn the_error_union_forwards_display_and_source() {
        for (error, has_source) in builders() {
            assert_eq!(error.source().is_some(), has_source, "{error}");
            assert!(!error.to_string().is_empty(), "{error}");
        }
    }

    #[test]
    fn every_command_error_converts_into_the_union() {
        let _: Error = InitError::AlreadyInitialized(std::path::PathBuf::from("mono.toml")).into();
    }
}
```

`ReleaseError::Write` is the source-carrying release case; `Error::Ci(CiError::InvalidJobs)` and `Error::Changelog(ChangelogError::Invalid)` are the bare cases.

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib project::tests::runner::tests::scheduler::tests::cache::tests::`
Run: `cargo test --lib commands::changelog::tests::`
Run: `cargo test --lib lib::tests::`
Expected: all PASS.

- [ ] **Step 6: Commit**

Run: `cargo fmt`, `cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`.

```bash
but diff
but commit -b testability/error-fault-model -m "test: pin the source contract of every error vocabulary" <ids>
```

---

## Phase B — Delete duplication and dead paths

### Task 6: One run-summary formatter, no unreachable renderer arms

`present_run_finished` emits `RunFinished` only when the mode is `Json`. The terminal and GitHub Actions arms that format the summary (`output.rs:302-313` and `output.rs:395-407`) can therefore never run, and they duplicate the string in `commands/ci.rs::format_summary`.

**Files:**
- Modify: `src/output.rs`
- Test: `src/output.rs` (existing `mod tests`)

- [ ] **Step 1: Write the failing test**

Append inside `mod tests` in `src/output.rs`:

```rust
    #[test]
    fn run_finished_renders_only_in_json_mode() {
        let summary = ExecutionSummary {
            completed: 1,
            cached: 0,
            failed: 0,
            cancelled: 0,
            blocked: 0,
        };

        for mode in [
            OutputMode::Terminal,
            OutputMode::GithubActions,
            OutputMode::Live,
        ] {
            let (sink, out, err) = captured(mode, 1);
            sink.present_run_finished(&summary).expect("summary renders");
            assert!(
                text(&out).is_empty() && text(&err).is_empty(),
                "{mode:?} must leave the summary to the CLI transport, not render it: {}{}",
                text(&out),
                text(&err)
            );
        }

        let (sink, out, _err) = captured(OutputMode::Json, 1);
        sink.present_run_finished(&summary).expect("summary renders");
        let rendered = text(&out);
        assert!(rendered.contains("\"event\":\"run_finished\""), "{rendered}");
    }
```

- [ ] **Step 2: Run the test to verify it passes before the cleanup**

Run: `cargo test --lib output::tests::run_finished_renders_only_in_json_mode`
Expected: PASS. This test is a regression guard for the deletion in the next step — it documents the contract the deletion preserves.

- [ ] **Step 3: Delete the duplicated formatting**

In `render_terminal`, replace the `ExecutionEvent::RunFinished { completed, cached, failed, cancelled, blocked, .. } => writeln!(...)` arm with:

```rust
            // `present_run_finished` emits this event only for JSON. The CLI
            // transport prints the summary for every other mode, so rendering
            // it here would either duplicate the line or drift from
            // `commands::ci::format_summary`.
            ExecutionEvent::RunFinished { .. } => Ok(()),
```

In `render_github_actions`, make the same replacement for its `RunFinished` arm.

- [ ] **Step 4: Run the tests and confirm the string is gone**

Run: `cargo test --lib output::tests::`
Run: `cargo test --test cli default_pipeline_runs_global_tasks_in_dependency_order`
Expected: PASS.

Run: `grep -rn "summary: {" src/`
Expected: exactly one hit, in `src/commands/ci.rs`.

- [ ] **Step 5: Commit**

Run: `cargo fmt`, `cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`.

```bash
but diff
but commit -b testability/one-summary-formatter -m "refactor: render the run summary in exactly one place" <ids>
```

---

### Task 7: Cover the legacy `verify_checksums` contract and the checksum parser

`verify_checksums` is exported but never called in-tree and never tested. `read_checksums` — the parser it and `verify_manifest` both use — has no error-path coverage either. Either the function is a capability, in which case its contract is pinned, or it is dead and should be removed; this task pins it.

**Files:**
- Modify: `src/release.rs` (extend the existing `mod tests`)

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `src/release.rs`:

```rust
    fn sha256_of(contents: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(contents);
        format!("{:x}", hasher.finalize())
    }

    #[test]
    fn verify_checksums_accepts_a_matching_sums_file() {
        let temp = TempDir::new();
        fs::write(temp.path().join("app.tar.gz"), b"app").unwrap();
        fs::write(
            temp.path().join(CHECKSUMS_FILE_NAME),
            format!("{}  app.tar.gz\n", sha256_of(b"app")),
        )
        .unwrap();

        let count = verify_checksums(temp.path()).expect("checksums verify");

        assert_eq!(count, 1);
    }

    #[test]
    fn verify_checksums_reports_a_mismatch() {
        let temp = TempDir::new();
        fs::write(temp.path().join("app.tar.gz"), b"after").unwrap();
        fs::write(
            temp.path().join(CHECKSUMS_FILE_NAME),
            format!("{}  app.tar.gz\n", sha256_of(b"before")),
        )
        .unwrap();

        let error = verify_checksums(temp.path()).expect_err("a mismatch fails");

        assert!(error.to_string().contains("checksum mismatch"), "{error}");
    }

    #[test]
    fn verify_checksums_rejects_a_non_regular_entry() {
        let temp = TempDir::new();
        fs::create_dir(temp.path().join("directory")).unwrap();
        fs::write(
            temp.path().join(CHECKSUMS_FILE_NAME),
            format!("{}  directory\n", sha256_of(b"")),
        )
        .unwrap();

        let error = verify_checksums(temp.path()).expect_err("a directory is not an artifact");

        assert!(error.to_string().contains("not a regular file"), "{error}");
    }

    #[test]
    fn malformed_sums_files_are_rejected_with_their_line_number() {
        let temp = TempDir::new();
        fs::write(temp.path().join("artifact"), b"data").unwrap();

        let cases = [
            ("no separator\n", "expected `<sha256>  <file>`"),
            ("abc  artifact\n", "invalid SHA-256 digest"),
            ("   \n", "lists no artifacts"),
            (
                "0000000000000000000000000000000000000000000000000000000000000000  ../escape\n",
                "invalid artifact name",
            ),
            (
                "0000000000000000000000000000000000000000000000000000000000000000  artifact\n0000000000000000000000000000000000000000000000000000000000000000  artifact\n",
                "duplicate artifact name",
            ),
        ];

        for (contents, expected_message) in cases {
            fs::write(temp.path().join(CHECKSUMS_FILE_NAME), contents).unwrap();
            let error = verify_checksums(temp.path()).expect_err(contents);
            assert!(
                error.to_string().contains(expected_message),
                "{contents:?}: {error}"
            );
        }
    }
```

- [ ] **Step 2: Run the tests**

Run: `cargo test --lib release::tests::`
Expected: 4 new tests PASS.

- [ ] **Step 3: Decide the function's fate and record it**

If the project has no legacy consumer, remove `verify_checksums` from `src/release.rs` and from the `pub use release::{...}` list in `src/lib.rs` and delete the tests from Step 1 instead. If it stays, leave the tests in place and add a one-line doc sentence recording that it is a public compatibility path with no in-tree caller.

- [ ] **Step 4: Commit**

Run: `cargo fmt`, `cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`.

```bash
but diff
but commit -b testability/verify-checksums -m "test: pin the legacy checksum verification contract" <ids>
```

---

### Task 8: Classify `ListError::Json` with the other tool failures

`CiError::Json` maps to exit 3, but `ListError::Json` — the same serde serialization failure — is caught by `Error::List(_) => EXIT_FAILED` and exits 1.

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Write the failing test**

Append inside `mod tests` in `src/main.rs`:

```rust
    #[test]
    fn a_list_serialization_failure_is_a_tool_failure_like_the_ci_one() {
        let source = serde_json::from_str::<u32>("not json").expect_err("invalid JSON");

        assert_eq!(
            exit_code(&Error::List(ListError::Json { source })),
            EXIT_TOOL,
            "a failed serialization is a mono failure, not a rejected request"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --bin mono a_list_serialization_failure_is_a_tool_failure_like_the_ci_one`
Expected: FAIL — got `1`, expected `3`.

- [ ] **Step 3: Split the `List` arm**

In `src/main.rs::exit_code`, replace:

```rust
        Error::Ci(CiError::Project(error))
        | Error::Doctor(DoctorError::Project(error))
        | Error::List(ListError::Project(error)) => project_exit_code(error),
        Error::Changelog(error) => changelog_exit_code(error),
        Error::Release(ReleaseCommandError::Release(error)) => release_exit_code(error),
        Error::List(_) => EXIT_FAILED,
```

with:

```rust
        Error::Ci(CiError::Project(error))
        | Error::Doctor(DoctorError::Project(error))
        | Error::List(ListError::Project(error)) => project_exit_code(error),
        Error::Changelog(error) => changelog_exit_code(error),
        Error::Release(ReleaseCommandError::Release(error)) => release_exit_code(error),
        // A serialization failure means mono could not produce the requested
        // output, exactly like `CiError::Json`; both are tool failures.
        Error::List(ListError::Json { .. }) => EXIT_TOOL,
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --bin mono`
Expected: all PASS, including the new test.

- [ ] **Step 5: Commit**

Run: `cargo fmt`, `cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`.

```bash
but diff
but commit -b fix/list-json-exit-code -m "fix: classify a list serialization failure as a tool failure" <ids>
```

---

## Phase C — Add the missing seams

### Task 9: Make the runner's poll decision a pure function

The poll loop decides between cancel, output-limit, descendant cleanup, timeout, wait-error, and sleep using live state. Extract that decision table so each branch is asserted without a clock or a child process; the driver keeps the side effects.

**Files:**
- Modify: `src/runner.rs`
- Test: `src/runner.rs` (existing `mod tests`)

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `src/runner.rs`:

```rust
    // `PollDecision`, `WaitOutcome`, `poll_decision`, `next_poll_interval`, and
    // the `POLL_INTERVAL_*` constants are reachable through the existing
    // `use super::*;` at the top of this module.
    const TIMEOUT: Duration = Duration::from_secs(10);

    #[test]
    fn cancellation_outranks_every_other_condition() {
        assert_eq!(
            poll_decision(
                true,
                Some("stdout"),
                WaitOutcome::Exited,
                2,
                Duration::ZERO,
                TIMEOUT,
                POLL_INTERVAL_START,
            ),
            PollDecision::Cancel
        );
    }

    #[test]
    fn an_exceeded_stream_stops_the_task_before_the_exit_is_considered() {
        assert_eq!(
            poll_decision(
                false,
                Some("stderr"),
                WaitOutcome::Exited,
                2,
                Duration::ZERO,
                TIMEOUT,
                POLL_INTERVAL_START,
            ),
            PollDecision::OutputLimit("stderr")
        );
    }

    #[test]
    fn an_exit_wants_descendant_cleanup_until_both_readers_finish() {
        for (readers_done, descendants_may_hold_pipes) in [(0, true), (1, true), (2, false)] {
            assert_eq!(
                poll_decision(
                    false,
                    None,
                    WaitOutcome::Exited,
                    readers_done,
                    Duration::ZERO,
                    TIMEOUT,
                    POLL_INTERVAL_START,
                ),
                PollDecision::Exited {
                    descendants_may_hold_pipes
                },
                "readers_done={readers_done}"
            );
        }
    }

    #[test]
    fn a_running_child_past_its_timeout_times_out() {
        assert_eq!(
            poll_decision(
                false,
                None,
                WaitOutcome::Running,
                2,
                TIMEOUT,
                TIMEOUT,
                POLL_INTERVAL_START,
            ),
            PollDecision::Timeout
        );
    }

    #[test]
    fn a_running_child_sleeps_for_the_shorter_of_the_interval_and_the_remaining_time() {
        assert_eq!(
            poll_decision(
                false,
                None,
                WaitOutcome::Running,
                2,
                Duration::from_secs(9),
                TIMEOUT,
                Duration::from_millis(5),
            ),
            PollDecision::Sleep(Duration::from_millis(5))
        );
        assert_eq!(
            poll_decision(
                false,
                None,
                WaitOutcome::Running,
                2,
                Duration::from_millis(9_999),
                TIMEOUT,
                Duration::from_millis(5),
            ),
            PollDecision::Sleep(Duration::from_millis(1))
        );
    }

    #[test]
    fn a_failed_wait_is_reported() {
        assert_eq!(
            poll_decision(
                false,
                None,
                WaitOutcome::Failed,
                2,
                Duration::ZERO,
                TIMEOUT,
                POLL_INTERVAL_START,
            ),
            PollDecision::WaitFailed
        );
    }

    #[test]
    fn the_poll_interval_doubles_up_to_the_cap() {
        assert_eq!(
            next_poll_interval(POLL_INTERVAL_START),
            (POLL_INTERVAL_START * 2).min(POLL_INTERVAL_MAX)
        );
        assert_eq!(next_poll_interval(POLL_INTERVAL_MAX), POLL_INTERVAL_MAX);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib runner::tests::`
Expected: FAIL with "cannot find type `PollDecision`".

- [ ] **Step 3: Add the pure decision table**

Add above `pub(crate) struct ManagedChild` usage in `src/runner.rs` (place it after the `POLL_INTERVAL_*` constants):

```rust
/// The observable result of one non-blocking child wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitOutcome {
    Exited,
    Running,
    Failed,
}

/// What the poll loop must do next, given the state it can observe.
///
/// Extracted from the loop so the precedence between cancellation, an exceeded
/// output limit, child exit, and timeout is a table a test can assert instead
/// of five interleaved branches over a live child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollDecision {
    /// Terminate the tree and report cancellation.
    Cancel,
    /// Terminate the tree and report that `stream` exceeded its limit.
    OutputLimit(&'static str),
    /// The direct child exited; descendants may still hold the pipes open.
    Exited {
        descendants_may_hold_pipes: bool,
    },
    /// Terminate the tree and report the timeout.
    Timeout,
    /// `try_wait` failed; report a wait error.
    WaitFailed,
    /// Still running; sleep before polling again.
    Sleep(Duration),
}

fn poll_decision(
    cancelled: bool,
    exceeded_stream: Option<&'static str>,
    wait: WaitOutcome,
    readers_done: u8,
    elapsed: Duration,
    timeout: Duration,
    poll_interval: Duration,
) -> PollDecision {
    if cancelled {
        return PollDecision::Cancel;
    }
    if let Some(stream) = exceeded_stream {
        return PollDecision::OutputLimit(stream);
    }
    match wait {
        WaitOutcome::Exited => PollDecision::Exited {
            descendants_may_hold_pipes: readers_done < 2,
        },
        WaitOutcome::Running if elapsed >= timeout => PollDecision::Timeout,
        WaitOutcome::Running => {
            let remaining = timeout.saturating_sub(elapsed);
            PollDecision::Sleep(poll_interval.min(remaining))
        }
        WaitOutcome::Failed => PollDecision::WaitFailed,
    }
}

fn next_poll_interval(current: Duration) -> Duration {
    (current * 2).min(POLL_INTERVAL_MAX)
}
```

- [ ] **Step 4: Preserve the cancellation ordering in the driver**

Replace the poll loop in `run_with_options` — the block beginning `let status = loop {` and ending with the `};` before `let joined = join_output(` — with:

```rust
        let mut poll_interval = POLL_INTERVAL_START;
        let mut exceeded_stream: Option<&'static str> = None;
        // `None` means the run is already cancelled, so no wait is attempted:
        // cancellation is decided before the child is touched, exactly as
        // before this refactor.
        let status = loop {
            if exceeded_stream.is_none()
                && let Ok(stream) = limit_receiver.try_recv()
            {
                exceeded_stream = Some(stream);
            }
            let cancelled = cancellation.is_some_and(CancellationToken::is_cancelled);
            let wait = (!cancelled).then(|| managed.try_wait());
            let outcome = match &wait {
                Some(Ok(WaitResult::Exited(_))) => WaitOutcome::Exited,
                Some(Err(_)) => WaitOutcome::Failed,
                _ => WaitOutcome::Running,
            };
            match poll_decision(
                cancelled,
                exceeded_stream,
                outcome,
                reader_done.load(Ordering::Acquire),
                started.elapsed(),
                planned.timeout(),
                poll_interval,
            ) {
                PollDecision::Cancel => {
                    terminate_tree(
                        &mut managed,
                        planned.project(),
                        planned.task(),
                        "cancellation",
                    )?;
                    managed.wait().map_err(|source| RunnerError::Wait {
                        project: planned.project().to_owned(),
                        task: planned.task().to_owned(),
                        source,
                    })?;
                    let output = join_output(
                        planned.project(),
                        planned.task(),
                        stdout_reader,
                        stderr_reader,
                    )?
                    .output;
                    return Err(RunnerError::Cancelled(Box::new(CancelledTask {
                        project: planned.project().to_owned(),
                        task: planned.task().to_owned(),
                        output,
                        elapsed: started.elapsed(),
                    })));
                }
                PollDecision::OutputLimit(stream_name) => {
                    terminate_tree(
                        &mut managed,
                        planned.project(),
                        planned.task(),
                        "output limit",
                    )?;
                    managed.wait().map_err(|source| RunnerError::Wait {
                        project: planned.project().to_owned(),
                        task: planned.task().to_owned(),
                        source,
                    })?;
                    let joined = join_output(
                        planned.project(),
                        planned.task(),
                        stdout_reader,
                        stderr_reader,
                    )?;
                    return Err(RunnerError::OutputLimit(Box::new(OutputLimitTask {
                        project: planned.project().to_owned(),
                        task: planned.task().to_owned(),
                        stream: stream_name,
                        limit: output_limit,
                        output: joined.output,
                        elapsed: started.elapsed(),
                    })));
                }
                PollDecision::Exited {
                    descendants_may_hold_pipes,
                } => {
                    if descendants_may_hold_pipes {
                        terminate_tree(
                            &mut managed,
                            planned.project(),
                            planned.task(),
                            "descendant cleanup",
                        )?;
                    }
                    match wait {
                        Some(Ok(WaitResult::Exited(status))) => break status,
                        _ => unreachable!("PollDecision::Exited implies an exited child"),
                    }
                }
                PollDecision::Timeout => {
                    terminate_tree(&mut managed, planned.project(), planned.task(), "timeout")?;
                    managed.wait().map_err(|source| RunnerError::Wait {
                        project: planned.project().to_owned(),
                        task: planned.task().to_owned(),
                        source,
                    })?;
                    let output = join_output(
                        planned.project(),
                        planned.task(),
                        stdout_reader,
                        stderr_reader,
                    )?
                    .output;
                    return Err(RunnerError::TimedOut(Box::new(TimedOutTask {
                        project: planned.project().to_owned(),
                        task: planned.task().to_owned(),
                        command: planned.command().to_vec(),
                        cwd: planned.cwd().to_path_buf(),
                        timeout: planned.timeout(),
                        output,
                        elapsed: started.elapsed(),
                    })));
                }
                PollDecision::WaitFailed => {
                    if let Err(error) = terminate_tree(
                        &mut managed,
                        planned.project(),
                        planned.task(),
                        "wait error",
                    ) {
                        let _ = stdout_reader.join();
                        let _ = stderr_reader.join();
                        return Err(error);
                    }
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    let source = match wait {
                        Some(Err(source)) => source,
                        _ => unreachable!("PollDecision::WaitFailed implies a failed wait"),
                    };
                    return Err(RunnerError::Wait {
                        project: planned.project().to_owned(),
                        task: planned.task().to_owned(),
                        source,
                    });
                }
                PollDecision::Sleep(duration) => {
                    thread::sleep(duration);
                    poll_interval = next_poll_interval(poll_interval);
                }
            }
        };
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib runner::tests::`
Expected: all PASS, including the seven new decision tests.

Run: `cargo test --test cli` and `cargo test --lib cache::tests::`
Expected: PASS — the subprocess timeout/cancel/output-limit/descendant tests still pass with identical behavior.

- [ ] **Step 6: Commit**

Run: `cargo fmt`, `cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`.

```bash
but diff
but commit -b testability/runner-poll-decision -m "refactor: extract the runner poll decision" <ids>
```

---

### Task 10: Give the scheduler an executor seam and test the loop

`execute_plan` is the crate's most complex loop and has no end-to-end unit test. Add a one-method executor seam, then drive the real loop with a scripted executor and captured output sinks. This is the largest task; do it last.

**Files:**
- Modify: `src/runner.rs` (add the trait and the `Runner` impl)
- Modify: `src/scheduler.rs` (`execute_plan` signature, `WorkerJob`, `execute_task`, tests)
- Modify: `src/commands/ci.rs` (wrap the runner in an `Arc`)
- Test: `src/scheduler.rs` (extend the existing `mod tests`)

- [ ] **Step 1: Add the seam and its production implementation**

In `src/runner.rs`, next to the `OutputCallback` alias, add:

```rust
/// The seam the scheduler executes tasks through.
///
/// [`Runner`] is the production implementation; a test supplies a scripted one
/// to drive the scheduling loop without spawning processes.
pub(crate) trait TaskExecutor: Send + Sync {
    fn execute(
        &self,
        project_root: &Path,
        planned: &PlannedTask,
        cancellation: Option<&CancellationToken>,
        output_callback: Option<OutputCallback>,
    ) -> Result<TaskResult, RunnerError>;
}

impl TaskExecutor for Runner {
    fn execute(
        &self,
        project_root: &Path,
        planned: &PlannedTask,
        cancellation: Option<&CancellationToken>,
        output_callback: Option<OutputCallback>,
    ) -> Result<TaskResult, RunnerError> {
        self.run_with_options(project_root, planned, cancellation, output_callback)
    }
}
```

- [ ] **Step 2: Thread the seam through the scheduler**

In `src/scheduler.rs`:

Change the import to include the trait:

```rust
use crate::runner::{CancellationToken, RunnerError, TaskExecutor, TaskResult};
```

Change the signature and the clone site:

```rust
pub(crate) fn execute_plan(
    project: &Project,
    plan: &[PlannedTask],
    jobs: usize,
    runner: Arc<dyn TaskExecutor>,
    output: &Arc<OutputSink>,
    cache_mode: CacheMode,
    cancellation: &CancellationToken,
) -> Result<ExecutionSummary, SchedulerError> {
```

In the dispatch block, replace `runner: runner.clone(),` with `runner: Arc::clone(&runner),`.

Change the `WorkerJob` field:

```rust
struct WorkerJob {
    node: TaskNode,
    task: Arc<PlannedTask>,
    cache: CacheStore,
    cache_session: Option<Arc<CacheSession>>,
    root: Arc<PathBuf>,
    can_cache: bool,
    force: bool,
    dependency_keys: Vec<String>,
    runner: Arc<dyn TaskExecutor>,
    cancellation: CancellationToken,
    output: Arc<OutputSink>,
}
```

In `execute_task`, replace the runner call:

```rust
        match runner.execute(&root, &task, task_cancellation, output_callback.clone()) {
```

Update `src/commands/ci.rs`:

```rust
use crate::runner::{CancellationToken, Runner, TaskExecutor, format_command};
```

and where the runner is created:

```rust
    let runner: Arc<dyn TaskExecutor> = Arc::new(Runner::new());
```

Pass it by value: `execute_plan(&project, &plan, jobs, runner, &output, execution.cache, &execution.cancellation)?;`

- [ ] **Step 3: Verify the refactor changes no behavior**

Run: `cargo test --workspace --all-targets --all-features`
Expected: all 163 existing tests PASS.

- [ ] **Step 4: Write the scripted executor and the failing tests**

Append inside `mod tests` in `src/scheduler.rs`:

```rust
    /// A task executor that records every call and fails a scripted set of
    /// tasks, so the scheduling loop can be driven deterministically.
    #[derive(Default)]
    struct ScriptedExecutor {
        calls: Mutex<Vec<String>>,
        failures: Mutex<BTreeSet<String>>,
    }

    impl ScriptedExecutor {
        fn failing(ids: &[&str]) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                failures: Mutex::new(ids.iter().map(|id| (*id).to_owned()).collect()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("call log is not poisoned").clone()
        }
    }

    impl TaskExecutor for ScriptedExecutor {
        fn execute(
            &self,
            _project_root: &std::path::Path,
            task: &PlannedTask,
            _cancellation: Option<&CancellationToken>,
            _output: Option<crate::runner::OutputCallback>,
        ) -> Result<TaskResult, RunnerError> {
            self.calls
                .lock()
                .expect("call log is not poisoned")
                .push(task.task().to_owned());
            if self
                .failures
                .lock()
                .expect("failure set is not poisoned")
                .contains(task.task())
            {
                return Err(RunnerError::EmptyCommand {
                    project: task.project().to_owned(),
                    task: task.task().to_owned(),
                });
            }
            succeeded()
        }
    }

    /// A sink whose `Write` always fails, standing in for a hung-up pipe.
    struct FailingWriter;

    impl std::io::Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "consumer hung up",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn terminal_sink() -> Arc<OutputSink> {
        Arc::new(OutputSink::test_sink(
            crate::output::OutputMode::Terminal,
            1,
            Box::new(Vec::new()),
            Box::new(Vec::new()),
        ))
    }

    fn project_and_plan(manifest: &str) -> (TempDir, Project, Vec<PlannedTask>) {
        let temp = TempDir::new();
        fs::write(config_path(temp.path()), manifest).expect("write manifest");
        let project = Project::load(temp.path()).expect("project loads");
        let plan = project.plan(None, &[]).expect("plan succeeds");
        (temp, project, plan)
    }

    fn run_with_script(
        manifest: &str,
        jobs: usize,
        executor: &Arc<ScriptedExecutor>,
        output: &Arc<OutputSink>,
        cache_mode: CacheMode,
        cancellation: &CancellationToken,
    ) -> Result<ExecutionSummary, SchedulerError> {
        let (_temp, project, plan) = project_and_plan(manifest);
        // The temp project must outlive the call, so keep `_temp` in scope.
        execute_plan(
            &project,
            &plan,
            jobs,
            Arc::clone(executor) as Arc<dyn TaskExecutor>,
            output,
            cache_mode,
            cancellation,
        )
    }

    #[test]
    fn a_chain_runs_in_dependency_order_and_accounts_for_every_task() {
        let executor = Arc::new(ScriptedExecutor::default());
        let summary = run_with_script(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"app\"]\n\n[tasks.base]\ncommand = [\"echo\", \"base\"]\n\n[tasks.app]\ncommand = [\"echo\", \"app\"]\ndepends_on = [\"base\"]\n",
            1,
            &executor,
            &terminal_sink(),
            CacheMode::NoCache,
            &CancellationToken::new(),
        )
        .expect("the chain succeeds");

        assert_eq!(executor.calls(), vec!["base".to_owned(), "app".to_owned()]);
        assert_eq!(summary.completed, 2);
        assert_eq!(summary.failed + summary.cancelled + summary.blocked, 0);
    }

    #[test]
    fn a_failed_task_stops_normal_work_but_still_runs_finalizers() {
        let executor = Arc::new(ScriptedExecutor::failing(&["build"]));
        let error = run_with_script(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\nfinally = [\"cleanup\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n\n[tasks.cleanup]\ncommand = [\"echo\", \"cleanup\"]\n",
            1,
            &executor,
            &terminal_sink(),
            CacheMode::NoCache,
            &CancellationToken::new(),
        )
        .expect_err("a failed task is the reported error");

        assert!(matches!(error, SchedulerError::Task(_)), "{error}");
        assert_eq!(
            executor.calls(),
            vec!["build".to_owned(), "cleanup".to_owned()],
            "the finalizer must run after a normal failure"
        );
    }

    #[test]
    fn a_cancelled_run_blocks_normal_work_and_reports_cancelled() {
        let executor = Arc::new(ScriptedExecutor::default());
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = run_with_script(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n",
            1,
            &executor,
            &terminal_sink(),
            CacheMode::NoCache,
            &cancellation,
        )
        .expect_err("a cancelled run is an error");

        assert!(matches!(error, SchedulerError::Cancelled), "{error}");
        assert!(
            executor.calls().is_empty(),
            "a cancelled task must never reach the executor"
        );
    }

    #[test]
    fn an_output_failure_is_reported_before_any_task_runs() {
        let executor = Arc::new(ScriptedExecutor::default());
        let output: Arc<OutputSink> = Arc::new(OutputSink::test_sink(
            crate::output::OutputMode::Terminal,
            1,
            Box::new(FailingWriter),
            Box::new(FailingWriter),
        ));

        let error = run_with_script(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\n",
            1,
            &executor,
            &output,
            CacheMode::NoCache,
            &CancellationToken::new(),
        )
        .expect_err("a renderer failure stops the run");

        assert!(matches!(error, SchedulerError::Output(_)), "{error}");
        assert!(executor.calls().is_empty());
    }

    #[test]
    fn a_cache_failure_is_reported_before_the_task_runs() {
        let executor = Arc::new(ScriptedExecutor::default());
        let error = run_with_script(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"build\"]\n\n[tasks.build]\ncommand = [\"echo\", \"build\"]\ncache = true\ninputs = [\"missing/**\"]\noutputs = [\"out/**\"]\n",
            1,
            &executor,
            &terminal_sink(),
            CacheMode::ReadWrite,
            &CancellationToken::new(),
        )
        .expect_err("an unmatched input pattern is a cache failure");

        assert!(matches!(error, SchedulerError::Cache(_)), "{error}");
        assert!(
            executor.calls().is_empty(),
            "the task must not run when its key cannot be computed"
        );
    }

    #[test]
    fn independent_tasks_run_once_each_at_one_worker() {
        let executor = Arc::new(ScriptedExecutor::default());
        let summary = run_with_script(
            "[project]\nname = \"fixture\"\n\n[pipelines.ci]\ntasks = [\"a\", \"b\", \"c\"]\n\n[tasks.a]\ncommand = [\"echo\", \"a\"]\n\n[tasks.b]\ncommand = [\"echo\", \"b\"]\n\n[tasks.c]\ncommand = [\"echo\", \"c\"]\n",
            1,
            &executor,
            &terminal_sink(),
            CacheMode::NoCache,
            &CancellationToken::new(),
        )
        .expect("independent tasks succeed");

        let mut calls = executor.calls();
        calls.sort();
        assert_eq!(
            calls,
            vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]
        );
        assert_eq!(summary.completed, 3);
    }
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib scheduler::tests::`
Expected: all scheduler tests PASS, including the six new loop tests.

- [ ] **Step 6: Run the full suite and commit**

Run: `cargo test --workspace --all-targets --all-features`
Expected: PASS.

Run: `cargo fmt`, `cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings` — clean.

```bash
but diff
but commit -b testability/scheduler-executor-seam -m "refactor: give the scheduler an executor seam and test the loop" <ids>
```

---

## Self-Review

**Spec coverage.** Every finding has a task: F1→10, F2→9, F3→1, F4→2, F5→6, F6→7, F7→3, F8→4, F9→5, F10→8, F11 is deferred with a reason (accessor churn is not worth the diff until another change touches the call sites). The "Deferred" list has no tasks by design.

**Placeholder scan.** Every code step contains the code to write. Every test step names an exact filter (`cargo test --lib <module>::tests::`, `cargo test --bin mono`). There is no "add error handling" / "write tests for the above" step.

**Type consistency.** `TaskExecutor::execute` is defined in Task 10 Step 1 and referenced only there and in Step 2's scheduler/ci edits and Step 4's `ScriptedExecutor`. `PollDecision`, `WaitOutcome`, `poll_decision`, `next_poll_interval` are introduced and used only in Task 9. `date_from_unix_seconds` is introduced and used only in Task 3. `ScriptedExecutor`, `FailingWriter`, `terminal_sink`, `project_and_plan`, `run_with_script` are introduced and used only in Task 10. `CacheError::io` (Task 5, cache test) is the existing private constructor in `cache/mod.rs`.

**Ordering dependencies.** Task 6 depends on nothing but should come before Task 10 so the summary contract is settled. Task 9 and Task 10 both edit `src/runner.rs`; Task 9 adds the poll helpers and Task 10 adds the trait next to `OutputCallback`, so they do not touch the same lines — apply Task 9 first. Task 5 edits test modules in files Task 9 and Task 10 also edit (`runner.rs`, `scheduler.rs`); because it only appends to `mod tests`, apply it first. Task 10's signature change is the only public-in-crate API change and is confined to `scheduler.rs` → `ci.rs`.

**Known follow-ups not in this plan.** Listed under "Findings explicitly deferred": the `Project::from_config` seam, `SchedulerError::UnresolvedDependency`/`NoReadyWork` → panics, the `O(n)` `finalizers_allowed` scan, `run_id` pinning, direct `ManagedChild` tests, a deterministic multi-worker overlap test, and the `PlannedTask::id()`/`task()` accessor merge.

---

## Out of Scope

- Publishing, registries, package managers, language detection — deliberate non-goals of the tool.
- The GitHub Actions workflows under `.github/workflows/` and `xtask`.
- The deleted `examples/` and `docs/*.md` files in the GitButler working tree. None of these tasks touches those paths; do not restore them here.
