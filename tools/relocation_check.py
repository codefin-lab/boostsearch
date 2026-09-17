#!/usr/bin/env python3
"""A copy that moves takes every acknowledged document with it.

Three nodes are started on their own data directories. Each iteration makes
an index (one or two shards, no replica or one), writes documents to it --
one by one or in bulks, with `?refresh` or without -- and moves its primary
while the writes are still arriving, right after the last one, or around a
refresh: with a `_cluster/reroute` `move`, or by changing
`index.routing.allocation.exclude._name` or `require._name` so the node
holding it may no longer. Some iterations stop the source node for a moment
(SIGSTOP) or kill it outright (SIGKILL, started again on its data) while the
copy is on its way.

Once the index is green and nothing is moving, every acknowledged document
is asked for by id and counted on every node holding a copy
(`preference=_local`), and after every few iterations all three nodes are
restarted and the indices kept since the last restart are checked again. A
document that was acknowledged and is not there is LOST and fails the run;
the sequence of the iteration and its seed are printed, and the run can be
repeated with `--seed`.

Also checked on the way:

  - a `move` that is answered 200 happens: the copy ends on the node it was
    moved to (unless a filter change or a killed node took it elsewhere);
  - a `move` that cannot happen -- to the node the copy is on, to a node
    holding another copy of the shard, of a copy not on the named node --
    is refused with an error rather than answered 200;
  - a snapshot repository, and the snapshots in it, are still there after
    the cluster manager is killed and another is elected.

    tools/relocation_check.py --binary target/release/velosearch --iterations 60 --seed 7

The nodes take HTTP 9861-9863 and transport 9961-9963 unless --http and
--transport say otherwise, and keep their data under /tmp/reloc-*. Only the
processes it started are stopped.
"""

import argparse
import json
import os
import random
import shutil
import signal
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request

NAMES = ["n1", "n2", "n3"]


def call(port, method, path, body=None, ndjson=False, timeout=60):
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
    except Exception as e:
        return 0, {"no answer": str(e)[:200]}


class Cluster:
    def __init__(self, binary, http, transport, root, repo):
        self.binary = binary
        self.http = [http + i for i in range(3)]
        self.transport = [transport + i for i in range(3)]
        self.root = root
        self.repo = repo
        self.procs = [None, None, None]
        self.stopped = set()

    def port(self, name):
        return self.http[NAMES.index(name)]

    def start(self, i):
        seeds = ",".join(f"127.0.0.1:{p}" for p in self.transport)
        data = f"{self.root}-{NAMES[i]}"
        os.makedirs(data, exist_ok=True)
        env = {k: v for k, v in os.environ.items() if not k.startswith("VELOSEARCH_")}
        env.update({
            "VELOSEARCH_ADDR": f"127.0.0.1:{self.http[i]}",
            "VELOSEARCH_DATA": data,
            "VELOSEARCH_TRANSPORT_PORT": str(self.transport[i]),
            "VELOSEARCH_NODE_NAME": NAMES[i],
            "VELOSEARCH_DISCOVERY_SEED_HOSTS": seeds,
            "VELOSEARCH_CLUSTER_INITIAL_CLUSTER_MANAGER_NODES": ",".join(NAMES),
            "VELOSEARCH_PATH_REPO": self.repo,
            "VELOSEARCH_CLUSTER_DEBUG": "1",
        })
        log = open(f"{self.root}-{NAMES[i]}.log", "ab")
        self.procs[i] = subprocess.Popen([self.binary], env=env, stdout=log, stderr=subprocess.STDOUT)

    def up(self):
        return [i for i in range(3) if self.procs[i] is not None and i not in self.stopped]

    def wait_formed(self, seconds=90):
        deadline = time.time() + seconds
        while time.time() < deadline:
            good = 0
            for i in self.up():
                st, r = call(self.http[i], "GET", "/_cluster/health", timeout=5)
                if st == 200 and r.get("number_of_nodes") == len(self.up()):
                    good += 1
            if good == len(self.up()):
                return True
            time.sleep(0.5)
        return False

    def start_all(self):
        for i in range(3):
            self.start(i)
        return self.wait_formed()

    def kill(self, i):
        p = self.procs[i]
        if p is not None:
            if i in self.stopped:
                p.send_signal(signal.SIGCONT)
                self.stopped.discard(i)
            p.kill()
            p.wait()
            self.procs[i] = None

    def term(self, i):
        p = self.procs[i]
        if p is not None:
            if i in self.stopped:
                p.send_signal(signal.SIGCONT)
                self.stopped.discard(i)
            p.send_signal(signal.SIGTERM)
            try:
                p.wait(timeout=20)
            except subprocess.TimeoutExpired:
                p.kill()
                p.wait()
            self.procs[i] = None

    def pause(self, i):
        if self.procs[i] is not None:
            self.procs[i].send_signal(signal.SIGSTOP)
            self.stopped.add(i)

    def resume(self, i):
        if self.procs[i] is not None and i in self.stopped:
            self.procs[i].send_signal(signal.SIGCONT)
            self.stopped.discard(i)

    def any_port(self):
        up = self.up()
        return self.http[up[0]] if up else self.http[0]

    def stop_all(self):
        for i in range(3):
            self.kill(i)


