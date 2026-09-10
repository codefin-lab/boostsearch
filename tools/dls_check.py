#!/usr/bin/env python3
"""Document- and field-level security, through every path that reads a document.

A document-level filter is only as good as the least careful path in the
server: `_search` honouring it while `_count`, an aggregation, `top_hits`,
`_mget`, `_termvectors`, `docvalue_fields` or a highlight does not is a leak
with a filter in front of it. The suites do not check this -- the security
plugin's own tests are not part of the corpus -- so this is what does.

A node is started with security on, an index is written with documents
belonging to two people, and a role is made that may see one person's
documents and may not see one field at all. Every path that can read a
document is then asked, as that role, whether it agrees.

    python3 tools/dls_check.py
    python3 tools/dls_check.py --port 9276 --transport 9376
"""
import argparse
import base64
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent


def call(url, method, path, who, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url + path, data=data, method=method,
                                 headers={"content-type": "application/json"})
    token = base64.b64encode(f"{who[0]}:{who[1]}".encode()).decode()
    req.add_header("authorization", f"Basic {token}")
    try:
        with urllib.request.urlopen(req, timeout=30) as a:
            return a.status, json.loads(a.read() or b"{}")
    except urllib.error.HTTPError as e:
        raw = e.read()
        try:
            return e.code, json.loads(raw or b"{}")
        except json.JSONDecodeError:
            return e.code, {}
    except Exception as e:
        return 0, {"no answer": str(e)[:120]}

ap = argparse.ArgumentParser()
ap.add_argument("--binary", default=str(ROOT / "target" / "release" / "boostsearch"))
ap.add_argument("--port", type=int, default=9276)
ap.add_argument("--transport", type=int, default=9376)
args = ap.parse_args()
PORT, TPORT = args.port, args.transport

data = tempfile.mkdtemp(prefix="boost-dls-")
config = pathlib.Path(data) / "config"
(config / "security").mkdir(parents=True, exist_ok=True)
env = dict(os.environ)
env.update({"BOOSTSEARCH_ADDR": f"127.0.0.1:{PORT}", "BOOSTSEARCH_DATA": data,
            "BOOSTSEARCH_CONFIG": str(config), "BOOSTSEARCH_TRANSPORT_PORT": str(TPORT),
            "BOOSTSEARCH_DISABLED": "false", "BOOSTSEARCH_RESTAPI_ROLES_ENABLED": "all_access"})
log = open(pathlib.Path(data) / "node.log", "w")
node = subprocess.Popen([args.binary], env=env, stdout=log, stderr=subprocess.STDOUT)
url = f"http://127.0.0.1:{PORT}"
admin = ("admin", "admin")
for _ in range(60):
    if call(url, "GET", "/", admin)[0]:
        break
    time.sleep(1)
else:
    print(f"the node did not start; its log is in {data}/node.log")
    sys.exit(2)

