#!/bin/sh
# Cross-cluster search, held to OpenSearch: the same requests to a local
# cluster that knows a remote, on both engines, and the answers compared.
#
# OpenSearch needs two containers on one network, the second told where the
# first is. The remote on each side is filled by OpenSearch's own
# `remote_cluster` suite, so both remotes hold the same documents.
#
#   docker network create ccs-ref
#   docker run -d --name os-ccs-remote --network ccs-ref -p 9253:9200 \
#     -e discovery.type=single-node -e DISABLE_SECURITY_PLUGIN=true \
#     opensearchproject/opensearch:3.1.0
#   docker run -d --name os-ccs-local --network ccs-ref -p 9252:9200 \
#     -e discovery.type=single-node -e DISABLE_SECURITY_PLUGIN=true \
#     -e cluster.remote.my_remote_cluster.seeds=os-ccs-remote:9300 \
#     opensearchproject/opensearch:3.1.0
#   python3 tools/yaml_runner.py --url http://127.0.0.1:9253 --manifest tools/ccs_remote_manifest.json
#
#   VELO_PORT=9320 tools/gate_node.sh &          # the remote
#   python3 tools/yaml_runner.py --url http://127.0.0.1:9320 --manifest tools/ccs_remote_manifest.json
#   VELOSEARCH_CLUSTER_REMOTE=my_remote_cluster:127.0.0.1:9320 VELO_PORT=9336 tools/gate_node.sh &
#   tools/compat_ccs.sh
#
# The remote's documents have generated ids, which differ between the
# engines by design; compare those rows by `_index` and `_source`.
set -e
A=${BENCH_A:-http://127.0.0.1:9252}
B=${BENCH_B:-http://127.0.0.1:9336}
python3 tools/compat_audit.py replay --requests tools/corpora/ccs.ndjson \
    --a "$A" --b "$B" --out /tmp/compat-ccs.json | tail -40
