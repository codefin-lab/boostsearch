# 14. Changing a mapping on a live index

A mapping cannot be changed in place. The field that was declared `text` two
years ago and should have been `keyword` stays `text` until the data is written
again somewhere else. This example does that migration properly: alias first,
new index, an ingest pipeline for the parts a mapping cannot fix, a sliced
reindex, and an atomic alias swap so no client sees a gap.

## What it shows

| Step | Feature |
|---|---|
| 1 | the badly-typed index: everything `text` |
| 2 | why it matters -- a `terms` aggregation refused on a `text` field |
| 3 | `_aliases`, `_cat/aliases` |
| 4 | the corrected mapping, with a `keyword` sub-field |
| 5 | `split`, `trim`, `convert` with `on_failure`, `script`, `_ingest.timestamp` |
| 6 | `_reindex` with `slices=auto`, `op_type: create`, a `pipeline`, `conflicts` |
| 7 | the aggregations that were impossible in step 2 |
| 8 | an atomic alias swap: remove and add in one action list |
| 9 | a filtered alias with routing |
| 10 | `_update_by_query` with a script |
| 11 | `_delete_by_query` |
| 12 | `_shrink` |
| 13 | `_split` |
| 14 | `_reindex` from a remote cluster |
| 15 | `_tasks` |

## Running it

```bash
make serve      # a node configured for this example, port 9274, foreground
make run        # the example, in another terminal
```

`make check` validates the scripts and every request body without a server;
`make clean` deletes what the example left behind. Copy `.env.example` to
`.env` to change the address or anything else.

The longer form, and what this example needs of the node:

Step 14 needs the node started with a reindex allowlist:

```bash
VELOSEARCH_REINDEX_ALLOWLIST='127.0.0.1:*' ./target/release/velosearch &
examples/14-reindex-migration/run.sh
```

Without it, that step says so and the rest still runs.

## What to look for

- **Step 3** is the step people skip, and it is the one that makes the rest
  possible. Point clients at an alias on day one and every future migration is
  a swap; point them at an index name and every future migration is a
  deployment.
- **Step 6**: `slices=auto` runs one sub-task per shard, so a reindex of a
  large index uses the whole machine. `op_type: create` means a document that
  somehow already exists in the destination is not overwritten -- a version
  conflict instead, which `conflicts: proceed` counts rather than aborts on.
- **Step 8** removes and adds in a single action list. Two separate calls would
  leave a window, however short, in which `people` resolves to nothing and
  every client gets a 404.
- **Step 9** makes one index look like two. `people-th` is the same data with a
  filter attached; a client given only that alias cannot see past it, which is
  a cheaper tenancy story than an index per tenant (and example 6 is the
  stronger one).
- **Steps 12 and 13** both need `index.blocks.write` set first, and both build
  the new index by hard-linking segments rather than copying documents. That is
  why they are near-instant and why the shard counts must be multiples.

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
| `requests/` | 1 request bodies, one file each |

## Leaves behind

`people-v1`, `people-v2`, `people-v3`, `people-small`, possibly
`people-from-remote`, the aliases `people` and `people-th`, and the pipeline
`people-fix`. Rerunning deletes the indices first.
