# Testability and Code Improvements — Round 3 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reduce the remaining side-effect coupling in `src/` so scheduler and runner failure paths can be tested deterministically without changing the CLI, cache, or JSON contracts.

**Architecture:** Keep `Project`, cache-key computation, output rendering, and CLI transport as separate boundaries. Add explicit scheduler services for cache, ambient environment, and retry sleeping; extract scheduler state transitions from the worker/channel orchestration; and give `Runner` a production process adapter plus test-only fake process and clock implementations. Pure cache and stream algorithms receive direct unit tests, while real subprocess tests remain for platform process-tree behavior.

**Tech Stack:** Rust 2024, Clap, Serde/JSON, TOML, `sha2`, platform process APIs, `ctrlc`. No new dependencies.

---

## Current baseline and analysis

The current tree already contains the earlier testability work: project/cache module splits, injected `OutputSink` writers, typed JSON documents, a `TaskExecutor` seam, a pure runner poll-decision table, deterministic changelog date input, and direct tests for manifest discovery, error sources, release checksums, and scheduler behavior.

Baseline verification on 2026-09-11:

- `cargo test --workspace --all-targets --all-features` → **204 tests pass**: 161 library tests, 17 binary tests, 21 CLI tests, 5 release CLI tests.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` → clean.
- `cargo fmt --check` → clean.

### Findings, ordered by payoff

| # | Finding | Location | Consequence |
| --- | --- | --- | --- |
| F1 | Scheduler orchestration still owns cache preparation, ambient environment reads, worker lifecycle, retries, output calls, and dependency state transitions in one function. | `src/scheduler.rs::execute_plan` | `TaskExecutor` is injectable, but cache-hit, cache-key, cache-store, retry-backoff, and several first-error paths still require real filesystem state or real sleeping. The function is difficult to reason about and difficult to cover exhaustively. |
| F2 | Runner decisions are pure, but the poll driver still owns `ManagedChild`, wall-clock reads, real sleeping, stream-reader threads, and process-tree termination. | `src/runner.rs::Runner::run_with_options` | Timeout, wait failure, cancellation, callback failure, and descendant-pipe behavior are mostly subprocess/timing tests. A regression can make the suite slow or flaky before a decision-table test notices it. |
| F3 | Cache glob matching, prefix pruning, path validation, and hash framing are private algorithms tested only through temporary-directory integration tests. | `src/cache/pattern.rs`, `src/cache/hash.rs` | A matcher or framing regression is diagnosed through a large cache test rather than at the smallest failing function. Important cases such as `**` zero-segment matches and ordered negation are not directly pinned. |
| F4 | `PlannedTask::id()` and `PlannedTask::task()` expose the same field under two names. | `src/project/mod.rs` | Callers disagree about whether the identifier is a task name or a node ID, adding mental overhead at every scheduler/runner/cache boundary. |
| F5 | Existing output and CLI seams are adequate; splitting large files by line count alone would add churn without improving a contract. | `src/output.rs`, `src/main.rs` | Do not perform a cosmetic file split in this round. Preserve the current `OutputSink` writer injection and CLI integration coverage. |

### Fail-fast policy for this round

Keep `assert!`/`expect` for broken internal invariants: duplicate validated nodes, missing validated dependencies, impossible worker-channel closure, missing piped streams in the production adapter, and poisoned internal locks. Return `CacheError`, `RunnerError`, `SchedulerError`, and `io::Error` for expected filesystem, process, output, timeout, cancellation, and user-manifest failures. The new service traits must not turn expected failures into panics.

---

## Files and responsibility map

- Modify `src/cache/pattern.rs`: add direct tests for the pure ordered matcher and directory-prefix pruning.
- Modify `src/cache/hash.rs`: add direct tests for path safety and length-delimited hashing helpers.
- Modify `src/project/mod.rs`, `src/project/tests.rs`, `src/cache/mod.rs`, `src/runner.rs`, and `src/scheduler.rs`: replace the duplicate `PlannedTask::task()` accessor with `id()`.
- Modify `src/scheduler.rs`: introduce `SchedulerServices`, move production wiring into a thin wrapper, and extract a pure `PlanState` transition object from the worker/channel loop.
- Modify `src/runner.rs`: introduce the process/clock seam, preserve the current `Runner` public constructor, and add deterministic driver tests.
- Modify `src/process.rs`: adapt the existing platform-specific `ManagedChild` to the runner process interface without changing termination semantics.
- Keep shared fake fixtures local to the relevant test modules; do not add an external test dependency.

No public CLI flags, serialized field names, event tags, exit-code mappings, cache format version, or release artifact formats change.

---

## Phase A — Pin small pure contracts first

### Task 1: Test cache pattern matching and hash framing directly

**Files:**
- Modify: `src/cache/pattern.rs` (append a `#[cfg(test)]` module)
- Modify: `src/cache/hash.rs` (append a `#[cfg(test)]` module)

