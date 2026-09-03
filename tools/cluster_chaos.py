#!/usr/bin/env python3
"""Chaos, soak and rolling restart against three real nodes.

The script starts the nodes itself (so it can kill and restart them on
their own data directories), drives writers and readers at them, and
applies faults on a schedule:

  partition   cut one node off through /_boost/chaos, heal after a while
  stop        SIGSTOP one node, SIGCONT after a while
  kill        SIGKILL one node, start it again on its data directory
  restart     SIGTERM one node (a graceful leave), start it again
  rolling     SIGTERM and restart every node in turn, green between each

At the end it waits for the cluster to settle, then checks every
acknowledged document on every node that holds a copy: an acknowledged
write that a copy does not have is LOST, and the run fails. A soak run
also samples each node's resident memory and reports first-minute
against last-minute, so a leak shows as a slope.

  cluster_chaos.py --mode chaos   --seconds 90
  cluster_chaos.py --mode rolling --rounds 2
  cluster_chaos.py --mode soak    --seconds 900
"""
import argparse, json, os, random, shutil, signal, subprocess, sys, tempfile, threading, time, urllib.error, urllib.request

HTTP = [9213, 9214, 9215]
TRANSPORT = [9303, 9304, 9305]
NAMES = ["n1", "n2", "n3"]


