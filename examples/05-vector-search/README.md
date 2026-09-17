# 5. Vector search that is not only vector search

Eight dimensions, chosen by hand so the geometry can be read: the first pair
means "about biology", the second "about computing", the third "about physics".
A real index puts a model's output there; here the numbers are legible, so
every answer can be checked by eye rather than taken on trust.

The interesting part is not the nearest-neighbour call. It is everything around
it: filtering without losing results, mixing a vector with words, reranking the
neighbours by citations and age, and aggregating over what came back.

## What it shows

| Step | Feature |
|---|---|
| 1 | `knn_vector` mapping, HNSW, `space_type`, `m` / `ef_construction` |
| 3 | the `knn` query with `k` |
| 4 | a filtered `knn` -- the filter applied *during* the search |
| 5 | `max_distance` -- a radius instead of a count |
| 6 | a `bool` mixing `match` and `knn` with boosts |
| 7 | `script_score` with `cosineSimilarity` -- exact, every document |
| 8 | `l2Squared`, `l1Norm`, `innerProduct` |
| 9 | `function_score` over a `knn` query: `field_value_factor` and `gauss` |
| 10 | aggregations over a vector result |
| 11 | `_plugins/_knn/warmup`, `_plugins/_knn/stats` |

## Running it

```bash
make serve      # a node configured for this example, port 9265, foreground
make run        # the example, in another terminal
```

`make check` validates the scripts and every request body without a server;
`make clean` deletes what the example left behind. Copy `.env.example` to
`.env` to change the address or anything else.

The longer form, and what this example needs of the node:

```bash
./target/release/velosearch &
examples/05-vector-search/run.sh
```

## What to look for

- **Step 4** is the one that matters in production. Ask for the 3 nearest
  *biology* papers with a computing query vector: the naive implementation
  fetches the 3 nearest overall (all computing) and then filters them away,
  leaving nothing. This applies the filter inside the graph walk, so three
  biology papers come back.
- **Step 7** scores every physics paper exactly, where step 3 walked a graph
  and may miss one. Exact is the right choice on a small candidate set --
  here, one `field`. The `+ 1.0` is there because cosine similarity runs to
  -1 and a score may not be negative.
- **Step 9** shows why "nearest" is rarely what a person wants. `v7` is the
  nearest to a query halfway between biology and computing, and with 800
  citations and a 2025 date it stays on top; drop the functions and the order
  is purely geometric.

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
| `requests/` | 8 request bodies, one file each |
| `data/` | 1 bulk document set |

## Leaves behind

The index `papers`. Rerunning deletes it first.
