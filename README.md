# monorelease

`monorelease` is a manifest-driven monorepo orchestrator. A repository has one root `monorepo.toml`; each application or package adds its own `monorepo.toml`. The CLI discovers those manifests, validates a task dependency graph, and runs arbitrary commands in dependency order.

`monorelease` does not know or care whether a package uses Rust, Node, Go, Make, Docker, a shell script, or a custom framework. Commands are the package's responsibility.

A runnable workspace using only `echo` commands is available in [`examples/echo`](examples/echo/README.md).

## Quick start

Create a root workspace:

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
tasks = ["build", "package", "publish"]
```

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

Task commands are structured executable/argument arrays rather than shell strings. The runner passes arguments without shell interpolation, captures stdout/stderr, and presents task output after completion.

Optional task execution context:

```toml
[tasks.generate]
command = ["make", "docs"]
cwd = "site"
env = { MODE = "check" }
timeout_seconds = 600
resource_group = "documentation"
```

`cwd` must be an existing relative directory inside the package. The resolved path is checked after symlink resolution, so a symlink cannot escape the package. Environment values are applied only to the child process. Command arrays are executed directly without a shell; shell syntax such as pipes or redirects is not interpreted. Tasks time out after ten minutes by default; set `timeout_seconds` to change the limit. Tasks sharing a `resource_group` never run concurrently. Output is captured without a configured size limit and presented after the task completes.

## Discovery and ordering

`workspace.members` is evaluated relative to the root manifest. Literal directories, `*` path segments, and `**` recursive path segments are supported. A matching directory must contain a package `monorepo.toml`.

The planner creates a DAG of `package:task` nodes. It validates missing packages, missing tasks, duplicate package names, invalid references, cycles, commands, environment values, working directories, timeouts, and resource groups before execution. Task working directories must exist and resolve inside their package, including after symlink resolution. Tasks execute dependency-first using a deterministic topological order. Independent tasks can run concurrently with `--jobs N`; newly unblocked tasks are scheduled immediately, up to the worker limit. Parallel task output is captured and presented in deterministic plan order so logs and CI sections do not interleave. A task failure or timeout prevents new dependent work from being scheduled.

Selecting a package with `--package` selects that package's pipeline roots and automatically includes their transitive task dependencies. Unrelated packages are excluded.
