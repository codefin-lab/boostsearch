# 13. Three nodes, and one of them killed

Everything else in this directory runs against one node. This one starts three,
gives an index a replica, kills a node mid-flight, writes while it is gone, and
brings it back -- which is the only way to find out whether the words "highly
available" mean anything in a given setup.

It starts and stops its own nodes on ports 9340-9342.

## What it shows

| Step | Feature |
|---|---|
| 1 | discovery by seed hosts, `wait_for_nodes`, `_cat/nodes` |
| 2 | `_cluster/state`, `_cat/cluster_manager` |
| 3 | shards and replicas, `_cat/shards` -- never two copies on one node |
| 4 | writing through any node, not only the one holding the primary |
| 5 | a node killed: what health says, and why it is not red |
| 6 | the cluster answering and accepting writes while short a node |
| 7 | `_cluster/allocation/explain` |
| 8 | the node returning, `_cat/recovery` -- filled from the primary |
| 9 | `preference=_local` against each node: do the copies agree |
| 10 | a cluster-wide search |
| 11 | `_cluster/reroute` |
| 12 | `_cluster/settings` |
| 13 | `_cat/allocation`, `_nodes/stats` |

## Running it

```bash
cargo build --release    # in the repository root
./run.sh                 # starts, drives and stops its own three nodes
```

`make check` validates the script without starting anything. This example
ignores `VS` and `make serve`: it is about a cluster, so it brings its own.

```bash
cargo build --release
examples/13-cluster-failover/run.sh
```

Do **not** run it at the same time as `tools/cluster_chaos.py` -- both want
ports and both are sensitive to CPU contention. `ROOT` sets where the data
directories go (default `/tmp/velo-cluster-example`).

## What to look for

- **Step 3**: the `p`/`r` column, and the node column. Three shards, one replica
  each, six rows, and no shard with both its copies on one node. That last part
  is the whole point of a replica.
- **Step 5** is the moment worth watching, and the thing to watch is that it
  is **not red**. Red means some shard has no copy at all; anything else means
  the data is still reachable. It passes through yellow -- the replicas that
  were on n3 are gone -- and here it can return to green on its own, because
  three shards with one replica need six copies and the two surviving nodes
  have room for six. On a cluster sized so the survivors cannot hold them all,
  it would stay yellow until n3 came back.
- **Step 6** proves the cluster is still a working database and not just a
  responsive one. If this write failed, the availability claim would be empty.
- **Step 8's** `_cat/recovery` shows `peer` recovery and a `files_percent`
  climbing. n3 does not replay a log from the beginning; it is filled from the
  current primary and then catches up on what happened during the fill.
- **Step 9** is the check that most tutorials skip. Each node is asked for its
  own local copy's count, with `preference=_local`. If two copies of the same
  shard disagree, this is where it shows, and a cluster-wide search would hide
  it by asking only one of them.

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
| `Makefile` | `serve`, `run`, `check`, `clean` |
| `.env.example` | the settings, with a line each on what they are for |

## Leaves behind

Nothing running -- the nodes are killed when the script exits. The data
directories and logs stay under `$ROOT` so the logs can be read afterwards.
