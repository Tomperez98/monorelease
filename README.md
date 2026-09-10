# monorelease

`monorelease` is a manifest-driven project and monorepo orchestrator. A standalone project has one root `monorepo.toml`; a monorepo has one root manifest and one manifest per application or package. The CLI discovers the appropriate root, validates a task dependency graph, and runs arbitrary commands in dependency order.

`monorelease` does not know or care whether a package uses Rust, Node, Go, Make, Docker, a shell script, or a custom framework. Commands are the package's responsibility.

Runnable examples are available in [`examples/standalone`](examples/standalone/README.md), [`examples/echo`](examples/echo/README.md), [`examples/cache`](examples/cache/README.md), and [`examples/release-gate`](examples/release-gate/README.md).

## Quick start

A project can use `monorelease` without a monorepo layout. Create one root
`monorepo.toml` with a `[package]` section and local tasks:

```toml
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

Standalone mode does not require `apps/*`, `packages/*`, or child
`monorepo.toml` files. The root project is represented as one package, so its
planned tasks are `my-project:build` and `my-project:test`. Invoke commands
from the project root or a nested directory such as `src/`; task `cwd` values
remain relative to the project root.

```bash
monorelease doctor
monorelease ci
```

To scaffold a standalone project, provide its initial build command. Repeat
`--command` once per argument:

```bash
monorelease init --standalone \
  --command cargo --command build
```

For a monorepo, create a root workspace instead:

```bash
monorelease init .
```

The generated root manifest looks like this:

```toml
[workspace]
name = "monorepo"
members = [
    "apps/*",
    "packages/*",
]
default_pipeline = "ci"

[pipelines.ci]
tasks = ["build", "test"]
```

Add a package manifest at `packages/shared/monorepo.toml`:

```toml
[package]
name = "shared"

[tasks.build]
command = ["make", "build"]

[tasks.test]
command = ["make", "test"]
depends_on = ["build"]
```

Add an application manifest at `apps/web/monorepo.toml`:

```toml
[package]
name = "web"

[tasks.build]
command = ["npm", "run", "build"]
depends_on = ["shared:build"]

[tasks.test]
command = ["npm", "test"]
depends_on = ["build", "shared:test"]
```

No Rust source changes or central package registry are required when adding another package under a configured member pattern.

The manifest schema is intentionally fixed and has no `version` header. Unknown fields are rejected so typos fail during `doctor` or `ci`. `doctor` validates all discovered manifests, task references, command inputs, and task working directories before execution. If the schema changes in the future, the tool will provide an explicit migration rather than accepting multiple implicit formats.

## Commands

Validate the root manifest, discover packages, and check task references:

```bash
monorelease doctor
```

Run the workspace's default pipeline:

```bash
monorelease ci
```

Run a named pipeline:

```bash
monorelease run release
```

Preview the resolved commands without running them:

```bash
monorelease ci --dry-run
monorelease plan
```

Print the task dependency graph:

```bash
monorelease graph
```

Run only one package's root tasks and their transitive task dependencies:

```bash
monorelease ci --package web
```

Run specific task names instead of the pipeline's tasks. Repeat `--task` for multiple roots:

```bash
monorelease ci --task build
monorelease ci --task build --task test

# Run independent task branches concurrently
monorelease ci --jobs 4

# Skip cache reads and writes for one run
monorelease ci --no-cache

# Re-run cached tasks and refresh their successful entries
monorelease ci --force

# Remove all local cache entries
monorelease cache clean
```

## Manifest model

The root workspace declares package discovery and named pipelines:

```toml
[workspace]
name = "acme"
members = ["apps/*", "packages/*"]
default_pipeline = "ci"

[pipelines.ci]
tasks = ["build", "test"]

[pipelines.release]
tasks = ["workspace:release-verify"]

[tasks.release-verify]
command = ["./automation/release-verify"]
depends_on = ["web:package", "api:package"]
timeout_seconds = 1800
```

Workspace tasks are declared in the root manifest and run once from the workspace root. They are useful for checks that coordinate multiple packages, such as release validation. The reserved `workspace:` namespace is used when referring to them.

Each package declares only its identity and commands:

```toml
[package]
name = "documentation"

[tasks.generate]
command = ["./scripts/generate-docs"]

[tasks.publish]
command = ["./scripts/publish-docs"]
depends_on = ["generate"]
```

Task dependencies are local by default. Use `package:task` for a task in another package:

```toml
[tasks.build]
command = ["./scripts/build"]
depends_on = ["shared:build"]
```

Task commands are structured executable/argument arrays rather than shell strings. The runner passes arguments without shell interpolation and captures stdout/stderr. During execution, monorelease announces each task when it starts, presents captured task output in deterministic plan order, reports a semantic status and duration, and prints a final summary.

A run uses one concise output model:

```text
▶ shared:build
[shared] build
shared:build: completed in 14ms
summary: 6 completed, 0 cached, 0 failed, 0 blocked across 3 package(s)
```

Cached, failed, timed-out, and blocked tasks are identified explicitly. CI runs use grouped sections while preserving the same task statuses. `plan` and `--dry-run` show declared input/output patterns, but task environment values are always redacted.

Optional task execution context:

```toml
[tasks.generate]
command = ["make", "docs"]
cwd = "site"
env = { MODE = "check" }
timeout_seconds = 600
resource_group = "documentation"
```

`cwd` must be an existing relative directory inside the package. The resolved path is checked after symlink resolution, so a symlink cannot escape the package. Environment values are applied only to the child process and are redacted in plan output. Command arrays are executed directly without a shell; shell syntax such as pipes or redirects is not interpreted. Tasks time out after ten minutes by default; set `timeout_seconds` to change the limit. Tasks sharing a `resource_group` never run concurrently. Output is captured without a configured size limit and presented after the task completes.

### Local task caching

Caching is opt-in because tasks may have external side effects. A cacheable task must declare the files that affect it and the files that can be restored:

```toml
[tasks.build]
command = ["cargo", "build", "--release"]
cache = true
inputs = ["src/**", "Cargo.toml", "Cargo.lock"]
outputs = ["target/release/my-binary"]
cache_env = ["RUSTFLAGS", "CARGO_BUILD_TARGET"]
```

Successful cache entries are stored under `.monorelease/cache`. The `.monorelease/.gitignore` keeps those entries out of Git. Cache keys include the task command/configuration, declared input contents, declared environment values, and dependency task keys. Use `cache_env = ["*"]` when a task depends on the complete inherited environment; this is more conservative and usually produces fewer cache hits. Cache hits restore declared output files and replay captured output. Tasks with `cache = false` always execute; failed or timed-out tasks are never cached.

Use `--force` to execute cacheable tasks and refresh their entries, or `--no-cache` to bypass caching for a run. `--dry-run` never reads or writes the cache. Use `monorelease cache clean` to remove all local cache entries. Remote caching is not included yet.

Output patterns must match regular files; symlink outputs and output patterns that match no files are rejected when the task is cached. Do not cache deployment, publishing, migration, time-dependent, network-dependent, or otherwise nondeterministic tasks. An undeclared input can produce a stale cache hit, while an overbroad input can cause unnecessary misses.

## Discovery and ordering

`workspace.members` is evaluated relative to the root manifest. Literal directories, `*` path segments, and `**` recursive path segments are supported. A matching directory must contain a package `monorepo.toml`.

The planner creates a DAG of `package:task` and `workspace:task` nodes. Workspace tasks run once from the root; package tasks run from their package directories. It validates missing packages, missing tasks, duplicate package names, invalid references, cycles, commands, environment values, working directories, timeouts, and resource groups before execution. Task working directories must exist and resolve inside their package, including after symlink resolution. Tasks execute dependency-first using a deterministic topological order. Independent tasks can run concurrently with `--jobs N`; newly unblocked tasks are scheduled immediately, up to the worker limit. Task starts are reported immediately, while captured task output is presented in deterministic plan order so logs and CI sections do not interleave. A task failure or timeout prevents new dependent work from being scheduled, and the final summary reports completed, cached, failed, and blocked tasks.

Selecting a package with `--package` selects that package's pipeline roots and automatically includes their transitive task dependencies. Unrelated packages are excluded. A pipeline root in the reserved `workspace:` namespace cannot be combined with `--package`; run the pipeline without package selection or select a package task explicitly.
