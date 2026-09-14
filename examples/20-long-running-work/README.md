# 20. Work that takes longer than a request

A courier keeps six thousand parcels in an index, and the back office runs
jobs over all of them: add a surcharge to remote deliveries, audit everything,
reprice by weight, purge the cancelled ones. Each job is one request that
touches thousands of documents, and none of them should be run the way a
search is run -- with a client holding a connection open and hoping.

This example sends those jobs off as tasks, follows them by id, slows one down
on purpose, lets a job collide with writes it did not expect -- once counting
the collisions, once stopping at the first -- and reads the results back from
where they are kept after the request that started them has gone.

## What it shows

| Step | Feature |
|---|---|
| 1 | an index with `refresh_interval: -1`, so what a search sees changes only when asked |
| 2 | 6000 generated parcels through `_bulk` |
| 3 | `_update_by_query?wait_for_completion=false` -- the answer is a task id |
| 4 | `GET _tasks/<id>`, polled until `completed` is true; `status` and `response` |
| 5 | `requests_per_second` and `scroll_size`: `batches` and `throttled_millis` |
| 6 | single-document `_update` calls the job's view of the index does not include |
| 7 | `conflicts: proceed` -- `version_conflicts` counted, the other parcels written |
| 8 | the default, `conflicts: abort` -- a 409, `version_conflict_engine_exception`, and no rollback |
| 9 | `_tasks?actions=*byquery&detailed` |
| 10 | the `.tasks` index: a finished task's result, read by id and searched |
| 11 | `_delete_by_query?wait_for_completion=false`, followed the same way |

`_rethrottle`, `_tasks/<id>/_cancel`, `slices`, asynchronous search and the
search `profile` are part of the same story and are not in `run.sh`, because
what they print depends on timing. `docs/design.md` says what each one is for.

## Running it

```bash
make serve      # a node configured for this example, port 9280, foreground
make run        # the example, in another terminal
```

`make check` validates the scripts and every request body without a server;
`make clean` deletes what the example left behind. Copy `.env.example` to
`.env` to change the address or anything else.

The longer form, and what this example needs of the node:

```bash
BOOSTSEARCH_ADDR=127.0.0.1:9280 BOOSTSEARCH_TRANSPORT_PORT=9380 ./target/release/boostsearch &
BS=http://127.0.0.1:9280 examples/20-long-running-work/run.sh
```

Nothing beyond a plain node. Port 9280 is also the default of the URL-repository
fixture in `tools/`; if that fixture is running, start this example on another
port with `PORT=9287 make serve` and `BS=http://127.0.0.1:9287`.

## What to look for

- **Step 3** answers `{"task": "..."}` and nothing else -- no counts, because
  on a real cluster the job has barely started. Everything the client learns
  from here on comes from `_tasks/<id>`. (On this node a job of 350 parcels has
  already finished by the time the id comes back, about 100 ms; step 4's first
  poll says `completed=true`.)
- **Step 5** is 6000 parcels in batches of 500 at 2000 a second. Twelve
  batches, a pause after each but the last: `throttled_millis` is 2750, and the
  request took 3096 ms. The throttle is a promise about load on the cluster, not
  about how long the job takes, and it costs exactly the time it says it does.
- **Step 7** is the step that matters. Forty parcels were marked delivered
  after the job's view of the index was taken. The job reads each parcel with
  its sequence number and writes it back only if that number has not moved, so
  those forty come back as `version_conflicts: 40`, `updated: 5960`, and all
  forty deliveries survive. Without that check the repricing would have written
  back the old `status` and the deliveries would have been silently undone.
- **Step 8** is the same collision without `conflicts: proceed`. The job works
  a batch of 1000 at a time: the first batch meets some of the forty writes, so
  it writes the rest of that batch, lists every conflict it met there in
  `failures` (the first is `TH000075`), answers 409 and stops. What it wrote
  stays written. An aborted job is not a transaction.
- **Step 10**: the repricing's result is a document in `.tasks`, with the same
  40 conflicts, and it is still there after the request, the connection and the
  client are gone. That is what makes a job sent off safe to lose track of.
- **Step 11** purges 554 cancelled parcels and leaves 5446.

## This directory

It is a project of its own: nothing here reaches outside the directory,
so it can be copied somewhere else and still run.

| | |
|---|---|
| `README.md` | this page |
| `docs/design.md` | why it is built this way, and what would change at scale |
| `docs/api.md` | every request it makes, and every endpoint it touches |
| `docs/troubleshooting.md` | what goes wrong, and what it means |
| `run.sh` | the example |
| `node.sh` | a node configured for exactly what this example needs |
| `lib.sh` | shell helpers; its own copy |
| `Makefile` | `serve`, `run`, `check`, `clean` |
| `.env.example` | the settings, with a line each on what they are for |
| `requests/` | 9 request bodies, one file each |

The parcels are generated by `run.sh` with a fixed seed rather than kept in
`data/`, so every run has the same 6000 and the same counts.

## Leaves behind

The index `parcels` (5446 documents) and one document in `.tasks` for each job
sent off with `wait_for_completion=false` -- three per run. Rerunning deletes
`parcels` first; `.tasks` belongs to the node and is left alone, so it grows by
three each run.