def shards(port, index):
    st, r = call(port, "GET", f"/_cat/shards/{index}?format=json&h=index,shard,prirep,state,node", timeout=10)
    return r if st == 200 and isinstance(r, list) else []


def primary_node(port, index, deadline_s=30):
    deadline = time.time() + deadline_s
    while time.time() < deadline:
        rows = shards(port, index)
        prim = [r for r in rows if r.get("prirep") == "p" and r.get("state") == "STARTED"]
        if prim and len(prim) == len({r["shard"] for r in rows}):
            return prim[0]["node"], rows
        time.sleep(0.1)
    return None, shards(port, index)


def settled(cluster, index, seconds=120):
    """Green, nothing moving, and every node agreeing about it."""
    deadline = time.time() + seconds
    while time.time() < deadline:
        port = cluster.any_port()
        st, h = call(port, "GET", f"/_cluster/health/{index}?wait_for_status=green&wait_for_no_relocating_shards=true&wait_for_no_initializing_shards=true&timeout=5s", timeout=15)
        if st == 200 and h.get("status") == "green" and not h.get("timed_out"):
            rows = shards(port, index)
            if rows and all(r.get("state") == "STARTED" for r in rows):
                return rows
        time.sleep(0.5)
    return None


class Writer:
    """Documents written to an index; the ids acknowledged are kept."""

    def __init__(self, cluster, index, rng, total, bulk, refresh):
        self.cluster = cluster
        self.index = index
        self.rng = random.Random(rng.random())
        self.total = total
        self.bulk = bulk
        self.refresh = refresh
        self.acked = {}
        self.refused = 0
        self.written = 0
        self.lock = threading.Lock()
        self.thread = None

    def one_round(self, first, count):
        port = self.cluster.http[self.rng.choice(self.cluster.up())]
        q = "?refresh=true" if self.refresh else ""
        if self.bulk:
            lines = []
            ids = []
            for k in range(first, first + count):
                doc_id = f"d{k}"
                ids.append(doc_id)
                lines.append(json.dumps({"index": {"_index": self.index, "_id": doc_id}}))
                lines.append(json.dumps({"n": k, "text": f"document {k}"}))
            st, r = call(port, "POST", f"/_bulk{q}", "\n".join(lines) + "\n", ndjson=True, timeout=60)
            items = r.get("items", []) if st == 200 else []
            with self.lock:
                for doc_id, item in zip(ids, items):
                    if item.get("index", {}).get("status") in (200, 201):
                        self.acked[doc_id] = int(doc_id[1:])
                    else:
                        self.refused += 1
                self.refused += max(0, len(ids) - len(items))
        else:
            for k in range(first, first + count):
                doc_id = f"d{k}"
                st, r = call(port, "PUT", f"/{self.index}/_doc/{doc_id}{q}", {"n": k, "text": f"document {k}"}, timeout=60)
                with self.lock:
                    if st in (200, 201):
                        self.acked[doc_id] = k
                    else:
                        self.refused += 1

    def run(self):
        k = 0
        while k < self.total:
            count = min(self.rng.randint(5, 40), self.total - k)
            self.one_round(k, count)
            k += count
            with self.lock:
                self.written = k

    def start(self):
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def wait_for(self, n, seconds=60):
        deadline = time.time() + seconds
        while time.time() < deadline:
            with self.lock:
                if self.written >= n:
                    return
            time.sleep(0.005)

    def join(self):
        if self.thread:
            self.thread.join(timeout=300)


