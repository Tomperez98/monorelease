# Explicit CLI Configuration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Mono and its repository `xtask` CLI accept configuration explicitly through command-line arguments instead of silently reading CLI configuration from environment variables, as a deliberate breaking change.

**Architecture:** Keep environment variables that are runtime inputs rather than CLI configuration: the child process environment, `PATH`, `CI` terminal policy, installer variables, and explicit cache environment selection. Remove all Clap `env = ...` fallbacks and all `xtask` release-tag environment fallback logic. Commands that require release identity will require explicit flags; commands whose domain model intentionally permits optional metadata will retain optional flags without environment inference.

**Tech Stack:** Rust 2024, Clap 4 derive, existing `mono` library error model, integration tests, Markdown documentation, GitHub Actions YAML.

---

## Scope and resulting contract

The following are CLI configuration and must no longer be read implicitly:

- `VERSION`
- `CHANGELOG_PR_URL`
- `RELEASE_TAG`
- `GITHUB_SHA`
- `GITHUB_REPOSITORY`
- `RELEASE_TAG_OBJECT`
- `GITHUB_RUN_URL`
- `RELEASE_COMMIT` / `RELEASE_RUN_URL` where they are only passed as inherited release metadata

The following remain intentionally supported because they are not Mono CLI configuration:

- the environment inherited by task processes
- `PATH` lookup performed by the OS when spawning commands
- `CI` detection used only to choose terminal presentation
- explicit `cache_env` values and wildcard ambient environment hashing
- installer variables such as `MONO_VERSION` and `MONO_INSTALL_DIR`
- `MONO_BIN` for the repository-only `xtask verify` test binary override
- GitHub Actions environment variables used by workflow shell code, provided their values are passed explicitly to CLI flags

Expected command changes:

- `mono changelog prepare`: `VERSION` is no longer a fallback; positional `VERSION` remains optional because the command can infer the next version from the changelog.
- `mono changelog prepare --pull-request-url`: no environment fallback.
- `mono changelog release-notes`: no hidden `RELEASE_TAG` fallback; positional version or visible `--release-tag` is explicit when a tag constraint is desired.
- `mono release source`: `--tag` and `--commit` remain required flags, with no environment fallback.
- `mono release manifest` / `verify`: identity fields remain optional because the provider-neutral manifest model permits partial identity, but every supplied field must be supplied through an explicit flag.
- `xtask verify`, `release-prepare`, and non-restore `release-stamp`: no environment fallback; their tag/version input must be explicit.

---

### Task 1: Remove environment-backed Clap configuration

**Files:**
- Modify: `src/main.rs:241-360`
- Test: `tests/release_cli.rs`
- Test: `src/main.rs` unit tests where parser behavior is covered

- [ ] **Step 1: Add regression coverage for environment variables being ignored**

Update the integration tests to invoke commands with misleading environment variables while passing explicit arguments. Assert that the explicit argument wins and that environment-only configuration no longer succeeds.

Use cases to cover:

```rust
let output = mono_with_env(
    &["changelog", "prepare", "1.1.0"],
    temp.path(),
    &[("VERSION", "9.9.9")],
);
assert!(output.status.success());
assert!(changelog.contains("## 1.1.0"));
assert!(!changelog.contains("## 9.9.9"));
```

Also add a missing-explicit-input test for `release source` with `RELEASE_TAG` and `GITHUB_SHA` set in the child environment. It must exit with Clap usage code `2` and mention `--tag`/`--commit`, proving the old environment fallback is gone.

- [ ] **Step 2: Run the focused tests and verify the new tests fail**

Run:

```bash
cargo test --test release_cli changelog -- --nocapture
cargo test --test release_cli release -- --nocapture
```

Expected: the new environment-only/explicit precedence assertions fail against the current `env = ...` declarations.

- [ ] **Step 3: Remove `env = ...` from the main CLI declarations**

In `src/main.rs`:

- remove `env = "VERSION"` from the positional changelog version
- remove `env = "CHANGELOG_PR_URL"` from `--pull-request-url`
- remove `env = "RELEASE_TAG"` from hidden `--release-tag`
- remove all environment attributes from `ReleaseIdentityOptions`
- remove `env = "RELEASE_TAG"` from `SourceIdentityOptions::tag`
- remove `env = "GITHUB_SHA"` from `SourceIdentityOptions::commit`

Update help text so it describes explicit flags rather than environment defaults. Keep `non_empty` only if it remains needed for explicitly supplied optional strings; otherwise remove it.

The resulting release identity declarations should have the same optional/required shape as today, except their values can only come from command-line arguments.

- [ ] **Step 4: Run the focused tests and then the full test suite**

Run:

```bash
cargo test --test release_cli
cargo test --all-targets
```

Expected: all tests pass after updating tests that previously depended on environment fallback.

- [ ] **Step 5: Commit the main CLI contract change**

```bash
git add src/main.rs tests/release_cli.rs
git commit -m "feat: require explicit mono CLI configuration"
```

---

### Task 2: Remove environment fallback from `xtask`

**Files:**
- Modify: `xtask/src/main.rs:42-178,307-422`
- Test: `xtask/src/main.rs` unit tests
- Test: `tests/release_cli.rs` only if repository-level behavior is exercised there

- [ ] **Step 1: Add parser regression tests for `xtask`**

Add tests that execute or parse the `xtask` binary with `RELEASE_TAG` set but no explicit tag. Assert:

- `verify` fails with a usage/argument error rather than reading `RELEASE_TAG`.
- `release-prepare` fails without `--tag`.
- `release-stamp` fails without `--version` unless `--restore` is supplied.

Also preserve the invariant that `release-stamp --restore --version ...` is rejected.

