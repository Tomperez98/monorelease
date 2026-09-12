# CLI reference

Run `mono --help` or `mono help <command>` for the complete option list. Every screen ends with a runnable example.

## Commands

| Command | What it does |
| --- | --- |
| `mono` | Run the default pipeline. Accepts the global flags only; use `mono run` for the execution flags. |
| `mono run [PIPELINE]` | Run a named pipeline, or the default pipeline when `PIPELINE` is omitted. `ci` is the conventional CI pipeline name. |
| `mono task TASK...` | Run specific tasks and their dependencies, skipping the rest of the pipeline. Matrix tasks are addressed as `name[dimension=value]`. |
| `mono check` | Validate `mono.toml` and the complete task graph. Also available as `mono doctor`. |
| `mono list` | List this project's pipelines, tasks, and suggested commands. |
| `mono plan [PIPELINE]` | Print the dependency-first execution plan without running anything. |
| `mono graph [PIPELINE]` | Print task dependency edges. |
| `mono cache clean` | Delete local cache entries. |
| `mono changelog check` | Validate a changelog file against Mono's release conventions. |
| `mono changelog prepare [VERSION]` | Infer or create the next editable changelog entry. |
| `mono changelog release-notes [VERSION]` | Extract release notes from the newest entry. |
| `mono release ...` | Create or verify provider-neutral release metadata. |

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | The command succeeded. |
| `1` | The request was understood and refused on its merits: a red pipeline, an invalid manifest, a version or date the project disagrees with, a rejected changelog or release check. |
| `2` | The command line was malformed: an unknown flag, a missing value, or a value whose shape Mono can judge without reading the project (`--jobs 0`, `--date 2001-13-45`, a version that is neither `X.Y.Z` nor `unreleased`). |
| `3` | Mono or its environment could not carry the command out: an unreadable manifest, a missing `git`, a result that could not be written. |

A task that exits non-zero makes `mono` exit `1`, not `3`. Exit `3` means the command was never given a fair chance to run. A consumer that closes the pipe before a run finishes (`mono run ci | head -1`) is one of those cases: Mono can no longer deliver the output contract it was asked for, so it stops dispatching and exits `3`. A pipe closed after the run finished leaves the exit code alone — the work was done.

`mono changelog` and `mono release` are path-based commands that never load a manifest, so `--dir` selects the directory their relative paths are resolved against rather than a project root to walk up to. From a nested directory, pass the root explicitly: `mono --dir ../.. changelog check`. Every other command walks upward from `--dir` to the nearest `mono.toml`.

There is no `mono uninstall`: Mono keeps no state of its own, so removing the
binary is the whole uninstall. `rm ~/.local/bin/mono` for an `install.sh`
installation, `cargo uninstall mono` for a `cargo install`, and `Remove-Item` on
Windows; per-project caches are `mono cache clean`.

## Execution flags

`run` and `task` support:

| Flag | Effect |
| --- | --- |
| `--jobs N` | Maximum number of independent tasks to execute concurrently. |
| `--dry-run` | Resolve and print the plan without running commands. |
| `--no-cache` | Skip cache reads and writes. Conflicts with `--force`. |
| `--force` | Ignore cache hits and refresh successful entries. Conflicts with `--no-cache`. |
| `--output text\|json` | Select the human or machine output contract. |
| `--ui auto\|tui\|stream` | Select the human execution presentation for `run` and `task`; bare `mono` accepts it before the command. |

Bare `mono` runs the default pipeline but accepts the global flags only. Use `mono plan` to preview it, or `mono run` to run it with the execution flags.

`--ui auto` uses the interactive task UI on a terminal and task-prefixed streaming output in pipes and CI. `--ui tui` currently resolves the same way, so the full-screen view is used only when a terminal is attached. Use `--ui stream` for predictable task-prefixed lines anywhere.

`mono changelog prepare` creates the next patch entry by default. Pass `unreleased` to explicitly skip a release cycle, or pass a version for a major/minor release. The generated entry contains only its release metadata and harvested bullets; it does not add empty category placeholders. `--date` makes the entry date deterministic. `--from REF --to REF` adds editable bullets from first-parent merge commits without fetching or switching branches. If that range has no merge commits, preparation succeeds with a warning so you can add content manually. `--pull-request-url 'https://github.com/org/repo/pull/{number}'` is the only Mono CLI input for that URL; it turns recognized merge commits into links.

`mono changelog release-notes` uses the newest entry by default. Pass `VERSION` or `--release-tag` when you need to require a specific release; the value must match the newest entry. There is no environment fallback. It rejects entries containing only `Released:` without substantive release content. The old `scaffold`, `validate`, and `notes` spellings remain aliases.

`mono release manifest` and `mono release verify` write and read a release directory: `--dist <PATH>` (default `dist`, below the root).

## Explicit CLI configuration

Release identity is passed on the command line, never read from the environment. `VERSION`, `CHANGELOG_PR_URL`, `RELEASE_TAG`, `GITHUB_SHA`, `GITHUB_REPOSITORY`, `RELEASE_TAG_OBJECT`, and `GITHUB_RUN_URL` configure nothing in the Mono CLI. CI shells may still use these variables as runtime plumbing, but every value must be forwarded to the command as a flag:

```console
mono changelog prepare 1.2.0 \
  --pull-request-url 'https://github.com/org/repo/pull/{number}'

mono changelog release-notes 1.2.0

mono release source --tag v1.2.0 --commit "$commit"
mono release manifest --dist dist --tag v1.2.0 --commit "$commit"
```

`--tag`, `--commit`, `--repository`, `--tag-object`, and `--workflow-run` are explicit CLI inputs whenever they appear. Installer scripts keep their own documented environment variables (`MONO_VERSION`, `MONO_INSTALL_DIR`), which are consumed by the installer shell script itself, not by the Mono binary.

## Machine-readable output

Use `--output json` for a versioned, newline-delimited contract. It never starts the TUI.

Execution emits these lifecycle events:

- `run_started`
- `task_started`
- `task_output`
- `task_finished`
- `run_finished`

`plan`, `graph`, `list`, and `check` emit one JSON document with `schema = 1` and a command-specific `kind`. Environment values are never included in plan or list output; only configured variable names are reported.

## Execution behavior

- Mono validates the complete plan before starting work. Unknown tasks, cycles, invalid paths, bad matrix references, and invalid task configuration fail early.
- Independent tasks run concurrently up to `--jobs`; `resource_group` adds explicit serialization.
- A normal task failure stops new normal work. In-flight tasks finish, then pipeline finalizers run as cleanup work.
- Finalizers are not cacheable because a cache hit must not skip cleanup side effects.
- Timeouts terminate the complete child process tree using Unix process groups or Windows Job Objects.
- Ctrl-C cancels normal tasks, terminates their process trees, and still permits finalizers to run.
