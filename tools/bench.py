#!/usr/bin/env python3
"""Benchmark an OpenSearch-compatible endpoint: indexing throughput, query
latency, and process RSS.

Engine-agnostic on purpose -- the same run drives boostsearch or a real OpenSearch
node, so the numbers are comparable rather than self-reported.

The qps figures here are NOT a throughput ceiling. This driver is Python, and
one process saturates on its own GIL long before either engine does: measured
against the same server, it reported 15k requests/s for a trivial GET where k6
measured 152k, and 10k for a search where k6 measured 23k. Read the latency
numbers, which are per-request and hold up; for throughput use a native load
generator (`k6 run tools/load.js`).
"""
import argparse, json, os, pathlib, statistics, subprocess, sys, time
from concurrent.futures import ThreadPoolExecutor
import requests

MAPPING = {
    "settings": {"index": {"number_of_shards": 1, "number_of_replicas": 0}},
    "mappings": {
        "properties": {
            "@timestamp": {"type": "date"},
            "clientip":   {"type": "keyword"},
            "method":     {"type": "keyword"},
            "request":    {"type": "text"},
            "status":     {"type": "integer"},
            "size":       {"type": "long"},
            "response_ms": {"type": "float"},
            "agent":      {"type": "text"},
            "region":     {"type": "keyword"},
            "referer":    {"type": "keyword"},
        }
    },
}

# name -> (body, weight). Weights mirror a log-analytics mix: mostly filters and
# aggregations, some full-text, some sorted paging.
def query_mix(index):
    return [
        ("match_all",      {"query": {"match_all": {}}, "size": 10}, 10),
        ("term_keyword",   {"query": {"term": {"region": "eu-west-1"}}, "size": 10}, 15),
        ("term_numeric",   {"query": {"term": {"status": 404}}, "size": 10}, 10),
        ("range_numeric",  {"query": {"range": {"size": {"gte": 5000, "lt": 50000}}}, "size": 10}, 15),
        ("match_text",     {"query": {"match": {"request": "api orders"}}, "size": 10}, 10),
        ("bool_filter",    {"query": {"bool": {
                                "must": [{"match": {"request": "api"}}],
                                "filter": [{"term": {"method": "GET"}},
                                           {"range": {"response_ms": {"gte": 20}}}]}},
                            "size": 10}, 15),
        ("agg_terms",      {"size": 0, "aggs": {"by_status": {"terms": {"field": "status", "size": 10}}}}, 10),
        ("agg_date_hist",  {"size": 0, "aggs": {"over_time": {
                                "date_histogram": {"field": "@timestamp", "fixed_interval": "1d"}}}}, 5),
        ("agg_nested",     {"size": 0, "aggs": {"by_region": {
                                "terms": {"field": "region"},
                                "aggs": {"avg_ms": {"avg": {"field": "response_ms"}},
                                         "p_size": {"stats": {"field": "size"}}}}}}, 5),
        ("sort_paged",     {"query": {"match_all": {}}, "sort": [{"size": "desc"}], "size": 10, "from": 100}, 5),
        # every real log query carries a time filter; the mix was missing it
        ("time_range",     {"query": {"range": {"@timestamp": {
                                "gte": "2026-01-01T00:00:00Z", "lt": "2026-01-02T17:40:00Z"}}},
                            "size": 10}, 15),
        ("time_range_agg", {"size": 0,
                            "query": {"range": {"@timestamp": {
                                "gte": "2026-01-01T00:00:00Z", "lt": "2026-01-02T17:40:00Z"}}},
                            "aggs": {"by_status": {"terms": {"field": "status", "size": 10}}}}, 10),
    ]


def rss_mb(pattern):
    """Resident set size of the server, in MB.

    `docker:<name>` reads the container's usage instead, since a containerised
    server is invisible to the host's process table on macOS.
    """
    if pattern.startswith("docker:"):
        name = pattern.split(":", 1)[1]
        try:
            out = subprocess.run(
                ["docker", "stats", "--no-stream", "--format", "{{.MemUsage}}", name],
                capture_output=True, text=True, timeout=30).stdout.strip()
        except Exception:
            return None
        if not out:
            return None
        used = out.split("/")[0].strip()
        num = float("".join(c for c in used if c.isdigit() or c == "."))
        unit = "".join(c for c in used if c.isalpha()).lower()
        factor = {"b": 1 / 1e6, "kib": 1 / 1024, "kb": 1e-3,
                  "mib": 1.048576, "mb": 1.0, "gib": 1073.741824, "gb": 1000.0}
        return round(num * factor.get(unit, 1.0), 1)
    try:
        out = subprocess.run(["ps", "-Ao", "rss,command"], capture_output=True, text=True).stdout
    except Exception:
        return None
    best = 0
    for line in out.splitlines()[1:]:
        parts = line.strip().split(None, 1)
        if len(parts) != 2:
            continue
        kb, cmd = parts
        if pattern in cmd and "ps -Ao" not in cmd:
            best = max(best, int(kb))
    return round(best / 1024, 1) if best else None


def pct(xs, p):
    if not xs:
        return 0.0
    xs = sorted(xs)
    k = (len(xs) - 1) * p / 100
    lo, hi = int(k), min(int(k) + 1, len(xs) - 1)
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)