- [ ] **Step 2: Run the focused tests and verify they fail**

Run:

```bash
cargo test -p xtask
```

Expected: the new tests fail because `Tag::from_env` currently supplies missing tags.

- [ ] **Step 3: Make required `xtask` inputs explicit**

Change the `Command` variants:

```rust
Verify { tag: String }
ReleasePrepare { tag: String }
```

Keep `ReleaseStamp { version: Option<String>, restore: bool }`, but when `restore` is false return an `Error::Invalid` explaining that `--version` is required. Do not inspect `RELEASE_TAG`.

Delete `Tag::from_env`. Update dispatch helpers so they parse the supplied strings directly:

```rust
fn verify_command(tag: String) -> Result<(), Error> {
    let tag = Tag::parse(&tag)?;
    verify::run(&tag, &binary_path())
}

fn release_prepare_command(tag: String) -> Result<(), Error> {
    let version = parse_version(&tag)?;
    release::prepare(&stamp::repository_root(), &tag, version)
}
```

For `release-stamp`, preserve `--restore` behavior and replace the environment fallback with an explicit missing-version error.

Update Clap help text and `after_help` examples to remove references to `RELEASE_TAG` defaults.

- [ ] **Step 4: Update repository workflows and internal callers**

Confirm every `xtask verify`, `release-prepare`, and `release-stamp` invocation passes its tag/version explicitly. Keep workflow environment variables when shell steps need them, but do not rely on them as CLI defaults.

The existing release workflow already passes `--tag` for release preparation/build/docs/contract/version checks/publish. Remove only redundant environment assignments if they are no longer used by the command itself; retain variables needed by shell interpolation or GitHub URLs.

- [ ] **Step 5: Run all `xtask` and CLI tests**

Run:

```bash
cargo test -p xtask
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

Expected: all tests pass with no warnings.

- [ ] **Step 6: Commit the `xtask` change**

```bash
git add xtask/src/main.rs xtask/src/*.rs .github/workflows/release.yml .github/workflows/release_validate.yml
 git commit -m "feat: require explicit xtask release inputs"
```

---

### Task 3: Update documentation to state the explicit-input policy

**Files:**
- Modify: `docs/commands.md`
- Modify: `docs/releases.md`
- Modify: `README.md` if it mentions environment-backed CLI configuration
- Modify: `xtask/src/main.rs` help strings if any wording remains inconsistent

- [ ] **Step 1: Replace environment fallback documentation**

Update changelog documentation:

- remove `or set CHANGELOG_PR_URL`
- explain that `--pull-request-url` is the only Mono CLI input for that value
- explain that `changelog release-notes [VERSION]` uses the newest changelog entry only when no explicit version/tag is supplied
- remove claims that CI-provided `RELEASE_TAG` is accepted implicitly

Update release documentation:

- state that `--tag`, `--commit`, `--repository`, `--tag-object`, and `--workflow-run` are explicit CLI inputs when used
- state that environment variables may still be used by workflow shell scripts, but must be forwarded as flags
- document the breaking change clearly

- [ ] **Step 2: Add explicit command examples**

Use examples such as:

```console
mono changelog prepare 1.2.0 \
  --pull-request-url 'https://github.com/org/repo/pull/{number}'

mono changelog release-notes 1.2.0

mono release source --tag v1.2.0 --commit "$commit"
mono release manifest --dist dist --tag v1.2.0 --commit "$commit"
```

The documentation must not suggest setting `VERSION`, `CHANGELOG_PR_URL`, or `RELEASE_TAG` to configure these commands.

- [ ] **Step 3: Search for stale claims**

Run:

```bash
rg -n 'defaults to RELEASE_TAG|CHANGELOG_PR_URL|VERSION.*environment|GITHUB_SHA|GITHUB_REPOSITORY|RELEASE_TAG.*accepted|Tag::from_env|env = "(VERSION|RELEASE_TAG|GITHUB_SHA|GITHUB_REPOSITORY|CHANGELOG_PR_URL)' .
```

Expected: only intentional references remain, such as installer variables, workflow shell plumbing, or historical migration notes.

- [ ] **Step 4: Commit documentation**

```bash
git add README.md docs/commands.md docs/releases.md xtask/src/main.rs
 git commit -m "docs: document explicit CLI configuration"
```

---

### Task 4: Final compatibility and platform verification

**Files:**
- No new production files
- Inspect: `src/main.rs`, `xtask/src/main.rs`, workflows, documentation

- [ ] **Step 1: Verify environment boundaries**

Confirm the following remain unchanged:

- task commands inherit their configured process environment
- `cache_env = ["*"]` still hashes ambient variables
- `CI` still affects automatic UI selection
- installers still support their documented environment variables
- `MONO_BIN` still overrides the binary used by `xtask verify`

- [ ] **Step 2: Run formatting, tests, lint, and cross-target checks**

Run:

```bash
cargo fmt --all -- --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo check --target x86_64-pc-windows-msvc --all-targets
cargo check --target aarch64-apple-darwin --all-targets
```

Expected: all commands pass.

- [ ] **Step 3: Manually verify the public behavior**

Run these checks:

```bash
VERSION=9.9.9 mono changelog prepare 1.2.0
RELEASE_TAG=v9.9.9 mono release source
cargo run -p xtask -- verify
cargo run -p xtask -- release-stamp
```

Expected:

- the first command prepares `1.2.0`, not `9.9.9`
- the second fails with missing explicit `--commit`/required flags rather than reading the environment
- the third fails because `--tag` is required
- the fourth fails because `--version` or `--restore` is required

- [ ] **Step 4: Review the final diff and commit if needed**

```bash
git diff HEAD~3..HEAD --check
git status --short
```

Ensure no unrelated files or environment behavior were changed.
