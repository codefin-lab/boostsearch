# API surface -- 25. The questions an operator asks at three in the morning

Every request this example makes, in the order it makes them. Generated
from `run.sh`; if the two disagree, `run.sh` is right.

| Step | Method | Path | Body |
|---|---|---|---|
| the patient: orders with a replica it cannot place, audit with none | `DELETE` | `/orders` |  |
|  | `DELETE` | `/audit` |  |
|  | `DELETE` | `/scratch-flood` |  |
|  | `PUT` | `/_cluster/settings` | `requests/08-forget-both-cluster-settings.json` |
|  | `PUT` | `/orders` | `requests/01-orders-one-shard-one-replica.json` |
|  | `PUT` | `/audit` | `requests/02-audit-one-shard-no-replica.json` |
|  | `GET` | `/_cluster/health/orders?wait_for_status=yellow&timeout=5s` |  |
|  | `POST` | `/orders/_bulk?refresh=true` | `data/01-orders-monday.ndjson` |
|  | `POST` | `/orders/_bulk?refresh=true` | `data/02-orders-tuesday.ndjson` |
|  | `POST` | `/orders/_bulk?refresh=true` | `data/03-orders-wednesday.ndjson` |
|  | `POST` | `/audit/_bulk?refresh=true` | `data/05-audit-events.ndjson` |
|  | `GET` | `/orders/_count` |  |
|  | `GET` | `/audit/_count` |  |
|  | `GET` | `/_cluster/health` |  |
| is the cluster healthy? -- _cluster/health | `GET` | `/_cluster/health` |  |
| which index is it? -- level=indices | `GET` | `/_cluster/health?level=indices&filter_path=status,indices` |  |
|  | `GET` | `/_cluster/health?level=indices` |  |
| which shard of it? -- level=shards | `GET` | `/_cluster/health/orders?level=shards&filter_path=indices.*.shards` |  |
|  | `GET` | `/_cluster/health/orders?level=shards` |  |
| will it go green if I wait? -- wait_for_status, with a timeout | `GET` | `/_cluster/health/orders?wait_for_status=green&timeout=2s&filter_path=status,timed_out` |  |
|  | `GET` | `/_cluster/health/orders?wait_for_status=yellow&timeout=2s&filter_path=status,timed_out` |  |
| the indices at a glance -- _cat/indices, then only the columns that matter | `GET` | `/_cat/indices?v` |  |
|  | `GET` | `/_cat/indices?v&h=health,index,pri,rep,docs.count,store.size&s=health,index` |  |
|  | `GET` | `/_cat/indices?h=index,health,docs.count&s=index&format=json` |  |
| every copy of every shard, and why the missing one is missing -- _cat/shards | `GET` | `/_cat/shards?v&s=index,shard,prirep` |  |
|  | `GET` | `/_cat/shards/orders?v&h=index,shard,prirep,state,unassigned.reason` |  |
|  | `GET` | `/_cat/shards/orders?format=json&h=prirep,unassigned.reason` |  |
| and the reason in full -- _cluster/allocation/explain | `GET` | `/_cluster/allocation/explain` | `requests/03-why-is-this-replica-unassigned.json` |
| the nodes, and what each is holding -- _cat/nodes, _cat/allocation | `GET` | `/_cat/nodes?v&h=name,ip,node.role,cluster_manager` |  |
|  | `GET` | `/_cat/allocation?v&h=shards,node` |  |
| the fix on one node: no replicas -- a dynamic setting, applied to an open index | `PUT` | `/orders/_settings` | `requests/04-no-replicas-on-a-single-node.json` |
|  | `GET` | `/_cluster/health/orders?wait_for_status=green&timeout=10s&filter_path=status,timed_out` |  |
|  | `GET` | `/_cluster/health/orders` |  |
|  | `GET` | `/_cluster/health` |  |
| a big load coming: stop refreshing, load, refresh once -- refresh_interval | `PUT` | `/orders/_settings` | `requests/05-stop-refreshing-during-a-load.json` |
|  | `GET` | `/orders/_settings?filter_path=*.settings.index.refresh_interval` |  |
|  | `POST` | `/orders/_bulk` | `data/04-orders-thursday.ndjson` |
|  | `GET` | `/orders/_count` |  |
|  | `POST` | `/orders/_refresh` |  |
|  | `GET` | `/orders/_count` |  |
|  | `PUT` | `/orders/_settings` | `requests/06-refresh-as-configured-again.json` |
|  | `GET` | `/orders/_settings` |  |
| how many segments? -- _cat/segments, before a force merge | `GET` | `/_cat/segments/orders?v&h=index,shard,segment,docs.count,docs.deleted,committed,searchable&s=segment` |  |
|  | `GET` | `/_cat/segments/orders?format=json` |  |
| merge them into one -- _forcemerge?max_num_segments=1 | `POST` | `/orders/_forcemerge?max_num_segments=1` |  |
|  | `GET` | `/_cat/segments/orders?v&h=index,shard,segment,docs.count,docs.deleted,committed,searchable` |  |
|  | `GET` | `/_cat/segments/orders?format=json` |  |
|  | `GET` | `/orders/_count` |  |
| cluster settings: persistent survives a restart, transient does not -- _cluster/settings | `PUT` | `/_cluster/settings` | `requests/07-one-persistent-one-transient.json` |
|  | `GET` | `/_cluster/settings` |  |
|  | `PUT` | `/_cluster/settings` | `requests/08-forget-both-cluster-settings.json` |
|  | `GET` | `/_cluster/settings` |  |
| stop the writes, keep the reads -- index.blocks.write | `PUT` | `/orders/_settings` | `requests/09-block-writes.json` |
|  | `POST` | `/orders/_doc` | inline |
|  | `POST` | `/orders/_bulk` | inline |
|  | `GET` | `/orders/_count` |  |
|  | `PUT` | `/orders/_settings` | `requests/10-lift-the-write-block.json` |
|  | `DELETE` | `/orders/_doc/o05?refresh=true` |  |
|  | `GET` | `/orders/_count` |  |
| the flood-stage block: writes refused, deletes of whole indices allowed -- read_only_allow_delete | `PUT` | `/orders/_settings` | `requests/11-read-only-allow-delete.json` |
|  | `POST` | `/orders/_doc/o99` | inline |
|  | `PUT` | `/scratch-flood` | inline |
|  | `PUT` | `/scratch-flood/_settings` | `requests/11-read-only-allow-delete.json` |
|  | `DELETE` | `/scratch-flood` |  |
|  | `PUT` | `/orders/_settings` | `requests/12-lift-read-only-allow-delete.json` |
|  | `GET` | `/orders/_settings?filter_path=*.settings.index.blocks` |  |
| is anything queueing or being rejected? -- _cat/thread_pool | `GET` | `/_cat/thread_pool/search,write,get?v&h=name,active,queue,rejected&s=name` |  |
|  | `GET` | `/_cat/thread_pool?h=name,type&s=name:desc&format=json` |  |
| caches, flush, refresh -- the three buttons, and what each one does | `POST` | `/orders/_cache/clear?query=true&request=true&fielddata=true` |  |
|  | `POST` | `/orders/_flush` |  |
|  | `POST` | `/orders/_refresh` |  |
| which searches are slow? -- the search slow log thresholds | `PUT` | `/orders/_settings` | `requests/13-slow-log-thresholds.json` |
|  | `GET` | `/orders/_settings/index.search.slowlog*` |  |
|  | `GET` | `/orders/_search` | `requests/14-a-search-the-slow-log-would-catch.json` |
|  | `PUT` | `/orders/_settings` | `requests/15-slow-log-thresholds-removed.json` |
|  | `GET` | `/orders/_settings` |  |
| what has this index been doing? -- _stats, per index | `GET` | `/orders/_stats/search` |  |
|  | `GET` | `/orders/_search` | inline |
|  | `GET` | `/orders,audit/_stats/docs,store,search,segments?filter_path=indices.*.primaries.docs,indices.*.primaries.store.size_in_bytes,indices.*.primaries.search.query_total,indices.*.primaries.segments.count` |  |
|  | `GET` | `/orders/_stats/search` |  |
|  | `GET` | `/orders/_stats/segments` |  |
| and the node as a whole -- _nodes/stats/indices, and the memory it holds | `GET` | `/_nodes/stats/indices?filter_path=nodes.*.name,nodes.*.indices.docs,nodes.*.indices.store` |  |
|  | `GET` | `/_nodes/stats/indices` |  |
|  | `GET` | `/_velosearch/memory?filter_path=allocator,indices` |  |
| is the cluster manager keeping up? -- _cluster/pending_tasks | `GET` | `/_cluster/pending_tasks` |  |
| what is running right now? -- _tasks | `GET` | `/_tasks?detailed=true&filter_path=nodes.*.tasks.*.action,nodes.*.tasks.*.cancellable` |  |
| what this example leaves behind, checked rather than assumed | `GET` | `/_cat/indices/orders,audit?v&h=health,index,pri,rep,docs.count&s=index` |  |
|  | `GET` | `/_cluster/health` |  |
|  | `GET` | `/_cluster/settings` |  |
|  | `GET` | `/orders/_settings` |  |
|  | `GET` | `/scratch-flood` |  |
|  | `GET` | `/orders/_count` |  |
|  | `GET` | `/audit/_count` |  |

