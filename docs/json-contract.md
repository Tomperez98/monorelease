# Mono JSON output contract

Mono's machine-readable output is newline-delimited JSON. Command documents
and errors have this shape:

```json
{"schema":1,"kind":"...",...}
```

Execution events use an `event` discriminator instead, together with `run_id`
and `sequence`. The schema number is shared by execution events, command
documents, and error documents. A consumer must reject an unsupported schema
instead of guessing.

## Execution events

`mono run --output json` and `mono task --output json` emit these events in
monotonically increasing `sequence` order. `run_id` identifies one invocation.

```json
{"run_id":123,"sequence":0,"event":"run_started","schema":1,"project":"/repo","task_count":2}
{"run_id":123,"sequence":1,"event":"task_started","schema":1,"task":"build"}
{"run_id":123,"sequence":2,"event":"task_output","schema":1,"task":"build","stream":"stdout","bytes":[111,107,10]}
{"run_id":123,"sequence":3,"event":"task_finished","schema":1,"task":"build","status":"completed","elapsed_ms":12}
{"run_id":123,"sequence":4,"event":"run_finished","schema":1,"completed":1,"cached":0,"failed":0,"blocked":0}
```

Event meanings:

- `run_started`: the validated plan is about to execute;
- `task_started`: a task was dispatched;
- `task_output`: captured output, represented as bytes so non-UTF-8 output is preserved;
- `task_finished`: one of `completed`, `cached`, `failed`, `timed_out`, `output_limit`, or `blocked`;
- `run_finished`: final task counts.

Task output is emitted before its `task_finished` event. Concurrent tasks may
finish in any order, but `sequence` is always increasing and presentation is
serialized by Mono.

## Plan document

```json
{
  "schema": 1,
  "kind": "plan",
  "project": "demo",
  "root": "/repo",
  "tasks": [
    {
      "id": "test",
      "command": ["tool", "test"],
      "cwd": "/repo",
      "cache": false,
      "inputs": [],
      "outputs": [],
      "cache_env": [],
      "timeout_seconds": 600,
      "max_output_bytes": 16777216,
      "resource_group": null,
      "retries": 0,
      "retry_backoff_seconds": 0,
      "finalizer": false,
      "depends_on": ["build"],
      "env": ["MODE"]
    }
  ]
}
```

`env` contains names only. Values are intentionally excluded.

## Graph document

```json
{
  "schema": 1,
  "kind": "graph",
  "project": "demo",
  "root": "/repo",
  "edges": [
    {"task":"test","depends_on":["build"]}
  ]
}
```

## List document

```json
{
  "schema": 1,
  "kind": "list",
  "project": "demo",
  "root": "/repo",
  "default_pipeline": "ci",
  "pipelines": [
    {"name":"ci","tasks":["test"],"finally":[]}
  ],
  "tasks": [
    {"id":"test","command":["tool","test"]}
  ]
}
```

## Check document

A successful `mono check --output json` returns:

```json
{"schema":1,"kind":"check","status":"ok","project":"/repo"}
```

## Errors

A command failure in JSON mode is one JSON document on stdout:

```json
{
  "schema": 1,
  "kind": "error",
  "code": 1,
  "message": "project has no task 'missing'"
}
```

The numeric `code` is the same process exit code that terminal mode returns:

- `1`: the request was understood but the project or task failed;
- `2`: invalid command-line usage;
- `3`: Mono or its environment could not carry out the request.

A run that already emitted execution events may emit an error document after
its final task event. Consumers should process all documents for the same
`run_id` before deciding whether the invocation succeeded.
