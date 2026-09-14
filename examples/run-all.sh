#!/usr/bin/env bash
# The examples that can share one node, run in order against a node started
# here with everything any of them needs.
#
# Skipped: 06 and 21 (want security on, which changes every request), 13
# (starts its own three-node cluster) and 25 (reads the health of the whole
# node, which the others leave yellow). Each has `make serve` of its own.
set -uo pipefail
cd "$(dirname "$0")"
BIN=${BIN:-../target/release/boostsearch}
PORT=${PORT:-9250}
DATA=${DATA:-/tmp/boost-examples}
REPO=${REPO_DIR:-/tmp/boost-examples-repo}

[ -x "$BIN" ] || { echo "no binary at $BIN -- cargo build --release" >&2; exit 1; }

pids=$(lsof -ti tcp:$PORT -sTCP:LISTEN 2>/dev/null || true)
[ -n "$pids" ] && kill -9 $pids 2>/dev/null
rm -rf "$DATA" "$REPO"; mkdir -p "$DATA" "$REPO"

BOOSTSEARCH_ADDR=127.0.0.1:$PORT \
BOOSTSEARCH_TRANSPORT_PORT=$((PORT + 100)) \
BOOSTSEARCH_DATA="$DATA" \
BOOSTSEARCH_PATH_REPO="$REPO" \
BOOSTSEARCH_ISM_INTERVAL_MS=2000 \
BOOSTSEARCH_REINDEX_ALLOWLIST='127.0.0.1:*' \
"$BIN" > "$DATA/node.log" 2>&1 &
NODE=$!
trap 'kill $NODE 2>/dev/null' EXIT

for _ in $(seq 60); do curl -sf "http://127.0.0.1:$PORT/" > /dev/null 2>&1 && break; sleep 0.5; done
export BS="http://127.0.0.1:$PORT"
export REPO=backups

ran=0; failed=0; failures=()
for d in [0-9]*/; do
  name=${d%/}
  case "$name" in
    06-*|13-*|21-*|25-*) echo "-- skipping $name (needs its own node; see $name/README.md)"; continue ;;
  esac
  [ -x "$name/run.sh" ] || continue
  echo
  echo "################ $name"
  if "./$name/run.sh"; then ran=$((ran + 1)); else failed=$((failed + 1)); failures+=("$name"); fi
done

echo
echo "ran $ran, failed $failed"
[ $failed -gt 0 ] && printf 'failed: %s\n' "${failures[*]}"
echo "the node's log is $DATA/node.log"
[ "$failed" -eq 0 ]
