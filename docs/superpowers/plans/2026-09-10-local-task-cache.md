# Local Task Cache Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a safe, opt-in, local content-addressed task cache to `monorelease`, including cached logs and declared output restoration, while preserving the existing TOML task graph and scheduler.

**Architecture:** Keep process execution in `runner.rs` and add a `cache.rs` storage/fingerprinting module. Extend task configuration with explicit cache inputs, outputs, and hash-relevant environment variables. The scheduler computes a key after dependencies complete, restores successful cache entries as completed tasks, and stores successful misses atomically. Cache failures are expected errors; broken invariants continue to panic through existing assertions.

**Tech Stack:** Rust 2024, existing `serde`/TOML, `serde_json` metadata, `sha2` SHA-256 keys, standard-library filesystem/process/thread APIs.

---

### Task 1: Add ignored cache directory and configuration fields

**Files:**
- Create: `.monorelease/.gitignore`
- Modify: `.gitignore`
- Modify: `Cargo.toml`
- Modify: `src/config.rs`
- Modify: `src/workspace.rs`
- Test: `src/config.rs`, `src/workspace.rs`

- [ ] **Step 1: Add the ignored cache directory**

Create `.monorelease/.gitignore` containing exactly:

```gitignore
*
```

Add `.monorelease` to the repository root `.gitignore` as defense in depth while retaining the nested ignore file.

- [ ] **Step 2: Add cache dependencies**

Add `serde_json` and `sha2` to `Cargo.toml`.

- [ ] **Step 3: Extend `TaskConfig` with opt-in cache settings**

Add these serde-defaulted fields:

```rust
#[serde(default)]
pub cache: bool,
#[serde(default)]
pub inputs: Vec<String>,
#[serde(default)]
pub outputs: Vec<String>,
#[serde(default)]
pub cache_env: Vec<String>,
```

Keep `cache` defaulting to `false` so existing arbitrary commands do not silently stop executing. Add accessors to `PlannedTask` for all four values.

- [ ] **Step 4: Validate cache patterns and environment names**

Reject empty, NUL-containing, absolute, or `..`-containing input/output patterns. Permit a leading `!` for output exclusions. Reject empty or NUL-containing `cache_env` names. Return `InvalidTask` with the package/task context.

- [ ] **Step 5: Add configuration tests**

Test that cache fields default to disabled/empty, and that a task with valid cache fields parses and survives workspace planning.

- [ ] **Step 6: Run focused tests**

Run:

```bash
cargo test config workspace
```

Expected: PASS.

---

### Task 2: Implement local fingerprinting and cache storage

**Files:**
- Create: `src/cache.rs`
- Modify: `src/lib.rs`
- Test: `src/cache.rs`

- [ ] **Step 1: Define cache error and metadata types**

Implement `CacheError` for expected filesystem, JSON, invalid-path, and hashing-input failures. Define JSON metadata containing cache format version, task key, captured elapsed milliseconds, and declared output file paths/modes.

- [ ] **Step 2: Implement stable task key generation**

Implement a `CacheStore::task_key` method that SHA-256 hashes:

- Cache format version
- Package/task identity
- Command argv
- Relative working directory
- Task timeout/resource group/cache configuration
- Root and package `monorepo.toml` bytes
- Declared input patterns and matched file paths/content
- `cache_env` names and current values
- Dependency task keys

Require cacheable tasks to declare at least one input pattern. Automatically exclude `.git`, `.monorelease`, and declared output matches so generated outputs do not poison the next key.

- [ ] **Step 3: Implement deterministic file matching**

Add a standard-library recursive file walker and segment matcher supporting literal segments, `*`, and `**`. Return sorted, deduplicated relative paths. Do not follow symlinked directories. Apply `!pattern` exclusions after positive matches.

- [ ] **Step 4: Implement cache lookup and output restoration**

Use `.monorelease/cache/<sha256>/`. On lookup, validate metadata and every cached output before copying anything. Restore output files beneath the task package path, create parent directories, and preserve Unix executable bits where available. Return a cache miss when the entry is absent or incomplete.

- [ ] **Step 5: Implement atomic cache writes**

Store stdout, stderr, metadata, and copied declared output files in a unique temporary entry, then rename it into the final hash directory. Do not write failed task results. Cache entries with no outputs still retain logs and timing.

