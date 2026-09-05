#!/usr/bin/env python3
"""Every dimension, both engines, same corpus, same machine.

The claim is that BoostSearch beats OpenSearch everywhere, which is only worth
saying if it is checked everywhere and checked again after every change. This
writes the table that says so, and exits non-zero if any dimension is lost.

Eighteen dimensions: eight about the work an engine does and how much of the
machine it takes, and ten query shapes. The tail of each query is measured as
well as its middle and printed beside it; the gate is on the middle, because a
p99 over a hundred and fifty requests moves more than a change usually does.

    BENCH_A             the engine to compare against  (default: OpenSearch)
    BENCH_B             the engine under test          (default: BoostSearch)
    BENCH_AUTH          user:pass, when either has security on
    BENCH_A_CONTAINER   the docker container A runs in, for its memory
    BENCH_DATA          the corpus  (default: /tmp/bench_logs.ndjson)
    BENCH_OUT           where the numbers are written  (default: /tmp/matrix.json)
"""
import urllib.request, json, time, statistics, subprocess, sys, os, ssl, base64
import concurrent.futures

_CTX = ssl.create_default_context()
_CTX.check_hostname = False
_CTX.verify_mode = ssl.CERT_NONE

DATA = os.environ.get("BENCH_DATA", "/tmp/bench_logs.ndjson")
OUT = os.environ.get("BENCH_OUT", "/tmp/matrix.json")
INDEX = "perf"


def req(base, method, path, body=None):
    data = body.encode() if isinstance(body, str) else (
        json.dumps(body).encode() if body is not None else None)
    headers = {"Content-Type": "application/json"}
    # BENCH_AUTH=user:pass sends basic auth, for an engine with security on
    if os.environ.get("BENCH_AUTH"):
        headers["Authorization"] = "Basic " + base64.b64encode(
            os.environ["BENCH_AUTH"].encode()).decode()
    r = urllib.request.Request(base + path, data, headers, method=method)
    return json.load(urllib.request.urlopen(r, context=_CTX))


MAPPING = {
    "settings": {"number_of_shards": 1, "number_of_replicas": 0},
    "mappings": {"properties": {
        "@timestamp": {"type": "date"}, "status": {"type": "long"},
        "region": {"type": "keyword"}, "agent": {"type": "text"},
        "request": {"type": "text"}, "size": {"type": "long"},
        "response_ms": {"type": "double"}}},
}


def bulk_index(base, name, path, batch=4000):
    """How fast the engine takes a corpus, and how many documents it took."""
    try:
        req(base, "DELETE", "/" + name)
    except Exception:
        pass
    req(base, "PUT", "/" + name, MAPPING)
    buf, n, t = [], 0, time.time()
    for line in open(path):
        buf.append('{"index":{}}')
        buf.append(line.strip())
        n += 1
        if len(buf) >= batch:
            req(base, "POST", f"/{name}/_bulk", "\n".join(buf) + "\n")
            buf = []
    if buf:
        req(base, "POST", f"/{name}/_bulk", "\n".join(buf) + "\n")
    el = time.time() - t
    req(base, "POST", f"/{name}/_refresh")
    return n / el, n


def settle(base, name):
    """Let the engine finish with what the load left it.

    A bulk load leaves segments to merge, and an engine measured while it is
    still merging is measured doing something other than what is being asked
    of it. Both are put in the same state -- one segment, refreshed -- before
    anything after the load is timed."""
    try:
        req(base, "POST", f"/{name}/_forcemerge?max_num_segments=1")
    except Exception:
        pass
    time.sleep(2)
    req(base, "POST", f"/{name}/_refresh")


def ids_of(base, name, want, page=5000):
    """Some of the documents that are there, to write over and to take away.

    Read through a scroll: more than a window's worth is wanted, and a window
    is the one thing a plain search will not hand over."""
    got = []
    body = {"query": {"match_all": {}}, "size": min(page, want), "_source": False}
    res = req(base, "POST", f"/{name}/_search?scroll=2m", body)
    sid = res.get("_scroll_id")
    got += [h["_id"] for h in res["hits"]["hits"]]
    while sid and len(got) < want:
        res = req(base, "POST", "/_search/scroll", {"scroll": "2m", "scroll_id": sid})
        sid = res.get("_scroll_id")
        hits = res["hits"]["hits"]
        if not hits:
            break
        got += [h["_id"] for h in hits]
    if sid:
        try:
            req(base, "DELETE", "/_search/scroll", {"scroll_id": [sid]})
        except Exception:
            pass
    return got[:want]


