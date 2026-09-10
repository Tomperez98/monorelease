# Echo demo

This is a runnable `monorelease` workspace that uses only `echo` commands. It demonstrates package discovery, local and cross-package dependencies, pipelines, package selection, task working directories, child environments, timeouts, resource groups, dry runs, and parallel scheduling.

Run these commands from the repository root:

```bash
# Validate every manifest and task reference.
cargo run -- check --dir examples/echo

# Inspect the resolved dependency-first plan without running commands.
cargo run -- --dir examples/echo task build --dry-run

# Inspect the dependency graph.
cargo run -- graph --dir examples/echo

# Run the default build/test pipeline with two workers.
cargo run -- run --dir examples/echo --jobs 2

# Run the release pipeline.
cargo run -- run release --dir examples/echo --jobs 2

# Select app and include its transitive shared dependency.
cargo run -- task build --dir examples/echo --package app --jobs 2
```

## Workspace layout

```text
examples/echo/
├── monorepo.toml
└── packages/
    ├── app/
    │   └── monorepo.toml
    ├── docs/
    │   ├── monorepo.toml
    │   └── site/
    └── shared/
        └── monorepo.toml
```

## What to look for

- `app:build` depends on `shared:build`.
- `app:test` depends on both its local `build` and `shared:test`.
- `docs:build` runs with `cwd = "site"` and `MODE=check` in its child environment.
- `shared:test`, `app:test`, and `docs:test` share the `checks` resource group and cannot overlap.
- Every task has a 30-second timeout for demonstration purposes.
- `shared:build` and `docs:build` are independent and can run concurrently with `--jobs 2`.
- The `release` pipeline resolves the `package` task for every package.

Because the commands are structured argv arrays, these tasks execute `echo` directly; no shell is involved.
