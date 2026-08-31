#!/usr/bin/env python3
"""What a running OpenSearch cluster asks for, and whether BoostSearch answers it.

Two modes, and a migration wants both.

  inventory  reads a cluster's indices, mappings, settings and templates, and
             says which of them BoostSearch cannot honour -- including the ones
             it would accept and quietly get wrong.

  replay     sends the same captured requests to two servers and compares the
             answers. This is the one that decides a cutover: not whether the
             feature list looks complete, but whether the two engines say the
             same thing about the same data.
"""
import argparse, json, re, sys, pathlib, difflib
import requests

HERE = pathlib.Path(__file__).resolve().parent.parent

# ---------------------------------------------------------------- support map

# The names worth asking about: everything OpenSearch registers, so the answer
# is "what does this engine do about each of them" rather than "what did we
# remember to list".
QUERIES = """bool boosting common constant_score dis_max distance_feature exists function_score
 fuzzy geo_bounding_box geo_distance geo_polygon geo_shape has_child has_parent ids intervals
 knn match match_all match_bool_prefix match_none match_phrase match_phrase_prefix more_like_this
 multi_match nested parent_id percolate prefix query_string range rank_feature regexp script
 script_score simple_query_string span_containing span_first span_multi span_near span_not span_or
 span_term span_within term terms terms_set wildcard wrapper""".split()

AGGS = """adjacency_matrix auto_date_histogram avg avg_bucket bucket_script bucket_selector
 bucket_sort cardinality children composite cumulative_sum date_histogram date_range derivative
 diversified_sampler extended_stats filter filters geo_bounds geo_centroid geo_distance geohash_grid
 global histogram ip_range matrix_stats max median_absolute_deviation min missing moving_avg
 moving_fn multi_terms nested percentile_ranks percentiles range rare_terms reverse_nested sampler
 scripted_metric serial_diff significant_terms significant_text stats sum sum_bucket terms top_hits
 value_count variable_width_histogram weighted_avg""".split()

TYPES = """binary boolean byte completion constant_keyword date date_nanos date_range double
 double_range flat_object float float_range geo_point geo_shape half_float integer integer_range
 ip ip_range keyword knn_vector long long_range match_only_text nested object percolator
 rank_feature rank_features scaled_float search_as_you_type short text token_count
 unsigned_long wildcard""".split()

ANALYZERS = """standard simple whitespace stop keyword pattern english french german italian
 spanish portuguese russian arabic thai chinese cjk fingerprint""".split()


def probe(url):
    """Ask an engine what it actually answers, rather than trusting a list."""
    sess = requests.Session()
    base = url.rstrip("/")
    idx = "boostsearch_compat_probe"
    sess.delete(f"{base}/{idx}", timeout=30)

    types = set()
    for t in TYPES:
        body = {"mappings": {"properties": {"f": {"type": t}}}}
        if t in ("scaled_float",):
            body["mappings"]["properties"]["f"]["scaling_factor"] = 100
        if t in ("knn_vector",):
            body["mappings"]["properties"]["f"]["dimension"] = 2
        name = f"{idx}_{t}"
        sess.delete(f"{base}/{name}", timeout=30)
        r = sess.put(f"{base}/{name}", json=body, timeout=30)
        if r.status_code < 300:
            types.add(t)
        sess.delete(f"{base}/{name}", timeout=30)

    sess.put(f"{base}/{idx}", json={"mappings": {"properties": {"f": {"type": "text"}}}}, timeout=30)
    sess.post(f"{base}/{idx}/_doc?refresh=true", json={"f": "probe"}, timeout=30)

    def unknown(text):
        low = text.lower()
        return (
            "unknown query" in low
            or "unknown aggregation" in low
            or "no [query] registered" in low
            or "not_implemented" in low
            or "unknown key for a start_object" in low and "aggregation" in low
        )

    queries = set()
    for q in QUERIES:
        r = sess.post(f"{base}/{idx}/_search", json={"query": {q: {}}}, timeout=30)
        if not unknown(r.text):
            queries.add(q)

    aggs = set()
    for a in AGGS:
        r = sess.post(f"{base}/{idx}/_search", json={"size": 0, "aggs": {"probe": {a: {}}}}, timeout=30)
        if not unknown(r.text):
            aggs.add(a)

    analyzers = set()
    for a in ANALYZERS:
        r = sess.post(f"{base}/_analyze", json={"analyzer": a, "text": "Probe Text"}, timeout=30)
        if r.status_code < 300 and r.json().get("tokens"):
            analyzers.add(a)

    # a custom analyser is the one that is accepted and then ignored, so it is
    # asked about by what it does rather than by whether it was accepted
    custom = f"{idx}_custom"
    sess.delete(f"{base}/{custom}", timeout=30)
    sess.put(f"{base}/{custom}", json={
        "settings": {"analysis": {"analyzer": {"folded": {
            "type": "custom", "tokenizer": "standard", "filter": ["lowercase", "asciifolding"]}}}},
        "mappings": {"properties": {"t": {"type": "text", "analyzer": "folded"}}},
    }, timeout=30)
    sess.post(f"{base}/{custom}/_doc?refresh=true", json={"t": "Café"}, timeout=30)
    r = sess.post(f"{base}/{custom}/_search", json={"query": {"match": {"t": "cafe"}}}, timeout=30)
    honours_custom = bool(r.json().get("hits", {}).get("total", {}).get("value"))
    sess.delete(f"{base}/{custom}", timeout=30)
    sess.delete(f"{base}/{idx}", timeout=30)

    return {
        "queries": queries,
        "aggs": aggs,
        "types": types,
        "analyzers": analyzers,
        "custom_analysis": honours_custom,
    }


