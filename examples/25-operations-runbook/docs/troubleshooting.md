# Troubleshooting

## Step 2 says green, not yellow

Another data node is in the cluster, and the replica was placed on it. This
example needs a node of its own:

```bash
curl -s 'localhost:9285/_cat/nodes?v&h=name,node.role'
```

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

The flood-stage block, `read_only_allow_delete`, answers `429` with
`TOO_MANY_REQUESTS/12/disk usage exceeded flood-stage watermark, index has
read-only-allow-delete block`, as OpenSearch does, so a client that retries
on 429 retries it. A block set with `PUT /<index>/_block/write` is the same
setting as `index.blocks.write`, and `{"index.blocks.write": false}` lifts it.

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

## Step 12 says one segment before the merge

Segments are merged in the background as well, so the count before a force
merge depends on timing. On this node a delete with `refresh=true` merged all
eight segments of `orders` into one, which is why the force merge happens
before step 15 deletes a document.

## `_nodes/hot_threads` shows no stack frames

The report is plain text in OpenSearch's layout, and the CPU time, the
percentages and the thread names are measured. A stack of another running
thread cannot be read without stopping it, so where OpenSearch prints frames
this node prints what the kernel says the thread is doing: its run state, and
on Linux the kernel function it sleeps in. For frames, use the operating
system's own tools (`perf`, `sample` on macOS).

## `_nodes/stats` reports a heap, and there is no JVM

The `os`, `process` and `fs` sections are read from the operating system.
`jvm.mem.heap_used_in_bytes` is what the allocator holds for the node's own
data, and `heap_max_in_bytes` the machine's memory; `_cat/nodes`'
`heap.percent` is the one against the other. For the allocator's own view:

```bash
curl -s localhost:9285/_boostsearch/memory | python3 -m json.tool
```

## `_cat/thread_pool` shows 0 in `queue`

`active`, `completed` and `rejected` count the requests each pool has run.
Requests do not wait in a queue of their pool here; they wait in the runtime,
whose backlog is reported as the `generic` pool's `queue`.

## Where the slow log goes

An entry is written to the node's log output, in OpenSearch's text, and to
`<cluster>_index_search_slowlog.log` and `<cluster>_index_indexing_slowlog.log`
in the directory named by `BOOSTSEARCH_LOGS` (or `path.logs`) when one is set.

## Cleaning up

```bash
make clean          # deletes orders, audit, scratch-flood
curl -s -X PUT localhost:9285/_cluster/settings -H 'content-type: application/json' \
  -d '{"persistent":{"cluster.routing.allocation.disk.watermark.low":null},
       "transient":{"cluster.routing.allocation.enable":null}}'
```
