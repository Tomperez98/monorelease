# Mono Release and Execution Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Make Mono consistently root-project oriented, retain the opinionated TigerBeetle-inspired changelog/release workflow, harden subprocess and release metadata behavior, and define stable JSON contracts for execution and planning.

**Architecture:** Keep the existing root `mono.toml` graph and generic command execution model. Retain changelog and release commands as intentional Mono conventions, but document them as the standard release pattern. Replace obsolete package terminology, make process-tree termination failures explicit, preserve atomic release metadata publication, and expose structured JSON for run, plan, graph, list, and errors.

**Tech Stack:** Rust 2024, Clap, Serde/JSON, TOML, platform process APIs, Cargo/GitHub Actions.

---

### Task 1: Remove obsolete package terminology

**Files:**
- Modify: `src/project.rs`
- Modify: `src/commands/ci.rs`
- Modify: `src/runner.rs`
- Modify: `src/cache.rs`
- Modify: `src/lib.rs`
- Modify: `src/main.rs`
- Test: `tests/cli.rs` and existing unit tests

- [x] Rename root-project compatibility accessors and error vocabulary from package to project/root.
- [x] Remove the always-`None` selected-package parameters and CLI compatibility flag.
- [x] Update tests and user-facing output.
- [x] Run `cargo test --workspace --all-targets --all-features`.

### Task 2: Harden managed process termination

**Files:**
- Modify: `src/process.rs`
- Modify: `src/runner.rs`
- Test: runner unit tests, including timeout and output-limit behavior

- [x] Introduce an explicit termination error path instead of ignoring `terminate_tree` failures.
- [x] Ensure timeout/output-limit cleanup cannot wait forever after a failed tree termination.
- [x] Preserve normal successful-child behavior and existing platform abstractions.
- [x] Run the focused runner tests, then the full workspace test suite.

### Task 3: Make release metadata publication atomic and tested

**Files:**
- Modify: `src/release.rs`
- Test: `src/release.rs` tests and `tests/release_cli.rs`

- [x] Use platform-appropriate atomic replacement semantics for generated metadata and checksums.
- [x] Ensure failed writes clean up temporary files without deleting a valid previous manifest.
- [x] Add a regression test covering replacement of existing metadata.
- [x] Run focused release tests and the full suite.

### Task 4: Document the opinionated release convention

**Files:**
- Create: `docs/release-conventions.md`
- Modify: `README.md`
- Modify: `CHANGELOG.md` only if the implementation changes require a release note

- [x] Document the standard changelog shape, version/tag relationship, release notes generation, artifact inventory, metadata, and validation flow.
- [x] Explicitly state that these are Mono’s supported release conventions, not language detection or package-manager knowledge.
- [x] Include the TigerBeetle-inspired release manager flow and resumable-release behavior.
- [x] Verify all README links and examples.

### Task 5: Dogfood `mono release source` in GitHub Actions

**Files:**
- Modify: `.github/workflows/release.yml`
- Modify: `.github/workflows/release_validate.yml` if needed
- Test: release workflow command syntax and release CLI tests

- [x] Replace duplicated shell source/tag validation with `mono release source` where the released source checkout is available.
- [x] Continue capturing annotated tag identity for metadata.
- [x] Keep the workflow’s explicit version-to-Cargo check because it is Mono-repository policy.
- [x] Ensure manual release resume still works.

### Task 6: Define JSON execution, plan, graph, list, and error contracts

**Files:**
- Modify: `src/events.rs`
- Modify: `src/output.rs`
- Modify: `src/commands/ci.rs`
- Modify: `src/commands/list.rs`
- Modify: `src/main.rs`
- Modify: `src/lib.rs`
- Test: `tests/cli.rs`, output tests, and new command contract tests

- [x] Define a versioned JSON schema for run lifecycle events, task metadata, output, failures, and summaries.
- [x] Add structured JSON output for plan, graph, and list without changing terminal output.
- [x] Emit structured error information for execution failures and command-level failures where possible.
- [x] Document the JSON contract in `README.md` or a dedicated `docs/json-contract.md`.
- [x] Add tests that parse every emitted line and assert stable fields and ordering.
- [x] Run the full test suite, formatting, Clippy, and `cargo package --locked`.

### Final verification

- [x] Run `cargo fmt --check`.
- [x] Run `cargo test --workspace --all-targets --all-features`.
- [x] Run `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
- [x] Run `cargo package --locked`.
- [x] Run `cargo run --locked -- check` and `cargo run --locked -- plan`.