def verify(cluster, index, acked, where):
    """Every acknowledged document, on every copy. The ids missing, by node."""
    lost = {}
    rows = shards(cluster.any_port(), index)
    holders = sorted({r["node"] for r in rows if r.get("state") == "STARTED" and r.get("node")})
    call(cluster.any_port(), "POST", f"/{index}/_refresh", timeout=30)
    for name in holders:
        port = cluster.port(name)
        missing = []
        ids = list(acked)
        for start in range(0, len(ids), 200):
            chunk = ids[start:start + 200]
            st, r = call(port, "POST", f"/{index}/_mget?preference=_local", {"ids": chunk}, timeout=60)
            docs = r.get("docs", []) if st == 200 else []
            found = {d.get("_id") for d in docs if d.get("found")}
            if st != 200:
                missing.extend(f"{i}(mget {st})" for i in chunk)
                continue
            missing.extend(i for i in chunk if i not in found)
        st, c = call(port, "GET", f"/{index}/_count?preference=_local", timeout=30)
        count = c.get("count") if st == 200 else None
        if missing or count is None or count < len(acked):
            lost[name] = {"missing_by_get": missing[:20], "missing_count": len(missing), "count": count, "acked": len(acked)}
    if lost:
        print(f"LOST  [{index}] {where}: {json.dumps(lost)}")
        print(f"      routing: {json.dumps(rows)}")
    return lost


def at_or_heading_for(cluster, index, node_name):
    """Whether a copy of the index is on that node, or on its way there.

    Read from the routing table rather than from `_cat/shards`, so a copy the
    balancer moves on again a moment later still counts as having arrived.
    """
    st, state = call(cluster.any_port(), "GET", "/_cluster/state", timeout=30)
    if st != 200:
        return False
    ids = {n.get("name"): i for i, n in (state.get("nodes") or {}).items()}
    want = ids.get(node_name)
    if want is None:
        return False
    shards = ((state.get("routing_table") or {}).get("indices") or {}).get(index) or {}
    for copies in (shards.get("shards") or {}).values():
        for c in copies:
            if c.get("node") == want or c.get("relocating_node") == want:
                return True
    return False


def reroute_move(port, index, shard, from_node, to_node):
    return call(port, "POST", "/_cluster/reroute", {"commands": [{"move": {"index": index, "shard": shard, "from_node": from_node, "to_node": to_node}}]}, timeout=30)


