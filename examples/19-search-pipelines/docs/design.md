# Design notes

## Why the client never changes

Every search in `run.sh` that stands for the shop app sends
`requests/02-what-the-app-sends.json`, byte for byte. That is the point of the
example: a search pipeline is how a server-side team changes behaviour that is
otherwise baked into clients it cannot redeploy -- mobile apps in the field,
a partner's integration, a dashboard nobody owns any more.

The alternative is to change every client, or to put a proxy in front of the
index that rewrites JSON. The pipeline is that proxy, run inside the search
path, with the same error handling and the same view of the index.

## Request processors, response processors, and what each may touch

| | Runs | Sees | Can change |
|---|---|---|---|
| `filter_query` | before the search | the request body | the query, by adding a filter |
| `script` | before the search | `ctx._source`: `from`, `size`, `explain`, `min_score`, ... | those values |
| `oversample` | before the search | `size` | `size`, and the request context |
| `rename_field` | after | each hit's `_source` | a field's name |
| `sort` | after | an array field in each hit | the order of its values |
| `collapse` | after | the hits, in order | which hits are kept |
| `truncate_hits` | after | the hits, and the request context | how many are kept |

A response processor only sees the page that came back, never the index. That
matters twice in this example:

- `collapse` in a pipeline is not the `collapse` search option. It deduplicates
  the hits it is given, so on its own it would turn a page of four into a page
  of two. That is why step 7 puts `oversample` in front of it -- ask for three
  times as many, dedupe, then `truncate_hits` back to what the client wanted.
  With a sample factor of 3 a carousel of 4 is safe as long as no page of 12
  has fewer than 4 authors in it. Pick the factor from the data.
- `rename_field` renames what is in `_source` after source filtering. A
  client that asks for `"_source": ["price"]` gets nothing to rename, because
  `price` does not exist in the index; the processor then fails the search
  (step 11 shows what that looks like). The app in this example asks for the
  whole document, which is what old clients usually do.

`truncate_hits` with no `target_size` reads the number `oversample` wrote into
the request context. Both take a `context_prefix`, for pipelines that
oversample twice for different reasons.

## Why `filter_query` does not change the scores

`filter_query` rewrites the query as

```json
{ "bool": { "must": [ <the app's query> ], "filter": [ <the pipeline's query> ] } }
```

A `filter` clause decides membership and adds nothing to the score, so the
order within the in-stock books is the order the app's query gave them.
Step 4 shows the same scores as step 2 with four rows missing. It also changes
`hits.total`, which a response processor that dropped the hits after the fact
could not: 8, not 12 with four missing.

## Three ways to pick a pipeline, and which one wins

| Where | Scope | Used for |
|---|---|---|
| `index.search.default_pipeline` | every search on the index | the behaviour clients should get without asking |
| `?search_pipeline=name` | one request | a variant (`one-per-author`), or `_none` to opt out |
| `"search_pipeline": { ... }` in the body | one request, not stored | trying a processor before storing it |

A named or inline pipeline replaces the default; they are never chained.
Step 10 is there to show it: the inline pipeline filters to fiction and the
sold-out fiction comes back, which it could not if `storefront` had also run.
`_none` (step 9) is how an administrator or a back-office tool sees the index
as it is, and it is worth giving such tools that parameter from the start.

Setting a default pipeline is a settings change, so it takes effect for every
client at once and can be undone the same way. That makes it the right place
for rules like "sold-out books are not results", and the wrong place for
experiments.

## Why step 5 writes the pipeline again rather than editing it

There is no partial update: a `PUT` to an existing name replaces the whole
definition, and the next search uses the new one. `version` is not checked by
the server; it is there for the people who read the pipeline later. Keep
pipeline definitions in files under version control, as this example does, so
"what was the storefront pipeline last Tuesday" has an answer.

## `ignore_failure`, and why it is not the fix for step 11

`ignore_failure` on a processor means "if this raises, go on as if it were not
there". Step 12 uses it on a rename of a field that no document has any more,
and the processor after it still runs.

It is the blunt tool. For `rename_field` the precise one is
`ignore_missing: true`, which tolerates a missing field but still fails on
anything else. Use `ignore_failure` for a processor whose failure is
genuinely harmless -- a cosmetic reordering, say -- and never on the
`filter_query` that keeps sold-out or private documents out of results: a
silently skipped filter is a data leak, not a degraded answer.

## What is not here, and why

**The `hybrid` query and the `normalization-processor`.** In OpenSearch with
the neural-search plugin, a pipeline with

```json
"phase_results_processors": [ { "normalization-processor": {
  "normalization": { "technique": "min_max" },
  "combination": { "technique": "arithmetic_mean", "parameters": { "weights": [0.3, 0.7] } } } } ]
```

combines the scores of the sub-queries of a `hybrid` query -- typically a
`match` and a `knn` -- after rescaling each to a common range (`min_max` or
`l2` or `z_score`), or ranks them by reciprocal rank fusion with a
`score-ranker-processor`. VeloSearch runs both the way the plugin does: each
shard collects each sub-query's best documents, and the pipeline scales and
combines them before a page is taken. One detail differs: a sub-query's scores
are computed with the term statistics of the whole index, where OpenSearch
scores each shard's list with that shard's own statistics, so on a small index
with several shards the scaled scores can differ slightly. The example leaves hybrid search out
because the catalogue has only one sensible way to score a title; example 05
mixes words and a vector, which is where a hybrid query earns its keep.

**The `split` response processor.** It turns a delimited string into a list
(`"csv": "x,y,z"` becomes `["x", "y", "z"]`); nothing in the catalogue is
stored that way.

## What would change at scale

- **`oversample` multiplies the cost of every search it is on.** A factor of 3
  fetches three times the hits, on every shard, before the response processors
  throw two thirds away. Keep it on the pipelines that need it, not on the
  index default.
- **Response processors run on the coordinating node over the merged page.**
  They are cheap for a page of 20 and not for `size: 10000`; a `script`
  request processor that caps `size`, as `storefront` does, protects them too.
- **A default pipeline is a property of one index.** OpenSearch applies it
  only to a search that resolves to a single index; this server also applies
  it across several indices when those that have a default agree on it. A
  search over an alias or a pattern is where the two part company, so do not
  rely on the default there -- name the pipeline on the request instead.
- **Filters in a pipeline are not security.** Anyone who can pass
  `search_pipeline=_none` gets around them. Rules that must hold for every
  reader belong in document-level security (example 06), not here.