# ---------------------------------------------------------------- inventory

def inventory(url, out, have):
    sess = requests.Session()
    get = lambda p: sess.get(url.rstrip("/") + p, timeout=30).json()
    report = {"cluster": url, "indices": [], "gaps": {}, "capacity": {}}

    stats = get("/_stats")
    total_docs = stats.get("_all", {}).get("primaries", {}).get("docs", {}).get("count", 0)
    total_bytes = stats.get("_all", {}).get("primaries", {}).get("store", {}).get("size_in_bytes", 0)
    report["capacity"] = {
        "documents": total_docs,
        "primary_bytes": total_bytes,
        "primary_gb": round(total_bytes / 1e9, 2),
    }

    mappings = get("/_all/_mapping")
    settings = get("/_all/_settings")
    used_types, used_analyzers, custom_analysis = set(), set(), {}

    def walk(props, prefix=""):
        for name, spec in (props or {}).items():
            if not isinstance(spec, dict):
                continue
            t = spec.get("type")
            if t:
                used_types.add(t)
            for key in ("analyzer", "search_analyzer", "normalizer"):
                if spec.get(key):
                    used_analyzers.add(spec[key])
            walk(spec.get("properties"), f"{prefix}{name}.")
            walk(spec.get("fields"), f"{prefix}{name}.")

    for index, body in mappings.items():
        if index.startswith("."):
            continue
        walk(body.get("mappings", {}).get("properties"))
        analysis = (
            settings.get(index, {})
            .get("settings", {})
            .get("index", {})
            .get("analysis")
        )
        if analysis:
            custom_analysis[index] = analysis
        idx_settings = settings.get(index, {}).get("settings", {}).get("index", {})
        report["indices"].append({
            "name": index,
            "shards": idx_settings.get("number_of_shards"),
            "replicas": idx_settings.get("number_of_replicas"),
        })

    report["gaps"]["field_types"] = sorted(used_types - have["types"])
    report["gaps"]["analyzers"] = sorted(
        a for a in used_analyzers if a not in have["analyzers"]
    )
    report["gaps"]["indices_with_custom_analysis"] = sorted(custom_analysis)
    report["used"] = {
        "field_types": sorted(used_types),
        "analyzers": sorted(used_analyzers),
    }
    pathlib.Path(out).write_text(json.dumps(report, indent=1))
    return report


# ---------------------------------------------------------------- replay

# What two engines are allowed to disagree about, and what a migration cares
# about instead: the documents that came back, in the order they came back, the
# numbers over them, and the tokens a text was cut into.
def answer_of(path, body):
    if "/_analyze" in path:
        return {"tokens": [t.get("token") for t in body.get("tokens", [])]}
    if "/_search" in path or "/_msearch" in path:
        hits = body.get("hits", {})
        out = {
            "total": hits.get("total", {}).get("value") if isinstance(hits.get("total"), dict) else hits.get("total"),
            "ids": [h.get("_id") for h in hits.get("hits", [])],
        }
        for key in ("aggregations", "suggest"):
            if key in body:
                out[key] = body[key]
        if "responses" in body:
            out["responses"] = [answer_of("/_search", r) for r in body["responses"]]
        return out
    if "/_count" in path:
        return {"count": body.get("count")}
    return body


