# monorelease

`monorelease` is a manifest-driven monorepo orchestrator. A repository has one root `monorepo.toml`; each application or package adds its own `monorepo.toml`. The CLI discovers those manifests, validates a task dependency graph, and runs arbitrary commands in dependency order.

`monorelease` does not know or care whether a package uses Rust, Node, Go, Make, Docker, a shell script, or a custom framework. Commands are the package's responsibility.

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

The manifest schema is intentionally fixed and has no `version` header. Unknown fields are rejected so typos fail during `doctor` or `ci`. If the schema changes in the future, the tool will provide an explicit migration rather than accepting multiple implicit formats.

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

Task commands are structured executable/argument arrays rather than shell strings. The runner passes arguments without shell interpolation and inherits the command's standard IO.

Optional task execution context:

```toml
[tasks.generate]
command = ["make", "docs"]
cwd = "site"
env = { MODE = "check" }
```

`cwd` is relative to the package directory and cannot escape it. Environment values are applied only to the child process.

## Discovery and ordering

`workspace.members` is evaluated relative to the root manifest. Literal directories, `*` path segments, and `**` recursive path segments are supported. A matching directory must contain a package `monorepo.toml`.

The planner creates a DAG of `package:task` nodes. It validates missing packages, missing tasks, duplicate package names, invalid references, cycles, and commands before execution. Tasks execute dependency-first using a deterministic topological order. Independent tasks can run concurrently with `--jobs N`; task output is emitted in scheduler order after each command completes.

Selecting a package with `--package` selects that package's pipeline roots and automatically includes their transitive task dependencies. Unrelated packages are excluded.
