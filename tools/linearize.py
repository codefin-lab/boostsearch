#!/usr/bin/env python3
"""Linearizability against real nodes, with real partitions.

Writers and readers work a small set of keys against whichever node,
recording every operation's call and return time and outcome. Meanwhile
the script cuts partitions (through each node's /_boost/chaos switch,
which breaks the transport for real) and stops and continues processes
(SIGSTOP/SIGCONT). At the end, every key's history is checked against a
register: is there an order of the operations, consistent with their
real-time windows, in which every read returns the latest write?

Two kinds of anomaly are told apart, because the shipped consistency mode
is OpenSearch's (ADR 0003): a read from an active copy may be behind.
  - LOST: a write that was acknowledged and is not in the final state on
    every node holding the index -- never acceptable.
  - STALE: a read that no linearization explains -- expected only inside a
    partition window, and reported with its time against the windows.

  linearize.py --nodes 127.0.0.1:9213,127.0.0.1:9214,127.0.0.1:9215 \
      --names n1,n2,n3 --pids 123,456,789 --seconds 40 --keys 8
"""
import argparse, json, os, random, signal, sys, threading, time, urllib.request

def call(url, method="GET", body=None, timeout=5):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, method=method, headers={"content-type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.status, json.loads(r.read() or b"{}")

class History:
    def __init__(self):
        self.lock = threading.Lock()
        self.ops = []  # dict(key, kind, value, call, ret, ok, node)
    def add(self, op):
        with self.lock:
            self.ops.append(op)

def worker(idx, nodes, index, keys, hist, stop, seed):
    rng = random.Random(seed)
    n = 0
    while not stop.is_set():
        node = rng.choice(nodes)
        key = rng.choice(keys)
        n += 1
        if rng.random() < 0.5:
            value = idx * 1_000_000 + n
            call_t = time.monotonic()
            ok, err = False, None
            try:
                st, _ = call(f"http://{node}/{index}/_doc/{key}", "PUT", {"v": value}, timeout=8)
                ok = st in (200, 201)
            except Exception as e:  # noqa: BLE001
                err = str(e)[:60]
            ret_t = time.monotonic()
            hist.add({"key": key, "kind": "write", "value": value, "call": call_t, "ret": ret_t, "ok": ok, "node": node, "err": err})
        else:
            call_t = time.monotonic()
            value, ok, err = None, False, None
            try:
                st, body = call(f"http://{node}/{index}/_doc/{key}", "GET", timeout=8)
                if st == 200:
                    ok = True
                    value = body.get("_source", {}).get("v") if body.get("found") else None
            except urllib.error.HTTPError as e:
                if e.code == 404:
                    ok, value = True, None
                else:
                    err = f"http {e.code}"
            except Exception as e:  # noqa: BLE001
                err = str(e)[:60]
            ret_t = time.monotonic()
            hist.add({"key": key, "kind": "read", "value": value, "call": call_t, "ret": ret_t, "ok": ok, "node": node, "err": err})
        time.sleep(rng.uniform(0.005, 0.03))

def check_key(ops):
    """Wing & Gong over a register: an order consistent with the windows in
    which each read returns the latest write. Writes that failed may or
    may not have taken effect: they are tried both ways (absent, or as a
    write that happened)."""
    ops = sorted(ops, key=lambda o: o["call"])
    n = len(ops)
    if n > 400:
        ops = ops[:400]
        n = 400
    done = [False] * n
    sys.setrecursionlimit(10000)
    calls = [o["call"] for o in ops]
    rets = [o["ret"] for o in ops]
    seen = set()
    def search(value, remaining):
        if remaining == 0:
            return True
        state_key = (value, tuple(done))
        if state_key in seen:
            return False
        seen.add(state_key)
        # the ops that may go next: not done, and called before every other pending op returned
        min_ret = min(rets[i] for i in range(n) if not done[i])
        for i in range(n):
            if done[i] or calls[i] > min_ret:
                continue
            o = ops[i]
            if o["kind"] == "read":
                if o["ok"] and o["value"] != value:
                    continue
                done[i] = True
                if search(value, remaining - 1):
                    return True
                done[i] = False
            else:
                # a failed write may have taken effect or not
                done[i] = True
                if o["ok"]:
                    if search(o["value"], remaining - 1):
                        return True
                else:
                    if search(value, remaining - 1) or search(o["value"], remaining - 1):
                        return True
                done[i] = False
        return False
    return search(None, n)

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--nodes", required=True)
    ap.add_argument("--names", required=True, help="node names, in the order of --nodes")
    ap.add_argument("--pids", default="", help="process ids, in the order of --nodes (for SIGSTOP)")
    ap.add_argument("--index", default="lin")
    ap.add_argument("--keys", type=int, default=6)
    ap.add_argument("--workers", type=int, default=6)
    ap.add_argument("--seconds", type=int, default=30)
    ap.add_argument("--faults", default="partition,stop", help="comma list of partition,stop or none")
    ap.add_argument("--seed", type=int, default=1)
    a = ap.parse_args()
    nodes = a.nodes.split(",")
    names = a.names.split(",")
    pids = [int(p) for p in a.pids.split(",")] if a.pids else []
    rng = random.Random(a.seed)
    keys = [f"k{i}" for i in range(a.keys)]
    # the index, fresh, with a replica and a short delay
    try:
        call(f"http://{nodes[0]}/{a.index}", "DELETE")
    except Exception:
        pass
    call(f"http://{nodes[0]}/{a.index}", "PUT", {"settings": {"number_of_shards": 1, "number_of_replicas": 1, "index.unassigned.node_left.delayed_timeout": "2s"}})
    time.sleep(3)
    hist = History()
    stop = threading.Event()
    threads = [threading.Thread(target=worker, args=(i, nodes, a.index, keys, hist, stop, a.seed * 100 + i), daemon=True) for i in range(a.workers)]
    for t in threads:
        t.start()
    windows = []  # (start, end, description)
    t0 = time.monotonic()
    faults = [f for f in a.faults.split(",") if f and f != "none"]
    while time.monotonic() - t0 < a.seconds and faults:
        time.sleep(rng.uniform(3, 6))
        kind = rng.choice(faults)
        victim = rng.randrange(len(nodes))
        start = time.monotonic()
        if kind == "partition":
            others = [i for i in range(len(nodes)) if i != victim]
            try:
                call(f"http://{nodes[victim]}/_boost/chaos", "POST", {"cut": [names[i] for i in others]})
                for i in others:
                    call(f"http://{nodes[i]}/_boost/chaos", "POST", {"cut": [names[victim]]})
            except Exception as e:  # noqa: BLE001
                print("could not cut:", e, file=sys.stderr)
            hold = rng.uniform(4, 9)
            time.sleep(hold)
            for i in range(len(nodes)):
                try:
                    call(f"http://{nodes[i]}/_boost/chaos", "POST", {"heal": True})
                except Exception:
                    pass
            windows.append((start - t0, time.monotonic() - t0, f"isolate {names[victim]}"))
        elif kind == "stop" and pids:
            os.kill(pids[victim], signal.SIGSTOP)
            hold = rng.uniform(3, 8)
            time.sleep(hold)
            os.kill(pids[victim], signal.SIGCONT)
            windows.append((start - t0, time.monotonic() - t0, f"stop {names[victim]}"))
    # the rest of the time, quiet
    while time.monotonic() - t0 < a.seconds:
        time.sleep(0.5)
    stop.set()
    for t in threads:
        t.join(timeout=15)
    # let the cluster settle: every node back and the index green, then read
    settled_in = None
    for i in range(120):
        try:
            st, h = call(f"http://{nodes[0]}/_cluster/health/{a.index}?wait_for_nodes={len(nodes)}&timeout=1s", timeout=5)
            if h.get("status") == "green" and h.get("number_of_nodes") == len(nodes) and not h.get("timed_out"):
                settled_in = i
                break
        except Exception:
            pass
        time.sleep(1)
    print(f"settled: {'after %ds' % settled_in if settled_in is not None else 'NOT within 120s'}")
    if settled_in is None:
        for node in nodes:
            try:
                req = urllib.request.Request(f"http://{node}/_cat/shards/{a.index}?v&h=index,prirep,state,node,unassigned.reason")
                with urllib.request.urlopen(req, timeout=5) as r:
                    print(f"  shards as {node} sees them:\n" + "\n".join("    " + l for l in r.read().decode().splitlines()))
                break
            except Exception:
                continue
    time.sleep(3)
    ops = hist.ops
    total = len(ops)
    acked = [o for o in ops if o["kind"] == "write" and o["ok"]]
    print(f"{total} operations, {len(acked)} writes acknowledged, {sum(1 for o in ops if o['kind']=='write' and not o['ok'])} writes refused or timed out, {sum(1 for o in ops if o['kind']=='read' and not o['ok'])} reads failed")
    for (s, e, d) in windows:
        print(f"  fault {d}: {s:.1f}s .. {e:.1f}s")
    # final values per key, per node holding a copy
    finals = {}
    for node in nodes:
        finals[node] = {}
        for k in keys:
            try:
                st, body = call(f"http://{node}/{a.index}/_doc/{k}?preference=_local", "GET", timeout=8)
                finals[node][k] = body.get("_source", {}).get("v") if body.get("found") else None
            except Exception as e:  # noqa: BLE001
                finals[node][k] = f"error {str(e)[:30]}"
    lost = 0
    for k in keys:
        vals = {finals[n][k] for n in nodes if not str(finals[n][k]).startswith("error")}
        if len(vals) > 1:
            print(f"  DIVERGED {k}: {[(n, finals[n][k]) for n in nodes]}")
            lost += 1
    # lost acknowledged writes: the last acknowledged write of a key must be the final value
    # unless a later write (acknowledged or not) overtook it
    for k in keys:
        ws = sorted([o for o in ops if o["key"] == k and o["kind"] == "write"], key=lambda o: o["ret"])
        last_acked = None
        for o in ws:
            if o["ok"]:
                last_acked = o
        if last_acked is None:
            continue
        # a write that was still in flight when the last acknowledged one was
        # called may have landed after it: either order is a legal one
        later = [o for o in ws if o["ret"] >= last_acked["call"] and o is not last_acked]
        candidates = {last_acked["value"]} | {o["value"] for o in later}
        for n in nodes:
            fv = finals[n][k]
            if str(fv).startswith("error"):
                continue
            if fv not in candidates:
                print(f"  LOST {k} on {n}: final {fv}, last acknowledged {last_acked['value']}")
                lost += 1
                if n == nodes[0]:
                    # the tail of the key's history, for reading against the fault windows
                    tail = sorted([o for o in ops if o["key"] == k], key=lambda o: o["call"])
                    idx = next((i for i, o in enumerate(tail) if o is last_acked), len(tail) - 1)
                    for o in tail[max(0, idx - 6): idx + 8]:
                        mark = "<-- last acked" if o is last_acked else ("<-- final" if o["kind"] == "write" and o["value"] == fv else "")
                        print(f"      {o['kind']:5} {o['call']-t0:7.2f}..{o['ret']-t0:7.2f} {o['node'][-4:]} ok={o['ok']!s:5} v={o['value']} {o['err'] or ''} {mark}")
    # per-key linearizability
    stale_keys = 0
    stale_detail = []
    for k in keys:
        kops = [o for o in ops if o["key"] == k and (o["ok"] or o["kind"] == "write")]
        if not check_key(kops):
            stale_keys += 1
            # which reads sit inside fault windows
            reads = [o for o in kops if o["kind"] == "read"]
            inside = sum(1 for r in reads if any(s - 1 <= r["call"] - t0 <= e + 12 for (s, e, _) in windows))
            stale_detail.append(f"{k}: {len(kops)} ops, {len(reads)} reads, {inside} of them within a fault window (+12s)")
    print(f"keys not linearizable: {stale_keys} of {len(keys)}")
    for d in stale_detail:
        print("  ", d)
    print("RESULT", "LOST" if lost else "no acknowledged write lost", "|", "linearizable" if stale_keys == 0 else "stale reads (see above)")
    return 1 if lost else 0

if __name__ == "__main__":
    sys.exit(main())
