# Release gate demo

This example shows a root-only release pipeline. The `api`, `web`, and `docs` directories are ordinary command working directories. Their global task IDs are connected directly in the single root DAG.

```bash
cargo run -- check --dir examples/release-gate
cargo run -- --dir examples/release-gate plan release
cargo run -- run release --dir examples/release-gate --jobs 3
```

The final release summary depends on the release verification task, which depends on all three package-directory flows.
