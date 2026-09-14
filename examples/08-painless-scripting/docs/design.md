# Design notes

## The one thing to learn: `doc` against `ctx`

Painless runs in about a dozen contexts and they divide cleanly in two.

| | `doc['field']` | `ctx._source.field` |
|---|---|---|
| Reads | the column store (doc values) | the stored JSON of the document |
| Speed | fast, no parsing | slow, parses the source |
| Writable | no | yes |
| Available in | search: script fields, queries, sorts, scores, aggregations | writes: `_update`, `_update_by_query`, `_reindex`, ingest |
| Sees | the *indexed* value, analysed and typed | exactly what was written |

The differences bite in specific ways:

- `doc['tags']` on a multi-valued field gives every value, sorted and
  deduplicated, because that is how doc values store it. `ctx._source.tags`
  gives the original array, in order, with duplicates.
- `doc['text_field']` usually fails or gives analysed tokens; text fields have
  no doc values by default. Scripts read `keyword` fields.
- A field missing from a document is an empty `doc[...]` (check `.size() == 0`)
  and a `null` in `ctx._source`.

If a script works at search time and fails at write time, or the reverse, this
table is nearly always the reason.

## Why `params` and not string interpolation

Every script in this example takes its variable parts through `params`:

```json
"source": "doc['km'].value - doc['serviced_km'].value > params.overdue",
"params": { "overdue": 20000 }
```

Scripts are compiled and the compiled form is cached by the exact text of the
source. Build the number into the text and every distinct threshold compiles a
new script, the cache fills, and the engine starts refusing to compile (there
is a rate limit, and hitting it is a production incident that looks like random
query failures).

Same text, different `params`: one compile, forever.

## `scripted_metric`, and why it has four scripts

It is a map-reduce, and the four parts are the four phases:

```
init_script     once per shard    set up state
map_script      once per document add to state
combine_script  once per shard    return what this shard has
reduce_script   once, on the      merge the shards' returns
                coordinator
```

The split is not decoration. `state` exists per shard and cannot be shared;
`states` in the reduce is the list of what each shard's combine returned. A
reduce that assumes one shard works on a one-shard index and returns a fraction
of the answer on three.

The rule of thumb: reach for `scripted_metric` only when no combination of the
built-in aggregations gives the shape you need. It is the slowest thing in the
aggregation framework, by a lot, because it runs interpreted code per document
where the built-ins run specialised collectors.

## Why the depreciation formula is a stored script

Step 11 stores it once and calls it from both a `script_field` and a `_script`
sort. That is not only tidiness: the two call sites would otherwise hold two
copies of the same arithmetic, which will drift, and each would compile
separately.

Stored scripts are also the only form some deployments allow -- inline scripts
can be disabled per context, so a cluster may accept a stored script where it
refuses the identical inline one.

## Lucene expressions, and why they are still here

Step 12 computes `doc['litres_per_100km'].value * 35` in `expression` rather
than Painless. Expressions are numeric-only and compile to bytecode; for pure
arithmetic on numeric fields they are faster than Painless and cannot do
anything else -- no strings, no conditionals, no `_source`.

They still read their fields through `doc[...]`: a bare field name is a link
error, in this engine and in the reference alike. An earlier draft of this
example wrote `litres_per_100km * 35` and was refused by both.

For `a * b`, use them. For anything with an `if`, do not.

## Dates, and why "now" is a parameter

```json
"source": "ChronoUnit.DAYS.between(doc['bought'].value, ZonedDateTime.parse(params.now)) / 365.0",
"params": { "now": "2026-09-13T00:00:00Z" }
```

Reading the clock inside a script makes the answer irreproducible and defeats
any caching the engine might do. Passing the time in makes the query a pure
function of its inputs, which means it can be tested, cached, and replayed
against yesterday.

## What would change at scale

- **A script runs per document per request.** Step 4's `_script` sort executes
  for every matching document, not just the returned page. Behind a `rescore`
  window it runs for the window only.
- **`_update_by_query` is not transactional.** It reads and writes document by
  document; `conflicts: proceed` is what keeps it going past a document
  something else changed, and the count of conflicts in the answer is a number
  worth looking at.
- **Prefer a computed field at write time** (example 7's `script` processor)
  over the same computation in every query. Write once, read many.
