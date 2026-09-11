# Testability and Code Improvements Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the `src/` application layer deterministic and directly testable while preserving the current CLI behavior, error vocabulary, output contracts, and process-tree safety guarantees.

**Architecture:** Keep the existing pure domain cores (`Project`, `PlanState`, `poll_decision`, changelog parsing, release verification) intact. Move production wiring to explicit runtime seams, keep filesystem/process effects at the edges, and test each boundary with either deterministic fakes or narrowly scoped integration tests. The binary should remain a thin transport adapter that maps parsed commands and domain results to stdout/stderr and exit codes.

**Tech Stack:** Rust 2024, Cargo unit tests, integration tests under `tests/`, `serde_json` contract assertions, `Arc<dyn Trait>` dependency seams already used by `runner.rs` and `scheduler.rs`.

---

## Findings and priorities

The baseline is healthy: `cargo test --all-targets` currently passes **228 tests** (185 library, 17 binary, 21 CLI, 5 release CLI), and `cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings` passes.

Existing strengths to preserve:

- `src/project/` has a useful pure planning core and extensive graph/matrix validation tests.
- `src/runner.rs` already injects `ProcessLauncher` and `RunnerClock` and has deterministic fake-child tests for timeout, cancellation, output limits, and reader cleanup.
- `src/scheduler.rs` already injects cache, retry sleeping, and environment services, and `PlanState` is isolated from I/O.
- `src/output.rs` accepts caller-owned writers and has a pinned-identity test constructor.
- `src/release.rs` already isolates Git verification behind `verify_source_with`.

Highest-value testability problems:

1. `src/commands/ci.rs` still wires `Project::load`, `Runner::new`, `OutputSink::new`, and the production scheduler together, so command-level tests must spawn real subprocesses or duplicate scheduler tests.
2. `src/main.rs` combines CLI parsing, Ctrl-C registration, command dispatch, output transport, and exit-code policy. The small pure helpers are tested, but the dispatch matrix is not directly testable without launching the binary.
3. `src/cache/mod.rs` and the command wrappers mix filesystem work with formatting/workflow decisions. The current tests are valuable, but most failures require a real temporary directory and several error paths are untested.
4. `src/process.rs` has the riskiest unmocked code: platform APIs, process-group setup, a hard-coded grace sleep, and `Drop` cleanup. The runner seam does not prove the actual Unix/Windows implementation works.
5. `src/testing.rs` and the two integration-test files each implement their own temporary-directory logic. The helper uses wall-clock time and `expect`, which can make test failures less diagnosable and can collide under unusual parallel runs.

The work below is intentionally staged. The first two tasks improve the largest seams without changing domain behavior; the latter tasks add safety coverage around filesystem and process effects.

---

### Task 1: Introduce an injectable pipeline runtime

**Files:**
- Modify: `src/commands/ci.rs`
- Modify: `src/scheduler.rs`
- Test: `src/commands/ci.rs`

**Purpose:** Test the command-level pipeline workflow with a scripted `TaskExecutor`, fake cache, recording sleeper, and captured `OutputSink`, without launching a child process. Keep `run_pipeline_with_mode` as the production convenience wrapper.

- [x] **Step 1: Define the runtime seam at the application boundary.**

Add a crate-visible service bundle in `src/commands/ci.rs` rather than exposing scheduler implementation details publicly:

```rust
pub(crate) struct PipelineServices {
    pub(crate) runner: Arc<dyn TaskExecutor>,
    pub(crate) output: Arc<OutputSink>,
    pub(crate) cache: Arc<dyn CacheBackend>,
    pub(crate) sleeper: Arc<dyn RetrySleeper>,
    pub(crate) environment: BTreeMap<String, String>,
}
```

Add a crate-visible function that accepts an already loaded project and plan:

```rust
pub(crate) fn execute_loaded_pipeline(
    project: &Project,
    plan: &[PlannedTask],
    jobs: usize,
    execution: &PipelineExecution,
    services: &PipelineServices,
) -> Result<ExecutionSummary, CiError> {
    if jobs == 0 {
        return Err(CiError::InvalidJobs);
    }
    execute_plan_with_services(
        project,
        plan,
        jobs,
        Arc::clone(&services.runner),
        &services.output,
        execution.cache,
        &execution.cancellation,
        Arc::clone(&services.cache),
        Arc::clone(&services.sleeper),
        services.environment.clone(),
    )
    .map_err(Into::into)
}
```

Use the actual existing imports and error conversion in the implementation; do not duplicate project loading or plan construction in this function.

- [x] **Step 2: Split scheduler service execution from production service construction.**

