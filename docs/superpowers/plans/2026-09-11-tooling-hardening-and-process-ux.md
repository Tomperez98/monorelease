# Tooling Hardening and Process UX Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Complete Mono's machine-output contract, harden CI log rendering, make changelog mutations safe and strict, and add generic stdin, cancellation, live-output, and retry-attempt support without adding language or framework knowledge.

**Architecture:** Keep project execution language-agnostic. Extend the existing command/output seams instead of teaching Mono about package managers or registries. `PipelineExecution` carries execution policy, `Runner` owns process lifecycle and optional output/cancellation hooks, `OutputSink` remains the sole renderer, and project-specific release behavior remains ordinary tasks.

**Tech Stack:** Rust 2024, Clap, Serde/JSON, TOML, platform process APIs, `ctrlc`, GitHub Actions log protocol.

---

### Task 1: Establish the expanded output contract

**Files:**
- Modify: `src/commands/ci.rs`
- Modify: `src/commands/changelog.rs`
- Modify: `src/commands/release.rs`
- Modify: `src/commands/init.rs`
- Modify: `src/main.rs`
- Modify: `src/output.rs`
- Modify: `src/events.rs`
- Modify: `docs/json-contract.md`
- Modify: `tests/cli.rs`
- Modify: `tests/release_cli.rs`

- [x] Add a small JSON success-document helper with `schema`, `kind`, and `status` fields for non-execution commands.
- [x] Make `init`, `cache clean`, all changelog commands, and all release commands return valid JSON when `--output json` is selected.
- [x] Make `run --dry-run --output json` return the same `kind: "plan"` document as `mono plan --output json`.
- [x] Keep terminal output byte-for-byte compatible where practical.
- [x] Add CLI tests that parse successful JSON output for each command and assert that every line/document is valid JSON.
- [x] Document the command success kinds and state that unsupported output combinations are not silently rendered as terminal text.
- [x] Run `cargo test --workspace --all-targets --all-features`.

### Task 2: Harden GitHub Actions log rendering

**Files:**
- Modify: `src/output.rs`
- Modify: `src/project.rs`
- Modify: `src/events.rs`
- Modify: `tests/cli.rs`
- Modify: `.github/workflows/ci.yml`

- [x] Reject control characters and newlines in project, pipeline, task, and resource-group identifiers where those values can enter CI commands.
- [x] Wrap arbitrary task output in GitHub Actions `stop-commands` markers using a per-run token before writing raw bytes.
- [x] Resume command processing after each output section and preserve stdout/stderr routing.
- [x] Ensure task names used in `::group::` are safe and cannot inject workflow commands.
- [x] Add a regression test with output containing `::error::`, `::warning::`, and `::set-output::` and assert the renderer surrounds it with stop/resume markers.
- [x] Run the focused output tests and Clippy.

### Task 3: Make changelog validation strict and writes atomic

**Files:**
- Modify: `src/changelog.rs`
- Modify: `src/commands/changelog.rs`
- Modify: `tests/release_cli.rs`
- Modify: `docs/release-conventions.md`

- [x] Validate every `Released: YYYY-MM-DD` line using a dependency-free Gregorian date validator, including leap-year rules.
- [x] Reject multiple `(unreleased)` entries and require `(unreleased)` to be the first entry.
- [x] Preserve strict descending version ordering and duplicate-version rejection.
- [x] Replace direct `fs::write` calls for changelogs and release notes with same-directory temporary files followed by platform-appropriate replacement.
- [x] Ensure failed writes remove only the temporary file and preserve the previous destination.
- [x] Add tests for invalid dates, invalid leap days, duplicate unreleased entries, and atomic replacement.
- [x] Run focused changelog/release tests and update the documented format.

### Task 4: Add generic stdin policy to tasks

**Files:**
- Modify: `src/config.rs`
- Modify: `src/project.rs`
- Modify: `src/runner.rs`
- Modify: `src/commands/ci.rs`
- Modify: `src/commands/list.rs`
- Modify: `docs/json-contract.md`
- Modify: `README.md`
- Modify: `tests/cli.rs`