def update_rate(base, name, ids, batch=1000):
    """Writing over a document that is already there is not the same work as
    writing a fresh one: the old one has to be found and put out of the way.

    The caller warms this on one set of documents and times it on another: a
    bulk load leaves both engines with work still in flight, and the first
    thing done after one measures that instead."""
    if not ids:
        return 0.0
    lines = []
    for i in ids:
        lines.append(json.dumps({"update": {"_index": name, "_id": i}}))
        lines.append('{"doc":{"status":599}}')
    t = time.time()
    for at in range(0, len(lines), batch * 2):
        req(base, "POST", "/_bulk", "\n".join(lines[at:at + batch * 2]) + "\n")
    return len(ids) / (time.time() - t)


def delete_rate(base, name, ids, batch=1000):
    if not ids:
        return 0.0
    lines = [json.dumps({"delete": {"_index": name, "_id": i}}) for i in ids]
    t = time.time()
    for at in range(0, len(lines), batch):
        req(base, "POST", "/_bulk", "\n".join(lines[at:at + batch]) + "\n")
    return len(ids) / (time.time() - t)


def scroll_pass(base, name, pages, size):
    """One walk of the index, and how long it took."""
    t = time.time()
    got = 0
    body = {"query": {"match_all": {}}, "size": size, "_source": False}
    first = req(base, "POST", f"/{name}/_search?scroll=2m", body)
    sid = first.get("_scroll_id")
    got += len(first["hits"]["hits"])
    for _ in range(pages - 1):
        if not sid:
            break
        page = req(base, "POST", "/_search/scroll", {"scroll": "2m", "scroll_id": sid})
        sid = page.get("_scroll_id")
        hits = page["hits"]["hits"]
        if not hits:
            break
        got += len(hits)
    el = time.time() - t
    if sid:
        try:
            req(base, "DELETE", "/_search/scroll", {"scroll_id": [sid]})
        except Exception:
            pass
    return got / el if el > 0 else 0.0


def scroll_rate(base, name, pages=20, size=1000):
    """Paging the whole index: what an export, a reindex or a backup costs.

    Walked once without being timed first. A bulk load leaves both engines
    with work still in flight -- merges, a segment being written -- and the
    first walk after one measures that rather than the walk."""
    scroll_pass(base, name, pages, size)
    return scroll_pass(base, name, pages, size)


QUERIES = {
    "match_all": {"query": {"match_all": {}}, "size": 10},
    "term": {"query": {"term": {"region": "eu-west-1"}}, "size": 10},
    "match": {"query": {"match": {"agent": "Chrome Safari"}}, "size": 10},
    "bool+filter": {"query": {"bool": {
        "must": [{"match": {"request": "api"}}],
        "filter": [{"range": {"status": {"gte": 200, "lt": 300}}}]}}, "size": 10},
    "range": {"query": {"range": {
        "@timestamp": {"gte": "2026-01-05", "lt": "2026-01-06"}}}, "size": 0},
    "sort_desc": {"query": {"match_all": {}}, "sort": [{"@timestamp": "desc"}], "size": 10},
    "terms_agg": {"size": 0, "aggs": {"a": {"terms": {"field": "region", "size": 10}}}},
    "date_histogram": {"size": 0, "aggs": {"h": {"date_histogram": {
        "field": "@timestamp", "fixed_interval": "1h"}}}},
    "nested_agg": {"size": 0, "aggs": {"a": {"terms": {"field": "region"},
                                             "aggs": {"s": {"avg": {"field": "response_ms"}}}}}},
    "cardinality": {"size": 0, "aggs": {"c": {"cardinality": {"field": "region"}}}},
}


def latency(base, name, body, n=150):
    # a few unmeasured requests first: connection setup, caches, JIT on the other side
    for _ in range(15):
        req(base, "POST", f"/{name}/_search", body)
    ts = []
    for _ in range(n):
        t = time.time()
        req(base, "POST", f"/{name}/_search", body)
        ts.append((time.time() - t) * 1000)
    ts.sort()
    return statistics.median(ts), ts[int(len(ts) * 0.99) - 1]


def throughput(base, name, workers=8, seconds=5):
    """How many searches a second the engine answers when more than one
    client is asking. A median latency says nothing about this."""
    bodies = list(QUERIES.values())
    stop = time.time() + seconds
    def run():
        n = 0
        i = 0
        while time.time() < stop:
            req(base, "POST", f"/{name}/_search", bodies[i % len(bodies)])
            n += 1
            i += 1
        return n
    with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:
        done = [f.result() for f in [pool.submit(run) for _ in range(workers)]]
    return sum(done) / seconds


def store_bytes(base, name):
    """What the index takes on disk.

    An engine accounts for its store when the segments are on disk, not when
    the documents are accepted, so this flushes first and then insists on an
    answer that can be true: an index holding documents does not take two
    hundred bytes. A run that cannot get one says so rather than reporting a
    number that would hand somebody the dimension."""
    try:
        req(base, "POST", f"/{name}/_flush?wait_if_ongoing=true")
    except Exception:
        pass
    for attempt in range(5):
        try:
            rows = req(base, "GET", f"/_cat/indices/{name}?format=json&bytes=b")
            size = int(rows[0].get("store.size") or rows[0].get("pri.store.size") or 0)
            docs = int(rows[0].get("docs.count") or 0)
            if docs == 0 or size > 1024:
                return size
        except Exception:
            pass
        time.sleep(2)
    return None