In `src/scheduler.rs`, extract production service construction into `production_services` and make the scheduler options boundary crate-visible. The command layer passes a `SchedulerOptions` reference to a crate-visible `execute_plan_with_services`; the scheduler loop itself remains behind the private `execute_plan_with_options` implementation. This avoids exposing scheduler implementation details outside the crate while keeping the service boundary injectable.

```rust
pub(crate) fn execute_plan_with_services(
    project: &Project,
    plan: &[PlannedTask],
    jobs: usize,
    runner: Arc<dyn TaskExecutor>,
    output: &Arc<OutputSink>,
    cache_mode: CacheMode,
    cancellation: &CancellationToken,
    cache: Arc<dyn CacheBackend>,
    sleeper: Arc<dyn RetrySleeper>,
    environment: BTreeMap<String, String>,
) -> Result<ExecutionSummary, SchedulerError> {
    let services = SchedulerServices {
        cache,
        sleeper,
        environment,
    };
    let options = SchedulerOptions {
        jobs,
        cache_mode,
        cancellation,
        services: &services,
    };
    execute_plan_with_options(project, plan, runner, output, &options)
}
```

Rename the current private implementation to `execute_plan_with_options`. Have `production_services` construct `CacheStore`, `ThreadSleeper`, and `ambient_environment()`. Preserve the existing `assert!(options.jobs > 0)` as the invariant check; `CiError::InvalidJobs` remains the expected caller-facing failure.

- [x] **Step 3: Make the production command wrapper use the seam.**

Change `run_pipeline_with_mode` to load and plan exactly once, then construct production services only for the non-dry-run path. Use the already-built plan for JSON dry runs instead of calling `plan_with_output` and loading the project a second time:

```rust
let project = Project::load(path)?;
let plan = project.plan(pipeline, requested_tasks)?;
if dry_run {
    return if execution.output == OutputMode::Json {
        serde_json::to_string(&PlanDocument::from((&project, plan.as_slice())))
            .map_err(|source| CiError::Json { source })
    } else {
        Ok(format_plan(&project, &plan))
    };
}

let root = Arc::new(project.root.clone());
let services = PipelineServices {
    runner: Arc::new(Runner::new()),
    output: Arc::new(OutputSink::new(execution.output)),
    cache: Arc::new(CacheStore::new(&root)),
    sleeper: Arc::new(ThreadSleeper),
    environment: ambient_environment(),
};
let summary = execute_loaded_pipeline(&project, &plan, jobs, &execution, &services)?;
```

If module visibility prevents direct construction of `ThreadSleeper` or `ambient_environment`, expose only the smallest `pub(crate)` production factory in `scheduler.rs`; do not make scheduler internals public outside the crate.

- [x] **Step 4: Add command-level deterministic tests before changing existing integration tests.**

In `src/commands/ci.rs`, reuse the existing scripted executor/cache/sleeper patterns from `src/scheduler.rs` through small test helpers. Add tests for these cases:

```rust
#[test]
fn loaded_pipeline_uses_the_injected_executor_and_returns_summary() {
    // Build a TempDir manifest with two dependency-ordered tasks.
    // Build a terminal test sink, scripted executor, and NoCache services.
    // Load and plan once, call execute_loaded_pipeline, then assert:
    // executor calls == ["base", "app"] and summary.completed == 2.
}

#[test]
fn loaded_pipeline_does_not_invoke_the_executor_when_the_injected_cache_hits() {
    // Use a cacheable manifest and a scripted cache returning cached=true.
    // Assert executor call list is empty and summary.cached == 1.
}

#[test]
fn loaded_pipeline_rejects_zero_jobs_before_dispatching() {
    // Pass jobs=0 with a scripted executor whose call list starts empty.
    // Assert CiError::InvalidJobs and no executor call.
}
```

Implement the fixtures with concrete manifest strings and the existing `TempDir` helper. The first test must use a `base` task and an `app` task depending on `base`; the second must use `cache = true`, one input pattern, one output pattern, and a scripted cache hit; the third must pass `jobs = 0`. Do not invoke `sh`, `sleep`, or the production `Runner` in these tests.

- [x] **Step 5: Run the focused and full suites.**

Run:

```bash
cargo test commands::ci::tests
cargo test scheduler::tests
cargo test --all-targets
```

Expected: all tests pass, with the existing CLI behavior unchanged. Commit this task separately as `test: inject pipeline runtime services`.

---

### Task 2: Separate CLI transport from command dispatch and lock down contracts

