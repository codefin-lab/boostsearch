#!/usr/bin/env python3
"""One node, under load, for as long as you ask: what a soak is for.

Tier 1 of the production path -- a node holding an index that can be built
again from its source -- asks for a short soak, and nothing here ran one. The
suites are quick and the chaos runs are ninety seconds; neither says what
happens to a node that is worked steadily for half an hour. This does.

Load is mixed, as a real one is: bulks, single writes, updates, deletes,
searches, aggregations and refreshes, all against one index. What it must
hold:

  * every acknowledged write is there at the end, with the value it was
    acknowledged with -- and still there after a restart
  * the node answers throughout; no request is refused for a reason the
    caller did not ask for
  * memory settles rather than climbing: the last quarter of the run is
    compared with the first full quarter, and a node that has doubled is a
    node with a leak
  * search stays as quick as it began -- asked of a control index of fixed
    size, written once before the run and never touched again. The index
    under load grows from nothing to millions of documents, so its searches
    are slower at the end because there is more to search, which says nothing
    about the health of the node; the control says whether the node itself
    slowed down. Both are reported; only the control is judged.

    python3 tools/soak_check.py --minutes 30

The default is thirty minutes. `--minutes 2` is enough to see the machinery
work; it is not a soak.
"""

import argparse
import json
import os
import pathlib
import random
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent
PORT = int(os.environ.get("VELO_SOAK_PORT", "9378"))
TRANSPORT = int(os.environ.get("VELO_SOAK_TRANSPORT", "9478"))
URL = f"http://127.0.0.1:{PORT}"
INDEX = "soak"
# the index that does not change, so that a slower search means a slower node
CONTROL = "soak-control"
CONTROL_DOCS = int(os.environ.get("VELO_SOAK_CONTROL_DOCS", "20000"))


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
            return r.status, (json.loads(raw) if raw else {})
    except urllib.error.HTTPError as e:
        raw = e.read()
        try:
            return e.code, json.loads(raw)
        except ValueError:
            return e.code, {"raw": raw.decode(errors="replace")[:200]}
    except Exception as e:
        return 0, {"error": str(e)[:200]}


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
        self.proc = subprocess.Popen([binary], env=env, stdout=self.log, stderr=subprocess.STDOUT)

    def answering(self, seconds=40):
        end = time.time() + seconds
        while time.time() < end:
            if self.proc.poll() is not None:
                return False
            if call("/", timeout=2)[0]:
                return True
            time.sleep(0.3)
        return False

    def rss_mib(self):
        try:
            out = subprocess.run(["ps", "-o", "rss=", "-p", str(self.proc.pid)],
                                 capture_output=True, text=True, timeout=5).stdout.strip()
            return int(out) / 1024 if out else 0
        except Exception:
            return 0

    def stop(self, graceful=True):
        if self.proc.poll() is None:
            self.proc.send_signal(signal.SIGTERM if graceful else signal.SIGKILL)
            try:
                self.proc.wait(30)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self.log.close()


