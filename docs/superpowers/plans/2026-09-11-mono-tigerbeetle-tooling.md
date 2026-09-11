# Mono TigerBeetle-Inspired Tooling Implementation Plan

> **For agentic workers:** Execute this plan task-by-task with tests and checkpoints.

**Goal:** Harden Mono's existing orchestration/release tooling and add generic matrix, retry, finalizer, artifact, and event capabilities without adding language-specific knowledge.

**Architecture:** Keep `src/process.rs` as the private Unix/Windows process-tree boundary. Extend the manifest and scheduler with provider-neutral task lifecycle primitives; retain Git and the strict Markdown/SemVer changelog as intentional Mono features; keep Cargo/GitHub/package publication details in repository tasks and workflows.

**Tech Stack:** Rust 2024, `std::process`, `serde`, TOML, SHA-256, Clap, GitHub Actions.

---

## Scope

The implementation will be delivered in four independently testable groups:

1. Platform CI and process-boundary regression coverage.
2. Cache and release correctness hardening.
3. Versioned execution events and task lifecycle features: retries, finalizers, and matrix tasks.
4. First-class artifact declarations and release-workflow integration.

This is the first public release, so the existing schema number remains `1` while the new fields are added directly.

## Files and responsibilities

- `src/config.rs`: schema-v2 manifest structures and validation.
- `src/workspace.rs`: expand matrix task instances, resolve lifecycle tasks, and expose stable task identities.
- `src/scheduler.rs`: retries, finalizers, matrix dependency scheduling, and result accounting.
- `src/cache.rs`: platform-aware keys, output digests, and concurrent publication.
- `src/events.rs`: versioned event wire contract.
- `src/output.rs`: render the expanded event contract.
- `src/process.rs`, `src/runner.rs`: preserve and test process-tree behavior.
- `src/release.rs`, `src/commands/release.rs`: exact Git tag validation, atomic metadata, expected artifact inventory.
- `tests/cli.rs`, `tests/release_cli.rs`: end-to-end CLI contracts.
- `.github/workflows/ci.yml`: Ubuntu/macOS/Windows test matrix.
- `.github/workflows/release.yml`, `.github/workflows/release_validate.yml`: consume generic artifact validation.
- `mono.toml`, `README.md`, `CHANGELOG.md`: dogfood and document the new contracts.

## Execution order

### Task 1: Add the platform test matrix

- Modify `.github/workflows/ci.yml` to run the existing test and Clippy commands on Ubuntu, macOS, and Windows.
- Keep the existing release build matrix unchanged.
- Run `cargo test --workspace --all-targets --all-features` and `cargo clippy --workspace --all-targets --all-features -- --deny warnings` locally.

### Task 2: Harden cache correctness

- Add OS, architecture, and target information to cache keys.
- Add output SHA-256 digests to cache metadata and verify them on restore.
- Include Unix executable mode in input fingerprints.
- Treat a concurrent `AlreadyExists` cache publication as success.
- Add regression tests for each behavior.

### Task 3: Harden Git release verification and metadata writes

- Require `refs/tags/<tag>` resolution instead of arbitrary Git revision expressions.
- Validate tag and commit values before invoking Git.
- Write `BUILD-METADATA.json` and `SHA256SUMS` through temporary files and rename them atomically.
- Add CLI and library tests.

### Task 4: Version and enrich execution events

- Add event schema version and sequence number.
- Include exit codes and failure classifications where available.
- Add retry attempt information and blocked-task events.
- Preserve terminal, JSON, and GitHub Actions rendering behavior.
- Extend event tests and CLI JSON tests.

### Task 5: Add retries

- Add `retries` and `retry_backoff_seconds` to task configuration, defaulting to zero.
- Retry only failed/time-out/output-limit task invocations; cache only the final successful result.
- Emit attempt information.
- Add scheduler tests for success-after-retry and exhausted retries.

### Task 6: Add finalizers

- Add pipeline-level `finally = [...]` task references.
- Run finalizers after the normal plan regardless of success or failure.
- Preserve the primary failure while reporting finalizer failures.
- Ensure finalizers are not silently treated as successful dependency results.
- Add scheduler and CLI tests.

### Task 7: Add matrix tasks

- Add task matrix values as opaque strings.
- Expand matrix instances into stable task node identities.
- Support matrix interpolation in argv, environment, cwd, inputs, outputs, and artifact names without invoking a shell.
- Include matrix dimensions in cache keys and JSON events.
- Add graph, scheduler, cache, and CLI tests.

### Task 8: Add first-class artifacts

- Add an `artifacts` declaration separate from cache `outputs`.
- Collect explicit artifact paths and reject missing, duplicate, unsafe, or unexpected files.
- Extend release manifest/verify to accept an expected inventory file.
- Add artifact digest tests.

### Task 9: Dogfood the release workflow

- Add changelog validation to the project CI pipeline.
- Declare the Mono release artifact inventory in repository configuration where practical.
- Replace duplicated workflow checks with `mono release verify --expected ...`.
- Keep Cargo target setup and GitHub publication in the workflow/project-specific scripts.
- Update README and changelog documentation.

### Task 10: Full validation

Run:

```bash
cargo fmt --check
cargo test --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- --deny warnings
cargo package --locked
cargo run --locked -- check
cargo run --locked -- run --dry-run
```

Verify that schema-1 examples still work and that schema-2 fixtures exercise every new feature.
