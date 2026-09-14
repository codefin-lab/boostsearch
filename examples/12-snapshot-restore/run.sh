#!/usr/bin/env bash
# A backup that is proved to be restorable, not merely taken.
source "$(dirname "$0")/lib.sh"
REPO=${REPO:-backups}
IDX=ledger

step "the repository -- the path must be inside the node's allowed repo path"
req GET "/_snapshot" || true
gone "/_snapshot/$REPO"
req PUT "/_snapshot/$REPO" '{ "type": "fs", "settings": { "location": "'"$REPO"'", "compress": true } }'
note "if this fails with a repository_exception, start the node with"
note "  BOOSTSEARCH_PATH_REPO=/some/writable/dir"

step "the repository answers, and is writable by every node"
req POST "/_snapshot/$REPO/_verify"

step "something worth backing up"
gone "/$IDX"; gone "/$IDX-restored"; gone "/other"
reqf PUT "/$IDX" requests/01-something-worth-backing-up.json
req PUT "/other" '{ "settings": { "number_of_shards": 1, "number_of_replicas": 0 } }'
python3 - <<'PY' > /tmp/ledger.ndjson
import json, random, datetime
random.seed(3)
out = []
for i in range(500):
    out.append(json.dumps({"index": {"_id": f"e{i}"}}))
    out.append(json.dumps({"entry": f"line-{i}", "amount": round(random.uniform(-500, 5000), 2),
                           "when": (datetime.date(2026, 1, 1) + datetime.timedelta(days=i % 240)).isoformat()}))
print("\n".join(out))
PY
ndjson "/$IDX/_bulk?refresh=true" /tmp/ledger.ndjson > /dev/null
expect_docs "$IDX" 500 "ledger entries"
quiet POST "/other/_doc?refresh=true" '{ "unrelated": true }'
BEFORE=$("${CURL[@]}" "$BS/$IDX/_count" | python3 -c 'import json,sys;print(json.load(sys.stdin)["count"])')
SUM_BEFORE=$("${CURL[@]}" -X POST "$BS/$IDX/_search" -H 'Content-Type: application/json' \
  -d '{"size":0,"aggs":{"t":{"sum":{"field":"amount"}}}}' \
  | python3 -c 'import json,sys;print(round(json.load(sys.stdin)["aggregations"]["t"]["value"],2))')
note "before the snapshot: $BEFORE documents, total $SUM_BEFORE"

step "take it, and wait for it"
reqf PUT "/_snapshot/$REPO/nightly-1?wait_for_completion=true" requests/02-take-it-and-wait-for-it.json

step "what is in the repository, and what the snapshot holds"
req GET "/_snapshot/$REPO/_all"
req GET "/_snapshot/$REPO/nightly-1/_status"
req GET "/_cat/snapshots/$REPO?v"

step "now do the damage"
quiet POST "/$IDX/_delete_by_query?refresh=true" '{ "query": { "range": { "amount": { "lt": 0 } } } }'
req GET "/$IDX/_count"
note "some entries are gone; the snapshot still has them"

step "restore beside the original, rather than over it"
req POST "/_snapshot/$REPO/nightly-1/_restore?wait_for_completion=true" '{
  "indices": "ledger",
  "rename_pattern": "(.+)",
  "rename_replacement": "$1-restored",
  "index_settings": { "index.number_of_replicas": 0 }
}'
green "$IDX-restored"
AFTER=$("${CURL[@]}" "$BS/$IDX-restored/_count" | python3 -c 'import json,sys;print(json.load(sys.stdin)["count"])')
SUM_AFTER=$("${CURL[@]}" -X POST "$BS/$IDX-restored/_search" -H 'Content-Type: application/json' \
  -d '{"size":0,"aggs":{"t":{"sum":{"field":"amount"}}}}' \
  | python3 -c 'import json,sys;print(round(json.load(sys.stdin)["aggregations"]["t"]["value"],2))')
note "restored: $AFTER documents, total $SUM_AFTER"
if [ "$BEFORE" = "$AFTER" ] && [ "$SUM_BEFORE" = "$SUM_AFTER" ]; then
  note "the restore matches what went in, document for document and baht for baht"
else
  note "MISMATCH -- before $BEFORE/$SUM_BEFORE, after $AFTER/$SUM_AFTER"
fi

step "restoring over a live index is refused, which is the right answer"
req POST "/_snapshot/$REPO/nightly-1/_restore?wait_for_completion=true" '{ "indices": "ledger" }' || true
note "close the index first, or restore under another name as above"

step "an incremental second snapshot: only what changed is written"
quiet POST "/$IDX/_doc?refresh=true" '{ "entry": "line-new", "amount": 1.0, "when": "2026-09-13" }'
req PUT "/_snapshot/$REPO/nightly-2?wait_for_completion=true" '{ "indices": "ledger" }'
req GET "/_snapshot/$REPO/nightly-2/_status"
note "compare the byte counts with nightly-1's: the segments already there are not written twice"

step "a snapshot policy, so nobody has to remember"
gone "/_plugins/_sm/policies/nightly"
req POST "/_plugins/_sm/policies/nightly" '{
  "description": "every night, kept for a week",
  "creation": { "schedule": { "cron": { "expression": "0 2 * * *", "timezone": "Asia/Bangkok" } } },
  "deletion": { "condition": { "max_age": "7d", "max_count": 10 } },
  "snapshot_config": { "repository": "'"$REPO"'", "indices": "ledger*", "ignore_unavailable": true }
}' || note "snapshot management may not be enabled on this node"

step "throwing one away, and tidying the repository"
req DELETE "/_snapshot/$REPO/nightly-1"
req POST "/_snapshot/$REPO/_cleanup"
req GET "/_cat/snapshots/$REPO?v"

step "what this example leaves behind, checked rather than assumed"
done_
