# Manifest reference

A project has one root `mono.toml`. The manifest contains named pipelines and a global task graph.

## Minimal manifest

```toml
schema = 1

[project]
name = "acme"
default_pipeline = "ci"

[pipelines.ci]
tasks = ["fmt", "test"]

[tasks.fmt]
command = ["formatter", "check"]

[tasks.test]
command = ["test-runner"]
depends_on = ["fmt"]
```

A pipeline selects the tasks to run. `depends_on` adds prerequisites to the global graph, so a task can be selected directly and still bring its dependencies with it.

## Task fields

| Field | Default | Meaning |
| --- | --- | --- |
| `command` | required | Executable and arguments as an array. No shell is inserted. |
| `depends_on` | `[]` | Global task IDs that must complete first. |
| `cwd` | project root | Existing directory inside the project root. |
| `env` | `{}` | Extra child-process environment. |
| `stdin` | `null` | Use `inherit` to pass the parent's standard input through. |
| `cache` | `false` | Allow successful results to be reused. |
| `inputs` | `[]` | Relative patterns included in the cache fingerprint. |
| `outputs` | `[]` | Relative files copied into and restored from the cache. |
| `cache_env` | `[]` | Environment variables included in the cache key; `*` means all. |
| `timeout_seconds` | `600` | Wall-clock limit for one invocation. |
| `max_output_bytes` | `16777216` | Per-stream capture limit. |
| `resource_group` | unset | Tasks sharing a group never overlap. |
| `retries` | `0` | Additional attempts after a failed invocation. |
| `retry_backoff_seconds` | `0` | Delay between attempts. |
| `matrix.<name>` | unset | Opaque dimensions expanded into task instances. |

`outputs` are cache outputs, not release artifacts. Release files are handled by `mono release manifest` and `mono release verify`.

## Working directories

Every task is root-scoped. Set `cwd` when a command belongs to a subdirectory:

```toml
[tasks.api-test]
cwd = "services/api"
command = ["go", "test", "./..."]
```

The directory must exist inside the project root. Mono never changes its own process-global working directory.

## Shell commands

Mono executes `command` directly as an argv array. It does not interpolate shell syntax, pipes, redirects, or environment expansion. If a task needs a shell, invoke one explicitly and keep the command platform-specific:

```toml
[tasks.generated]
command = ["sh", "-c", "generate-inputs | transform > output.txt"]
```
