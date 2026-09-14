# Design notes

## Why a job is sent off rather than waited for

An `_update_by_query` over a large index can run for minutes. Waited for, it
holds a connection open that long, and every proxy, load balancer and client
library between the caller and the cluster has a timeout shorter than that.
When one of them gives up, the job does not stop -- the cluster keeps working --
but the caller has lost the only thing that would have told it what happened.

`wait_for_completion=false` turns that around. The answer is a task id, given
at once, and the job's progress and result are read from `_tasks/<id>` by
whoever wants them, whenever they want them, from any connection. Step 4's
`follow` is the loop a client writes: poll once a second, stop when
`completed` is true, read `response`.

## Why the parcels are generated, and why 6000

A job over a handful of documents is over before anything about it can be
looked at, and the numbers that matter -- batches, throttle, conflicts -- are
all 0 or 1. Six thousand gives twelve batches of 500, a throttle measured in
seconds, and a purge of several hundred. The generator uses a fixed seed, so
every run has the same parcels: 350 remote-area parcels still to deliver, 554
cancelled, and the checks can name exact numbers.

## Throttling is about the cluster, not the job

```
POST /parcels/_update_by_query?requests_per_second=2000&scroll_size=500
```

`scroll_size` is how many documents each batch reads and writes;
`requests_per_second` is the rate the job may average. After each batch the job
waits until it is back under the rate: 500 documents at 2000 a second is a
quarter of a second per batch. Twelve batches, eleven waits, 2750 ms -- which is
what `throttled_millis` says.

The point is not to make the job slow. It is to leave the cluster able to
answer searches while a back-office job runs. A job that is too slow is sped up
with `_rethrottle` on the running task (`requests_per_second=-1` removes the
limit), which takes effect from the next batch -- no need to cancel and restart.

## The version check, and why step 6 does not refresh

A by-query job is a search followed by writes. It reads each document along
with its `_seq_no` and `_primary_term`, and writes it back only if they have
not changed since. A document that has changed is a *version conflict*.

That is the check that keeps a job from undoing a write it never saw. In
step 7 the repricing script sets `fee`; the dispatch app has set `status`. If
the job wrote back the document it read, the parcel would be repriced and
undelivered again, and nothing would say so.

A conflict happens whenever a document is written after the job's view of the
index was taken. On a busy index that is a write landing while the job runs.
That race is real but not repeatable, so the example makes it repeatable
instead: `refresh_interval: -1` on the index, forty writes with no refresh
between them and the job, and the job's view is guaranteed to predate all
forty. The job sees exactly what it would have seen of forty concurrent writes,
every run.

## `conflicts: proceed` against `abort`

| | `abort` (the default) | `proceed` |
|---|---|---|
| On a conflict | stops the job | counts it, moves on |
| HTTP status, waited for | 409 | 200 |
| Already written | stays written | stays written |
| Right for | a job whose partial result would be wrong | a job that can be run again |

Neither rolls anything back; there is no transaction. Step 8 stops after 107
parcels, and those 107 have `checked_at` set. `proceed` is right for the
repricing because it is idempotent: the forty parcels it skipped still carry
the old fee, and running it again picks them up. A job that is not idempotent
-- add 35 to every fee -- must not be rerun blindly after a partial run, with
either setting.

## Where a task's result lives

A running task lives in memory on the node running it. When it finishes with
`wait_for_completion=false`, its result is written to the `.tasks` index, which
is how `_tasks/<id>` can answer after the task is gone. Being a document, it can
be searched: step 10 asks for every finished job that met a conflict.

`.tasks` is never cleaned up by the cluster. Delete old results yourself (by
`DELETE /.tasks/_doc/<id>`, or a `_delete_by_query` over it) once they have been
read.

## `slices`: one job, several walks

`slices=N` splits a job into N sub-tasks, each walking a disjoint part of the
index, in parallel; `slices=auto` picks one per shard. The parent's answer sums
them and lists each under `slices`, and `_rethrottle` divides the rate among
them. It is how a large job uses more than one core per node. It is not in
`run.sh`; see below.

## Cancelling

`POST /_tasks/<id>/_cancel` asks a running job to stop. It stops between
batches, not mid-batch, and like an abort it rolls nothing back: the task's
result says how far it got. A task that has already finished cannot be
cancelled, and asking is an error, not a success.

## Asynchronous search and `profile`

The same problem appears on the read side. A search that takes a minute can be
submitted to `_plugins/_asynchronous_search`, which returns an id; the partial
and then final result is fetched by id and deleted when no longer needed.

A search that is slow in the first place is taken apart with `"profile": true`:
per shard, the time spent in each query clause (with a breakdown into
`create_weight`, `build_scorer`, `next_doc`, `score` and so on), in the
collectors, in each aggregation, and in the fetch phase. That is how "this
search is slow" becomes "the wildcard clause on shard 1 is slow".

## Where this node differs, and what was left out of `run.sh`

Everything above is how OpenSearch behaves. This node answers the same
endpoints, but for these parts its answers are not yet the same, so `run.sh`
does not print them as if they were:

- **A job sent off is finished before its id comes back.** The request with
  `wait_for_completion=false` returns when the job is done; `_tasks/<id>` is
  never `completed: false`, and `_tasks?actions=*byquery` never lists a running
  job (step 9 runs it only once nothing should be running). A throttled job sent
  off is not held to its rate at all, though its result reports the
  `throttled_millis` it would have waited. A waited-for job (step 5) is held to
  it.
- **`_rethrottle`** answers 200 with the task's finished result under a node
  named `node-0`, and changes nothing.
- **`_tasks/<id>/_cancel`** answers `{"nodes":{},"node_failures":[],"tasks":[]}`
  with 200 for any id, known or not.
- **`slices`** does one walk; the totals are right, but every entry under
  `slices` reports zeros.
- **Asynchronous search** answers 501, `not_implemented_exception`.
- **`profile`** reports one shard however many there are, `searches: []` (no
  query timing at all), the same total time for each `terms` aggregation, and
  zeros for a `date_histogram`.
- **Result details**: the task's `description` is its own id rather than what
  the job does, `start_time_in_millis` is 0, `action` is
  `indices:data/write/by_query` for both job kinds (so `actions=*byquery` would
  not match it), and a by-query job refreshes the index when it finishes even
  without `?refresh`.

## What would change at scale

- **Poll less often, and from anywhere.** Once a second is fine for an
  example. A job that runs for an hour is checked every minute, and the id is
  stored somewhere durable -- it is the only handle on the job.
- **Slice by shard count.** `slices=auto` on a 20-shard index is 20 parallel
  walks; throttle the job as a whole, because the rate is shared.
- **Expect conflicts on a live index**, and design the job to be rerun: an
  idempotent script, a query that excludes what is already done (`must_not`
  on a marker field), and `conflicts: proceed`.
- **Clean `.tasks`.** Thousands of jobs a day leave thousands of documents.
