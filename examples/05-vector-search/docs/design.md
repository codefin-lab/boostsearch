# Design notes

## Why the embeddings are hand-made

Eight dimensions, in three pairs: 0-1 mean "about biology", 2-3 "about
computing", 4-5 "about physics", 6-7 are noise. A real index puts a model's
output in that field, where the dimensions mean nothing a person can name.

Hand-made numbers are used here so every answer can be checked by eye. When
step 4 returns three biology papers for a computing query vector, you can read
the vectors and confirm that is genuinely what "nearest biology" means, rather
than trusting that the filter did something. An example built on real
embeddings can only be checked by believing it.

## `cosinesimil`, and when it is the wrong space

| Space | Measures | Ignores |
|---|---|---|
| `cosinesimil` | the angle between vectors | how long they are |
| `l2` | straight-line distance | nothing |
| `innerproduct` | projection, length included | -- |

Text embeddings are usually compared by cosine, because a longer document
produces a longer vector and length is not relevance. That is why this example
uses it, and why step 7's `script_score` adds 1.0: cosine runs from -1 to 1 and
a score may not be negative.

`l2` is right when the magnitudes carry meaning -- coordinates, measurements,
anything where "twice as far along this axis" is a real statement.

## Filtered k-NN, and the naive version that returns nothing

This is the step that matters in production.

Ask for the 3 nearest *biology* papers using a computing query vector. The
naive implementation is:

1. ask the graph for the 3 nearest overall -- all three are computing papers;
2. apply the filter -- nothing is left;
3. return an empty result.

The engine has done exactly what it was told and the answer is useless. The
fix is to apply the filter inside the graph walk, so the search keeps
descending until it has three that pass. That is what `filter` inside the `knn`
query does, and step 4 is there so the difference is visible.

The cost is real: a very selective filter makes the walk long, and at some
selectivity an exact scan over the filtered set (step 7's `script_score`) is
cheaper than an approximate walk that has to reject almost everything. There is
no universal crossover point; measure it on your own data.

## Approximate against exact

| | `knn` query | `script_score` with `cosineSimilarity` |
|---|---|---|
| Visits | a graph, a fraction of documents | every matching document |
| Cost | sublinear in index size | linear |
| Correct | almost always | always |
| Needs | `index.knn: true`, an HNSW method | nothing but the field |

Step 7 scores every physics paper exactly, which is three documents. On a
candidate set that small, exact is both cheaper and right. Reach for the graph
when the candidate set is large; reach for `script_score` when a filter has
already made it small.

## `m` and `ef_construction`

```json
"parameters": { "m": 16, "ef_construction": 128 }
```

`m` is how many neighbours each node keeps -- higher means a better-connected
graph, more memory, better recall. `ef_construction` is how hard the build
works to find those neighbours -- higher means a slower build and better
recall, and costs nothing at query time.

16 and 128 are ordinary defaults. They matter not at all on seven documents
and a great deal on seven million; the reason to write them explicitly here is
that they are fixed at index creation and changing them means reindexing.

## Why reranking is a separate step

Step 9 wraps the `knn` query in a `function_score` with citations and age.
"Nearest" is a statement about geometry and almost never what a person wants:
the nearest paper to a query might be an unread preprint from 2013.

The shape -- retrieve by vector, rerank by everything else -- is the standard
one, and the reason to keep them separate is that the retrieval decides
*recall* (did the right document come back at all) and the rerank decides
*precision* (is it near the top). Mixing them makes a recall failure look like
a ranking failure.

## What would change at scale

- **HNSW lives in memory.** The graph is not paged in from disk on demand the
  way an inverted index is; the index must fit, and `_plugins/_knn/stats`
  (step 11) is where you find out whether it does.
- **`k` is per shard.** Asking for `k: 10` on a five-shard index visits ten per
  shard and the coordinator keeps the best ten. That is more work than it
  looks, and a reason to keep vector indices on few shards.
- **Dimension is fixed at mapping time**, like every other mapping decision.
  Changing the model means reindexing (example 14).
