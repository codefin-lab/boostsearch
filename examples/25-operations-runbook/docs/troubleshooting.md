# Troubleshooting

## Step 2 says green, not yellow

Another data node is in the cluster, and the replica was placed on it. This
example needs a node of its own:

```bash
curl -s 'localhost:9285/_cat/nodes?v&h=name,node.role'
```

It can also be the short window described in `run.sh` next to `settle`: for
about half a second after an index is created, this node's `_cluster/health`
can answer `"status":"green"` while its own `unassigned_shards` is 1 and
`level=indices` already shows the new index yellow. `run.sh` waits for the
two to agree before it asks. If you ask by hand straight after a create, ask
twice.

## Step 5 answers 200 where 408 was expected

The cluster did go green within the timeout, which on this example means the
replica was placed, which means a second node. See above.

## Step 10 does not go green

`number_of_replicas` was changed, and health is still yellow: another index
on the node also has a replica. Find it:

```bash
curl -s 'localhost:9285/_cat/shards?v&h=index,shard,prirep,state&s=state' | grep UNASSIGNED
```

## A write answers 403 `cluster_block_exception`

A block is set on the index. Find which, and remove it with `null`:

```bash
curl -s 'localhost:9285/_all/_settings/index.blocks*?pretty'
curl -s -X PUT localhost:9285/orders/_settings -H 'content-type: application/json' \
  -d '{"index.blocks.write": null, "index.blocks.read_only_allow_delete": null}'
```

If `run.sh` was interrupted between steps 15 and 16, `orders` is left blocked.
The next run deletes and recreates it, so rerunning is also a fix.

Two things differ from OpenSearch here:

- **`read_only_allow_delete` is reported as the plain read-only block.** This
  node answers `403` with `FORBIDDEN/5/index read-only (api)`. OpenSearch
  answers `429` with
  `TOO_MANY_REQUESTS/12/disk usage exceeded flood-stage watermark, index has read-only-allow-delete block`.
  A client that retries on 429 will not retry here.
- **A write block set with `PUT /<index>/_block/write` cannot be lifted with
  the settings API.** `{"index.blocks.write": false}` answers
  `acknowledged`, the setting then reads `"false"`, and writes are still
  refused with 403. `run.sh` sets and removes the block through
  `_settings` only, which works in both directions.

## A bulk answers 200 and the documents are not there

Look at `errors` in the answer. Under a block, or on any per-document failure,
the bulk as a whole succeeds and each item carries its own status:

```bash
... | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["errors"], {i["index"]["status"] for i in d["items"]})'
```

## Documents never become visible without `_refresh`

On this node, a document written without `refresh=true` or an explicit
`_refresh` does not appear in `_search` or `_count`, however long you wait --
with `refresh_interval` at its default, or set explicitly to `1s`. `GET` by id
finds it at once. OpenSearch refreshes every `refresh_interval`, so the
document would be searchable within about a second.

That is why step 11 shows only the part that behaves the same on both: with
`refresh_interval: -1` nothing is visible until `_refresh`, and afterwards
everything is. Until the periodic refresh exists, write with
`?refresh=wait_for` or call `_refresh` after a load.

A related difference: an index created with `refresh_interval` in its
settings, updated to another value, and then reset with `null`, goes back to
the value it was *created* with, not to the default. `run.sh` avoids this by
never setting it at creation.

## Step 12 says one segment before the merge

Segments are merged in the background as well, so the count before a force
merge depends on timing. On this node a delete with `refresh=true` merged all
eight segments of `orders` into one, which is why the force merge happens
before step 15 deletes a document.

A related difference: a force merge (`max_num_segments=1`, or
`only_expunge_deletes=true`) on an index that is already one segment with a
deleted document leaves the deleted document in the segment
(`docs.deleted` stays 1 in `_cat/segments`). OpenSearch rewrites that segment
and the count goes to 0. At the same time `_stats` and `_cat/indices` report
`docs.deleted: 0` for the index while `_cat/segments` reports 1.

`_cat/segments` also reports `size` as `0b` for every segment.

## `_nodes/hot_threads` answers with JSON about the node

This node does not implement hot threads; the path is answered as a nodes-info
request, so the answer is the node's plugins, modules and addresses rather
than the plain-text thread dump OpenSearch returns
(`::: {node}{id}... Hot threads at ..., interval=500ms, busiestThreads=3`).
There is no substitute on the node; use the operating system's own tools
(`top -H -p <pid>`, `perf`, `sample` on macOS).

## `_nodes/stats` says one gigabyte of memory and two of disk

The `jvm`, `os`, `fs` and `process` sections of `_nodes/stats` are fixed
numbers on this node: 1 GiB total memory, 50% used; 2 GiB of disk, 1 GiB free;
zero heap, zero file descriptors. `_cat/allocation`'s disk columns and
`_cat/nodes`' `heap.percent`, `ram.percent`, `cpu` and `load_*` columns come
from the same place. Do not alert on them.

For memory, the node's own report is real:

```bash
curl -s localhost:9285/_boostsearch/memory | python3 -m json.tool
```

For disk, ask the operating system about the data directory (`df -h`, and
`du -sh` on `BOOSTSEARCH_DATA`). `store.size` in `_cat/indices` and `_stats`
is real.

In `_nodes/stats/indices`, `docs` and `store` are real; `search.query_total`
and `segments.count` read 0 at node level even when the per-index `_stats`
count them. `level=indices` is not supported on `_nodes/stats`.

## `_cat/thread_pool` never shows anything queued

`active`, `queue`, `rejected` and `completed` are not measured on this node
and always read 0. A zero there is not evidence that nothing is being
rejected. Watch for `429` answers and `es_rejected_execution_exception` in
clients instead.

## The slow log is empty

The thresholds are accepted, stored and reported by `_settings`, but this node
does not write slow log entries anywhere -- not to its standard output, not
under its data directory. There is no substitute on the node; the closest is
`"profile": true` on the search you suspect, or `took` in its answer.

## `_stats?groups=` is always empty

Searches made with `?stats=dashboard` are counted in `query_total`, but
`_stats/search?groups=dashboard` answers `"groups":{}`. OpenSearch answers with
`groups.dashboard.query_total` and the rest of the search counters for that
group.

## Other counters in `_stats` that do not count

On an index where documents `1` and `2` were indexed, `2` indexed again, `1`
deleted with `refresh=true`, and `_refresh` and `_forcemerge` called:

| Counter | This node | OpenSearch |
|---|---|---|
| `indexing.index_total` | 1 -- the live document count | 3, every index operation |
| `indexing.delete_total` | 0 | 1 |
| `refresh.total` | 0 | at least 2 |
| `merges.total` | 0 | at least 1 |

`docs`, `store`, `search.query_total` and `segments.count` are real, which is
why step 20 reads only those.

## Settings a live index should refuse

`PUT /<index>/_settings {"index.number_of_shards": 3}` on an open index
answers `acknowledged` on this node, and `_cat/shards` then lists three
shards. OpenSearch refuses it with 400,
`Can't update non dynamic settings [[index.number_of_shards]] for open indices`.
Changing the shard count is `_split` or `_shrink` (example 14).

Under `read_only_allow_delete`, changing `refresh_interval` is accepted here;
OpenSearch refuses any settings change other than removing the block itself.

## Cleaning up

```bash
make clean          # deletes orders, audit, scratch-flood
curl -s -X PUT localhost:9285/_cluster/settings -H 'content-type: application/json' \
  -d '{"persistent":{"cluster.routing.allocation.disk.watermark.low":null},
       "transient":{"cluster.routing.allocation.enable":null}}'
```
