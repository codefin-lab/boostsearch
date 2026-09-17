# Troubleshooting

## The nodes do not form a cluster -- health never reaches three nodes

Two settings must agree across all three:

```
VELOSEARCH_DISCOVERY_SEED_HOSTS=127.0.0.1:9440,127.0.0.1:9441,127.0.0.1:9442
VELOSEARCH_CLUSTER_INITIAL_CLUSTER_MANAGER_NODES=n1,n2,n3
```

The seed hosts are **transport** ports (9440-9442), not HTTP ports
(9340-9342). Pointing them at the HTTP ports is the most common mistake and
produces three nodes that each think they are alone.

Read the logs:

```bash
tail -40 /tmp/velo-cluster-example/logs/n1.log
```

## `Address already in use`

Something is on the ports. The script kills them on start and on exit, but a
previous run killed mid-flight can leave a process:

```bash
lsof -ti tcp:9340,9341,9342,9440,9441,9442 -sTCP:LISTEN | xargs kill -9
```

This is worth checking before blaming anything else -- a stale node holding a
port produces failures that look like cluster bugs.

## Health stays yellow after all three nodes are up

Some replica cannot be allocated. Ask why:

```bash
curl -s -XPOST localhost:9340/_cluster/allocation/explain \
  -H 'content-type: application/json' \
  -d '{"index":"orders","shard":0,"primary":false}' | jq
```

Usual answers: a disk watermark (all three nodes share one disk here, so a full
disk affects all of them), or allocation disabled by a leftover transient
setting.

## Health goes red

Some shard has no copy at all. With one replica and one node down that should
not happen; if it does, two nodes are down, or the primary's copy was lost.
Check `_cat/shards` for `UNASSIGNED` rows and run allocation explain on one.

## Step 9 shows different counts on different nodes

Two things to separate:

1. **The cluster has not settled.** Wait for green and for a refresh, then ask
   again. A copy being filled is legitimately behind.
2. **It is green and they still differ.** That is a correctness bug. Keep the
   data directories (`ROOT`) and the logs -- that is exactly the evidence
   `tools/cluster_chaos.py` collects.

## `_cat/recovery` is empty after restarting n3

Recovery finished before the request. Watch it while it happens:

```bash
while true; do curl -s 'localhost:9340/_cat/recovery/orders?v&active_only=true'; sleep 1; done
```

## The script leaves nodes running

It kills them in an `EXIT` trap, which does not fire if the shell is killed
with `-9`. Clean up as above.

## Do not run this with the chaos harness

`tools/cluster_chaos.py` wants its own ports and is sensitive to CPU
contention. Running both produces failures in the chaos run that are not bugs,
which costs more time to disprove than the example takes to run.

## Cleaning up

```bash
rm -rf /tmp/velo-cluster-example
```

The nodes are already stopped; this removes their data and logs.
