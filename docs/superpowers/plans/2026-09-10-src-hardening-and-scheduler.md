# Source Hardening and Scheduler Improvements Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Harden `src/` into a validated, path-safe, deterministic monorepo orchestrator with correct concurrent execution, useful diagnostics, and focused tests.

**Architecture:** Keep TOML parsing, filesystem discovery, graph planning, process execution, scheduling, and output presentation as separate responsibilities. The planner will produce validated immutable task plans; the runner will only execute processes and return results; the scheduler will own dependency state and output ordering. Existing public command functions and manifest syntax remain compatible unless a previously unsafe/ambiguous behavior is rejected explicitly.

**Tech Stack:** Rust 2024, standard library filesystem/process/thread/channel APIs, existing `clap`, `serde`, and `toml` dependencies.

---

## File map

- Modify `src/config.rs`: add reusable manifest value validation and process-argument validation helpers.
- Create `src/identifiers.rs`: validated package/task/pipeline identifiers and task-reference parsing.
- Create `src/discovery.rs`: root discovery and member-pattern expansion.
- Create `src/graph.rs`: task graph construction, dependency resolution, cycle-path diagnostics, and deterministic root selection.
- Modify `src/workspace.rs`: compose discovery, validated manifests, graph planning, and validated `PlannedTask` construction; preserve the public workspace API where practical.
- Modify `src/runner.rs`: process execution only; return captured output and timing; never print from worker threads.
- Create `src/output.rs`: synchronized task-result presentation and dry-run formatting helpers.
- Create `src/scheduler.rs`: ready-queue bounded-concurrency scheduler.
- Modify `src/commands/ci.rs`: use the scheduler and output presenter; keep command-level errors and summaries.
- Modify `src/commands/init.rs`: write the generated manifest through a temporary file and atomic rename while preserving no-overwrite semantics.
- Modify `src/lib.rs`: register modules and export only validated domain types/functions.
- Modify `tests/cli.rs`: add CLI coverage for validation, execution failures, package selection, and output ordering.
- Modify `README.md`: document actual output behavior, path validation, scheduler behavior, and failure semantics.

---

### Task 1: Add validated identifiers and process-input validation

**Files:**
- Create: `src/identifiers.rs`
- Modify: `src/config.rs`
- Modify: `src/lib.rs`
- Test: `src/identifiers.rs`, `src/config.rs`

- [ ] **Step 1: Write failing identifier tests**

Add tests for:

```rust
assert!(Name::parse("build").is_ok());
assert!(Name::parse("").is_err());
assert!(Name::parse("build:test").is_err());
assert!(Name::parse("has\0nul").is_err());
assert!(TaskReference::parse("shared:build").is_ok());
assert!(TaskReference::parse("shared:build:extra").is_err());
assert!(TaskReference::parse(":build").is_err());
```

Add config validation tests rejecting NUL bytes in command arguments and environment keys/values.

- [ ] **Step 2: Run the focused tests and verify failure**

Run:

```bash
cargo test identifiers config
```

Expected: compilation/test failure because the validated types do not exist yet.

- [ ] **Step 3: Implement validated value types**

Create `Name` with private storage, `AsRef<str>`, `Display`, `Clone`, `Eq`, `Ord`, and typed wrappers or aliases for package/task/pipeline names. Add `TaskReference` with either a local task name or `(package, task)` pair. Validation must reject empty strings, `:`, and NUL bytes.

Add `validate_process_string(value, field)` in `config.rs` and call it for command arguments and environment entries. Return a structured validation error rather than allowing invalid values to reach `Command`.

- [ ] **Step 4: Run focused tests**

```bash
cargo test identifiers config
cargo fmt --all -- --check
```

Expected: PASS.

---

### Task 2: Separate discovery and make member traversal safe

**Files:**
- Create: `src/discovery.rs`
- Modify: `src/workspace.rs`
- Modify: `src/lib.rs`
- Test: `src/discovery.rs`, `src/workspace.rs`

- [ ] **Step 1: Add discovery failure tests**

Cover:

- root discovery from a nested directory
- literal, `*`, and `**` member patterns
- invalid absolute and `..` patterns
- overlapping patterns deduplicating the same canonical path
- symlinked member resolving outside the root being rejected
- non-directory member matches being ignored

- [ ] **Step 2: Extract discovery functions**

Move `find_root`, `read_manifest`, `expand_member_pattern`, `expand_segments`, `read_directories`, and `wildcard_matches` into `discovery.rs`. Keep all filesystem failures mapped to a discovery error with the relevant path.

The extracted API should be:

```rust
pub(crate) fn load_root(start: &Path) -> Result<DiscoveredRoot, DiscoveryError>;
pub(crate) fn discover_members(
    root: &Path,
    patterns: &[String],
) -> Result<Vec<PathBuf>, DiscoveryError>;
```

