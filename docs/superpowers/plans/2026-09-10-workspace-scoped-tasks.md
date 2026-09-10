# Workspace-Scoped Tasks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Allow a root `monorepo.toml` to define tasks that run once at the workspace root and can depend on package tasks.

**Architecture:** Treat the reserved package namespace `workspace` as a pseudo-package backed by the workspace root directory. Qualified references such as `workspace:release-verify` select these tasks; unqualified references inside a workspace task remain workspace-local. Existing package discovery, task scheduling, command execution, concurrency, and output behavior remain unchanged.

**Tech Stack:** Rust 2024, serde/TOML, existing `Workspace` DAG planner, Cargo tests.

---

### Task 1: Add workspace-task validation and planning

**Files:**
- Modify: `src/config.rs`
- Modify: `src/workspace.rs`
- Test: `src/workspace.rs`

- [ ] **Step 1: Add the reserved workspace namespace constant and document root tasks**

Expose `WORKSPACE_PACKAGE_NAME` from `config.rs` as `"workspace"`. Document that root `[tasks.*]` entries are workspace-scoped and run from the root directory.

- [ ] **Step 2: Stop rejecting root tasks during workspace loading**

Allow root manifests to contain `[workspace]`, `[tasks]`, and `[pipelines]`. Store `root_config.tasks` in a new `Workspace.workspace_tasks` field.

- [ ] **Step 3: Reuse task validation for root tasks**

Refactor package task validation into a helper that accepts a manifest path, logical package name, package/root path, and task map. Run it for root tasks using the reserved `workspace` namespace and the canonical workspace root.

- [ ] **Step 4: Reject package names that collide with the reserved namespace**

Return an `InvalidManifest` error when a discovered package is named `workspace`, because that name identifies root-scoped tasks in task references.

- [ ] **Step 5: Resolve and plan workspace tasks**

Update task lookup and planned-task construction so `workspace:<task>` resolves from `Workspace.workspace_tasks`, uses the workspace root as `package_path` and default `cwd`, and supports cross-package dependencies such as `depends_on = ["web:package"]`.

- [ ] **Step 6: Add focused unit tests before broad test execution**

Cover:

1. A pipeline containing `workspace:release-verify` plans the package dependency first and then the workspace task.
2. The workspace task defaults to the workspace root as its cwd.
3. An unqualified dependency inside a workspace task resolves to another workspace task.
4. A package named `workspace` is rejected.

- [ ] **Step 7: Run the focused tests**

Run:

```bash
cargo test workspace::tests
```

Expected: all workspace tests pass.

---

### Task 2: Expose and document the manifest behavior

**Files:**
- Modify: `README.md`
- Modify: `src/commands/ci.rs` if formatting or selection behavior requires an adjustment
- Test: `tests/cli.rs`

- [ ] **Step 1: Document workspace-scoped task syntax**

Add a manifest example showing a root task and a release pipeline:

```toml
[pipelines.release]
tasks = ["workspace:release-verify"]

[tasks.release-verify]
command = ["./automation/release-verify"]
depends_on = ["web:package", "api:package"]
timeout_seconds = 1800
```

Explain that workspace tasks run once from the repository root and are useful for cross-package checks.

- [ ] **Step 2: Add an end-to-end CLI test**

Create a temporary root manifest with a workspace release task depending on a package task. Run `monorelease plan` and assert that the package task appears before `workspace:release-verify`.

- [ ] **Step 3: Run the full test suite**

Run:

```bash
cargo test
```

Expected: all unit and CLI tests pass.

- [ ] **Step 4: Run formatting and lint checks**

Run:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: both commands succeed.
