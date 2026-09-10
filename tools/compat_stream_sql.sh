#!/bin/sh
# The guarantee: the same request, to both engines, and the answers compared.
#
# `tools/compat_audit.py replay` does the comparing; these are the two corpora
# it is given -- data streams and SQL/PPL, the two features OpenSearch keeps
# outside the conformance suite this repository runs, and the two where every
# recent defect has been found.
#
# It needs a real OpenSearch to compare against. One run of it is enough to
# find a difference; it is not something CI can do without a container.
#
#   docker run -d --name os-ref -p 9251:9200 \
#     -e discovery.type=single-node -e DISABLE_SECURITY_PLUGIN=true \
#     opensearchproject/opensearch:3.1.0
#   BOOST_PORT=9252 tools/gate_node.sh &
#   tools/compat_stream_sql.sh
set -e
A=${BENCH_A:-http://127.0.0.1:9251}
B=${BENCH_B:-http://127.0.0.1:9252}
for corpus in data_stream sql; do
    echo "== $corpus"
    python3 tools/compat_audit.py replay \
        --requests "tools/corpora/$corpus.ndjson" \
        --a "$A" --b "$B" \
        --out "/tmp/compat-$corpus.json" | tail -40
done