Every returned member must be canonicalized and must satisfy component-aware `starts_with(root)`. Reject symlink escapes consistently for both literal and wildcard paths.

- [ ] **Step 3: Run discovery tests**

```bash
cargo test discovery workspace
```

Expected: PASS, with existing workspace behavior unchanged.

---

### Task 3: Harden manifest and working-directory validation

**Files:**
- Modify: `src/config.rs`
- Modify: `src/workspace.rs`
- Test: `src/config.rs`, `src/workspace.rs`

- [ ] **Step 1: Add failing validation tests**

Add tests for:

- empty workspace/package/pipeline/task names
- empty pipeline task entries
- missing task `cwd`
- `cwd` containing `..`, absolute paths, Windows prefixes, or a symlink escape
- command arguments and environment entries containing NUL bytes
- package manifests containing root-only sections
- root manifests containing package-only sections

- [ ] **Step 2: Implement validation**

Validate all manifest values immediately after parsing. Use a path validator that only accepts normal and current-directory components and rejects parent, root, and prefix components. For each configured task directory:

1. join it to the canonical package path;
2. canonicalize it;
3. return an `InvalidTask` error if it does not exist or is not a directory;
4. reject it if the canonical path is outside the package root.

Store the canonical working directory in `PlannedTask`.

Validate all pipelines even when the workspace currently has no packages. An empty workspace created by `init` may still have an empty execution plan, but malformed pipeline references and names must not be silently accepted.

- [ ] **Step 3: Run validation tests**