def refusals(cluster, failures):
    """A move that cannot happen is refused, not answered 200."""
    port = cluster.any_port()
    index = "reloc-refuse"
    call(port, "DELETE", f"/{index}")
    call(port, "PUT", f"/{index}", {"settings": {"number_of_shards": 2, "number_of_replicas": 1}})
    rows = settled(cluster, index, 60) or []
    p0 = next((r for r in rows if r["shard"] == "0" and r["prirep"] == "p"), None)
    r0 = next((r for r in rows if r["shard"] == "0" and r["prirep"] == "r"), None)
    if not p0 or not r0:
        failures.append(f"refusal index did not settle: {rows}")
        return
    third = next(n for n in NAMES if n not in (p0["node"], r0["node"]))

    def expect_refused(what, shard, frm, to):
        st, r = reroute_move(port, index, shard, frm, to)
        ok = st >= 400
        print(("ok    " if ok else "FAIL  ") + f"a move {what} is refused ({st})")
        if not ok:
            failures.append(f"move {what} answered {st}: {json.dumps(r)[:300]}")

    expect_refused("to the node the copy is on", 0, p0["node"], p0["node"])
    expect_refused("to the node holding the other copy", 0, p0["node"], r0["node"])
    expect_refused("of a copy the named node does not hold", 0, third, p0["node"])
    expect_refused("to a node that does not exist", 0, p0["node"], "nowhere")
    # shard 1 of an index whose shards are held together: the move of it is
    # either refused or it happens, never answered and then undone
    st, r = reroute_move(port, index, 1, p0["node"], third)
    if st == 200:
        moved = False
        deadline = time.time() + 120
        while time.time() < deadline:
            rows = shards(port, index)
            if any(x["shard"] == "1" and x["node"] == third for x in rows):
                moved = True
                break
            time.sleep(0.2)
        print(("ok    " if moved else "FAIL  ") + "a move of shard 1 answered 200 happens")
        if not moved:
            failures.append(f"a move of shard 1 answered 200 and did nothing: {shards(port, index)}")
    else:
        print(f"ok    a move of shard 1 alone is refused ({st}: {json.dumps(r)[:160]})")
    # one command per shard, which is how a copy is taken off a node
    rows = settled(cluster, index, 120) or []
    p0 = next((x for x in rows if x["shard"] == "0" and x["prirep"] == "p"), None)
    if p0:
        others = {x["node"] for x in rows if x["node"] != p0["node"]}
        to = next((n for n in NAMES if n not in others and n != p0["node"]), None)
        if to:
            st, r = call(port, "POST", "/_cluster/reroute", {"commands": [
                {"move": {"index": index, "shard": 0, "from_node": p0["node"], "to_node": to}},
                {"move": {"index": index, "shard": 1, "from_node": p0["node"], "to_node": to}},
            ]}, timeout=30)
            arrived = False
            deadline = time.time() + 120
            while st == 200 and time.time() < deadline:
                now = shards(port, index)
                if len([x for x in now if x["node"] == to]) >= 2:
                    arrived = True
                    break
                time.sleep(0.2)
            ok = st == 200 and arrived
            print(("ok    " if ok else "FAIL  ") + f"one move command per shard takes the copy to {to} ({st})")
            if not ok:
                failures.append(f"a move with one command per shard: {st} {json.dumps(r)[:200]} {shards(port, index)}")
    call(port, "DELETE", f"/{index}")


def manager_of(cluster):
    """The node the cluster has elected, from the table that stars it."""
    st, rows = call(cluster.any_port(), "GET", "/_cat/nodes?format=json&h=name,cluster_manager")
    if st == 200 and isinstance(rows, list):
        starred = [r["name"] for r in rows if r.get("cluster_manager") == "*"]
        if len(starred) == 1:
            return starred[0]
    st, m = call(cluster.any_port(), "GET", "/_cat/cluster_manager?format=json")
    return m[0]["node"] if st == 200 and isinstance(m, list) and m else None


