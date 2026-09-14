#!/usr/bin/env bash
# The questions an operator asks at three in the morning, each with the API
# that answers it -- and every change made along the way undone at the end.
source "$(dirname "$0")/lib.sh"

# --- helpers this example needs beyond lib.sh --------------------------------

# ask METHOD PATH [BODY | @FILE] -- a request whose refusal is an answer worth
# reading, not a reason to stop. lib.sh's curl fails on any 4xx; a runbook has
# to look at a 403 or a 408 and carry on. The status lands in $CODE.
CODE=0
_body=$(mktemp); trap 'rm -f "$_body"' EXIT
ask() {
  local m=$1 p=$2 b=${3-} ct='application/json'
  local c=(curl -sS -o "$_body" -w '%{http_code}' -X "$m" "$BS$p")
  [ -n "$AUTH" ] && c+=(-u "$AUTH")
  case "$p" in *_bulk*) ct='application/x-ndjson' ;; esac
  # --data-binary keeps the newlines a bulk body needs, and reads @file as a file
  [ -n "$b" ] && c+=(-H "Content-Type: $ct" --data-binary "$b")
  CODE=$("${c[@]}")
  cat "$_body"; echo
  note "HTTP $CODE"
}

# pick PYTHON-EXPR -- read a JSON answer on stdin, print one value from it (`d`)
pick() { python3 -c 'import json,sys; d = json.load(sys.stdin); print(eval(sys.argv[1]))' "$1"; }

# get PATH EXPR -- one value from a GET, without printing the answer
get() { "${CURL[@]}" "$BS$1" | pick "$2"; }

# expect WHAT GOT WANT -- a value the step exists to show, checked
expect() {
  if [ "$2" = "$3" ]; then
    printf '   \033[32mok\033[0m  %s: %s\n' "$1" "$2"
  else
    printf '   \033[31mNOT WHAT WAS EXPECTED\033[0m  %s: %s, expected %s\n' "$1" "$2" "$3" >&2
    _fails=$((_fails + 1))
  fi
}

# settle -- wait, up to five seconds, for the cluster-wide health to agree with
# the per-index health.
#
# Earlier builds of this node could, for about half a second after an index
# was created, answer "green" from _cluster/health while counting an
# unassigned shard, and while level=indices already said the new index was
# yellow. The summary is now judged from the same state as the per-index view,
# so this returns at once; it stays for a runbook pointed at an older node.
settle() {
  local i=0
  while [ "$i" -lt 25 ]; do
    [ "$(get /_cluster/health 'd["status"] == "green" and d["unassigned_shards"] > 0')" = False ] && return 0
    sleep 0.2; i=$((i + 1))
  done
}

# -----------------------------------------------------------------------------

step "the patient: orders with a replica it cannot place, audit with none"
note "one node; orders asks for a replica, which needs a second node to live on"
gone /orders; gone /audit; gone /scratch-flood
quiet PUT /_cluster/settings @requests/08-forget-both-cluster-settings.json 2>/dev/null || true
reqf PUT /orders requests/01-orders-one-shard-one-replica.json
reqf PUT /audit requests/02-audit-one-shard-no-replica.json
green orders
for f in data/01-orders-monday.ndjson data/02-orders-tuesday.ndjson data/03-orders-wednesday.ndjson; do
  ndjson "/orders/_bulk?refresh=true" "$f" | pick '"errors=%s items=%d" % (d["errors"], len(d["items"]))'
done
ndjson "/audit/_bulk?refresh=true" data/05-audit-events.ndjson | pick '"errors=%s items=%d" % (d["errors"], len(d["items"]))'
expect_docs orders 18 "three days of orders, one bulk and one refresh each"
expect_docs audit 10 "audit events"
settle

step "is the cluster healthy? -- _cluster/health"
req GET /_cluster/health
expect "cluster status" "$(get /_cluster/health 'd["status"]')" yellow
note "yellow: every primary is serving, some replica is not. Nothing is lost; nothing is spare."

