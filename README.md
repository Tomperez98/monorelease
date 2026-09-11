# mono

Run a project's commands in dependency order, from one root `mono.toml` — in any language.

`mono` does not detect languages, frameworks, package managers, or project conventions. A task is an argv array executed directly. The only project model is a root manifest containing a task graph.

```console
$ mono plan
project mono (/repo/mono)
would run fmt in /repo/mono: cargo fmt --check [timeout=30s]
would run check in /repo/mono: cargo check --workspace ... [timeout=120s]
```

## Install

```bash
cargo install --path .
```

Prebuilt binaries for Linux x86_64, macOS arm64, macOS x86_64, and Windows x86_64 are attached to each [GitHub release](https://github.com/Tomperez98/mono/releases).

## Quick start

Create one root manifest:

```bash
mono init
```

The generated file is valid immediately and can be edited into the commands your project needs:

```toml
schema = 1

[project]
name = "my-project"
default_pipeline = "ci"

[pipelines.ci]
tasks = ["build", "test"]

[tasks.build]
command = ["tool", "build"]

[tasks.test]
command = ["tool", "test"]
depends_on = ["build"]
```

Every task is root-scoped. Use `cwd` when a command belongs to a subdirectory:

```toml
[tasks.api-test]
cwd = "services/api"
command = ["go", "test", "./..."]
```

There are no package manifests, member globs, standalone mode, package selection, or language-specific task types. A repository with one directory and a repository with one hundred directories use the same model.

Commands can be run from nested directories. `mono` walks upward from `--dir` until it finds the project root manifest:

```bash
mono --dir services/api task api-test
```

## Commands

| Command | What it does |
| --- | --- |
| `mono` | Run the default pipeline. |
| `mono run [PIPELINE]` | Run a named pipeline. `ci` is an alias. |
| `mono task TASK...` | Run one or more tasks and dependencies. |
| `mono check` | Validate the root manifest and complete task graph. |
| `mono list` | List pipelines and tasks. |
| `mono plan [PIPELINE]` | Print the dependency-first execution plan. |
| `mono graph [PIPELINE]` | Print task dependency edges. |
| `mono cache clean` | Delete local cache entries. |
| `mono changelog ...` | Apply Mono's documented changelog conventions. |
| `mono release ...` | Create or verify provider-neutral artifact metadata. |

Commands accept `--output text|json`. Execution commands also accept
`--ui auto|tui|stream`:

- `auto` (the default) uses the interactive task UI on a terminal and
  task-prefixed streaming output in pipes and CI.
- `tui` explicitly requests the interactive task list and per-task log view;
  it falls back to streaming output when no interactive terminal is available.
- `stream` writes task-prefixed lines as they arrive, so concurrent output
  remains attributable to its task.

`json` is a versioned, newline-delimited machine contract and never starts the
TUI. Human stream output buffers partial lines until a newline or task
completion, then prefixes each line with its task id. `run` and `task`
additionally emit execution lifecycle events (`run_started`, `task_started`,
`task_output`, `task_finished`, and `run_finished`). `plan`, `graph`, `list`,
and `check` return one JSON document with `schema = 1` and a command-specific
`kind`. Failures in JSON mode are emitted as
`{"schema":1,"kind":"error",...}` and retain the same process exit code as
text mode. Environment values are never included in plan or list output; only
configured variable names are reported.

`run` and `task` support:

| Flag | Effect |
| --- | --- |
| `--jobs N` | Maximum independent tasks to execute concurrently. |
| `--dry-run` | Resolve and print the plan without running commands. |
| `--no-cache` | Skip cache reads and writes. |
| `--force` | Ignore cache hits and refresh successful entries. |
| `--output text\|json` | Select the human or machine output contract. |
| `--ui auto\|tui\|stream` | Select the execution presentation for human output. |

## Manifest model

The manifest schema remains `1`; the root-only model is a deliberate breaking change while Mono is pre-1.0.

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
resource_group = "checks"
timeout_seconds = 900
retries = 1
retry_backoff_seconds = 5

[tasks.release-verify]
command = ["./automation/release-verify"]
depends_on = ["test"]
cwd = "automation"

[tasks.release-cleanup]
command = ["./automation/release-cleanup"]
cwd = "automation"
```

Task fields:

| Field | Default | Meaning |
| --- | --- | --- |
| `command` | required | Executable and arguments as an array. No shell is inserted. |
| `depends_on` | `[]` | Global task IDs that must complete first. |
| `cwd` | project root | Existing directory inside the project root. |
| `env` | `{}` | Extra child-process environment. |
| `stdin` | `null` | Use `inherit` to pass the parent process's standard input through. |
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

`outputs` are cache outputs. They are not release artifacts. Release files are intentionally handled by the separate `mono release manifest` and `mono release verify` commands, which inventory an explicit directory and generate `BUILD-METADATA.json` and `SHA256SUMS`.

## Execution behavior

- Plans are validated before any task starts: unknown tasks, cycles, invalid paths, bad matrix references, and invalid task configuration fail early.
- Independent tasks overlap up to `--jobs`; resource groups add explicit serialization only where required.
- A normal task failure stops new normal work. In-flight tasks finish, then pipeline finalizers run as cleanup work.
- Finalizers are not cacheable because a cache hit must not skip cleanup side effects.
- Timeouts terminate the complete child process tree using Unix process groups or Windows Job Objects.
- Human execution uses the TUI on interactive terminals and task-prefixed stream output elsewhere. Partial output lines are flushed with their task prefix when a task completes. JSON output is newline-delimited and preserves non-UTF-8 task output as byte arrays.
- Ctrl-C cancels running normal tasks, terminates their process trees, and still permits finalizer tasks to run.
- Direct execution never changes Mono's process-global working directory.

## Release conventions

Mono is intentionally opinionated about release process. Every Mono project gets the same TigerBeetle-inspired changelog and release pattern:

- `CHANGELOG.md` is Markdown.
- The newest heading is `## (unreleased)` or `## X.Y.Z`.
- Version entries contain `Released: YYYY-MM-DD`.
- Release tags are `vX.Y.Z`.
- The tag must resolve to the checked-out source commit.
- The matching changelog entry becomes `RELEASE_NOTES.md`.
- Release directories contain an explicit, verified artifact inventory and SHA-256 metadata.
- Publishing remains an ordinary project task or CI step; Mono does not know registries or package managers.

These are release conventions, not language or framework conventions. See [`docs/release-conventions.md`](docs/release-conventions.md).

Mono does not publish to npm, Cargo, Maven, PyPI, Docker, or any other registry. Those operations remain ordinary tasks or CI workflow steps, preserving the language-agnostic execution kernel.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | The command succeeded. |
| `1` | The request was understood but the project, task, changelog, or release check failed. |
| `2` | The command line was invalid. |
| `3` | Mono or its environment could not carry the command out. |

## Development

```bash
cargo fmt --check
cargo test --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo package --locked
mono check
```
