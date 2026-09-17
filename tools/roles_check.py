#!/usr/bin/env python3
"""A node without the data role holds no shard, from the moment an index exists.

Four nodes are started on their own data directories: a dedicated cluster
manager (`node.roles: [cluster_manager]`), two data nodes, and a
coordinating-only node (`node.roles: []`). Then:

  - an index made through the manager has every copy on a data node as soon
    as the create is answered, and the writes sent through the manager and
    the coordinating node land there too;
  - an index a write creates through the manager is moved to a data node
    with the document in it, rather than made again empty there;
  - replicas go to data nodes only, and a `move` to the manager is refused;
  - an index filter keeps the copies off a data node from the start, and a
    cluster exclude moves the copies of an existing index off the node it
    names;
  - an empty role list is kept as empty: the coordinating node reports no
    roles and is never given a copy.

    tools/roles_check.py --binary target/release/velosearch
"""

import argparse
import json
import os
import pathlib
import shutil
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent

NODES = [
    ("rm", "[cluster_manager]"),
    ("rd1", "[data]"),
    ("rd2", "[data, ingest]"),
    ("rc", "[]"),
]


def call(port, path, method="GET", body=None, timeout=30):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}{path}",
        data=data,
        method=method,
        headers={"content-type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw = r.read()
            return r.status, json.loads(raw or b"{}")
    except urllib.error.HTTPError as e:
        raw = e.read()
        try:
            return e.code, json.loads(raw or b"{}")
        except json.JSONDecodeError:
            return e.code, {}
    except Exception as e:
        return 0, {"no answer": str(e)[:160]}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default=str(ROOT / "target" / "release" / "velosearch"))
    ap.add_argument("--http", type=int, default=9771)
    ap.add_argument("--transport", type=int, default=9871)
    ap.add_argument("--root", default="/tmp/rolesasync-roles")
    ap.add_argument("--keep", action="store_true")
    args = ap.parse_args()

    shutil.rmtree(args.root, ignore_errors=True)
    http = {name: args.http + i for i, (name, _) in enumerate(NODES)}
    seeds = ",".join(f"127.0.0.1:{args.transport + i}" for i in range(len(NODES)))
    procs = []
    failures = []

    def check(ok, what, detail=None):
        print(("ok    " if ok else "FAIL  ") + what + ("" if ok or detail is None else f": {detail}"))
        if not ok:
            failures.append(what)

    try:
        for i, (name, roles) in enumerate(NODES):
            d = pathlib.Path(args.root) / name
            (d / "config").mkdir(parents=True, exist_ok=True)
            (d / "config" / "velosearch.yml").write_text(f"node.roles: {roles}\n")
            env = {k: v for k, v in os.environ.items() if not k.startswith("VELOSEARCH_")}
            env.update(
                {
                    "VELOSEARCH_ADDR": f"127.0.0.1:{args.http + i}",
                    "VELOSEARCH_DATA": str(d / "data"),
                    "VELOSEARCH_CONFIG": str(d / "config"),
                    "VELOSEARCH_TRANSPORT_PORT": str(args.transport + i),
                    "VELOSEARCH_NODE_NAME": name,
                    "VELOSEARCH_DISCOVERY_SEED_HOSTS": seeds,
                    "VELOSEARCH_CLUSTER_INITIAL_CLUSTER_MANAGER_NODES": "rm",
                }
            )
            log = open(d / "node.log", "ab")
            procs.append(subprocess.Popen([args.binary], env=env, stdout=log, stderr=subprocess.STDOUT))
        deadline = time.time() + 60
        while time.time() < deadline:
            st, nodes = call(http["rm"], "/_cat/nodes?format=json&h=name,node.roles,cluster_manager")
            if st == 200 and isinstance(nodes, list) and len(nodes) == len(NODES):
                break
            time.sleep(0.5)
        else:
            print("the cluster did not form")
            return 2
        roles = {n["name"]: n.get("node.roles", "") for n in nodes}
        print("nodes", json.dumps(roles))
        check(roles.get("rm") == "cluster_manager", "the manager reports only cluster_manager", roles)
        check(roles.get("rc") in ("", "-"), "an empty role list stays empty", roles)
        st, info = call(http["rc"], "/_nodes")
        rc_roles = [n.get("roles") for n in info.get("nodes", {}).values() if n.get("name") == "rc"]
        check(rc_roles == [[]], "the coordinating node's roles are [] in _nodes", rc_roles)

        def shards(index):
            st, rows = call(http["rm"], f"/_cat/shards/{index}?format=json&h=index,shard,prirep,state,node")
            return rows if st == 200 and isinstance(rows, list) else []

        def settle(index, want_started, seconds=30):
            deadline = time.time() + seconds
            rows = []
            while time.time() < deadline:
                rows = shards(index)
                if len([r for r in rows if r.get("state") == "STARTED"]) >= want_started and all(
                    r.get("state") == "STARTED" for r in rows
                ):
                    return rows
                time.sleep(0.25)
            return rows

        # an index made through the manager: every copy on a data node at once
        st, made = call(http["rm"], "/placed", "PUT", {"settings": {"number_of_shards": 2, "number_of_replicas": 0}})
        check(st == 200, "create through the manager is answered", made)
        rows = shards("placed")
        print("placed right after create", json.dumps(rows))
        check(
            len(rows) == 2 and all(r.get("node") in ("rd1", "rd2") for r in rows),
            "right after the create, every copy of [placed] is on a data node",
            rows,
        )
        for j in range(20):
            port = http["rm"] if j % 2 else http["rc"]
            st, b = call(port, f"/placed/_doc/{j}?refresh=true", "PUT", {"n": j})
            if st not in (200, 201):
                check(False, f"write {j} to [placed]", (st, b))
                break
        rows = settle("placed", 2)
        check(
            all(r.get("node") in ("rd1", "rd2") for r in rows) and len(rows) == 2,
            "after writes through the manager and the coordinating node, [placed] is still only on data nodes",
            rows,
        )
        for name in ("rm", "rd1", "rc"):
            st, c = call(http[name], "/placed/_count")
            check(st == 200 and c.get("count") == 20, f"[placed] counts 20 through {name}", (st, c))

        # an index a write makes, through the manager: the manager writes the
        # first document into the index it makes, and the primary has to be
        # moved from there -- with the document -- rather than made again
        # empty on a data node
        st, w = call(http["rm"], "/auto/_doc/1?refresh=true", "PUT", {"v": 1})
        print("auto-creating write", st, json.dumps(w)[:200])
        deadline = time.time() + 30
        rows, count = [], None
        while time.time() < deadline:
            rows = [r for r in shards("auto") if r.get("prirep") == "p"]
            st, c = call(http["rc"], "/auto/_count")
            count = c.get("count") if st == 200 else (st, c)
            if rows and all(r.get("node") in ("rd1", "rd2") and r.get("state") == "STARTED" for r in rows) and count == 1:
                break
            time.sleep(0.25)
        print("auto after it settled", json.dumps(rows))
        check(
            bool(rows) and all(r.get("node") in ("rd1", "rd2") and r.get("state") == "STARTED" for r in rows),
            "the primary of [auto] is moved off the manager",
            rows,
        )
        check(count == 1, "the document that created [auto] went with it", count)

        # replicas: data nodes only, one copy each
        st, _ = call(http["rc"], "/replicated", "PUT", {"settings": {"number_of_shards": 1, "number_of_replicas": 1}})
        rows = settle("replicated", 2)
        print("replicated", json.dumps(rows))
        check(
            sorted(r.get("node") for r in rows) == ["rd1", "rd2"],
            "a primary and a replica sit on the two data nodes",
            rows,
        )
        # an index asking for more replicas than there are data nodes leaves
        # the extra one unassigned rather than putting it on the manager
        call(http["rm"], "/wide", "PUT", {"settings": {"number_of_shards": 1, "number_of_replicas": 2}})
        time.sleep(3)
        rows = shards("wide")
        check(
            all(r.get("node") in ("rd1", "rd2", "", None) for r in rows)
            and len([r for r in rows if r.get("state") == "UNASSIGNED"]) == 1,
            "a second replica with no data node left stays unassigned",
            rows,
        )
        on = next((r.get("node") for r in shards("placed")), "rd1")
        st, moved = call(
            http["rm"],
            "/_cluster/reroute",
            "POST",
            {"commands": [{"move": {"index": "placed", "shard": 0, "from_node": on, "to_node": "rm"}}]},
        )
        check(st >= 400, "a move onto the manager is refused", (st, str(moved)[:300]))
        st, moved = call(
            http["rm"],
            "/_cluster/reroute",
            "POST",
            {"commands": [{"move": {"index": "placed", "shard": 0, "from_node": on, "to_node": "rc"}}]},
        )
        check(st >= 400, "a move onto the coordinating node is refused", (st, str(moved)[:300]))

        # filters: an index kept off rd1 from its first placement
        st, _ = call(
            http["rm"],
            "/filtered",
            "PUT",
            {"settings": {"number_of_shards": 1, "number_of_replicas": 0, "index.routing.allocation.exclude._name": "rd1"}},
        )
        rows = shards("filtered")
        check(
            len(rows) == 1 and rows[0].get("node") == "rd2",
            "an index excluding rd1 is placed on rd2 from the start",
            rows,
        )
        # and a cluster-wide exclude moves what a node already holds
        call(http["rm"], "/drained", "PUT", {"settings": {"number_of_shards": 1, "number_of_replicas": 0}})
        for j in range(10):
            call(http["rc"], f"/drained/_doc/{j}?refresh=true", "PUT", {"n": j})
        holder = next((r.get("node") for r in settle("drained", 1)), None)
        other = "rd2" if holder == "rd1" else "rd1"
        call(http["rm"], "/_cluster/settings", "PUT", {"transient": {"cluster.routing.allocation.exclude._name": holder}})
        deadline = time.time() + 30
        rows = []
        while time.time() < deadline:
            rows = shards("drained")
            if rows and all(r.get("node") == other and r.get("state") == "STARTED" for r in rows):
                break
            time.sleep(0.5)
        check(
            bool(rows) and all(r.get("node") == other and r.get("state") == "STARTED" for r in rows),
            f"excluding {holder} moves [drained] to {other}",
            rows,
        )
        st, c = call(http["rc"], "/drained/_count")
        check(st == 200 and c.get("count") == 10, "[drained] keeps its ten documents after the move", (st, c))
        call(http["rm"], "/_cluster/settings", "PUT", {"transient": {"cluster.routing.allocation.exclude._name": None}})

        # nothing, anywhere, on the two nodes without the data role
        st, rows = call(http["rm"], "/_cat/shards?format=json&h=index,shard,prirep,state,node")
        held = [r for r in rows if r.get("node") in ("rm", "rc")] if isinstance(rows, list) else rows
        check(held == [], "no copy of any index is on the manager or the coordinating node", held)
        st, health = call(http["rm"], "/_cluster/health")
        print("health", json.dumps({k: health.get(k) for k in ("status", "number_of_data_nodes", "unassigned_shards")}))
        check(health.get("number_of_data_nodes") == 2, "two data nodes are counted", health)
    finally:
        for p in procs:
            p.send_signal(signal.SIGTERM)
        for p in procs:
            try:
                p.wait(15)
            except subprocess.TimeoutExpired:
                p.kill()
                p.wait()
        if not args.keep and not failures:
            shutil.rmtree(args.root, ignore_errors=True)
    print("PASS" if not failures else f"FAIL {len(failures)}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
