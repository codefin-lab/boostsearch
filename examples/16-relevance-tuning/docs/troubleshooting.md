# Troubleshooting

## `_rank_eval` returns 404

Rank evaluation is not answered for by this node:

```bash
curl -s 'localhost:9276/_cat/plugins?v'
```

Everything up to step 8 still works; the proof at steps 9 and 10 does not.

## `metric_score` is 0 for every request

The ratings did not match any returned document. Ratings identify documents by
`_index` **and** `_id`, and both must match exactly:

```json
{ "_index": "docs", "_id": "d1", "rating": 3 }
```

An `_index` of `docs*` or an alias name will not match. Check what the query
actually returns:

```bash
curl -s localhost:9276/docs/_search -H 'content-type: application/json' \
  -d '{"query":{"match":{"body":"snapshot restore"}}}' | jq '.hits.hits[]._id'
```

## The tuned query scores *lower* than the baseline

Useful information, and the reason for measuring. Three things to check before
concluding the tuning is bad:

- **`k` is too small.** nDCG@5 on a query with two rated documents is
  dominated by noise;
- **the judgements encode the old behaviour.** If they were written by looking
  at the current results, they will favour the current ranking by construction;
- **the change helped one query and hurt two.** `_rank_eval` returns per-request
  `details`; read those, not only the aggregate.

## `_explain` output is hard to read

It is a nested tree of `value` / `description` / `details`. The useful parts are
the leaves. Flatten it:

```bash
curl -s localhost:9276/docs/_explain/d3 -H 'content-type: application/json' \
  -d '{"query":{"match":{"body":"snapshot restore"}}}' \
  | jq -r '.. | objects | select(.description) | "\(.value)\t\(.description)"' | head -20
```

## `boosting` has no effect

`negative_boost` must be between 0 and 1 -- it is a multiplier, not a
subtraction. A value of 1 does nothing; a value above 1 promotes.

Also check the `negative` clause actually matches. `{"term": {"deprecated":
true}}` needs `deprecated` to be a `boolean` in the mapping; if it is a
`keyword`, the term is the string `"true"` and the boolean does not match.

## `rescore` seems to do nothing

Three possibilities:

- `window_size` is smaller than `size`, so there is nothing above the window to
  reorder;
- the rescore query matches the same documents in the same order;
- `query_weight` and `rescore_query_weight` sum such that the original score
  dominates. Step 7 uses 0.6 and 2.0.

Rescore does not change which documents are returned, only their order within
the window. If a document is missing entirely, the problem is in the main
query.

## `similarity` on a field has no effect

Similarity is fixed at index creation, like analysis. Changing it needs a new
index. Check it took:

```bash
curl -s localhost:9276/docs/_mapping | jq '.docs.mappings.properties.title.similarity'
```

## The profile output in step 11 is empty

The `jq`-free Python summary depends on the profile shape. Read it raw:

```bash
curl -s 'localhost:9276/docs/_search?profile=true' -H 'content-type: application/json' \
  -d '{"query":{"match":{"body":"snapshot"}}}' | jq '.profile'
```

## Cleaning up

```bash
make clean          # deletes docs
```
