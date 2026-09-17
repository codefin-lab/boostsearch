#!/usr/bin/env python3
"""What a node does when the disk it writes to has no room left.

The production-readiness review of 2026-09-07 asked for fault injection on
storage, with a real restart, to show that acknowledged writes are not lost.
This is that test. A node is started on a small memory-backed volume, loaded,
and then the volume is filled by a ballast file until nothing more will fit.
The rules it must keep:

  * a write made while the disk is full is refused, or it is durable -- what
    is never allowed is an acknowledgement for a document that is then gone
  * everything acknowledged before the disk filled is still there
  * the node stays up and keeps answering, rather than dying or going silent
  * with the room given back, a `kill -9` and a restart bring back every
    acknowledged document, and the index takes writes again

The volume is a RAM disk made with hdiutil, which needs no password and is
ejected at the end; nothing outside it is touched.

    python3 tools/disk_fault_check.py [--binary ./target/release/velosearch]
"""

import argparse
import json
import os
import pathlib
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent
PORT = int(os.environ.get("VELO_DISK_PORT", "9376"))
TRANSPORT = int(os.environ.get("VELO_DISK_TRANSPORT", "9476"))
URL = f"http://127.0.0.1:{PORT}"
INDEX = "diskfault"
# how much of the volume the documents may take before the ballast goes in
MB = int(os.environ.get("VELO_DISK_MB", "128"))


def call(path, method="GET", body=None, ndjson=None, timeout=30):
    if ndjson is not None:
        data, kind = ndjson.encode(), "application/x-ndjson"
    elif body is not None:
        data, kind = json.dumps(body).encode(), "application/json"
    else:
        data, kind = None, "application/json"
    req = urllib.request.Request(URL + path, data=data, method=method,
                                 headers={"content-type": kind})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw = r.read()
            return r.status, json.loads(raw) if raw else {}
    except urllib.error.HTTPError as e:
        raw = e.read()
        try:
            return e.code, json.loads(raw)
        except ValueError:
            return e.code, {"raw": raw.decode(errors="replace")[:300]}
    except Exception as e:
        return 0, {"error": str(e)}


def ram_disk(mb):
    """A volume of its own, so filling it fills nothing else."""
    dev = subprocess.run(["hdiutil", "attach", "-nomount", f"ram://{mb * 2048}"],
                         capture_output=True, text=True, check=True).stdout.split()[0]
    name = f"bsdisk{os.getpid()}"
    subprocess.run(["diskutil", "erasevolume", "HFS+", name, dev],
                   capture_output=True, text=True, check=True)
    return dev, pathlib.Path("/Volumes") / name


def free_bytes(path):
    st = os.statvfs(path)
    return st.f_bavail * st.f_frsize


class Node:
    def __init__(self, binary, data, log):
        env = {k: v for k, v in os.environ.items() if not k.startswith("VELOSEARCH_")}
        env.update({
            "VELOSEARCH_ADDR": f"127.0.0.1:{PORT}",
            "VELOSEARCH_DATA": str(data),
            "VELOSEARCH_TRANSPORT_PORT": str(TRANSPORT),
            "VELOSEARCH_TRANSPORT_INSECURE": "true",
            "VELOSEARCH_PLUGINS_SECURITY_DISABLED": "true",
        })
        self.log = open(log, "ab")
        self.proc = subprocess.Popen([binary], env=env, stdout=self.log,
                                     stderr=subprocess.STDOUT)

    def answering(self, seconds=40):
        end = time.time() + seconds
        while time.time() < end:
            if self.proc.poll() is not None:
                return False
            if call("/", timeout=2)[0]:
                return True
            time.sleep(0.3)
        return False

    def kill(self):
        if self.proc.poll() is None:
            self.proc.send_signal(signal.SIGKILL)
            self.proc.wait()

    def stop(self):
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(15)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self.log.close()


def write_batch(first, count, acked, refused=None):
    """A bulk of documents; every item answered 200 or 201 is acknowledged.

    An item answered anything else is a refusal, which is what a full disk
    must produce: `refused` counts them, and what they were refused with.
    """
    lines = []
    ids = []
    for i in range(first, first + count):
        doc = f"d{i}"
        ids.append(doc)
        lines.append(json.dumps({"index": {"_index": INDEX, "_id": doc}}))
        lines.append(json.dumps({"n": i, "text": f"document {i} " + "x" * 200}))
    status, r = call("/_bulk?refresh=true", "POST", ndjson="\n".join(lines) + "\n")
    if status != 200:
        if refused is not None:
            refused.append((status, str(r)[:120]))
        return status, 0
    taken = 0
    for doc, item in zip(ids, r.get("items", [])):
        one = item.get("index", {})
        st = one.get("status")
        if st in (200, 201):
            acked.add(doc)
            taken += 1
        elif refused is not None:
            refused.append((st, str(one.get("error", ""))[:120]))
    return status, taken