step "which index is it? -- level=indices"
req GET "/_cluster/health?level=indices&filter_path=status,indices"
expect "orders" "$(get '/_cluster/health?level=indices' 'd["indices"]["orders"]["status"]')" yellow
expect "audit" "$(get '/_cluster/health?level=indices' 'd["indices"]["audit"]["status"]')" green

step "which shard of it? -- level=shards"
req GET "/_cluster/health/orders?level=shards&filter_path=indices.*.shards"
expect "orders shard 0 unassigned copies" \
  "$(get '/_cluster/health/orders?level=shards' 'd["indices"]["orders"]["shards"]["0"]["unassigned_shards"]')" 1

step "will it go green if I wait? -- wait_for_status, with a timeout"
note "a script that must not start until the cluster is ready asks this, rather than polling"
ask GET "/_cluster/health/orders?wait_for_status=green&timeout=2s&filter_path=status,timed_out"
expect "waiting for green on one node" "$CODE" 408
note "408 with timed_out:true -- the timeout ran out, the colour never arrived, and it never will"
ask GET "/_cluster/health/orders?wait_for_status=yellow&timeout=2s&filter_path=status,timed_out"
expect "waiting for yellow" "$CODE" 200

step "the indices at a glance -- _cat/indices, then only the columns that matter"
req GET "/_cat/indices?v"
req GET "/_cat/indices?v&h=health,index,pri,rep,docs.count,store.size&s=health,index"
note "format=json is the same table for a script: every value is a string, as in OpenSearch"
req GET "/_cat/indices?h=index,health,docs.count&s=index&format=json"

step "every copy of every shard, and why the missing one is missing -- _cat/shards"
req GET "/_cat/shards?v&s=index,shard,prirep"
req GET "/_cat/shards/orders?v&h=index,shard,prirep,state,unassigned.reason"
expect "the replica's unassigned.reason" \
  "$(get '/_cat/shards/orders?format=json&h=prirep,unassigned.reason' '[r["unassigned.reason"] for r in d if r["prirep"] == "r"][0]')" INDEX_CREATED

step "and the reason in full -- _cluster/allocation/explain"
reqf GET /_cluster/allocation/explain requests/03-why-is-this-replica-unassigned.json
expect "the decider that said no" \
  "$("${CURL[@]}" -X GET "$BS/_cluster/allocation/explain" -H 'Content-Type: application/json' \
      --data-binary @requests/03-why-is-this-replica-unassigned.json \
    | pick 'd["node_allocation_decisions"][0]["deciders"][0]["decider"]')" same_shard
note "same_shard: a replica may not share a node with its primary, because then it protects nothing"

step "the nodes, and what each is holding -- _cat/nodes, _cat/allocation"
req GET "/_cat/nodes?v&h=name,ip,node.role,cluster_manager"
req GET "/_cat/allocation?v&h=shards,node"
note "the UNASSIGNED row is the replica again, counted where it would have gone"

step "the fix on one node: no replicas -- a dynamic setting, applied to an open index"
reqf PUT /orders/_settings requests/04-no-replicas-on-a-single-node.json
ask GET "/_cluster/health/orders?wait_for_status=green&timeout=10s&filter_path=status,timed_out"
expect "orders after the change" "$(get /_cluster/health/orders 'd["status"]')" green
expect "the cluster" "$(get /_cluster/health 'd["status"]')" green
note "on a real cluster the fix is a second node, not fewer copies; see docs/design.md"

step "a big load coming: stop refreshing, load, refresh once -- refresh_interval"
reqf PUT /orders/_settings requests/05-stop-refreshing-during-a-load.json
req GET "/orders/_settings?filter_path=*.settings.index.refresh_interval"
ndjson /orders/_bulk data/04-orders-thursday.ndjson | pick '"errors=%s items=%d" % (d["errors"], len(d["items"]))'
expect_docs orders 18 "Thursday is written, and not yet visible to search"
req POST /orders/_refresh
expect_docs orders 24 "one refresh, and all of it is there"
reqf PUT /orders/_settings requests/06-refresh-as-configured-again.json
expect "refresh_interval set on orders afterwards" \
  "$(get '/orders/_settings' 'd["orders"]["settings"]["index"].get("refresh_interval", "none -- the default again")')" \
  "none -- the default again"

