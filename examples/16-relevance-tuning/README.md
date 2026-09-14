# 16. Making search better, and proving it

Relevance work goes wrong when it is done by argument: somebody looks at the
first page, says it is bad, a boost is added, somebody else says it is better,
and nobody knows. This example does the opposite. It starts with a search that
is demonstrably wrong -- a deprecated page outranks the current one because it
says "snapshot" more often -- fixes it four ways, and then scores every version
against a judgement list with `_rank_eval`, so the argument is about a number.

## What it shows

| Step | Feature |
|---|---|
| 1 | per-field `similarity`: BM25 with different `b` and `k1` |
| 2 | the naive `match`, and the wrong answer it gives |
| 3 | `_explain` -- the arithmetic |
| 4 | `boosting` query: demote without excluding |
| 5 | `match_phrase` with `slop` as a `should` |
| 6 | `function_score`: `gauss` on a date, `field_value_factor`, a filtered weight |
| 7 | `rescore` -- an expensive query over the top window only |
| 8 | `multi_match` types compared: `best_fields`, `most_fields`, `cross_fields`, `phrase` |
| 9 | `_rank_eval` with nDCG, twice: before and after |
| 10 | `precision`, `recall`, `mean_reciprocal_rank`, `expected_reciprocal_rank` |
| 11 | `profile=true`, summarised |
| 12 | the two similarities from step 1, measured against each other |

## Running it

```bash
make serve      # a node configured for this example, port 9276, foreground
make run        # the example, in another terminal
```

`make check` validates the scripts and every request body without a server;
`make clean` deletes what the example left behind. Copy `.env.example` to
`.env` to change the address or anything else.

The longer form, and what this example needs of the node:

```bash
./target/release/boostsearch &
examples/16-relevance-tuning/run.sh
```

## What to look for

- **Step 2** puts `d3` -- "Backing up with the old snapshot tool", deprecated
  three years ago -- at or near the top. Its body says "snapshot" three times.
  This is not a bug in the scorer; it is what term frequency means.
- **Step 3** shows why, term by term: the `tf` component for `snapshot` in
  `d3`. Read this before changing anything; most relevance fixes are applied to
  the wrong cause.
- **Step 4** uses `boosting` rather than a `must_not`. A `must_not` would make
  the deprecated page unfindable even by someone searching for it by name; a
  `negative_boost` of 0.15 pushes it down and leaves it reachable.
- **Step 7** is the pattern for anything expensive. The phrase query runs over
  20 documents, not the index. On a real corpus that is the difference between
  a phrase boost you can afford and one you cannot.
- **Step 9** is the point of the example. Two nDCG numbers, same judgements,
  same queries. If the tuned one is not higher, the tuning did not work, and no
  amount of looking at the first page changes that.
- **Step 12** shows what `b` does. `b: 0` on `title` means a long title is not
  penalised; `b: 0.9` on `body` means a short body that mentions the term is
  worth more than a long one that mentions it as much. Both are defensible;
  the point is that it is a choice, per field.

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
| `requests/` | 6 request bodies, one file each |
| `data/` | 1 bulk document set |

## Leaves behind

The index `docs` and `/tmp/rankeval.json`. Rerunning deletes the index first.
