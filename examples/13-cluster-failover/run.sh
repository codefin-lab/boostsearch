#!/usr/bin/env bash
# Three nodes, a replica, and one of them killed while writes are going in.
#
# This example starts and stops its own nodes. It does not use lib.sh's single
# server, and it must not be run at the same time as the chaos gate.
set -euo pipefail
cd "$(dirname "$0")/../.."
BIN=${BIN:-./target/release/boostsearch}
ROOT=${ROOT:-/tmp/boost-cluster-example}
PORTS=(9340 9341 9342)
TPORTS=(9440 9441 9442)
NAMES=(n1 n2 n3)
SEEDS="127.0.0.1:9440,127.0.0.1:9441,127.0.0.1:9442"
N1="http://127.0.0.1:9340"

_n=0
step() { _n=$((_n + 1)); printf '\n\033[1m== %d. %s\033[0m\n' "$_n" "$*"; }
note() { printf '   %s\n' "$*"; }
req() {
  local m=$1 p=$2 b=${3-} url=${4-$N1}
  if [ -n "$b" ]; then curl -sS -X "$m" "$url$p" -H 'Content-Type: application/json' -d "$b"
  else curl -sS -X "$m" "$url$p"; fi
  echo
}

_fails=0
fails() { _fails=$((_fails + 1)); }

# nodes_now -- how many nodes the cluster says it has, 0 if it will not say
nodes_now() {
  curl -sS "$N1/_cluster/health" 2>/dev/null \
    | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("number_of_nodes", 0))
except Exception: print(0)'
}

# until_nodes N SECONDS -- wait for the cluster to have N nodes.
#
# `wait_for_nodes=N` on the health API is meant to do this, and asking it the
# moment the last node answers HTTP came back saying one node and `timed_out`,
# where the cluster in fact formed a few seconds later. Whether that is the
# API or the timing of the question is worth its own check; a script that
# depends on the answer should not find out the hard way, so this asks again.
until_nodes() {
  local want=$1 seconds=${2:-60} waited=0
  while [ "$(nodes_now)" != "$want" ] && [ "$waited" -lt "$seconds" ]; do
    sleep 1
    waited=$((waited + 1))
  done
}

# expect_nodes N -- the cluster has this many nodes, or the example says so
expect_nodes() {
  local got
  got=$(curl -sS "$N1/_cluster/health" 2>/dev/null \
    | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("number_of_nodes", 0))
except Exception: print(0)')
  if [ "$got" = "$1" ]; then
    printf '   \033[32mok\033[0m  %s nodes%s\n' "$got" "${2:+ -- $2}"
  else
    printf '   \033[31mNOT WHAT WAS EXPECTED\033[0m  %s nodes, expected %s%s\n' "$got" "$1" "${2:+ -- $2}" >&2
    fails
  fi
}

# expect_health COLOUR [what]
expect_health() {
  local got
  got=$(curl -sS "$N1/_cluster/health" 2>/dev/null \
    | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("status", "unknown"))
except Exception: print("unknown")')
  if [ "$got" = "$1" ]; then
    printf '   \033[32mok\033[0m  the cluster is %s%s\n' "$got" "${2:+ -- $2}"
  else
    printf '   \033[31mNOT WHAT WAS EXPECTED\033[0m  the cluster is %s, expected %s%s\n' "$got" "$1" "${2:+ -- $2}" >&2
    fails
  fi
}

# expect_not_red [what] -- no shard has lost every copy
expect_not_red() {
  local got
  got=$(curl -sS "$N1/_cluster/health" 2>/dev/null \
    | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("status", "unknown"))
except Exception: print("unknown")')
  if [ "$got" != "red" ] && [ "$got" != "unknown" ]; then
    printf '   \033[32mok\033[0m  the cluster is %s, not red%s\n' "$got" "${1:+ -- $1}"
  else
    printf '   \033[31mNOT WHAT WAS EXPECTED\033[0m  the cluster is %s%s\n' "$got" "${1:+ -- $1}" >&2
    fails
  fi
}

