# Design notes

## Why the questions come in this order

A page at three in the morning is read from the outside in:

1. **Is it healthy?** `_cluster/health`. One word, and the only one that
   decides whether anyone needs to be awake.
2. **What is not?** `level=indices`, then `level=shards`. Each level narrows
   the search by one step without reading anything you do not need.
3. **Why?** `_cat/shards` gives the short reason; `_cluster/allocation/explain`
   gives the rule that said no.
4. **Will it fix itself?** `wait_for_status` with a timeout.
5. **What do I change?** A dynamic setting, a block, a merge.
6. **Did I put it all back?** The last step.

The example follows the same order so it can be used as the checklist it
imitates. Every step that changes something has a later step, or a later part
of the same step, that removes the change.

## Why a replica on one node

It is the most common yellow there is. A development node, a single-node test
environment, or a cluster that has lost all but one node: the index template
says `number_of_replicas: 1`, and there is nowhere to put the copy.

It is also the case where the tools tell the whole story without a second
process. `_cat/shards` shows the missing copy, `unassigned.reason` says it was
never placed, and the explain API names the `same_shard` decider. Example 13
does the other half, where the replica has somewhere to go and loses it.

## Yellow, and what it does not mean

| Colour | Primaries | Replicas | Data reachable | Survives the loss of a node |
|---|---|---|---|---|
| green | all assigned | all assigned | yes | yes, for one node per replica |
| yellow | all assigned | some not | yes | not for the shards short of a copy |
| red | some not | -- | not all of it | -- |

Yellow is not an outage. It is the absence of a spare. The fix in step 10,
dropping the replica, is right on one node and wrong everywhere else: on a
real cluster the answer to "the replica has no node" is a node.

## `wait_for_status`, rather than polling

```
GET /_cluster/health/orders?wait_for_status=green&timeout=2s
```

The server holds the request until the colour arrives or the timeout runs out,
and says which: `timed_out: false` with 200, or `timed_out: true` with 408. A
script that polls in a loop has to invent both the interval and the give-up
condition; this has both, and the status code is enough for `curl --fail` to
stop a deploy.

Waiting for a colour is "at least this good". Asking for yellow on a green
cluster answers at once.

## `h=`, `s=` and `format=json`

The `_cat` APIs are for people, and `v` gives them headers. The same tables
become useful to scripts with three parameters:

| | |
|---|---|
| `h=index,health,docs.count` | only these columns, in this order |
| `s=health,index` or `s=name:desc` | sorted, server-side |
| `format=json` | an array of objects, one per row, every value a string |

Every value in `format=json` is a string, including `docs.count`. That is how
OpenSearch answers too, and the reason a script should convert before it
compares. `_cat` output is also not a stable contract; anything that must
survive an upgrade should read the JSON APIs (`_cluster/health`, `_stats`)
instead.

## `refresh_interval: -1` during a load

Every refresh opens a new segment, and every segment is something each search
visits and a merge must later combine. During a bulk load nobody is searching
the new documents yet, so refreshing is work with no reader. Turning it off,
loading, and refreshing once is the standard shape, and step 11 shows the cost
is only visibility: the documents are written, and appear on the one refresh.

The setting is put back with `null`, not with `"1s"`. `null` removes the
override, so the index goes back to following the default; `"1s"` would pin
a copy of today's default on the index forever.

## Force merge, and when not to

Step 12 shows eight segments from four loads; step 13 merges them into one.
Fewer segments means fewer structures per search, and a merged segment drops
the space taken by deleted documents.

It is only safe on an index that has stopped changing -- yesterday's log
index, a finished load. A single very large segment on an index that keeps
receiving writes is a problem the merge policy will not solve for a long time,
because it avoids merging segments that large again. That is why the example
merges `orders` after the last load and before anything else is written.

## `persistent` against `transient`

Both are cluster-wide and take effect at once. The difference is a full
cluster restart: `persistent` settings survive it, `transient` ones do not.
OpenSearch has deprecated `transient` because a setting that silently
disappears on restart is a surprise nobody wanted; it is shown here because
old runbooks and scripts still set it, and an operator needs to know to look
in both sections.

## Blocks, and what a client sees

| Block | Reads | Writes | Metadata changes | Delete the index |
|---|---|---|---|---|
| `index.blocks.write` | yes | no | yes | yes |
| `index.blocks.read_only` | yes | no | no | no |
| `index.blocks.read_only_allow_delete` | yes | no | no | yes |

`read_only_allow_delete` is the one a node applies on its own, when a disk
passes the flood-stage watermark. "Allow delete" means deleting whole indices,
which is how space is freed; deleting documents is itself a write and is
refused. Step 16 shows both halves.

The important detail in step 15 is the bulk. A blocked single write answers
403. A blocked bulk answers 200 with `errors: true`, and each item carries its
own 403. Error handling that looks only at the HTTP status of a bulk will not
notice.

## Clear cache, flush, refresh

They are often pressed together, and they do unrelated things:

| | What it does | What it costs | What it is not |
|---|---|---|---|
| `_cache/clear` | drops cached query and request results | the next queries rebuild them | a way to see new documents |
| `_flush` | a durable commit; the translog is trimmed | an fsync | a way to see new documents |
| `_refresh` | new writes become searchable | a new segment | a way to make writes durable |

## The slow log

The thresholds are per index and per phase (`query`, `fetch`), with four
levels each (`warn`, `info`, `debug`, `trace`). A search slower than a
threshold is written at that level; `index.search.slowlog.level` is the lowest
level that is written at all. In OpenSearch the entries go to
`logs/<cluster>_index_search_slowlog.json` (and `.log`), one line per slow
shard-level query, with the index, the time, and the source of the search.

Step 19 sets `query.debug` to `0ms` so every search qualifies, which is how
to catch a specific query once, and removes every threshold again afterwards:
a zero threshold left on a busy index writes a line for every search.

## What would change at scale

- **More than one node.** The replica is placed, the cluster is green, and
  the interesting reasons in `allocation/explain` become the disk watermarks
  (`disk_threshold`), awareness attributes, and `allocation.enable`.
- **`_cat` on thousands of indices.** Always pass `h=` and an index pattern;
  the full table is expensive to build and useless to read.
- **`_tasks` on a busy cluster** has thousands of short entries. Filter with
  `actions=*reindex*` or `actions=*byquery*`, and use `detailed=true` only
  with a filter. Example 20 catches a long task mid-flight.
- **`_cluster/pending_tasks`** matters once there are many nodes and
  indices: a queue that does not drain means the cluster manager is the
  bottleneck, and every mapping change and index creation waits behind it.
