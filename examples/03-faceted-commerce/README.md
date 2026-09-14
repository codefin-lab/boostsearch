# 3. A faceted listing page, done properly

Two things go wrong on nearly every faceted search, and both are shown here
next to their fix.

The first is nested matching: a shoe with a size 42 that is out of stock and a
size 44 that is in stock is *not* a shoe you can buy in 42, but two separate
`nested` clauses say it is. The second is facet counting: if the colour facet
is computed after the colour filter, every colour but the chosen one reads
zero, and the user can never click anything else.

## What it shows

| Step | Feature |
|---|---|
| 1 | `nested` mapping, `scaled_float`, `keyword` sub-field |
| 3 vs 4 | two `nested` clauses versus one -- the classic wrong answer, then the right one |
| 4 | `inner_hits`: which variant matched |
| 5 | `post_filter` -- narrow the hits, leave the facets counting |
| 6 | `global`, `filter` and `filters` aggregations side by side |
| 7 | `nested` aggregation, `filter` inside it, `range` bands, `reverse_nested` |
| 8 | sorting on a nested field with `mode` and a `nested.filter` |
| 9 | `search_after` with a tiebreak, `track_total_hits: false` |
| 10 | `_count` |
| 11 | `_field_caps` |

## Running it

```bash
make serve      # a node configured for this example, port 9263, foreground
make run        # the example, in another terminal
```

`make check` validates the scripts and every request body without a server;
`make clean` deletes what the example left behind. Copy `.env.example` to
`.env` to change the address or anything else.

The longer form, and what this example needs of the node:

```bash
./target/release/boostsearch &
examples/03-faceted-commerce/run.sh
```

## What to look for

- **Step 3** returns Trail Runner GTX. **Step 4** does not, and its `inner_hits`
  name the exact SKU that matched. Compare the two answers before anything
  else in this example.
- **Step 5** filters the hits to black but the `colours` facet still reports
  brown, because `post_filter` runs after the aggregations. Move that clause
  into the `query` and rerun: the facet collapses to one row, and the page
  becomes a dead end.
- **Step 7** shows the two directions of a nested aggregation. Inside the
  `nested` block the buckets count *variants*; `reverse_nested` climbs back out
  and counts *products*, which is the number a "12 products" heading wants.
- **Step 8** sorts by the cheapest variant a customer could actually buy. Drop
  the `nested.filter` and the out-of-stock prices come back into the sort.

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

The index `catalogue`. Rerunning deletes it first.