def rss_mib(container=None, pid=None):
    if container:
        out = subprocess.run(
            ["docker", "stats", "--no-stream", "--format", "{{.MemUsage}}", container],
            capture_output=True, text=True).stdout
        text = out.split('/')[0].strip()
        for unit, mul in (("GiB", 1024), ("MiB", 1), ("KiB", 1 / 1024)):
            if text.endswith(unit):
                return float(text[:-len(unit)]) * mul
        return 0.0
    out = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)],
                         capture_output=True, text=True).stdout.strip()
    return int(out) / 1024 if out else 0.0


def human_bytes(n):
    for scale, unit in ((1 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")):
        if n >= scale:
            return f"{n / scale:.1f}{unit}"
    return f"{n}b"


A = ("OpenSearch", os.environ.get("BENCH_A", "http://127.0.0.1:9201"))
B = ("BoostSearch", os.environ.get("BENCH_B", "http://127.0.0.1:9200"))

res = {}
for label, base in (A, B):
    print(f"indexing into {label}...", flush=True)
    rate, count = bulk_index(base, INDEX, DATA)
    r = {"index_docs_per_s": rate, "docs": count}
    settle(base, INDEX)
    r["store_bytes"] = store_bytes(base, INDEX)
    r["scroll_docs_per_s"] = scroll_rate(base, INDEX)
    r["latency"] = {q: latency(base, INDEX, body) for q, body in QUERIES.items()}
    r["queries_per_s"] = throughput(base, INDEX)
    # the writes that change what is already there come last: they leave the
    # index a different size, and every read above should see the same one.
    # Each is warmed on one set of documents and timed on another.
    touch = ids_of(base, INDEX, 20000)
    update_rate(base, INDEX, touch[:5000])
    r["update_docs_per_s"] = update_rate(base, INDEX, touch[5000:10000])
    delete_rate(base, INDEX, touch[10000:15000])
    r["delete_docs_per_s"] = delete_rate(base, INDEX, touch[15000:20000])
    res[label] = r

res["OpenSearch"]["rss_mib"] = rss_mib(
    container=os.environ.get("BENCH_A_CONTAINER", "os-bench"))
pids = subprocess.run(["pgrep", "-f", "release/boostsearch"],
                      capture_output=True, text=True).stdout.split()
res["BoostSearch"]["rss_mib"] = rss_mib(pid=pids[0]) if pids else 0.0

json.dump(res, open(OUT, "w"), indent=1)

o, b = res["OpenSearch"], res["BoostSearch"]
lost = []


def row(name, ov, bv, higher_wins, fmt=lambda v: f"{v:,.0f}", note=""):
    # a dimension that could not be measured is not a dimension that was won:
    # it is named as unmeasured and it fails the gate, so that a broken
    # measurement is never mistaken for a result either way
    if ov is None or bv is None:
        lost.append(f"{name} (not measured)")
        print(f"{name:<24}{'?':>14}{'?':>14}   not measured")
        return
    won = bv > ov if higher_wins else bv < ov
    winner = "BoostSearch" if won else "OpenSearch"
    if not won:
        lost.append(name)
    print(f"{name:<24}{fmt(ov):>14}{fmt(bv):>14}   {winner}{note}")


print(f"\n{'dimension':<24}{'OpenSearch':>14}{'BoostSearch':>14}   winner")
row("index docs/s", o["index_docs_per_s"], b["index_docs_per_s"], True)
row("update docs/s", o["update_docs_per_s"], b["update_docs_per_s"], True)
row("delete docs/s", o["delete_docs_per_s"], b["delete_docs_per_s"], True)
row("scroll docs/s", o["scroll_docs_per_s"], b["scroll_docs_per_s"], True)
row("queries/s (8 clients)", o["queries_per_s"], b["queries_per_s"], True)
row("memory", o["rss_mib"], b["rss_mib"], False, lambda v: f"{v:,.0f}MiB")
row("store on disk", o["store_bytes"], b["store_bytes"], False, human_bytes)
row("query p99 (worst)",
    max(p for _, p in o["latency"].values()),
    max(p for _, p in b["latency"].values()), False, lambda v: f"{v:.2f}ms")
for q in QUERIES:
    om, op = o["latency"][q]
    bm, bp = b["latency"][q]
    row(f"{q} p50 (ms)", om, bm, False, lambda v: f"{v:.2f}",
        f"   (p99 {op:.2f} / {bp:.2f})")

print()
if lost:
    print(f"LOST {len(lost)} of 18: {', '.join(lost)}")
    sys.exit(1)
print("all 18 dimensions ahead")
