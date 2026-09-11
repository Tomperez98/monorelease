# Cache demo

This example uses one root `mono.toml`. The build task runs in `packages/app`, fingerprints `packages/app/input.txt`, and restores `packages/app/dist/artifact.txt` from the local cache.

```bash
cargo run -- --dir examples/cache run
cargo run -- --dir examples/cache run
cargo run -- --dir examples/cache run --force
cargo run -- --dir examples/cache cache clean
```

The directory layout is ordinary project data; only the root manifest participates in orchestration.
