# Troubleshooting

## `_tasks/<id>` answers 404, `resource_not_found_exception`

The task is neither running nor stored. Three usual reasons:

- the job was waited for (no `wait_for_completion=false`), so no result was
  written to `.tasks` -- the answer to the request was the only copy;
- the id was mistyped or truncated; it is the whole string from the
  `{"task": "..."}` answer, colons included;
- someone deleted the result from `.tasks`.

## Step 7 says 0 version conflicts

The job's view of the index included the forty dispatch writes, so there was
nothing to conflict with. That means something refreshed the index between
step 6 and step 7: either the index was created without
`refresh_interval: -1` (the default refreshes every second), or another
client ran a refresh. Check:

```bash
curl -s 'localhost:9280/parcels/_settings?filter_path=**.refresh_interval'
```

## A waited-for job answers 409, and some documents changed anyway

That is `conflicts: abort`, the default, doing its job: it stopped at the first
version conflict. Nothing is rolled back -- the answer's `updated` says how many
documents were written before it stopped, and `failures` names the document it
stopped on. Either rerun with `conflicts: proceed`, if the script is safe to
apply twice, or narrow the query so it excludes what is already done.

## `version_conflicts` is not 0 and nobody else is writing

A conflict needs only a write after the job's search, and "somebody" includes
the application's own background workers, an ingest pipeline reprocessing
documents, or a previous by-query job still running. `_tasks?actions=*byquery`
lists jobs still running.

## `curl` exits with an error on step 8

`lib.sh` runs curl with `--fail-with-body`, which turns a 409 into a failed
command, and the script would stop. Step 8 uses a plain `curl -sS` for that one
request so the 409 is shown rather than fatal. A copy of the step that uses
`req` needs `|| true` after it.

## The throttled job in step 5 took far longer than `throttled_millis`

`throttled_millis` counts only the waiting; the batches themselves take time
too, and a loaded machine makes them slower. On an idle laptop the request
takes a few hundred milliseconds more than the throttle. If it takes several
seconds more, the node is busy with something else.

## `.tasks` keeps growing

It is supposed to: nothing removes a task's result automatically. Delete the
results once they have been read:

```bash
curl -s -X POST 'localhost:9280/.tasks/_delete_by_query?refresh=true' \
  -H 'content-type: application/json' -d '{"query":{"match_all":{}}}'
```

## `_rethrottle`, `_cancel`, `slices`, asynchronous search or `profile` look wrong

They are not part of `run.sh` because this node does not yet answer them the
way OpenSearch does; `docs/design.md` lists what it answers instead.

## The node will not start: address in use

Port 9280 is also the default of the URL-repository fixture in `tools/`. Start
this example's node on another port and point the example at it:

```bash
PORT=9287 make serve
BS=http://127.0.0.1:9287 make run
```

## Cleaning up

```bash
make clean          # deletes parcels; .tasks is left to the node
```
