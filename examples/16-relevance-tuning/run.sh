#!/usr/bin/env bash
# Making search better on purpose, and being able to prove it.
source "$(dirname "$0")/lib.sh"
IDX=docs

step "a documentation site's pages"
gone "/$IDX"
reqf PUT "/$IDX" requests/01-a-documentation-site-s-pagesx.json
green "$IDX"
ndjson "/$IDX/_bulk?refresh=wait_for" data/01-a-documentation-site-s-pages.ndjson
expect_docs "$IDX" 6 "documentation pages"

step "the baseline: a plain match, and what it gets wrong"
reqf GET "/$IDX/_search" requests/02-the-baseline-a-plain-match-and.json
note "the deprecated page mentions snapshot most often, so it wins on term frequency alone"

step "why -- the arithmetic of one document's score"
req GET "/$IDX/_explain/d3" '{ "query": { "match": { "body": "snapshot restore" } } }'

step "first fix: say what the fields are worth, and demote the deprecated"
reqf GET "/$IDX/_search" requests/03-first-fix-say-what-the-fields.json
note "boosting demotes rather than excludes -- the page is still findable if nothing else matches"

step "second fix: a phrase is worth more than the two words apart"
reqf GET "/$IDX/_search" requests/04-second-fix-a-phrase-is-worth.json

step "third fix: fresh and popular, decayed rather than added"
reqf GET "/$IDX/_search" requests/05-third-fix-fresh-and-popular-decayed.json

step "rescoring: cheap query over everything, expensive one over the top few"
reqf GET "/$IDX/_search" requests/06-rescoring-cheap-query-over-everything-expensive.json
note "the phrase query never runs over the whole index, only over the first 20 hits"

step "the same words asked four ways, side by side"
for t in best_fields most_fields cross_fields phrase; do
  note "--- $t"
  req GET "/$IDX/_search" "{
    \"size\": 3, \"_source\": [\"title\"],
    \"query\": { \"multi_match\": { \"query\": \"snapshot repository settings\",
        \"fields\": [\"title\", \"body\"], \"type\": \"$t\" } } }" \
    | python3 -c 'import json,sys; d=json.load(sys.stdin); print([(h["_source"]["title"], round(h["_score"],3)) for h in d["hits"]["hits"]])'
done

step "now prove it: a judgement list, and a score for each ranking"
note "three queries, with the documents a human said were right"
RANKEVAL='{
  "requests": [
    { "id": "snapshot_restore",
      "request": { "query": { "match": { "body": "snapshot restore" } } },
      "ratings": [ { "_index": "docs", "_id": "d1", "rating": 3 },
                   { "_index": "docs", "_id": "d4", "rating": 2 },
                   { "_index": "docs", "_id": "d2", "rating": 1 },
                   { "_index": "docs", "_id": "d3", "rating": 0 } ] },
    { "id": "repository",
      "request": { "query": { "match": { "body": "repository settings" } } },
      "ratings": [ { "_index": "docs", "_id": "d6", "rating": 3 },
                   { "_index": "docs", "_id": "d1", "rating": 1 } ] },
    { "id": "health",
      "request": { "query": { "match": { "body": "green yellow red" } } },
      "ratings": [ { "_index": "docs", "_id": "d5", "rating": 3 } ] }
  ],
  "metric": { "dcg": { "k": 5, "normalize": true } }
}'
note "--- the plain match"
req GET "/$IDX/_rank_eval" "$RANKEVAL"

note "--- the tuned query, same judgements"
TUNED='{
  "requests": [
    { "id": "snapshot_restore",
      "request": { "query": { "boosting": {
          "positive": { "multi_match": { "query": "snapshot restore", "fields": ["title^4", "body"] } },
          "negative": { "term": { "deprecated": true } }, "negative_boost": 0.15 } } },
      "ratings": [ { "_index": "docs", "_id": "d1", "rating": 3 },
                   { "_index": "docs", "_id": "d4", "rating": 2 },
                   { "_index": "docs", "_id": "d2", "rating": 1 },
                   { "_index": "docs", "_id": "d3", "rating": 0 } ] },
    { "id": "repository",
      "request": { "query": { "multi_match": { "query": "repository settings", "fields": ["title^4", "body"] } } },
      "ratings": [ { "_index": "docs", "_id": "d6", "rating": 3 },
                   { "_index": "docs", "_id": "d1", "rating": 1 } ] },
    { "id": "health",
      "request": { "query": { "multi_match": { "query": "green yellow red", "fields": ["title^4", "body"] } } },
      "ratings": [ { "_index": "docs", "_id": "d5", "rating": 3 } ] }
  ],
  "metric": { "dcg": { "k": 5, "normalize": true } }
}'
req GET "/$IDX/_rank_eval" "$TUNED"
note "the metric_score is the number to argue about, not anybody'\''s opinion of the first page"

step "the other metrics, on the tuned query"
for m in '"precision": { "k": 3, "relevant_rating_threshold": 2 }' '"recall": { "k": 5, "relevant_rating_threshold": 2 }' '"mean_reciprocal_rank": { "k": 5, "relevant_rating_threshold": 2 }' '"expected_reciprocal_rank": { "maximum_relevance": 3, "k": 5 }'; do
  note "--- ${m%%:*}"
  echo "$TUNED" | python3 -c "
import json, sys
d = json.load(sys.stdin)
d['metric'] = json.loads('{' + '''$m''' + '}')
print(json.dumps(d))" > /tmp/rankeval.json
  "${CURL[@]}" -X GET "$BS/$IDX/_rank_eval" -H 'Content-Type: application/json' --data-binary @/tmp/rankeval.json \
    | python3 -c 'import json,sys; d=json.load(sys.stdin); print("metric_score", round(d.get("metric_score",0),4))'
done

step "where the time actually goes"
req GET "/$IDX/_search?profile=true" '{
  "size": 3,
  "query": { "function_score": {
    "query": { "multi_match": { "query": "snapshot restore", "fields": ["title^4", "body"] } },
    "functions": [{ "gauss": { "updated": { "origin": "2026-09-13", "scale": "180d" } } }] } }
}' | python3 -c '
import json, sys
d = json.load(sys.stdin)
for s in d.get("profile", {}).get("shards", []):
    for q in s.get("searches", [{}])[0].get("query", []):
        print(f'"'"'{q.get("type","?"):<24} {q.get("time_in_nanos",0)/1e6:8.3f} ms  {q.get("description","")[:60]}'"'"')
' || true

step "the two similarities, measured"
req GET "/$IDX/_search" '{ "size": 6, "_source": ["title"], "query": { "match": { "title": "snapshot" } } }' \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print("title (b=0, length ignored): ", [(h["_source"]["title"][:28], round(h["_score"],3)) for h in d["hits"]["hits"]])'
req GET "/$IDX/_search" '{ "size": 6, "_source": ["title"], "query": { "match": { "body": "snapshot" } } }' \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print("body  (b=0.9, short wins):  ", [(h["_source"]["title"][:28], round(h["_score"],3)) for h in d["hits"]["hits"]])'

step "what this example leaves behind, checked rather than assumed"
done_
