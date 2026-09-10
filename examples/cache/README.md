# Cache demo

This workspace demonstrates local task caching, cache-hit output replay, cached output restoration, and cache invalidation when an input changes.

The example uses `sh`, so it is intended for macOS/Linux environments.

Run these commands from the repository root.

## First run: execute and store the result

```bash
cargo run --quiet -- doctor examples/cache
cargo run --quiet -- ci examples/cache 2>&1
```

The first run executes `app:build`, stores `dist/artifact.txt` in the local cache, and then runs `app:verify`:

```text
▶ app:build
[build] generated artifact from input: hello from the cache example
app:build: completed in 14ms
▶ app:verify
[verify] artifact: hello from the cache example
app:verify: completed in 13ms
summary: 2 completed, 0 cached, 0 failed, 0 blocked across 1 package(s)
```

## Second run: use the cache

```bash
cargo run --quiet -- ci examples/cache 2>&1
```

The build command is skipped and its captured output is replayed from the cache:

```text
▶ app:build
[build] generated artifact from input: hello from the cache example
app:build: cache hit in 0ms
▶ app:verify
[verify] artifact: hello from the cache example
app:verify: completed in 13ms
summary: 1 completed, 1 cached, 0 failed, 0 blocked across 1 package(s)
```

## Restore a missing output

The cache can restore declared outputs, not just command logs:

```bash
rm examples/cache/packages/app/dist/artifact.txt
cargo run --quiet -- ci examples/cache 2>&1
```

`app:build` should still report a cache hit, and `app:verify` should pass because the cached artifact was restored.

## Invalidate the cache

The cache key includes `input.txt`. Change it and run again:

```bash
printf 'changed input\n' > examples/cache/packages/app/input.txt
cargo run --quiet -- ci examples/cache 2>&1
```

`app:build` should execute instead of reporting a cache hit. Restore the original example input afterward if needed:

```bash
printf 'hello from the cache example\n' > examples/cache/packages/app/input.txt
```

## Inspect or clear the cache

```bash
cargo run --quiet -- plan examples/cache
cargo run --quiet -- cache clean examples/cache
```

The plan displays the declared cache inputs and outputs without exposing command environment values.

Caching is intentionally opt-in. Do not cache publishing, deployment, migration, network-dependent, time-dependent, or otherwise nondeterministic tasks.