VOLATILE = re.compile(r'^(took|_shards|_seq_no|_primary_term|_version|timed_out|max_score|_score|_id|uuid|cluster_uuid|start_time.*|end_time.*|duration.*|_index|_node|node|name|version|build.*)$')

def scrub(node, keep_scores):
    """Drop what two engines are allowed to disagree about."""
    if isinstance(node, dict):
        out = {}
        for k, v in node.items():
            if VOLATILE.match(k) and not (keep_scores and k in ("_score", "max_score", "_id", "_index")):
                continue
            out[k] = scrub(v, keep_scores)
        return out
    if isinstance(node, list):
        return [scrub(v, keep_scores) for v in node]
    if isinstance(node, float):
        return round(node, 4)
    return node


def replay(requests_file, a_url, b_url, out, keep_scores, strict=False):
    sess = requests.Session()
    rows, same, differ, failed = [], 0, 0, 0
    for line in pathlib.Path(requests_file).read_text().splitlines():
        line = line.strip()
        if not line:
            continue
        req = json.loads(line)
        method = req.get("method", "GET").upper()
        path = req["path"]
        body = req.get("body")
        answers = {}
        for label, base in (("a", a_url), ("b", b_url)):
            try:
                r = sess.request(
                    method,
                    base.rstrip("/") + path,
                    json=body if isinstance(body, (dict, list)) else None,
                    data=body if isinstance(body, str) else None,
                    headers={"Content-Type": "application/json"},
                    timeout=60,
                )
                answers[label] = (r.status_code, r.json() if r.text.strip() else {})
            except Exception as e:
                answers[label] = (0, {"__error": str(e)})
        (code_a, body_a), (code_b, body_b) = answers["a"], answers["b"]
        if strict:
            shown_a, shown_b = body_a, body_b
        else:
            shown_a, shown_b = answer_of(path, body_a), answer_of(path, body_b)
        left = json.dumps(scrub(shown_a, keep_scores), indent=1, sort_keys=True).splitlines()
        right = json.dumps(scrub(shown_b, keep_scores), indent=1, sort_keys=True).splitlines()
        if code_a == code_b and left == right:
            same += 1
            continue
        if code_a == 0 or code_b == 0:
            failed += 1
        else:
            differ += 1
        rows.append({
            "method": method,
            "path": path,
            "body": body,
            "status": [code_a, code_b],
            "diff": list(difflib.unified_diff(left, right, "opensearch", "boostsearch", n=1))[:40],
        })
    pathlib.Path(out).write_text(json.dumps({"same": same, "differ": differ, "failed": failed, "rows": rows}, indent=1))
    return same, differ, failed, rows



# ---------------------------------------------------------------- corpus

SETUP = [
    ("DELETE", "/compat", None),
    ("PUT", "/compat", {
        "settings": {"number_of_shards": 1, "number_of_replicas": 0,
                     "analysis": {"analyzer": {"folded": {
                         "type": "custom", "tokenizer": "standard",
                         "filter": ["lowercase", "asciifolding"]}}}},
        "mappings": {"properties": {
            "title": {"type": "text"},
            "folded": {"type": "text", "analyzer": "folded"},
            "tag": {"type": "keyword"},
            "n": {"type": "long"},
            "price": {"type": "double"},
            "when": {"type": "date"},
            "ok": {"type": "boolean"},
            "ip": {"type": "ip"},
            "loc": {"type": "geo_point"},
            "span": {"type": "date_range"},
            "obj": {"type": "nested", "properties": {"k": {"type": "keyword"}, "v": {"type": "long"}}},
        }},
    }),
]

