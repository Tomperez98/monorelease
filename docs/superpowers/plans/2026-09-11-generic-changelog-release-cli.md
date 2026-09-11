# Generic Changelog and Release CLI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add provider-neutral `monorelease changelog` and `monorelease release` CLI commands that provide reusable changelog validation/scaffolding/notes and file-based release manifest/checksum verification without adding manifest configuration or assuming a language, registry, or executable artifact.

**Architecture:** Move the pure changelog model into the published library, expose CLI command modules for changelog and release operations, and keep repository-specific release gates in `xtask`. Release manifests operate only on files and explicit/environment-provided identity values. Publishing, package registries, GitHub PR decoration, binary behavior, and project workloads remain outside the core.

**Tech Stack:** Rust 2024, clap derive, serde/serde_json, sha2, existing `Result` error boundaries and temporary-directory tests.

---

## Scope and invariants

The implementation must preserve these invariants:

- Existing `monorepo.toml` files remain valid; no new TOML fields are required.
- Existing task commands remain external argv arrays; no built-in task kind is introduced in this change.
- Changelog parsing is deterministic and does not require Git, GitHub, or a package manager.
- Changelog mutation is explicit and refuses duplicate versions.
- Release manifest generation never includes its own metadata/checksum outputs as artifacts.
- Release verification rejects missing, extra, malformed, or mismatched artifacts/checksums.
- Symlinks are rejected from release artifact inventories.
- File I/O, parsing, invalid user input, and mismatches return errors; internal impossible states may assert/panic.
- Release commands do not publish, make network requests, execute artifacts, inspect registries, or assume a binary.

## File map

- Create `src/changelog.rs`: pure version, heading, changelog parsing, validation, rendering, scaffolding, and release-entry extraction.
- Create `src/commands/changelog.rs`: CLI-facing changelog operations and command error mapping.
- Create `src/release.rs`: pure artifact inventory, SHA-256 checksums, manifest metadata, and verification.
- Create `src/commands/release.rs`: CLI-facing manifest/source/verification operations.
- Modify `src/commands/mod.rs`: expose new command modules.
- Modify `src/lib.rs`: register modules, re-export public generic types/functions, and add top-level error variants.
- Modify `src/main.rs`: add `changelog` and `release` subcommands and map their errors.
- Modify `Cargo.toml`: no dependency change is expected; confirm existing `serde`, `serde_json`, and `sha2` are sufficient.
- Add or extend tests in the new modules using `src/testing.rs` temporary directories.
- Do not modify `xtask` in the first implementation slice; retain its repository-specific PR collection, binary behavior checks, examples, and registry-independent project gates. A later migration can consume the new library after the CLI contract is stable.

## Task 1: Extract the generic changelog model

**Files:**
- Create: `src/changelog.rs`
- Test: `src/changelog.rs` unit tests

- [ ] **Step 1: Write the pure model and parser tests first.** Cover valid parsing/rendering, strict semantic versions, `(unreleased)` placement, newest-first ordering, required `Released: ` lines, duplicate prevention, and promotion of `(unreleased)`.

```rust
#[test]
fn parses_and_renders_a_valid_changelog() { /* assert render == input */ }

#[test]
fn rejects_an_unrecognized_heading() { /* assert descriptive error */ }

#[test]
fn rejects_versions_that_are_not_newest_first() { /* assert descriptive error */ }

#[test]
fn rejects_unreleased_below_a_version() { /* assert descriptive error */ }

#[test]
fn scaffold_promotes_unreleased_without_collecting_provider_data() { /* assert rename */ }
```

- [ ] **Step 2: Run the focused test target and confirm it fails because the module is absent.**

Run: `cargo test changelog --lib`

Expected: compilation failure until `src/changelog.rs` exists.

- [ ] **Step 3: Implement the pure public model.** Provide `Version`, `Heading`, `Request`, `Entry`, `Changelog`, `Action`, `ChangelogError`-compatible string errors, `today_utc`, `Changelog::parse`, `Changelog::render`, `Changelog::top`, `Changelog::replaces_unreleased`, and `Changelog::scaffold`.

- [ ] **Step 4: Keep provider-specific behavior out.** The generic scaffold accepts caller-supplied bullet strings but never shells out to Git, parses remotes, creates GitHub URLs, or assumes categories. The parser accepts the repository’s current `# Changelog` and `## <version>` format.

- [ ] **Step 5: Run focused tests.**

Run: `cargo test changelog --lib`

Expected: all changelog model tests pass.

## Task 2: Add changelog CLI commands

**Files:**
- Create: `src/commands/changelog.rs`
- Modify: `src/commands/mod.rs`
- Modify: `src/lib.rs`
- Modify: `src/main.rs`
- Test: `src/commands/changelog.rs` unit tests

- [ ] **Step 1: Define the CLI/error contract.** Add `ChangelogCommands` with `Validate`, `Scaffold`, and `Notes`. Default file is `CHANGELOG.md`; scaffold version may be positional or `VERSION`; notes accept a version/tag and default output `RELEASE_NOTES.md`.

- [ ] **Step 2: Write command tests.** Test validate success/failure, scaffold writing a new entry, notes extracting the requested version, missing environment/version errors, and refusal to overwrite a duplicate.

```rust
#[test]
fn validate_reads_the_default_changelog() { /* temp fixture */ }

#[test]
fn notes_writes_only_the_requested_entry() { /* assert output */ }

#[test]
fn scaffold_requires_an_explicit_version_or_version_environment() { /* expected error */ }
```

