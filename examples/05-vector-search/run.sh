#!/usr/bin/env bash
# Vectors: nearest neighbours, filtered, and mixed with ordinary search.
source "$(dirname "$0")/lib.sh"
IDX=papers

step "an index with a vector field, HNSW over cosine"
gone "/$IDX"
reqf PUT "/$IDX" requests/01-an-index-with-a-vector-field.json
green "$IDX"

step "papers, with hand-made embeddings so the geometry is readable"
note "dimensions 0-1 are 'about biology', 2-3 'about computing', 4-5 'about physics'"
ndjson "/$IDX/_bulk?refresh=wait_for" data/01-papers-with-hand-made-embeddings-so.ndjson
expect_docs "$IDX" 7 "papers"

step "the three nearest papers to a query about computing"
reqf GET "/$IDX/_search" requests/02-the-three-nearest-papers-to-a.json

step "the nearest three that are ALSO biology -- three, not three minus the others"
reqf GET "/$IDX/_search" requests/03-the-nearest-three-that-are-also.json
note "an unfiltered search then thrown away would have returned nothing here"

step "everything within a radius, however many that is"
reqf GET "/$IDX/_search" requests/04-everything-within-a-radius-however-many.json

step "hybrid: the words and the vector, each contributing"
reqf GET "/$IDX/_search" requests/05-hybrid-the-words-and-the-vector.json

step "exact, not approximate: score every document by a distance you choose"
reqf GET "/$IDX/_search" requests/06-exact-not-approximate-score-every-document.json

step "the other measures, side by side on the same query vector"
for m in "l2Squared(params.q, doc['embedding'])" "l1Norm(params.q, doc['embedding'])" "innerProduct(params.q, doc['embedding'])"; do
  note "--- $m"
  req GET "/$IDX/_search" "{
    \"size\": 3, \"_source\": [\"title\"],
    \"query\": { \"script_score\": { \"query\": { \"match_all\": {} },
        \"script\": { \"source\": \"$m + 100\", \"params\": { \"q\": [0.0,0.0,1.0,1.0,0.0,0.0,0.0,0.0] } } } }
  }"
done
note "subtract 100 from each score: the offset is there because a score may not be negative"

step "near, and well cited, and recent -- rerank the neighbours"
reqf GET "/$IDX/_search" requests/07-near-and-well-cited-and-recent.json

step "an aggregation over what the vectors found"
reqf GET "/$IDX/_search" requests/08-an-aggregation-over-what-the-vectors.json

step "the graph in memory, and what the plugin reports about it"
quiet GET "/_plugins/_knn/warmup/$IDX"
req GET "/_plugins/_knn/stats"

step "what this example leaves behind, checked rather than assumed"
done_
