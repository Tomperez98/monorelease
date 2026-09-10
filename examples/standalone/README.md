# Standalone demo

This is a single-project `monorelease` configuration. It has one root
`monorepo.toml`, one package, and no `apps/`, `packages/`, or child manifests.
The tasks use `src/` as their working directory to demonstrate that commands
can be invoked from a nested project directory.

Run these commands from the repository root:

```bash
# Validate the standalone manifest.
cargo run -- check --dir examples/standalone

# Inspect the dependency-first plan.
cargo run -- plan --dir examples/standalone

# Run the standalone pipeline.
cargo run -- --dir examples/standalone

# Root discovery also works from inside src/.
(cd examples/standalone/src && cargo run -- plan)
```

Expected task nodes are:

```text
standalone-demo:build
standalone-demo:test
```

`standalone-demo:test` depends on `standalone-demo:build`, so the build task is
planned and executed first.
