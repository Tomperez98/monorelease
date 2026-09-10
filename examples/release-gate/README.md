# Release gate example

This example demonstrates how a language-agnostic workspace task can coordinate a release without putting release knowledge into `monorelease` itself.

Every command is an `echo`, so the example runs without Node, Rust, Docker, or any other toolchain.

## Run it

From the repository root:

```bash
# Validate every package and workspace task.
cargo run -- check --dir examples/release-gate

# Inspect the normal CI graph.
cargo run -- graph --dir examples/release-gate

# Run build and test tasks with bounded parallelism.
cargo run -- run --dir examples/release-gate --jobs 3

# Inspect the release graph without executing it.
cargo run -- plan release --dir examples/release-gate

# Run the release pipeline.
cargo run -- run release --dir examples/release-gate --jobs 3
```

## Workspace layout

```text
examples/release-gate/
├── automation/
├── monorepo.toml
└── packages/
    ├── api/
    │   └── monorepo.toml
    ├── docs/
    │   ├── monorepo.toml
    │   └── site/
    └── web/
        └── monorepo.toml
```

## What this demonstrates

### Package-owned work

Each package owns its commands and its lifecycle:

```text
build -> test -> package
```

The packages can use completely different languages or frameworks in a real repository. `monorelease` only sees executable argument arrays.

### Cross-package dependencies

`web` depends on `api`:

```toml
[tasks.build]
depends_on = ["api:build"]

[tasks.test]
depends_on = ["build", "api:test"]
```

The planner includes the required API work automatically and rejects cycles or missing tasks before execution.

### Parallel execution

The docs build is independent of the API build and can run concurrently when `--jobs 3` is used. Test tasks share the `checks` resource group, so they cannot overlap even when workers are available.

### Workspace-scoped release coordination

The root manifest defines:

```toml
[pipelines.release]
tasks = ["workspace:release-summary"]

[tasks.release-verify]
depends_on = ["api:package", "web:package", "docs:package"]

[tasks.release-summary]
depends_on = ["release-verify"]
```

The resulting release graph is:

```text
api:package  ─┐
web:package  ─┼─> workspace:release-verify -> workspace:release-summary
 docs:package ─┘
```

The package tasks run according to their own dependency graphs. `workspace:release-verify` then runs **once**, from `automation/`, after every package is ready. This is where a real repository could perform generic-to-the-orchestrator release checks such as artifact comparison, publishing validation, or cross-package version checks.

The orchestrator does not know what those checks mean. It only schedules the command and reports its result.
