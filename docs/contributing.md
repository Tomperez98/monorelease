# Contributing

Run the repository's checks before opening a change:

```bash
cargo fmt --check
cargo test --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo package --locked
mono check
```

Or run the checked-in dependency-ordered pipeline:

```bash
mono run ci --ui stream
```

The CI manifest is at the repository root in `mono.toml`. Mono itself is intentionally language-agnostic: changes to task execution should preserve direct argv execution, dependency validation, process-tree cleanup, and the text/JSON output contracts.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | The command succeeded. |
| `1` | The request was understood but the project, task, changelog, or release check failed. |
| `2` | The command line was invalid. |
| `3` | Mono or its environment could not carry the command out. |
