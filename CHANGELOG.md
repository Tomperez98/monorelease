# Changelog

Notable changes per release, newest first.


## 0.1.5
Released: 2026-09-12

### Features

-

### Fixes

-

### Internals

-

## 0.1.4
Released: 2026-09-11

### Breaking Changes

- Replace the flat `--output terminal|live|json|github-actions` interface with
  `--output text|json` and `--ui auto|tui|stream`. Interactive runs now use a
  task-oriented TUI automatically when attached to a terminal.

### Features

- Use one root `mono.toml` task graph for every project shape.
- Document Mono's opinionated Markdown/SemVer release conventions.
- Add matrix task expansion with argv/environment interpolation.
- Add retry policies and pipeline finalizers for cleanup and diagnostics.
- Add explicit release artifact inventory validation.
- Add structured JSON execution events for machine-readable task orchestration.
- Add task-prefixed stream output and an interactive per-task terminal UI for
  concurrent pipeline execution.

### Fixes

- Terminate descendant processes when a task times out and reject cache restores through symlinked paths.

### Internals

- Delegate cross-platform process-group and Job Object lifecycle management to
  `process-wrap`, keeping platform-specific process code out of Mono.
- Add platform-aware cache keys, output digests, and concurrent cache publication.
- Validate process-tree behavior across the supported operating systems in CI.
- Harden Git release-source validation and publish release metadata through temporary files with atomic replacement where supported.
- Remove package manifests, member discovery, standalone mode, and task artifacts while keeping the `mono.toml` schema at `1`.
- Version the execution event contract while keeping the `mono.toml` schema at `1`.

The top `## ` entry is the release being prepared. `release.yml` requires the git
tag to be `v` + that heading, and the body under it becomes the GitHub release
notes, so the changelog a reviewer approves is exactly what users read. Cutting a
release is:

1. run `mono changelog prepare` (or pass an explicit version),
2. edit and review the entry with `mono changelog check`,
3. merge it, then push the tag `v<version>`.

If a release is skipped, prepare the entry as `## (unreleased)` and fold it into the
next version on the following release. Versions are monotonic but may skip
numbers: shipping nothing is cheaper than shipping something broken. `Released:`
is the date the entry was drafted; correct it if the release slips. Committed
package manifests remain pinned placeholders and are stamped from the tag during
release preparation.

## 0.1.3
Released: 2026-09-11

### Features

- Rename project to `mono`
- Add changelog and notes commands

## 0.1.2
Released: 2026-09-11

### Features

- `max_output_bytes` bounds how much of a task's stdout or stderr is captured. A
  task that exceeds it is killed and reports the output it did produce.
- `--jobs` defaults to the CPUs this process may actually use, including cgroup
  quotas and CPU affinity, instead of a hard-coded `1`.

### Fixes

- `--version`, `--help`, and error prefixes report the program as `mono`.
  They previously said `monore`.
- The documented Rust minimum is now 1.88, matching the language features used
  by the workspace.
- Release validation selects the root `mono` package instead of the
  repository-only `xtask` member when checking the tag version.

### Internals

- Releasing moved into this repository's own `mono.toml` as a `release`
  pipeline: notes are generated from this file, and the binary is run against
  `examples/` before a release is published and again after. The release tasks
  are Rust programs in `xtask/`, which is not part of the published crate.
- The local cache and the scheduler were reworked for content-addressed keys and
  deterministic, plan-ordered output.
- Releases now record source identity, publish exact platform inventories,
  generate GitHub Actions provenance, and rebuild the released Linux artifact
  during recurring validation.

## 0.1.0
Released: 2026-09-10

First release: prebuilt binaries for Linux x86_64, macOS arm64, macOS x86_64,
and Windows x86_64, attached with `SHA256SUMS`.
