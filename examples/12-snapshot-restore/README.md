# 12. A backup that is proved, not merely taken

A snapshot nobody has restored is a hope, not a backup. This example does the
whole loop in one run: count and sum the data, snapshot it, destroy part of it,
restore beside the original, and compare the two numbers. If the last step does
not say the numbers match, the backup was not a backup.

## What it shows

| Step | Feature |
|---|---|
| 1 | `PUT _snapshot/<repo>`, an `fs` repository with `compress` |
| 2 | `_snapshot/<repo>/_verify` -- every node can write to it |
| 4 | `wait_for_completion`, `ignore_unavailable`, `include_global_state`, snapshot `metadata` |
| 5 | `_snapshot/<repo>/_all`, `_status`, `_cat/snapshots` |
| 6 | `_delete_by_query` -- the damage |
| 7 | `_restore` with `rename_pattern` / `rename_replacement` and `index_settings` |
| 7 | the proof: document count and a sum, before against after |
| 8 | a restore over a live index, refused |
| 9 | a second, incremental snapshot |
| 10 | a snapshot management policy with a cron and a retention rule |
| 11 | deleting a snapshot, `_cleanup` |

## Running it

```bash
make serve      # a node configured for this example, port 9272, foreground
make run        # the example, in another terminal
```

`make check` validates the scripts and every request body without a server;
`make clean` deletes what the example left behind. Copy `.env.example` to
`.env` to change the address or anything else.

The longer form, and what this example needs of the node:

The node must be told where repositories may live:

```bash
BOOSTSEARCH_PATH_REPO=/tmp/boost-repo \
BOOSTSEARCH_DATA=/tmp/boost-snap \
./target/release/boostsearch &

examples/12-snapshot-restore/run.sh
```

`REPO` names the repository (default `backups`); its `location` is relative to
`BOOSTSEARCH_PATH_REPO`.

## What to look for

- **Step 4's** `metadata` block is free and worth using: when somebody finds a
  snapshot in six months, it says who took it and why.
- **Step 7** is the only step that matters. It restores under a new name so the
  original stays put, then compares document count *and* a sum over a numeric
  field. A count alone would pass on a restore that lost the contents of every
  document.
- **Step 8** fails on purpose. A restore cannot write over an open index, and
  the refusal is the safety: if it silently succeeded, an accidental restore
  would destroy live data. The two ways round it are closing the index or
  renaming, and step 7 shows the rename.
- **Step 9's** `_status` reports far fewer bytes than step 5's. A snapshot
  references the segment files already in the repository rather than copying
  them, which is why a nightly snapshot of a large index is cheap and why
  `_cleanup` -- step 11 -- is needed after deleting one.

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
| `requests/` | 2 request bodies, one file each |

## Leaves behind

The repository `backups` with `nightly-2` in it, the indices `ledger`,
`ledger-restored` and `other`, and the policy `nightly`. Rerunning deletes and
recreates them. The repository's files are under `BOOSTSEARCH_PATH_REPO`.
