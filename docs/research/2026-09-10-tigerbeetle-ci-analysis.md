# TigerBeetle CI and `monorelease`: research brief

**Date:** 2026-09-10  
**Scope:** TigerBeetle first-party repository/source only, compared with the checked-out Rust project.

## Evidence and validation note

The authoritative TigerBeetle source is the `main` branch file requested by the task: [`src/scripts/ci.zig`](https://github.com/tigerbeetle/tigerbeetle/blob/main/src/scripts/ci.zig). This report cites stable symbol/section names as well as the exact source URL. GitHub's line numbers are branch-relative and should be pinned to a commit before using this as a normative compatibility specification. The available research runtime did not provide a web-fetch/source-check provider, so I could inspect the local Rust checkout but could not independently fetch the upstream file in this run. Consequently, TigerBeetle-specific implementation claims below are marked **to verify against upstream** rather than represented as fetched direct evidence. No source code was modified.

## Summary

TigerBeetle's CI script is a repository-specific, explicit command runner: it owns the CI matrix, invokes tools as child processes, and turns a failed child into a failed CI run. It is not a general manifest/DAG scheduler. Its strongest ideas for `monorelease` are an explicit, reviewable CI command list, strict argument/environment construction, consistent command boundaries, and early validation. Its process-control details should not be copied wholesale: `monorelease` already has a stronger package/task dependency DAG and bounded parallel scheduler, while TigerBeetle's repository-wide CI assumptions are not appropriate as defaults for arbitrary languages and packages.

## Findings

1. **Command execution — explicit child processes, not shell composition.** The TigerBeetle implementation is concentrated in the `ci.zig` command-runner helpers and its command table/entry point: [`ci.zig`](https://github.com/tigerbeetle/tigerbeetle/blob/main/src/scripts/ci.zig) (sections/functions that construct and run `std.process.Child`). The model is to create a child for each named check and wait for it, rather than hand a large shell string to a shell. **Support:** source interpretation; upstream fetch validation unavailable in this run. **Confidence:** medium.  
   `monorelease` makes this property declarative and language-neutral: task `command` is an executable-plus-argv array and `Runner::run` uses `std::process::Command` directly ([`src/runner.rs`](https://github.com/tomasperez/monorelease/blob/main/src/runner.rs), `Runner::run`; local lines 31–75; [`README.md`](https://github.com/tomasperez/monorelease/blob/main/README.md), “Manifest model”). This is a good existing design; do not add implicit shell parsing.

2. **Argument handling — argv is data and CI arguments are intentionally constrained.** In TigerBeetle, the `ci.zig` entry point parses the script's small set of CI modes/options and constructs argument slices for each tool (the command-dispatch/argument-building sections). **Support:** source interpretation; verify exact option names and line ranges upstream. **Confidence:** low-to-medium.  
   In this project, Clap owns top-level parsing in [`src/main.rs`](https://github.com/tomasperez/monorelease/blob/main/src/main.rs), `Cli`, `Commands`, and `run` (local lines 15–144). Package commands are arrays, so spaces, quotes, pipes, and redirects cannot be accidentally reinterpreted. This is more general than a script-specific parser. Recommended addition: expose a structured “resolved command” representation in diagnostics, but do not accept shell strings as an alternative schema.

3. **Environment and working directory — explicit process context is safer than global mutation.** TigerBeetle's CI runner sets the environment/tool context needed by its checks in the child-process setup and uses repository-relative paths (the child setup and command-dispatch sections of [`ci.zig`](https://github.com/tigerbeetle/tigerbeetle/blob/main/src/scripts/ci.zig)). Exact variable names and inheritance behavior must be checked against the pinned upstream revision. **Support:** source interpretation. **Confidence:** low-to-medium.  
   `monorelease` applies task `env` only to the child and uses `current_dir`, without changing the runner's process-global cwd ([`src/runner.rs`](https://github.com/tomasperez/monorelease/blob/main/src/runner.rs), `Runner::run`, local lines 49–64). Workspace validation also requires `cwd` to remain inside the package, including symlink resolution ([`README.md`](https://github.com/tomasperez/monorelease/blob/main/README.md), “Manifest model”; [`src/workspace.rs`](https://github.com/tomasperez/monorelease/blob/main/src/workspace.rs), `PlannedTask`/path validation). Preserve this isolation. Recommended addition: document which inherited variables are intentionally passed through and provide an opt-in scrubbed environment for hermetic CI.

4. **Logging/output — task identity and captured output are essential.** TigerBeetle's script labels individual CI stages/checks and reports child failure through the CI process (command execution/reporting portions of [`ci.zig`](https://github.com/tigerbeetle/tigerbeetle/blob/main/src/scripts/ci.zig)); exact formatting needs upstream verification. **Support:** source interpretation. **Confidence:** low-to-medium.  
   The Rust tool captures stdout/stderr per task and presents it afterward in deterministic plan order ([`src/runner.rs`](https://github.com/tomasperez/monorelease/blob/main/src/runner.rs), `CapturedOutput`/`Runner::run`, local lines 12–75; [`src/scheduler.rs`](https://github.com/tomasperez/monorelease/blob/main/src/scheduler.rs), final presentation loop; [`src/output.rs`](https://github.com/tomasperez/monorelease/blob/main/src/output.rs), `OutputSink`). It emits GitHub Actions groups when `CI` or `GITHUB_ACTIONS` is present. This is a sound improvement over interleaved live output. Recommended additions: include command, cwd, exit status, and elapsed time in a machine-readable failure record; optionally add a `--stream` mode for interactive debugging. Do not replace deterministic buffered output by default.

5. **Concurrency — TigerBeetle is a sequential CI checklist unless the upstream command table explicitly parallelizes checks.** The `ci.zig` architecture is a script-level sequence of checks; no evidence was fetched in this run that it exposes a general worker-pool/DAG interface. **Support:** source interpretation; exact concurrency claim requires fetch. **Confidence:** low.  
   `monorelease` has a materially different model: `scheduler::execute_plan` tracks dependency counts, starts newly-ready tasks up to `jobs`, and stops admitting work after a failure ([`src/scheduler.rs`](https://github.com/tomasperez/monorelease/blob/main/src/scheduler.rs), `execute_plan`; local lines 13–119). Keep this DAG/bounded-worker design. If adopting an upstream idea, add named serial barriers or resource-group limits—not a flat serial mode.

6. **Failure and exit semantics — fail CI on any failed check, preserve diagnostic context.** TigerBeetle's CI entry point returns failure from a failed child and the top-level CI process therefore exits unsuccessfully (failure handling in [`ci.zig`](https://github.com/tigerbeetle/tigerbeetle/blob/main/src/scripts/ci.zig)). Verify whether it stops at first failure or runs all independent checks before returning; this is decision-relevant and was not source-checked here. **Support:** source interpretation. **Confidence:** medium for non-zero failure, low for exact stop policy.  
   `monorelease` maps all expected command errors to one non-zero `ExitCode::FAILURE` in [`src/main.rs`](https://github.com/tomasperez/monorelease/blob/main/src/main.rs), `main` (local lines 111–121). The scheduler stops launching new tasks after a failure, lets already-active tasks finish, then presents completed results in plan order ([`src/scheduler.rs`](https://github.com/tomasperez/monorelease/blob/main/src/scheduler.rs), `stopping` and final result loop). This is a reasonable default. Recommended additions: preserve signal termination separately from an ordinary numeric exit code, and report all failures from already-running tasks rather than only the first error.

7. **Platform handling — repository-specific checks must be gated by target/tool availability.** TigerBeetle's CI source and first-party build documentation are the relevant authorities: [`ci.zig`](https://github.com/tigerbeetle/tigerbeetle/blob/main/src/scripts/ci.zig) and [`BUILD.md`](https://github.com/tigerbeetle/tigerbeetle/blob/main/BUILD.md) (platform/toolchain sections). The CI script's platform branches/command selection should be treated as TigerBeetle policy, not as a universal runner rule. **Support:** source interpretation; not fetched/validated in this run. **Confidence:** low.  
   `monorelease` deliberately delegates platform behavior to package commands and performs no OS-specific command substitution. Keep that language-agnostic boundary. Recommended addition: manifest-level declarative predicates such as `platforms`/`required_env`, validated before execution, while retaining package-owned commands for platform-specific behavior.

8. **Reproducibility — explicit tools and repository checks help, but the runner cannot guarantee toolchain reproducibility.** TigerBeetle's first-party build/toolchain docs ([`BUILD.md`](https://github.com/tigerbeetle/tigerbeetle/blob/main/BUILD.md)) and CI script are the sources to pin when documenting required Zig versions, generated artifacts, and dependency/cache assumptions. **Support:** source interpretation; upstream content not fetched. **Confidence:** low-to-medium.  
   `monorelease` validates manifests, task references, commands, cwd paths, and symlink containment before running; it offers `plan`/`--dry-run` ([`README.md`](https://github.com/tomasperez/monorelease/blob/main/README.md), “Commands” and “Discovery and ordering”; [`src/commands/ci.rs`](https://github.com/tomasperez/monorelease/blob/main/src/commands/ci.rs), `run_pipeline_with_jobs`/`format_plan`). Recommended additions: a lockfile-like resolved plan containing command, cwd, env keys (redacted values), and tool-version policy; a strict reproducible mode that rejects undeclared ambient inputs.

## What should and should not be adopted

### Adopt

- **Named, reviewable CI checks** and a stable task label in every diagnostic. This is the useful script-level discipline from TigerBeetle, expressed in this project's manifest model.
- **Strict argv execution** and no implicit shell. Already implemented; add tests/documentation rather than changing it.
- **Preflight validation** before spawning any task: executable/argument validity, cwd containment, required tools, and platform predicates.
- **Reproducible plan output**: add a versioned, machine-readable plan alongside the current human-readable `plan` output.
- **Explicit CI reporting adapters** (GitHub Actions groups are already supported; consider generic CI annotations without coupling the core runner to one provider).
- **Failure records for every task that ran**, including signal-vs-exit-code, command, cwd, and captured output.

### Do not adopt

- A flat hard-coded TigerBeetle command list as the monorepo's orchestration model: it would discard package ownership and dependency edges.
- Global cwd/environment mutation or shell-string command assembly.
- Unbounded parallelism or live interleaving of child output.
- TigerBeetle-specific compiler/platform assumptions in a language-agnostic tool.
- A policy that assumes every CI check should run serially; retain `--jobs` and DAG scheduling.

## Contradictions

No direct source contradiction was resolved because the upstream file could not be fetched by the available runtime. The principal uncertainty is whether the current TigerBeetle script parallelizes any subset of checks or has nuanced stop/continue behavior. The Rust checkout's README describes deterministic output; the implementation confirms buffered per-task capture and plan-order presentation.

## Missing evidence

- Pinned TigerBeetle commit, exact line ranges, and exact command names/options in `src/scripts/ci.zig`.
- Whether TigerBeetle uses inherited versus scrubbed environment, and its exact cwd/stdio configuration.
- Whether independent checks continue after one failure; whether signals are preserved distinctly.
- Exact platform branches and reproducibility/toolchain checks in the current first-party CI/build docs.
- A source-check result against upstream content. These gaps matter before copying any implementation detail; the recommendations above intentionally rely only on generic process-runner properties and the locally inspected Rust code.

## Sources

### Kept

- [TigerBeetle `src/scripts/ci.zig`](https://github.com/tigerbeetle/tigerbeetle/blob/main/src/scripts/ci.zig) — requested authoritative CI implementation.
- [TigerBeetle `BUILD.md`](https://github.com/tigerbeetle/tigerbeetle/blob/main/BUILD.md) — first-party build/platform/toolchain policy.
- [monorelease README](https://github.com/tomasperez/monorelease/blob/main/README.md) — checked-out project's public orchestration contract.
- [monorelease `src/main.rs`](https://github.com/tomasperez/monorelease/blob/main/src/main.rs), [`runner.rs`](https://github.com/tomasperez/monorelease/blob/main/src/runner.rs), [`scheduler.rs`](https://github.com/tomasperez/monorelease/blob/main/src/scheduler.rs), [`output.rs`](https://github.com/tomasperez/monorelease/blob/main/src/output.rs), [`commands/ci.rs`](https://github.com/tomasperez/monorelease/blob/main/src/commands/ci.rs) — direct implementation evidence.

### Rejected/deprioritized

- Search snippets, third-party blog posts, and non-TigerBeetle CI examples — outside the requested first-party scope and not suitable for implementation claims.

## Next steps

1. Fetch and pin the TigerBeetle commit, then replace the provisional upstream claims with exact GitHub `#Lx-Ly` anchors and run source checks on command execution, environment, concurrency, and failure semantics.
2. Add a `--format json`/resolved-plan artifact and structured task-failure records to `monorelease`.
3. Add preflight checks for declared tools/platform predicates and an opt-in hermetic environment mode.
