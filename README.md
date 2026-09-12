# mono

Run a project's build, test, lint, and release commands from one root `mono.toml`—in dependency order, in any language.

[![Mono running the ci pipeline in the interactive task view](https://asciinema.org/a/5JVHsZ7ruT557Qpu.svg)](https://asciinema.org/a/5JVHsZ7ruT557Qpu)

```console
$ mono plan
project mono (/repo/mono)
would run fmt in /repo/mono: cargo fmt --check [timeout=30s] [max_output_bytes=16777216]
would run check in /repo/mono: cargo check --workspace ... [timeout=120s] [max_output_bytes=16777216]
```

Mono is a task orchestrator, not a package manager or workspace detector. You describe the commands and their dependencies; Mono validates the graph, runs independent tasks concurrently, and gives humans and CI the same execution contract.

## Install

### Install script

The release installer carries its version and archive digest, verifies the
archive before extraction, and installs to `~/.local/bin`.

Linux and macOS:

```bash
curl -fsSL https://github.com/Tomperez98/mono/releases/latest/download/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://github.com/Tomperez98/mono/releases/latest/download/install.ps1 | iex
```

Each release also publishes version-pinned copies of both installers as release
assets. To install a specific release:

```bash
curl -fsSL https://github.com/Tomperez98/mono/releases/download/v0.1.5/install.sh | sh
```

```powershell
irm https://github.com/Tomperez98/mono/releases/download/v0.1.5/install.ps1 | iex
```

The release installers carry the selected release's archive checksums and do not
query the GitHub API at install time. Pass options after `sh -s --`, or use
`MONO_VERSION` and `MONO_INSTALL_DIR` when a script arrives through a pipe:

```bash
curl -fsSL https://github.com/Tomperez98/mono/releases/latest/download/install.sh \
  | MONO_INSTALL_DIR="$HOME/.local" sh
```

Or pass an explicit option:

```bash
curl -fsSL https://github.com/Tomperez98/mono/releases/latest/download/install.sh \
  | sh -s -- --prefix /usr/local
```

Remove the binary with `rm ~/.local/bin/mono`, or `Remove-Item` on Windows — see
[Uninstall](#uninstall).

### Prebuilt binaries

Download the archive for your platform from the [GitHub releases page](https://github.com/Tomperez98/mono/releases), extract `mono`, and put it on your `PATH`.

| Platform | Archive suffix |
| --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-gnu.tar.gz` |
| macOS arm64 | `aarch64-apple-darwin.tar.gz` |
| macOS x86_64 | `x86_64-apple-darwin.tar.gz` |
| Windows x86_64 | `x86_64-pc-windows-msvc.zip` |

Archives are named `mono-v<version>-<platform>.<format>`.

### Build from source

From a checkout, with Rust 1.88 or newer:

```bash
cargo install --path .
```

### Uninstall

Mono keeps no state outside the project it runs in, so uninstalling is deleting
the binary the same way you would delete any other executable:

```bash
rm ~/.local/bin/mono          # install.sh
cargo uninstall mono          # cargo install
```

```powershell
Remove-Item -Force "$HOME\.local\bin\mono.exe"   # install.ps1
```

The one thing not to delete by hand is a `cargo install`: cargo records what it
installed, so `cargo uninstall mono` is the spelling that keeps that bookkeeping
honest. Per-project caches are separate and already scoped — `mono cache clean`
removes one project's `.mono/` directory, and nothing outside it.

## Quick start

This repository is itself configured with Mono. From a checkout, inspect the dependency-first plan and run the CI pipeline:

```bash
cargo install --path .
mono plan
mono run ci --ui stream
```

To add Mono to another repository, create a starter manifest at its root:

```bash
mono init
```

Then replace the generated placeholder task with the commands your project needs. For example, this is a complete pipeline for a Rust project:

```toml
schema = 1

[project]
name = "my-project"
default_pipeline = "ci"

[pipelines.ci]
tasks = ["build", "test", "lint"]

[tasks.build]
command = ["cargo", "build", "--workspace"]

[tasks.test]
command = ["cargo", "test", "--workspace"]
depends_on = ["build"]

[tasks.lint]
command = ["cargo", "clippy", "--workspace", "--", "-D", "warnings"]
depends_on = ["test"]
```

Validate the manifest, preview the plan, then run it:

```bash
mono check
mono plan
mono run
```

The same model works for Go, Python, JavaScript, Zig, or a repository that mixes languages. Commands are argv arrays, so Mono does not insert a shell or reinterpret arguments.

## The manifest model

A project has one root `mono.toml`. The manifest contains named pipelines and a global task graph:

```toml
schema = 1

[project]
name = "acme"
default_pipeline = "ci"

[pipelines.ci]
tasks = ["fmt", "test"]

[pipelines.release]
tasks = ["release-verify"]
finally = ["release-cleanup"]

[tasks.fmt]
command = ["formatter", "check"]

[tasks.test]
command = ["test-runner"]
depends_on = ["fmt"]
timeout_seconds = 900
retries = 1
retry_backoff_seconds = 5

[tasks.release-verify]
command = ["./automation/release-verify"]
cwd = "automation"
depends_on = ["test"]

[tasks.release-cleanup]
command = ["./automation/release-cleanup"]
cwd = "automation"
```

### Tasks

| Field | Default | Meaning |
| --- | --- | --- |
| `command` | required | Executable and arguments as an array. No shell is inserted. |
| `depends_on` | `[]` | Global task IDs that must complete first. |
| `cwd` | project root | Existing directory inside the project root. |
| `env` | `{}` | Extra child-process environment. |
| `stdin` | `null` | Use `inherit` to pass the parent's standard input through. |
| `cache` | `false` | Allow successful results to be reused. |
| `inputs` | `[]` | Relative patterns included in the cache fingerprint. |
| `outputs` | `[]` | Relative files copied into and restored from the cache. |
| `cache_env` | `[]` | Environment variables included in the cache key; `*` means all. |
| `timeout_seconds` | `600` | Wall-clock limit for one invocation. |
| `max_output_bytes` | `16777216` | Per-stream capture limit. |
| `resource_group` | unset | Tasks sharing a group never overlap. |
| `retries` | `0` | Additional attempts after a failed invocation. |
| `retry_backoff_seconds` | `0` | Delay between attempts. |
| `matrix.<name>` | unset | Opaque dimensions expanded into task instances. |

`outputs` are cache outputs, not release artifacts. Release files are handled by
`mono release manifest` and `mono release verify`, which inventory an explicit
directory and generate `BUILD-METADATA.json` and `SHA256SUMS`.

Every task is root-scoped. Set `cwd` when a command belongs to a subdirectory:

```toml
[tasks.api-test]
cwd = "services/api"
command = ["go", "test", "./..."]
```

From a nested directory, Mono walks upward from `--dir` until it finds the root manifest:

```bash
mono --dir services/api task api-test
```

There are no package manifests, member globs, standalone mode, package selection, or language-specific task types. A repository with one directory and a repository with one hundred directories use the same model.

## Commands

| Command | What it does |
| --- | --- |
| `mono` | Run the default pipeline. Accepts the global flags only; use `mono run` for the execution flags. |
| `mono run [PIPELINE]` | Run a named pipeline, or the default pipeline when `PIPELINE` is omitted. `ci` is the conventional CI pipeline name. |
| `mono task TASK...` | Run specific tasks and their dependencies, skipping the rest of the pipeline. |
| `mono check` | Validate `mono.toml` and the complete task graph. Also available as `mono doctor`. |
| `mono list` | List this project's pipelines, tasks, and suggested commands. |
| `mono plan [PIPELINE]` | Print the dependency-first execution plan without running anything. |
| `mono graph [PIPELINE]` | Print task dependency edges. |
| `mono cache clean` | Delete local cache entries. |
| `mono changelog check` | Validate a changelog against Mono's release conventions. |
| `mono changelog prepare [VERSION]` | Infer or create an editable release entry. |
| `mono changelog release-notes [VERSION]` | Extract notes from the newest entry. |
| `mono release ...` | Create or verify provider-neutral release metadata. |

Run `mono --help` or `mono help <command>` for the complete option list.

## Execution and CI

Independent tasks run concurrently up to `--jobs` (the default is the number of CPUs available to the process). Use `resource_group` when tasks share an external resource that must be serialized.

`run` and `task` support:

| Flag | Effect |
| --- | --- |
| `--jobs N` | Maximum number of independent tasks to execute concurrently. |
| `--dry-run` | Resolve and print the plan without running commands. |
| `--no-cache` | Skip cache reads and writes. Conflicts with `--force`. |
| `--force` | Ignore cache hits and refresh successful entries. Conflicts with `--no-cache`. |
| `--output text\|json` | Select the human or machine output contract. |
| `--ui auto\|tui\|stream` | Select the human execution presentation for `run` and `task`; bare `mono` accepts it before the command. |

`mono` with no command runs the default pipeline but accepts the global flags only. Use `mono plan` to preview it, or `mono run` to run it with the execution flags.

`--ui auto` uses the interactive task UI on a terminal and task-prefixed streaming output in pipes and CI. `--ui tui` currently resolves the same way, so the full-screen view is used only when a terminal is attached; pass `--ui stream` for predictable task-prefixed lines anywhere. Use `--output json` for a versioned newline-delimited machine contract; it never starts the TUI.

JSON execution emits `run_started`, `task_started`, `task_output`, `task_finished`, and `run_finished` events. `plan`, `graph`, `list`, and `check` emit one JSON document with `schema = 1` and a command-specific `kind`. Environment values are never included in plan or list output; only configured variable names are reported.

Mono validates the complete plan before starting work. Unknown tasks, cycles, invalid paths, bad matrix references, and invalid task configuration fail early. A normal task failure stops new normal work; in-flight tasks finish, then pipeline finalizers run as cleanup work. Finalizers are not cacheable because a cache hit must not skip cleanup side effects.

Timeouts terminate the complete child process tree using Unix process groups or Windows Job Objects. Ctrl-C cancels normal tasks, terminates their process trees, and still permits finalizers to run. Direct execution never changes Mono's process-global working directory.

## Release conventions

Mono uses the same provider-neutral release pattern for every project that adopts its release commands:

- `CHANGELOG.md` is Markdown.
- The newest heading is `## (unreleased)` or `## X.Y.Z`.
- Version entries contain `Released: YYYY-MM-DD`.
- The newest versioned changelog entry is the release being prepared.
- Release tags are `vX.Y.Z` and must match the newest entry and resolve to the checked-out source commit.
- The matching changelog entry becomes `RELEASE_NOTES.md`.
- Prepared entries contain release metadata and meaningful bullets, not empty category placeholders.
- Release directories contain an explicit, verified artifact inventory and SHA-256 metadata.
- Publishing remains an ordinary project task or CI step; Mono does not know registries or package managers.

The repository's release files use pinned placeholders for reproducible source releases: the root package in `Cargo.toml`, the `mono` package in `Cargo.lock`, and the displayed Zensical site version are pinned at `0.0.0`. The repository-owned `xtask` coordinator receives an explicit release identity, stamps them from the tag, builds the four canonical native archives, writes `site/release.json`, verifies every user-visible version, and restores the placeholders. The committed files never move. GitHub Actions only provisions toolchains, moves artifacts, deploys Pages, and invokes the coordinator; release commands do not infer their identity from ambient environment variables.

Prepare and review a release from the newest changelog entry:

```console
mono changelog prepare
mono changelog check
mono changelog release-notes
```

To explicitly skip a release cycle, prepare `unreleased`. To seed an entry
with editable first-parent merge bullets, pass `--from REF --to REF`. Add
`--pull-request-url 'https://github.com/org/repo/pull/{number}'` to link
recognized PR merge commits; this is a read-only Git operation.

Run the pinned-state release preflight before creating the tag:

```console
cargo run --locked --quiet -- run release-preflight --no-cache --ui stream
```

Create and push an annotated release tag only after the preflight and changelog
are approved; the tag push starts the GitHub release workflow:

```console
cargo run --locked --quiet -p xtask -- tag --tag v0.1.3
```

The tag is the only version input. To stamp a checkout manually:

```console
cargo run --locked --quiet -p xtask -- release-stamp --version 0.1.5
cargo run --locked --quiet -p xtask -- release-stamp --restore
```

`release-stamp` saves `.backup` copies while stamping. The release coordinator uses it around preparation and documentation builds, while each native build job stamps its own throwaway checkout. The committed placeholders remain unchanged. Mono does not publish to npm, Cargo, Maven, PyPI, or Docker; those operations remain ordinary tasks or CI workflow steps.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | The command succeeded. |
| `1` | The request was understood but the project, task, changelog, or release check failed. |
| `2` | The command line was invalid. |
| `3` | Mono or its environment could not carry the command out. |

## Development

Run the repository's complete CI pipeline with:

```bash
cargo fmt --check
cargo test --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo package --locked
mono check
```

Or use the checked-in pipeline to run the same dependency-ordered workflow:

```bash
mono run ci --ui stream
```

On Windows, run that pipeline through an installed Mono. The `test` task
re-links `target/debug/mono`, which is the very binary that `cargo run -- ci`
executes, and Windows will not delete a running executable:

```bash
cargo install --locked --path . --profile dev
mono run ci --ui stream
```

`--profile dev` reuses the artifacts the tasks build, so the install adds no
compilation of its own. The `install.sh`/`install.ps1` release binaries work the
same way and need no Rust toolchain.