def present(ids):
    """Which of these documents the node can still find."""
    missing = []
    for doc in sorted(ids):
        status, r = call(f"/{INDEX}/_doc/{doc}")
        if status != 200 or not r.get("found"):
            missing.append(doc)
    return missing


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default=str(ROOT / "target/release/velosearch"))
    ap.add_argument("--keep", action="store_true", help="leave the volume attached")
    a = ap.parse_args()

    results = []

    def check(name, ok, detail=""):
        results.append(ok)
        print(f"  {'ok    ' if ok else 'FAILED'} {name}")
        if not ok and detail:
            print(f"    {detail}")

    dev, volume = ram_disk(MB)
    data = volume / "data"
    data.mkdir()
    ballast = volume / "ballast"
    acked = set()
    node = None
    try:
        node = Node(a.binary, data, volume / "node.log")
        if not node.answering():
            check("the node starts on the volume", False, "it never answered")
            return finish(results)
        check("the node starts on the volume", True)

        call(f"/{INDEX}", "PUT", {"settings": {"index": {"number_of_shards": 1}}})
        rounds = 0
        while free_bytes(volume) > 40 * 1024 * 1024 and rounds < 40:
            status, taken = write_batch(len(acked), 200, acked)
            if status != 200 or taken == 0:
                break
            rounds += 1
        check("documents are written while there is room", len(acked) > 0,
              f"{len(acked)} acknowledged")
        before_full = set(acked)

        # the room goes, in one file that is nobody's business but this test's
        def fill():
            # A merge finishing after the fill hands back the files it
            # replaced: a volume filled once had most of a megabyte again by
            # the first write, the writes fitted, and nothing was refused. The
            # room is taken again before every write, down to the last block.
            with open(ballast, "ab") as f:
                for size in (1024 * 1024, 64 * 1024, 4096):
                    block = b"\0" * size
                    try:
                        while free_bytes(volume) >= size:
                            f.write(block)
                            f.flush()
                            os.fsync(f.fileno())
                    except OSError:
                        pass

        fill()
        left = free_bytes(volume)
        check("the volume is full", left < 2 * 1024 * 1024, f"{left} bytes free")

        # writes against a full disk: refused, or durable -- never both
        acked_while_full = set()
        statuses = []
        refused = []
        for k in range(5):
            fill()
            status, taken = write_batch(100000 + k * 50, 50, acked_while_full, refused)
            statuses.append(status)
        check("the node still answers with the disk full",
              any(s != 0 for s in statuses), f"statuses {statuses}")
        # a test that never made a write fail proves nothing about what
        # happens when one does: the disk has to bite, and be seen to
        kinds = sorted({f"{st}" for st, _ in refused})
        check("the full disk actually refused writes", bool(refused),
              f"250 writes with the disk full, none refused: "
              f"{len(acked_while_full)} acknowledged -- the fault never reached the node")
        print(f"         ({len(refused)} of 250 refused, statuses {kinds}; "
              f"{len(acked_while_full)} acknowledged)")
        if refused:
            print(f"         (first refusal: {refused[0][1]})")
        missing_full = present(acked_while_full)
        check("a write acknowledged with the disk full is really there",
              not missing_full,
              f"{len(missing_full)} of {len(acked_while_full)} acknowledged are gone: {missing_full[:5]}")

        missing_before = present(before_full)
        check("everything acknowledged before the disk filled is still there",
              not missing_before,
              f"{len(missing_before)} of {len(before_full)} gone: {missing_before[:5]}")
        status, _ = call("/_cluster/health")
        check("the node is still up after the disk filled", status == 200, f"health answered {status}")

        # the room comes back, the node is killed outright, and starts again
        acked |= acked_while_full
        os.remove(ballast)
        node.kill()
        node = Node(a.binary, data, volume / "node.log")
        check("the node starts again on its data", node.answering(), "it never answered")
        call(f"/{INDEX}/_refresh", "POST")
        missing_after = present(acked)
        check("every acknowledged document survived the disk filling and a kill -9",
              not missing_after,
              f"{len(missing_after)} of {len(acked)} gone: {missing_after[:5]}")
        status, taken = write_batch(999000, 10, set())
        check("the index takes writes again", status == 200 and taken == 10,
              f"status {status}, {taken} of 10 taken")
    finally:
        if node is not None:
            node.stop()
        if not a.keep:
            subprocess.run(["diskutil", "eject", dev], capture_output=True)
        else:
            print(f"volume left at {volume} ({dev})")
    return finish(results)


def finish(results):
    passed = sum(results)
    print(f"\n  {passed}/{len(results)} checks passed")
    print("RESULT", "a full disk refuses writes and loses none"
          if all(results) else "FAILED")
    return 0 if all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