def moving_onto_the_manager_keeps_the_documents(cluster, failures):
    """The case that lost them: a copy of a two-shard index moved onto the
    cluster manager.

    The manager tells itself a copy has started, so a copy of a shard above
    zero that called itself started because the empty index the fill of shard
    zero had just made was there became the primary at once -- before the
    documents arrived -- and the copy the move came from was taken out of the
    in-sync set and dropped.
    """
    port = cluster.any_port()
    index = "reloc-onto-manager"
    call(port, "DELETE", f"/{index}")
    # made through the manager, so the manager's own store holds an empty copy
    # of it; moved off there before anything is written, that empty copy is
    # what the move back onto the manager finds
    call(port, "PUT", f"/{index}", {"settings": {"number_of_shards": 2, "number_of_replicas": 0}})
    rows = settled(cluster, index, 120) or []
    manager = manager_of(cluster)
    if not rows or manager is None:
        failures.append(f"the index for the move onto the manager did not settle: {rows} {manager}")
        return
    source = rows[0]["node"]
    deadline = time.time() + 120
    while source == manager and time.time() < deadline:
        # off the manager first, so it can be moved back onto it
        other = next(n for n in NAMES if n != manager)
        reroute_move(port, index, 0, source, other)
        rows = settled(cluster, index, 120) or []
        source = rows[0]["node"] if rows else source
        if source != manager:
            break
        time.sleep(1)
    if source == manager:
        failures.append(f"[{index}] could not be moved off the manager {manager} to move it back")
        return
    # written where the copy is now, so the empty copy the manager kept is
    # short of every one of them; and enough of them that the fill of shard
    # zero is still running when the copy of shard one is looked at, which is
    # the window they fell through
    acked = {}
    for begin in range(0, 2000, 200):
        lines = []
        ids = [f"d{i}" for i in range(begin, begin + 200)]
        for doc_id in ids:
            lines.append(json.dumps({"index": {"_index": index, "_id": doc_id}}))
            lines.append(json.dumps({"n": doc_id, "text": f"document {doc_id} " + "x" * 200}))
        st, r = call(port, "POST", "/_bulk?refresh=true", "\n".join(lines) + "\n", ndjson=True, timeout=120)
        items = r.get("items", []) if st == 200 else []
        for doc_id, item in zip(ids, items):
            if item.get("index", {}).get("status") in (200, 201):
                acked[doc_id] = doc_id
    st, r = reroute_move(port, index, 0, source, manager)
    ok = st == 200
    print(("ok    " if ok else "FAIL  ") + f"the move of [{index}] from {source} onto the manager {manager} is accepted ({st})")
    if not ok:
        failures.append(f"the move onto the manager was refused: {st} {json.dumps(r)[:200]}")
        return
    # The moment the cluster says the move is over -- every shard started, and
    # none of them where it came from -- the documents have to be there. A
    # copy the cluster calls complete while it is still filling is a copy the
    # manager has taken the source out of the in-sync set for: what it is
    # missing is gone the moment the source is.
    arrived_at = None
    deadline = time.time() + 180
    while time.time() < deadline:
        rows = shards(cluster.any_port(), index)
        if rows and all(r.get("state") == "STARTED" and r.get("node") == manager for r in rows):
            arrived_at = rows
            break
        time.sleep(0.05)
    if arrived_at is None:
        failures.append(f"[{index}] never arrived whole on the manager: {shards(cluster.any_port(), index)}")
        return
    lost = verify(cluster, index, acked, "the moment the move onto the manager was over")
    print(("ok    " if not lost else "FAIL  ") + f"every one of the {len(acked)} documents is there the moment the move onto the manager is over")
    if lost:
        failures.append(f"documents missing the moment the move of [{index}] onto the manager was over: {json.dumps(lost)}")
    if settled(cluster, index, 180) is None:
        failures.append(f"[{index}] did not settle green after the move onto the manager")
        return
    lost = verify(cluster, index, acked, "after the move onto the manager")
    print(("ok    " if not lost else "FAIL  ") + f"every one of the {len(acked)} documents is still there once it has settled")
    if lost:
        failures.append(f"documents lost moving [{index}] onto the manager: {json.dumps(lost)}")
    call(port, "DELETE", f"/{index}")


