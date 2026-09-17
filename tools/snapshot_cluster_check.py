#!/usr/bin/env python3
"""A snapshot of a cluster holds what the whole cluster holds.

Three nodes are started with one repository directory between them. Indices
are kept off the cluster manager, which is the node a snapshot request is
answered by, so that everything worth keeping lives on the other two. The
snapshot must then hold every document of every primary shard: its record
says SUCCESS only when every shard was written, `_status` reports each shard,
and a restore -- beside the original, over it once it is deleted, over it
while it is closed, and into a new cluster that has never held the data --
brings back every document, unchanged.

Then a node holding some of the primaries is stopped. A snapshot of those
indices must not say SUCCESS: without `partial` it is refused, and with it
the snapshot is PARTIAL, names every shard that could not be written, and an
index missing a shard is refused by a restore.

Global state goes with a snapshot that asks for it: templates of both kinds,
component templates, ingest and search pipelines, stored scripts and the
persistent cluster settings come back with `include_global_state`, and stay
away without it.

    tools/snapshot_cluster_check.py --binary target/release/velosearch

The nodes take HTTP 9731-9733 and transport 9831-9833 unless
VELOSEARCH_TEST_PORTS=<first http>,<first transport> says otherwise, and keep
their data under /tmp/snaprepl-cluster. Only the processes it started are
stopped.
"""

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request

_base = os.environ.get("VELOSEARCH_TEST_PORTS", "9731,9831").split(",")
HTTP = [int(_base[0]) + i for i in range(3)]
TRANSPORT = [int(_base[1]) + i for i in range(3)]
NAMES = ["n1", "n2", "n3"]
ROOT = os.environ.get("VELO_SNAPSHOT_CLUSTER_ROOT", "/tmp/snaprepl-cluster")
REPO = os.path.join(ROOT, "repo")
DOCS = 600


def call(port, method, path, body=None, ndjson=False, timeout=120):
    data = None
    headers = {}
    if body is not None:
        data = body.encode() if isinstance(body, str) else json.dumps(body).encode()
        headers["content-type"] = "application/x-ndjson" if ndjson else "application/json"
    req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw = r.read()
            try:
                return r.status, json.loads(raw) if raw else {}
            except ValueError:
                return r.status, {"raw": raw.decode(errors="replace")}
    except urllib.error.HTTPError as e:
        raw = e.read()
        try:
            return e.code, json.loads(raw)
        except ValueError:
            return e.code, {"raw": raw.decode(errors="replace")}
    except Exception as e:  # a node that is not there
        return 0, {"no answer": str(e)[:200]}


class Cluster:
    def __init__(self, binary, generation):
        self.binary = binary
        self.dir = os.path.join(ROOT, generation)
        self.procs = [None, None, None]
        os.makedirs(os.path.join(self.dir, "logs"), exist_ok=True)

    def start(self, i):
        seeds = ",".join(f"127.0.0.1:{p}" for p in TRANSPORT)
        env = dict(os.environ)
        env.update({
            "VELOSEARCH_ADDR": f"127.0.0.1:{HTTP[i]}",
            "VELOSEARCH_DATA": os.path.join(self.dir, NAMES[i]),
            "VELOSEARCH_TRANSPORT_PORT": str(TRANSPORT[i]),
            "VELOSEARCH_NODE_NAME": NAMES[i],
            "VELOSEARCH_DISCOVERY_SEED_HOSTS": seeds,
            "VELOSEARCH_CLUSTER_INITIAL_CLUSTER_MANAGER_NODES": ",".join(NAMES),
            "VELOSEARCH_PATH_REPO": REPO,
        })
        log = open(os.path.join(self.dir, "logs", f"{NAMES[i]}.log"), "ab")
        self.procs[i] = subprocess.Popen([self.binary], env=env, stdout=log, stderr=subprocess.STDOUT)

    def start_all(self):
        for i in range(3):
            self.start(i)
        deadline = time.time() + 90
        while time.time() < deadline:
            st, r = call(HTTP[0], "GET", "/_cluster/health")
            if st == 200 and r.get("number_of_nodes") == 3:
                return True
            time.sleep(0.5)
        return False

    def kill(self, i):
        p = self.procs[i]
        if p is not None:
            p.kill()
            p.wait()
            self.procs[i] = None

    def stop(self):
        for i in range(3):
            self.kill(i)