- [ ] **Step 3: Implement filesystem-at-the-edge command functions.** Use `Path` inputs and return command-local errors for read/write/parse/invalid-version failures. Keep formatting and mutation in `src/changelog.rs`.

- [ ] **Step 4: Wire one top-level `Error::Changelog` mapping.** `main.rs` should parse CLI values, call the command module, print one error at the transport edge, and return failure.

- [ ] **Step 5: Run focused and full tests.**

Run: `cargo test changelog --lib && cargo test`

Expected: all tests pass.

## Task 3: Implement generic release manifests and checksums

**Files:**
- Create: `src/release.rs`
- Test: `src/release.rs` unit tests

- [ ] **Step 1: Define domain types.** Add `Artifact`, `ReleaseManifest`, `ReleaseIdentity`, `ReleasePaths`, and `ReleaseError`. Metadata fields such as repository, tag, commit, and workflow run are optional; artifact names and SHA-256 values are required.

- [ ] **Step 2: Write pure checksum/inventory tests.** Cover deterministic ordering, SHA-256 for known content, malformed checksum lines, missing files, extra files, mismatched hashes, symlink rejection, empty inventories, and metadata/checksum files being excluded from the artifact set.

```rust
#[test]
fn manifest_orders_artifacts_deterministically() { /* names sorted */ }

#[test]
fn verification_rejects_an_extra_regular_file() { /* expected error */ }

#[test]
fn verification_rejects_a_symlink_artifact() { /* expected error */ }

#[test]
fn verification_rejects_a_checksum_mismatch() { /* expected error */ }
```

- [ ] **Step 3: Implement deterministic artifact collection.** Walk only the requested artifact directory, collect regular files recursively, reject symlinks, exclude the configured metadata and checksum output paths, normalize relative names with `/`, and sort by name. Return expected filesystem errors instead of panicking.

- [ ] **Step 4: Implement manifest/checksum rendering and verification.** Write `BUILD-METADATA.json` through `serde_json`, write `SHA256SUMS` with stable `sha256  relative/path` lines, and verify that manifest artifacts, checksum entries, and on-disk artifacts are exactly the same set.

- [ ] **Step 5: Add explicit identity expectations.** Verification accepts optional expected tag, commit, and repository values from the CLI. If an expectation is supplied and metadata is absent or different, return a descriptive error. Do not infer package versions or execute artifacts.

- [ ] **Step 6: Run focused tests.**

Run: `cargo test release --lib`

Expected: all release module tests pass.

## Task 4: Add release CLI commands

**Files:**
- Create: `src/commands/release.rs`
- Modify: `src/commands/mod.rs`
- Modify: `src/lib.rs`
- Modify: `src/main.rs`
- Test: `src/commands/release.rs` unit tests

- [ ] **Step 1: Define CLI commands.** Add `release manifest` and `release verify`, both with `--directory` defaulting to `dist`; manifest accepts optional `--tag`, `--commit`, `--repository`, and `--workflow-run`; verify accepts optional expected identity flags.

- [ ] **Step 2: Write command tests.** Test manifest creation and verification through a temporary directory, metadata output, checksum output, explicit identity mismatch, and environment fallback for `RELEASE_TAG`, `GITHUB_SHA`, and `GITHUB_REPOSITORY`.

- [ ] **Step 3: Implement environment parsing at the command edge.** Convert raw environment strings into `ReleaseIdentity` once. Empty variables behave as absent. The release library receives typed values and does not read process-global environment state.

- [ ] **Step 4: Wire `Error::Release` and CLI transport mapping.** Keep all filesystem and validation errors in the release command vocabulary; `main.rs` prints them once and returns a nonzero exit code.

- [ ] **Step 5: Run focused and full tests.**

Run: `cargo test release --lib && cargo test && cargo run -- release --help`

Expected: all tests pass and help lists `manifest` and `verify`.

## Task 5: Expose generic library APIs and document boundaries

**Files:**
- Modify: `src/lib.rs`
- Modify: `README.md`
- Test: existing integration/CLI tests if present

- [ ] **Step 1: Re-export only stable generic types/functions.** Export changelog parsing/rendering and release manifest/verification types needed by future project `xtask` migrations. Do not export GitHub, registry, binary, or publishing helpers because they are intentionally not implemented.

- [ ] **Step 2: Document CLI usage.** Add a concise README section showing changelog validation/notes and release manifest/verify commands, with explicit wording that building, publishing, registry checks, and project behavior remain regular tasks.

- [ ] **Step 3: Run formatting, lint, tests, and package checks.**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace --all-targets --all-features && cargo package --locked`

Expected: all commands pass.

## Task 6: Review migration boundary

**Files:**
- Inspect only: `xtask/src/changelog.rs`, `xtask/src/notes.rs`, `xtask/src/verify.rs`

- [ ] **Step 1: Confirm no project-specific code moved.** GitHub PR link generation, release header text, binary `--version`, example workloads, release plan assertions, and package registry behavior must remain in `xtask`.

- [ ] **Step 2: Confirm no TOML schema changes.** Existing projects should not require `[changelog]`, `[release]`, or `[xtask]` sections to use the new CLI.

- [ ] **Step 3: Record follow-up migration separately.** A later change may make this repository’s `xtask` call the new public library APIs, but this feature must not couple the generic CLI to this repository’s own release claims.