## Endpoints touched

Path parameters and query strings removed, deduplicated:

- `/<var>`
- `/<var>/_bulk`
- `/<var>/_cache/clear`
- `/<var>/_count`
- `/<var>/_doc`
- `/<var>/_doc/<var>`
- `/<var>/_flush`
- `/<var>/_forcemerge`
- `/<var>/_refresh`
- `/<var>/_search`
- `/<var>/_settings`
- `/<var>/_settings/<var>`
- `/<var>/_stats/<var>`
- `/_velosearch/memory`
- `/_cat/allocation`
- `/_cat/indices`
- `/_cat/indices/<var>`
- `/_cat/nodes`
- `/_cat/segments/<var>`
- `/_cat/shards`
- `/_cat/shards/<var>`
- `/_cat/thread_pool`
- `/_cat/thread_pool/<var>`
- `/_cluster/allocation/explain`
- `/_cluster/health`
- `/_cluster/health/<var>`
- `/_cluster/pending_tasks`
- `/_cluster/settings`
- `/_nodes/stats/indices`
- `/_tasks`

## Request bodies

- [`requests/01-orders-one-shard-one-replica.json`](../requests/01-orders-one-shard-one-replica.json)
- [`requests/02-audit-one-shard-no-replica.json`](../requests/02-audit-one-shard-no-replica.json)
- [`requests/03-why-is-this-replica-unassigned.json`](../requests/03-why-is-this-replica-unassigned.json)
- [`requests/04-no-replicas-on-a-single-node.json`](../requests/04-no-replicas-on-a-single-node.json)
- [`requests/05-stop-refreshing-during-a-load.json`](../requests/05-stop-refreshing-during-a-load.json)
- [`requests/06-refresh-as-configured-again.json`](../requests/06-refresh-as-configured-again.json)
- [`requests/07-one-persistent-one-transient.json`](../requests/07-one-persistent-one-transient.json)
- [`requests/08-forget-both-cluster-settings.json`](../requests/08-forget-both-cluster-settings.json)
- [`requests/09-block-writes.json`](../requests/09-block-writes.json)
- [`requests/10-lift-the-write-block.json`](../requests/10-lift-the-write-block.json)
- [`requests/11-read-only-allow-delete.json`](../requests/11-read-only-allow-delete.json)
- [`requests/12-lift-read-only-allow-delete.json`](../requests/12-lift-read-only-allow-delete.json)
- [`requests/13-slow-log-thresholds.json`](../requests/13-slow-log-thresholds.json)
- [`requests/14-a-search-the-slow-log-would-catch.json`](../requests/14-a-search-the-slow-log-would-catch.json)
- [`requests/15-slow-log-thresholds-removed.json`](../requests/15-slow-log-thresholds-removed.json)
- [`data/01-orders-monday.ndjson`](../data/01-orders-monday.ndjson)
- [`data/02-orders-tuesday.ndjson`](../data/02-orders-tuesday.ndjson)
- [`data/03-orders-wednesday.ndjson`](../data/03-orders-wednesday.ndjson)
- [`data/04-orders-thursday.ndjson`](../data/04-orders-thursday.ndjson)
- [`data/05-audit-events.ndjson`](../data/05-audit-events.ndjson)
