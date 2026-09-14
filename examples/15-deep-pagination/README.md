# 15. Page 500, and exporting the lot

`from: 11000` is refused, and the refusal is a kindness: to serve it, every
shard collects 11,020 hits and the coordinator sorts three times that to throw
almost all of it away. This example shows the three things to do instead, what
each is for, and the failure mode each one fixes.

- **`search_after`** -- a cursor built from the last hit's sort values. Costs
  the same on page 500 as on page 1. Does not protect you from the index
  changing underneath.
- **`search_after` with a point in time** -- the same cursor over a frozen view.
  This is the right answer for user-facing paging.
- **`scroll`** -- a held context, `sort: ["_doc"]`, sliceable. The right answer
  for an export, and the wrong one for a user, because it pins resources.

## What it shows

| Step | Feature |
|---|---|
| 2 | ordinary paging |
| 3 | `from` past `index.max_result_window`, and the refusal |
| 4 | `search_after` with a tiebreak sort, `track_total_hits: false` |
| 5 | `_search/point_in_time` |
| 6 | paging a frozen view while writes arrive |
| 7 | `DELETE _search/point_in_time` |
| 8 | `scroll`, `_search/scroll`, `sort: ["_doc"]`, releasing the context |
| 9 | a sliced scroll |
| 10 | `track_total_hits` exact, off, and capped |
| 11 | raising `index.max_result_window`, and why not to |

## Running it

```bash
make serve      # a node configured for this example, port 9275, foreground
make run        # the example, in another terminal
```

`make check` validates the scripts and every request body without a server;
`make clean` deletes what the example left behind. Copy `.env.example` to
`.env` to change the address or anything else.

The longer form, and what this example needs of the node:

```bash
./target/release/boostsearch &
examples/15-deep-pagination/run.sh
```

It writes 50,000 documents, so give it a few seconds.

## What to look for

- **Step 4's** note about the tiebreak is the bug nobody finds in testing. Sort
  by a field with duplicate values and no second key, and at every page
  boundary a document is either shown twice or skipped -- silently, and only in
  production where the data has duplicates.
- **Step 6** is the whole reason point-in-time exists. Three documents are
  written *between* the pages, all of which sort before everything else, and
  none of them appear. Without the PIT they would push the whole result set
  along and page 2 would repeat what page 1 already showed. Comment out the
  `pit` block and rerun to watch it happen.
- **Step 8** sorts by `_doc`, which is not an ordering at all -- it is "whatever
  order the segments happen to be in". That is what makes a scroll cheap, and
  why it is useless for showing a user anything.
- **Step 10**: the default already caps counting. `track_total_hits: false`
  removes the count entirely and is free; a number caps it and reports
  `"relation": "gte"`. Exact counting of 50,000 hits is real work done for a
  number nobody reads past the first two digits of.

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

The index `events` (with `max_result_window` raised to 20,000) and
`/tmp/events.ndjson`. Rerunning deletes the index first.
