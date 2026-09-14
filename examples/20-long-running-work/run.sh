#!/usr/bin/env bash
# Work that takes longer than a request: jobs sent off as tasks, followed by id,
# throttled, meeting writes they did not expect, and their results kept.
source "$(dirname "$0")/lib.sh"
IDX=parcels
WORK=$(mktemp -d "${TMPDIR:-/tmp}/boost-example-20.XXXXXX")
trap 'rm -rf "$WORK"' EXIT

# --- helpers of this example's own -------------------------------------------

# now_ms -- the wall clock in milliseconds, to time a request from outside it
now_ms() { python3 -c 'import time; print(int(time.time() * 1000))'; }

# pick FILE EXPR -- one value out of a saved JSON answer; `d` is the answer
pick() {
  python3 -c 'import json,sys; d = json.load(open(sys.argv[1])); print(eval(sys.argv[2], {"d": d}))' "$1" "$2"
}

# expect WHAT CONDITION -- a check on something other than a document count;
# CONDITION is a Python expression with the numbers already put in it
expect() {
  if python3 -c "import sys; sys.exit(0 if ($2) else 1)"; then
    printf '   \033[32mok\033[0m  %s\n' "$1"
  else
    printf '   \033[31mNOT WHAT WAS EXPECTED\033[0m  %s  (%s)\n' "$1" "$2" >&2
    _fails=$((_fails + 1))
  fi
}

# count BODY -- how many parcels a query matches
count() {
  "${CURL[@]}" -X POST "$BS/$IDX/_count" -H 'Content-Type: application/json' -d "$1" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["count"])'
}

# task_of FILE -- the task id out of a `wait_for_completion=false` answer
task_of() { pick "$1" 'd["task"]'; }

# follow ID FILE -- poll GET /_tasks/ID once a second until it says it has
# completed, printing one line of progress per poll; the last answer is kept
# in FILE. This is what a client that sent a job off does.
follow() {
  local id=$1 out=$2 i=0
  while :; do
    "${CURL[@]}" "$BS/_tasks/$id" > "$out"
    python3 - "$out" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
s = d.get("response") or d["task"].get("status", {})
shown = "  ".join(f"{k}={s[k]}" for k in ("total", "updated", "deleted", "version_conflicts") if k in s)
print(f'   completed={str(d["completed"]).lower():<5}  {shown}')
PY
    [ "$(pick "$out" 'd["completed"]')" = True ] && break
    i=$((i + 1)); [ "$i" -gt 120 ] && { echo "the task never completed" >&2; return 1; }
    sleep 1
  done
}

# ------------------------------------------------------------------------------

step "an index of parcels, on two shards, refreshed only when asked"
note "refresh_interval -1: what a search sees changes only at a refresh, which steps 6 to 8 depend on"
gone "/$IDX"
reqf PUT "/$IDX" requests/01-parcels-on-two-shards.json
green "$IDX"

step "six thousand parcels, generated so the jobs have something to work through"
python3 - > "$WORK/parcels.ndjson" <<'PY'
import json, random
random.seed(20)
statuses = ["booked"] * 5 + ["in_transit"] * 6 + ["out_for_delivery"] * 2 + ["delivered"] * 8 + ["returned", "cancelled", "cancelled"]
regions = ["north", "northeast", "central", "east", "west", "south"]
for i in range(1, 6001):
    region = random.choice(regions)
    print(json.dumps({"index": {"_id": f"TH{i:06d}"}}))
    print(json.dumps({
        "tracking": f"TH{i:06d}",
        "customer": f"C{random.randint(1, 1500):04d}",
        "status": random.choice(statuses),
        "region": region,
        "remote_area": region in ("north", "northeast") and random.random() < 0.4,
        "weight_g": random.randint(100, 30000),
        "fee": 40.0,
        "booked_at": f"2026-08-{random.randint(1, 31):02d}T{random.randint(8, 19):02d}:00:00Z",
    }))
PY
ndjson "/$IDX/_bulk?refresh=true" "$WORK/parcels.ndjson" | clip 160
expect_docs "$IDX" 6000 "parcels"

step "a job sent off rather than waited for: the answer is a task id"
want=$(count '{"query":{"bool":{"filter":[{"term":{"remote_area":true}},{"terms":{"status":["booked","in_transit"]}}]}}}')
note "$want remote-area parcels not yet delivered should get the surcharge"
t0=$(now_ms)
reqf POST "/$IDX/_update_by_query?wait_for_completion=false&refresh=true" requests/02-add-the-remote-area-surcharge.json | tee "$WORK/surcharge.json"
note "the answer came back after $(( $(now_ms) - t0 )) ms"
SURCHARGE=$(task_of "$WORK/surcharge.json")

step "following it by id until it says it has completed"
follow "$SURCHARGE" "$WORK/surcharge-task.json"
clip 700 < "$WORK/surcharge-task.json"
expect "it updated every parcel the query matched ($want)" \
  "$(pick "$WORK/surcharge-task.json" 'd["response"]["updated"]') == $want"
expect "and the index agrees: $want parcels carry a surcharge" \
  "$(count '{"query":{"term":{"surcharge":35.0}}}') == $want"