def repository_survives(cluster, failures):
    """A repository and its snapshots outlive the manager that registered them."""
    port = cluster.any_port()
    st, r = call(port, "PUT", "/_snapshot/reloc_repo", {"type": "fs", "settings": {"location": "reloc_repo"}})
    if st != 200:
        failures.append(f"repository could not be registered: {st} {r}")
        return
    call(port, "PUT", "/reloc-snap", {"settings": {"number_of_shards": 1, "number_of_replicas": 1}})
    settled(cluster, "reloc-snap", 60)
    call(port, "PUT", "/reloc-snap/_doc/1?refresh=true", {"a": 1})
    st, r = call(port, "PUT", "/_snapshot/reloc_repo/s1?wait_for_completion=true", {"indices": "reloc-snap"}, timeout=120)
    if st != 200:
        failures.append(f"snapshot could not be taken: {st} {json.dumps(r)[:300]}")
        return
    manager = manager_of(cluster)
    if manager is None:
        failures.append("no manager to kill")
        return
    mi = NAMES.index(manager)
    cluster.kill(mi)
    rest = [i for i in range(3) if i != mi]
    deadline = time.time() + 60
    new = None
    while time.time() < deadline:
        st, rows = call(cluster.http[rest[0]], "GET", "/_cat/nodes?format=json&h=name,cluster_manager", timeout=5)
        starred = [r["name"] for r in rows if r.get("cluster_manager") == "*"] if st == 200 and isinstance(rows, list) else []
        if len(starred) == 1 and starred[0] != manager:
            new = starred[0]
            break
        time.sleep(0.5)
    if new is None:
        failures.append("no new manager was elected")
    for i in rest:
        p = cluster.http[i]
        st, r = call(p, "GET", "/_snapshot/reloc_repo")
        ok = st == 200 and "reloc_repo" in r
        print(("ok    " if ok else "FAIL  ") + f"the repository is known through {NAMES[i]} after the manager {manager} was killed ({st})")
        if not ok:
            failures.append(f"repository lost after manager change, through {NAMES[i]}: {st} {json.dumps(r)[:200]}")
        st, r = call(p, "GET", "/_snapshot/reloc_repo/_all")
        names = [s.get("snapshot") for s in r.get("snapshots", [])] if st == 200 else []
        ok = "s1" in names
        print(("ok    " if ok else "FAIL  ") + f"the snapshot is listed through {NAMES[i]} ({st} {names})")
        if not ok:
            failures.append(f"snapshot not listed after manager change, through {NAMES[i]}: {st} {json.dumps(r)[:200]}")
    p = cluster.http[rest[0]]
    call(p, "DELETE", "/reloc-snap-restored")
    st, r = call(p, "POST", "/_snapshot/reloc_repo/s1/_restore?wait_for_completion=true", {"indices": "reloc-snap", "rename_pattern": "(.+)", "rename_replacement": "$1-restored"}, timeout=120)
    ok = st == 200
    if ok:
        deadline = time.time() + 30
        count = None
        while time.time() < deadline:
            st2, c = call(p, "GET", "/reloc-snap-restored/_count")
            count = c.get("count") if st2 == 200 else None
            if count == 1:
                break
            time.sleep(0.5)
        ok = count == 1
    print(("ok    " if ok else "FAIL  ") + f"a restore through the new manager works ({st})")
    if not ok:
        failures.append(f"restore after manager change: {st} {json.dumps(r)[:300]}")
    # a new manager registering or deleting keeps the others told
    st, r = call(p, "DELETE", "/_snapshot/reloc_repo/s1")
    cluster.start(mi)
    cluster.wait_formed()
    time.sleep(2)
    st, r = call(cluster.http[mi], "GET", "/_snapshot/reloc_repo")
    ok = st == 200 and "reloc_repo" in r
    print(("ok    " if ok else "FAIL  ") + f"the restarted old manager knows the repository ({st})")
    if not ok:
        failures.append(f"restarted old manager does not know the repository: {st}")
    call(p, "DELETE", "/reloc-snap")
    call(p, "DELETE", "/reloc-snap-restored")
    call(p, "DELETE", "/_snapshot/reloc_repo")


SCENARIOS = ["move_during", "move_after", "move_before_refresh", "move_after_refresh",
             "exclude", "require", "move_stop_source", "move_kill_source", "exclude_during"]


