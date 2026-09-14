# Design notes

## The example is a test, not a demonstration

Every other example here shows how something works. This one asserts something:
it records a document count and a numeric sum before the snapshot, destroys
part of the data, restores, and compares. If the last line does not say the
numbers match, the run failed.

That shape is deliberate, because a snapshot nobody has restored is not a
backup and the only way to know is to do it. The two numbers are chosen to
catch different failures:

- **document count** catches a restore that lost documents;
- **the sum over `amount`** catches a restore that produced the right number of
  documents with the wrong contents -- a count alone passes that.

A real backup verification does the same thing on a schedule, against a
scratch cluster, and pages someone when the numbers diverge.

## Why the restore is renamed rather than done in place

```json
"rename_pattern": "(.+)", "rename_replacement": "$1-restored"
```

Two reasons. The engine refuses to restore over an open index (step 8), which
is a safety feature: an accidental restore would otherwise destroy live data.
And renaming means the original and the restored copy exist side by side, which
is the only way to *compare* them.

The alternatives are to close the index first (`POST /index/_close`) and restore
over it, or to restore into a differently-named index and swap an alias --
example 14's pattern, and the right one for a production recovery where clients
must not see a gap.

## Snapshots are incremental, and what that implies

Step 9's `_status` reports far fewer bytes than step 5's. A snapshot does not
copy segment files that are already in the repository; it references them.

Three consequences worth knowing:

- **A nightly snapshot of a large index is cheap**, so take them often.
- **Deleting a snapshot does not free its space** if other snapshots reference
  the same segments. `_cleanup` (step 11) removes what nothing references any
  more, and is worth running on a schedule.
- **A force-merge rewrites segments**, so the snapshot after a force-merge is
  effectively a full one. Merging a large index the night before a backup
  window is a way to be surprised.

## `include_global_state: false`

The global state is cluster settings, templates, ingest pipelines and stored
scripts. Including it makes the snapshot a cluster backup rather than an index
backup, and restoring it overwrites the destination's settings -- which is
occasionally what you want for disaster recovery and almost never what you want
when restoring one index into a running cluster.

It is set to `false` here explicitly rather than left to default, because the
default is a thing people should decide rather than inherit.

## The `metadata` block

```json
"metadata": { "taken_by": "examples/12", "reason": "before the change" }
```

Free, and worth using. In six months somebody will find `nightly-1` and need to
know whether it predates a schema change. The snapshot name almost never says
enough.

## Snapshot management policies

Step 10 creates a policy with a cron and a retention rule. The value is not
that it saves typing -- it is that retention is expressed once, declaratively,
rather than as a cron job on somebody's laptop that deletes by name pattern and
breaks when the naming changes.

`max_age: 7d` and `max_count: 10` together mean "keep a week, but never fewer
than nothing and never more than ten", which is the usual shape: an age bound
for the policy and a count bound as a safety valve against a runaway scheduler.

## What would change at scale

- **`fs` repositories must be a shared filesystem** every node can write to.
  The `_verify` in step 2 is what tells you it is not: a repository that one
  node can write and another cannot produces snapshots that are silently
  partial.
- **S3, GCS and Azure repositories** are the usual production answer, and their
  settings (chunk size, buffer size, concurrent streams) matter more for
  restore time than for snapshot time.
- **Restore is the slow half.** Snapshots are incremental; restores are not.
  Measure the restore, not the snapshot, when sizing a recovery window.
