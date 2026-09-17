#!/usr/bin/env bash
# Changing a mapping on a live index, without anyone noticing.
source "$(dirname "$0")/lib.sh"

step "the index as it was written in a hurry, two years ago"
gone "/people-v1"; gone "/people-v2"; gone "/people-v3"; gone "/people-small"
reqf PUT "/people-v1" requests/01-the-index-as-it-was-written.json
green "people-v1"
python3 - <<'PY' > /tmp/people.ndjson
import json, random
random.seed(5)
first = ["Somchai", "Pim", "Nok", "Dee", "Anan", "Mali", "Krit", "Ploy"]
last = ["Sukhum", "Chai", "Wong", "Tan", "Rat", "Boon"]
ctry = ["Thailand", "Singapore", "Japan", "Viet Nam"]
out = []
for i in range(2000):
    out.append(json.dumps({"index": {"_id": f"p{i}"}}))
    out.append(json.dumps({
        "name": f"{random.choice(first)} {random.choice(last)}",
        "country": random.choice(ctry),
        "born": f"{random.randint(1960, 2010)}-0{random.randint(1,9)}-1{random.randint(0,9)}",
        "score": str(random.randint(0, 100)),
        "tags": ", ".join(random.sample(["alumni", "staff", "board", "donor", "volunteer"], 2)),
    }))
print("\n".join(out))
PY
ndjson "/people-v1/_bulk?refresh=true" /tmp/people.ndjson > /dev/null
expect_docs people-v1 2000 "the badly-typed original"
req GET "/_cat/count/people-v1?v"

step "what is wrong with it: you cannot aggregate, sort or range over any of this"
req GET "/people-v1/_search" '{ "size": 0, "aggs": { "by_country": { "terms": { "field": "country" } } } }' || true
note "text fields have no column store, so a terms aggregation on one is refused"

step "the alias that should have been there from the start"
req POST "/_aliases" '{ "actions": [{ "add": { "index": "people-v1", "alias": "people" } }] }'
req GET "/_cat/aliases/people?v"
note "from here on every client talks to 'people' and never to an index name"

step "the index as it should be"
req PUT "/people-v2" '{
  "settings": { "number_of_shards": 2, "number_of_replicas": 0 },
  "mappings": { "properties": {
    "name":    { "type": "text", "fields": { "raw": { "type": "keyword" } } },
    "country": { "type": "keyword" },
    "born":    { "type": "date", "format": "yyyy-MM-dd" },
    "age":     { "type": "integer" },
    "score":   { "type": "integer" },
    "tags":    { "type": "keyword" },
    "migrated_at": { "type": "date" }
  }}
}'

step "a pipeline that fixes what the mapping alone cannot"
gone "/_ingest/pipeline/people-fix"
req PUT "/_ingest/pipeline/people-fix" '{
  "processors": [
    { "split": { "field": "tags", "separator": ",\\s*" } },
    { "trim": { "field": "tags" } },
    { "convert": { "field": "score", "type": "integer", "on_failure": [{ "set": { "field": "score", "value": 0 } }] } },
    { "script": { "lang": "painless", "source":
        "ctx.age = 2026 - Integer.parseInt(ctx.born.substring(0, 4));" } },
    { "set": { "field": "migrated_at", "value": "{{_ingest.timestamp}}" } }
  ]
}'
req POST "/_ingest/pipeline/people-fix/_simulate" '{
  "docs": [{ "_source": { "name": "Dee Wong", "country": "Thailand", "born": "1994-03-11", "score": "77", "tags": "staff, donor" } }]
}'

step "the reindex itself -- sliced, so it uses more than one core"
req POST "/_reindex?wait_for_completion=true&refresh=true&slices=auto" '{
  "conflicts": "proceed",
  "source": { "index": "people-v1", "size": 500 },
  "dest": { "index": "people-v2", "pipeline": "people-fix", "op_type": "create" }
}'

