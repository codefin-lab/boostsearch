#!/usr/bin/env python3
"""The shape of an answer here, beside the shape of the same answer there.

Some differences are not about which documents come back but about how the
answer is written: a whole number where OpenSearch writes a whole number, a
field that is present rather than absent, a default that differs. This walks a
set of requests against both servers and reports where the two answers differ
in structure -- the keys they hold and the kinds of value under them -- rather
than in the values themselves.
"""
import json, sys, urllib.request, urllib.error

import os
A = ("OpenSearch", os.environ.get("DIFF_A", "http://127.0.0.1:9299"))
B = ("BoostSearch", os.environ.get("DIFF_B", "http://127.0.0.1:9200"))
INDEX = "shape"

MAPPING = {
    "settings": {"number_of_shards": 1, "number_of_replicas": 0},
    "mappings": {"properties": {
        "title": {"type": "text"},
        "tag": {"type": "keyword", "store": True},
        "n": {"type": "long"},
        "price": {"type": "scaled_float", "scaling_factor": 100},
        "when": {"type": "date"},
    }},
}
DOCS = [
    {"title": "quick brown fox", "tag": "a", "n": 1, "price": 9.5, "when": "2021-01-01"},
    {"title": "lazy dog", "tag": "b", "n": 2, "price": 19.5, "when": "2021-02-01"},
]

REQUESTS = [
    ("GET", f"/{INDEX}/_settings", None),
    ("GET", f"/{INDEX}/_settings?flat_settings=true", None),
    ("GET", f"/{INDEX}/_mapping", None),
    ("GET", f"/{INDEX}/_field_caps?fields=*", None),
    ("GET", f"/{INDEX}/_doc/1", None),
    ("GET", f"/{INDEX}/_source/1", None),
    ("GET", f"/{INDEX}/_termvectors/1?fields=title", None),
    ("POST", f"/{INDEX}/_termvectors/1", {"fields": ["title"], "term_statistics": True,
                                          "field_statistics": True}),
    ("GET", f"/{INDEX}/_count", None),
    ("GET", f"/{INDEX}/_stats", None),
    ("GET", f"/{INDEX}/_search?size=1", None),
    ("POST", f"/{INDEX}/_search", {"size": 1, "explain": True, "query": {"term": {"tag": "a"}}}),
    ("POST", f"/{INDEX}/_search", {"size": 1, "version": True, "seq_no_primary_term": True}),
    ("POST", f"/{INDEX}/_search", {"size": 0, "aggs": {"a": {"terms": {"field": "tag"}}}}),
    ("POST", f"/{INDEX}/_search", {"size": 1, "docvalue_fields": ["n", "when"]}),
    ("POST", f"/{INDEX}/_search", {"size": 1, "stored_fields": ["tag"]}),
    ("POST", f"/{INDEX}/_search", {"size": 1, "fields": ["title", "n"], "_source": False}),
    ("POST", f"/{INDEX}/_msearch", None),   # filled in below
    ("GET", "/_cluster/health", None),
    ("GET", f"/_cluster/health/{INDEX}", None),
    ("GET", "/_cluster/state/metadata", None),
    ("GET", "/_cat/indices?format=json", None),
    ("GET", "/_cat/shards?format=json", None),
    ("GET", "/_nodes/stats/indices?filter_path=nodes.*.indices.docs", None),
    ("GET", f"/{INDEX}/_alias", None),
    ("GET", f"/{INDEX}/_mapping/field/n", None),
    ("POST", f"/{INDEX}/_validate/query?explain=true", {"query": {"match_all": {}}}),
    ("POST", f"/{INDEX}/_analyze", {"text": "quick brown", "analyzer": "standard"}),
    ("GET", f"/{INDEX}/_explain/1?q=tag:a", None),
    ("GET", "/_tasks?actions=*search*", None),
]