try:
    call(url, "PUT", "/secret", admin, {"mappings": {"properties": {
        "owner": {"type": "keyword"}, "ssn": {"type": "keyword"}, "note": {"type": "text"}}}})
    for i, owner in [(1, "alice"), (2, "bob"), (3, "bob")]:
        call(url, "PUT", f"/secret/_doc/{i}?refresh=true", admin,
             {"owner": owner, "ssn": f"000-00-000{i}", "note": f"note for {owner}"})
    call(url, "PUT", "/_plugins/_security/api/roles/dls_role", admin, {
        "index_permissions": [{
            "index_patterns": ["secret*"],
            "dls": json.dumps({"term": {"owner": "alice"}}),
            "fls": ["~ssn"],
            "allowed_actions": ["read"],
        }]})
    call(url, "PUT", "/_plugins/_security/api/internalusers/dee", admin, {"password": "dee-password-1"})
    call(url, "PUT", "/_plugins/_security/api/rolesmapping/dls_role", admin, {"users": ["dee"]})
    dee = ("dee", "dee-password-1")

    bad = []
    asked = []

    def check(what, ok, detail=""):
        asked.append(what)
        print(("  ok     " if ok else "  LEAK   ") + what + ("" if ok else f"  <- {detail}"))
        if not ok:
            bad.append(what)

    s, r = call(url, "POST", "/secret/_search", dee, {"query": {"match_all": {}}})
    ids = [h["_id"] for h in r.get("hits", {}).get("hits", [])]
    check("search sees only alice's document", ids == ["1"], f"{s} {ids}")
    check("search hides the masked field", all("ssn" not in h["_source"] for h in r.get("hits", {}).get("hits", [])), r)

    s, r = call(url, "POST", "/secret/_count", dee, {"query": {"match_all": {}}})
    check("count counts only what is visible", r.get("count") == 1, f"{s} {r}")

    s, r = call(url, "POST", "/secret/_search", dee, {"size": 0, "aggs": {"o": {"terms": {"field": "owner"}}}})
    buckets = [b["key"] for b in r.get("aggregations", {}).get("o", {}).get("buckets", [])]
    check("a terms aggregation sees only what is visible", buckets == ["alice"], f"{s} {buckets}")

    s, r = call(url, "POST", "/secret/_search", dee, {"size": 0, "aggs": {"c": {"cardinality": {"field": "owner"}}}})
    check("cardinality sees only what is visible", r.get("aggregations", {}).get("c", {}).get("value") == 1, r)

    s, r = call(url, "GET", "/secret/_doc/2", dee)
    check("a get of a hidden document is not found", s == 404 or r.get("found") is False, f"{s} {r}")

    s, r = call(url, "POST", "/_mget", dee, {"docs": [{"_index": "secret", "_id": "2"}, {"_index": "secret", "_id": "1"}]})
    found = [(d.get("_id"), d.get("found")) for d in r.get("docs", [])]
    check("mget hides the hidden document", found == [("2", False), ("1", True)], f"{s} {found}")
    check("mget hides the masked field", all("ssn" not in (d.get("_source") or {}) for d in r.get("docs", [])), r)

    s, r = call(url, "GET", "/secret/_termvectors/2", dee, None)
    check("termvectors of a hidden document say nothing", (r.get("found") is False) or s >= 400, f"{s} {r}")

    s, r = call(url, "POST", "/secret/_termvectors/1", dee, {"fields": ["ssn", "note"]})
    check("termvectors hide the masked field", "ssn" not in (r.get("term_vectors") or {}), f"{s} {r}")

    s, r = call(url, "GET", "/secret/_field_caps?fields=*", dee)
    check("field_caps hides the masked field", "ssn" not in (r.get("fields") or {}), f"{s} {list((r.get('fields') or {}).keys())}")

    s, r = call(url, "POST", "/secret/_search", dee, {"query": {"term": {"ssn": "000-00-0002"}}})
    check("a query on a masked field finds nothing", r.get("hits", {}).get("total", {}).get("value") == 0, f"{s} {r}")

    s, r = call(url, "POST", "/secret/_search", dee, {"size": 0, "aggs": {"s": {"terms": {"field": "ssn"}}}})
    b = (r.get("aggregations") or {}).get("s", {}).get("buckets")
    check("an aggregation on a masked field yields nothing", not b, f"{s} {b}")

    s, r = call(url, "POST", "/secret/_search", dee, {"query": {"match_all": {}}, "sort": [{"ssn": "asc"}]})
    check("sorting by a masked field is refused or yields the visible one only",
          s >= 400 or [h["_id"] for h in r.get("hits", {}).get("hits", [])] == ["1"], f"{s} {r}")

    s, r = call(url, "POST", "/secret/_search", dee, {"query": {"match_all": {}}, "docvalue_fields": ["ssn"]})
    leaked = any("ssn" in (h.get("fields") or {}) for h in r.get("hits", {}).get("hits", []))
    check("docvalue_fields does not return a masked field", not leaked, f"{s} {r}")

    s, r = call(url, "POST", "/secret/_search", dee, {"query": {"match_all": {}}, "stored_fields": ["ssn"]})
    leaked = any("ssn" in (h.get("fields") or {}) for h in r.get("hits", {}).get("hits", []))
    check("stored_fields does not return a masked field", not leaked, f"{s} {r}")

    s, r = call(url, "POST", "/secret/_search?scroll=1m", dee, {"query": {"match_all": {}}, "size": 10})
    ids = [h["_id"] for h in r.get("hits", {}).get("hits", [])]
    check("a scroll sees only what is visible", ids == ["1"], f"{s} {ids}")

    s, r = call(url, "POST", "/secret/_msearch", dee, None)
    s, r = call(url, "GET", "/secret/_search?q=owner:bob", dee)
    check("a uri search sees only what is visible", r.get("hits", {}).get("total", {}).get("value") == 0, f"{s} {r}")

    s, r = call(url, "GET", "/secret/_source/2", dee)
    check("the _source endpoint hides a hidden document", s >= 400, f"{s} {r}")

    s, r = call(url, "GET", "/secret/_explain/2", dee, {"query": {"match_all": {}}})
    check("explain says nothing about a hidden document", (r.get("matched") is False) or s >= 400, f"{s} {r}")

    s, r = call(url, "POST", "/secret/_search", dee, {"query": {"match_all": {}}, "_source": {"includes": ["ssn", "owner"]}})
    leaked = any("ssn" in (h.get("_source") or {}) for h in r.get("hits", {}).get("hits", []))
    check("_source includes cannot ask for a masked field", not leaked, f"{s} {r}")

    s, r = call(url, "POST", "/secret/_search", dee, {"query": {"match_all": {}}, "highlight": {"fields": {"ssn": {}}}})
    leaked = any("ssn" in (h.get("highlight") or {}) for h in r.get("hits", {}).get("hits", []))
    check("highlighting cannot show a masked field", not leaked, f"{s} {r}")

    s, r = call(url, "POST", "/secret/_search", dee, {"size": 0, "aggs": {"t": {"top_hits": {"size": 5}}}})
    hits = (((r.get("aggregations") or {}).get("t") or {}).get("hits") or {}).get("hits") or []
    check("top_hits sees only what is visible", [h["_id"] for h in hits] == ["1"], f"{s} {hits}")
    check("top_hits hides the masked field", all("ssn" not in (h.get("_source") or {}) for h in hits), hits)

    # A filtered alias is the same promise made without a role, and it broke
    # twice in the same place: named beside another index, the search ran its
    # shards on threads that never saw the alias's filter, and bob's
    # documents came back through a view that exists to hide them.
    call(url, "PUT", "/beside", admin, {"mappings": {"properties": {"owner": {"type": "keyword"}}}})
    call(url, "PUT", "/beside/_doc/b1?refresh=true", admin, {"owner": "carol"})
    call(url, "POST", "/_aliases", admin, {"actions": [
        {"add": {"index": "secret", "alias": "alice_view", "filter": {"term": {"owner": "alice"}}}}]})
    for path in ["/alice_view/_search", "/alice_view,beside/_search", "/beside,alice_view/_search"]:
        s, r = call(url, "POST", path + "?size=20", admin, {"query": {"match_all": {}}})
        seen = sorted(h["_id"] for h in r.get("hits", {}).get("hits", []))
        want = ["1", "b1"] if "beside" in path else ["1"]
        check(f"a filtered alias holds its filter: {path}", seen == want, f"{s} {seen}")
    s, r = call(url, "POST", "/alice_view,beside/_count", admin, {"query": {"match_all": {}}})
    check("a filtered alias holds its filter in a count beside another index", r.get("count") == 2, f"{s} {r}")
    s, r = call(url, "POST", "/alice_view,beside/_search", admin,
                {"size": 0, "aggs": {"o": {"terms": {"field": "owner"}}}})
    keys = sorted(b["key"] for b in (r.get("aggregations") or {}).get("o", {}).get("buckets", []))
    check("a filtered alias holds its filter in an aggregation beside another index",
          keys == ["alice", "carol"], f"{s} {keys}")

    print()
    print(f"  {len(asked)} paths asked")
    print()
    if bad:
        print(f"RESULT {len(bad)} path(s) did not honour the filter")
        sys.exit(1)
    print("RESULT every path that reads a document honoured the filter")
finally:
    node.terminate()
    try: node.wait(timeout=10)
    except Exception: node.kill()
