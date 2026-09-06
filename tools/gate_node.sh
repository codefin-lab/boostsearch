#!/bin/sh
# The node the gates are run against, with everything the suites expect.
#
# The corpus asks a node for things that are arranged outside it: an
# attribute the cat and settings tests read back, the geoip databases and the
# phonetic rule files (neither vendored -- see docs/geoip.md and
# docs/phonetic.md), where a URL repository may be read from, and which
# clusters a reindex may read from. A node
# started without these fails sections that have nothing wrong with them, so
# this is the one way to start it.
#
# One suite is written against a cluster with no ingest node, which is a
# different cluster rather than a different request -- BOOST_ROLES starts one.
#
#   tools/gate_node.sh            starts it on 9213
#   BOOST_PORT=9214 tools/gate_node.sh
#   BOOST_PORT=9214 BOOST_DATA=/tmp/boost-noingest \
#     BOOST_ROLES=data,cluster_manager,remote_cluster_client tools/gate_node.sh
set -e
PORT=${BOOST_PORT:-9213}
DATA=${BOOST_DATA:-/tmp/boost-gate}
REPO=${BOOST_URL_REPO:-/tmp/boost-url-repo}
FIXTURE=${BOOST_URL_FIXTURE_PORT:-9280}
rm -rf "$DATA" "$REPO"
BOOSTSEARCH_ADDR=127.0.0.1:$PORT \
BOOSTSEARCH_DATA="$DATA" \
BOOSTSEARCH_NODE_ATTRS=testattr=test \
BOOSTSEARCH_GEOIP_PATH=${BOOST_GEOIP:-/tmp/geoip-db} \
BOOSTSEARCH_PHONETIC_RULES=${BOOST_PHONETIC:-/tmp/phonetic-rules} \
BOOSTSEARCH_PATH_REPO="$REPO" \
BOOSTSEARCH_URL_ALLOWED="http://snapshot.test*,http://127.0.0.1:$FIXTURE*" \
BOOSTSEARCH_REINDEX_ALLOWLIST="127.0.0.1:*" \
BOOSTSEARCH_NODE_ROLES="${BOOST_ROLES:-cluster_manager,data,ingest,remote_cluster_client}" \
exec target/release/boostsearch