def call(base, method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(base + path, data=data, method=method,
                                 headers={"content-type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=60) as answer:
            return json.load(answer)
    except urllib.error.HTTPError as e:
        try:
            return {"__status": e.code, **json.load(e)}
        except Exception:
            return {"__status": e.code}
    except Exception as e:
        return {"__error": str(e)}


def load(base):
    call(base, "DELETE", "/" + INDEX)
    call(base, "PUT", "/" + INDEX, MAPPING)
    lines = []
    for i, doc in enumerate(DOCS, start=1):
        lines.append(json.dumps({"index": {"_index": INDEX, "_id": str(i)}}))
        lines.append(json.dumps(doc))
    data = ("\n".join(lines) + "\n").encode()
    req = urllib.request.Request(base + "/_bulk", data=data, method="POST",
                                 headers={"content-type": "application/json"})
    urllib.request.urlopen(req, timeout=60).read()
    call(base, "POST", f"/{INDEX}/_refresh")


# what a value is, without saying what it holds: the shape alone
VOLATILE = {
    "took", "_seq_no", "_primary_term", "uuid", "index_uuid", "creation_date", "version",
    "settings_version", "aliases_version", "mappings_version", "routing_num_shards", "id",
    "start_time_in_millis", "running_time_in_nanos", "timestamp", "size_in_bytes",
    "memory_size_in_bytes", "total_time_in_millis", "time_in_millis", "millis", "node",
    "cluster_uuid", "max_score", "_score", "value", "_version", "epoch", "timeout", "ttl",
    "creation_date_string", "provided_name", "store", "docs", "seq_no", "primary_term",
}


def shape(value, key=""):
    if key in VOLATILE:
        return "~"
    if isinstance(value, dict):
        # a map keyed by node holds one entry per node, named after it
        if key == "nodes" and value:
            return {"<node>": shape(next(iter(value.values())), "<node>")}
        return {k: shape(v, k) for k, v in sorted(value.items())}
    if isinstance(value, list):
        # how many there are is not the shape; what one of them looks like is
        return [shape(value[0], key)] if value else []
    if isinstance(value, bool):
        return "bool"
    if isinstance(value, int):
        return "int"
    if isinstance(value, float):
        return "float" if value != int(value) else "int-like-float"
    if value is None:
        return "null"
    return "str"


def main():
    show = "-v" in sys.argv
    for _, base in (A, B):
        load(base)
    same, differences = 0, []
    for method, path, body in REQUESTS:
        if path.endswith("_msearch"):
            continue
        theirs = shape(call(A[1], method, path, body))
        ours = shape(call(B[1], method, path, body))
        if theirs == ours:
            same += 1
        else:
            differences.append((f"{method} {path}", theirs, ours))
    if show:
        for label, theirs, ours in differences:
            print(f"\n{label}")
            for line in diff_lines(theirs, ours):
                print(f"    {line}")
    print(f"\n{same} of {len(REQUESTS) - 1} identical  "
          f"({100 * same / (len(REQUESTS) - 1):.1f}%)")
    return 0 if not differences else 1


def diff_lines(theirs, ours, at=""):
    """Where two shapes part company, named by the path that gets there."""
    out = []
    if isinstance(theirs, dict) and isinstance(ours, dict):
        for key in sorted(set(theirs) | set(ours)):
            if key not in ours:
                out.append(f"{at}.{key}: missing here (theirs {json.dumps(theirs[key])[:60]})")
            elif key not in theirs:
                out.append(f"{at}.{key}: only here ({json.dumps(ours[key])[:60]})")
            else:
                out.extend(diff_lines(theirs[key], ours[key], f"{at}.{key}"))
    elif isinstance(theirs, list) and isinstance(ours, list):
        if not theirs or not ours:
            if theirs != ours:
                out.append(f"{at}: {json.dumps(theirs)[:50]} vs {json.dumps(ours)[:50]}")
        else:
            out.extend(diff_lines(theirs[0], ours[0], f"{at}[]"))
    elif theirs != ours:
        out.append(f"{at}: {theirs} vs {ours}")
    return out[:12]

sys.exit(main())
