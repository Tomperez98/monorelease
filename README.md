# mono (release)

Run every package's tasks in dependency order, from one `monorepo.toml` — in any language.

[![CI](https://github.com/Tomperez98/monorelease/actions/workflows/ci.yml/badge.svg)](https://github.com/Tomperez98/monorelease/actions/workflows/ci.yml)
[![Release](https://github.com/Tomperez98/monorelease/actions/workflows/release.yml/badge.svg)](https://github.com/Tomperez98/monorelease/actions/workflows/release.yml)
![License](https://img.shields.io/badge/license-Apache--2.0-blue)

`monorelease` reads your manifests, builds one task graph across every package, and runs the commands. It does not know whether a package is Rust, Node, Go, Make, Docker, or a shell script: a task is an argv array, executed directly with no shell in between.

```console
$ monorelease --dir examples/echo task build --jobs 2
▶ docs:build
▶ shared:build
▶ app:build
[shared] build
shared:build: completed in 4ms
[app] build after shared:build
app:build: completed in 4ms
[docs] build from site with MODE=check
docs:build: completed in 4ms
summary: 3 completed, 0 cached, 0 failed, 0 blocked across 3 package(s)
```

`app:build` waited for `shared:build`. `docs:build` ran alongside them in its own directory with its own environment. No central registry of packages, no language plugins, no build backend.

## Install

```bash
cargo install --path .
```

Building requires Rust 1.88+ (edition 2024). Prebuilt binaries for Linux x86_64, macOS arm64, macOS x86_64, and Windows x86_64 are attached to each [GitHub release](https://github.com/Tomperez98/monorelease/releases) together with a `SHA256SUMS` file:

```bash
sha256sum -c SHA256SUMS
```

## Quick start

A project does not need to be a monorepo. One root manifest is enough:

```toml
# monorepo.toml
[package]
name = "my-project"

[pipelines.ci]
tasks = ["build", "test"]

[tasks.build]
command = ["cargo", "build"]

[tasks.test]
command = ["cargo", "test"]
depends_on = ["build"]
```

```bash
monorelease check    # validate manifests and the task graph
monorelease          # run the default pipeline (ci)
monorelease plan     # print the resolved order without running anything
```

Scaffold that file instead of writing it by hand. The generated package is named `project`; rename it in the manifest afterward:

```bash
monorelease init --standalone --command cargo --command build
```

For a monorepo, `monorelease init` writes a root workspace with member patterns instead:

```bash
monorelease init
```

```toml
[workspace]
name = "monorepo"
members = ["apps/*", "packages/*"]
default_pipeline = "ci"

[pipelines.ci]
tasks = ["build", "test"]
```

Then add one manifest per package. `apps/web` can depend on `packages/shared` without either package knowing about the other:

```toml
# apps/web/monorepo.toml
[package]
name = "web"

[tasks.build]
command = ["npm", "run", "build"]
depends_on = ["shared:build"]
```

## Commands

`--dir <PATH>` is global and defaults to `.`; root discovery walks up from there, so commands work from any nested directory.

| Command | What it does |
| --- | --- |
| `monorelease` | Run the default pipeline. |
| `monorelease run [PIPELINE]` | Run the default or a named pipeline (alias: `ci`). |
| `monorelease task TASK...` | Run one or more tasks and their transitive dependencies. |
| `monorelease check` | Validate manifests, references, and working directories (alias: `doctor`). |
| `monorelease plan [PIPELINE]` | Print the dependency-first execution plan. |
| `monorelease graph [PIPELINE]` | Print the dependency edges. |
| `monorelease list` | List pipelines, tasks, and common commands. |
| `monorelease init [--standalone] [--command CMD]` | Write a fresh manifest; refuses to overwrite an existing one. |
| `monorelease cache clean` | Delete all local cache entries. |

`run` and `task` accept:

| Flag | Default | Effect |
| --- | --- | --- |
| `--package NAME` | all packages | Restrict the run to one package, keeping its transitive dependencies. |
| `--jobs N` | machine CPUs | Maximum independent tasks to execute concurrently. Must be at least `1`. Defaults to this machine's available parallelism. |
| `--dry-run` | off | Resolve and print the plan; run nothing and touch no cache. |
| `--no-cache` | off | Skip reading and writing the cache for this run. |
| `--force` | off | Ignore cache hits and refresh successful entries. |
| `--output terminal\|github-actions` | `terminal` | Select the output contract. |

```bash
monorelease task test --package web --jobs 4
monorelease run release --dry-run
monorelease --dir examples/echo task build --jobs 2
```

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | The command succeeded. |
| `1` | The command was understood and failed: an invalid or unreadable manifest, an absent `--dir`, an unknown task or package, a task that exited non-zero, or a changelog or release check that did not pass. |
| `2` | The command line was wrong. `clap` rejects it while parsing, so a bad value never reaches the pipeline — `--jobs 0` and empty values such as `--package ""` fail here. |
| `3` | `monorelease` could not carry the command out: `init` could not write the manifest, `changelog` or `release` could not read or write its files, `git` could not be run, the cache could not be updated, or task output could not be written. |

`1` and `3` are the useful pair in CI: `1` means the pipeline is red, `3` means the tool never got far enough to tell you.

## Manifest model

A root manifest is either a workspace or a standalone project — never both. Package manifests declare identity and tasks only.

```toml
[workspace]
name = "acme"
members = ["apps/*", "packages/*"]
default_pipeline = "ci"

[pipelines.ci]
tasks = ["build", "test"]

[pipelines.release]
tasks = ["workspace:release-verify"]

# Root tasks run once, from the workspace root, and are addressed as
# `workspace:<name>`. Use them for cross-package coordination such as
# release validation.
[tasks.release-verify]
command = ["./automation/release-verify"]
depends_on = ["web:package", "api:package"]
cwd = "automation"
timeout_seconds = 1800
```

```toml
[package]
name = "web"

[tasks.build]
command = ["npm", "run", "build"]
depends_on = ["shared:build"]

[tasks.test]
command = ["npm", "test"]
depends_on = ["build", "shared:test"]
resource_group = "checks"
```

Every task accepts the same fields:

| Field | Default | Meaning |
| --- | --- | --- |
| `command` | — (required) | Executable and arguments as an array. No shell, so pipes and redirects are not interpreted. |
| `depends_on` | `[]` | Task references: `build` (same package), `shared:build` (another package), `workspace:verify` (root task). |
| `cwd` | package root | Relative directory inside the package. Must exist and resolve inside the package after symlinks. |
| `env` | `{}` | Extra child-process environment. Values are redacted in plan output. |
| `cache` | `false` | Allow a successful result to be reused from the local cache. |
| `inputs` | `[]` | Globs whose contents are hashed into the cache key. |
| `outputs` | `[]` | Globs copied into the cache and restored on a hit. |
| `cache_env` | `[]` | Environment variable names that affect the cache key; `["*"]` means the whole environment. |
| `timeout_seconds` | `600` | Wall-clock limit for one invocation. Must be greater than zero. |
| `max_output_bytes` | `16777216` | Per-stream capture limit. A task that exceeds it is killed and reports its partial output. |
| `resource_group` | unset | Tasks sharing a group never run concurrently. |

Names must be non-empty and cannot contain `:`. `workspace` is reserved for root tasks.

The schema is fixed: there is no `version` header, and unknown fields are rejected, so typos fail in `check` instead of being ignored.

## Execution model

- The plan is a dependency-first topological order. Cycles, unknown packages, unknown tasks, and unresolvable references are rejected before anything runs; near-misses get a "did you mean" suggestion.
- With `--jobs N`, a task starts as soon as its dependencies finish and a worker is free. Independent branches overlap; `resource_group` and dependencies hold back only what they must. `N` defaults to this machine's available parallelism (cgroup quotas and CPU affinity included), so pass `--jobs 1` for strictly serial execution. The default is a dispatch limit, not a promise about the commands themselves: a task that starts its own workers, such as a compiler, can oversubscribe the machine.
- Every task's stdout and stderr are captured. Task output and status lines are presented in deterministic plan order, so parallel logs never interleave. In `terminal` mode status goes to stderr and task output to stdout; `github-actions` mode wraps each task in `::group::` on stdout.
- A failure or timeout stops new work from being scheduled while in-flight tasks finish. The summary reports `completed`, `cached`, `failed`, and `blocked`, and the process exits `1`.
- Nothing is implicit: no shell, no hidden environment merging, no working-directory mutation.

```console
$ monorelease --dir examples/release-gate plan release
monorepo release-gate (/path/to/examples/release-gate)
would run api:build in .../packages/api: echo '[api] build' [timeout=30s] [max_output_bytes=16777216]
would run api:test in .../packages/api: echo '[api] test' [timeout=30s] [max_output_bytes=16777216] [resource_group=checks]
would run api:package in .../packages/api: echo '[api] package' [timeout=30s] [max_output_bytes=16777216]
...
would run workspace:release-verify in .../automation: echo '[workspace] verify artifacts, installability, and versions' [timeout=60s] [max_output_bytes=16777216] [resource_group=release-gate]
would run workspace:release-summary in .../automation: echo '[workspace] release gate passed once for all packages' [timeout=60s] [max_output_bytes=16777216]
```

## Caching

Caching is opt-in, because a task's side effects are yours to declare. A cacheable task must declare at least one positive input pattern:

```toml
[tasks.build]
command = ["cargo", "build", "--release"]
cache = true
inputs = ["src/**", "Cargo.toml", "Cargo.lock"]
outputs = ["target/release/my-binary"]
cache_env = ["RUSTFLAGS"]
```

A cache hit replays the captured stdout/stderr and restores the declared outputs, so downstream tasks see the same artifacts as a real run:

```console
$ monorelease --dir examples/cache run
▶ app:build
[build] generated artifact from input: hello from the cache example
app:build: cache hit in 0ms
▶ app:verify
[verify] artifact: hello from the cache example
app:verify: completed in 9ms
summary: 1 completed, 1 cached, 0 failed, 0 blocked across 1 package(s)
```

The key covers the task's package, name, cwd, command, timeout, output limit, resource group, input/output patterns, declared environment values, dependency keys, and the workspace and package manifest contents. Changing a declared input changes the key.

Patterns select files with `*` (within a path segment), `**` (across segments), and a leading `!` to exclude; the last matching pattern wins. `.git` and `.monorelease` are never walked. A positive pattern that matches no files is an error, and symlink matches are rejected because content addressing cannot represent them. `.monorelease/.gitignore` keeps the store out of Git.

Failed and timed-out tasks are never cached. `--force` re-runs cacheable tasks and refreshes their entries, `--no-cache` bypasses the cache for a run, and `--dry-run` never reads or writes it. `cache_env = ["*"]` hashes the complete inherited environment: more correct for environment-sensitive tasks, fewer hits.

Do not cache publishing, deployment, migration, time-dependent, network-dependent, or otherwise nondeterministic tasks. Caching is local only; there is no remote cache.

## Discovery

`workspace.members` is evaluated relative to the root manifest. Literal segments, `*`, and `**` are supported, and every match must be a directory containing a package manifest.

Root discovery walks up from `--dir` until it finds a manifest: the nearest `[workspace]` wins, otherwise the nearest `[package]`. A manifest containing both sections is rejected. A package manifest may contain only `[package]` and `[tasks]`; pipelines live in the root.

`monorelease` plans and schedules work. It does not fetch dependencies, bump versions, or publish artifacts — a task does that by running the command your package already uses.

## Examples

Runnable workspaces under [`examples/`](examples), each with its own README:

| Example | Demonstrates |
| --- | --- |
| [`examples/standalone`](examples/standalone/README.md) | One manifest, one package, tasks with a nested `cwd`. |
| [`examples/echo`](examples/echo/README.md) | Package discovery, local and cross-package dependencies, pipelines, package selection, timeouts, resource groups, dry runs, and parallel scheduling. |
| [`examples/cache`](examples/cache/README.md) | Cache hits, output restoration, and invalidation when an input changes. |
| [`examples/release-gate`](examples/release-gate/README.md) | A `workspace:` task that coordinates a release once after every package is ready. |

```bash
cargo run -- check --dir examples/echo
cargo run -- run --dir examples/echo --jobs 2
```

## CI and releases

The project dogfoods itself: `.github/workflows/ci.yml` runs the pipeline defined in its own root `monorepo.toml`, so local and hosted checks use the same task graph.

```bash
cargo run --locked -- ci --no-cache --output github-actions
```

Releasing is a pipeline too, and it is the only place release knowledge lives:

```bash
RELEASE_TAG=v0.1.2 cargo run --locked -- run release
```

The tasks call [`xtask/`](xtask), a Rust helper that is deliberately *not* part of the published binary. It contains this repository's project-specific release gates: binary behavior, examples, and release-plan assertions. `monorelease` now provides the provider-neutral changelog and artifact operations that can be reused by any language or package manager.

### Generic changelog and release commands

These commands require no additional `monorepo.toml` sections:

```bash
monorelease changelog validate
monorelease changelog scaffold --version 0.1.2
RELEASE_TAG=v0.1.2 monorelease changelog notes --output RELEASE_NOTES.md

monorelease release source --tag v0.1.2 --commit "$GITHUB_SHA"
monorelease release manifest --directory dist
monorelease release verify --directory dist --tag v0.1.2
```

Changelog commands validate and render the repository's structured `CHANGELOG.md`; they do not assume GitHub, pull requests, or a programming language. Release commands operate on files, checksums, and explicit source identity. They do not publish, inspect registries, execute binaries, or assume a package format.

The project's `release-notes` task turns `CHANGELOG.md` into `RELEASE_NOTES.md`, while `release-verify` asserts this repository's project-specific claims: that its binary reports the version it claims, that its own manifests validate, that examples run, and that the documented release plan resolves. The generic `monorelease release verify` command validates published metadata and artifacts separately. The version of a release is the newest `## ` entry of [`CHANGELOG.md`](CHANGELOG.md), so the notes a reviewer approves are the notes users read. Name that entry `## (unreleased)` to fold a skipped release into the next one.

To cut a release, scaffold the newest changelog entry, fill in the user-facing notes, set the same version in `Cargo.toml`, merge, then push the tag:

```bash
cargo run --locked -- changelog scaffold --version 0.1.2
# edit CHANGELOG.md
# CHANGELOG.md: ## 0.1.2  |  Cargo.toml: version = "0.1.2"
git tag v0.1.2
git push origin v0.1.2
```

The `Release` workflow refuses a tag that matches neither `Cargo.toml` nor the newest changelog entry, verifies that the tag resolves to the checked-out commit, runs the project's `ci` pipeline and release gates, builds the four targets, asserts each binary reports the release version, and generates GitHub Actions provenance for each archive. Before publication it requires exactly the four expected archives, `SHA256SUMS`, and `BUILD-METADATA.json`. The draft only becomes the latest release once every asset is uploaded. Re-running is safe: an existing draft is reused, `--clobber` refreshes its assets, and an already published tag is never overwritten. If a run fails halfway, resume it from the tag instead of moving it:

```bash
gh workflow run Release --field tag=v0.1.2
```

`Release (validate)` runs weekly and after every successful release, sharing the `release` concurrency group so it cannot race publication. It checks out the released tag, verifies `BUILD-METADATA.json`, downloads the published assets, rebuilds the Linux binary from the tag, compares it with the published binary, verifies its GitHub Actions provenance, and runs the release pipeline with the published binary — checksums, identity, manifests, and examples. Tags cut before metadata and provenance existed fall back to the legacy checksum/version checks.

The publishing job uses the protected GitHub `release` environment. Repository administrators must create that environment and may configure required reviewers before a release can become public. Published releases are immutable: retry drafts, but supersede a bad published release with a new fix-forward version rather than moving tags or replacing assets.

## License

Apache-2.0. See [LICENSE](LICENSE).
