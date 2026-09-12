---
title: Mono
hide:
  - navigation
---

# Run project pipelines in dependency order

Mono runs build, test, lint, and release commands from one root `mono.toml`—in any language.

<script src="https://asciinema.org/a/5JVHsZ7ruT557Qpu.js" id="asciicast-5JVHsZ7ruT557Qpu" async="true" data-cols="161" data-rows="38"></script>

```toml
[project]
name = "mono"
default_pipeline = "ci"

[pipelines.ci]
tasks = ["changelog-validate", "fmt", "check", "test", "clippy"]


[tasks.changelog-validate]
command = ["cargo", "run", "--locked", "--quiet", "--", "changelog", "validate"]
timeout_seconds = 30

[tasks.fmt]
command = ["cargo", "fmt", "--check"]
timeout_seconds = 30

[tasks.check]
command = ["cargo", "check", "--workspace", "--all-targets", "--all-features"]
depends_on = ["fmt"]
timeout_seconds = 120

[tasks.test]
command = ["cargo", "test", "--workspace", "--all-targets", "--all-features"]
depends_on = ["check"]
timeout_seconds = 120

[tasks.clippy]
command = ["cargo", "clippy", "--workspace", "--all-targets", "--all-features", "--", "-D", "warnings"]
depends_on = ["test"]
timeout_seconds = 120

```

[Mono running this repository's `ci` pipeline in the interactive task view](https://asciinema.org/a/5JVHsZ7ruT557Qpu) — independent tasks report as they finish, and the run ends with `summary: 5 completed, 0 failed`.

```console
$ mono plan
project mono (/repo/mono)
would run fmt in /repo/mono: cargo fmt --check [timeout=30s]
would run check in /repo/mono: cargo check --workspace ... [timeout=120s]
```

Mono is a task orchestrator, not a package manager or workspace detector. You describe commands and their dependencies; Mono validates the graph, runs independent tasks concurrently, and gives local development and CI the same execution contract.

<div class="grid cards" markdown>

-   :material-rocket-launch: **Get started**

    Install Mono and create your first `mono.toml`.

    [:octicons-arrow-right-24: Get started](getting-started.md)

-   :material-file-tree: **Define a task graph**

    Learn the manifest schema, task fields, dependencies, and caching.

    [:octicons-arrow-right-24: Manifest reference](manifest.md)

-   :material-console: **Automate your pipeline**

    Run tasks locally, in CI, or from a nested directory.

    [:octicons-arrow-right-24: CLI reference](commands.md)

</div>

## Why Mono?

- **One root manifest:** no package discovery, member globs, or language-specific task types.
- **Explicit commands:** commands are argv arrays; Mono does not insert a shell.
- **Dependency-aware execution:** independent tasks run concurrently, while dependencies and resource groups stay ordered.
- **CI-friendly output:** use task-prefixed streams for humans or versioned JSON events for automation.
