#!/usr/bin/env bash
# Saved searches that find the documents, not the other way round.
source "$(dirname "$0")/lib.sh"
IDX=alerts
EVENTS=events

# --- checks of this example's own, beside the ones in lib.sh ----------------

# answer METHOD PATH FILE -- the answer and its status, whatever the status;
# sets BODY and CODE, and prints the body
answer() {
  local out
  out=$(curl -sS ${AUTH:+-u "$AUTH"} -X "$1" "$VS$2" -H 'Content-Type: application/json' \
        --data-binary "@$3" -w '\n%{http_code}')
  CODE=${out##*$'\n'}
  BODY=${out%$'\n'*}
  echo "$BODY"
}

# expect_that WHAT METHOD PATH FILE EXPR -- a search whose answer must satisfy
# a Python expression. In it: d is the answer, ids the hit ids in order, and
# score(id), slots(id), hl(id) read one hit.
expect_that() {
  local what=$1 m=$2 p=$3 f=$4 expr=$5 got
  got=$("${CURL[@]}" -X "$m" "$VS$p" -H 'Content-Type: application/json' --data-binary "@$f" 2>/dev/null \
    | python3 -c 'import json, sys
try:
    d = json.load(sys.stdin)
    hits = {h["_id"]: h for h in d["hits"]["hits"]}
    ids = [h["_id"] for h in d["hits"]["hits"]]
    score = lambda i: hits[i]["_score"]
    slots = lambda i: hits[i].get("fields", {}).get("_percolator_document_slot")
    hl = lambda i: hits[i].get("highlight", {})
    print("yes" if eval(sys.argv[1]) else "no")
except Exception:
    print("no")' "$expr") || got=no
  if [ "$got" = yes ]; then
    printf '   \033[32mok\033[0m  %s\n' "$what"
  else
    printf '   \033[31mNOT WHAT WAS EXPECTED\033[0m  %s\n' "$what" >&2
    _fails=$((_fails + 1))
  fi
}

# expect_status WANT WHAT -- the CODE the last `answer` got
expect_status() {
  if [ "$CODE" = "$1" ]; then
    printf '   \033[32mok\033[0m  %s -- %s\n' "$CODE" "$2"
  else
    printf '   \033[31mNOT WHAT WAS EXPECTED\033[0m  %s, expected %s -- %s\n' "$CODE" "$1" "$2" >&2
    _fails=$((_fails + 1))
  fi
}

# ---------------------------------------------------------------------------

step "an index of alert rules: the queries are the documents"
note "the event fields are mapped here too -- a stored query is parsed against this mapping"
gone "/$IDX"
reqf PUT "/$IDX" requests/01-an-index-of-alert-rules-the.json
green "$IDX"

step "twelve alert rules, each a stored query with an owner, a severity and a channel"
ndjson "/$IDX/_bulk?refresh=wait_for" data/01-twelve-alert-rules-each-a-stored.ndjson \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print({"errors": d["errors"], "items": len(d["items"])})'
expect_docs "$IDX" 12 "alert rules"

step "a rule that names a field nobody mapped is refused when it is stored"
note "hostname is a typo for host; accepted, the rule would never fire and nobody would know"
answer PUT "/$IDX/_doc/r99?refresh=true" requests/02-a-rule-that-names-a-field.json
expect_status 400 "refused"
python3 -c 'import json,sys; e=json.loads(sys.argv[1])["error"]
print("  ", e["type"], "/", e["caused_by"]["type"] + ":", e["caused_by"]["reason"])' "$BODY"
expect_docs "$IDX" 12 "still twelve: nothing was stored"

step "one event, not yet indexed anywhere: which rules does it trip, best first?"
reqf GET "/$IDX/_search" requests/03-one-event-which-rules-does-it.json
expect_hits 4 GET "/$IDX/_search" "$(cat requests/03-one-event-which-rules-does-it.json)" \
  "a payments timeout in eu-west: r01, r05, r06, r09"
expect_that "the two rules that match words come first, scored above 0" \
  GET "/$IDX/_search" requests/03-one-event-which-rules-does-it.json \
  'set(ids[:2]) == {"r06", "r09"} and score("r06") > 0 and score("r09") > 0'
expect_that "the two rules that only filter score 0.0 and come last" \
  GET "/$IDX/_search" requests/03-one-event-which-rules-does-it.json \
  'set(ids[2:]) == {"r01", "r05"} and score("r01") == 0 and score("r05") == 0'
expect_that "one document, so every hit names slot [0]" \
  GET "/$IDX/_search" requests/03-one-event-which-rules-does-it.json \
  'all(slots(i) == [0] for i in ids)'

step "three events at once: which rules, and which of the events tripped each"
reqf GET "/$IDX/_search" requests/04-three-events-at-once-which-rules.json
expect_hits 5 GET "/$IDX/_search" "$(cat requests/04-three-events-at-once-which-rules.json)" \
  "five rules, one hit each"
expect_that "r02 by slot 0 (the slow checkout), r05 r07 r12 by slot 1 (the refused connection), r04 by slot 2 (the login)" \
  GET "/$IDX/_search" requests/04-three-events-at-once-which-rules.json \
  'slots("r02") == [0] and slots("r05") == [1] and slots("r07") == [1] and slots("r12") == [1] and slots("r04") == [2]'

step "which words tripped which rule: highlighting the events, not the rules"
note "out of memory (slot 0), a lock timed out at 5400 ms (slot 1), the payments timeout (slot 2)"
reqf GET "/$IDX/_search" requests/05-which-words-tripped-which-rule-and.json
F=requests/05-which-words-tripped-which-rule-and.json
expect_hits 6 GET "/$IDX/_search" "$(cat $F)" "r10, r08, r06, r09, r01, r05"
expect_that "r06 was tripped by slots 1 and 2, and says by which word in each" GET "/$IDX/_search" $F \
  'slots("r06") == [1, 2] and hl("r06") == {"1_message": ["request <em>timed</em> out waiting for lock"], "2_message": ["upstream <em>timeout</em> calling card processor"]}'
expect_that "r10's phrase is marked in slot 0" GET "/$IDX/_search" $F \
  'slots("r10") == [0] and hl("r10") == {"0_message": ["worker killed: <em>out</em> <em>of</em> <em>memory</em>"]}'
expect_that "r08 is a range on latency: slots 0 and 1, and no words to mark" GET "/$IDX/_search" $F \
  'slots("r08") == [0, 1] and hl("r08") == {}'
expect_that "best first: the phrase, then the range at 1.0, then the words, then the filters at 0.0" GET "/$IDX/_search" $F \
  'ids[:2] == ["r10", "r08"] and score("r10") > 1 and score("r08") == 1 and set(ids[4:]) == {"r01", "r05"} and all(score(a) >= score(b) for a, b in zip(ids, ids[1:]))'

step "an event index of its own, and ten events in it"
gone "/$EVENTS"
reqf PUT "/$EVENTS" requests/06-an-event-index-of-its-own.json
green "$EVENTS"
ndjson "/$EVENTS/_bulk?refresh=wait_for" data/02-an-hour-of-events-already-indexed.ndjson \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print({"errors": d["errors"], "items": len(d["items"])})'
expect_docs "$EVENTS" 10 "events"

step "an event already stored, percolated by its index and id"
note "the server fetches e05 from events and percolates its _source; the body carries no document"
reqf GET "/$IDX/_search" requests/07-an-event-already-stored-percolated-by.json
expect_hits 2 GET "/$IDX/_search" "$(cat requests/07-an-event-already-stored-percolated-by.json)" \
  "a full disk on db-1: r03 and r12"

note "--- and one that is not there: an error, not an empty list that reads as 'no rule matched'"
answer GET "/$IDX/_search" requests/08-an-event-that-is-not-there.json
expect_status 404 "resource_not_found_exception"

step "only the critical rules: the stored queries filtered by what is written beside them"
reqf GET "/$IDX/_search" requests/09-only-the-critical-rules-the-stored.json
expect_hits 2 GET "/$IDX/_search" "$(cat requests/09-only-the-critical-rules-the-stored.json)" \
  "the same event as step 4, four rules narrowed to r01 and r09"

note "--- and by owner: what the same event means for ben"
PERC_E06=$(python3 -c 'import json; print(json.dumps(json.load(open("requests/03-one-event-which-rules-does-it.json"))["query"]["percolate"]))')
BY_OWNER="{ \"_source\": [\"name\", \"owner\"],
  \"query\": { \"bool\": { \"filter\": [ { \"percolate\": $PERC_E06 }, { \"term\": { \"owner\": \"ben\" } } ] } } }"
req GET "/$IDX/_search" "$BY_OWNER"
expect_hits 1 GET "/$IDX/_search" "$BY_OWNER" "r06, ben's timeout rule"

step "a burst of five events: who is told, and how"
note "percolate as a filter, informational rules left out, and the rules that remain counted"
reqf GET "/$IDX/_search" requests/10-a-burst-of-five-events-who.json
expect_hits 7 GET "/$IDX/_search" "$(cat requests/10-a-burst-of-five-events-who.json)" \
  "ten rules tripped, three of them info"
expect_that "5 critical and 2 warning; ops 3, ana 2, ben 1, chen 1" \
  GET "/$IDX/_search" requests/10-a-burst-of-five-events-who.json \
  '{b["key"]: b["doc_count"] for b in d["aggregations"]["by_severity"]["buckets"]} == {"critical": 5, "warning": 2} and {b["key"]: b["doc_count"] for b in d["aggregations"]["by_owner"]["buckets"]} == {"ops": 3, "ana": 2, "ben": 1, "chen": 1}'

step "a rule is a document: change it, and the next event is judged by the new version"
note "r08 fires above 5000 ms; lowered to 3000, the payments timeout from step 4 now trips it too"
req POST "/$IDX/_update/r08?refresh=true" '{ "doc": { "name": "anything slower than three seconds",
  "query": { "range": { "latency_ms": { "gte": 3000 } } } } }'
expect_hits 5 GET "/$IDX/_search" "$(cat requests/03-one-event-which-rules-does-it.json)" \
  "step 4's event, four rules then, five now"
note "nothing was reindexed: that event was never stored, and the rules are read at the moment of asking"

step "what this example leaves behind, checked rather than assumed"
expect_docs "$IDX" 12 "alert rules, r08 at its new threshold"
expect_docs "$EVENTS" 10 "events"
done_