# counts_now -- what each node's own copy answers, as one line
counts_now() {
  local counts=""
  for i in 0 1 2; do
    local c
    c=$(curl -sS -X POST "http://127.0.0.1:${PORTS[$i]}/orders/_search?preference=_local" \
      -H 'Content-Type: application/json' -d '{"size":0,"track_total_hits":true}' 2>/dev/null \
      | python3 -c 'import json,sys
try: print(json.load(sys.stdin)["hits"]["total"]["value"])
except Exception: print("?")')
    counts="$counts ${NAMES[$i]}=$c"
  done
  echo "$counts"
}

# expect_same_counts -- every node's own copy answers the same number, once
# the cluster has settled. A copy being filled is legitimately behind, so the
# question is only worth asking after it has had the chance to catch up --
# which is what the chaos harness does too, and why it waits before comparing.
expect_same_counts() {
  local waited=0
  while [ "$waited" -lt 60 ]; do
    local now values
    now=$(counts_now)
    values=$(echo "$now" | tr ' ' '\n' | sed 's/.*=//' | sort -u | grep -c . || true)
    if [ "$values" = "1" ]; then
      printf '   \033[32mok\033[0m  every copy answers the same:%s\n' "$now"
      return 0
    fi
    sleep 2
    waited=$((waited + 2))
  done
  printf '   \033[31mNOT WHAT WAS EXPECTED\033[0m  the copies still disagree after %ss:%s\n' \
    "$waited" "$(counts_now)" >&2
  fails
  return 0
}

_unused_expect_same_counts() {
  local counts=""
  for i in 0 1 2; do
    local c
    c=$(curl -sS -X POST "http://127.0.0.1:${PORTS[$i]}/orders/_search?preference=_local" \
      -H 'Content-Type: application/json' -d '{"size":0,"track_total_hits":true}' 2>/dev/null \
      | python3 -c 'import json,sys
try: print(json.load(sys.stdin)["hits"]["total"]["value"])
except Exception: print("?")')
    counts="$counts ${NAMES[$i]}=$c"
  done
  local uniq
  uniq=$(echo "$counts" | tr ' ' '\n' | grep -c . || true)
  local values
  values=$(echo "$counts" | tr ' ' '\n' | sed 's/.*=//' | sort -u | grep -c . || true)
  if [ "$values" = "1" ]; then
    printf '   \033[32mok\033[0m  every copy answers the same:%s\n' "$counts"
  else
    printf '   \033[31mNOT WHAT WAS EXPECTED\033[0m  the copies disagree:%s\n' "$counts" >&2
    fails
  fi
}

cleanup() {
  for p in "${PORTS[@]}" "${TPORTS[@]}"; do
    pids=$(lsof -ti tcp:"$p" -sTCP:LISTEN 2>/dev/null || true)
    [ -n "$pids" ] && kill -9 $pids 2>/dev/null || true
  done
}
trap cleanup EXIT
cleanup
rm -rf "$ROOT"; mkdir -p "$ROOT/logs"

start_node() {
  local i=$1
  BOOSTSEARCH_ADDR=127.0.0.1:${PORTS[$i]} \
  BOOSTSEARCH_TRANSPORT_PORT=${TPORTS[$i]} \
  BOOSTSEARCH_DATA="$ROOT/${NAMES[$i]}" \
  BOOSTSEARCH_NODE_NAME=${NAMES[$i]} \
  BOOSTSEARCH_DISCOVERY_SEED_HOSTS="$SEEDS" \
  BOOSTSEARCH_CLUSTER_INITIAL_CLUSTER_MANAGER_NODES="n1,n2,n3" \
  "$BIN" > "$ROOT/logs/${NAMES[$i]}.log" 2>&1 &
}

step "three nodes, which find each other by their transport addresses"
for i in 0 1 2; do start_node $i; done
for i in 0 1 2; do
  for _ in $(seq 60); do curl -sf "http://127.0.0.1:${PORTS[$i]}/" > /dev/null 2>&1 && break; sleep 0.5; done
done
until_nodes 3 60
req GET "/_cluster/health?wait_for_nodes=3&wait_for_status=green&timeout=30s"
expect_nodes 3 "all three found each other"
req GET "/_cat/nodes?v&h=name,node.role,cluster_manager,http,version"

step "who is the cluster manager, and what the cluster state says"
req GET "/_cluster/state/cluster_manager_node,nodes"
req GET "/_cat/cluster_manager?v"

step "an index with a replica, so a copy of every shard is on another node"
req PUT "/orders" '{ "settings": { "number_of_shards": 3, "number_of_replicas": 1 } }'
req GET "/_cluster/health/orders?wait_for_status=green&timeout=60s"
expect_health green "three shards and their replicas are all placed"
req GET "/_cat/shards/orders?v&h=index,shard,prirep,state,docs,node"
note "each shard appears twice, p and r, and never twice on the same node"

step "write through every node, not just one"
for i in 0 1 2; do
  curl -sS -X POST "http://127.0.0.1:${PORTS[$i]}/orders/_bulk?refresh=true" \
    -H 'Content-Type: application/x-ndjson' --data-binary @- > /dev/null <<ND
{"index":{"_id":"w${i}-1"}}
{"who":"${NAMES[$i]}","n":1}
{"index":{"_id":"w${i}-2"}}
{"who":"${NAMES[$i]}","n":2}
ND
done
req GET "/orders/_count"

step "kill n3 while it holds copies"
pids=$(lsof -ti tcp:${PORTS[2]} -sTCP:LISTEN 2>/dev/null || true)
[ -n "$pids" ] && kill -9 $pids
until_nodes 2 60
req GET "/_cluster/health?timeout=20s"
expect_nodes 2 "n3 is gone"
expect_not_red "every shard still has a copy; with one replica and two nodes left there is room for all of them, so it can even get back to green"
req GET "/_cat/shards/orders?v&h=index,shard,prirep,state,node"

step "the cluster still answers, and still takes writes"
req POST "/orders/_doc/after-the-loss?refresh=true" '{ "who": "written while n3 was down", "n": 3 }'
req GET "/orders/_count"

step "why a shard is where it is -- or is not anywhere"
req POST "/_cluster/allocation/explain" '{ "index": "orders", "shard": 0, "primary": false }' || true

step "bring n3 back: it is filled from the primaries rather than starting empty"
start_node 2
for _ in $(seq 60); do curl -sf "http://127.0.0.1:${PORTS[2]}/" > /dev/null 2>&1 && break; sleep 0.5; done
until_nodes 3 90
req GET "/_cluster/health?wait_for_status=green&timeout=60s"
expect_nodes 3 "n3 is back"
expect_health green "and its copies have been filled again"
req GET "/_cat/recovery/orders?v&h=index,shard,type,stage,source_node,target_node,files_percent" || true
req GET "/_cat/shards/orders?v&h=index,shard,prirep,state,docs,node"

step "every copy agrees on the count -- ask each node for its own"
for i in 0 1 2; do
  note "--- ${NAMES[$i]}"
  req GET "/orders/_search?preference=_local" '{"size":0,"track_total_hits":true}' "http://127.0.0.1:${PORTS[$i]}"
done

step "search across the whole cluster, and see which shards answered"
req GET "/orders/_search" '{ "size": 0, "aggs": { "who": { "terms": { "field": "who.keyword" } } } }'

step "moving a shard by hand"
req POST "/_cluster/reroute?explain=true" '{ "commands": [] }'
note "the commands array is where move / allocate_replica / cancel go"

step "settings that live in the cluster rather than on one node"
req PUT "/_cluster/settings" '{ "transient": { "cluster.routing.allocation.enable": "all" } }'
req GET "/_cluster/settings?flat_settings=true"

step "what each node is doing"
req GET "/_cat/nodes?v&h=name,heap.percent,ram.percent,cpu,load_1m,node.role"
req GET "/_nodes/stats/indices?filter_path=nodes.*.name,nodes.*.indices.docs" || true
req GET "/_cat/allocation?v"

step "what this example claimed, checked rather than assumed"
expect_same_counts
echo
if [ "$_fails" = 0 ]; then
  printf '\033[1;32mRESULT\033[0m every check in this example held\n'
else
  printf '\033[1;31mRESULT\033[0m %s check(s) did not hold\n' "$_fails" >&2
fi
note ""
note "the nodes are stopped when this script exits; their logs stay in $ROOT/logs"
[ "$_fails" = 0 ]
