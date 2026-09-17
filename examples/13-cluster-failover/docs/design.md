# Design notes

## Why this example starts its own nodes

Every other example runs against one server and is about what the engine
computes. This one is about what the *cluster* does when part of it stops
existing, and that cannot be shown against a single node. It starts three on
ports 9340-9342, drives them, kills one, and stops them all on exit.

It deliberately does not use `lib.sh` or `VS`: the whole point is having three
addresses and asking each of them separately.

## What "yellow, not red" means

When n3 is killed:

| Colour | Means |
|---|---|
| green | every shard has all the copies it was asked for |
| yellow | every shard has at least one copy; some replicas are missing |
| red | some shard has **no** copy -- data is unreachable |

The cluster does not wait to find out whether n3 is coming back before
promoting a replica to primary, because waiting would make every write to
those shards fail in the meantime. It goes yellow the moment the replicas that
lived on n3 are gone -- and in *this* arrangement it can go back to green
without n3, because three shards with one replica need six copies and two
nodes have room for six. Size the cluster so the survivors cannot hold them
all and it stays yellow until the node returns.

So the check this example makes is that health is **not red**. Yellow against
green is a fact about how much room is left; red against the rest is a fact
about whether the data can be read at all.

Red is the one that matters operationally: yellow is a resilience problem
(you have lost redundancy), red is an availability problem (you have lost
data access). Alerting on yellow at 3am is how people learn to ignore alerts.

## Why step 9 asks each node separately

```
GET /orders/_search?preference=_local  -- against each of n1, n2, n3
```

An ordinary cluster-wide search asks *one* copy of each shard. If two copies of
the same shard disagree, a cluster-wide search returns whichever one it asked
and looks fine; the disagreement only shows up as a result that changes between
identical requests.

`preference=_local` makes each node answer from its own copy, so the three
answers can be compared. It is the check worth making after any failover test:
a replica that silently missed writes is invisible to every other request.

If the three counts differ, that is a correctness bug, not a timing artefact --
wait for the cluster to go green first, then compare.

## Writing through any node

Step 4 writes through all three. A node that does not hold the primary for a
shard forwards the write to the node that does; the client does not need to
know, and a load balancer in front of the cluster is free to pick any node.

The cost is one extra network hop per write on average. The benefit is that
there is nothing for a client to get wrong, and nothing to reconfigure when
shards move.

## `_cat/recovery`, and why recovery is not a log replay

When n3 returns, its copy is filled from the current primary rather than
replaying a translog from the beginning. `_cat/recovery` shows the stage and a
`files_percent` climbing: it is a file copy, followed by catching up on
whatever was written during the copy.

This matters for capacity planning: recovery bandwidth is a function of index
*size*, not of how long the node was away. A node gone for one minute and a
node gone for one day cost roughly the same to bring back if the index changed
enough to invalidate the segments.

## `_cluster/allocation/explain`

Step 7 asks why a particular shard copy is where it is, or is nowhere. It is
the only tool that answers "why is this shard unassigned" without guessing, and
the answer is usually one of: no node has a copy and none can be made
(insufficient nodes for the replica count), a disk watermark is exceeded, or an
allocation filter excludes every eligible node.

Reach for it before restarting anything.

## Transient settings, and why they are a trap

```json
"transient": { "cluster.routing.allocation.enable": "all" }
```

Transient settings are lost on a full cluster restart. Setting
`allocation.enable: none` transiently before a rolling restart is the standard
procedure; forgetting that it is transient, and relying on it surviving, is the
standard way to be surprised. Persistent settings survive; static ones live in
the configuration file and need a restart.

## What would change at scale

- **Three shards and one replica** on three nodes is the smallest arrangement
  that shows anything. Real sizing is driven by shard size (20-50 GB) and by
  how many nodes must be able to fail.
- **A two-node cluster cannot tolerate a failure** and elect a manager, because
  two is not a majority of two. Three manager-eligible nodes is the minimum for
  an available cluster, which is why this example has three.
- **`preference=_local` skews load** and should never be a production default;
  it is a diagnostic.
