# Design notes

## The two bugs this example exists to show

Both are bugs of *shape*, not of syntax. Both queries are valid, both return
results, and both are wrong in a way that testing with three products will
never reveal.

### One `nested` clause, not two

```json
"filter": [
  { "nested": { "path": "variants", "query": { "term": { "variants.size": "42" } } } },
  { "nested": { "path": "variants", "query": { "range": { "variants.stock": { "gt": 0 } } } } }
]
```

This says: *some* variant is size 42, and *some* variant is in stock. A shoe
whose 42 is sold out and whose 44 is in stock satisfies both. The customer
clicks through to a product they cannot buy in their size.

The fix is one `nested` clause with a `bool` inside it, so both conditions are
evaluated against the same child document:

```json
"query": { "nested": { "path": "variants",
  "query": { "bool": { "filter": [ {size}, {stock} ] } } } }
```

The general rule: **one `nested` block is one child document.** Two blocks are
two independent existence tests.

### Facets counted before the filter, not after

If the colour filter is inside `query`, the colour facet counts only the chosen
colour and every other row reads zero. The page becomes a dead end: the user
can narrow but never widen.

`post_filter` runs after the aggregations have collected, so the hits are
narrowed and the facets are not.

## Why `post_filter` and not one `global` aggregation per facet

Both work. The difference is what they cost and what they mean.

| | `post_filter` | `global` + `filter` sub-agg |
|---|---|---|
| Facets see | the query, minus the post-filtered clause | whatever you tell each one |
| Cost | one collection pass | one pass per differently-scoped facet |
| Good for | one "active" facet dimension | facets with different scopes |

Step 6 shows the `global`/`filter`/`filters` approach beside it, because a real
listing page usually needs both: the colour facet excludes the colour filter,
while a "12 results" heading counts everything the query matched.

## Why `variants` is `nested` rather than an object or a child

| Model | Cost of a variant update | Can match one variant | Index size |
|---|---|---|---|
| plain object | one product rewritten | **no** -- fields are flattened | smallest |
| `nested` | one product rewritten | yes | product + one doc per variant |
| `join` | one variant rewritten | yes | separate documents, needs routing |

A plain object is out immediately: `{size: 42}` and `{stock: 0}` on different
variants become indistinguishable, which is exactly bug one made unfixable.

Between `nested` and `join`: variants change when stock changes, which on a
busy shop is constantly, and `nested` rewrites the whole product each time.
This example uses `nested` because a shoe has three variants and the rewrite is
free; example 11 shows where that stops being true.

## `reverse_nested`, and the number a heading wants

Inside a `nested` aggregation the unit of counting is the child. `sizes` in
step 7 counts *variants* -- correct for a size facet, since a size facet counts
sizes. But "12 products" is a count of parents, and `reverse_nested` is how you
climb back out to get it. Miss this and the heading says 34 because that is how
many variants matched.

## Why the sort has a `nested.filter`

```json
"sort": [{ "variants.price": { "mode": "min",
  "nested": { "path": "variants", "filter": { "range": { "variants.stock": { "gt": 0 } } } } } }]
```

Sorting a parent by a child field needs two decisions: *which* children count
(`filter`) and *how* to reduce several values to one (`mode`). Leave out the
filter and a product sorts by a price nobody can pay. Leave out `mode` and the
default -- `min` for `asc` -- is usually right and occasionally not.

## What would change at scale

- **`track_total_hits: false`** (step 9) is the right default for a listing
  page past page one; the total is already on the screen from page one.
- **A nested document is a Lucene document.** A catalogue of 100k products with
  20 variants each is 2.1M documents, not 100k, and every index-size estimate
  has to use the larger number.
- **`terms` facets need `size` tuned.** The default of 10 is fine for colours
  and wrong for brands; and on multiple shards a `terms` count is approximate
  unless `shard_size` is raised.