def manager_name(port):
    """The cluster manager, once every node names the same one."""
    deadline = time.time() + 30
    while True:
        named = set()
        for p in HTTP:
            st, r = call(p, "GET", "/_cat/cluster_manager?format=json")
            named.add(r[0]["node"] if st == 200 and isinstance(r, list) and r else None)
        if len(named) == 1 and None not in named or time.time() > deadline:
            return sorted(n for n in named if n)[0] if any(named) else None
        time.sleep(0.5)


def port_of(name):
    return HTTP[NAMES.index(name)]


def shards(port, index):
    st, r = call(port, "GET", f"/_cat/shards/{index}?format=json")
    return r if st == 200 and isinstance(r, list) else []


def wait_green(port, index, seconds=60):
    st, r = call(port, "GET", f"/_cluster/health/{index}?wait_for_status=green&timeout={seconds}s", timeout=seconds + 10)
    return st == 200 and r.get("status") == "green"


def move_off(port, index, manager, others):
    """Move every copy of an index off the manager; true once none is left there."""
    commands = []
    for i, s in enumerate(x for x in shards(port, index) if x.get("node") == manager):
        commands.append({"move": {"index": index, "shard": int(s["shard"]), "from_node": manager, "to_node": others[i % len(others)]}})
    if commands:
        call(port, "POST", "/_cluster/reroute", {"commands": commands})
    deadline = time.time() + 120
    while time.time() < deadline:
        now = shards(port, index)
        if now and all(s.get("node") != manager and s.get("state") == "STARTED" for s in now):
            return True
        time.sleep(0.5)
    return False


def settled(port, index, want, seconds=30):
    """What a node answers for an index once it has heard of all of it: a
    node other than the one that made the index learns of it a moment later."""
    deadline = time.time() + seconds
    got = contents(port, index)
    while got != want and time.time() < deadline:
        time.sleep(1)
        got = contents(port, index)
    return got


def contents(port, index):
    out = {}
    st, r = call(port, "POST", f"/{index}/_search?scroll=1m", {"size": 500, "sort": ["_doc"]})
    while st == 200:
        hits = r.get("hits", {}).get("hits", [])
        if not hits:
            break
        for h in hits:
            out[h["_id"]] = hashlib.sha256(json.dumps(h["_source"], sort_keys=True).encode()).hexdigest()
        st, r = call(port, "POST", "/_search/scroll", {"scroll": "1m", "scroll_id": r.get("_scroll_id")})
    return out


def index_docs(port, index, n, prefix):
    lines = []
    for i in range(n):
        action = {"index": {"_index": index, "_id": f"{prefix}{i}"}}
        if i % 5 == 0:
            action["index"]["routing"] = f"r{i % 4}"
        lines.append(json.dumps(action))
        lines.append(json.dumps({"n": i, "text": f"{prefix} document {i}", "tags": [i % 3, i % 7]}))
    # a primary placed a moment ago may still be settling, and a write to it
    # is told to retry: the same documents are written again, which is
    # idempotent by id
    deadline = time.time() + 30
    while True:
        st, r = call(port, "POST", "/_bulk?refresh=true", "\n".join(lines) + "\n", ndjson=True)
        if (st == 200 and not r.get("errors")) or time.time() > deadline:
            return st, r
        time.sleep(1)


GLOBAL = [
    ("legacy template", "/_template/snaprepl-legacy", {"index_patterns": ["snaprepl-legacy-*"], "settings": {"number_of_replicas": 0}}),
    ("composable template", "/_index_template/snaprepl-composable", {"index_patterns": ["snaprepl-composable-*"], "template": {"settings": {"number_of_replicas": 0}}}),
    ("component template", "/_component_template/snaprepl-component", {"template": {"mappings": {"properties": {"k": {"type": "keyword"}}}}}),
    ("ingest pipeline", "/_ingest/pipeline/snaprepl-ingest", {"processors": [{"set": {"field": "restored", "value": True}}]}),
    ("search pipeline", "/_search/pipeline/snaprepl-search", {"request_processors": [{"filter_query": {"query": {"match_all": {}}}}]}),
    ("stored script", "/_scripts/snaprepl-script", {"script": {"lang": "painless", "source": "doc['n'].value * 2"}}),
]