```bash
cargo test config workspace
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: PASS with no warnings.

---

### Task 4: Extract graph planning and improve diagnostics

**Files:**
- Create: `src/graph.rs`
- Modify: `src/workspace.rs`
- Modify: `src/identifiers.rs`
- Test: `src/graph.rs`, `src/workspace.rs`

- [ ] **Step 1: Add graph tests**

Cover:

- deterministic dependency-first ordering
- local and cross-package dependencies
- missing packages and missing tasks
- cycle diagnostics containing the complete cycle path
- package selection including transitive dependencies
- qualified task selection outside `--package` returning an explicit error rather than an empty success
- duplicate roots being deduplicated

- [ ] **Step 2: Implement graph domain types**

Move task-reference parsing, `TaskNode`, visit state, dependency resolution, and cycle detection to `graph.rs`. Replace the single-node cycle error with a path-bearing error:

```rust
TaskCycle { path: Vec<TaskNode> }
```

Use `BTreeMap`/`BTreeSet` for stable ordering. Build a reverse-dependency map for scheduler use, but keep planning free of process execution.

- [ ] **Step 3: Restrict `PlannedTask` construction**

Make `PlannedTask` fields private. Construct it only from a validated workspace/task configuration. Keep `node()` and add accessors needed by `runner`, `scheduler`, and command output.

- [ ] **Step 4: Run graph tests**

```bash
cargo test graph workspace
```

Expected: PASS, including complete cycle messages.

---

### Task 5: Make the runner side-effect-free with structured results

**Files:**
- Modify: `src/runner.rs`
- Create: `src/output.rs`
- Test: `src/runner.rs`

- [ ] **Step 1: Add runner tests**

Use portable test commands available through the current platform abstraction, or add a small test-only command fixture. Cover:

- successful command output
- nonzero exit status
- spawn failure
- environment propagation
- canonical working directory
- stdout and stderr capture
- exit status without a numeric code

- [ ] **Step 2: Define the result contract**

Add:

```rust
pub struct TaskResult {
    pub output: CapturedOutput,
    pub elapsed: Duration,
}
```

Change the runner API to:

```rust
pub fn run(
    &self,
    workspace_root: &Path,
    planned: &PlannedTask,
) -> Result<TaskResult, RunnerError>;
```

The runner must not call `println!`, `eprintln!`, `print!`, or `emit_output`. It should only construct `Command`, execute it, capture output, measure duration, and return a structured error/result.

- [ ] **Step 3: Add synchronized presentation**

Create `OutputSink` in `output.rs` that owns a `Mutex` around stdout/stderr presentation. Add methods for serial task sections and GitHub Actions sections. Emit child output before closing a section.

Keep dry-run formatting separate from process output and ensure command arguments are shell-safe for display only, never for execution.

- [ ] **Step 4: Run runner tests**

```bash
cargo test runner
cargo fmt --all -- --check
```

Expected: PASS.

---

### Task 6: Implement a ready-queue scheduler

**Files:**
- Create: `src/scheduler.rs`
- Modify: `src/commands/ci.rs`
- Modify: `src/runner.rs`
- Test: `src/scheduler.rs`, `src/commands/ci.rs`

- [ ] **Step 1: Add scheduler tests**

Build a fake runner or injectable execution function and test:

- dependencies complete before dependents start
- no more than `jobs` tasks run simultaneously
- independent tasks overlap when `jobs > 1`
- newly-ready work starts without waiting for unrelated tasks
- failed tasks prevent dependents from running
- all already-running tasks are joined before returning an error
- result presentation follows deterministic scheduler order

- [ ] **Step 2: Define scheduler inputs and outputs**

Use a scheduler API similar to:

```rust
pub(crate) fn execute_plan(
    workspace: &Workspace,
    plan: &[PlannedTask],
    jobs: NonZeroUsize,
    runner: &Runner,
    output: &OutputSink,
) -> Result<(), SchedulerError>;
```

Build an indegree map and reverse-dependency map from the validated plan. Maintain a sorted ready queue. Spawn at most `jobs` workers, receive results through a channel, update dependency counts, and enqueue newly-unblocked tasks immediately.

- [ ] **Step 3: Centralize failure handling**

Workers return errors; they do not print them. The scheduler records the first deterministic error, stops scheduling new work, waits for active workers, presents completed results, and returns the command error.

- [ ] **Step 4: Wire `commands::ci` to the scheduler**

Preserve `jobs == 0` validation at the command boundary. Remove the old batch loop and all direct runner output calls. Keep summaries and dry-run behavior unchanged except for corrected output ordering.

- [ ] **Step 5: Run scheduler tests**

```bash
cargo test scheduler commands::ci
cargo test --all-targets
```

Expected: PASS.

---

### Task 7: Make `init` atomic

**Files:**
- Modify: `src/commands/init.rs`
- Test: `src/commands/init.rs`, `tests/cli.rs`

- [ ] **Step 1: Add an initialization failure test**

Test that a failed write leaves no partial manifest and that an existing manifest is never overwritten. Keep the current `create_new` no-overwrite contract.

- [ ] **Step 2: Implement temporary-file publication**

Create a unique temporary file in the target directory with `create_new`, write the rendered manifest, flush it, optionally call `sync_all`, then rename it into place only if the destination still does not exist. Remove the temporary file on every failure.

If the platform cannot atomically preserve no-overwrite behavior through rename, retain the current destination `create_new` reservation and document the fallback rather than risking overwrite.

- [ ] **Step 3: Run init tests**

```bash
cargo test commands::init
cargo test --test cli
```

Expected: PASS.

---

### Task 8: Refine module boundaries and public API

**Files:**
- Modify: `src/lib.rs`
- Modify: `src/workspace.rs`
- Modify: `src/runner.rs`
- Modify: `src/commands/mod.rs`
- Test: all existing tests

- [ ] **Step 1: Register new modules**

Add `identifiers`, `discovery`, `graph`, `output`, and `scheduler` to the crate. Keep implementation details `pub(crate)` unless library consumers need them.

- [ ] **Step 2: Remove redundant public state**

Make `ExecutionContext` internal or remove it if it no longer carries independent behavior. Keep `Workspace`, `Package`, `TaskNode`, and `PlannedTask` as stable public domain types with private fields and accessors.

- [ ] **Step 3: Preserve command error conversion**

Ensure all expected filesystem, manifest, graph, runner, and scheduler failures remain represented as `Result` values. Panics should remain limited to broken internal invariants such as impossible worker protocol violations.

- [ ] **Step 4: Run the complete suite**

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

Expected: all commands pass.

---

### Task 9: Update end-to-end tests and documentation

**Files:**
- Modify: `tests/cli.rs`
- Modify: `README.md`

- [ ] **Step 1: Add CLI failure coverage**

Add tests for:

- invalid `cwd` rejected by `doctor`
- missing task dependency rejected before execution
- failed task returns a nonzero exit code and includes package/task context
- `--package` with an invalid qualified root returns an error
- `--jobs 0` returns a CLI error
- `plan` and `graph` remain deterministic

- [ ] **Step 2: Update documentation**

Document that:

- command arguments are executed directly without a shell;
- configured `cwd` must exist inside the package after symlink resolution;
- parallel tasks are scheduled as soon as dependencies complete;
- task output is captured and presented by the scheduler in deterministic order;
- `doctor` validates all manifests, task references, command inputs, and working directories;
- task cycles show their complete dependency path.

Correct the existing claim that child processes inherit standard IO if output remains captured.

- [ ] **Step 3: Run the release-quality checks**

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo run -- init /tmp/monorelease-smoke
cargo run -- doctor /tmp/monorelease-smoke
```

Expected: formatting, linting, unit tests, integration tests, and smoke commands all pass.

---

## Self-review

- Path safety: covered by Tasks 2 and 3.
- Output ordering and CI sections: covered by Tasks 5 and 6.
- Real bounded concurrency: covered by Task 6.
- Invalid public state: covered by Tasks 1, 4, and 8.
- Graph diagnostics and package selection: covered by Task 4.
- Atomic initialization: covered by Task 7.
- Test coverage and documentation: covered by Task 9.
- Existing CLI surface and manifest syntax are retained except for explicit rejection of unsafe or ambiguous inputs.
