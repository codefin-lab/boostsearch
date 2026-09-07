#!/usr/bin/env python3
"""Every acknowledged write survives `kill -9`, in every shape of index.

A write is answered once it is recorded; it reaches the index at the next
commit. Everything between those two points lives in the translog and nowhere
else, so what a node does with that record when it starts again is the whole of
its durability. The suites never restart a node, so nothing measured this.

It found something on its first run: an index with `blocks.write` set lost every
unreplayed write, because the block that refuses a caller's write was also
refusing the node's own replay of its record. The block is a caller's business;
the replay is not.

    tools/restart_check.py

Four indices of different shapes are loaded, the node is killed with SIGKILL,
and the counts are compared with what was acknowledged.
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


def call(url, path, method="GET", body=None, ndjson=None, timeout=60):
    if ndjson is not None:
        data, kind = ndjson.encode(), "application/x-ndjson"
    elif body is not None:
        data, kind = json.dumps(body).encode(), "application/json"
    else:
        data, kind = None, "application/json"
    req = urllib.request.Request(
        url + path, data=data, method=method, headers={"content-type": kind}
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as answer:
            return answer.status, json.loads(answer.read() or b"{}")
    except urllib.error.HTTPError as e:
        raw = e.read()
        try:
            return e.code, json.loads(raw or b"{}")
        except json.JSONDecodeError:
            return e.code, {}
    except Exception as e:
        return 0, {"no answer": str(e)[:120]}


def start(binary, data, url, port, transport):
    env = dict(os.environ)
    env.update(
        {
            "BOOSTSEARCH_ADDR": f"127.0.0.1:{port}",
            "BOOSTSEARCH_DATA": data,
            "BOOSTSEARCH_TRANSPORT_PORT": str(transport),
        }
    )
    log = open(pathlib.Path(data) / "node.log", "a")
    node = subprocess.Popen([binary], env=env, stdout=log, stderr=subprocess.STDOUT)
    for _ in range(60):
        status, _ = call(url, "/", timeout=5)
        if status:
            time.sleep(1)  # the node elects itself a moment after it listens
            return node
        if node.poll() is not None:
            break
        time.sleep(1)
    node.kill()
    print(f"the node did not start; its log is in {data}/node.log")
    sys.exit(2)


def load(url, index, count):
    """Documents written as one bulk, and answered for."""
    lines = []
    for i in range(count):
        lines.append(json.dumps({"index": {"_index": index, "_id": str(i)}}))
        lines.append(json.dumps({"n": i, "s": f"document {i}"}))
    status, answer = call(url, "/_bulk", "POST", ndjson="\n".join(lines) + "\n")
    return status == 200 and not answer.get("errors")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default=str(ROOT / "target" / "release" / "boostsearch"))
    ap.add_argument("--port", type=int, default=9264)
    ap.add_argument("--transport", type=int, default=9364)
    ap.add_argument("--documents", type=int, default=2000)
    ap.add_argument("--keep", action="store_true")
    args = ap.parse_args()

    data = str(pathlib.Path(f"/tmp/boost-restart-{os.getpid()}"))
    shutil.rmtree(data, ignore_errors=True)
    pathlib.Path(data).mkdir(parents=True)
    url = f"http://127.0.0.1:{args.port}"
    node = start(args.binary, data, url, args.port, args.transport)
    bad = []
    try:
        # the shapes an index can be in when a node dies
        call(url, "/plain", "PUT", {})
        call(url, "/async", "PUT", {"settings": {"index.translog.durability": "async"}})
        call(url, "/closed", "PUT", {})
        call(url, "/blocked", "PUT", {})
        call(url, "/readonly", "PUT", {})
        wanted = {}
        for index in ["plain", "async", "closed", "blocked", "readonly"]:
            if load(url, index, args.documents):
                wanted[index] = args.documents
            else:
                bad.append(f"{index}: the bulk was not acknowledged, so nothing is being checked")
        # and the states an operator puts them in, after the writes
        call(url, "/closed/_close", "POST")
        call(url, "/blocked/_settings", "PUT", {"index.blocks.write": True})
        call(url, "/readonly/_settings", "PUT", {"index.blocks.read_only": True})
        # one more write, answered on its own, as close to the kill as we can
        status, _ = call(url, "/plain/_doc/late", "PUT", {"n": -1, "s": "the last word"})
        late = status in (200, 201)
        if late:
            wanted["plain"] += 1
        else:
            bad.append(f"the last write was not acknowledged ({status})")
        time.sleep(0.2)

        node.send_signal(signal.SIGKILL)
        node.wait()
        node = start(args.binary, data, url, args.port, args.transport)

        # let the indices be read again, without writing anything
        call(url, "/closed/_open", "POST")
        call(url, "/blocked/_settings", "PUT", {"index.blocks.write": False})
        call(url, "/readonly/_settings", "PUT", {"index.blocks.read_only": False})
        time.sleep(1)
        for index, count in wanted.items():
            call(url, f"/{index}/_refresh", "POST")
            _, answer = call(url, f"/{index}/_count")
            held = answer.get("count")
            if held != count:
                bad.append(
                    f"{index}: {held} documents after kill -9, {count} were acknowledged"
                )
        if late:
            _, answer = call(url, "/plain/_doc/late")
            if not answer.get("found"):
                bad.append("the document written just before the kill is gone")

        print(f"  {sum(wanted.values())} acknowledged writes, five index shapes, one kill -9")
        for row in bad:
            print(f"    {row}")
        if bad:
            print("\nRESULT a write that was acknowledged did not survive the restart")
            return 1
        print("\nRESULT every acknowledged write was there after the restart")
        return 0
    finally:
        if node.poll() is None:
            node.send_signal(signal.SIGTERM)
            try:
                node.wait(timeout=15)
            except subprocess.TimeoutExpired:
                node.kill()
        if not args.keep:
            shutil.rmtree(data, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
