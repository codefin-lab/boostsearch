#!/usr/bin/env python3
"""Asynchronous search results are kept where they say they are, and within bounds.

Three nodes are started with security on, and two users, alice and bob, may
search one index and use asynchronous search. Then:

  kept   - a result kept with `keep_on_completion` reads back STORE_RESIDENT
           on the node that ran it and on the others, and not for bob;
         - a search just submitted on one node is found through another;
         - the node that ran it is restarted, and the result is still read
           back through it and through another node before its keep_alive;
         - a delete through a third node removes it everywhere.
  bounds - with the retained bytes set low, alice keeps results until she is
           refused with 429 `asynchronous_search_rejected_exception`, and bob
           can still submit and keep one; the results kept on disk stay within
           the node's budget, and the node's resident memory does not grow by
           what alice asked to keep;
         - with the running searches set low, a burst of slow searches from
           alice is partly refused, and bob still gets his started;
  expiry - a result whose keep_alive runs out is removed from disk without
           anyone asking for it (about seventy seconds; --skip-expiry leaves
           it out).

    tools/async_search_check.py --binary target/release/velosearch
"""

import argparse
import base64
import concurrent.futures
import json
import os
import pathlib
import shutil
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent
NAMES = ["as0", "as1", "as2"]
ADMIN = ("admin", "admin")
ALICE = ("alice", "Wz7-qLm2#rTv9k")
BOB = ("bob", "Hp4!nVx8-sQe3j")


