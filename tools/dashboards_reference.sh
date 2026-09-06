#!/bin/sh
# OpenSearch Dashboards 3.1.0 and the engine behind it, started the way its
# own API suite starts them.
#
# The suite's config (test/api_integration/config.js) starts the server with
# settings that are not defaults, and a server started without them fails
# cases that have nothing wrong with them. Two of those settings matter more
# than the rest:
#
#   --server.xsrf.disableProtection=true   the suite's supertest sends no
#       `osd-xsrf` header, so every POST is refused without this. It is the
#       difference between 76 cases passing and 140.
#   --server.maxPayloadBytes=1759977       one case sends a body just under it
#       and expects 200, and another sends one just over and expects 413.
#
# Ports: 9221 for the engine and 5613 for the console, chosen to stay away
# from the reference containers this repository already reads from.
set -e
NET=${OSD_NET:-osd13}
OS_PORT=${OSD_OS_PORT:-9221}
UI_PORT=${OSD_UI_PORT:-5613}
VERSION=${OSD_VERSION:-3.1.0}

docker network create "$NET" 2>/dev/null || true
docker rm -f os13 osd13-ui >/dev/null 2>&1 || true

docker run -d --name os13 --network "$NET" -p "$OS_PORT":9200 \
    -e discovery.type=single-node -e DISABLE_SECURITY_PLUGIN=true \
    -e OPENSEARCH_JAVA_OPTS="-Xms2g -Xmx2g" \
    opensearchproject/opensearch:"$VERSION" >/dev/null
echo "engine starting on $OS_PORT"
until curl -sf "http://127.0.0.1:$OS_PORT/" >/dev/null 2>&1; do sleep 2; done
echo "engine up"

docker run -d --name osd13-ui --network "$NET" -p "$UI_PORT":5601 \
    -e OPENSEARCH_HOSTS="[\"http://os13:9200\"]" \
    -e DISABLE_SECURITY_DASHBOARDS_PLUGIN=true \
    opensearchproject/opensearch-dashboards:"$VERSION" \
    opensearch-dashboards \
        --server.host=0.0.0.0 \
        --status.allowAnonymous=true \
        --home.disableWelcomeScreen=false \
        --data.search.aggs.shardDelay.enabled=true \
        --server.maxPayloadBytes=1759977 \
        --server.xsrf.disableProtection=true \
        "--uiSettings.overrides[query:enhancements:enabled]=false" >/dev/null
echo "console starting on $UI_PORT"
until curl -sf "http://127.0.0.1:$UI_PORT/api/status" >/dev/null 2>&1; do sleep 3; done
echo "console up"
