# API surface -- 5. Vector search that is not only vector search

Every request this example makes, in the order it makes them. Generated
from `run.sh`; if the two disagree, `run.sh` is right.

| Step | Method | Path | Body |
|---|---|---|---|
| an index with a vector field, HNSW over cosine | `PUT` | `/$IDX` | `requests/01-an-index-with-a-vector-field.json` |
| papers, with hand-made embeddings so the geometry is readable | `POST` | `/$IDX/_bulk?refresh=wait_for` | `data/01-papers-with-hand-made-embeddings-so.ndjson` |
| the three nearest papers to a query about computing | `GET` | `/$IDX/_search` | `requests/02-the-three-nearest-papers-to-a.json` |
| the nearest three that are ALSO biology -- three, not three minus the others | `GET` | `/$IDX/_search` | `requests/03-the-nearest-three-that-are-also.json` |
| everything within a radius, however many that is | `GET` | `/$IDX/_search` | `requests/04-everything-within-a-radius-however-many.json` |
| hybrid: the words and the vector, each contributing | `GET` | `/$IDX/_search` | `requests/05-hybrid-the-words-and-the-vector.json` |
| exact, not approximate: score every document by a distance you choose | `GET` | `/$IDX/_search` | `requests/06-exact-not-approximate-score-every-document.json` |
| the other measures, side by side on the same query vector | `GET` | `/$IDX/_search` | inline |
| near, and well cited, and recent -- rerank the neighbours | `GET` | `/$IDX/_search` | `requests/07-near-and-well-cited-and-recent.json` |
| an aggregation over what the vectors found | `GET` | `/$IDX/_search` | `requests/08-an-aggregation-over-what-the-vectors.json` |
| the graph in memory, and what the plugin reports about it | `POST` | `/_plugins/_knn/warmup/$IDX` | inline |
|  | `GET` | `/_plugins/_knn/stats` | inline |

## Endpoints touched

Path parameters and query strings removed, deduplicated:

- `/<var>`
- `/<var>/_bulk`
- `/<var>/_search`
- `/_plugins/_knn/stats`
- `/_plugins/_knn/warmup/<var>`

## Request bodies

- [`requests/01-an-index-with-a-vector-field.json`](../requests/01-an-index-with-a-vector-field.json)
- [`requests/02-the-three-nearest-papers-to-a.json`](../requests/02-the-three-nearest-papers-to-a.json)
- [`requests/03-the-nearest-three-that-are-also.json`](../requests/03-the-nearest-three-that-are-also.json)
- [`requests/04-everything-within-a-radius-however-many.json`](../requests/04-everything-within-a-radius-however-many.json)
- [`requests/05-hybrid-the-words-and-the-vector.json`](../requests/05-hybrid-the-words-and-the-vector.json)
- [`requests/06-exact-not-approximate-score-every-document.json`](../requests/06-exact-not-approximate-score-every-document.json)
- [`requests/07-near-and-well-cited-and-recent.json`](../requests/07-near-and-well-cited-and-recent.json)
- [`requests/08-an-aggregation-over-what-the-vectors.json`](../requests/08-an-aggregation-over-what-the-vectors.json)
- [`data/01-papers-with-hand-made-embeddings-so.ndjson`](../data/01-papers-with-hand-made-embeddings-so.ndjson)
