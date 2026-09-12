# CLI Testability and IO Separation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Apply the CLI architecture improvements without changing command behavior, output contracts, exit codes, or release workflows.

**Architecture:** Keep `main.rs` as a thin process adapter. Convert command/core APIs from formatted `String` results to structured domain results, isolate filesystem/Git/process/terminal effects behind focused ports, and render text/JSON/TUI output at the edge. Preserve the existing pure scheduler and runner decision cores, extending them rather than replacing them.

**Tech Stack:** Rust 2024, clap, serde/serde_json, existing `ExecutionEvent` model, existing `ProcessLauncher`/`RunnerClock`/`CacheBackend` seams, cargo test, cargo clippy.

---

## Files and responsibilities

### Create

- `src/app.rs` — application request types, resolved project context, and command execution orchestration.
- `src/render.rs` — text and JSON rendering for non-execution command results and errors.
- `src/io.rs` — focused filesystem and Git ports with production implementations.

### Modify

- `src/main.rs` — retain clap parsing, Ctrl-C setup, process writers, terminal detection, and exit-code mapping; remove command-specific formatting and effectful orchestration.
- `src/lib.rs` — expose structured command/domain result types and the application-facing APIs.
- `src/project/mod.rs` — add pure `Project::from_config` construction and keep loading as an adapter path.
- `src/discovery.rs` — move discovery/read operations behind the focused filesystem boundary and remove duplicate schema validation.
- `src/commands/ci.rs` — return plans/summaries instead of text and stop serializing JSON here.
- `src/commands/list.rs` — return a structured project description.
- `src/commands/changelog.rs` — separate changelog transformations from file/Git operations.
- `src/commands/release.rs` and `src/release.rs` — separate release-domain validation from artifact filesystem traversal and writes.
- `src/scheduler.rs` — report structured execution events through a narrow reporter seam instead of concrete `OutputSink`.
- `src/output.rs` — implement the reporter/rendering adapter over `ExecutionEvent`.
- `src/cache/mod.rs` — separate input snapshots from pure cache-key calculation.
- `src/runner.rs` — preserve current seams; only extract lifecycle policy if needed to keep the refactor testable.
- `src/tui.rs` — keep terminal drawing behind the output adapter and make state transitions independently testable.
- `tests/cli.rs` and `tests/release_cli.rs` — add contract tests for root resolution and unchanged output/exit behavior.

### Separate lower-priority follow-up

- `xtask/src/main.rs`, `xtask/src/process.rs`, `xtask/src/release.rs`, and `xtask/src/tag.rs` — apply the same structured-result and process-adapter pattern after the published CLI is complete.

---

### Task 1: Establish a clean baseline and protect current contracts

**Files:**
- Test: `tests/cli.rs`
- Test: `tests/release_cli.rs`
- Test: `src/main.rs`

- [ ] **Step 1: Add contract tests for nested path resolution.**

Add tests that create a manifest at a temporary root, invoke the binary with `--dir` pointing at a nested directory, and verify that project commands and changelog/release path commands use the documented path base. Cover `plan`, `changelog check`, and `release manifest`.

- [ ] **Step 2: Run the focused integration tests.**

Run:

```bash
cargo test --test cli --test release_cli
```

Expected: existing tests pass; the new path-resolution tests expose the current distinction between the search path and discovered project root.

- [ ] **Step 3: Document the chosen path contract in `README.md` and `docs/commands.md`.**

Use this contract: `--dir` is the search start, and project-relative files are resolved from the discovered `mono.toml` root. Absolute paths remain unchanged.

- [ ] **Step 4: Commit the behavior specification.**

```bash
git add tests/cli.rs tests/release_cli.rs README.md docs/commands.md
git commit -m "test: define CLI project-root path semantics"
```

---

### Task 2: Add project context and pure project construction

**Files:**
- Modify: `src/project/mod.rs`
- Modify: `src/discovery.rs`
- Create: `src/io.rs`
- Test: `src/project/tests.rs`
- Test: `src/discovery.rs`

- [ ] **Step 1: Add a pure constructor test.**

Construct a `MonoConfig` in memory and assert that `Project::from_config` validates it, stores the supplied absolute root, and produces the expected dependency-first plan without creating `mono.toml` on disk.

- [ ] **Step 2: Implement `Project::from_config`.**

Move schema, identifier, task, graph, and pipeline validation currently performed in `Project::load` into:

```rust
pub fn from_config(root: PathBuf, config: MonoConfig) -> Result<Self, ProjectError>
```

Keep invariant assertions for impossible validated states. The function must not read files, canonicalize paths, inspect the environment, or spawn processes.

- [ ] **Step 3: Add focused filesystem primitives.**

Define a small `FileSystem` port in `src/io.rs` for canonicalization, metadata checks, text reads, and writes. Provide a production implementation using `std::fs`. Do not create one giant mockable filesystem API; only add operations required by discovery and command adapters.

- [ ] **Step 4: Update discovery to return parsed configuration once.**

Have discovery locate and parse the nearest manifest. Remove the second schema-validation call from `Project::load`; schema validation belongs to pure project construction.

- [ ] **Step 5: Run project and discovery tests.**

Run:

```bash
cargo test project:: discovery::
```

Expected: all existing project/discovery tests pass, including matrix, cycle, path, schema, and missing-root cases.

- [ ] **Step 6: Commit the loader/core split.**

```bash
git add src/io.rs src/project/mod.rs src/project/tests.rs src/discovery.rs
 git commit -m "refactor: separate project construction from manifest IO"
```

---

### Task 3: Introduce structured command results and edge rendering

**Files:**
- Create: `src/app.rs`
- Create: `src/render.rs`
- Modify: `src/lib.rs`
- Modify: `src/main.rs`
- Modify: `src/commands/ci.rs`
- Modify: `src/commands/list.rs`
- Test: `src/main.rs`
- Test: `src/commands/ci.rs`
- Test: `src/commands/list.rs`

- [ ] **Step 1: Define structured result types.**

Add result types for:

```rust
pub struct PlanResult { pub project: ProjectSummary, pub tasks: Vec<PlanTask> }
pub struct ProjectDescription { pub project: ProjectSummary, pub pipelines: Vec<...>, pub tasks: Vec<...> }
pub struct ExecutionSummary { ... }
pub struct CommandSuccess { pub kind: &'static str, pub message: String }
```

Keep execution lifecycle data in `ExecutionEvent`; do not duplicate event payloads in these summaries.

- [ ] **Step 2: Convert plan/list APIs to return values.**

Replace `plan_with_output` and `list_with_output` with APIs that return structured values. Keep compatibility wrappers only where they are public API and clearly delegate to the new core function.

- [ ] **Step 3: Move text and JSON rendering to `src/render.rs`.**

Implement pure functions:

```rust
pub fn render_plan_text(plan: &PlanResult) -> String
pub fn render_plan_json(plan: &PlanResult) -> Result<String, RenderError>
pub fn render_project_text(project: &ProjectDescription) -> String
pub fn render_project_json(project: &ProjectDescription) -> Result<String, RenderError>
pub fn render_success(success: &CommandSuccess, mode: OutputMode) -> Result<String, RenderError>
```

Redaction of environment values must happen while constructing the structured plan projection, never by string replacement.

- [ ] **Step 4: Add `CommandRequest` and `ProjectContext` to `src/app.rs`.**

Convert clap-specific command values into an application request. Resolve the discovered project root once and use it for project-relative changelog/release/cache paths.

- [ ] **Step 5: Make `main.rs` a transport adapter.**

Leave these responsibilities in `main.rs`:

- `Cli::parse`
- Ctrl-C handler installation
- terminal detection
- stdout/stderr selection
- final writing
- exit-code mapping

Move command-specific branch logic and rendering calls into `app.rs`/`render.rs`.

- [ ] **Step 6: Preserve CLI integration contracts.**

Run:

```bash
cargo test --test cli --test release_cli
```

Expected: text output, JSON shapes, exit codes, aliases, and stdout/stderr routing remain unchanged.

- [ ] **Step 7: Commit structured command results.**

```bash
git add src/app.rs src/render.rs src/lib.rs src/main.rs src/commands/ci.rs src/commands/list.rs tests/cli.rs tests/release_cli.rs
 git commit -m "refactor: render structured command results at the CLI edge"
```

---

### Task 4: Separate scheduler events from output presentation

**Files:**
- Modify: `src/scheduler.rs`
- Modify: `src/output.rs`
- Modify: `src/commands/ci.rs`
- Modify: `src/events.rs`
- Test: `src/scheduler.rs`
- Test: `src/output.rs`

- [ ] **Step 1: Add a reporter seam.**

Define a narrow `TaskReporter` trait that accepts structured run/task events and returns an explicit output failure. The scheduler must depend on this trait, not on `OutputSink`.

