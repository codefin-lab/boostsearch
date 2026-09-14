# 25. The questions an operator asks at three in the morning

Something has paged. The questions come in roughly the same order every time:
is the cluster healthy, which index is not, which shard of it, why, and will it
fix itself if I wait. Then, once the cause is known: stop the writes, change a
setting on a live index, merge the segments, clear a cache -- and afterwards,
put everything back the way it was, because the change nobody undid is the
cause of the next page.

This example asks each of those questions of one node, with the API that
answers it, and checks the answer rather than printing it and hoping. The
patient is an index that asks for a replica on a cluster of one node, which is
the most common reason a single-node setup is yellow.

Example 13 does the same kind of reading on three nodes while one is killed.
This one stays on a single node and is about the tools, the flags that make
their output readable (`v`, `h=`, `s=`, `format=json`), and the changes an
operator makes and must then undo.

## What it shows

| Step | Feature |
|---|---|
| 1 | an index with `number_of_replicas: 1` on a one-node cluster |
| 2 | `_cluster/health` -- what yellow does and does not mean |
| 3 | `_cluster/health?level=indices` |
| 4 | `_cluster/health/<index>?level=shards` |
| 5 | `wait_for_status` with `timeout` -- 408 and `timed_out: true` when the colour never arrives |
| 6 | `_cat/indices` with `v`, `h=`, `s=`, `format=json` |
| 7 | `_cat/shards` and the `unassigned.reason` column |
| 8 | `_cluster/allocation/explain` for the unassigned replica: the `same_shard` decider |
| 9 | `_cat/nodes`, `_cat/allocation` |
| 10 | `number_of_replicas` changed on an open index, and waiting for green |
| 11 | `refresh_interval: -1` for a load, one `_refresh`, and the setting removed with `null` |
| 12 | `_cat/segments` before a force merge |
| 13 | `_forcemerge?max_num_segments=1`, and the segment count after |
| 14 | `_cluster/settings`: `persistent` against `transient`, and `null` to remove both |
| 15 | `index.blocks.write`: a single write refused with 403, a bulk answering 200 with the refusal per item |
| 16 | `index.blocks.read_only_allow_delete`: writes refused, deleting a whole index allowed |
| 17 | `_cat/thread_pool` with `h=`, `s=` and `format=json` |
| 18 | `_cache/clear`, `_flush`, `_refresh`, and what each one is for |
| 19 | `index.search.slowlog.threshold.*` and `index.search.slowlog.level` |
| 20 | `_stats` per index, narrowed with metrics and `filter_path` |
| 21 | `_nodes/stats/indices`, and `_boostsearch/memory` for the process's memory |
| 22 | `_cluster/pending_tasks` |
| 23 | `_tasks` |
| 24 | what is left behind, checked |

## Running it

```bash
make serve      # a node configured for this example, port 9285, foreground
make run        # the example, in another terminal
```

`make check` validates the script and every request body without a server;
`make clean` deletes what the example left behind. Copy `.env.example` to
`.env` to change the address.

The longer form:

```bash
BOOSTSEARCH_ADDR=127.0.0.1:9285 BOOSTSEARCH_TRANSPORT_PORT=9385 ./target/release/boostsearch &
BS=http://127.0.0.1:9285 examples/25-operations-runbook/run.sh
```

It needs a node on its own. On a cluster with a second data node, the replica
in step 1 would be placed and steps 2 to 10 would have nothing to diagnose.

## What to look for

- **Step 5** is the question "will it go green if I wait", asked properly.
  The answer is not a colour but a status code:

  ```
  {"cluster_name":"boostsearch","status":"yellow","timed_out":true, ...}
     HTTP 408
  ```

  A deploy script that waits for green with `curl --fail` stops here, which is
  what it should do: on one node, green is not coming. Waiting for yellow in
  the same breath answers 200 at once.

- **Steps 7 and 8** go from "a copy is missing" to "why". `_cat/shards` gives
  the short reason, `INDEX_CREATED` -- it has never been placed, as opposed
  to `NODE_LEFT`. The explain API gives the long one, and names the rule:

  ```
  "deciders":[{"decider":"same_shard","decision":"NO",
    "explanation":"a copy of this shard is already allocated to this node ..."}]
  ```

  A replica on the same node as its primary would protect against nothing, so
  it is not allowed there. No setting fixes that; a second node does, or no
  replica (step 10).

- **Step 11** shows the reason `refresh_interval: -1` exists. The Thursday bulk
  answers `errors=False items=6`, and the count stays at 18. Nothing is lost:
  the documents are written and durable, just not yet visible. One `_refresh`
  and the count is 24. Setting it back with `null` removes the override, so
  the index follows the default again instead of carrying a copy of it.

- **Steps 12 and 13**. Four bulks, each with its own refresh, left eight
  segments; the force merge leaves one, with the same 24 documents:

  ```
  orders 0     _0      5          0            true      true
  ...
  orders 0     _7      1          0            true      true
                                  ->
  orders 0     _0      24         0            true      true
  ```

  How many segments there are before depends on timing; the check is that
  there is more than one before and exactly one after.

- **Step 15** is the answer a client actually sees under a write block, and it
  is not the same for every API. A single write is refused outright:

  ```
  {"error":{"type":"cluster_block_exception",
    "reason":"index [orders] blocked by: [FORBIDDEN/8/index write (api)];"}, "status":403}
  ```

  A bulk answers **200** with `"errors":true` and the 403 inside the item. A
  client that checks only the HTTP status of its bulk requests will believe
  every write under a block succeeded.

- **Step 14**: `transient` settings are gone after a full restart,
  `persistent` ones are not. The example sets one of each, reads them back in
  their separate sections, and removes both with `null`. The one to worry
  about in real life is `cluster.routing.allocation.enable: primaries`, set
  during maintenance and forgotten: replicas then never get placed and the
  cluster stays yellow with no error anywhere.

- **Step 20** checks that `_stats` counts what happened: three searches,
  `query_total` up by exactly three.

## What this node does not answer yet

Some of the questions in this runbook get an answer from this node that is not
the one OpenSearch gives. They are kept out of the checks, or the step says so
where it runs:

- `_nodes/hot_threads` answers with the nodes-info JSON, not the plain-text
  thread dump.
- `_nodes/stats` `jvm`, `os`, `fs` and `process` are fixed numbers (1 GiB of
  memory, 2 GiB of disk, zero heap), as are the disk columns of
  `_cat/allocation` and the heap, RAM and CPU columns of `_cat/nodes`. Step 21
  reads `_boostsearch/memory` for the process's real memory instead.
- `_cat/thread_pool` always reads 0 for `active`, `queue` and `rejected`.
- The search slow log thresholds are stored and read back, but no slow log
  entries are written.
- `_stats?groups=` returns `"groups":{}` after searches made with `stats=`.

`docs/troubleshooting.md` has the detail of each, and what to use in the
meantime.

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
| `requests/` | 15 request bodies, one file each |
| `data/` | 5 bulk document sets |

## Leaves behind

The indices `orders` (23 documents, no replicas) and `audit` (10 documents),
and a green cluster. Every change the runbook made is undone and step 24
checks it: no cluster settings, no blocks on `orders`, no `refresh_interval`
or slow log settings on it, and the index `scratch-flood` deleted. Rerunning
deletes both indices first; `make clean` deletes them.