DOCS = [
    {"title": "the quick brown fox", "folded": "Café Résumé", "tag": "a", "n": 1, "price": 9.5,
     "when": "2024-01-01", "ok": True, "ip": "10.0.0.1", "loc": {"lat": 13.7, "lon": 100.5},
     "span": {"gte": "2024-01-01", "lte": "2024-06-01"},
     "obj": [{"k": "x", "v": 1}, {"k": "y", "v": 2}]},
    {"title": "a quick red dog", "folded": "cafe resume", "tag": "b", "n": 20, "price": 12.0,
     "when": "2025-06-15", "ok": False, "ip": "10.0.0.9", "loc": {"lat": 51.5, "lon": -0.1},
     "span": {"gte": "2025-01-01", "lte": "2025-06-01"},
     "obj": [{"k": "x", "v": 5}]},
    {"title": "brown dogs jumping over", "folded": "CAFÉ", "tag": "a", "n": 300, "price": 3.25,
     "when": "2026-02-02", "ok": True, "ip": "192.168.1.4", "loc": {"lat": -33.9, "lon": 151.2},
     "span": {"gte": "2026-01-01", "lte": "2026-03-01"},
     "obj": [{"k": "z", "v": 9}]},
]

TEXTS = ["The Quick Brown Foxes", "Café Résumé naïve", "running runner ran", "ELASTIC search 2024"]

