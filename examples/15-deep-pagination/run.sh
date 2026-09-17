#!/usr/bin/env bash
# Getting to page 500, and exporting everything, without the engine falling over.
source "$(dirname "$0")/lib.sh"
IDX=events

step "fifty thousand events across three shards"
gone "/$IDX"
reqf PUT "/$IDX" requests/01-fifty-thousand-events-across-three-shards.json
green "$IDX"
python3 - <<'PY' > /tmp/events.ndjson
import json, random, datetime
random.seed(13)
kinds = ["click", "view", "buy", "return"]
base = datetime.datetime(2026, 1, 1)
out = []
for i in range(50000):
    out.append(json.dumps({"index": {"_id": f"e{i}"}}))
    out.append(json.dumps({"seq": i, "kind": random.choice(kinds),
                           "at": (base + datetime.timedelta(minutes=i)).isoformat() + "Z",
                           "payload": f"event number {i} of the run"}))
print("\n".join(out))
PY
ndjson "/$IDX/_bulk?refresh=true" /tmp/events.ndjson > /dev/null
expect_docs "$IDX" 50000 "events"
req GET "/_cat/count/$IDX?v"

step "page one is free"
req GET "/$IDX/_search" '{ "size": 3, "_source": ["seq"], "sort": [{ "seq": "asc" }] }'

step "page five hundred by from/size -- and why it is refused"
req GET "/$IDX/_search" '{ "size": 20, "from": 11000, "_source": ["seq"], "sort": [{ "seq": "asc" }] }' || true
note "from + size must be under index.max_result_window (10,000 by default)"
note "every shard would have to collect from+size hits and the coordinator sort 3x that"

step "search_after: the cursor that costs the same on page 500 as on page 1"
note "--- page 1"
req GET "/$IDX/_search" '{
  "size": 3, "_source": ["seq"],
  "query": { "term": { "kind": "buy" } },
  "sort": [{ "seq": "asc" }, { "_id": "asc" }],
  "track_total_hits": false
}'
note "--- take the last hit'\''s sort array and pass it back"
req GET "/$IDX/_search" '{
  "size": 3, "_source": ["seq"],
  "query": { "term": { "kind": "buy" } },
  "sort": [{ "seq": "asc" }, { "_id": "asc" }],
  "search_after": [40000, "e40000"],
  "track_total_hits": false
}'
note "the tiebreak on _id matters: without it, two documents with the same sort value"
note "can be skipped or repeated at a page boundary"

step "the problem search_after alone does not solve: the index moves under you"
note "a document written between page 3 and page 4 shifts everything after it"
note "a point in time freezes the view"
PIT=$("${CURL[@]}" -X POST "$VS/$IDX/_search/point_in_time?keep_alive=2m" \
      | python3 -c 'import json,sys;print(json.load(sys.stdin).get("pit_id",""))')
note "pit id: ${PIT:0:40}..."

step "paging through a frozen view"
LAST=""
for page in 1 2 3; do
  if [ -z "$LAST" ]; then
    BODY="{\"size\":2,\"_source\":[\"seq\"],\"pit\":{\"id\":\"$PIT\",\"keep_alive\":\"2m\"},\"sort\":[{\"seq\":\"asc\"}],\"track_total_hits\":false}"
  else
    BODY="{\"size\":2,\"_source\":[\"seq\"],\"pit\":{\"id\":\"$PIT\",\"keep_alive\":\"2m\"},\"sort\":[{\"seq\":\"asc\"}],\"search_after\":$LAST,\"track_total_hits\":false}"
  fi
  note "--- page $page"
  OUT=$("${CURL[@]}" -X POST "$VS/_search" -H 'Content-Type: application/json' -d "$BODY")
  echo "$OUT" | python3 -c 'import json,sys; d=json.load(sys.stdin); print([h["_source"]["seq"] for h in d["hits"]["hits"]])'
  LAST=$(echo "$OUT" | python3 -c 'import json,sys; d=json.load(sys.stdin); h=d["hits"]["hits"]; print(json.dumps(h[-1]["sort"]) if h else "")')
  # a write between pages, which the frozen view must not see
  quiet POST "/$IDX/_doc?refresh=true" "{ \"seq\": -$page, \"kind\": \"click\", \"at\": \"2026-01-01T00:00:00Z\", \"payload\": \"written during paging\" }"
done
note "seq -1, -2, -3 were written between the pages and sort before everything;"
note "they never appeared, because the point in time was taken before they existed"

step "give the point in time back"
req DELETE "/_search/point_in_time" "{ \"pit_id\": \"$PIT\" }"

step "scroll: the older way, still the right one for a full export"
SCROLL=$("${CURL[@]}" -X POST "$VS/$IDX/_search?scroll=1m" -H 'Content-Type: application/json' \
  -d '{"size":1000,"_source":["seq"],"query":{"term":{"kind":"buy"}},"sort":["_doc"]}')
SID=$(echo "$SCROLL" | python3 -c 'import json,sys;print(json.load(sys.stdin)["_scroll_id"])')
TOTAL=$(echo "$SCROLL" | python3 -c 'import json,sys;print(len(json.load(sys.stdin)["hits"]["hits"]))')
note "first batch: $TOTAL"
for _ in 1 2; do
  NEXT=$("${CURL[@]}" -X POST "$VS/_search/scroll" -H 'Content-Type: application/json' \
    -d "{\"scroll\":\"1m\",\"scroll_id\":\"$SID\"}")
  N=$(echo "$NEXT" | python3 -c 'import json,sys;print(len(json.load(sys.stdin)["hits"]["hits"]))')
  SID=$(echo "$NEXT" | python3 -c 'import json,sys;print(json.load(sys.stdin)["_scroll_id"])')
  note "next batch: $N"
done
req DELETE "/_search/scroll" "{ \"scroll_id\": [\"$SID\"] }"
note "sort by _doc: no ordering work at all, which is what an export wants"

step "a sliced scroll, so an export can be run in parallel"
for s in 0 1; do
  note "--- slice $s"
  req POST "/$IDX/_search?scroll=1m" "{
    \"slice\": { \"id\": $s, \"max\": 2 },
    \"size\": 5, \"_source\": [\"seq\"], \"sort\": [\"_doc\"],
    \"query\": { \"term\": { \"kind\": \"buy\" } } }" | clip 300
done

step "counting: exact is expensive, and usually not needed"
req GET "/$IDX/_search" '{ "size": 0, "track_total_hits": true }'
req GET "/$IDX/_search" '{ "size": 0, "track_total_hits": 1000 }'
note 'the second says gte 1000 rather than the exact number, and stops counting there'

step "raising the window, if you really must"
req PUT "/$IDX/_settings" '{ "index.max_result_window": 20000 }'
req GET "/$IDX/_search" '{ "size": 5, "from": 11000, "_source": ["seq"], "sort": [{ "seq": "asc" }] }'
note "it works, and it is still the wrong answer: memory grows with 'from'"

step "what this example leaves behind, checked rather than assumed"
done_
