#!/usr/bin/env bash
# Logs: an index that rolls itself over, ages, and is deleted by its own policy,
# with the aggregations an operations dashboard actually asks for.
source "$(dirname "$0")/lib.sh"

step "a template, so every index the alias rolls into is shaped the same"
gone "/_index_template/logs-template"
gone "/logs-000001"; gone "/logs-000002"; gone "/logs-000003"
gone "/_plugins/_ism/policies/logs-lifecycle"
reqf PUT "/_index_template/logs-template" requests/01-a-template-so-every-index-the.json

step "the policy: roll at 5 documents, go cold after a moment, then be deleted"
reqf PUT "/_plugins/_ism/policies/logs-lifecycle" requests/02-the-policy-roll-at-5-documents.json

step "the first index, and the alias everything writes through"
req PUT "/logs-000001" '{ "aliases": { "logs": { "is_write_index": true } } }'
quiet POST "/_plugins/_ism/add/logs-000001" '{ "policy_id": "logs-lifecycle" }'

step "a morning of traffic, written through the alias"
python3 - "$VS" <<'PY' > /tmp/logs.ndjson
import json, random, sys, datetime
random.seed(7)
base = datetime.datetime(2026, 9, 12, 6, 0, 0)
services = ["checkout", "catalogue", "auth", "search"]
hosts = ["node-a", "node-b", "node-c"]
paths = ["/api/cart", "/api/items", "/api/login", "/api/search"]
out = []
for i in range(1200):
    t = base + datetime.timedelta(seconds=i * 21)
    svc = random.choice(services)
    # checkout has a bad half-hour in the middle of the morning
    bad = svc == "checkout" and 3000 < i < 4200
    status = random.choice([500, 503, 200]) if bad else random.choices([200, 201, 404, 500], [80, 8, 9, 3])[0]
    lvl = "ERROR" if status >= 500 else ("WARN" if status >= 400 else "INFO")
    out.append(json.dumps({"index": {}}))
    out.append(json.dumps({
        "@timestamp": t.isoformat() + "Z", "service": svc, "host": random.choice(hosts),
        "level": lvl, "status": status,
        "took_ms": round(random.lognormvariate(3.0 if not bad else 4.2, 0.5), 2),
        "bytes": random.randint(200, 90000), "path": random.choice(paths),
        "message": f"{svc} answered {status} in the morning run",
    }))
print("\n".join(out))
PY
ndjson "/logs/_bulk?refresh=true" /tmp/logs.ndjson > /dev/null
expect_docs logs 1200 "a morning of traffic, through the alias"
req GET "/_cat/indices/logs-*?v&h=index,docs.count,status"

step "the shape of the morning, by service"
reqf GET "/logs/_search" requests/03-the-shape-of-the-morning-by.json

step "only the half hours that were actually bad, and how fast they got worse"
reqf GET "/logs/_search" requests/04-only-the-half-hours-that-were.json

step "the latency an SLO is written against"
reqf GET "/logs/_search" requests/05-the-latency-an-slo-is-written.json

step "the noisiest paths, and what each of them costs in bytes"
reqf GET "/logs/_search" requests/06-the-noisiest-paths-and-what-each.json

step "the terms that mark out the bad window from the rest of the morning"
reqf GET "/logs/_search" requests/07-the-terms-that-mark-out-the.json

step "the policy at work -- it may take a few ticks"
i=0
while [ $i -lt 24 ]; do
  sleep 2; i=$((i + 1))
  n=$("${CURL[@]}" "$VS/_cat/indices/logs-*?h=index" 2>/dev/null | wc -l | tr -d ' ')
  [ "$n" -gt 1 ] && break
done
req GET "/_plugins/_ism/explain/logs-000001"
req GET "/_cat/indices/logs-*?v&h=index,docs.count,status"
note "if only logs-000001 is listed, the node's ISM job interval is longer than this"
note "example waits: start it with VELOSEARCH_ISM_INTERVAL_MS=2000 to watch the whole life"

step "the alias always points at the one index that takes writes"
req GET "/_cat/aliases/logs?v"

step "what this example leaves behind, checked rather than assumed"
done_