def index_corpus(base, index, path, batch, session):
    with open(path) as f:
        lines = f.readlines()
    total = len(lines)
    header = json.dumps({"index": {"_index": index}}) + "\n"
    sent = 0
    t0 = time.perf_counter()
    buf = []
    for line in lines:
        buf.append(header)
        buf.append(line)
        if len(buf) >= batch * 2:
            r = session.post(f"{base}/_bulk", data="".join(buf),
                             headers={"Content-Type": "application/x-ndjson"}, timeout=300)
            r.raise_for_status()
            if r.json().get("errors"):
                bad = [i for i in r.json()["items"] if list(i.values())[0].get("status", 200) >= 300]
                raise SystemExit(f"bulk errors, first: {json.dumps(bad[:1])[:300]}")
            sent += len(buf) // 2
            buf = []
            print(f"\r  indexed {sent}/{total}", end="", flush=True)
    if buf:
        r = session.post(f"{base}/_bulk", data="".join(buf),
                         headers={"Content-Type": "application/x-ndjson"}, timeout=300)
        r.raise_for_status()
        sent += len(buf) // 2
    print(f"\r  indexed {sent}/{total}")
    elapsed = time.perf_counter() - t0
    session.post(f"{base}/{index}/_refresh", timeout=300)
    refreshed = time.perf_counter() - t0
    return {"docs": total, "index_seconds": round(elapsed, 2),
            "index_docs_per_sec": round(total / elapsed),
            "index_plus_refresh_seconds": round(refreshed, 2)}


def run_queries(base, index, rounds, concurrency, warmup):
    mix = query_mix(index)
    plan = []
    for name, body, weight in mix:
        plan.extend([(name, body)] * weight)
    per_query = {name: [] for name, _, _ in mix}

    def once(item):
        name, body = item
        s = time.perf_counter()
        r = requests.post(f"{base}/{index}/_search", json=body, timeout=120)
        dt = (time.perf_counter() - s) * 1000
        if r.status_code >= 300:
            raise SystemExit(f"{name} -> {r.status_code}: {r.text[:300]}")
        return name, dt

    with ThreadPoolExecutor(max_workers=concurrency) as pool:
        list(pool.map(once, plan * warmup))
        t0 = time.perf_counter()
        for name, dt in pool.map(once, plan * rounds):
            per_query[name].append(dt)
        wall = time.perf_counter() - t0

    total_n = sum(len(v) for v in per_query.values())
    summary = {
        "queries": total_n,
        "concurrency": concurrency,
        "qps": round(total_n / wall, 1),
        "latency_ms": {
            "p50": round(pct([x for v in per_query.values() for x in v], 50), 2),
            "p90": round(pct([x for v in per_query.values() for x in v], 90), 2),
            "p99": round(pct([x for v in per_query.values() for x in v], 99), 2),
            "max": round(max(x for v in per_query.values() for x in v), 2),
        },
        "per_query_ms": {
            name: {"n": len(v), "p50": round(pct(v, 50), 2), "p90": round(pct(v, 90), 2),
                   "p99": round(pct(v, 99), 2)}
            for name, v in per_query.items() if v
        },
    }
    return summary


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default="http://127.0.0.1:9200")
    ap.add_argument("--index", default="bench_logs")
    ap.add_argument("--data", default="bench/data/http_logs.ndjson")
    ap.add_argument("--batch", type=int, default=2000)
    ap.add_argument("--rounds", type=int, default=3)
    ap.add_argument("--warmup", type=int, default=1)
    ap.add_argument("--concurrency", type=int, nargs="*", default=[1, 8])
    ap.add_argument("--proc", default="boostsearch", help="process name to sample RSS for")
    ap.add_argument("--label", default="boostsearch")
    ap.add_argument("--out", default="")
    args = ap.parse_args()

    s = requests.Session()
    try:
        root = s.get(args.url, timeout=10).json()
    except Exception as e:
        raise SystemExit(f"cannot reach {args.url}: {e}")

    result = {
        "label": args.label,
        "target": root.get("version", {}),
        "rss_mb_idle": rss_mb(args.proc),
    }

    s.delete(f"{args.url}/{args.index}", timeout=60)
    r = s.put(f"{args.url}/{args.index}", json=MAPPING, timeout=60)
    if r.status_code >= 300:
        raise SystemExit(f"create index failed: {r.status_code} {r.text[:300]}")

    print(f"[{args.label}] indexing…")
    result["indexing"] = index_corpus(args.url, args.index, args.data, args.batch, s)
    result["rss_mb_after_index"] = rss_mb(args.proc)

    count = s.get(f"{args.url}/{args.index}/_count", timeout=60).json().get("count")
    result["doc_count"] = count
    print(f"[{args.label}] doc_count={count}  rss={result['rss_mb_after_index']} MB")

    result["search"] = []
    for c in args.concurrency:
        print(f"[{args.label}] querying at concurrency {c}…")
        result["search"].append(run_queries(args.url, args.index, args.rounds, c, args.warmup))
    result["rss_mb_after_search"] = rss_mb(args.proc)

    print(json.dumps(result, indent=1))
    if args.out:
        pathlib.Path(args.out).parent.mkdir(parents=True, exist_ok=True)
        pathlib.Path(args.out).write_text(json.dumps(result, indent=1))
        print(f"wrote {args.out}")


main()