- [ ] **Step 1: Add matcher tests**

Cover these exact behaviors in `src/cache/pattern.rs`:

```rust
#[test]
fn ordered_patterns_apply_the_last_matching_selection() {
    let patterns = CachePatterns::compile(&[
        "src/**".to_owned(),
        "!src/generated/**".to_owned(),
        "src/generated/keep.txt".to_owned(),
    ]);

    assert!(patterns.matches("src/main.rs"));
    assert!(!patterns.matches("src/generated/drop.txt"));
    assert!(patterns.matches("src/generated/keep.txt"));
}

#[test]
fn double_star_matches_zero_or_more_path_segments() {
    let patterns = CachePatterns::compile(&["src/**/Cargo.toml".to_owned()]);

    assert!(patterns.matches("src/Cargo.toml"));
    assert!(patterns.matches("src/a/b/Cargo.toml"));
    assert!(!patterns.matches("tests/Cargo.toml"));
}

#[test]
fn directory_pruning_rejects_unreachable_subtrees() {
    let patterns = CachePatterns::compile(&["src/**/*.rs".to_owned()]);

    assert!(patterns.could_match_below("src"));
    assert!(patterns.could_match_below("src/lib"));
    assert!(!patterns.could_match_below("target"));
}
```

Use the exact function name `double_star_matches_zero_or_more_path_segments`; the test name must be a valid Rust identifier.

- [ ] **Step 2: Add hash-helper tests**

Cover `hex_digest`, `validate_cached_path`, and `hash_strings` in `src/cache/hash.rs`:

```rust
#[test]
fn hex_digest_uses_lowercase_two_digit_encoding() {
    assert_eq!(hex_digest(&[0x00, 0x01, 0xab, 0xff]), "0001abff");
}

#[test]
fn cached_paths_accept_nested_relative_names_only() {
    assert!(validate_cached_path("dist/app.bin").is_ok());
    for unsafe_path in ["", "/tmp/app", "../app", "dist/../../app"] {
        assert!(validate_cached_path(unsafe_path).is_err(), "{unsafe_path}");
    }
}

#[test]
fn hash_string_framing_distinguishes_different_value_boundaries() {
    let mut first = Sha256::new();
    hash_strings(&mut first, &["ab".to_owned(), "c".to_owned()]);
    let mut second = Sha256::new();
    hash_strings(&mut second, &["a".to_owned(), "bc".to_owned()]);

    assert_ne!(first.finalize(), second.finalize());
}
```

- [ ] **Step 3: Run focused tests**

Run:

```bash
cargo test --lib cache::pattern::tests::
cargo test --lib cache::hash::tests::
```

Expected: all new matcher/hash tests pass. If a test exposes a mismatch between the documented matcher semantics and the implementation, fix the implementation only when the existing cache integration tests demonstrate that the intended behavior is correct; otherwise adjust the test to the current manifest contract and record the decision in the task commit.

- [ ] **Step 4: Run formatting and the full library suite**

Run `cargo fmt`, `cargo fmt --check`, and `cargo test --lib`. Expected: clean formatting and all existing library tests pass.

- [ ] **Step 5: Commit**

Use GitButler after inspecting `but diff`:

```bash
but commit -b testability/cache-pure-contracts -m "test: pin cache matcher and hash helper contracts" <ids>
```

### Task 2: Test stream capture without a subprocess

`read_stream` is the runner’s smallest I/O boundary and currently has no direct tests. Test its bounded-output and callback contract with `std::io::Cursor` and an `mpsc::channel`.

**Files:**
- Modify: `src/runner.rs` (append tests inside the existing test module)

- [ ] **Step 1: Add tests for exact-limit, overflow, and callback failure**

The tests must assert:

1. bytes exactly equal to `limit` are retained and do not send a limit notification;
2. bytes beyond `limit` are truncated, only the visible prefix is sent to the callback, and the stream name is sent once;
3. an output callback `io::Error` is returned to the reader rather than swallowed.

Use a `Cursor<Vec<u8>>` for the input and a `Mutex<Vec<(String, Vec<u8>)>>` or a local callback closure for callback capture. Do not sleep or spawn a process in these tests.

- [ ] **Step 2: Run the focused tests**

Run `cargo test --lib runner::tests::read_stream`. Expected: the three tests pass without invoking a shell or taking more than one second.

- [ ] **Step 3: Preserve invariant handling and commit**

Do not replace `join_output`’s reader-thread `expect` with an expected-error branch: a panicked reader is an internal bug, not a caller-recoverable process failure. Run `cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, then commit:

```bash
but commit -b testability/runner-stream-tests -m "test: cover bounded runner stream capture" <ids>
```

### Task 3: Remove the duplicate planned-task accessor

Use `PlannedTask::id()` as the one identifier accessor. It already describes both plain task names and matrix node IDs.

**Files:**
- Modify: `src/project/mod.rs`
- Modify: `src/project/tests.rs`
- Modify: `src/cache/mod.rs`
- Modify: `src/runner.rs`
- Modify: `src/scheduler.rs`
- Modify: `src/commands/ci.rs`

- [ ] **Step 1: Replace every `PlannedTask::task` and `planned.task()` call with `id`**

Use `rg -n '\.task\(\)|PlannedTask::task' src tests` before and after the edit. Keep `TaskNode::id()` unchanged. The resulting `impl PlannedTask` must contain `id()` but no `task()` method.

- [ ] **Step 2: Run targeted project/cache/runner/scheduler tests**

Run:

```bash
cargo test --lib project::tests:: cache::tests:: runner::tests:: scheduler::tests::
```

Expected: all tests pass and no `PlannedTask::task` reference remains.

- [ ] **Step 3: Commit**

```bash
but commit -b testability/single-task-identifier -m "refactor: use one planned task identifier accessor" <ids>
```

---

## Phase B — Make scheduler side effects injectable

### Task 4: Introduce scheduler services and a testable entry point

The production `execute_plan` wrapper should remain the normal caller-facing internal function. Add an `execute_plan_with_services` function used by tests and by the wrapper so cache, environment, and retry sleeping are explicit inputs. Put the cache service trait beside `CacheStore` in `src/cache/mod.rs`; the scheduler consumes that trait, while the retry-sleeper trait remains scheduler-specific.

**Files:**
- Modify: `src/scheduler.rs`
- Modify: `src/cache/mod.rs` (define and implement the cache service trait for `CacheStore`, plus a test-only `CacheSession` constructor)

- [ ] **Step 1: Define the service contracts**

Add private crate-level traits with these responsibilities. Define `CacheBackend` in `src/cache/mod.rs` beside `CacheStore` and `CacheSession`; define `RetrySleeper` in `src/scheduler.rs`:

```rust
pub(crate) trait CacheBackend: Send + Sync {
    fn prepare(
        &self,
        project_root: &Path,
        environment: BTreeMap<String, String>,
    ) -> Result<CacheSession, CacheError>;
    fn task_key(
        &self,
        session: &CacheSession,
        task: &PlannedTask,
        dependency_keys: &[String],
    ) -> Result<String, CacheError>;
    fn lookup(
        &self,
        task: &PlannedTask,
        key: &str,
    ) -> Result<Option<TaskResult>, CacheError>;
    fn store(
        &self,
        task: &PlannedTask,
        key: &str,
        result: &TaskResult,
    ) -> Result<(), CacheError>;
}