- [ ] **Step 2: Add a recording reporter test double.**

Use a `RecordingReporter` that stores `ExecutionEvent` values in a mutex and can be scripted to fail on a selected event. Assert scheduler behavior without constructing writers, JSON encoders, or TUI controllers.

- [ ] **Step 3: Replace scheduler `OutputSink` parameters.**

Change `execute_plan_with_services` and worker jobs to use the reporter trait. Preserve live-output behavior by forwarding `TaskOutput` events through the reporter.

- [ ] **Step 4: Implement the production reporter in `output.rs`.**

Adapt `OutputSink` to the new reporter interface. Keep JSON sequence/run identity generation, stream framing, terminal status output, and TUI integration in this adapter.

- [ ] **Step 5: Make run identity deterministic in tests.**

Inject the run identity source or use an explicit constructor parameter. Production may continue using time/process identity; tests must not depend on `SystemTime`.

- [ ] **Step 6: Run scheduler/output tests.**

Run:

```bash
cargo test scheduler:: output:: commands::ci::
```

Expected: all existing scheduler failure, retry, cache, finalizer, output, and JSON tests pass.

- [ ] **Step 7: Commit the reporter boundary.**

```bash
git add src/scheduler.rs src/output.rs src/commands/ci.rs src/events.rs
 git commit -m "refactor: decouple scheduling from output presentation"
```

---

### Task 5: Separate changelog workflows from filesystem and Git effects

**Files:**
- Modify: `src/io.rs`
- Modify: `src/commands/changelog.rs`
- Modify: `src/changelog.rs`
- Modify: `src/main.rs` or `src/app.rs`
- Test: `src/commands/changelog.rs`
- Test: `tests/release_cli.rs`

- [ ] **Step 1: Add a pure preparation test.**

Pass a parsed `Changelog`, `Request`, date, and bullet list to a pure function and assert the resulting rendered content and action. No temporary directory or Git executable should be involved.

- [ ] **Step 2: Add focused Git and text-file ports.**

Define `GitLog` and `TextFileStore` in `src/io.rs`. The production adapters should preserve the current bounded Git output, atomic writes, and error vocabulary.

- [ ] **Step 3: Convert `prepare_from_git` to an adapter workflow.**

The workflow should:

1. read through `TextFileStore`
2. parse and validate through `Changelog`
3. request merge-log text through `GitLog`
4. transform the log with the pure bullet formatter
5. render the new changelog
6. write through `TextFileStore`

- [ ] **Step 4: Return structured changelog results.**

Return a result containing the action, version/heading, bullet count, and warning state. Render the current human message and JSON success document at the CLI edge.

- [ ] **Step 5: Add tests for expected failure values.**

Cover Git spawn failure, nonzero Git status, output-size limits, write failure, invalid refs, and no-merge warning using scripted ports.

- [ ] **Step 6: Run changelog tests.**

Run:

```bash
cargo test commands::changelog --test release_cli
```

Expected: current CLI behavior and all pure workflow tests pass.

- [ ] **Step 7: Commit the changelog split.**

```bash
git add src/io.rs src/commands/changelog.rs src/changelog.rs src/app.rs src/main.rs tests/release_cli.rs
 git commit -m "refactor: isolate changelog transformations from Git and file IO"
```

---

### Task 6: Separate release-domain operations from artifact IO

**Files:**
- Modify: `src/release.rs`
- Modify: `src/commands/release.rs`
- Modify: `src/io.rs`
- Modify: `src/app.rs`
- Test: `src/release.rs`
- Test: `src/commands/release.rs`
- Test: `tests/release_cli.rs`

- [ ] **Step 1: Introduce an in-memory artifact inventory.**

Represent artifact names, digests, and sizes independently of filesystem paths. Add a pure function that builds a `ReleaseManifest` from an identity and inventory.

- [ ] **Step 2: Keep filesystem traversal in the production adapter.**

Move recursive walking, symlink rejection, metadata reads, and file hashing behind the release IO adapter. Preserve deterministic sorting and generated-file exclusion.

- [ ] **Step 3: Make manifest comparison pure.**

Extract identity comparison, artifact inventory comparison, and checksum-entry comparison into functions operating on parsed values.

- [ ] **Step 4: Preserve the existing `verify_source_with` pattern.**

Keep Git verification as a pure workflow over an injected Git runner, and use the same error vocabulary for the production adapter.

