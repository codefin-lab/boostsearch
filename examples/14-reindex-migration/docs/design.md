# Design notes

## The rule that makes everything here possible

**Clients talk to aliases, never to index names.**

That is the whole design. A mapping cannot be changed in place, so every schema
change is "build a new index and move to it". If clients know the index name,
that move is a coordinated deployment. If they know an alias, it is a single
atomic call.

Step 3 adds the alias to the *old* index before anything else happens, which is
the retrofit: even if it should have been there from day one, adding it is
free and must happen before the migration, not during it.

## The order of operations, and why each step is where it is

```
1. alias the old index                 clients stop knowing index names
2. create the new index, properly      mapping fixed at creation
3. create the ingest pipeline          for what a mapping cannot express
4. simulate the pipeline               cheap, before 2,000 documents
5. reindex old -> new, sliced          the long part
6. verify counts and the new queries   before anyone depends on it
7. swap the alias, atomically          one action list, no gap
```

Skipping step 6 is how a migration ships with half the documents. The reindex
reports `created`, `updated`, `version_conflicts` and `failures`; a `failures`
array that is non-empty and unread is the usual cause.

## `slices=auto`, `op_type: create`, `conflicts: proceed`

```json
"?wait_for_completion=true&refresh=true&slices=auto"
"dest": { "index": "people-v2", "pipeline": "people-fix", "op_type": "create" },
"conflicts": "proceed"
```

- **`slices=auto`** runs one sub-task per source shard, so the reindex uses the
  whole machine instead of one thread. On a four-shard source that is roughly a
  four-times speed-up for a CPU-bound reindex.
- **`op_type: create`** means a document that already exists in the destination
  is *not* overwritten. Without it, a reindex rerun silently replaces newer
  documents with older ones -- which is precisely what happens when a migration
  is retried after a partial failure while the new index is already taking
  writes.
- **`conflicts: proceed`** turns the resulting version conflicts into a counter
  instead of an abort. The count in the answer is then the number to look at.

Together they make the reindex safely re-runnable, which is the property you
want at 2am.

## Why an ingest pipeline as well as a mapping

A mapping can change a field's *type*. It cannot:

- split a comma-separated string into an array (`split`);
- derive a field that was never there (`age`, from `born`);
- recover from a value that will not convert (`convert` with `on_failure`);
- stamp when the migration happened (`_ingest.timestamp`).

So the migration is a mapping *and* a pipeline, and the pipeline is simulated
(step 5) before it runs over two thousand documents.

## The atomic swap

```json
{"actions": [
  {"remove": {"index": "people-v1", "alias": "people"}},
  {"add":    {"index": "people-v2", "alias": "people"}}]}
```

One request, applied as one cluster-state update. Two separate calls leave a
window -- microseconds, but real -- in which `people` resolves to nothing and
every client gets a 404. Under load, microseconds is thousands of requests.

## Filtered aliases

Step 9 makes `people-th` a view of `people-v2` filtered to Thailand. It is
cheap, and it is *not* a security boundary: a caller who can reach the cluster
can query `people-v2` directly. For a real tenancy boundary the filter has to
be attached to the caller's role, which is example 6.

## `_shrink` and `_split` are not reindexes

Both build a new index by hard-linking the source's segment files rather than
re-reading documents. That makes them near-instant and imposes two conditions:

- the index must be read-only first (`index.blocks.write: true`);
- the shard counts must divide: shrink to a factor, split to a multiple.

They cannot change a mapping, which is why they are separate from the migration
above rather than part of it. Use them when the *sharding* is wrong and the
schema is right.

## What would change at scale

- **Reindex reads through a scroll**, so a very long reindex holds a search
  context. `size` in the source controls the batch; the default is often too
  small for small documents.
- **Reindex competes with live traffic** for the same threads. `requests_per_second`
  throttles it, and `_rethrottle` changes that on a running task.
- **Remote reindex** (step 14) is slower by a lot -- it goes over HTTP and
  re-parses JSON. For a large migration between clusters, a snapshot restore
  (example 12) is usually faster.