def iteration(cluster, it, seed, failures, stats, kept):
    rng = random.Random(seed * 1000003 + it)
    shard_count = rng.choice([1, 2])
    replicas = rng.choice([0, 1])
    total = rng.choice([20, 60, 150, 400])
    bulk = rng.random() < 0.6
    refresh = rng.random() < 0.4
    scenario = rng.choice(SCENARIOS)
    # `require._name` names one node, and a replica has nowhere else to go:
    # the index is yellow by design, which is not what this is looking for
    if scenario == "require" and replicas > 0:
        scenario = "move_after"
    index = f"reloc-{seed}-{it}"
    desc = f"iteration {it} seed {seed}: index={index} shards={shard_count} replicas={replicas} docs={total} bulk={bulk} refresh={refresh} scenario={scenario}"
    print(desc, flush=True)
    port = cluster.any_port()
    st, r = call(port, "PUT", f"/{index}", {"settings": {"number_of_shards": shard_count, "number_of_replicas": replicas}})
    if st != 200:
        failures.append(f"{desc}: create {st} {r}")
        return
    if not settled(cluster, index, 60):
        failures.append(f"{desc}: new index did not settle")
        return
    source, rows = primary_node(port, index)
    if source is None:
        failures.append(f"{desc}: no primary: {rows}")
        return
    replica_nodes = {r["node"] for r in rows if r.get("prirep") == "r"}
    choices = [n for n in NAMES if n != source and n not in replica_nodes]
    target = rng.choice(choices)
    writer = Writer(cluster, index, rng, total, bulk, refresh)
    expected_node = target
    moved_answer = None
    si = NAMES.index(source)

    def do_move():
        nonlocal moved_answer
        shard = rng.choice(range(shard_count))
        st, r = reroute_move(cluster.http[rng.choice([i for i in cluster.up() if i != si] or cluster.up())], index, shard, source, target)
        moved_answer = (st, json.dumps(r)[:300])
        if st == 200:
            # a move that was answered happens: the copy has to reach the node
            # it was moved to. What the balancer does with it afterwards is
            # the balancer's business, so this looks for the copy arriving or
            # on its way rather than for where it ends up.
            deadline = time.time() + 120
            while time.time() < deadline:
                if at_or_heading_for(cluster, index, target):
                    return st
                time.sleep(0.2)
            stats["moves_ignored"] += 1
            failures.append(f"{desc}: a move of shard {shard} from {source} to {target} was answered 200 and the copy never arrived there: {shards(cluster.any_port(), index)}")
            print(f"FAIL  a move of shard {shard} answered 200 and the copy never reached {target}")
        return st

    def set_filter(kind):
        nonlocal expected_node
        value = source if kind == "exclude" else target
        st, r = call(cluster.http[rng.choice(cluster.up())], "PUT", f"/{index}/_settings", {f"index.routing.allocation.{kind}._name": value}, timeout=30)
        if kind == "exclude":
            expected_node = None
        return st

    if scenario == "move_during":
        writer.start()
        writer.wait_for(rng.randint(0, total // 2))
        do_move()
        writer.join()
    elif scenario == "move_after":
        writer.run()
        do_move()
    elif scenario == "move_before_refresh":
        writer.refresh = False
        writer.run()
        do_move()
        call(port, "POST", f"/{index}/_refresh")
    elif scenario == "move_after_refresh":
        writer.run()
        call(port, "POST", f"/{index}/_refresh")
        do_move()
    elif scenario in ("exclude", "require"):
        writer.start()
        writer.wait_for(rng.randint(total // 2, total))
        set_filter(scenario)
        writer.join()
    elif scenario == "exclude_during":
        writer.start()
        writer.wait_for(rng.randint(0, total // 3))
        set_filter("exclude")
        writer.join()
    elif scenario == "move_stop_source":
        writer.start()
        writer.wait_for(rng.randint(0, total))
        do_move()
        time.sleep(rng.uniform(0, 0.3))
        cluster.pause(si)
        time.sleep(rng.uniform(0.5, 3.0))
        cluster.resume(si)
        writer.join()
        expected_node = None
    elif scenario == "move_kill_source":
        writer.run()
        do_move()
        time.sleep(rng.uniform(0, 0.5))
        cluster.kill(si)
        time.sleep(rng.uniform(0.5, 2.0))
        cluster.start(si)
        cluster.wait_formed()
        expected_node = None
    if moved_answer is not None:
        stats["moves"] += 1
        if moved_answer[0] != 200:
            stats["moves_refused"] += 1
            print(f"      move answered {moved_answer[0]}: {moved_answer[1]}")
            expected_node = None
    rows = settled(cluster, index, 180)
    if rows is None:
        failures.append(f"{desc}: did not settle green: {shards(cluster.any_port(), index)}")
        print(f"FAIL  {desc}: did not settle green")
        return
    if scenario == "require" and expected_node is not None:
        # the index is green either way -- a filter moves a copy that is
        # perfectly well where it is -- so this waits for the move
        primaries = set()
        deadline = time.time() + 120
        while time.time() < deadline:
            rows = shards(cluster.any_port(), index) or rows
            primaries = {r["node"] for r in rows if r.get("prirep") == "p"}
            if primaries == {expected_node}:
                break
            time.sleep(0.5)
        if primaries != {expected_node}:
            failures.append(f"{desc}: require._name={expected_node} left the primary on {primaries}")
            print(f"FAIL  require._name={expected_node} left the primary on {primaries}")
        rows = settled(cluster, index, 180) or rows
    if scenario == "exclude":
        deadline = time.time() + 120
        while time.time() < deadline and source in {r["node"] for r in rows}:
            time.sleep(0.5)
            rows = shards(cluster.any_port(), index) or rows
        rows = settled(cluster, index, 180) or rows
    if scenario == "exclude" and source in {r["node"] for r in rows}:
        failures.append(f"{desc}: exclude left a copy on {source}: {rows}")
    stats["acked"] += len(writer.acked)
    stats["refused"] += writer.refused
    lost = verify(cluster, index, writer.acked, "after the move")
    if lost:
        stats["lost_iterations"] += 1
        failures.append(f"LOST {desc}: {json.dumps(lost)}")
    kept.append((index, dict(writer.acked), desc))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default="target/release/velosearch")
    ap.add_argument("--iterations", type=int, default=40)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--http", type=int, default=9861)
    ap.add_argument("--transport", type=int, default=9961)
    ap.add_argument("--root", default="/tmp/reloc")
    ap.add_argument("--restart-every", type=int, default=10)
    ap.add_argument("--skip-repository", action="store_true")
    ap.add_argument("--keep", action="store_true", help="leave the data directories behind")
    args = ap.parse_args()

    for name in NAMES:
        shutil.rmtree(f"{args.root}-{name}", ignore_errors=True)
        try:
            os.remove(f"{args.root}-{name}.log")
        except FileNotFoundError:
            pass
    repo = f"{args.root}-repo"
    shutil.rmtree(repo, ignore_errors=True)
    os.makedirs(repo, exist_ok=True)
    cluster = Cluster(os.path.abspath(args.binary), args.http, args.transport, args.root, repo)
    failures = []
    stats = {"moves": 0, "moves_refused": 0, "moves_ignored": 0, "acked": 0, "refused": 0, "lost_iterations": 0, "restarts": 0}
    t0 = time.time()
    try:
        if not cluster.start_all():
            print("the cluster did not form")
            return 2
        refusals(cluster, failures)
        moving_onto_the_manager_keeps_the_documents(cluster, failures)
        kept = []
        for it in range(args.iterations):
            iteration(cluster, it, args.seed, failures, stats, kept)
            if (it + 1) % args.restart_every == 0 or it + 1 == args.iterations:
                for i in range(3):
                    cluster.term(i)
                if not cluster.start_all():
                    failures.append("the cluster did not form again after a restart")
                    break
                stats["restarts"] += 1
                for index, acked, desc in kept:
                    # every index of the batch is recovering at once, and each
                    # copy waits for the primary to start sending its writes
                    # there before it calls itself filled
                    if settled(cluster, index, 300) is None:
                        failures.append(f"{desc}: not green after a restart")
                        print(f"FAIL  {desc}: not green after a restart")
                        continue
                    lost = verify(cluster, index, acked, "after restarting every node")
                    if lost:
                        stats["lost_iterations"] += 1
                        failures.append(f"LOST after restart {desc}: {json.dumps(lost)}")
                    call(cluster.any_port(), "DELETE", f"/{index}")
                kept = []
        if not args.skip_repository:
            repository_survives(cluster, failures)
    finally:
        cluster.stop_all()
        if not args.keep and not failures:
            for name in NAMES:
                shutil.rmtree(f"{args.root}-{name}", ignore_errors=True)
            shutil.rmtree(repo, ignore_errors=True)
    print(json.dumps({**stats, "iterations": args.iterations, "seed": args.seed, "seconds": round(time.time() - t0)}))
    if failures:
        print(f"FAILED: {len(failures)}")
        for f in failures:
            print("  " + f[:600])
        return 1
    print("PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