- [x] Add `stdin = "null" | "inherit"` to `TaskConfig`, defaulting to `null`.
- [x] Carry the policy into `PlannedTask` and expose it in plan/list JSON without exposing secrets.
- [x] Configure `Command` with `Stdio::null()` or `Stdio::inherit()` accordingly.
- [x] Reject cacheable tasks using inherited stdin because the input is undeclared and cannot be safely fingerprinted.
- [x] Add manifest validation and execution tests for both policies.
- [x] Document that inherited stdin should generally be used only for explicitly serialized or interactive tasks.

### Task 5: Add cancellation and process-tree cleanup

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `src/runner.rs`
- Modify: `src/scheduler.rs`
- Modify: `src/commands/ci.rs`
- Modify: `src/events.rs`
- Modify: `src/output.rs`
- Modify: `src/main.rs`
- Modify: `src/lib.rs`
- Modify: `tests/cli.rs`

- [x] Add `ctrlc` as the cross-platform signal adapter; keep the cancellation token independent from signal handling so library tests can trigger it directly.
- [x] Add a cloneable `CancellationToken` backed by an atomic flag.
- [x] Check cancellation before dispatching new normal tasks and while polling running children.
- [x] Terminate the complete managed process tree, reap the child, join output readers, and return a distinct `Cancelled` runner error.
- [x] Stop new normal work after cancellation while still allowing already-ready finalizer tasks to run for cleanup.
- [x] Add cancelled task/run statuses and counts to the JSON event contract and terminal summaries.
- [x] Install the Ctrl-C handler once in the CLI and pass the token through `PipelineExecution`; do not install global handlers inside library tests.
- [x] Add a Unix regression test proving a cancelled child and descendant do not continue running.
- [x] Add CLI/event tests for cancellation accounting.
- [x] Run the full test suite on the supported host and Clippy.

### Task 6: Add optional live task output

**Files:**
- Modify: `src/output.rs`
- Modify: `src/runner.rs`
- Modify: `src/scheduler.rs`
- Modify: `src/main.rs`
- Modify: `src/events.rs`
- Modify: `docs/json-contract.md`
- Modify: `README.md`
- Modify: `tests/cli.rs`

- [x] Add a `live` output mode while preserving buffered `terminal` behavior.
- [x] Add a runner output callback seam used only by live modes; the callback receives task identity, stream, and byte chunks.
- [x] Keep output-reader threads bounded and propagate renderer failures back through the runner instead of ignoring them.
- [x] Render live terminal output as it arrives while retaining deterministic task-finished and run-finished summaries.
- [x] Keep JSON task-output events lossless and schema-stable; live streaming is provided by the `live` terminal mode.
- [x] Ensure cancellation and output-limit paths still terminate descendants and join both readers.
- [x] Add tests proving live output is emitted before task completion and that output limits remain enforced.
- [x] Document `--output live` and clarify that normal terminal mode remains buffered for deterministic presentation.

### Task 7: Expose retry attempt events

**Files:**
- Modify: `src/events.rs`
- Modify: `src/output.rs`
- Modify: `src/scheduler.rs`
- Modify: `docs/json-contract.md`
- Modify: `tests/cli.rs`

- [x] Add a `task_attempt_started` event containing task ID, attempt number, and maximum attempts.
- [x] Emit one attempt event before every actual runner invocation, including retries.
- [x] Preserve the existing `task_started` event as the scheduling event.
- [x] Render retry attempts clearly in terminal and GitHub Actions modes without duplicating task groups.
- [x] Add a retry test asserting attempts `1..N` appear in order and the final task status remains correct.
- [x] Document event ordering and retry semantics.

### Task 8: Final integration and contract verification

**Files:**
- Modify: `README.md`
- Modify: `docs/json-contract.md`
- Modify: `docs/release-conventions.md`
- Modify: `mono.toml`
- Modify: `.github/workflows/ci.yml`

- [x] Update the README and JSON contract to document stdin policy, live output, retries, cancellation, and JSON command documents without introducing language-specific behavior.
- [x] Existing repository CI runs the full test suite, which exercises the JSON and process behavior.
- [x] Run:

```bash
cargo fmt --check
cargo test --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo package --locked
mono check
mono plan
```

- [x] Verify that all JSON success documents, execution events, and errors carry the documented schema.
- [x] Verify that ordinary terminal output and existing release workflows still work.