**Files:**
- Modify: `src/main.rs`
- Modify: `src/commands/ci.rs`
- Modify: `src/commands/list.rs`
- Test: `src/main.rs`
- Test: `src/commands/ci.rs`
- Test: `src/commands/list.rs`
- Test: `tests/cli.rs`

**Purpose:** Keep Ctrl-C, stdout/stderr, JSON serialization, and exit codes at the binary boundary. Make dispatch and document creation directly testable so tests do not need to spawn `mono` for every output-contract case.

- [x] **Step 1: Extract pure output-document builders from `main.rs`.**

Keep the existing serializable structs and refactor the existing `success_document`, `check_document`, and `exit_code` helpers into a small pure transport section. Add `error_document` and keep all four functions explicit in their return types:

```rust
fn success_document(output: OutputMode, kind: &'static str, message: String) -> String;
fn check_document(output: OutputMode, root: &Path) -> String;
fn error_document(error: &Error) -> String;
fn exit_code(error: &Error) -> u8;
```

`error_document` must serialize `ErrorDocument { schema, kind: "error", code, message }` and must not write to any stream. Keep `serialize` as the single fail-fast boundary for the known-serializable document structs. `emit_summary` and `emit_error` should only write the strings returned by these builders and map `BrokenPipe` as they do now.

- [x] **Step 2: Extract command dispatch into a function with no signal or stream side effects.**

Rename the current `run` implementation to `dispatch` or move it into a crate-visible helper with this contract:

```rust
fn dispatch(
    root: PathBuf,
    output: OutputMode,
    command: Option<Commands>,
    cancellation: CancellationToken,
) -> Result<String, Error>;
```

`main` should only parse `Cli`, install the Ctrl-C handler, call `dispatch`, and call `emit_summary` or `emit_error`. Do not move `ctrlc::set_handler`, `ExitCode`, or `std::io::stdout/stderr` into the dispatch helper.

- [x] **Step 3: Add direct unit tests for the extracted contracts.**

In `src/main.rs`, add tests that assert:

```rust
#[test]
fn error_document_is_newline_delimited_json_ready() {
    let error = Error::Ci(CiError::InvalidJobs);
    let value: serde_json::Value = serde_json::from_str(&error_document(&error))
        .expect("error document is valid JSON");
    assert_eq!(value["kind"], "error");
    assert_eq!(value["code"], 2);
    assert!(value["message"].as_str().is_some());
}

#[test]
fn dispatch_rejects_invalid_jobs_without_loading_a_project() {
    let error = dispatch(
        PathBuf::from("does-not-exist"),
        OutputMode::Terminal,
        Some(Commands::Run {
            pipeline: None,
            tasks: Vec::new(),
            options: ExecutionOptions { jobs: 0, ..ExecutionOptions::default() },
        }),
        CancellationToken::new(),
    )
    .expect_err("zero jobs must fail");
    assert!(matches!(error, Error::Ci(CiError::InvalidJobs)));
}
```

Use the actual `Commands` variant fields from `main.rs`; if Clap stores execution options differently, construct the parsed value using those existing fields rather than adding a second command representation.

- [x] **Step 4: Test JSON/terminal renderers at their owning command modules.**

Add pure assertions in `src/commands/ci.rs` for `plan_with_output` and `graph_with_output`, and in `src/commands/list.rs` for `list_with_output`:

- JSON has `schema`, the correct `kind`, stable task order, and no environment values.
- Terminal plan redacts every environment value and preserves dependency order.
- Graph JSON contains one edge object per planned task.
- List JSON contains the default pipeline, pipeline finalizers, task command, and stdin mode.

Keep the existing CLI tests for one end-to-end representative per output mode; replace only tests that duplicate pure serialization assertions.

- [x] **Step 5: Run focused transport and CLI tests.**

Run:

```bash
cargo test --bin mono
cargo test commands::ci::tests commands::list::tests
cargo test --test cli
cargo test --all-targets
```

Expected: exit codes and JSON schemas remain byte-compatible except for intentionally nondeterministic run identity values. Commit as `test: isolate cli dispatch and output contracts`.

---

### Task 3: Make filesystem workflows easier to test without hiding real I/O failures

**Files:**
- Modify: `src/cache/mod.rs`
- Modify: `src/atomic_file.rs`
- Modify: `src/commands/changelog.rs`
- Modify: `src/commands/init.rs`
- Modify: `src/testing.rs`
- Create: `tests/support/mod.rs`
- Test: `src/cache/mod.rs`
- Test: `src/atomic_file.rs`
- Test: `src/commands/changelog.rs`
- Test: `src/commands/init.rs`
- Test: `tests/cli.rs`
- Test: `tests/release_cli.rs`