class Load:
    """Writers and readers, with every acknowledged value remembered."""

    def __init__(self, seed=1):
        self.rng = random.Random(seed)
        self.stop = threading.Event()
        self.lock = threading.Lock()
        self.acked = {}
        self.n = 0
        self.attempted = 0
        self.refused = []          # (status, kind) for anything not 2xx
        self.searches = []         # (elapsed_seconds, latency_ms) on the growing index
        self.control = []          # the same, on the index that does not change
        self.unanswered = 0

    def note(self, status, kind, doc=None, value=None):
        with self.lock:
            if 200 <= status < 300:
                if doc is not None:
                    self.acked[doc] = value
            elif status == 0:
                self.unanswered += 1
            else:
                self.refused.append((status, kind))

    def worker(self, w, t0):
        rng = random.Random(1000 + w)
        while not self.stop.is_set():
            r = rng.random()
            with self.lock:
                self.n += 1
                n = self.n
                self.attempted += 1
            if r < 0.35:
                doc, val = f"w{w}-{n}", n
                st, _ = call(f"/{INDEX}/_doc/{doc}", "PUT",
                             {"v": val, "w": w, "text": f"document {n} " + "x" * (n % 80),
                              "tag": f"t{n % 17}", "when": 1735689600000 + n * 1000})
                self.note(st, "index", doc, val)
            elif r < 0.55:
                lines, ids = [], []
                for _ in range(20):
                    with self.lock:
                        self.n += 1
                        m = self.n
                        self.attempted += 1
                    doc = f"w{w}-{m}"
                    ids.append((doc, m))
                    lines.append(json.dumps({"index": {"_index": INDEX, "_id": doc}}))
                    lines.append(json.dumps({"v": m, "w": w, "text": f"bulk {m}",
                                             "tag": f"t{m % 17}", "when": 1735689600000 + m * 1000}))
                st, body = call("/_bulk", "POST", ndjson="\n".join(lines) + "\n")
                if st == 200:
                    for (doc, val), item in zip(ids, body.get("items", [])):
                        self.note(item.get("index", {}).get("status", 0), "bulk", doc, val)
                else:
                    self.note(st, "bulk")
            elif r < 0.65:
                # an update to a document already acknowledged, and a delete of
                # another: an index that only grows is not a soak
                with self.lock:
                    known = list(self.acked)
                if known:
                    doc = rng.choice(known)
                    val = rng.randrange(1_000_000)
                    st, _ = call(f"/{INDEX}/_update/{doc}", "POST", {"doc": {"v": val}})
                    if 200 <= st < 300:
                        self.note(st, "update", doc, val)
                    elif st != 404:
                        self.note(st, "update")
                if len(known) > 500 and rng.random() < 0.3:
                    doc = rng.choice(known)
                    st, _ = call(f"/{INDEX}/_doc/{doc}", "DELETE")
                    if 200 <= st < 300 or st == 404:
                        with self.lock:
                            self.acked.pop(doc, None)
                    else:
                        self.note(st, "delete")
            elif r < 0.9:
                began = time.monotonic()
                which = rng.random()
                if which < 0.5:
                    st, _ = call(f"/{INDEX}/_search", "POST",
                                 {"size": 5, "query": {"match": {"text": "document"}}})
                elif which < 0.8:
                    st, _ = call(f"/{INDEX}/_search", "POST",
                                 {"size": 0, "aggs": {"t": {"terms": {"field": "tag", "size": 5}},
                                                      "h": {"date_histogram": {"field": "when",
                                                                               "calendar_interval": "day"}}}})
                else:
                    st, _ = call(f"/{INDEX}/_search", "POST",
                                 {"size": 3, "query": {"range": {"v": {"gte": rng.randrange(1000)}}},
                                  "sort": [{"v": "desc"}]})
                took = (time.monotonic() - began) * 1000
                if 200 <= st < 300:
                    with self.lock:
                        self.searches.append((time.monotonic() - t0, took))
                else:
                    self.note(st, "search")
            elif r < 0.95:
                began = time.monotonic()
                st, _ = call(f"/{CONTROL}/_search", "POST",
                             {"size": 5, "query": {"match": {"text": "control"}}})
                took = (time.monotonic() - began) * 1000
                if 200 <= st < 300:
                    with self.lock:
                        self.control.append((time.monotonic() - t0, took))
                else:
                    self.note(st, "control search")
            else:
                st, _ = call(f"/{INDEX}/_refresh", "POST")
                if not 200 <= st < 300:
                    self.note(st, "refresh")


