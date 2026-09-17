#!/usr/bin/env python3
"""Points in time and scrolls hold the index as it was when they were opened.

A point in time (and a scroll, which reads through one) must answer with the
documents as they stood when it was opened, however the index changes while
it is read: an updated document once with its old source, a deleted one still
there, a new one absent, and paging through it with `search_after` or scroll
batches returning every document exactly once. Refreshes and merges between
pages must not change that.

The script starts its own nodes -- one alone, then a cluster of three -- runs
the same checks against each, and fails on any difference. On the cluster the
indices are placed away from the node that opens the context, and pages are
asked of every node in turn, so a context has to be found wherever it lives.
With security on, it also checks that one caller cannot list, read or clear
another caller's contexts.

  pit_check.py --binary target/release/velosearch
  pit_check.py --binary target/release/velosearch --only single,cluster,security

Ports: HTTP 9751-9753, transport 9851-9853; data under /tmp/pitsql-pit-*.
"""
import argparse
import base64
import json
import os
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request

ADMIN_PASSWORD = "Pit-Check-Key-2026"
HTTP = [9751, 9752, 9753]
TRANSPORT = [9851, 9852, 9853]
NAMES = ["n1", "n2", "n3"]
failures = []
checks = 0


def call(base, method, path, body=None, auth=None, timeout=60):
    data = None if body is None else json.dumps(body).encode()
    headers = {"content-type": "application/json"}
    if auth:
        headers["authorization"] = "Basic " + base64.b64encode(f"{auth[0]}:{auth[1]}".encode()).decode()
    req = urllib.request.Request(base + path, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw = r.read()
            status = r.status
    except urllib.error.HTTPError as e:
        raw = e.read()
        status = e.code
    try:
        return status, json.loads(raw or b"{}")
    except ValueError:
        return status, raw.decode(errors="replace")


def expect(what, ok, detail=""):
    global checks
    checks += 1
    if ok:
        print(f"  ok    {what}")
    else:
        print(f"  FAIL  {what}  {detail}"[:1200])
        failures.append(what)


class Nodes:
    """Nodes this script started, stopped by what it holds rather than by port."""

    def __init__(self, binary, count, tag, security=False):
        self.binary = binary
        self.procs = []
        self.bases = [f"http://127.0.0.1:{HTTP[i]}" for i in range(count)]
        self.root = f"/tmp/pitsql-pit-{tag}"
        shutil.rmtree(self.root, ignore_errors=True)
        seeds = ",".join(f"127.0.0.1:{TRANSPORT[i]}" for i in range(count))
        for i in range(count):
            data = os.path.join(self.root, NAMES[i])
            config = os.path.join(data, "config")
            os.makedirs(os.path.join(config, "security"), exist_ok=True)
            env = dict(os.environ)
            env.update({
                "VELOSEARCH_ADDR": f"127.0.0.1:{HTTP[i]}",
                "VELOSEARCH_DATA": os.path.join(data, "data"),
                "VELOSEARCH_CONFIG": config,
                "VELOSEARCH_TRANSPORT_PORT": str(TRANSPORT[i]),
                "VELOSEARCH_NODE_NAME": NAMES[i],
            })
            if count > 1:
                env["VELOSEARCH_DISCOVERY_SEED_HOSTS"] = seeds
                env["VELOSEARCH_CLUSTER_INITIAL_CLUSTER_MANAGER_NODES"] = ",".join(NAMES[:count])
            if security:
                env["VELOSEARCH_DISABLED"] = "false"
                env["VELOSEARCH_RESTAPI_ROLES_ENABLED"] = "all_access"
                # a secured node has no user until one is given a password
                env["VELOSEARCH_INITIAL_ADMIN_PASSWORD"] = ADMIN_PASSWORD
            else:
                env["VELOSEARCH_DISABLED"] = "true"
            log = open(os.path.join(self.root, f"{NAMES[i]}.log"), "ab")
            self.procs.append(subprocess.Popen([binary], env=env, stdout=log, stderr=subprocess.STDOUT))
        self.auth = ("admin", ADMIN_PASSWORD) if security else None
        for base in self.bases:
            self.wait_up(base)
        if count > 1:
            deadline = time.time() + 60
            while time.time() < deadline:
                st, h = call(self.bases[0], "GET", "/_cluster/health", auth=self.auth)
                if st == 200 and h.get("number_of_nodes") == count:
                    break
                time.sleep(0.5)
            else:
                raise SystemExit("the cluster did not form")

    def wait_up(self, base):
        deadline = time.time() + 60
        while time.time() < deadline:
            try:
                st, _ = call(base, "GET", "/", auth=self.auth, timeout=2)
                if st == 200:
                    return
            except Exception:
                pass
            time.sleep(0.2)
        raise SystemExit(f"{base} did not come up")

    def stop(self):
        for p in self.procs:
            p.terminate()
        for p in self.procs:
            try:
                p.wait(15)
            except subprocess.TimeoutExpired:
                p.kill()
                p.wait()


def hits_of(answer):
    return answer.get("hits", {}).get("hits", []) if isinstance(answer, dict) else []


def as_map(hits):
    """(index, id) -> source, and how many times each came back."""
    seen = {}
    counts = {}
    for h in hits:
        key = (h["_index"], h["_id"])
        counts[key] = counts.get(key, 0) + 1
        seen[key] = h.get("_source")
    return seen, counts


def compare(what, hits, expected):
    got, counts = as_map(hits)
    dupes = sorted(k for k, c in counts.items() if c > 1)
    missing = sorted(set(expected) - set(got))
    extra = sorted(set(got) - set(expected))
    changed = sorted(k for k in expected if k in got and got[k] != expected[k])
    ok = not dupes and not missing and not extra and not changed
    detail = f"returned={len(hits)} expected={len(expected)} dupes={dupes[:5]} missing={missing[:5]} extra={extra[:5]} changed={changed[:5]}"
    expect(what, ok, detail)


class Writer:
    """The writes that go on while contexts are read, and what was there before."""

    def __init__(self, base, auth, indices):
        self.base = base
        self.auth = auth
        self.indices = indices
        self.round = 0

    def load(self, index, first, count, batches=5):
        per = count // batches
        n = first
        for b in range(batches):
            lines = []
            for _ in range(per):
                lines.append(json.dumps({"index": {"_index": index, "_id": str(n)}}))
                lines.append(json.dumps({"n": n, "tag": "orig", "text": f"document {n}"}))
                n += 1
            req = urllib.request.Request(
                self.base + "/_bulk?refresh=true",
                data=("\n".join(lines) + "\n").encode(),
                method="POST",
                headers={"content-type": "application/x-ndjson", **self.auth_header()},
            )
            with urllib.request.urlopen(req, timeout=60) as r:
                out = json.loads(r.read())
                if out.get("errors"):
                    raise SystemExit(f"bulk load failed: {json.dumps(out)[:400]}")

    def auth_header(self):
        if not self.auth:
            return {}
        return {"authorization": "Basic " + base64.b64encode(f"{self.auth[0]}:{self.auth[1]}".encode()).decode()}

    def churn(self, index, ids_update, ids_delete, new_ids):
        """Update, delete and add, then refresh and merge the index down."""
        self.round += 1
        for i in ids_update:
            st, b = call(self.base, "PUT", f"/{index}/_doc/{i}", {"n": 100000 + int(i), "tag": f"updated{self.round}", "text": "changed"}, auth=self.auth)
            if st not in (200, 201):
                raise SystemExit(f"update failed {st} {b}")
        for i in ids_delete:
            st, b = call(self.base, "DELETE", f"/{index}/_doc/{i}", auth=self.auth)
            if st not in (200, 404):
                raise SystemExit(f"delete failed {st} {b}")
        for i in new_ids:
            st, b = call(self.base, "PUT", f"/{index}/_doc/{i}", {"n": int(i), "tag": "new", "text": "new"}, auth=self.auth)
            if st not in (200, 201):
                raise SystemExit(f"add failed {st} {b}")
        call(self.base, "POST", f"/{index}/_refresh", auth=self.auth)
        call(self.base, "POST", f"/{index}/_forcemerge?max_num_segments=1", auth=self.auth)
        call(self.base, "POST", f"/{index}/_refresh", auth=self.auth)


def snapshot(base, auth, expr):
    st, b = call(base, "POST", f"/{expr}/_search", {"size": 10000, "query": {"match_all": {}}}, auth=auth)
    assert st == 200, (st, b)
    return {(h["_index"], h["_id"]): h["_source"] for h in hits_of(b)}


def page_pit(bases, auth, pit, sort, size, between=None, keep="2m"):
    """Every page of a point in time, each asked of the next node in turn."""
    out = []
    after = None
    pages = 0
    while True:
        body = {"size": size, "sort": sort, "pit": {"id": pit, "keep_alive": keep}}
        if after is not None:
            body["search_after"] = after
        base = bases[pages % len(bases)]
        st, b = call(base, "POST", "/_search", body, auth=auth)
        if st != 200:
            return out, f"page {pages} from {base}: {st} {json.dumps(b)[:400]}"
        hits = hits_of(b)
        if not hits:
            return out, None
        out.extend(hits)
        after = hits[-1]["sort"]
        pages += 1
        if between and pages in between:
            between[pages]()
        if pages > 1000:
            return out, "more than 1000 pages"


def page_scroll(bases, auth, expr, body, between=None):
    """Every batch of a scroll, the first from one node and the rest from each in turn."""
    st, b = call(bases[0], "POST", f"/{expr}/_search?scroll=2m", body, auth=auth)
    if st != 200:
        return [], f"opening: {st} {json.dumps(b)[:400]}"
    out = list(hits_of(b))
    sid = b.get("_scroll_id")
    batches = 1
    while True:
        if between and batches in between:
            between[batches]()
        base = bases[batches % len(bases)]
        st, b = call(base, "POST", "/_search/scroll", {"scroll": "2m", "scroll_id": sid}, auth=auth)
        if st != 200:
            return out, f"batch {batches} from {base}: {st} {json.dumps(b)[:400]}"
        hits = hits_of(b)
        if not hits:
            call(bases[-1], "DELETE", "/_search/scroll", {"scroll_id": sid}, auth=auth)
            return out, None
        out.extend(hits)
        sid = b.get("_scroll_id", sid)
        batches += 1
        if batches > 1000:
            return out, "more than 1000 batches"


def place(base, auth, index, node, seconds=60):
    """Ask for every shard of an index on one node, and wait for it to get there."""
    call(base, "PUT", f"/{index}/_settings", {"index.routing.allocation.include._name": node}, auth=auth)
    deadline = time.time() + seconds
    while time.time() < deadline:
        st, shards = call(base, "GET", f"/_cat/shards/{index}?format=json", auth=auth)
        if st == 200 and shards and all(s.get("state") == "STARTED" for s in shards):
            if all(s.get("node") == node for s in shards):
                return
            # the filter alone does not always move a shard already placed
            moves = [{"move": {"index": index, "shard": int(s["shard"]), "from_node": s["node"], "to_node": node}}
                     for s in shards if s.get("node") != node]
            call(base, "POST", "/_cluster/reroute", {"commands": moves}, auth=auth)
        time.sleep(1)


def wait_placed(base, auth, expr, seconds=60):
    """Wait until every shard is started and none is on its way anywhere."""
    deadline = time.time() + seconds
    where = None
    while time.time() < deadline:
        st, shards = call(base, "GET", f"/_cat/shards/{expr}?format=json", auth=auth)
        if st == 200 and shards and all(s.get("state") == "STARTED" and "->" not in (s.get("node") or "") for s in shards):
            return sorted({(s["index"], s["shard"], s["node"]) for s in shards})
        where = shards
        time.sleep(0.5)
    raise SystemExit(f"shards did not settle: {where}")


def consistency(nodes, label):
    bases = nodes.bases
    auth = nodes.auth
    a, b = "pitsql-a", "pitsql-b"
    clustered = len(bases) > 1
    for name, shards in ((a, 3), (b, 2)):
        settings = {"number_of_shards": shards, "number_of_replicas": 0}
        if clustered:
            # the indices are kept off the node that opens the contexts, and
            # each on a node of its own
            settings["index.routing.allocation.include._name"] = "n2" if name == a else "n3"
        st, out = call(bases[0], "PUT", f"/{name}", {"settings": settings, "mappings": {"properties": {"n": {"type": "long"}, "tag": {"type": "keyword"}}}}, auth=auth)
        if st != 200:
            raise SystemExit(f"create {name}: {st} {out}")
    if clustered:
        for name, node in ((a, "n2"), (b, "n3")):
            place(bases[0], auth, name, node)
        where = wait_placed(bases[0], auth, "pitsql-*")
        print(f"  shards: {where}")
        nodes_of = {(i, n) for i, _, n in where}
        expect(f"{label}: the indices are held away from n1, on two nodes", nodes_of == {(a, "n2"), (b, "n3")}, where)
    writer = Writer(bases[0], auth, [a, b])
    writer.load(a, 0, 100)
    writer.load(b, 1000, 60, batches=3)
    before_a = snapshot(bases[0], auth, a)
    before_ab = snapshot(bases[0], auth, f"{a},{b}")
    expect(f"{label}: the documents are there to begin with", len(before_a) == 100 and len(before_ab) == 160, f"{len(before_a)} {len(before_ab)}")

    # every context is opened before anything changes
    st, pit_a = call(bases[0], "POST", f"/{a}/_search/point_in_time?keep_alive=5m", auth=auth)
    expect(f"{label}: open a point in time", st == 200 and "pit_id" in pit_a, f"{st} {pit_a}")
    pit_a = pit_a.get("pit_id")
    st, pit_ab = call(bases[-1], "POST", f"/{a},{b}/_search/point_in_time?keep_alive=5m", auth=auth)
    expect(f"{label}: open a point in time over two indices", st == 200 and "pit_id" in pit_ab, f"{st} {pit_ab}")
    pit_ab = pit_ab.get("pit_id")

    # a scroll whose first batch is read now and the rest after the changes
    scroll_opened = {}

    def open_scroll(key, body):
        st, out = call(bases[0], "POST", f"/{a}/_search?scroll=5m", body, auth=auth)
        scroll_opened[key] = (st, out)

    open_scroll("plain", {"size": 9})
    open_scroll("sorted", {"size": 9, "sort": [{"n": "desc"}]})
    open_scroll("slice0", {"size": 6, "slice": {"id": 0, "max": 2}})
    open_scroll("slice1", {"size": 6, "slice": {"id": 1, "max": 2}})

    ids = sorted(before_a, key=lambda k: int(k[1]))
    writer.churn(a, [k[1] for k in ids[0:10]], [k[1] for k in ids[10:20]], [str(i) for i in range(500, 510)])
    writer.churn(b, ["1000", "1001", "1002"], ["1003", "1004"], ["1500", "1501"])
    live = snapshot(bases[0], auth, a)
    expect(f"{label}: the live index changed (sanity)", live != before_a and len(live) == 100, f"live={len(live)}")

    # without paging, from every node
    for i, base in enumerate(bases):
        st, out = call(base, "POST", "/_search", {"size": 1000, "pit": {"id": pit_a, "keep_alive": "5m"}}, auth=auth)
        compare(f"{label}: point in time, one page, asked of node {i + 1}", hits_of(out) if st == 200 else [], before_a)
        total = out.get("hits", {}).get("total", {}).get("value") if st == 200 else None
        expect(f"{label}: point in time total, node {i + 1}", total == 100, f"{st} total={total}")
    st, out = call(bases[-1], "POST", "/_search", {"size": 0, "pit": {"id": pit_a}, "query": {"term": {"tag": "orig"}}, "aggs": {"s": {"sum": {"field": "n"}}}}, auth=auth)
    want_sum = sum(v["n"] for v in before_a.values())
    got_sum = out.get("aggregations", {}).get("s", {}).get("value") if st == 200 else None
    got_total = out.get("hits", {}).get("total", {}).get("value") if st == 200 else None
    expect(f"{label}: point in time query and aggregation see the old documents", got_total == 100 and got_sum == want_sum, f"{st} total={got_total} sum={got_sum} want={want_sum} {json.dumps(out)[:300]}")

    # paged with search_after, with more changes between pages
    between = {
        2: lambda: writer.churn(a, [k[1] for k in ids[20:25]], [k[1] for k in ids[25:30]], [str(i) for i in range(510, 515)]),
        5: lambda: writer.churn(a, [k[1] for k in ids[30:35]], [k[1] for k in ids[35:40]], []),
    }
    hits, why = page_pit(bases, auth, pit_a, [{"n": "asc"}], 7, between)
    expect(f"{label}: search_after over a point in time answers every page", why is None, why)
    compare(f"{label}: search_after by n over a point in time", hits, before_a)
    hits, why = page_pit(bases, auth, pit_a, [{"_shard_doc": "asc"}], 11)
    expect(f"{label}: _shard_doc pages answer", why is None, why)
    compare(f"{label}: search_after by _shard_doc over a point in time", hits, before_a)

    # two indices at once
    st, out = call(bases[0], "POST", "/_search", {"size": 1000, "pit": {"id": pit_ab}}, auth=auth)
    compare(f"{label}: point in time over two indices, one page", hits_of(out) if st == 200 else [], before_ab)
    hits, why = page_pit(bases, auth, pit_ab, [{"n": "asc"}], 13)
    expect(f"{label}: two-index pages answer", why is None, why)
    compare(f"{label}: search_after by n over two indices", hits, before_ab)
    hits, why = page_pit(bases, auth, pit_ab, [{"_shard_doc": "asc"}], 13)
    expect(f"{label}: two-index _shard_doc pages answer", why is None, why)
    compare(f"{label}: search_after by _shard_doc over two indices", hits, before_ab)

    # the scrolls opened before the changes carry on over what was there
    def rest_of_scroll(key):
        st, first = scroll_opened[key]
        if st != 200:
            return [], f"opening: {st} {json.dumps(first)[:300]}"
        out = list(hits_of(first))
        sid = first.get("_scroll_id")
        n = 0
        while True:
            base = bases[(n + 1) % len(bases)]
            st, page = call(base, "POST", "/_search/scroll", {"scroll": "5m", "scroll_id": sid}, auth=auth)
            if st != 200:
                return out, f"batch {n + 1} from {base}: {st} {json.dumps(page)[:300]}"
            hits = hits_of(page)
            if not hits:
                return out, None
            out.extend(hits)
            sid = page.get("_scroll_id", sid)
            n += 1
            if n == 3:
                writer.churn(a, [k[1] for k in ids[40:45]], [k[1] for k in ids[45:50]], [str(i) for i in range(515, 520)])
            if n > 500:
                return out, "too many batches"

    for key in ("plain", "sorted"):
        hits, why = rest_of_scroll(key)
        expect(f"{label}: scroll ({key}) answers every batch from every node", why is None, why)
        compare(f"{label}: scroll ({key}) opened before the changes", hits, before_a)
    h0, why0 = rest_of_scroll("slice0")
    h1, why1 = rest_of_scroll("slice1")
    expect(f"{label}: sliced scroll answers every batch", why0 is None and why1 is None, f"{why0} {why1}")
    ids0 = {(h["_index"], h["_id"]) for h in h0}
    ids1 = {(h["_index"], h["_id"]) for h in h1}
    expect(f"{label}: the slices do not overlap", not (ids0 & ids1), sorted(ids0 & ids1)[:5])
    compare(f"{label}: sliced scroll opened before the changes", h0 + h1, before_a)

    # a scroll opened now, with changes and a merge between its batches
    now = snapshot(bases[0], auth, a)
    later = sorted(now, key=lambda k: k[1])
    between = {
        2: lambda: writer.churn(a, [k[1] for k in later[0:5]], [k[1] for k in later[5:10]], [str(i) for i in range(600, 605)]),
        4: lambda: writer.churn(a, [k[1] for k in later[10:15]], [k[1] for k in later[15:20]], []),
    }
    hits, why = page_scroll(bases, auth, a, {"size": 8}, between)
    expect(f"{label}: scroll with changes between batches answers", why is None, why)
    compare(f"{label}: scroll with changes and merges between batches", hits, now)

    # the answers OpenSearch gives around the edges
    st, out = call(bases[0], "GET", f"/{a}/_search/point_in_time", auth=auth)
    expect(f"{label}: GET /<index>/_search/point_in_time is 405", st == 405 and "Incorrect HTTP method" in json.dumps(out), f"{st} {out}")
    st, out = call(bases[-1], "GET", "/_search/point_in_time/_all", auth=auth)
    listed = [p.get("pit_id") for p in out.get("pits", [])] if st == 200 else []
    expect(f"{label}: list every point in time, from any node", st == 200 and pit_a in listed and pit_ab in listed, f"{st} {json.dumps(out)[:400]}")
    ok_times = st == 200 and all(p.get("creation_time", 0) > 1_600_000_000_000 and p.get("keep_alive") == 300000 for p in out.get("pits", []) if p.get("pit_id") in (pit_a, pit_ab))
    expect(f"{label}: listed points in time carry creation_time and keep_alive", ok_times, json.dumps(out)[:400])
    st, out = call(bases[0], "POST", "/_search", {"pit": {"id": "not-a-pit-id"}}, auth=auth)
    expect(f"{label}: an id that is not one is 400 invalid id", st == 400 and out.get("error", {}).get("type") == "illegal_argument_exception", f"{st} {out}")
    st, out = call(bases[0], "POST", f"/{a}/_search", {"pit": {"id": pit_a}}, auth=auth)
    expect(f"{label}: an index beside a point in time is refused", st == 400 and "cannot be used with point in time" in json.dumps(out), f"{st} {out}")
    st, out = call(bases[-1], "DELETE", "/_search/point_in_time", {"pit_id": [pit_a]}, auth=auth)
    expect(f"{label}: delete a point in time from another node", st == 200 and (out.get("pits") or [{}])[0].get("successful") is True, f"{st} {out}")
    for i, base in enumerate(bases):
        st, out = call(base, "POST", "/_search", {"pit": {"id": pit_a}}, auth=auth)
        rc = out.get("error", {}).get("root_cause", [{}])[0].get("type") if isinstance(out, dict) else None
        expect(f"{label}: a deleted point in time is 404 search_context_missing, node {i + 1}", st == 404 and out.get("error", {}).get("type") == "search_phase_execution_exception" and rc == "search_context_missing_exception", f"{st} {out}")
    st, out = call(bases[0], "POST", f"/{a}/_search/point_in_time?keep_alive=1s", auth=auth)
    short = out.get("pit_id")
    time.sleep(2.5)
    st, out = call(bases[-1], "POST", "/_search", {"pit": {"id": short}}, auth=auth)
    expect(f"{label}: an expired point in time is 404", st == 404, f"{st} {out}")
    st, out = call(bases[0], "DELETE", "/_search/point_in_time/_all", auth=auth)
    st, out = call(bases[-1], "GET", "/_search/point_in_time/_all", auth=auth)
    expect(f"{label}: delete all empties the list on every node", st == 200 and out.get("pits") == [], f"{st} {out}")
    st, out = call(bases[-1], "POST", "/_search/scroll", {"scroll_id": "velosearch-scroll-nothere"}, auth=auth)
    expect(f"{label}: an unknown scroll is 404", st == 404, f"{st} {out}")


def security(binary):
    nodes = Nodes(binary, 1, "security", security=True)
    try:
        base = nodes.base = nodes.bases[0]
        admin = nodes.auth
        call(base, "PUT", "/pitsql-public", {"settings": {"number_of_replicas": 0}}, auth=admin)
        for i in range(5):
            call(base, "PUT", f"/pitsql-public/_doc/{i}?refresh=true", {"v": i}, auth=admin)
        role = {"cluster_permissions": ["indices:data/read/scroll*", "indices:data/read/point_in_time*", "manage_point_in_time"],
                "index_permissions": [{"index_patterns": ["pitsql-public"], "allowed_actions": ["read", "manage_point_in_time"]}]}
        st, out = call(base, "PUT", "/_plugins/_security/api/roles/pitsqlreader", role, auth=admin)
        expect("security: role made", st in (200, 201), f"{st} {out}")
        for u in ("alice", "bob"):
            st, out = call(base, "PUT", f"/_plugins/_security/api/internalusers/{u}", {"password": "Test-Password-123", "opendistro_security_roles": ["pitsqlreader"]}, auth=admin)
            expect(f"security: user {u} made", st in (200, 201), f"{st} {out}")
        alice = ("alice", "Test-Password-123")
        bob = ("bob", "Test-Password-123")
        st, a = call(base, "POST", "/pitsql-public/_search?scroll=1m", {"size": 1}, auth=alice)
        expect("security: alice opens a scroll", st == 200 and a.get("_scroll_id"), f"{st} {a}")
        sid = a.get("_scroll_id")
        st, out = call(base, "DELETE", "/_search/scroll", {"scroll_id": sid}, auth=bob)
        expect("security: bob cannot clear alice's scroll by id", st == 404 and out.get("num_freed") == 0, f"{st} {out}")
        st, out = call(base, "DELETE", "/_search/scroll", {"scroll_id": "_all"}, auth=bob)
        expect("security: bob clearing _all frees none of alice's", out.get("num_freed") == 0, f"{st} {out}")
        st, out = call(base, "POST", "/_search/scroll", {"scroll": "1m", "scroll_id": sid}, auth=bob)
        expect("security: bob cannot read alice's scroll", st == 404, f"{st} {out}")
        st, out = call(base, "POST", "/_search/scroll", {"scroll": "1m", "scroll_id": sid}, auth=alice)
        expect("security: alice carries on reading her scroll", st == 200 and len(hits_of(out)) == 1, f"{st} {out}")
        st, p = call(base, "POST", "/pitsql-public/_search/point_in_time?keep_alive=1m", auth=alice)
        expect("security: alice opens a point in time", st == 200 and p.get("pit_id"), f"{st} {p}")
        pid = p.get("pit_id")
        st, out = call(base, "GET", "/_search/point_in_time/_all", auth=bob)
        expect("security: bob does not see alice's point in time listed", pid not in json.dumps(out), f"{st} {out}")
        st, out = call(base, "POST", "/_search", {"pit": {"id": pid}}, auth=bob)
        expect("security: bob cannot search alice's point in time", st == 404, f"{st} {out}")
        st, out = call(base, "DELETE", "/_search/point_in_time", {"pit_id": [pid]}, auth=bob)
        expect("security: bob cannot delete alice's point in time", st != 200 or (out.get("pits") or [{}])[0].get("successful") is not True, f"{st} {out}")
        st, out = call(base, "DELETE", "/_search/point_in_time/_all", auth=bob)
        st, out = call(base, "POST", "/_search", {"size": 0, "pit": {"id": pid}}, auth=alice)
        expect("security: alice's point in time survives bob deleting all", st == 200, f"{st} {out}")
        st, out = call(base, "DELETE", "/_search/scroll", {"scroll_id": "_all"}, auth=admin)
        expect("security: an administrator clears every scroll", st == 200 and out.get("num_freed", 0) >= 1, f"{st} {out}")
        st, out = call(base, "DELETE", "/_search/point_in_time/_all", auth=admin)
        expect("security: an administrator deletes every point in time", st == 200 and pid in json.dumps(out), f"{st} {out}")
    finally:
        nodes.stop()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default="target/release/velosearch")
    ap.add_argument("--only", default="single,cluster,security")
    args = ap.parse_args()
    binary = os.path.abspath(args.binary)
    only = set(args.only.split(","))
    if "single" in only:
        print("== one node")
        nodes = Nodes(binary, 1, "single")
        try:
            consistency(nodes, "single")
        finally:
            nodes.stop()
    if "cluster" in only:
        print("== three nodes")
        nodes = Nodes(binary, 3, "cluster")
        try:
            consistency(nodes, "cluster")
        finally:
            nodes.stop()
    if "security" in only:
        print("== security")
        security(binary)
    print(f"\n{checks - len(failures)}/{checks} passed")
    if failures:
        print("FAILED:")
        for f in failures:
            print("  " + f)
        sys.exit(1)


if __name__ == "__main__":
    main()