**Purpose:** Keep security-sensitive cache and atomic-write behavior on real temporary filesystems, but move decision logic into pure helpers and make failure paths observable. Do not introduce a fake filesystem for the whole application; that would obscure path/symlink behavior that must be tested against the OS.

- [x] **Step 1: Replace the test-only temp directory's wall-clock naming with collision-safe creation.**

Change `src/testing.rs` so `TempDir::new()` uses the process ID and an atomic counter, then retries `fs::create_dir` on `AlreadyExists` instead of calling `SystemTime::now().expect(...)` and `create_dir_all`. The invariant is that the helper either returns a uniquely created directory or panics with the exact path and I/O error because the test fixture cannot be created.

Use this shape:

```rust
pub fn new() -> Self {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    for _ in 0..100 {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir()
            .join(format!("mono-{}-{id}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Self(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("create temp dir {}: {error}", path.display()),
        }
    }
    panic!("could not allocate a unique temporary directory after 100 attempts");
}
```

Import `std::io`; preserve `Drop` cleanup. Add a unit test that creates two helpers concurrently and asserts their paths differ.

- [x] **Step 2: Extract cache metadata validation and environment selection into pure functions.**

In `src/cache/mod.rs`, add private pure helpers:

```rust
fn select_cache_environment(
    task: &PlannedTask,
    ambient: &BTreeMap<String, String>,
) -> BTreeMap<String, String>;

fn valid_metadata(metadata: &CacheMetadata, key: &str) -> bool;
```

`select_cache_environment` must preserve the current rules: `cache_env = ["*"]` includes the ambient map plus task overrides; named variables use task values first, ambient values second, and `"<unset>"` otherwise. `valid_metadata` must return false for a wrong version or key. Replace the inline branches in `task_key_with_session` and `lookup` with these helpers.

Add table-driven tests for wildcard, task override, ambient value, unset value, and metadata version/key mismatches. These tests must not touch the filesystem.

- [ ] **Step 3: Add explicit cache error-path tests around the existing real filesystem boundary.**

Add tests in `src/cache/mod.rs` for:

- malformed `metadata.json` being treated as a cache miss;
- metadata referencing a missing output being treated as a cache miss without creating destination files;
- a cache entry whose output digest differs from metadata being treated as a cache miss;
- a failed output copy leaving all earlier destination files unchanged;
- `store` cleaning its temporary directory after a write failure.

Use separate `TempDir` fixtures for each test and assert both returned values and filesystem state. Keep the existing symlink tests; they are security regression tests, not candidates for fake I/O.

- [ ] **Step 4: Add atomic-write failure assertions and command wrapper coverage.**

In `src/atomic_file.rs`, add tests that verify a failed `New` write leaves the existing destination byte-for-byte unchanged and that a failed replacement does not leave the temporary file. If inducing a portable write failure is not possible with ordinary permissions, test the already-existing destination/refusal branch and document that OS-level permission failures remain integration coverage.

In `src/commands/changelog.rs` and `src/commands/init.rs`, add direct tests for:

- missing input file returning `Read`/`Invalid` with the correct path;
- empty changelog entry being rejected without writing the notes file;
- `init` refusing overwrite while preserving the original manifest bytes;
- scaffold output using `scaffold_on`'s explicit date and never consulting the clock.

- [x] **Step 5: Consolidate integration-test fixtures.**

Move the duplicated `TempDir` implementation in `tests/cli.rs` and `tests/release_cli.rs` to the new `tests/support/mod.rs`. The shared helper must expose:

```rust
pub struct TempDir { /* owned path */ }
impl TempDir {
    pub fn new(name: &str) -> Self;
    pub fn path(&self) -> &Path;
}
pub fn mono(args: &[&str], cwd: &Path) -> Output;
```

Keep test names and assertions focused on externally visible behavior. Do not make production `src/testing.rs` available to integration tests because it is compiled only under `cfg(test)` in the library.

- [ ] **Step 6: Run cache, atomic, command, and full suites.**

Run:

```bash
cargo test cache::tests atomic_file::tests commands::changelog::tests commands::init::tests
cargo test --test cli --test release_cli
cargo test --all-targets
```

Expected: no cache or release artifacts survive test teardown, and all existing symlink/path-safety tests remain green. Commit as `test: cover filesystem workflow failure paths`.

---

### Task 4: Test the actual process-tree boundary and remove timing fragility from tests

**Files:**
- Modify: `src/process.rs`
- Modify: `src/runner.rs`
- Test: `src/process.rs`
- Test: `src/runner.rs`
- Test: `tests/cli.rs`