def corpus(out):
    """Requests that ask both engines the same questions about the same data."""
    rows = list(SETUP)
    for i, d in enumerate(DOCS):
        rows.append(("PUT", f"/compat/_doc/{i+1}?refresh=true", d))

    def search(body):
        rows.append(("POST", "/compat/_search", body))

    # every analyser, on every text: the tokens are the answer
    for a in ANALYZERS:
        for t in TEXTS:
            rows.append(("POST", "/_analyze", {"analyzer": a, "text": t}))
    rows.append(("POST", "/compat/_analyze", {"analyzer": "folded", "text": "Café Résumé"}))

    queries = {
        "match_all": {}, "match": {"title": "quick"}, "match_phrase": {"title": "quick brown"},
        "match_bool_prefix": {"title": "quick bro"}, "match_phrase_prefix": {"title": "quick bro"},
        "multi_match": {"query": "quick", "fields": ["title", "tag"]},
        "term": {"tag": "a"}, "terms": {"tag": ["a", "b"]}, "ids": {"values": ["1", "2"]},
        "range": {"n": {"gte": 2, "lt": 500}}, "exists": {"field": "ip"},
        "prefix": {"tag": "a"}, "wildcard": {"tag": "a*"}, "regexp": {"tag": "a|b"},
        "fuzzy": {"title": {"value": "quik", "fuzziness": 1}},
        "bool": {"must": [{"match": {"title": "quick"}}], "filter": [{"term": {"tag": "a"}}]},
        "dis_max": {"queries": [{"match": {"title": "quick"}}, {"term": {"tag": "b"}}]},
        "boosting": {"positive": {"match": {"title": "quick"}}, "negative": {"term": {"tag": "b"}}, "negative_boost": 0.2},
        "constant_score": {"filter": {"term": {"tag": "a"}}},
        "query_string": {"query": "title:quick AND tag:a"},
        "simple_query_string": {"query": "quick + brown", "fields": ["title"]},
        "nested": {"path": "obj", "query": {"term": {"obj.k": "x"}}},
        "geo_distance": {"distance": "100km", "loc": {"lat": 13.75, "lon": 100.5}},
        "geo_bounding_box": {"loc": {"top_left": {"lat": 60, "lon": -10}, "bottom_right": {"lat": -40, "lon": 160}}},
        "terms_set": {"tag": {"terms": ["a", "b"], "minimum_should_match_script": {"source": "1"}}},
        "distance_feature": {"field": "when", "pivot": "30d", "origin": "2025-01-01"},
        "intervals": {"title": {"match": {"query": "quick brown", "max_gaps": 1}}},
        "rank_feature": {"field": "price"},
        "function_score": {"query": {"match_all": {}}, "boost": 2},
        "script_score": {"query": {"match_all": {}}, "script": {"source": "1.0"}},
        "script": {"script": {"source": "true"}},
        "wrapper": {"query": "eyJ0ZXJtIjp7InRhZyI6ImEifX0="},
        "more_like_this": {"fields": ["title"], "like": "quick brown", "min_term_freq": 1, "min_doc_freq": 1},
        "span_term": {"title": "quick"},
        "span_near": {"clauses": [{"span_term": {"title": "quick"}}, {"span_term": {"title": "brown"}}], "slop": 1, "in_order": True},
        "percolate": {"field": "query", "document": {"title": "quick"}},
        "knn": {"loc": {"vector": [1, 2], "k": 1}},
        "has_child": {"type": "x", "query": {"match_all": {}}},
        "has_parent": {"parent_type": "x", "query": {"match_all": {}}},
        "parent_id": {"type": "x", "id": "1"},
        "common": {"title": {"query": "quick brown"}},
    }
    for name, q in queries.items():
        search({"query": {name: q}, "size": 3, "sort": [{"n": "asc"}]})

    aggs = {
        "terms": {"field": "tag"}, "histogram": {"field": "n", "interval": 50},
        "date_histogram": {"field": "when", "calendar_interval": "year"},
        "date_range": {"field": "when", "ranges": [{"to": "2025-01-01"}, {"from": "2025-01-01"}]},
        "range": {"field": "n", "ranges": [{"to": 50}, {"from": 50}]},
        "ip_range": {"field": "ip", "ranges": [{"to": "10.0.0.5"}, {"from": "10.0.0.5"}]},
        "avg": {"field": "price"}, "sum": {"field": "price"}, "min": {"field": "n"},
        "max": {"field": "n"}, "stats": {"field": "price"}, "extended_stats": {"field": "price"},
        "value_count": {"field": "tag"}, "cardinality": {"field": "tag"},
        "percentiles": {"field": "price"}, "percentile_ranks": {"field": "price", "values": [5, 10]},
        "median_absolute_deviation": {"field": "price"},
        "missing": {"field": "nothing"}, "filter": {"term": {"tag": "a"}},
        "filters": {"filters": {"a": {"term": {"tag": "a"}}, "b": {"term": {"tag": "b"}}}},
        "global": {}, "nested": {"path": "obj"},
        "sampler": {"shard_size": 2}, "diversified_sampler": {"field": "tag", "shard_size": 2},
        "significant_terms": {"field": "tag"}, "rare_terms": {"field": "tag"},
        "multi_terms": {"terms": [{"field": "tag"}, {"field": "ok"}]},
        "composite": {"sources": [{"t": {"terms": {"field": "tag"}}}]},
        "auto_date_histogram": {"field": "when", "buckets": 2},
        "variable_width_histogram": {"field": "n", "buckets": 2},
        "geo_distance": {"field": "loc", "origin": "13.7,100.5", "ranges": [{"to": 1000}]},
        "geo_bounds": {"field": "loc"}, "geo_centroid": {"field": "loc"},
        "geohash_grid": {"field": "loc", "precision": 3},
        "top_hits": {"size": 1}, "weighted_avg": {"value": {"field": "price"}, "weight": {"field": "n"}},
        "matrix_stats": {"fields": ["n", "price"]},
        "scripted_metric": {"init_script": "state.s=0", "map_script": "state.s+=1",
                            "combine_script": "return state.s", "reduce_script": "return 1"},
        "adjacency_matrix": {"filters": {"a": {"term": {"tag": "a"}}, "b": {"term": {"tag": "b"}}}},
    }
    for name, a in aggs.items():
        search({"size": 0, "aggs": {"probe": {name: a}}})

    # pipelines, which read the buckets above
    search({"size": 0, "aggs": {"per": {"date_histogram": {"field": "when", "calendar_interval": "year"},
                                        "aggs": {"total": {"sum": {"field": "price"}}}},
                                "sum_total": {"sum_bucket": {"buckets_path": "per>total"}},
                                "max_total": {"max_bucket": {"buckets_path": "per>total"}},
                                "avg_total": {"avg_bucket": {"buckets_path": "per>total"}},
                                "stats_total": {"stats_bucket": {"buckets_path": "per>total"}}}})
    search({"size": 0, "aggs": {"per": {"date_histogram": {"field": "when", "calendar_interval": "year"},
                                        "aggs": {"total": {"sum": {"field": "price"}},
                                                 "run": {"cumulative_sum": {"buckets_path": "total"}},
                                                 "diff": {"derivative": {"buckets_path": "total"}},
                                                 "mv": {"moving_fn": {"buckets_path": "total", "window": 2,
                                                                      "script": "MovingFunctions.max(values)"}}}}}})

    # the rest of a search request: what a client actually sends
    search({"query": {"match": {"title": "quick"}}, "highlight": {"fields": {"title": {}}}})
    search({"query": {"match_all": {}}, "collapse": {"field": "tag"}, "sort": [{"n": "asc"}]})
    search({"query": {"match_all": {}}, "_source": ["title", "tag"], "size": 2})
    search({"query": {"match_all": {}}, "fields": ["tag", {"field": "when", "format": "yyyy"}], "size": 2})
    search({"query": {"match_all": {}}, "docvalue_fields": ["n"], "size": 2})
    search({"query": {"match_all": {}}, "sort": [{"obj.v": {"order": "desc", "nested": {"path": "obj"}}}]})
    search({"query": {"match_all": {}}, "search_after": [20], "sort": [{"n": "asc"}], "size": 2})
    search({"query": {"match_all": {}}, "rescore": {"window_size": 3, "query": {"rescore_query": {"match": {"title": "brown"}}}}})
    search({"suggest": {"s": {"text": "quik", "term": {"field": "title"}}}})
    search({"query": {"match": {"title": "quick"}}, "explain": True})
    search({"query": {"nested": {"path": "obj", "query": {"term": {"obj.k": "x"}}, "inner_hits": {}}}})
    search({"query": {"match_all": {}}, "aggs": {"t": {"terms": {"field": "tag", "order": {"p": "desc"}},
                                                       "aggs": {"p": {"avg": {"field": "price"}}}}}})
    rows.append(("GET", "/compat/_mapping", None))
    rows.append(("GET", "/compat/_settings", None))
    rows.append(("GET", "/compat/_count", None))
    rows.append(("POST", "/compat/_field_caps?fields=*", None))
    rows.append(("GET", "/compat/_doc/1", None))
    rows.append(("POST", "/compat/_mget", {"ids": ["1", "2"]}))
    rows.append(("POST", "/compat/_msearch",
                 '{"index":"compat"}\n{"query":{"match_all":{}}}\n'))
    rows.append(("POST", "/compat/_termvectors/1", {"fields": ["title"]}))
    rows.append(("POST", "/compat/_update/1", {"doc": {"n": 2}}))
    rows.append(("POST", "/compat/_delete_by_query", {"query": {"term": {"tag": "zzz"}}}))
    rows.append(("POST", "/compat/_update_by_query", {"query": {"term": {"tag": "zzz"}}}))

    with open(out, "w") as f:
        for method, path, body in rows:
            f.write(json.dumps({"method": method, "path": path, "body": body}) + "\n")
    return len(rows)

