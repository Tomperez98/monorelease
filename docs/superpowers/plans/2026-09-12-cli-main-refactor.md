# CLI Main Boundary Refactor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix dry-run signal setup, move command orchestration and rendering out of `src/main.rs`, replace the ambiguous optional string result, and pass validated changelog inputs through the CLI boundary without reparsing them.

**Architecture:** Keep `src/main.rs` responsible for Clap parsing, terminal detection, Ctrl-C setup, output sinks, final writing, and exit-code mapping. Move command execution into `src/app.rs`, represent command outcomes with tagged enums, and move text/JSON success rendering into `src/render.rs`. Changelog CLI values become typed boundary values; new typed command APIs preserve the existing string-based public compatibility wrappers while letting the CLI path avoid duplicate validation.

**Tech Stack:** Rust 2024, Clap, serde/serde_json, existing `mono` command modules, unit and CLI integration tests.

---

### Task 1: Make task detection respect dry-run

**Files:**
- Modify: `src/main.rs:515-538`
- Test: `src/main.rs` unit tests

- [x] **Step 1: Add tests for dry-run task detection.**

Extend the existing `runs_tasks` test with `run --dry-run` and `task --dry-run` command values. Both must assert `false`; normal `run` and `task` must remain `true`.

- [x] **Step 2: Update `runs_tasks`.**

Match execution options and return `!options.dry_run` for `Run` and `Task`. Keep bare `mono` as task-running because its default execution is not dry-run.

- [x] **Step 3: Run the focused binary tests.**

Run `cargo test --bin mono runs_tasks`. Expected: all matching tests pass.

---

### Task 2: Add typed command and rendering boundaries

**Files:**
- Create: `src/app.rs`
- Create: `src/render.rs`
- Modify: `src/main.rs:461-1059`
- Test: `src/main.rs` tests, plus new module tests where useful

- [x] **Step 1: Define tagged command outcomes in `src/app.rs`.**

Add:

```rust
pub(crate) enum CommandResult {
    Success { kind: &'static str, message: String },
    Check { project_root: PathBuf },
    PreRendered(String),
    EventsAlreadyEmitted,
}
```

Move `dispatch`, `default_execution`, `run_init`, `execute_pipeline`, `run_check`, `run_list`, `run_plan`, `run_graph`, `run_cache`, `run_changelog`, `run_release`, and `resolve_path` into `app.rs`. The execution helper maps `Some(summary)` to `PreRendered(summary)` and `None` to `EventsAlreadyEmitted`, eliminating `Result<Option<String>, Error>`.

- [x] **Step 2: Keep terminal probing at the process edge.**

Pass the already-computed terminal boolean into the application dispatch function. Keep `interactive_terminal` in `main.rs`; keep the pure `resolve_execution_output` table in `app.rs` or a private helper there.

- [x] **Step 3: Define rendering outcomes in `src/render.rs`.**

Add:

```rust
pub(crate) enum RenderedCommand {
    Summary(String),
    EventsAlreadyEmitted,
}
```

Move `SuccessDocument`, `CheckDocument`, `success_document`, `check_document`, and the success rendering match into `render.rs`. `Success` and `Check` become JSON documents in JSON mode and human-readable messages otherwise; `PreRendered` passes through unchanged.

- [x] **Step 4: Reduce `main.rs` to transport responsibilities.**

Declare `mod app;` and `mod render;`, make the Clap command types visible to those modules with `pub(crate)` visibility where required, and make `main` call:

```rust
let result = app::dispatch(root, output, ui, command, cancellation, terminal);
let rendered = result.map(|result| render::render(result, output));
```

Retain `install_cancellation`, `runs_tasks`, `error_sink`, `emit_summary`, `emit_error`, exit-code mapping, JSON error serialization, and path-independent transport tests in `main.rs`.

- [x] **Step 5: Add result-shape tests.**

Test that normal command results render one summary, JSON execution returns `EventsAlreadyEmitted`, and check results produce the existing text and JSON shapes.

- [x] **Step 6: Run the complete test suite.**

Run `cargo test --all-targets`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo fmt --all -- --check`. Expected: all existing output, exit-code, and CLI contract tests remain green.

---

### Task 3: Parse changelog inputs once at the CLI boundary

**Files:**
- Modify: `src/main.rs` Clap fields and parsers
- Modify: `src/app.rs` command conversion
- Modify: `src/commands/changelog.rs`
- Modify: `src/lib.rs` exports
- Test: `src/main.rs`, `src/commands/changelog.rs`, `tests/release_cli.rs`

- [x] **Step 1: Add typed CLI value wrappers.**

Define boundary types in `main.rs`:

```rust
#[derive(Clone, Debug)]
struct ParsedVersion(Request);

#[derive(Clone, Debug)]
struct ParsedDate(String);

#[derive(Clone, Debug)]
struct ParsedPullRequestUrl(String);
```

Their Clap parsers must perform the existing validation and store the validated value. Implement `AsRef<str>` for the string-backed types and expose the inner `Request`/string only through small accessors.

- [x] **Step 2: Add typed changelog command entry points.**

In `src/commands/changelog.rs`, add typed functions that accept `Option<Request>` and validated date/template values. Move the shared workflow below these typed functions so it does not call `Request::parse`, `is_valid_date`, or `validate_pull_request_url` again. Keep the existing string-based public functions as compatibility wrappers that parse once and delegate to the typed functions.

- [x] **Step 3: Add typed release-notes target construction.**

Add a constructor such as:

```rust
pub fn from_requests(
    version: Option<Request>,
    release_tag: Option<Request>,
) -> Result<Self, ChangelogError>
```

Keep `ReleaseNotesTarget::parse` as a compatibility wrapper that parses strings and delegates. The CLI passes the already parsed requests to `from_requests`.

- [x] **Step 4: Update Clap fields and application dispatch.**

Use the typed wrappers for changelog positional versions, hidden compatibility versions, release tags, dates, and pull-request URLs. Update `app.rs` to pass typed values to the new command APIs; no command workflow should reparse these raw strings.

- [x] **Step 5: Test parser and typed workflow behavior.**

Retain existing malformed-value tests and add tests proving valid `v1.2.3`, `unreleased`, ISO dates, and URL templates reach the typed workflow unchanged. Keep tests proving malformed values fail with exit code `2`.

- [x] **Step 6: Run the complete validation suite.**

Run `cargo test --all-targets`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo fmt --all -- --check`.

---

### Self-review checklist

- [x] Bare `mono` still runs the default pipeline.
- [x] `run --dry-run` and `task --dry-run` do not require a Ctrl-C handler.
- [x] JSON execution emits events without an extra summary line.
- [x] Non-execution text and JSON output remain byte-for-byte compatible.
- [x] Existing exit-code classification remains in `main.rs`.
- [x] Changelog values are validated at the Clap boundary and typed workflows do not reparse them.
- [x] No command-specific orchestration remains in `main.rs` beyond parsing and process transport.