step "how many segments? -- _cat/segments, before a force merge"
req GET "/_cat/segments/orders?v&h=index,shard,segment,docs.count,docs.deleted,committed,searchable&s=segment"
before=$(get '/_cat/segments/orders?format=json' 'len(d)')
expect "more than one segment before" "$([ "$before" -gt 1 ] && echo yes || echo "no ($before)")" yes
note "every refresh wrote at least one new segment; a search visits each of them"

step "merge them into one -- _forcemerge?max_num_segments=1"
note "Thursday was the last load; from here orders is read far more than it is written"
note "force merge only an index in that state: one giant segment is expensive to merge again"
req POST "/orders/_forcemerge?max_num_segments=1"
req GET "/_cat/segments/orders?v&h=index,shard,segment,docs.count,docs.deleted,committed,searchable"
expect "segments after" "$(get '/_cat/segments/orders?format=json' 'len(d)')" 1
expect "documents in it" "$(get '/_cat/segments/orders?format=json' 'd[0]["docs.count"]')" 24
expect_docs orders 24 "the same documents, in fewer files"

step "cluster settings: persistent survives a restart, transient does not -- _cluster/settings"
reqf PUT /_cluster/settings requests/07-one-persistent-one-transient.json
req GET /_cluster/settings
expect "persistent low watermark" \
  "$(get /_cluster/settings 'd["persistent"]["cluster"]["routing"]["allocation"]["disk"]["watermark"]["low"]')" "90%"
expect "transient allocation.enable" \
  "$(get /_cluster/settings 'd["transient"]["cluster"]["routing"]["allocation"]["enable"]')" primaries
note "null removes a setting; the default returns. Leaving 'primaries' behind is how a cluster stays yellow for a week."
reqf PUT /_cluster/settings requests/08-forget-both-cluster-settings.json
expect "cluster settings afterwards" \
  "$(get /_cluster/settings 'len(d["persistent"]) + len(d["transient"])')" 0

step "stop the writes, keep the reads -- index.blocks.write"
reqf PUT /orders/_settings requests/09-block-writes.json
ask POST /orders/_doc '{"sku":"kettle","qty":1,"city":"York","status":"paid"}'
expect "a single write under the block" "$CODE" 403
note "a bulk does not fail as a whole: 200, errors:true, and the refusal is in each item"
ask POST /orders/_bulk '{"index":{}}
{"sku":"kettle","qty":1,"city":"York","status":"paid"}
'
expect "the bulk's own status" "$CODE" 200
expect "the item's status" "$(pick 'd["items"][0]["index"]["status"]' < "$_body")" 403
expect_docs orders 24 "reads still answer, and nothing was written"
reqf PUT /orders/_settings requests/10-lift-the-write-block.json
note "lifted; the next write is a real one -- a refunded order removed"
ask DELETE "/orders/_doc/o05?refresh=true"
expect "the delete once the block is gone" "$CODE" 200
expect_docs orders 23

step "the flood-stage block: writes refused, deletes of whole indices allowed -- read_only_allow_delete"
note "the node puts this on by itself when a disk passes the flood-stage watermark"
reqf PUT /orders/_settings requests/11-read-only-allow-delete.json
ask POST /orders/_doc/o99 '{"sku":"kettle","qty":1,"city":"York","status":"paid"}'
expect "a write under read_only_allow_delete refused" "$([ "$CODE" -ge 400 ] && echo yes || echo "no ($CODE)")" yes
note "and the way out of a full disk is to delete something -- an index made to be deleted:"
quiet PUT /scratch-flood '{"settings":{"number_of_shards":1,"number_of_replicas":0}}'
quietf PUT /scratch-flood/_settings requests/11-read-only-allow-delete.json
ask DELETE /scratch-flood
expect "deleting a whole index under the block" "$CODE" 200
reqf PUT /orders/_settings requests/12-lift-read-only-allow-delete.json
req GET "/orders/_settings?filter_path=*.settings.index.blocks"
note "{} -- no blocks left on orders"

