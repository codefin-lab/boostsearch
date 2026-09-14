# Troubleshooting

## Step 2 fails -- and it is supposed to

`Fielddata is disabled on text fields by default` (or similar) is the point of
the step: a `terms` aggregation on a `text` field has no column store to read.
That refusal is what the whole migration is for.

## The reindex finishes but the destination has fewer documents

Read the response rather than trusting the exit status:

```bash
curl -s -XPOST 'localhost:9274/_reindex?wait_for_completion=true' ... | jq '{created, updated, version_conflicts, failures}'
```

- `failures` non-empty: individual documents were rejected, usually by the
  mapping (a value that will not coerce) or by the pipeline. The array says
  which and why.
- `version_conflicts` non-zero with `op_type: create`: those documents already
  existed in the destination. Expected on a rerun, a problem on a first run.

## The pipeline is not applied

`pipeline` goes in `dest`, not in the query string and not in `source`:

```json
"dest": { "index": "people-v2", "pipeline": "people-fix" }
```

An index-level `default_pipeline` on the destination also works, and is applied
*in addition*.

## `_split` or `_shrink` fails with `index ... must be read-only`

Set the block first, and clear it in the target's settings:

```bash
curl -XPUT localhost:9274/people-v2/_settings -H 'content-type: application/json' \
  -d '{"settings":{"index.blocks.write":true}}'
```

Remember to clear it on the source afterwards, or writes to it stay blocked --
the example does this and it is easy to omit by hand.

## `_split` fails with a shard-count error

Split must go to a multiple of the current count; shrink must go to a factor.
4 -> 6 fails; 4 -> 8 works. 4 -> 1 shrinks; 4 -> 3 does not.

Older index layouts also require `index.number_of_routing_shards` to have been
set at creation for a split to be possible at all.

## `_shrink` fails with an allocation error

Shrink requires every shard copy of the source to be on one node. On a single
node that is automatic; on a cluster it means setting
`index.routing.allocation.require._name` to a node first and waiting for the
shards to move.

## Remote reindex fails with `reindex.remote.whitelist`

The source host must be allowed. Start the node with:

```bash
BOOSTSEARCH_REINDEX_ALLOWLIST='127.0.0.1:*' ./target/release/boostsearch
```

`make serve` does this. Without it, only that one step fails.

## The alias swap left `people` pointing at nothing

The two actions must be in one `_aliases` call. If they were two calls and the
second failed, add it back:

```bash
curl -XPOST localhost:9274/_aliases -H 'content-type: application/json' \
  -d '{"actions":[{"add":{"index":"people-v2","alias":"people"}}]}'
```

## Writing to the alias fails with "has more than one index"

An alias over several indices cannot take writes unless exactly one is marked
`is_write_index: true`. This is what example 2's rollover alias uses.

## A long reindex needs to be stopped

Run it without `wait_for_completion`, and it returns a task id:

```bash
curl -s 'localhost:9274/_tasks?actions=*reindex*&detailed=true' | jq
curl -XPOST 'localhost:9274/_tasks/<id>/_cancel'
```

Cancelling leaves the destination partially written; with `op_type: create` the
rerun picks up where it stopped.

## Cleaning up

```bash
make clean          # deletes the people-* indices
curl -XDELETE localhost:9274/_ingest/pipeline/people-fix
```