step "a job told how fast it may go: requests_per_second"
note "6000 parcels in batches of 500 at 2000 a second: a quarter-second pause after each batch but the last"
t0=$(now_ms)
reqf POST "/$IDX/_update_by_query?requests_per_second=2000&scroll_size=500&refresh=true" requests/03-mark-every-parcel-audited.json | tee "$WORK/audit.json"
took=$(( $(now_ms) - t0 ))
throttled=$(pick "$WORK/audit.json" 'd["throttled_millis"]')
note "the request took $took ms, of which $throttled ms was the job holding itself back"
expect "12 batches, all 6000 parcels audited" \
  "$(pick "$WORK/audit.json" 'd["batches"]') == 12 and $(pick "$WORK/audit.json" 'd["updated"]') == 6000"
expect "the throttle cost real time: at least the $throttled ms it says it waited" \
  "$throttled >= 2500 and $took >= $throttled"
expect "and the index agrees: 6000 parcels audited" "$(count '{"query":{"term":{"audited":true}}}') == 6000"

step "the dispatch app marks 40 parcels delivered, and nobody refreshes"
delivered=0
for i in $(seq 150 150 6000); do
  quietf POST "/$IDX/_update/$(printf 'TH%06d' "$i")" requests/05-the-dispatch-app-marks-a-parcel-delivered.json
  delivered=$((delivered + 1))
done
note "$delivered writes made; a search still sees $(count '{"query":{"term":{"delivered_by":"dispatch-app"}}}') of them"
note "a job that starts now reads parcels as they were -- exactly what a job sees of a write that lands while it runs"

step "a repricing job with conflicts: proceed, sent off and followed"
reqf POST "/$IDX/_update_by_query?wait_for_completion=false&refresh=true" requests/04-reprice-by-weight-band.json | tee "$WORK/reprice.json"
REPRICE=$(task_of "$WORK/reprice.json")
follow "$REPRICE" "$WORK/reprice-task.json"
conflicts=$(pick "$WORK/reprice-task.json" 'd["response"]["version_conflicts"]')
updated=$(pick "$WORK/reprice-task.json" 'd["response"]["updated"]')
expect "one version conflict for each parcel written since the job's view: $delivered" "$conflicts == $delivered"
expect "every other parcel repriced: $updated + $conflicts = 6000" "$updated + $conflicts == 6000"
expect "no dispatch write was lost: all $delivered still say who delivered them" \
  "$(count '{"query":{"term":{"delivered_by":"dispatch-app"}}}') == $delivered"
note "the $conflicts skipped parcels still carry the old fee; running the job again picks them up"

step "the same collision without conflicts: proceed -- the job stops at the first"
for i in $(seq 75 150 6000); do
  quietf POST "/$IDX/_update/$(printf 'TH%06d' "$i")" requests/06-the-dispatch-app-marks-a-parcel-returned.json
done
note "40 more parcels written and not refreshed; this time the job is waited for"
NOFAIL=(curl -sS); [ -n "$AUTH" ] && NOFAIL+=(-u "$AUTH")
"${NOFAIL[@]}" -X POST "$BS/$IDX/_update_by_query" -H 'Content-Type: application/json' \
  --data-binary @requests/07-the-same-job-without-conflicts-proceed.json -w '\n%{http_code}\n' > "$WORK/abort.raw"
python3 - "$WORK/abort.raw" "$WORK/abort.json" <<'PY'
import sys
body, status = open(sys.argv[1]).read().rstrip("\n").rsplit("\n", 1)
open(sys.argv[2], "w").write(body)
print(body); print(f"   HTTP {status}")
PY
expect "it stopped at the first conflict and said which parcel" \
  "$(pick "$WORK/abort.json" 'd["version_conflicts"]') == 1 and $(pick "$WORK/abort.json" 'd["failures"][0]["cause"]["type"] == "version_conflict_engine_exception"')"
note "what it had already written stays written: an aborted job is not rolled back"

step "nothing is left running once the jobs have answered"
req GET "/_tasks?actions=*byquery&detailed"

step "the .tasks index: where a finished task's result outlives the request"
quiet POST "/.tasks/_refresh"
req GET "/.tasks/_doc/$REPRICE" > "$WORK/reprice-record.json"
clip 600 < "$WORK/reprice-record.json"
reqf GET "/.tasks/_search" requests/08-task-results-that-met-a-conflict.json | clip 900
expect "the repricing is on record, with its $conflicts conflicts" \
  "$(pick "$WORK/reprice-record.json" 'd["_source"]["response"]["version_conflicts"]') == $conflicts"

step "a delete by query, sent off and followed the same way"
quiet POST "/$IDX/_refresh"
cancelled=$(count '{"query":{"term":{"status":"cancelled"}}}')
note "$cancelled cancelled parcels to purge"
reqf POST "/$IDX/_delete_by_query?wait_for_completion=false&refresh=true" requests/09-purge-cancelled-parcels.json | tee "$WORK/purge.json"
PURGE=$(task_of "$WORK/purge.json")
follow "$PURGE" "$WORK/purge-task.json"
expect "the task deleted all $cancelled" "$(pick "$WORK/purge-task.json" 'd["response"]["deleted"]') == $cancelled"

step "what this example leaves behind, checked rather than assumed"
expect_docs "$IDX" $((6000 - cancelled)) "parcels, less the purged ones"
expect_at_least ".tasks" 3 "a result for each job sent off: three per run, kept across runs"
done_
