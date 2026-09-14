#!/usr/bin/env sh
# A node configured for exactly what this example needs, on a port of its own.
# It runs in the foreground; stop it with control-C. `make serve` calls this.
set -e
cd "$(dirname "$0")"
[ -f .env ] && . ./.env
PORT=${PORT:-9269}
BIN=${BIN:-../../target/release/boostsearch}
DATA=${DATA:-/tmp/boost-example-09-multilingual-analysis}

[ -x "$BIN" ] || { echo "no binary at $BIN -- run cargo build --release in the repository root" >&2; exit 1; }
rm -rf "$DATA"; mkdir -p "$DATA"

echo "starting on http://127.0.0.1:$PORT, data in $DATA"
BOOSTSEARCH_ADDR=127.0.0.1:$PORT \
BOOSTSEARCH_TRANSPORT_PORT=$((PORT + 100)) \
BOOSTSEARCH_DATA="$DATA" \
BOOSTSEARCH_PHONETIC_RULES=/tmp/phonetic-rules \
exec "$BIN"