**Purpose:** The runner fakes prove the control loop, but the platform adapter still needs bounded tests proving that the real child process and its descendants are cleaned up. Tests must not wait several seconds merely to prove a marker was not written.

- [ ] **Step 1: Extract the Unix termination timing policy from the syscall wrapper.**

In `src/process.rs`, keep `ManagedChild` responsible for owning `Child` and platform state, but move the Unix sequence into a helper whose policy is explicit:

```rust
#[cfg(unix)]
fn terminate_group_with(
    child: &mut Child,
    pgid: i32,
    grace: Duration,
    sleep: &mut dyn FnMut(Duration),
) -> io::Result<()>;
```

Production `terminate_tree_unix` passes `Duration::from_millis(100)` and `&mut std::thread::sleep`. The helper must preserve the current behavior: SIGTERM is best effort, SIGKILL is sent to the process group, `ESRCH` is accepted, and a non-existent group is only tolerated when the direct child has already exited. This gives unit tests a zero-duration recording sleeper for the policy while leaving the production safety behavior unchanged.

If the Windows implementation has an equivalent policy decision, keep it in a separate `#[cfg(windows)]` helper rather than adding cross-platform conditionals to the Unix test code.

- [ ] **Step 2: Add platform-specific process lifecycle tests.**

On Unix, add tests that use `ManagedChild::spawn` with a shell command and assert:

1. stdout/stderr can be taken and the child can be reaped after normal exit;
2. `terminate_tree` followed by `wait` returns without leaving a running child;
3. a descendant holding the output pipe is terminated when the parent exits and the runner invokes cleanup;
4. a successful child does not terminate a detached descendant, preserving the existing runner regression test.

Use `kill -0` or a marker file with a short bounded deadline rather than unconditional multi-second sleeps. A polling helper should have a fixed maximum duration and return a test failure with the child PID and marker path when the deadline expires.

On Windows, retain the existing detached-descendant regression test and add a direct spawn/terminate/reap test around the Job Object path.

- [ ] **Step 3: Add runner tests for all launcher contract failures.**

Extend the existing fake launcher tests in `src/runner.rs` with fakes that return:

- `None` for stdout;
- `None` for stderr;
- a spawn `io::Error`;
- a terminate-tree `io::Error` during timeout or cancellation;
- a reader `io::Error` from stdout or stderr.

The missing pipe cases should remain fail-fast assertions because `ProductionLauncher` promises piped streams. Spawn, termination, wait, and reader failures must return the corresponding `RunnerError` variant with project, task, stream, and operation details.

- [ ] **Step 4: Make cancellation integration bounded and diagnosable.**

In `tests/cli.rs`, replace the unconditional two-second pre-signal sleep in `an_interrupted_run_does_not_report_success` with a bounded readiness protocol. The task command should create a `started` marker before entering its long sleep; the test should poll for that marker until a deadline, send SIGINT only after the marker exists, and include the captured child output in the failure message. This keeps the test deterministic without assuming a fixed machine startup time.

- [ ] **Step 5: Run platform and full validation.**

Run on Unix:

```bash
cargo test process:: runner::tests
cargo test --test cli an_interrupted_run_does_not_report_success
cargo test --all-targets
```

Run on Windows in CI:

```powershell
cargo test process:: runner::tests
cargo test --all-targets
```

Expected: process-tree cleanup tests are bounded, no test relies on a fixed multi-second sleep for readiness, and normal successful descendants remain alive as specified. Commit as `test: cover platform process lifecycle boundary`.

---

## Final review checklist

- [ ] `cargo fmt --all -- --check` passes.
- [ ] `cargo clippy --all-targets -- -D warnings` passes.
- [ ] `cargo test --all-targets` passes on Unix.
- [ ] Windows CI covers the Job Object code path.
- [ ] No production function silently swallows an expected I/O failure.
- [ ] Assertions remain only for impossible states or violated internal contracts; user input, filesystem, process, cache, and output failures remain `Result` values.
- [ ] New seams are crate-private unless they are part of the documented public API.
- [ ] Existing JSON schemas, terminal summaries, exit codes, cache safety rules, and process-tree semantics are unchanged.
- [ ] Each task is committed independently so a regression can be bisected or reverted without mixing domain changes and test-fixture changes.

**Recommended implementation order:** Task 1, Task 2, Task 3, Task 4. Task 1 is the highest-leverage change because it removes real subprocesses from command-level tests; Task 4 should remain last because platform-process tests are inherently more expensive and environment-sensitive.