class Cluster:
    def __init__(self, binary, root, http, transport):
        self.binary = binary
        self.root = pathlib.Path(root)
        self.http = [http + i for i in range(3)]
        self.transport = [transport + i for i in range(3)]
        self.procs = [None, None, None]

    def data(self, i):
        return self.root / NAMES[i] / "data"

    def start(self, i):
        d = self.root / NAMES[i]
        (d / "config" / "security").mkdir(parents=True, exist_ok=True)
        env = {k: v for k, v in os.environ.items() if not k.startswith("VELOSEARCH_")}
        env.update(
            {
                "VELOSEARCH_ADDR": f"127.0.0.1:{self.http[i]}",
                "VELOSEARCH_DATA": str(self.data(i)),
                "VELOSEARCH_CONFIG": str(d / "config"),
                "VELOSEARCH_TRANSPORT_PORT": str(self.transport[i]),
                "VELOSEARCH_NODE_NAME": NAMES[i],
                "VELOSEARCH_DISCOVERY_SEED_HOSTS": ",".join(f"127.0.0.1:{p}" for p in self.transport),
                "VELOSEARCH_CLUSTER_INITIAL_CLUSTER_MANAGER_NODES": ",".join(NAMES),
                "VELOSEARCH_DISABLED": "false",
                "VELOSEARCH_RESTAPI_ROLES_ENABLED": "all_access",
            }
        )
        log = open(d / "node.log", "ab")
        self.procs[i] = subprocess.Popen([self.binary], env=env, stdout=log, stderr=subprocess.STDOUT)
        for _ in range(240):
            if self.call(i, "GET", "/")[0] == 200:
                return
            time.sleep(0.25)
        raise SystemExit(f"{NAMES[i]} did not start; see {d / 'node.log'}")

    def stop(self, i):
        p = self.procs[i]
        self.procs[i] = None
        if p:
            p.send_signal(signal.SIGTERM)
            try:
                p.wait(20)
            except subprocess.TimeoutExpired:
                p.kill()
                p.wait()

    def rss_mib(self, i):
        try:
            out = subprocess.check_output(["ps", "-o", "rss=", "-p", str(self.procs[i].pid)], text=True)
            return int(out.strip()) / 1024
        except Exception:
            return None

    def call(self, i, method, path, body=None, who=ADMIN, timeout=60):
        data = None if body is None else (body if isinstance(body, bytes) else json.dumps(body).encode())
        kind = "application/x-ndjson" if isinstance(body, bytes) else "application/json"
        token = base64.b64encode(f"{who[0]}:{who[1]}".encode()).decode()
        req = urllib.request.Request(
            f"http://127.0.0.1:{self.http[i]}{path}",
            data=data,
            method=method,
            headers={"content-type": kind, "authorization": f"Basic {token}"},
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
    ap.add_argument("--root", default="/tmp/rolesasync-async")
    ap.add_argument("--skip-expiry", action="store_true")
    ap.add_argument("--keep", action="store_true")
    args = ap.parse_args()

    shutil.rmtree(args.root, ignore_errors=True)
    c = Cluster(args.binary, args.root, args.http, args.transport)
    failures = []

    def check(ok, what, detail=None):
        print(("ok    " if ok else "FAIL  ") + what + ("" if ok or detail is None else f": {str(detail)[:400]}"), flush=True)
        if not ok:
            failures.append(what)

    def path(id_):
        return "/_plugins/_asynchronous_search/" + urllib.parse.quote(id_, safe="")

    def submit(i, who, query, index="public", **params):
        qs = urllib.parse.urlencode({"index": index, **params})
        return c.call(i, "POST", f"/_plugins/_asynchronous_search?{qs}", query, who=who)

    try:
        for i in range(3):
            c.start(i)
        deadline = time.time() + 60
        while time.time() < deadline:
            st, nodes = c.call(0, "GET", "/_cat/nodes?format=json")
            if st == 200 and isinstance(nodes, list) and len(nodes) == 3:
                break
            time.sleep(0.5)
        else:
            print("the cluster did not form")
            return 2

        # users, and an index every node holds a copy of
        role = {
            "cluster_permissions": ["cluster:admin/opendistro/asynchronous_search/*"],
            "index_permissions": [{"index_patterns": ["public", "slow"], "allowed_actions": ["read"]}],
        }
        made = [c.call(0, "PUT", "/_plugins/_security/api/roles/async_user", role)]
        for who in (ALICE, BOB):
            made.append(c.call(0, "PUT", f"/_plugins/_security/api/internalusers/{who[0]}", {"password": who[1]}))
        made.append(c.call(0, "PUT", "/_plugins/_security/api/rolesmapping/async_user", {"users": ["alice", "bob"]}))
        if any(st not in (200, 201) for st, _ in made):
            print("the users could not be made", made)
            return 2
        c.call(0, "PUT", "/public", {"settings": {"number_of_shards": 1, "number_of_replicas": 2}})
        c.call(0, "PUT", "/slow", {"settings": {"number_of_shards": 1, "number_of_replicas": 2}})
        blob = "x" * 1000
        lines = []
        for j in range(2000):
            lines.append(json.dumps({"index": {"_index": "public", "_id": str(j)}}))
            lines.append(json.dumps({"n": j, "blob": blob}))
        for j in range(20000):
            lines.append(json.dumps({"index": {"_index": "slow", "_id": str(j)}}))
            lines.append(json.dumps({"n": j}))
        st, b = c.call(0, "POST", "/_bulk?refresh=true", ("\n".join(lines) + "\n").encode(), timeout=300)
        check(st == 200 and not b.get("errors"), "the documents are loaded", (st, str(b)[:200]))
        deadline = time.time() + 60
        while time.time() < deadline:
            st, h = c.call(0, "GET", "/_cluster/health")
            if h.get("status") == "green":
                break
            time.sleep(0.5)
        # security settles on every node before the users are asked for
        deadline = time.time() + 30
        while time.time() < deadline:
            if all(submit(i, ALICE, {"size": 0}, wait_for_completion_timeout="5s")[0] == 200 for i in range(3)):
                break
            time.sleep(0.5)

        # ---- kept: where the result is, and for how long -------------------
        st, a = submit(0, ALICE, {"size": 3, "sort": [{"n": "asc"}]}, keep_on_completion="true", wait_for_completion_timeout="10s")
        check(st == 200 and a.get("state") in ("PERSISTING", "STORE_RESIDENT"), "a kept search answers PERSISTING", (st, a.get("state"), a.get("error")))
        rid = a.get("id", "missing")
        want = [h["_id"] for h in a.get("response", {}).get("hits", {}).get("hits", [])]
        for i in range(3):
            st, g = c.call(i, "GET", path(rid), who=ALICE)
            got = [h["_id"] for h in g.get("response", {}).get("hits", {}).get("hits", [])]
            check(st == 200 and g.get("state") == "STORE_RESIDENT" and got == want, f"the kept result reads back through {NAMES[i]}", (st, g))
        st, g = c.call(2, "GET", path(rid), who=BOB)
        check(st == 404, "bob is told alice's result does not exist", (st, g))
        st, g = c.call(1, "DELETE", path(rid), who=BOB)
        check(st == 404, "bob cannot delete alice's result", (st, g))
        st, g = c.call(0, "GET", path(rid), who=ALICE)
        check(st == 200, "alice's result is still there after bob's delete", (st, g))

        st, r = submit(0, ALICE, {"size": 1, "query": {"match_all": {}}}, keep_on_completion="true", wait_for_completion_timeout="0ms")
        rid2 = r.get("id", "missing")
        st2, g = c.call(1, "GET", path(rid2), who=ALICE)
        check(st == 200 and st2 == 200, "a search just submitted is found through another node", (st, r.get("state"), st2, g))

        c.stop(0)
        c.start(0)
        deadline = time.time() + 60
        while time.time() < deadline:
            st, h = c.call(1, "GET", "/_cluster/health")
            if h.get("number_of_nodes") == 3:
                break
            time.sleep(0.5)
        time.sleep(2)
        for i in (0, 2):
            st, g = c.call(i, "GET", path(rid), who=ALICE)
            got = [h["_id"] for h in g.get("response", {}).get("hits", {}).get("hits", [])]
            check(st == 200 and g.get("state") == "STORE_RESIDENT" and got == want, f"after its node restarts, the result reads back through {NAMES[i]}", (st, g))
        st, g = c.call(1, "GET", path(rid2), who=ALICE)
        check(st == 200, "the search submitted before the restart is kept too", (st, g))
        st, g = c.call(2, "DELETE", path(rid), who=ALICE)
        check(st == 200 and g.get("acknowledged") is True, "alice deletes her result through a third node", (st, g))
        for i in range(3):
            st, g = c.call(i, "GET", path(rid), who=ALICE)
            check(st == 404, f"the deleted result is gone through {NAMES[i]}", (st, g))
        c.call(0, "DELETE", path(rid2), who=ALICE)

        # ---- bounds: bytes kept ---------------------------------------------
        settings = {
            "plugins.asynchronous_search.node_retained_bytes": "24mb",
            "plugins.asynchronous_search.user_retained_bytes": "8mb",
            "plugins.asynchronous_search.node_concurrent_running_searches": "6",
            "plugins.asynchronous_search.user_concurrent_running_searches": "3",
        }
        st, b = c.call(0, "PUT", "/_cluster/settings", {"persistent": settings})
        check(st == 200, "the limits are set", (st, b))
        time.sleep(2)
        rss_before = c.rss_mib(0)
        # alice floods from six clients at once while bob keeps small results
        # one after another: alice runs out of room, bob does not notice
        def flood(n):
            out = []
            for _ in range(n):
                out.append(submit(0, ALICE, {"size": 200}, keep_on_completion="true", wait_for_completion_timeout="30s"))
            return out

        def steady(n):
            out = []
            for _ in range(n):
                out.append(submit(0, BOB, {"size": 20}, keep_on_completion="true", wait_for_completion_timeout="30s"))
                time.sleep(0.2)
            return out

        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            floods = [pool.submit(flood, 30) for _ in range(6)]
            bobs = pool.submit(steady, 20)
            alice = [a for f in floods for a in f.result()]
            bob = bobs.result()
        rss_after = c.rss_mib(0)
        states = {}
        for st, r in alice:
            key = r.get("state") if st == 200 else f"{st} {r.get('error', {}).get('type')}"
            states[key] = states.get(key, 0) + 1
        print(f"alice: {json.dumps(states)}; resident memory {rss_before:.0f} MiB -> {rss_after:.0f} MiB", flush=True)
        check(
            states.get("429 asynchronous_search_rejected_exception", 0) > 0 and all(k in ("PERSISTING", "PERSIST_FAILED", "429 asynchronous_search_rejected_exception") for k in states),
            "alice is refused with 429 once her bytes are used",
            states,
        )
        bob_states = [r.get("state") if st == 200 else st for st, r in bob]
        check(all(x == "PERSISTING" for x in bob_states), "bob keeps every result while alice floods", bob_states)
        bob_id = next((r.get("id") for st, r in bob if st == 200), "missing")
        st, g = c.call(1, "GET", path(bob_id), who=BOB)
        check(st == 200 and g.get("state") == "STORE_RESIDENT", "bob reads his result back", (st, g.get("state")))
        kept_dir = c.data(0) / "_state" / "asynchronous_search"
        on_disk = sum(p.stat().st_size for p in kept_dir.glob("*.json")) if kept_dir.exists() else 0
        print(f"kept on disk on {NAMES[0]}: {on_disk / 1048576:.1f} MiB", flush=True)
        check(on_disk <= 24 * 1048576, "what is kept on disk stays within the node's bytes", on_disk)
        check(
            rss_before is not None and rss_after is not None and rss_after - rss_before < 48,
            "resident memory does not grow by what alice asked to keep",
            (rss_before, rss_after),
        )

        # ---- bounds: searches running ---------------------------------------
        slow = {
            "size": 1,
            "query": {"script_score": {"query": {"match_all": {}}, "script": {"source": "double s = 0; for (int i = 0; i < 300; i++) { s += i; } return s;"}}},
        }
        t0 = time.time()
        st, r = submit(0, ALICE, slow, index="slow", wait_for_completion_timeout="30s")
        took = time.time() - t0
        print(f"one slow search takes {took:.2f}s ({st} {r.get('state')})", flush=True)

        def fire(who):
            return submit(0, who, slow, index="slow", wait_for_completion_timeout="0ms")

        with concurrent.futures.ThreadPoolExecutor(max_workers=24) as pool:
            alice_calls = [pool.submit(fire, ALICE) for _ in range(16)]
            time.sleep(0.2)
            bob_calls = [pool.submit(fire, BOB) for _ in range(4)]
            alice = [f.result() for f in alice_calls]
            bob = [f.result() for f in bob_calls]
        st, stats = c.call(0, "GET", "/_plugins/_asynchronous_search/stats")
        a_ok = sum(1 for s, _ in alice if s == 200)
        a_429 = sum(1 for s, _ in alice if s == 429)
        b_ok = sum(1 for s, _ in bob if s == 200)
        print(f"burst: alice {a_ok} started, {a_429} refused; bob {b_ok} started", flush=True)
        check(a_ok <= 3 and a_429 >= 13, "alice runs at most her three searches at once", (a_ok, a_429))
        check(b_ok >= 1, "bob still gets a search started while alice is refused", [s for s, _ in bob])
        running = [n["asynchronous_search_stats"]["running_current"] for n in stats.get("nodes", {}).values()] if st == 200 else stats
        check(st == 200 and all(x <= 6 for x in running), "no more than six run on the node", running)
        # the burst runs out before anything else is asked of the node
        deadline = time.time() + 120
        while time.time() < deadline:
            st, stats = c.call(0, "GET", "/_plugins/_asynchronous_search/stats")
            if all(n["asynchronous_search_stats"]["running_current"] == 0 for n in stats.get("nodes", {}).values()):
                break
            time.sleep(1)

        # ---- expiry: let go of without being asked --------------------------
        if not args.skip_expiry:
            st, r = submit(0, ADMIN, {"size": 1}, keep_on_completion="true", keep_alive="1m", wait_for_completion_timeout="10s")
            eid = r.get("id", "missing")
            name = eid.encode().hex() + ".json"
            check(st == 200 and (kept_dir / name).exists(), "a result kept for a minute is on disk", (st, r.get("state")))
            time.sleep(75)
            check(not (kept_dir / name).exists(), "a minute and a bit later it is gone without anyone asking")
            st, g = c.call(1, "GET", path(eid))
            check(st == 404, "and reading it answers 404", (st, g))
    finally:
        for i in range(3):
            c.stop(i)
        if not args.keep and not failures:
            shutil.rmtree(args.root, ignore_errors=True)
    print("PASS" if not failures else f"FAIL {len(failures)}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
