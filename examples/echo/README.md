# Echo demo

This is a runnable root-only `mono` project using only `echo` commands. It demonstrates global task dependencies, task working directories, child environments, timeouts, resource groups, dry runs, pipelines, and parallel scheduling.

```bash
cargo run -- check --dir examples/echo
cargo run -- --dir examples/echo plan
cargo run -- graph --dir examples/echo
cargo run -- run --dir examples/echo --jobs 2
cargo run -- run release --dir examples/echo --jobs 2
```

The `packages/` directories are ordinary command working directories. They contain no Mono manifests. Tasks such as `app-build`, `shared-build`, and `docs-build` are global IDs with explicit `cwd` values.

Because commands are argv arrays, these tasks execute `echo` directly; no shell is involved.
