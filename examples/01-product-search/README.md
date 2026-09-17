# 1. A product search that has to be good

A shop's search box is judged on what it puts in the first three rows, not on
whether it matched. This example builds the whole path from a customer's
typing to those three rows: an analysis chain that knows the shop's own words,
a query that forgives a typo, a score bent towards what actually sells, one
row per brand, and the suggestions shown while the customer is still typing.

## What it shows

| Step | Feature |
|---|---|
| 1 | custom analysis: `mapping` char filter, `synonym_graph`, `stemmer`, `edge_ngram` |
| 1 | `scaled_float`, `half_float`, `keyword` sub-fields, `completion` |
| 2 | `_bulk` with `refresh=wait_for` |
| 3 | synonyms at search time, both directions |
| 4 | `_analyze` against a named analyser -- what the chain really produced |
| 5 | `multi_match` with field boosts and `fuzziness: AUTO`, `highlight` |
| 6 | `function_score`: `field_value_factor`, a filtered `weight`, `score_mode`/`boost_mode` |
| 7 | `_explain` -- the arithmetic behind one document's score |
| 8 | `collapse` with `inner_hits` |
| 9 | the completion suggester |
| 10 | the term suggester ("did you mean") |
| 11-12 | a stored Mustache search template, and `_render/template` |
| 13 | `_msearch` -- four widgets, one round trip |
| 14 | `profile=true` |

## Running it

```bash
make serve      # a node configured for this example, port 9261, foreground
make run        # the example, in another terminal
```

`make check` validates the scripts and every request body without a server;
`make clean` deletes what the example left behind. Copy `.env.example` to
`.env` to change the address or anything else.

The longer form, and what this example needs of the node:

```bash
./target/release/velosearch &
examples/01-product-search/run.sh
```

`VS` sets the address if the server is not on `http://127.0.0.1:9200`.

## What to look for

- **Step 3** returns the Aurora notebooks for the word *laptop*, which appears
  in neither of their names. The synonym graph is applied at index time here,
  so the match is on the token, not on a rewritten query.
- **Step 5** finds Meridian from *meridan*: one edit, which `AUTO` allows at
  that term length. The highlight shows which field earned the hit.
- **Step 6** moves the cheap earphones -- 1,500 sold, but out of stock -- below
  the soundbar, because the `in_stock: false` filter multiplies its score by
  0.2. Change that weight to 1.0 and rerun to see the order change.
- **Step 8** returns one row per brand with the two cheapest of each underneath,
  which is the shape a "3 brands, expand for more" listing wants.

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
| `requests/` | 9 request bodies, one file each |
| `data/` | 2 bulk document sets |

## Leaves behind

The index `shop-products` and the stored script `product-search`. Rerunning
deletes both first.