def call(url, method="GET", body=None, timeout=5):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, method=method, headers={"content-type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.status, json.loads(r.read() or b"{}")


class Node:
    def __init__(self, i, binary, root, seeds, log_dir):
        self.i = i
        self.name = NAMES[i]
        self.http = f"127.0.0.1:{HTTP[i]}"
        self.binary = binary
        self.data = os.path.join(root, self.name)
        os.makedirs(self.data, exist_ok=True)
        self.seeds = seeds
        self.log = open(os.path.join(log_dir, f"{self.name}.log"), "ab")
        self.proc = None
        self.up_since = None

    def start(self):
        env = dict(os.environ)
        env.update({
            "BOOSTSEARCH_ADDR": self.http,
            "BOOSTSEARCH_DATA": self.data,
            "BOOSTSEARCH_TRANSPORT_PORT": str(TRANSPORT[self.i]),
            "BOOSTSEARCH_NODE_NAME": self.name,
            "BOOSTSEARCH_CHAOS": "1",
            "BOOSTSEARCH_DISCOVERY_SEED_HOSTS": self.seeds,
            "BOOSTSEARCH_CLUSTER_INITIAL_CLUSTER_MANAGER_NODES": ",".join(NAMES),
            "BOOSTSEARCH_CLUSTER_DEBUG": "1",
        })
        self.proc = subprocess.Popen([self.binary], env=env, stdout=self.log, stderr=subprocess.STDOUT)
        self.up_since = time.monotonic()

    def wait_http(self, seconds=30):
        t0 = time.monotonic()
        while time.monotonic() - t0 < seconds:
            try:
                call(f"http://{self.http}/", timeout=2)
                return True
            except Exception:
                time.sleep(0.25)
        return False

    def rss_mib(self):
        if not self.proc:
            return None
        try:
            out = subprocess.check_output(["ps", "-o", "rss=", "-p", str(self.proc.pid)], text=True).strip()
            return int(out) / 1024 if out else None
        except Exception:
            return None

    def signal(self, sig):
        if self.proc:
            os.kill(self.proc.pid, sig)

    def stop_graceful(self, seconds=15):
        if not self.proc:
            return
        self.proc.send_signal(signal.SIGTERM)
        try:
            self.proc.wait(timeout=seconds)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        self.proc = None

    def kill(self):
        if not self.proc:
            return
        self.proc.kill()
        self.proc.wait()
        self.proc = None


class Load:
    """Writers and readers. Every acknowledged document id is remembered
    with the value written; every error is counted by phase."""

    def __init__(self, nodes, index, workers, seed):
        self.nodes = nodes
        self.index = index
        self.workers = workers
        self.seed = seed
        self.lock = threading.Lock()
        self.acked = {}  # id -> value
        self.acked_at = {}  # id -> (seconds since start, node name)
        self.acked_copies = {}  # id -> _shards.successful
        self.t0 = time.monotonic()
        self.attempted = 0
        self.errors = 0
        self.reads = 0
        self.read_errors = 0
        self.stop = threading.Event()
        self.threads = []

    def run(self):
        for w in range(self.workers):
            t = threading.Thread(target=self.worker, args=(w,), daemon=True)
            t.start()
            self.threads.append(t)

    def worker(self, w):
        rng = random.Random(self.seed * 100 + w)
        n = 0
        while not self.stop.is_set():
            node = rng.choice(self.nodes)
            n += 1
            r = rng.random()
            try:
                if r < 0.45:
                    doc_id = f"w{w}-{n}"
                    value = n
                    with self.lock:
                        self.attempted += 1
                    st, ans = call(f"http://{node.http}/{self.index}/_doc/{doc_id}", "PUT", {"v": value, "w": w}, timeout=10)
                    if st in (200, 201):
                        with self.lock:
                            self.acked[doc_id] = value
                            self.acked_at[doc_id] = (time.monotonic() - self.t0, node.name)
                            self.acked_copies[doc_id] = ans.get("_shards", {}).get("successful")
                    else:
                        with self.lock:
                            self.errors += 1
                elif r < 0.6:
                    # a bulk of twenty
                    lines = []
                    ids = []
                    for k in range(20):
                        n += 1
                        doc_id = f"w{w}-{n}"
                        ids.append((doc_id, n))
                        lines.append(json.dumps({"index": {"_index": self.index, "_id": doc_id}}))
                        lines.append(json.dumps({"v": n, "w": w}))
                    with self.lock:
                        self.attempted += len(ids)
                    body = ("\n".join(lines) + "\n").encode()
                    req = urllib.request.Request(f"http://{node.http}/_bulk", data=body, method="POST", headers={"content-type": "application/x-ndjson"})
                    with urllib.request.urlopen(req, timeout=15) as resp:
                        out = json.loads(resp.read())
                    items = out.get("items", [])
                    with self.lock:
                        for (doc_id, value), item in zip(ids, items):
                            st = item.get("index", {}).get("status")
                            if st in (200, 201):
                                self.acked[doc_id] = value
                                self.acked_at[doc_id] = (time.monotonic() - self.t0, node.name)
                                self.acked_copies[doc_id] = item.get("index", {}).get("_shards", {}).get("successful")
                            else:
                                self.errors += 1
                        self.errors += max(0, len(ids) - len(items))
                else:
                    with self.lock:
                        self.reads += 1
                        known = list(self.acked.items())[-50:] if self.acked else []
                    if known and rng.random() < 0.7:
                        doc_id, value = rng.choice(known)
                        st, body = call(f"http://{node.http}/{self.index}/_doc/{doc_id}", timeout=10)
                    else:
                        st, body = call(f"http://{node.http}/{self.index}/_search", "POST", {"size": 5, "query": {"term": {"w": w}}}, timeout=10)
            except urllib.error.HTTPError as e:
                with self.lock:
                    if r < 0.6:
                        self.errors += 1 if r < 0.45 else 20
                    else:
                        self.read_errors += 1
            except Exception:
                with self.lock:
                    if r < 0.6:
                        self.errors += 1 if r < 0.45 else 20
                    else:
                        self.read_errors += 1
            time.sleep(rng.uniform(0.002, 0.02))

    def halt(self):
        self.stop.set()
        for t in self.threads:
            t.join(timeout=20)


def any_up(nodes):
    for n in nodes:
        if n.proc is None:
            continue
        try:
            call(f"http://{n.http}/", timeout=2)
            return n
        except Exception:
            continue
    return None


def wait_green(nodes, index, seconds=120):
    """Every running node says green, with every node in the cluster.

    Asking one node is not enough: a node that never rejoined after a
    partition answers happily about the cluster it remembers."""
    t0 = time.monotonic()
    while time.monotonic() - t0 < seconds:
        want = sum(1 for x in nodes if x.proc is not None)
        agreed = 0
        for n in nodes:
            if n.proc is None:
                continue
            try:
                st, h = call(f"http://{n.http}/_cluster/health/{index}?wait_for_nodes={want}&timeout=1s", timeout=5)
                if h.get("status") == "green" and h.get("number_of_nodes") == want and not h.get("timed_out"):
                    agreed += 1
            except Exception:
                pass
        if agreed == want:
            return time.monotonic() - t0
        time.sleep(0.5)
    return None


def copy_holders(nodes, index):
    n = any_up(nodes)
    if not n:
        return []
    try:
        req = urllib.request.Request(f"http://{n.http}/_cat/shards/{index}?h=node,state")
        with urllib.request.urlopen(req, timeout=5) as r:
            rows = [l.split() for l in r.read().decode().splitlines() if l.strip()]
        return sorted({row[0] for row in rows if len(row) == 2 and row[1] == "STARTED"})
    except Exception:
        return []


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default="./target/release/boostsearch")
    ap.add_argument("--mode", choices=["chaos", "rolling", "soak"], default="chaos")
    ap.add_argument("--seconds", type=int, default=90)
    ap.add_argument("--rounds", type=int, default=2, help="rolling: how many times round the nodes")
    ap.add_argument("--faults", default="partition,stop,kill,restart", help="chaos and soak: kinds to mix")
    ap.add_argument("--workers", type=int, default=4)
    ap.add_argument("--index", default="chaos")
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--root", default="", help="data root; a fresh temporary one when empty")
    a = ap.parse_args()
    rng = random.Random(a.seed)
    root = a.root or tempfile.mkdtemp(prefix="bschaos.")
    log_dir = os.path.join(root, "logs")
    os.makedirs(log_dir, exist_ok=True)
    seeds = ",".join(f"127.0.0.1:{p}" for p in TRANSPORT)
    nodes = [Node(i, a.binary, root, seeds, log_dir) for i in range(3)]
    for n in nodes:
        n.start()
    for n in nodes:
        if not n.wait_http():
            print(f"{n.name} did not come up; see {log_dir}", file=sys.stderr)
            return 2
    time.sleep(6)
    n0 = nodes[0]
    call(f"http://{n0.http}/{a.index}", "PUT", {"settings": {"number_of_shards": 1, "number_of_replicas": 1, "index.unassigned.node_left.delayed_timeout": "2s"}}, timeout=10)
    if wait_green(nodes, a.index, 60) is None:
        print("the index did not go green before the run", file=sys.stderr)
        return 2
    print(f"data under {root}; logs in {log_dir}")
    load = Load(nodes, a.index, a.workers, a.seed)
    load.run()
    routing_log = []

    def watch_routing():
        last = None
        while not load.stop.is_set():
            n = any_up(nodes)
            if n:
                try:
                    req = urllib.request.Request(f"http://{n.http}/{a.index}/_search_shards")
                    with urllib.request.urlopen(req, timeout=2) as r:
                        body = json.loads(r.read())
                    names = {v["name"]: k for k, v in body.get("nodes", {}).items()}
                    id_to_name = {k: v["name"] for k, v in body.get("nodes", {}).items()}
                    shards = body.get("shards", [[]])[0]
                    now = ", ".join(f"{'p' if c.get('primary') else 'r'}={id_to_name.get(c.get('node'), '?')}:{c.get('state')}" for c in shards)
                    now = f"{now} (asked {n.name})"
                    if now != last:
                        routing_log.append((time.monotonic() - load.t0, now))
                        last = now
                except Exception:
                    pass
            time.sleep(0.3)

    threading.Thread(target=watch_routing, daemon=True).start()
    t0 = time.monotonic()
    events = []
    samples = []  # (t, [rss per node])

    def note(what):
        events.append((time.monotonic() - t0, what))
        print(f"{time.monotonic() - t0:7.1f}s  {what}", flush=True)

    def sample():
        samples.append((time.monotonic() - t0, [n.rss_mib() for n in nodes]))

    def fault(kind, victim):
        v = nodes[victim]
        others = [nodes[i] for i in range(3) if i != victim]
        if kind == "partition":
            note(f"isolate {v.name}")
            for n in nodes:
                try:
                    cut = [x.name for x in others] if n is v else [v.name]
                    call(f"http://{n.http}/_boost/chaos", "POST", {"cut": cut}, timeout=5)
                except Exception:
                    pass
            time.sleep(rng.uniform(4, 9))
            for n in nodes:
                try:
                    call(f"http://{n.http}/_boost/chaos", "POST", {"heal": True}, timeout=5)
                except Exception:
                    pass
            note(f"heal {v.name}")
        elif kind == "stop":
            note(f"stop {v.name}")
            v.signal(signal.SIGSTOP)
            time.sleep(rng.uniform(3, 8))
            v.signal(signal.SIGCONT)
            note(f"continue {v.name}")
        elif kind == "kill":
            note(f"kill {v.name}")
            v.kill()
            time.sleep(rng.uniform(2, 6))
            v.start()
            v.wait_http()
            note(f"{v.name} back after a kill")
        elif kind == "restart":
            note(f"restart {v.name} (SIGTERM)")
            v.stop_graceful()
            time.sleep(rng.uniform(1, 3))
            v.start()
            v.wait_http()
            note(f"{v.name} back")

    if a.mode == "rolling":
        for r in range(a.rounds):
            for i in range(3):
                t = time.monotonic()
                fault("restart", i)
                g = wait_green(nodes, a.index, 120)
                note(f"green {'after %.1fs' % g if g is not None else 'NOT within 120s'} ({nodes[i].name}, round {r + 1})")
                time.sleep(3)
    else:
        kinds = [k for k in a.faults.split(",") if k and k != "none"]
        last_sample = 0
        while time.monotonic() - t0 < a.seconds and kinds:
            if a.mode == "soak":
                time.sleep(rng.uniform(8, 20))
            else:
                time.sleep(rng.uniform(3, 6))
            if time.monotonic() - last_sample > 10:
                sample()
                last_sample = time.monotonic()
            fault(rng.choice(kinds), rng.randrange(3))
        while time.monotonic() - t0 < a.seconds:
            time.sleep(0.5)
        sample()
    # quiet, then settle
    time.sleep(3)
    load.halt()
    settled = wait_green(nodes, a.index, 120)
    print(f"settled: {'after %.1fs' % settled if settled is not None else 'NOT within 120s'}")
    if settled is None:
        for x in [x for x in nodes if x.proc is not None]:
            try:
                st, h = call(f"http://{x.http}/_cluster/health/{a.index}", timeout=5)
                st2, who = call(f"http://{x.http}/_cluster/state?filter_path=cluster_manager_node,master_node", timeout=5)
                print(f"  as {x.name} sees it: {h.get('status')}, nodes={h.get('number_of_nodes')}, manager={who.get('cluster_manager_node') or who.get('master_node')}")
            except Exception as e:
                print(f"  as {x.name} sees it: no answer ({e})")
        n = any_up(nodes)
        if n:
            try:
                req = urllib.request.Request(f"http://{n.http}/_cat/shards/{a.index}?v&h=index,prirep,state,node,unassigned.reason")
                with urllib.request.urlopen(req, timeout=5) as r:
                    print("  " + r.read().decode().replace("\n", "\n  "))
            except Exception:
                pass
    time.sleep(2)
    # the check: every acknowledged document, on every copy
    holders = copy_holders(nodes, a.index)
    print(f"{load.attempted} writes attempted, {len(load.acked)} acknowledged, {load.errors} refused or failed; {load.reads} reads, {load.read_errors} failed; copies on {holders}")
    lost = 0
    wrong = 0
    checked = 0
    lost_ids = []
    # doc id -> the holders that do not have it
    missing_from = {}
    for n in nodes:
        if n.name not in holders:
            continue
        try:
            call(f"http://{n.http}/{a.index}/_refresh", "POST", timeout=10)
            st, c = call(f"http://{n.http}/{a.index}/_count?preference=_local", timeout=10)
            print(f"  {n.name}: _count {c.get('count')} against {len(load.acked)} acknowledged")
        except Exception as e:
            print(f"  {n.name}: count failed: {e}")
        for doc_id, value in load.acked.items():
            checked += 1
            try:
                st, body = call(f"http://{n.http}/{a.index}/_doc/{doc_id}?preference=_local", timeout=10)
                if not body.get("found"):
                    missing_from.setdefault(doc_id, []).append(n.name)
                    if lost <= 5:
                        print(f"  LOST {doc_id} on {n.name}")
                elif body.get("_source", {}).get("v") != value:
                    wrong += 1
                    if wrong <= 5:
                        print(f"  WRONG {doc_id} on {n.name}: {body.get('_source')} against v={value}")
            except urllib.error.HTTPError as e:
                if e.code == 404:
                    missing_from.setdefault(doc_id, []).append(n.name)
                else:
                    print(f"  read of {doc_id} on {n.name}: http {e.code}")
            except Exception as e:
                print(f"  read of {doc_id} on {n.name}: {e}")
    if samples:
        first = samples[0][1]
        last = samples[-1][1]
        print("memory (RSS MiB) first sample -> last sample per node:")
        for i, n in enumerate(nodes):
            print(f"  {n.name}: {first[i]:.0f} -> {last[i]:.0f}" if first[i] and last[i] else f"  {n.name}: n/a")
    # an acknowledged write missing from every holder is lost; missing from
    # some of them is a copy that is behind while the cluster says green
    behind = {}
    for doc_id, nodes_without in missing_from.items():
        if len(nodes_without) >= len(holders):
            lost += 1
            lost_ids.append(doc_id)
        else:
            for name in nodes_without:
                behind[name] = behind.get(name, 0) + 1
    for name, n_behind in sorted(behind.items()):
        times = sorted(
            load.acked_at.get(i, (0, "?"))[0]
            for i, ns in missing_from.items()
            if name in ns and len(ns) < len(holders)
        )
        print(f"  BEHIND {name}: {n_behind} acknowledged writes it does not have, from {times[0]:.1f}s to {times[-1]:.1f}s on the load clock")
    for doc_id in lost_ids[:5]:
        print(f"  LOST {doc_id}: on none of {holders}")
    if lost_ids:
        # when the lost writes were acknowledged, and by which node, against the faults
        times = sorted(load.acked_at.get(i, (0, "?")) for i in set(lost_ids))
        by_node = {}
        for t, name in times:
            by_node[name] = by_node.get(name, 0) + 1
        print(f"lost writes were acknowledged between {times[0][0]:.1f}s and {times[-1][0]:.1f}s (load clock), by node {by_node}")
        buckets = {}
        for t, _ in times:
            buckets[int(t // 5) * 5] = buckets.get(int(t // 5) * 5, 0) + 1
        print("  per 5s: " + ", ".join(f"{k}s:{v}" for k, v in sorted(buckets.items())))
        copies = {}
        for i in set(lost_ids):
            c = load.acked_copies.get(i)
            copies[c] = copies.get(c, 0) + 1
        print(f"  lost writes by _shards.successful at the time: {copies}")
        print("  faults (event clock) and routing (load clock; the load clock starts %.1fs earlier):" % (t0 - load.t0))
        merged = [(t + (t0 - load.t0), "FAULT " + what) for t, what in events] + [(t, "routing " + r) for t, r in routing_log]
        for t, what in sorted(merged):
            print(f"    {t:6.1f}s {what}")
    print(f"checked {checked} copies of acknowledged documents: {lost} lost, {wrong} wrong")
    print(
        "RESULT",
        "LOST" if lost or wrong else "no acknowledged write lost",
        "|",
        "every copy has them all" if not behind else f"copies behind: {behind}",
        "|",
        "settled" if settled is not None else "NOT settled",
    )
    for n in nodes:
        n.stop_graceful(seconds=10)
    return 1 if lost or wrong or settled is None else 0


if __name__ == "__main__":
    sys.exit(main())