- [ ] **Step 5: Return structured release results from command functions.**

The command layer should return the manifest or verification summary; `main`/`render.rs` should produce messages such as `wrote release manifest...`.

- [ ] **Step 6: Run release tests.**

Run:

```bash
cargo test release:: commands::release --test release_cli
```

Expected: artifact creation, verification, expected-inventory checks, annotated/lightweight tag behavior, and mismatch failures all remain unchanged.

- [ ] **Step 7: Commit the release split.**

```bash
git add src/release.rs src/commands/release.rs src/io.rs src/app.rs tests/release_cli.rs
 git commit -m "refactor: isolate release validation from artifact IO"
```

---

### Task 7: Separate cache snapshots from cache-key semantics

**Files:**
- Modify: `src/cache/mod.rs`
- Modify: `src/cache/hash.rs`
- Modify: `src/scheduler.rs`
- Test: `src/cache/mod.rs`
- Test: `src/scheduler.rs`

- [ ] **Step 1: Define an input snapshot value.**

Represent the files, paths, sizes, modes, and digests needed by the cache key without retaining filesystem handles.

- [ ] **Step 2: Extract pure key calculation.**

Make cache-key calculation consume a `CacheSession`, `PlannedTask`, dependency keys, and a snapshot. It must not call `std::fs`, `std::env`, or pattern traversal.

- [ ] **Step 3: Keep snapshot collection in `CacheStore`.**

The production cache store remains responsible for reading the manifest, collecting matching files, hashing them, and persisting/restoring entries.

- [ ] **Step 4: Add direct key-semantic tests.**

Test that command, environment, dependency keys, task metadata, and file digests change the key without creating a cache directory.

- [ ] **Step 5: Run cache/scheduler tests.**

Run:

```bash
cargo test cache:: scheduler:: commands::ci::
```

Expected: cache hit/miss, output restore, symlink safety, and scheduler cache failure behavior remain unchanged.

- [ ] **Step 6: Commit the cache split.**

```bash
git add src/cache src/scheduler.rs
 git commit -m "refactor: separate cache snapshots from key computation"
```

---

### Task 8: Final runner/TUI cleanup and full verification

**Files:**
- Modify: `src/runner.rs` only where the prior changes expose a remaining mixed concern.
- Modify: `src/tui.rs` to keep state transitions independent from terminal setup.
- Test: `src/runner.rs`, `src/tui.rs`, `tests/cli.rs`

- [ ] **Step 1: Preserve the existing runner seams.**

Do not replace `ProcessLauncher`, `ChildProcess`, or `RunnerClock`. If lifecycle extraction is needed, move only pure action selection out of `Runner::run_with_options`; keep process-tree operations in the effect layer.

- [ ] **Step 2: Replace only invalid test seams, not invariant assertions.**

Keep `expect`/`assert` for broken internal invariants. Return values for expected child, process, output, timeout, cancellation, and callback failures.

- [ ] **Step 3: Extract TUI state transition tests.**

Test task creation, partial-line flushing, selection, scroll behavior, completion, and bounded history without starting a terminal.

- [ ] **Step 4: Run the complete verification suite.**

Run:

```bash
cargo fmt --all -- --check
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: all commands pass and the CLI integration suite preserves current behavior.

- [ ] **Step 5: Review the final diff for accidental contract changes.**

Check:

```bash
git diff --check
git status --short
git log --oneline -8
```

Confirm that stdout/stderr routing, JSON schema values, aliases, exit codes, and release workflow commands are unchanged.

- [ ] **Step 6: Commit final cleanup.**

```bash
git add src tests docs README.md
 git commit -m "refactor: finish CLI testability boundaries"
```

---

## Self-review

- **Spec coverage:** project loading, structured command results, CLI rendering, scheduler/output separation, changelog IO, release IO, cache-key isolation, runner/TUI seams, and CLI path semantics are all covered.
- **Behavior preservation:** every phase requires the existing focused tests, and the final phase runs all targets plus clippy.
- **Failure policy:** expected filesystem, Git, process, output, cache, timeout, and cancellation failures remain `Result` values. Internal invariant violations remain assertions/panics.
- **Scope control:** no large generic IO mock is introduced. Ports remain focused by capability: file store, Git log, reporter, process launcher, clock, and cache backend.
- **Lower-priority work:** `xtask` is explicitly deferred until the published CLI is stable, avoiding two simultaneous orchestration refactors.
