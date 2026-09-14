# Troubleshooting

## `repository_exception ... location ... is not allowed`

A filesystem repository may only be created under a path the node was started
with. Start the node with one:

```bash
make serve          # node.sh sets BOOSTSEARCH_PATH_REPO=/tmp/boost-repo
```

The repository's `location` is relative to that directory.

## `_verify` fails, or reports fewer nodes than the cluster has

The repository directory is not writable by every node. On a single node this
means a permissions problem; on a cluster it usually means the directory is
local to one machine rather than a shared filesystem, and snapshots taken
against it will be missing whatever the other nodes' shards held.

This is the failure mode that produces a backup that restores to a partial
index, and `_verify` exists to catch it before that happens.

## `cannot restore index [ledger] because an open index with same name already exists`

Working as designed -- step 8 triggers it on purpose. Either close the index
first:

```bash
curl -XPOST localhost:9272/ledger/_close
```

or restore under another name, which is what step 7 does.

## The restore succeeds but the counts do not match

This is the failure the example exists to catch, so read it carefully before
assuming the example is wrong:

- if the restored count is **lower**, the snapshot was taken while writes were
  in flight and did not include them, or a shard was unavailable at snapshot
  time. Check `_status` on the snapshot for shard failures;
- if the counts match and the **sums** do not, documents were restored with
  different contents -- which should be impossible, and is worth reporting.

```bash
curl -s localhost:9272/_snapshot/backups/nightly-1/_status | jq '.snapshots[].shards_stats'
```

## `snapshot_missing_exception`

The snapshot name or the repository name is wrong, or the repository was
registered pointing at a different directory than the one holding the files.
List what is actually there:

```bash
curl -s 'localhost:9272/_cat/snapshots/backups?v'
```

## Step 10 fails, or `_plugins/_sm` returns 404

Snapshot management is a separate feature and may not be answered for. The
example notes it and continues; the manual snapshot in step 4 is unaffected.

## `_cleanup` reports zero bytes freed

Expected if the deleted snapshot's segments are still referenced by another
snapshot. Delete the other one too, or accept that the space is genuinely still
in use.

## A restore is very slow

Restores are not incremental -- every segment is copied. This is the number to
measure when sizing a recovery window, not the snapshot duration.

## Cleaning up

```bash
make clean          # deletes ledger, ledger-restored, other
curl -XDELETE localhost:9272/_snapshot/backups/nightly-2
curl -XDELETE localhost:9272/_snapshot/backups
rm -rf /tmp/boost-repo/backups
```

Deleting the repository registration does not delete its files; the last line
does.