- [ ] **Step 6: Add cache seam tests**

Test through `CacheStore` behavior that:

1. The same task/input set produces the same key.
2. Changing an input changes the key.
3. A successful result can be stored and looked up with logs intact.
4. Deleted declared outputs are restored on lookup.
5. Missing/corrupt metadata is a cache miss rather than a panic.
6. Output paths cannot escape the package.

- [ ] **Step 7: Run cache tests**

Run:

```bash
cargo test cache
```

Expected: PASS.

---

### Task 3: Integrate cache hits and writes into scheduling

**Files:**
- Modify: `src/scheduler.rs`
- Modify: `src/lib.rs`
- Test: `src/scheduler.rs`, `src/commands/ci.rs`

- [ ] **Step 1: Add cache state to the scheduler**

Create a `CacheStore` rooted at the workspace root. Track each completed task's key and whether it was cacheable. A task is cacheable only when its own `cache` flag is true and all planned dependencies are cacheable; this prevents a cached dependent from reusing results produced by an always-running dependency.

- [ ] **Step 2: Add cache lookup before spawning a runner**

When a ready task is selected, compute its key from dependency keys. For cacheable tasks, look up the entry. Schedule a cache hit as a completed worker result so normal dependency unblocking and deterministic output presentation remain unchanged. Cache misses continue through `Runner::run`.

- [ ] **Step 3: Store successful misses before unblocking dependents**

After a task succeeds, store its result and declared outputs before decrementing dependent indegrees. On a cache-store error, stop admitting new tasks, join already-running workers, present completed output, and return a `SchedulerError::Cache`.

- [ ] **Step 4: Preserve existing failure behavior**

Never cache failed or timed-out tasks. A cache hit must be presented as successful task output and must unblock dependents exactly like a real successful process.

- [ ] **Step 5: Add scheduler tests**

Test that a second execution skips a marker-producing command, that changing an input reruns it, and that a cached dependency restores its output before a dependent task executes.

- [ ] **Step 6: Run scheduler and command tests**

Run:

```bash
cargo test scheduler commands::ci
```

Expected: PASS.

---

### Task 4: Add CLI controls and documentation

**Files:**
- Modify: `src/commands/ci.rs`
- Modify: `src/main.rs`
- Modify: `README.md`
- Modify: `examples/echo/packages/shared/monorepo.toml`
- Modify: `examples/echo/packages/app/monorepo.toml`
- Test: `tests/cli.rs`

- [ ] **Step 1: Add cache controls to `ci` and `run`**

Add `--no-cache` and `--force` flags. `--no-cache` bypasses cache reads and writes. `--force` bypasses cache reads but permits successful results to refresh the cache. Thread the mode through public command functions without changing existing callers by using an internal options function.

- [ ] **Step 2: Add cache status to task output**

Keep existing task output stable, but include a concise `cache hit` marker in the task section when a result was restored. Keep dry-run output free of cache mutation.

- [ ] **Step 3: Document opt-in caching**

Update the README with the manifest fields, `.monorelease/cache` location, cache correctness rules, CLI flags, and the fact that remote caching is not included yet. Add one example task with explicit `inputs` and `outputs`.

- [ ] **Step 4: Add an end-to-end CLI test**

Create a temporary workspace whose task increments a file only when executed. Run `ci` twice and assert the second run reports a cache hit and does not increment the file. Delete the declared output, run again, and assert it is restored.

- [ ] **Step 5: Run the full validation suite**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

Expected: all commands pass.

---

### Task 5: Simplify and review the implementation

**Files:**
- Review all files changed in Tasks 1–4.

- [ ] **Step 1: Check invariants and failure paths**

Verify that cache read/write errors return typed errors, impossible scheduler states still use assertions, failed tasks never write entries, and all restored paths remain package-contained.

- [ ] **Step 2: Remove speculative behavior**

Keep remote cache, cache eviction, compression, and automatic Git integration out of this first implementation. Retain only local hashing, logs, declared outputs, and CLI bypass controls.

- [ ] **Step 3: Re-run formatting, lint, and tests**

Run the full validation commands from Task 4 and inspect `git diff --check`.
