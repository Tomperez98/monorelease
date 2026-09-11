# Root project demo

This example demonstrates that a small project and a large project use the same root-only `mono.toml` model. Its commands run from `src`, but there is no standalone mode or child manifest.

```bash
cargo run -- check --dir examples/standalone
cargo run -- run --dir examples/standalone
```