trait RetrySleeper: Send + Sync {
    fn sleep(&self, duration: Duration);
}
```

Implement `CacheBackend` for `CacheStore` by forwarding to the existing methods. Add `CacheSession::for_test()` behind `#[cfg(test)]` so the scheduler fake can return a valid empty session without accessing the filesystem. Implement `RetrySleeper` for a small production `ThreadSleeper` that calls `thread::sleep`. Do not expose either trait through `lib.rs`.

- [ ] **Step 2: Make environment data an explicit execution input**

Move the `std::env::vars_os()` conversion into a small `ambient_environment() -> BTreeMap<String, String>` helper. The production wrapper calls it once and passes the resulting map to `execute_plan_with_services`. Tests pass `BTreeMap::new()` or a fixed map. Keep the current `to_string_lossy` behavior so non-Unicode environment variables remain non-panicking.

- [ ] **Step 3: Thread services through worker jobs**

Add a `SchedulerServices` value containing `Arc<dyn CacheBackend>` and `Arc<dyn RetrySleeper>`. `execute_plan_with_services` owns it; `WorkerJob` receives clones. Replace direct calls to `CacheStore::prepare`, `task_key_with_session`, `lookup`, `store`, and `thread::sleep` with the service methods. Preserve the existing `CacheMode::NoCache` behavior: no cache preparation, key calculation, lookup, or store may occur in that mode.

- [ ] **Step 4: Add deterministic fake services**

Inside `scheduler.rs`’s test module, add:

- `ScriptedCache` with fixed key results, optional hit results, and an injectable `CacheError` for prepare/key/lookup/store;
- `RecordingSleeper` that records every requested duration without sleeping.

The fake cache must return a valid `CacheSession` for successful preparation; use the existing `CacheSession` fields rather than adding a second cache-session model.

- [ ] **Step 5: Add service-path tests**

Add tests proving:

1. a cache hit returns a cached result and never calls the `TaskExecutor`;
2. a key failure returns `SchedulerError::Cache` before the executor runs;
3. a store failure reports `SchedulerError::Cache` after the task result is presented;
4. a task with `retries = 2` invokes the executor three times and records exactly the configured backoff twice without real sleeping;
5. `CacheMode::NoCache` does not call the fake cache at all;
6. the fixed environment map is passed to cache preparation rather than reading process-global state.

- [ ] **Step 6: Run the focused and full suites**

Run:

```bash
cargo test --lib scheduler::tests::
cargo test --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --check
```

Expected: all tests pass, no warning is emitted, and existing CLI cache behavior remains unchanged.

- [ ] **Step 7: Commit**

```bash
but commit -b testability/scheduler-services -m "refactor: inject scheduler cache and retry services" <ids>
```

### Task 5: Extract pure scheduler plan state transitions

`execute_plan_with_services` should orchestrate workers and output, not also be the only representation of dependency-state mutation. Introduce a private `PlanState` in `src/scheduler.rs`.

**Files:**
- Modify: `src/scheduler.rs`

- [ ] **Step 1: Define `PlanState` around existing collections**

Move these fields into `PlanState`: `tasks`, `remaining_dependencies`, `dependents`, `ready`, `active`, `active_groups`, `results`, `task_keys`, `cacheable`, and `stopping`. Its constructor must retain the existing duplicate-node assertion and return `UnresolvedDependency` for a dependency absent from the validated plan.

- [ ] **Step 2: Move state-only decisions into methods**

Implement methods with these contracts:

```rust
impl PlanState {
    fn new(plan: &[PlannedTask]) -> Result<Self, SchedulerError>;
    fn next_ready(&self) -> Option<TaskNode>;
    fn mark_dispatched(&mut self, node: &TaskNode);
    fn complete(
        &mut self,
        node: TaskNode,
        result: Result<TaskResult, RunnerError>,
        key: Option<String>,
        cache_error: Option<CacheError>,
    );
    fn finish(&self, plan: &[PlannedTask]) -> (ExecutionSummary, Option<TaskNode>);
}
```

