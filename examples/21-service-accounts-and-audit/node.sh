#!/usr/bin/env sh
# A node configured for exactly what this example needs, on a port of its own.
# It runs in the foreground; stop it with control-C. `make serve` calls this.
set -e
cd "$(dirname "$0")"
[ -f .env ] && . ./.env
PORT=${PORT:-9281}
BIN=${BIN:-../../target/release/velosearch}
DATA=${DATA:-/tmp/velo-example-21-service-accounts-and-audit}

[ -x "$BIN" ] || { echo "no binary at $BIN -- run cargo build --release in the repository root" >&2; exit 1; }
rm -rf "$DATA"; mkdir -p "$DATA"

echo "starting on http://127.0.0.1:$PORT, data in $DATA"
VELOSEARCH_ADDR=127.0.0.1:$PORT \
VELOSEARCH_TRANSPORT_PORT=$((PORT + 100)) \
VELOSEARCH_DATA="$DATA" \
VELOSEARCH_DISABLED=false \
VELOSEARCH_RESTAPI_ROLES_ENABLED=all_access \
VELOSEARCH_INITIAL_ADMIN_PASSWORD="${VELOSEARCH_INITIAL_ADMIN_PASSWORD:-Example-Passphrase-2026}" \
VELOSEARCH_AUDIT_TYPE=${VELOSEARCH_AUDIT_TYPE:-internal_opensearch} \
exec "$BIN"