def quarters(samples, t0_len):
    """Split (elapsed, value) samples into four equal stretches of the run."""
    if not samples:
        return []
    out = [[], [], [], []]
    for elapsed, v in samples:
        q = min(3, int(4 * elapsed / t0_len)) if t0_len else 0
        out[q].append(v)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default=str(ROOT / "target/release/velosearch"))
    ap.add_argument("--minutes", type=float, default=30.0)
    ap.add_argument("--workers", type=int, default=4)
    ap.add_argument("--keep", action="store_true")
    a = ap.parse_args()

    results = []

    def check(name, ok, detail=""):
        results.append(ok)
        print(f"  {'ok    ' if ok else 'FAILED'} {name}")
        if not ok and detail:
            print(f"    {detail}")

    root = tempfile.mkdtemp(prefix="bssoak.")
    data = pathlib.Path(root) / "data"
    data.mkdir()
    node = Node(a.binary, data, pathlib.Path(root) / "node.log")
    seconds = a.minutes * 60
    rss = []
    try:
        if not node.answering():
            check("the node starts", False, "it never answered")
            return finish(results, root, a.keep)
        check("the node starts", True)
        call(f"/{INDEX}", "PUT", {"settings": {"index": {"number_of_shards": 1, "number_of_replicas": 0}},
                                  "mappings": {"properties": {"v": {"type": "long"}, "w": {"type": "integer"},
                                                              "text": {"type": "text"}, "tag": {"type": "keyword"},
                                                              "when": {"type": "date"}}}})
        call(f"/{CONTROL}", "PUT", {"settings": {"index": {"number_of_shards": 1, "number_of_replicas": 0}},
                                    "mappings": {"properties": {"v": {"type": "long"},
                                                                "text": {"type": "text"},
                                                                "tag": {"type": "keyword"}}}})
        for start in range(0, CONTROL_DOCS, 1000):
            lines = []
            for i in range(start, min(start + 1000, CONTROL_DOCS)):
                lines.append(json.dumps({"index": {"_index": CONTROL, "_id": f"c{i}"}}))
                lines.append(json.dumps({"v": i, "text": f"control document {i} " + "y" * (i % 60),
                                         "tag": f"c{i % 13}"}))
            call("/_bulk", "POST", ndjson="\n".join(lines) + "\n")
        call(f"/{CONTROL}/_refresh", "POST")
        st, c = call(f"/{CONTROL}/_count")
        print(f"    control index: {c.get('count')} documents, fixed for the run", flush=True)

        load = Load()
        t0 = time.monotonic()
        threads = [threading.Thread(target=load.worker, args=(w, t0), daemon=True) for w in range(a.workers)]
        for t in threads:
            t.start()
        health_failures = 0
        while time.monotonic() - t0 < seconds:
            time.sleep(min(15, seconds / 20))
            rss.append((time.monotonic() - t0, node.rss_mib()))
            st, _ = call("/_cluster/health", timeout=10)
            if not 200 <= st < 300:
                health_failures += 1
            done = time.monotonic() - t0
            with load.lock:
                print(f"    {done:6.0f}s  acknowledged {len(load.acked):7d}  "
                      f"attempted {load.attempted:7d}  rss {rss[-1][1]:6.0f} MiB  "
                      f"refused {len(load.refused)}", flush=True)
        load.stop.set()
        for t in threads:
            t.join(timeout=30)

        call(f"/{INDEX}/_refresh", "POST")
        check("the node answered its health check throughout", health_failures == 0,
              f"{health_failures} health checks failed")
        kinds = {}
        for st, kind in load.refused:
            kinds[f"{kind} {st}"] = kinds.get(f"{kind} {st}", 0) + 1
        check("no request was refused", not load.refused, f"{kinds}")
        check("every request was answered", load.unanswered == 0, f"{load.unanswered} got no answer")

        # every acknowledged document, with the value it was acknowledged with
        with load.lock:
            expected = dict(load.acked)
        # a sample, not the whole of it: a soak acknowledges millions, and
        # asking after every one of them takes longer than the soak did
        sample = list(expected.items())
        random.Random(7).shuffle(sample)
        sample = sample[:3000]
        missing, wrong = [], []
        for doc, val in sample:
            st, r = call(f"/{INDEX}/_doc/{doc}")
            if st != 200 or not r.get("found"):
                missing.append(doc)
            elif r.get("_source", {}).get("v") != val:
                wrong.append((doc, val, r.get("_source", {}).get("v")))
            if len(missing) > 20:
                break
        check(f"every acknowledged document is there, with its value ({len(sample)} sampled)",
              not missing and not wrong,
              f"{len(missing)} missing {missing[:5]}, {len(wrong)} wrong {wrong[:3]}")

        # memory: the last quarter against the first full quarter
        qs = quarters(rss, seconds)
        if all(qs) and len(rss) >= 8:
            first, last = statistics.median(qs[1]), statistics.median(qs[3])
            check("memory settles rather than climbing", last <= first * 2,
                  f"first quarter {first:.0f} MiB, last {last:.0f} MiB")
            print(f"         (rss {rss[0][1]:.0f} -> {rss[-1][1]:.0f} MiB, peak {max(v for _, v in rss):.0f})")
        else:
            print("         (too few samples to judge memory; run longer)")

        qs = quarters(load.control, seconds)
        if all(qs) and len(load.control) >= 40:
            first, last = statistics.median(qs[0]), statistics.median(qs[3])
            check("search of an index that did not change stays as quick as it began",
                  last <= first * 3,
                  f"median {first:.1f} ms in the first quarter, {last:.1f} ms in the last")
            print(f"         (control median {first:.1f} -> {last:.1f} ms)")
        else:
            print("         (too few control searches to judge latency; run longer)")
        qs = quarters(load.searches, seconds)
        if all(qs) and len(load.searches) >= 40:
            first, last = statistics.median(qs[0]), statistics.median(qs[3])
            print(f"         (the growing index, for information: median {first:.1f} -> "
                  f"{last:.1f} ms, over an index that went from nothing to "
                  f"{len(expected)} documents)")

        # and it all survives the node going away and coming back
        node.stop(graceful=True)
        node = Node(a.binary, data, pathlib.Path(root) / "node.log")
        check("the node starts again on its data", node.answering(), "it never answered")
        call(f"/{INDEX}/_refresh", "POST")
        missing_after = []
        for doc, _ in sample[:2000]:
            st, r = call(f"/{INDEX}/_doc/{doc}")
            if st != 200 or not r.get("found"):
                missing_after.append(doc)
            if len(missing_after) > 20:
                break
        check("every acknowledged document survived the restart", not missing_after,
              f"{len(missing_after)} gone: {missing_after[:5]}")
        st, r = call(f"/{INDEX}/_search", "POST", {"size": 1, "query": {"match": {"text": "document"}}})
        check("the index still answers a search", st == 200 and bool(r.get("hits")), f"status {st}")
        print(f"\n  {len(expected)} documents acknowledged over {a.minutes:g} minutes, "
              f"{load.attempted} requests attempted")
    finally:
        node.stop()
        if not a.keep:
            shutil.rmtree(root, ignore_errors=True)
    return finish(results, root, a.keep)


def finish(results, root, keep):
    if keep:
        print(f"data left under {root}")
    passed = sum(results)
    print(f"\n  {passed}/{len(results)} checks passed")
    print("RESULT", "a node under steady load keeps what it acknowledged"
          if results and all(results) else "FAILED")
    return 0 if results and all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
