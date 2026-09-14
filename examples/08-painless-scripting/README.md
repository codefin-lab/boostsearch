# 8. Painless, in every place it runs

Painless turns up in eight different jobs and they are easy to confuse: some
scripts see `doc`, some see `ctx`, some run once per document, one runs once
per shard and once more over the shards. This example runs the same fleet of
vehicles through all of them, so the differences sit next to each other.

The rule worth remembering: **`doc` is read-only and fast** (it reads the
column store), **`ctx` is the document itself and is only available where you
are writing it**, and `params` is the only thing that should ever vary between
calls -- a script whose text changes per request is recompiled per request.

## What it shows

| Step | Context | Sees |
|---|---|---|
| 2 | `script_fields` | `doc` |
| 3 | `script` query (filter) | `doc` |
| 4 | `_script` sort | `doc` |
| 5 | `script_score` | `doc`, `_score` |
| 6 | `terms` aggregation by script, `bucket_script` | `doc`; then bucket paths |
| 7 | `scripted_metric` | `doc`, `state`, `states` |
| 8 | `_update` | `ctx._source` |
| 9 | `_update` with `upsert` | `ctx._source` |
| 10 | `_update_by_query` | `ctx._source` |
| 11 | stored scripts, `_scripts/<id>` | as at the call site |
| 12 | Lucene `expression` | field values, numbers only |
| 13 | `_script_language`, `_script_context` | |
| 14 | a compile error | |

## Running it

```bash
make serve      # a node configured for this example, port 9268, foreground
make run        # the example, in another terminal
```

`make check` validates the scripts and every request body without a server;
`make clean` deletes what the example left behind. Copy `.env.example` to
`.env` to change the address or anything else.

The longer form, and what this example needs of the node:

```bash
./target/release/boostsearch &
examples/08-painless-scripting/run.sh
```

## What to look for

- **Step 2** computes three fields that are nowhere in the document: kilometres
  since service, range on a full tank, and age in years. Note `ChronoUnit` and
  `ZonedDateTime` -- the date library is available, and "now" arrives as a
  parameter rather than being read from the clock, so the answer is
  reproducible.
- **Step 7** is the one that is hard to find a written example of.
  `scripted_metric` runs `init`/`map`/`combine` on each shard and `reduce` over
  what the shards returned; the map keeps a per-driver running total, and the
  reduce adds the shards' maps together. Anything you cannot express with the
  built-in aggregations goes here.
- **Steps 8 to 10** are the `ctx` half. `ctx._source.km += ...` changes the
  document in place; the elvis operator `?:` gives a field that may not exist a
  default. Step 10 does it to every matching document, and `conflicts=proceed`
  keeps going past a document someone else changed underneath.
- **Step 11** stores the depreciation formula once and calls it from both a
  `script_field` and a sort. Same script, two contexts, one definition.
- **Step 12** is the reminder that Painless is not always needed. Lucene
  expressions are numeric-only and compile to bytecode; for `a * b` they are
  the cheaper choice.

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
| `requests/` | 13 request bodies, one file each |
| `data/` | 1 bulk document set |

## Leaves behind

The index `fleet` and the stored script `depreciated-value`. Rerunning deletes
them first.