def global_present(port):
    """Which of the global objects this node answers for, and the setting."""
    out = {}
    for name, path, _ in GLOBAL:
        st, _r = call(port, "GET", path)
        out[name] = st == 200
    st, r = call(port, "GET", "/_cluster/settings?flat_settings=true")
    out["persistent setting"] = r.get("persistent", {}).get("search.max_buckets") in ("4321", 4321)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default="target/release/velosearch")
    args = ap.parse_args()
    binary = os.path.abspath(args.binary)

    results = []

    def check(name, ok, detail=""):
        results.append(ok)
        print(f"  {'ok    ' if ok else 'FAILED'} {name}", flush=True)
        if not ok and detail:
            print(f"    {detail}", flush=True)
        return ok

    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(REPO, exist_ok=True)
    first = Cluster(binary, "first")
    second = None
    try:
        if not check("three nodes form a cluster", first.start_all()):
            return finish(results)
        manager = manager_name(HTTP[0])
        m = port_of(manager)
        others = [n for n in NAMES if n != manager]
        print(f"  (the cluster manager is {manager})")

        # kept off the manager from the start, so no documents are written to
        # a copy that is then moved, and the balancer never puts one back
        off_manager = {"index.routing.allocation.exclude._name": manager}
        call(m, "PUT", "/spread", {"settings": {"number_of_shards": 3, "number_of_replicas": 0, **off_manager}})
        call(m, "PUT", "/elsewhere", {"settings": {"number_of_shards": 1, "number_of_replicas": 0, **off_manager}})
        check("no copy of [spread] is on the cluster manager", move_off(m, "spread", manager, others), str(shards(m, "spread")))
        check("no copy of [elsewhere] is on the cluster manager", move_off(m, "elsewhere", manager, others), str(shards(m, "elsewhere")))
        st, r = index_docs(m, "spread", DOCS, "s")
        check("the documents of [spread] are indexed", st == 200 and not r.get("errors"), str(r)[:300])
        st, r = index_docs(m, "elsewhere", DOCS // 3, "e")
        check("the documents of [elsewhere] are indexed", st == 200 and not r.get("errors"), str(r)[:300])
        before_spread = contents(m, "spread")
        before_elsewhere = contents(m, "elsewhere")
        # everything after compares with these, so without them there is
        # nothing to compare with
        if not check("every document reads back before the snapshot",
                     len(before_spread) == DOCS and len(before_elsewhere) == DOCS // 3,
                     f"{len(before_spread)} and {len(before_elsewhere)}"):
            return finish(results)

        for name, path, body in GLOBAL:
            st, r = call(m, "PUT", path, body)
            check(f"the {name} is made", st == 200, str(r)[:200])
        st, r = call(m, "PUT", "/_cluster/settings", {"persistent": {"search.max_buckets": 4321}})
        check("the persistent setting is made", st == 200, str(r)[:200])
        for port in HTTP:
            deadline = time.time() + 10
            there = global_present(port)
            while not all(there.values()) and time.time() < deadline:
                time.sleep(0.5)
                there = global_present(port)
            check(f"every global object is there before the snapshot, asked of {NAMES[HTTP.index(port)]}",
                  all(there.values()), str(there))

        st, r = call(m, "PUT", "/_snapshot/repo", {"type": "fs", "settings": {"location": "snaprepl"}})
        check("the repository is registered", st == 200, str(r)[:300])

        # asked of a node that is not the manager, naming the index outright
        st, r = call(port_of(others[0]), "PUT", "/_snapshot/repo/named?wait_for_completion=true", {"indices": "spread,elsewhere"})
        snap = r.get("snapshot", {})
        check("a snapshot naming indices held off the manager is SUCCESS with every shard",
              st == 200 and snap.get("state") == "SUCCESS" and snap.get("shards") == {"total": 4, "failed": 0, "successful": 4},
              f"{st} {str(r)[:400]}")

        st, r = call(m, "PUT", "/_snapshot/repo/everything?wait_for_completion=true")
        snap = r.get("snapshot", {})
        check("a snapshot of everything holds the indices held off the manager",
              st == 200 and snap.get("state") == "SUCCESS" and {"spread", "elsewhere"} <= set(snap.get("indices", []))
              and snap.get("shards", {}).get("failed") == 0 and snap.get("include_global_state") is True,
              f"{st} {str(r)[:400]}")

        st, r = call(m, "GET", "/_snapshot/repo/named/_status")
        status = (r.get("snapshots") or [{}])[0]
        spread_status = status.get("indices", {}).get("spread", {})
        per_shard = spread_status.get("shards", {})
        check("_status reports every shard of [spread] as done",
              st == 200 and sorted(per_shard) == ["0", "1", "2"]
              and all(s.get("stage") == "DONE" for s in per_shard.values())
              and spread_status.get("shards_stats", {}).get("done") == 3
              and status.get("shards_stats", {}).get("total") == 4
              and status.get("stats", {}).get("total", {}).get("size_in_bytes", 0) > 0,
              f"{st} {json.dumps(r)[:600]}")

        st, r = call(m, "POST", "/_snapshot/repo/named/_restore?wait_for_completion=true",
                     {"indices": "spread,elsewhere", "rename_pattern": "(.+)", "rename_replacement": "$1-back",
                      # kept apart from where the restored [spread] will be,
                      # so that stopping either node loses only some primaries
                      "index_settings": {"index.routing.allocation.require._name": others[0]}})
        check("the snapshot restores beside the originals", st == 200, str(r)[:300])
        wait_green(m, "spread-back")
        wait_green(m, "elsewhere-back")
        for port in HTTP:
            got = settled(port, "spread-back", before_spread)
            check(f"[spread-back] holds every document, unchanged, asked of {NAMES[HTTP.index(port)]}",
                  got == before_spread, f"{len(got)} of {len(before_spread)}")
        got = contents(m, "elsewhere-back")
        check("[elsewhere-back] holds every document, unchanged", got == before_elsewhere, f"{len(got)} of {len(before_elsewhere)}")

        call(m, "DELETE", "/spread")
        st, r = call(m, "POST", "/_snapshot/repo/named/_restore?wait_for_completion=true",
                     {"indices": "spread", "index_settings": {"index.routing.allocation.require._name": others[1]}})
        check("the snapshot restores over a deleted index", st == 200, str(r)[:300])
        wait_green(m, "spread")
        got = settled(port_of(others[1]), "spread", before_spread)
        check("the restored [spread] holds every document, unchanged", got == before_spread, f"{len(got)} of {len(before_spread)}")
        check("no copy of [elsewhere] is on the cluster manager before it is closed",
              move_off(m, "elsewhere", manager, others), str(shards(m, "elsewhere")))
        st, r = call(m, "POST", "/elsewhere/_close")
        check("[elsewhere] is closed", st == 200, str(r)[:300])
        st, r = call(m, "POST", "/_snapshot/repo/named/_restore?wait_for_completion=true", {"indices": "elsewhere"})
        check("the snapshot restores over a closed index held off the manager", st == 200, str(r)[:300])
        wait_green(m, "elsewhere")
        got = settled(m, "elsewhere", before_elsewhere)
        check("the restored [elsewhere] holds every document, unchanged", got == before_elsewhere,
              f"{len(got)} of {len(before_elsewhere)}; {shards(m, 'elsewhere')}")

        # global state: gone, then brought back only when asked for
        for _name, path, _ in GLOBAL:
            call(m, "DELETE", path)
        call(m, "PUT", "/_cluster/settings", {"persistent": {"search.max_buckets": None}})
        time.sleep(1)
        gone = global_present(port_of(others[0]))
        check("the global objects are deleted", not any(gone.values()), str(gone))
        st, r = call(m, "POST", "/_snapshot/repo/everything/_restore?wait_for_completion=true",
                     {"indices": "-*", "include_global_state": False})
        time.sleep(1)
        still_gone = global_present(port_of(others[0]))
        check("a restore without include_global_state leaves them deleted", st == 200 and not any(still_gone.values()),
              f"{st} {str(r)[:200]} {still_gone}")
        st, r = call(m, "POST", "/_snapshot/repo/everything/_restore?wait_for_completion=true",
                     {"indices": "-*", "include_global_state": True})
        check("a restore with include_global_state answers", st == 200, str(r)[:300])
        for port in HTTP:
            deadline = time.time() + 10
            back = global_present(port)
            while not all(back.values()) and time.time() < deadline:
                time.sleep(0.5)
                back = global_present(port)
            check(f"every global object is back, asked of {NAMES[HTTP.index(port)]}", all(back.values()), str(back))

        # A primary that cannot be reached is not a snapshot that succeeded.
        # The node stopped is the one other than the manager holding the most
        # primaries of these indices; the rest are held by the nodes left.
        several = ["spread", "elsewhere", "spread-back", "elsewhere-back", "home"]
        # an index made where the manager holds it, which stays whatever else goes
        call(m, "PUT", "/home", {"settings": {"number_of_shards": 1, "number_of_replicas": 0,
                                              "index.routing.allocation.require._name": manager}})
        wait_green(m, "home")
        index_docs(m, "home", 50, "h")
        primaries = [(i, int(s["shard"]), s.get("node")) for i in several for s in shards(m, i) if s.get("prirep") == "p"]
        held = {n: [(i, sh) for i, sh, node in primaries if node == n] for n in others}
        holder = max(others, key=lambda n: len(held[n]))
        lost = sorted(held[holder])
        total = len(primaries)
        if check(f"{holder} holds some of the primaries of {several}, and not all of them",
                 0 < len(lost) < total and total == 9, str(primaries)):
            first.kill(NAMES.index(holder))
            deadline = time.time() + 60
            while time.time() < deadline:
                st, r = call(m, "GET", "/_cluster/health")
                if r.get("number_of_nodes") == 2:
                    break
                time.sleep(0.5)
            expr = ",".join(several)
            # the node that manages the cluster now may not be the one that
            # did, and a repository is registered with the manager
            st, r = call(m, "PUT", "/_snapshot/repo", {"type": "fs", "settings": {"location": "snaprepl"}})
            check("the repository is registered with the cluster as it now is", st == 200, str(r)[:300])
            st, r = call(m, "PUT", "/_snapshot/repo/lost?wait_for_completion=true", {"indices": expr})
            check("without partial, a snapshot missing a primary is refused rather than SUCCESS",
                  st == 500 and r.get("error", {}).get("type") == "snapshot_exception"
                  and "primary shards" in r.get("error", {}).get("reason", ""), f"{st} {str(r)[:400]}")
            st, r = call(m, "PUT", "/_snapshot/repo/partly?wait_for_completion=true", {"indices": expr, "partial": True})
            snap = r.get("snapshot", {})
            failed = sorted((f.get("index"), f.get("shard_id")) for f in snap.get("failures", []))
            check("with partial, it is PARTIAL and names every shard it could not write",
                  st == 200 and snap.get("state") == "PARTIAL"
                  and snap.get("shards") == {"total": total, "failed": len(lost), "successful": total - len(lost)}
                  and failed == lost,
                  f"{st} {json.dumps(r)[:800]} -- expected failures {lost}")
            st, r = call(m, "GET", "/_snapshot/repo/partly/_status")
            status = (r.get("snapshots") or [{}])[0]
            stages = sorted((i, int(k)) for i, v in status.get("indices", {}).items()
                            for k, s in v.get("shards", {}).items() if s.get("stage") == "FAILURE")
            check("_status of the partial snapshot counts the failed shards, shard by shard",
                  status.get("state") == "PARTIAL" and status.get("shards_stats", {}).get("failed") == len(lost)
                  and stages == lost, json.dumps(r)[:600])
            broken = lost[0][0]
            st, r = call(m, "POST", "/_snapshot/repo/partly/_restore?wait_for_completion=true",
                         {"indices": broken, "rename_pattern": "(.+)", "rename_replacement": "$1-partial"})
            st2, _ = call(m, "GET", f"/{broken}-partial")
            check(f"[{broken}], which the snapshot does not hold whole, is refused, and no index is left",
                  st >= 400 and st2 == 404, f"{st} {str(r)[:300]}; the index answers {st2}")
        first.stop()

        # a new cluster that never held the data, whose manager holds nothing
        second = Cluster(binary, "second")
        if not check("a second cluster forms on fresh data", second.start_all()):
            return finish(results)
        m2 = port_of(manager_name(HTTP[0]))
        st, r = call(m2, "PUT", "/_snapshot/repo", {"type": "fs", "settings": {"location": "snaprepl"}})
        check("the second cluster registers the same repository", st == 200, str(r)[:300])
        st, r = call(m2, "POST", "/_snapshot/repo/named/_restore?wait_for_completion=true", {"indices": "spread,elsewhere"})
        check("the second cluster restores the snapshot", st == 200, str(r)[:300])
        wait_green(m2, "spread")
        wait_green(m2, "elsewhere")
        for port in HTTP:
            got = settled(port, "spread", before_spread)
            check(f"[spread] in the second cluster is whole, asked of {NAMES[HTTP.index(port)]}",
                  got == before_spread, f"{len(got)} of {len(before_spread)}")
        got = settled(m2, "elsewhere", before_elsewhere)
        check("[elsewhere] in the second cluster is whole", got == before_elsewhere,
              f"{len(got)} of {len(before_elsewhere)}; {shards(m2, 'elsewhere')}")
        st, r = call(m2, "GET", "/spread/_doc/s5?routing=r1")
        check("a routed document keeps its routing", st == 200 and r.get("_routing") == "r1", str(r)[:300])
    finally:
        first.stop()
        if second is not None:
            second.stop()
    return finish(results)


def finish(results):
    passed = sum(results)
    print(f"\n  {passed}/{len(results)} checks passed")
    print("RESULT", "a cluster's snapshot holds the whole cluster" if all(results) else "FAILED")
    return 0 if all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
