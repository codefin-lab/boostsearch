# Troubleshooting

## `knn` query returns `illegal_argument_exception` or 400

Three settings must line up, and all are fixed at index creation:

```json
"settings": { "index": { "knn": true } }
"mappings": { "properties": { "embedding": {
  "type": "knn_vector", "dimension": 8, "method": { ... } } } }
```

- `index.knn` must be `true` -- without it there is no graph to walk;
- `dimension` must be declared, and must match every vector written;
- the query vector must have exactly that many elements.

A vector of the wrong length is refused at write time, so if documents are in
the index the mismatch is on the query side.

## The `knn` query returns fewer than `k` results

Expected, and not a bug, in two cases: there are fewer than `k` documents, or
a `filter` excludes them. If neither applies, the graph walk is terminating
early -- raise `ef_search` if the engine exposes it, or use the exact
`script_score` form (step 7) to find out what the true neighbours are.

## Step 4 returns nothing

If a filtered k-NN returns empty while the same filter alone matches
documents, the filter is being applied *after* the search rather than during
it. That is the failure the step exists to demonstrate -- see `design.md`. Ask
the filter on its own to confirm the documents are there:

```bash
curl -s localhost:9265/papers/_search -H 'content-type: application/json' \
  -d '{"query":{"term":{"field":"biology"}}}' | jq '.hits.total'
```

## `cosineSimilarity` in a script returns an error about doc values

`doc["embedding"]` in a script reads the raw vector, which requires the field
to be a `knn_vector`. A vector stored as an array of `float` is not the same
thing and the function will not take it.

## Scores from `script_score` look wrong -- everything is around 100

The scripts add a constant (`+ 1.0`, `+ 100`) because a score may not be
negative and `l2Squared`/`innerProduct` can be. Subtract the constant to read
the real value. Step 8 says which constant it used.

## `_plugins/_knn/stats` returns 404

The k-NN plugin is not answered for by this node. The `knn` query and
`script_score` may still work; check:

```bash
curl -s 'localhost:9265/_cat/plugins?v'
```

## Recall seems poor -- a document you know is near is missing

Approximate search trades recall for speed. To find out whether it is the graph
or your expectation, score every document exactly:

```bash
curl -s localhost:9265/papers/_search -H 'content-type: application/json' -d '{
  "size": 20, "_source": ["title"],
  "query": {"script_score": {"query": {"match_all": {}},
    "script": {"source": "cosineSimilarity(params.q, doc[\"embedding\"]) + 1.0",
               "params": {"q": [0,0,1,1,0,0,0,0]}}}}}' | jq '.hits.hits[] | [._source.title, ._score]'
```

If the missing document is genuinely near by that measure, raise `m` and
`ef_construction` and reindex.

## Cleaning up

```bash
make clean          # deletes papers
```