def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="mode", required=True)
    inv = sub.add_parser("inventory")
    inv.add_argument("--cluster", required=True, help="the OpenSearch being replaced")
    inv.add_argument("--engine", required=True, help="the BoostSearch replacing it")
    inv.add_argument("--out", default="compat-inventory.json")
    rep = sub.add_parser("replay")
    rep.add_argument("--requests", required=True)
    rep.add_argument("--a", required=True, help="the engine being replaced")
    rep.add_argument("--b", required=True, help="the engine replacing it")
    rep.add_argument("--out", default="compat-replay.json")
    rep.add_argument("--scores", action="store_true", help="compare scores and hit order too")
    rep.add_argument("--strict", action="store_true", help="compare whole responses, not just the answers")
    cor = sub.add_parser("corpus")
    cor.add_argument("--out", default="compat-corpus.ndjson")
    args = ap.parse_args()

    if args.mode == "corpus":
        n = corpus(args.out)
        print(f"{n} requests written to {args.out}")
        sys.exit(0)

    if args.mode == "inventory":
        have = probe(args.engine)
        if not have["custom_analysis"]:
            print(f"  [GAP] {args.engine} accepts a custom analyzer and does not apply it")
        r = inventory(args.cluster, args.out, have)
        print(f"{len(r['indices'])} indices, {r['capacity']['documents']:,} documents, "
              f"{r['capacity']['primary_gb']} GB of primaries")
        for what, missing in r["gaps"].items():
            mark = "OK " if not missing else "GAP"
            print(f"  [{mark}] {what}: {', '.join(missing) if missing else 'nothing missing'}")
        print(f"written to {args.out}")
        sys.exit(1 if any(r["gaps"].values()) else 0)

    same, differ, failed, rows = replay(args.requests, args.a, args.b, args.out, args.scores, args.strict)
    total = same + differ + failed
    print(f"{same}/{total} answers identical, {differ} differ, {failed} could not be asked")
    for row in rows[:10]:
        print(f"\n  {row['method']} {row['path']}  status {row['status']}")
        for line in row["diff"][:12]:
            print(f"    {line.rstrip()}")
    print(f"\nwritten to {args.out}")
    sys.exit(1 if differ or failed else 0)


main()
