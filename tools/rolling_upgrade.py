#!/usr/bin/env python3
"""A rolling upgrade: every node replaced, one at a time, while the cluster serves.

Two builds are named -- the one the cluster starts on and the one it ends
on -- and the nodes are restarted onto the new build in turn, waiting for
green between each. Writers and readers work throughout, so the run says
whether a mixed-version cluster kept answering and kept every
acknowledged write. With one binary given twice it is a rolling restart,
which is the same test with nothing to upgrade.

  rolling_upgrade.py --from ./target/release/boostsearch --to ./build/new/boostsearch
"""
import argparse, json, os, subprocess, sys, threading, time, urllib.error, urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from cluster_chaos import HTTP, TRANSPORT, NAMES, Node, Load, call, wait_green, copy_holders  # noqa: E402


def version_of(node):
    try:
        _, body = call(f"http://{node.http}/", timeout=5)
        return body.get("version", {}).get("number", "?")
    except Exception:
        return "?"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--from", dest="old", default="./target/release/boostsearch")
    ap.add_argument("--to", dest="new", default="./target/release/boostsearch")
    ap.add_argument("--index", default="upgrade")
    ap.add_argument("--workers", type=int, default=4)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--root", default="")
    a = ap.parse_args()
    import tempfile
    root = a.root or tempfile.mkdtemp(prefix="bsupgrade.")
    log_dir = os.path.join(root, "logs")
    os.makedirs(log_dir, exist_ok=True)
    seeds = ",".join(f"127.0.0.1:{p}" for p in TRANSPORT)
    nodes = [Node(i, a.old, root, seeds, log_dir) for i in range(3)]
    for n in nodes:
        n.start()
    for n in nodes:
        if not n.wait_http():
            print(f"{n.name} did not come up; see {log_dir}", file=sys.stderr)
            return 2
    time.sleep(6)
    call(f"http://{nodes[0].http}/{a.index}", "PUT",
         {"settings": {"number_of_shards": 1, "number_of_replicas": 1,
                       "index.unassigned.node_left.delayed_timeout": "2s"}}, timeout=10)
    if wait_green(nodes, a.index, 60) is None:
        print("the index did not go green before the upgrade", file=sys.stderr)
        return 2
    print(f"data under {root}")
    print(f"starting on {version_of(nodes[0])} from {a.old}")
    load = Load(nodes, a.index, a.workers, a.seed)
    load.run()
    t0 = time.monotonic()
    ok = True
    for n in nodes:
        print(f"{time.monotonic() - t0:6.1f}s  {n.name}: stopping", flush=True)
        n.stop_graceful()
        n.binary = a.new
        n.start()
        if not n.wait_http(60):
            print(f"{n.name} did not come back on the new build", file=sys.stderr)
            ok = False
            break
        g = wait_green(nodes, a.index, 120)
        print(f"{time.monotonic() - t0:6.1f}s  {n.name}: back on {version_of(n)}, "
              f"{'green after %.1fs' % g if g is not None else 'NOT green within 120s'}", flush=True)
        if g is None:
            ok = False
        # the cluster answers while the versions are mixed
        try:
            st, body = call(f"http://{n.http}/{a.index}/_search", "POST", {"size": 0}, timeout=10)
            print(f"          search on {n.name}: {st}, {body.get('hits', {}).get('total', {}).get('value')} documents")
        except Exception as e:
            print(f"          search on {n.name} failed: {e}")
            ok = False
        time.sleep(3)
    time.sleep(3)
    load.halt()
    settled = wait_green(nodes, a.index, 120)
    holders = copy_holders(nodes, a.index)
    print(f"settled: {'after %.1fs' % settled if settled is not None else 'NOT within 120s'}; copies on {holders}")
    lost = 0
    for n in nodes:
        if n.name not in holders:
            continue
        try:
            call(f"http://{n.http}/{a.index}/_refresh", "POST", timeout=10)
        except Exception:
            pass
        for doc_id, value in load.acked.items():
            try:
                st, body = call(f"http://{n.http}/{a.index}/_doc/{doc_id}?preference=_local", timeout=10)
                if not body.get("found") or body.get("_source", {}).get("v") != value:
                    lost += 1
                    if lost <= 5:
                        print(f"  LOST {doc_id} on {n.name}")
            except urllib.error.HTTPError as e:
                if e.code == 404:
                    lost += 1
                    if lost <= 5:
                        print(f"  LOST {doc_id} on {n.name}")
            except Exception:
                pass
    print(f"{load.attempted} writes attempted, {len(load.acked)} acknowledged, {load.errors} refused or failed; "
          f"{load.reads} reads, {load.read_errors} failed")
    print("RESULT", "LOST" if lost else "every acknowledged write survived the upgrade", "|",
          "every node upgraded and green" if ok else "a node did not come back green")
    for n in nodes:
        n.stop_graceful(seconds=10)
    return 0 if ok and not lost else 1


if __name__ == "__main__":
    sys.exit(main())
