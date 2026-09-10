# Manifest-Driven Workspace CI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `monorelease` a fully language- and framework-agnostic monorepo orchestrator driven entirely by root and package-local manifests.

**Architecture:** The root manifest declares member discovery and named pipelines. Each package manifest declares only its identity and structured commands. Task dependencies form a DAG of `package:task` nodes. A shared runner executes commands with package-relative working directories, environment overrides, CI sections, timing, and inherited IO.

**Tech Stack:** Rust 2024, `clap`, `serde`, `toml`, `std::process::Command`, standard-library filesystem traversal.

---

### Task 1: Define the fixed manifest schema

**Files:** `src/config.rs`

- [x] Remove version headers and reject unknown fields.
- [x] Define root `[workspace]`, `[pipelines.<name>]`, package `[package]`, and package `[tasks.<name>]` tables.
- [x] Support task command argv, local/cross-package `depends_on`, relative `cwd`, and child-process `env`.
- [x] Generate a useful root template with `ci` as the default pipeline.

### Task 2: Discover and validate workspace manifests

**Files:** `src/workspace.rs`

- [x] Discover the root manifest from the current directory or ancestors.
- [x] Expand literal, `*`, and `**` member patterns.
- [x] Load package-local manifests and enforce root containment.
- [x] Reject duplicate package names and invalid package/task names.
- [x] Validate all named pipelines and task references before execution.

### Task 3: Build a task dependency graph

**Files:** `src/workspace.rs`

- [x] Represent tasks as `package:task` nodes.
- [x] Resolve local task references and qualified cross-package references.
- [x] Detect missing tasks and dependency cycles.
- [x] Produce deterministic dependency-first plans.
- [x] Support package selection while including transitive task dependencies.
- [x] Execute independent task branches concurrently with a bounded `--jobs` worker count.

### Task 4: Implement the shared execution context

**Files:** `src/runner.rs`

- [x] Execute structured argv commands without changing the parent cwd.
- [x] Apply task-relative cwd and environment overrides.
- [x] Preserve inherited stdout/stderr.
- [x] Add named sections, elapsed timing, GitHub Actions grouping, and contextual errors.
- [x] Capture task output so concurrent batches can emit output in scheduler order.
- [x] Keep dry-run formatting separate from process execution.

### Task 5: Add orchestration commands

**Files:** `src/commands/ci.rs`, `src/main.rs`, `src/lib.rs`

- [x] Implement `ci` for the default pipeline.
- [x] Implement `run <pipeline>` for named pipelines.
- [x] Implement `plan` for resolved command plans.
- [x] Implement `graph` for task dependency edges.
- [x] Keep CLI output and exit-code translation in the transport edge.

### Task 6: Test and document the contract

**Files:** `src/*` tests, `tests/cli.rs`, `README.md`

- [x] Test fixed-schema parsing, discovery, task ordering, task cycles, package selection, cwd, env, and dry runs.
- [x] Test CLI initialization, validation, and dry-run discovery.
- [x] Document language-agnostic manifests, pipelines, task references, and command execution.
- [x] Verify with `cargo fmt --check`, `cargo test`, and manual CLI smoke tests.