step "is anything queueing or being rejected? -- _cat/thread_pool"
req GET "/_cat/thread_pool/search,write,get?v&h=name,active,queue,rejected&s=name"
req GET "/_cat/thread_pool?h=name,type&s=name:desc&format=json"
note "active and completed count the requests each pool has run; queue is the runtime's backlog -- see docs/troubleshooting.md"

step "caches, flush, refresh -- the three buttons, and what each one does"
req POST "/orders/_cache/clear?query=true&request=true&fielddata=true"
note "_cache/clear: drops cached results; costs the next queries, never data"
req POST /orders/_flush
note "_flush: commits to disk and trims the translog; a restart replays less"
req POST /orders/_refresh
note "_refresh: makes what is written visible to search; nothing to do with durability"

step "which searches are slow? -- the search slow log thresholds"
reqf PUT /orders/_settings requests/13-slow-log-thresholds.json
req GET "/orders/_settings/index.search.slowlog*"
reqf GET /orders/_search requests/14-a-search-the-slow-log-would-catch.json
note "with query.debug at 0ms, OpenSearch writes that search to logs/<cluster>_index_search_slowlog.json"
note "this node writes the same entry to its log output, and to BOOSTSEARCH_LOGS when set -- see docs/troubleshooting.md"
reqf PUT /orders/_settings requests/15-slow-log-thresholds-removed.json
expect "slow log settings left on orders" \
  "$(get '/orders/_settings' 'len(d["orders"]["settings"]["index"].get("search", {}))')" 0

step "what has this index been doing? -- _stats, per index"
q0=$(get '/orders/_stats/search' 'd["indices"]["orders"]["total"]["search"]["query_total"]')
for city in Leeds York Glasgow; do
  quiet GET /orders/_search "{\"size\":0,\"query\":{\"term\":{\"city\":\"$city\"}}}"
done
req GET "/orders,audit/_stats/docs,store,search,segments?filter_path=indices.*.primaries.docs,indices.*.primaries.store.size_in_bytes,indices.*.primaries.search.query_total,indices.*.primaries.segments.count"
q1=$(get '/orders/_stats/search' 'd["indices"]["orders"]["total"]["search"]["query_total"]')
expect "queries counted for three searches" "$((q1 - q0))" 3
expect "segments.count in _stats" "$(get '/orders/_stats/segments' 'd["indices"]["orders"]["primaries"]["segments"]["count"]')" 1

step "and the node as a whole -- _nodes/stats/indices, and the memory it holds"
req GET "/_nodes/stats/indices?filter_path=nodes.*.name,nodes.*.indices.docs,nodes.*.indices.store"
expect "documents on the node" \
  "$(get '/_nodes/stats/indices' 'list(d["nodes"].values())[0]["indices"]["docs"]["count"]')" 33
note "there is no JVM here; the process's own memory is what the heap figures would have stood for"
req GET "/_boostsearch/memory?filter_path=allocator,indices"

step "is the cluster manager keeping up? -- _cluster/pending_tasks"
req GET /_cluster/pending_tasks
expect "pending cluster tasks" "$(get /_cluster/pending_tasks 'len(d["tasks"])')" 0

step "what is running right now? -- _tasks"
req GET "/_tasks?detailed=true&filter_path=nodes.*.tasks.*.action,nodes.*.tasks.*.cancellable"
note "the only task at rest is the task list asking; example 20 shows a long one caught mid-flight"

step "what this example leaves behind, checked rather than assumed"
req GET "/_cat/indices/orders,audit?v&h=health,index,pri,rep,docs.count&s=index"
expect "cluster status" "$(get /_cluster/health 'd["status"]')" green
expect "cluster settings" "$(get /_cluster/settings 'len(d["persistent"]) + len(d["transient"])')" 0
expect "blocks on orders" "$(get '/orders/_settings' '"blocks" in d["orders"]["settings"]["index"]')" False
expect "refresh_interval on orders" "$(get '/orders/_settings' '"refresh_interval" in d["orders"]["settings"]["index"]')" False
expect "scratch-flood" "$(ask GET /scratch-flood > /dev/null; echo "$CODE")" 404
expect_docs orders 23
expect_docs audit 10
done_