step "the same counts, and the questions that were impossible before"
req GET "/_cat/count/people-v2?v"
req GET "/people-v2/_search" '{
  "size": 0,
  "aggs": {
    "by_country": { "terms": { "field": "country" },
                    "aggs": { "mean_score": { "avg": { "field": "score" } } } },
    "by_year":    { "date_histogram": { "field": "born", "calendar_interval": "1y", "min_doc_count": 0 } },
    "by_decade":  { "histogram": { "field": "age", "interval": 10 } },
    "roles":      { "terms": { "field": "tags" } },
    "ages":       { "stats": { "field": "age" } }
  }
}'

step "the swap: one atomic action, no window where the alias points at nothing"
req POST "/_aliases" '{ "actions": [
  { "remove": { "index": "people-v1", "alias": "people" } },
  { "add":    { "index": "people-v2", "alias": "people" } }
]}'
req GET "/people/_search" '{ "size": 1, "_source": ["name", "country", "age", "tags"] }'
note "the client never changed a URL, and never saw a moment with no index behind it"

step "a filtered alias, so one index can look like several"
req POST "/_aliases" '{ "actions": [
  { "add": { "index": "people-v2", "alias": "people-th",
             "filter": { "term": { "country": "Thailand" } },
             "routing": "th" } }
]}'
req GET "/people-th/_count"
req GET "/people/_count"

step "fixing what the reindex could not know: a script over what is written"
req POST "/people/_update_by_query?wait_for_completion=true&refresh=true&conflicts=proceed" '{
  "query": { "range": { "age": { "gte": 60 } } },
  "script": { "lang": "painless", "source": "if (!ctx._source.tags.contains(\"senior\")) { ctx._source.tags.add(\"senior\") }" }
}'
req GET "/people/_search" '{ "size": 0, "query": { "term": { "tags": "senior" } } }'

step "and removing what should never have been there"
req POST "/people/_delete_by_query?wait_for_completion=true&refresh=true&conflicts=proceed" '{
  "query": { "term": { "score": 0 } }
}'

step "shrinking four shards to one, for an index that stopped growing"
quiet PUT "/people-v1/_settings" '{ "settings": { "index.blocks.write": true, "index.number_of_replicas": 0 } }'
req POST "/people-v1/_shrink/people-small" '{ "settings": { "index.number_of_shards": 1, "index.blocks.write": null } }' || true
green "people-small" || true
req GET "/_cat/shards/people-small?v&h=index,shard,prirep,docs,state" || true

step "and splitting the other way"
quiet PUT "/people-v2/_settings" '{ "settings": { "index.blocks.write": true } }'
req POST "/people-v2/_split/people-v3" '{ "settings": { "index.number_of_shards": 6, "index.blocks.write": null } }' || true
quiet PUT "/people-v2/_settings" '{ "settings": { "index.blocks.write": null } }'
req GET "/_cat/indices/people*?v&h=index,pri,rep,docs.count,store.size"

step "reindexing from another cluster entirely"
note "the source cluster's host must be in VELOSEARCH_REINDEX_ALLOWLIST"
req POST "/_reindex?wait_for_completion=true" '{
  "source": { "remote": { "host": "'"$VS"'" }, "index": "people-v2", "size": 100,
              "query": { "term": { "country": "Japan" } } },
  "dest": { "index": "people-from-remote" }
}' || note "not allowed from here -- start the node with VELOSEARCH_REINDEX_ALLOWLIST=127.0.0.1:*"

step "a long reindex runs as a task you can watch and cancel"
req GET "/_tasks?actions=*reindex*&detailed=true"
note "without wait_for_completion the call returns a task id; GET /_tasks/<id> follows it"
note "and POST /_tasks/<id>/_cancel stops it"

step "what this example leaves behind, checked rather than assumed"
expect_docs people-v2 2000 "every document survived the reindex -- a reindex answers 200 with its failures inside, so this is the only thing that proves it"
done_
