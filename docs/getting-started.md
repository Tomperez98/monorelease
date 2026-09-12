# Get started

Install Mono, add a root `mono.toml`, validate the graph, and run your default pipeline.

## Install

### Prebuilt binary

Download the archive for your platform from the [GitHub releases page](https://github.com/Tomperez98/mono/releases), extract `mono`, and put it on your `PATH`.

| Platform | Archive suffix |
| --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-gnu.tar.gz` |
| macOS arm64 | `aarch64-apple-darwin.tar.gz` |
| macOS x86_64 | `x86_64-apple-darwin.tar.gz` |
| Windows x86_64 | `x86_64-pc-windows-msvc.zip` |

Archives are named `mono-v<version>-<platform>.<format>`.

### Build from source

From a checkout, with Rust 1.88 or newer:

```bash
cargo install --path .
```

## Create a manifest

From your repository root:

```bash
mono init
```

Then edit the generated task to match your project. For example:

```toml
schema = 1

[project]
name = "my-project"
default_pipeline = "ci"

[pipelines.ci]
tasks = ["build", "test", "lint"]

[tasks.build]
command = ["cargo", "build", "--workspace"]

[tasks.test]
command = ["cargo", "test", "--workspace"]
depends_on = ["build"]

[tasks.lint]
command = ["cargo", "clippy", "--workspace", "--", "-D", "warnings"]
depends_on = ["test"]
```

The same structure works with commands from any language or toolchain.

## Validate and run

Check the manifest and inspect the resolved order before executing it:

```bash
mono check
mono plan
mono run
```

Run a named pipeline or selected tasks:

```bash
mono run release
mono task test
```

From a nested directory, Mono discovers the root manifest by walking upward from `--dir`:

```bash
mono --dir services/api task api-test
```