`complete` must preserve the existing rules: a failed normal task stops new normal work; finalizers can become ready after normal failure; cache failure stops the run; resource groups are released when a task completes; successful dependencies decrement dependent counts; and blocked tasks remain absent from `results`.

- [ ] **Step 3: Rewrite the worker loop around `PlanState`**

Replace the parallel local maps and dependency mutation in `execute_plan_with_services` with calls to the new methods. Keep worker creation, channel receives, output rendering, and final error precedence in the orchestration function. Do not move output or cache I/O into `PlanState`.

- [ ] **Step 4: Add pure transition tests**

Use existing manifest fixtures and `TaskNode` values to test:

- constructor rejects an unresolved dependency;
- dispatching reserves a resource group and completion releases it;
- normal failure unblocks finalizers but not new normal tasks;
- cache failure stops dispatch and still permits the finalizer closure;
- completion classification accounts for every node exactly once;
- the first failed/cancelled node is selected in plan order.

These tests must not spawn a thread, write output, read the environment, or touch `.mono/cache`.

- [ ] **Step 5: Run the full verification suite**

Run `cargo fmt`, `cargo fmt --check`, `cargo test --workspace --all-targets --all-features`, and `cargo clippy --workspace --all-targets --all-features -- -D warnings`. Expected: the existing 204 tests plus the new pure transition tests pass.

- [ ] **Step 6: Commit**

```bash
but commit -b testability/scheduler-plan-state -m "refactor: isolate scheduler dependency state transitions" <ids>
```

---

## Phase C — Make runner lifecycle decisions deterministic

### Task 6: Add a process and clock adapter around `ManagedChild`

The current pure `poll_decision` function is retained. This task makes the driver that consumes it testable without changing platform-specific process-tree behavior.

**Files:**
- Modify: `src/runner.rs`
- Modify: `src/process.rs`

- [ ] **Step 1: Define the runner process vocabulary**

In `src/runner.rs`, add private types/traits equivalent to:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProcessExit {
    code: Option<i32>,
    success: bool,
}

trait ChildProcess: Send {
    fn stdout(&mut self) -> Option<Box<dyn Read + Send>>;
    fn stderr(&mut self) -> Option<Box<dyn Read + Send>>;
    fn try_wait(&mut self) -> io::Result<Option<ProcessExit>>;
    fn wait(&mut self) -> io::Result<ProcessExit>;
    fn terminate_tree(&mut self) -> io::Result<()>;
}

