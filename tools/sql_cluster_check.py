#!/usr/bin/env python3
"""SQL and PPL answer the same whichever node of a cluster is asked.

A query names its index in the body, and the index may be held on any node,
or on several. Every node has to answer the way a search does: reading every
index the query names wherever it is held, with the aggregations reduced over
all of it. The script first asks every query of one node holding everything,
which is the answer to expect, then starts three nodes with the indices held
away from the first and asks every query of every node -- once as placed,
again after an index moves to another node, and again after a node holding a
copy is killed.

  sql_cluster_check.py --binary target/release/velosearch

Ports: HTTP 9751-9753, transport 9851-9853; data under /tmp/pitsql-sql-*.
SQL cursors (`fetch_size`) are asked for as well: each node must give the same
answer, whatever that answer is.
"""
import argparse
import json
import os
import shutil
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request

HTTP = [9751, 9752, 9753]
TRANSPORT = [9851, 9852, 9853]
NAMES = ["n1", "n2", "n3"]
failures = []
checks = 0

SALES = "pitsql-sales"
STOCK = "pitsql-stock"
REGIONS = ["north", "south", "east", "west"]
PRODUCTS = ["widget", "gadget", "gizmo", "doohickey", "sprocket"]


def call(base, method, path, body=None, raw=False, timeout=60):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(base + path, data=data, method=method, headers={"content-type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            text = r.read()
            status = r.status
    except urllib.error.HTTPError as e:
        text = e.read()
        status = e.code
    except Exception as e:  # a node that is down answers nothing
        return 0, str(e)
    if raw:
        return status, text.decode(errors="replace")
    try:
        return status, json.loads(text or b"{}")
    except ValueError:
        return status, text.decode(errors="replace")


def expect(what, ok, detail=""):
    global checks
    checks += 1
    if ok:
        print(f"  ok    {what}")
    else:
        print(f"  FAIL  {what}  {detail}"[:1500])
        failures.append(what)


class Nodes:
    def __init__(self, binary, count, tag):
        self.procs = {}
        self.binary = binary
        self.count = count
        self.bases = [f"http://127.0.0.1:{HTTP[i]}" for i in range(count)]
        self.root = f"/tmp/pitsql-sql-{tag}"
        shutil.rmtree(self.root, ignore_errors=True)
        for i in range(count):
            self.start(i)
        for i in range(count):
            self.wait_up(i)
        if count > 1:
            self.wait_nodes(0, count)

    def start(self, i):
        data = os.path.join(self.root, NAMES[i])
        os.makedirs(data, exist_ok=True)
        env = dict(os.environ)
        env.update({
            "VELOSEARCH_ADDR": f"127.0.0.1:{HTTP[i]}",
            "VELOSEARCH_DATA": os.path.join(data, "data"),
            "VELOSEARCH_TRANSPORT_PORT": str(TRANSPORT[i]),
            "VELOSEARCH_NODE_NAME": NAMES[i],
            "VELOSEARCH_DISABLED": "true",
        })
        if self.count > 1:
            env["VELOSEARCH_DISCOVERY_SEED_HOSTS"] = ",".join(f"127.0.0.1:{TRANSPORT[j]}" for j in range(self.count))
            env["VELOSEARCH_CLUSTER_INITIAL_CLUSTER_MANAGER_NODES"] = ",".join(NAMES[: self.count])
        log = open(os.path.join(self.root, f"{NAMES[i]}.log"), "ab")
        self.procs[i] = subprocess.Popen([self.binary], env=env, stdout=log, stderr=subprocess.STDOUT)

    def wait_up(self, i):
        deadline = time.time() + 60
        while time.time() < deadline:
            st, _ = call(self.bases[i], "GET", "/", timeout=2)
            if st == 200:
                return
            time.sleep(0.2)
        raise SystemExit(f"node {i + 1} did not come up")

    def wait_nodes(self, i, want, seconds=60):
        deadline = time.time() + seconds
        while time.time() < deadline:
            st, h = call(self.bases[i], "GET", "/_cluster/health", timeout=5)
            if st == 200 and h.get("number_of_nodes") == want:
                return
            time.sleep(0.5)
        raise SystemExit(f"the cluster did not reach {want} nodes")

    def kill(self, i):
        p = self.procs.pop(i)
        p.send_signal(signal.SIGKILL)
        p.wait()

    def stop(self):
        for p in self.procs.values():
            p.terminate()
        for p in self.procs.values():
            try:
                p.wait(15)
            except subprocess.TimeoutExpired:
                p.kill()
                p.wait()


def load(base, settings_for):
    for index, props in ((SALES, {"region": {"type": "keyword"}, "product": {"type": "keyword"}, "price": {"type": "double"}, "units": {"type": "long"}}),
                         (STOCK, {"sku": {"type": "keyword"}, "qty": {"type": "long"}, "store": {"type": "keyword"}})):
        settings = {"number_of_shards": 3 if index == SALES else 2, "number_of_replicas": 0}
        settings.update(settings_for(index))
        st, out = call(base, "PUT", f"/{index}", {"settings": settings, "mappings": {"properties": props}})
        if st != 200:
            raise SystemExit(f"create {index}: {st} {out}")
    lines = []
    for i in range(120):
        lines.append(json.dumps({"index": {"_index": SALES, "_id": str(i)}}))
        lines.append(json.dumps({"region": REGIONS[i % 4], "product": PRODUCTS[(i * 7) % 5], "price": round(5 + (i * 37 % 97) + i / 1000, 3), "units": i + 1}))
    for i in range(40):
        lines.append(json.dumps({"index": {"_index": STOCK, "_id": str(i)}}))
        lines.append(json.dumps({"sku": f"sku-{i:03d}", "qty": (i * 13) % 50, "store": ["a", "b", "c"][i % 3]}))
    req = urllib.request.Request(base + "/_bulk?refresh=true", data=("\n".join(lines) + "\n").encode(), method="POST", headers={"content-type": "application/x-ndjson"})
    with urllib.request.urlopen(req, timeout=60) as r:
        out = json.loads(r.read())
        if out.get("errors"):
            raise SystemExit(f"bulk failed: {json.dumps(out)[:500]}")


# (name, language, query, format, whether row order is part of the answer)
QUERIES = [
    ("select where order limit", "sql", f"SELECT region, product, price FROM {SALES} WHERE price > 50 ORDER BY price DESC LIMIT 7", None, True),
    ("select where, every row", "sql", f"SELECT product, units FROM {SALES} WHERE region = 'north'", None, False),
    ("count", "sql", f"SELECT COUNT(*) FROM {SALES}", None, True),
    ("count where", "sql", f"SELECT COUNT(*) FROM {STOCK} WHERE qty >= 20", None, True),
    ("sum and group by", "sql", f"SELECT region, COUNT(*), SUM(units) FROM {SALES} GROUP BY region", None, False),
    ("min max avg", "sql", f"SELECT MIN(price), MAX(price), AVG(units) FROM {SALES}", None, True),
    ("group by order by", "sql", f"SELECT store, SUM(qty) AS total FROM {STOCK} GROUP BY store ORDER BY total DESC", None, True),
    ("order by limit offset", "sql", f"SELECT units FROM {SALES} ORDER BY units ASC LIMIT 5 OFFSET 3", None, True),
    ("json format", "sql", f"SELECT region, COUNT(*) FROM {SALES} GROUP BY region", "json", False),
    ("jdbc format", "sql", f"SELECT sku, qty FROM {STOCK} ORDER BY qty DESC, sku LIMIT 4", "jdbc", True),
    ("csv format", "sql", f"SELECT product, units FROM {SALES} ORDER BY units DESC LIMIT 6", "csv", True),
    ("ppl where stats", "ppl", f"source={SALES} | where price > 30 | stats count() by region", None, False),
    ("ppl stats sum", "ppl", f"source={SALES} | stats sum(units), count()", None, True),
    ("ppl sort head fields", "ppl", f"source={SALES} | where region = 'south' | sort - units | head 3 | fields product, units", None, True),
    ("ppl on the other index", "ppl", f"source={STOCK} | where qty < 10 | stats count() by store", None, False),
    ("ppl csv", "ppl", f"source={STOCK} | where store = 'b' | sort qty | fields sku, qty", "csv", True),
    ("two indices by pattern", "sql", "SELECT COUNT(*) FROM pitsql-s*", None, True),
    ("ppl over two indices by pattern", "ppl", "source=pitsql-s* | stats count()", None, True),
]


def ask(base, language, query, fmt):
    path = f"/_plugins/_{language}" + (f"?format={fmt}" if fmt else "")
    return call(base, "POST", path, {"query": query}, raw=(fmt == "csv"))


def normal(answer, ordered, fmt):
    """The part of an answer that has to agree: rows (sorted unless order is asked), schema, totals."""
    status, body = answer
    if fmt == "csv":
        if not isinstance(body, str):
            return status, body
        lines = body.strip("\n").split("\n")
        head, rows = lines[0], lines[1:]
        return status, [head] + (rows if ordered else sorted(rows))
    if not isinstance(body, dict) or "datarows" not in body:
        return status, body
    rows = body.get("datarows")
    key = lambda r: json.dumps(r, sort_keys=True)
    return status, {
        "schema": body.get("schema"),
        "datarows": rows if ordered else sorted(rows, key=key),
        "total": body.get("total"),
        "size": body.get("size"),
    }


def baseline(binary):
    print("== one node holding everything: the answers to expect")
    nodes = Nodes(binary, 1, "single")
    try:
        load(nodes.bases[0], lambda index: {})
        out = {}
        for name, language, query, fmt, ordered in QUERIES:
            got = normal(ask(nodes.bases[0], language, query, fmt), ordered, fmt)
            expect(f"single: {name} answers", got[0] == 200, json.dumps(got)[:400])
            out[name] = got
        st, cursor = call(nodes.bases[0], "POST", "/_plugins/_sql", {"query": f"SELECT units FROM {SALES} ORDER BY units", "fetch_size": 10})
        out["cursor"] = (st, cursor)
        return out
    finally:
        nodes.stop()


def move_to(base, index, node):
    """Hold every shard of an index on one node: the filter says so, and a
    reroute moves whatever the filter alone has not moved."""
    call(base, "PUT", f"/{index}/_settings", {"index.routing.allocation.include._name": node})
    deadline = time.time() + 90
    while time.time() < deadline:
        st, shards = call(base, "GET", f"/_cat/shards/{index}?format=json")
        if st == 200 and shards and all(s.get("state") == "STARTED" for s in shards):
            if all(s.get("node") == node for s in shards):
                return sorted((s["shard"], s["prirep"], s["node"]) for s in shards)
            moves = [{"move": {"index": index, "shard": int(s["shard"]), "from_node": s["node"], "to_node": node}}
                     for s in shards if s.get("node") != node]
            call(base, "POST", "/_cluster/reroute", {"commands": moves})
        time.sleep(1)
    return f"not settled: {call(base, 'GET', f'/_cat/shards/{index}?format=json')}"


def wait_placed(base, index, want_nodes, seconds=90):
    deadline = time.time() + seconds
    shards = None
    while time.time() < deadline:
        st, shards = call(base, "GET", f"/_cat/shards/{index}?format=json")
        if st == 200 and shards and all(s.get("state") == "STARTED" for s in shards) and {s.get("node") for s in shards} == set(want_nodes):
            return sorted((s["shard"], s["prirep"], s["node"]) for s in shards)
        time.sleep(0.5)
    return f"not settled: {shards}"


def every_node_agrees(label, bases, expected):
    for name, language, query, fmt, ordered in QUERIES:
        answers = [normal(ask(base, language, query, fmt), ordered, fmt) for base in bases]
        for base, got in zip(bases, answers):
            expect(f"{label}: {name}, asked of {base[-4:]}", got == expected[name], f"\n      got  {json.dumps(got)[:600]}\n      want {json.dumps(expected[name])[:600]}")
    cursors = [call(base, "POST", "/_plugins/_sql", {"query": f"SELECT units FROM {SALES} ORDER BY units", "fetch_size": 10}) for base in bases]
    expect(f"{label}: a query with fetch_size answers alike on every node", all(c == expected["cursor"] for c in cursors), json.dumps(cursors)[:600])


def cluster(binary, expected):
    print("== three nodes")
    nodes = Nodes(binary, 3, "cluster")
    try:
        first = nodes.bases[0]
        load(first, lambda index: {"index.routing.allocation.include._name": "n2" if index == SALES else "n3"})
        for index, node in ((SALES, "n2"), (STOCK, "n3")):
            where = move_to(first, index, node)
            print(f"  {index}: {where}")
            expect(f"placed: {index} is held on {node} alone", isinstance(where, list), where)
        every_node_agrees("placed", nodes.bases, expected)

        # the index moves, and every node follows it
        where = move_to(first, SALES, "n3")
        print(f"  {SALES} after the move: {where}")
        expect(f"relocated: {SALES} moved to n3", isinstance(where, list), where)
        every_node_agrees("relocated", nodes.bases, expected)

        # a copy on two nodes, and one of them killed
        call(first, "PUT", f"/{STOCK}/_settings", {"index.routing.allocation.include._name": "n2,n3", "index.number_of_replicas": 1})
        call(first, "PUT", f"/{SALES}/_settings", {"index.routing.allocation.include._name": "n2,n3", "index.number_of_replicas": 1})
        for index in (STOCK, SALES):
            where = wait_placed(first, index, ["n2", "n3"])
            print(f"  {index} with a replica: {where}")
            expect(f"replicated: {index} has a copy on n2 and n3", isinstance(where, list), where)
        nodes.kill(2)
        nodes.wait_nodes(0, 2)
        deadline = time.time() + 60
        while time.time() < deadline:
            st, shards = call(first, "GET", "/_cat/shards/pitsql-*?format=json")
            if st == 200 and shards and all(s.get("node") != "n3" for s in shards if s.get("state") == "STARTED") and all(any(s.get("state") == "STARTED" and s.get("prirep") == "p" for s in shards if s["index"] == i) for i in (SALES, STOCK)):
                break
            time.sleep(0.5)
        every_node_agrees("after n3 was killed", nodes.bases[:2], expected)
    finally:
        nodes.stop()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default="target/release/velosearch")
    args = ap.parse_args()
    binary = os.path.abspath(args.binary)
    expected = baseline(binary)
    cluster(binary, expected)
    print(f"\n{checks - len(failures)}/{checks} passed")
    if failures:
        print("FAILED:")
        for f in failures:
            print("  " + f)
        sys.exit(1)


if __name__ == "__main__":
    main()