trait ProcessLauncher: Send + Sync {
    fn spawn(&self, spec: &ProcessSpec<'_>) -> io::Result<Box<dyn ChildProcess>>;
}
```

`ProcessSpec` carries the already validated program, arguments, working directory, environment, and stdin mode. `ProcessExit` replaces direct dependence on platform `ExitStatus` inside the runner loop.

- [ ] **Step 2: Add production adapters without changing kill semantics**

In `src/process.rs`, adapt `ManagedChild` to `ChildProcess` and add a production launcher that builds the same `Command` currently built in `Runner::run_with_options`. Preserve `ManagedChild::terminate_tree`, descendant cleanup, and Windows/Unix implementations byte-for-byte except for adapter plumbing. Missing piped stdout/stderr remains an invariant failure because the production launcher always requests both pipes.

- [ ] **Step 3: Add a clock/sleeper seam**

Define a private `RunnerClock` with `now() -> Instant` and `sleep(Duration)`. The production implementation delegates to `Instant::now` and `thread::sleep`. Change the driver to calculate elapsed time through the clock and to sleep through the seam. Keep the initial one-millisecond interval and 50-millisecond cap unchanged.

- [ ] **Step 4: Preserve the public `Runner` construction API**

`Runner::new()` must continue to create the production launcher and clock. Add a private constructor used only by unit tests to install fake services. Keep `Runner`’s public behavior and `TaskExecutor` implementation unchanged from callers’ perspectives.

- [ ] **Step 5: Add deterministic fake-driver tests**

Add a fake child with scripted `try_wait` results, in-memory stdout/stderr, and recorded termination calls, plus a fake clock whose `sleep` advances a monotonic duration. Test without shell commands:

1. a child that remains running crosses the timeout and returns `RunnerError::TimedOut` without real waiting;
2. cancellation terminates the child and returns `RunnerError::Cancelled`;
3. a scripted wait error returns `RunnerError::Wait`;
4. a callback error becomes `RunnerError::OutputRead` with the correct stream;
5. an exited child with unfinished readers invokes descendant cleanup, while an exited child with both readers complete does not;
6. exit status mapping still produces `RunnerError::Failed` with the original code.

Retain the existing Unix process-tree integration tests because only real subprocesses can prove signal/descendant behavior.

- [ ] **Step 6: Run focused, integration, and lint checks**

Run:

```bash
cargo test --lib runner::tests::
cargo test --test cli
cargo test --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --check
```

Expected: deterministic fake-driver tests pass quickly; real timeout, cancellation, output-limit, and descendant tests still pass; the full suite remains green.

- [ ] **Step 7: Commit**

```bash
but commit -b testability/runner-process-seam -m "refactor: inject runner process and clock services" <ids>
```

---

## Phase D — Final simplification and verification

### Task 7: Review the resulting boundaries and remove only proven duplication

**Files:**
- Modify only files changed by Tasks 1–6; this task may delete concrete duplication introduced by the new seams but must not introduce an unrelated refactor.

- [ ] **Step 1: Check the boundary rules**

Confirm all of the following:

- `main.rs` remains the only exit-code/stdout/stderr transport layer;
- `OutputSink` still owns rendering and writer locking;
- `CacheStore` remains the only production cache filesystem implementation;
- `ManagedChild` remains the only platform process-tree implementation;
- `PlanState` contains no I/O, sleeping, thread creation, or process-global reads;
- fake services are private test support and are not re-exported;
- every expected failure returns an existing error type, while every `expect`/`assert` still names an impossible invariant.

- [ ] **Step 2: Search for accidental bypasses**

Run:

```bash
rg -n 'std::env::vars_os|thread::sleep|CacheStore::|ManagedChild::|Command::new|\.task\(\)|PlannedTask::task' src
```

Expected: ambient environment collection and production side effects appear only in their documented adapters/wrappers; no duplicate planned-task accessor remains; scheduler tests do not bypass `SchedulerServices`.

- [ ] **Step 3: Run release-quality verification**

Run:

```bash
cargo fmt --check
cargo test --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Expected: formatting is clean, all tests pass, and Clippy reports no warnings.

- [ ] **Step 4: Commit the final cleanup**

```bash
but commit -b testability/round-3-cleanup -m "refactor: tighten execution core test boundaries" <ids>
```

---

## Self-review

- **Spec coverage:** F1 is addressed by Tasks 4–5; F2 by Task 6; F3 by Tasks 1–2; F4 by Task 3; F5 is explicitly protected by Task 7’s boundary review.
- **No behavior changes:** Existing cache format, event schema, CLI output, exit codes, process termination, and release behavior are covered by the unchanged integration tests and explicit verification commands.
- **Fail-fast consistency:** Service failures remain returned values. Internal state contradictions and impossible production adapter states remain panics with explanatory messages.
- **YAGNI check:** No new output abstraction or cosmetic file split is proposed. The process seam is limited to the interfaces needed to test current runner branches; platform process behavior remains in `process.rs` and continues to have real integration coverage.
- **Dependency check:** The plan adds no crates and uses only existing standard-library synchronization, I/O, time, and process types.

Plan complete and saved to `docs/superpowers/plans/2026-09-11-testability-and-code-improvements-round-3.md`. Two execution options:

1. **Subagent-Driven (recommended)** — dispatch a fresh subagent per task and review between tasks.
2. **Inline Execution** — execute the tasks in this session with checkpoints.

Which approach?
