#!/usr/bin/env python3
"""What OpenSearch answers a query with, and what BoostSearch answers.

Indexes the same small set of documents into both servers, then runs every
query and aggregation shape the corpus names against both and reports where
the answers differ -- the documents returned and their order, the numbers over
them, and the shape of the answer itself. This is the gate for the query and
aggregation phase: the diff is meant to be empty.
"""
import json, sys, urllib.request, urllib.error

import os
A = ("OpenSearch", os.environ.get("DIFF_A", "http://127.0.0.1:9299"))
B = ("BoostSearch", os.environ.get("DIFF_B", "http://127.0.0.1:9200"))
INDEX = "diff"

MAPPING = {
    "settings": {"number_of_shards": 1, "number_of_replicas": 0},
    "mappings": {"properties": {
        "title": {"type": "text"},
        "body": {"type": "text"},
        "tag": {"type": "keyword"},
        "n": {"type": "long"},
        "price": {"type": "double"},
        "when": {"type": "date"},
        "here": {"type": "geo_point"},
        "ip": {"type": "ip"},
        "flag": {"type": "boolean"},
        "nested": {"type": "nested", "properties": {"name": {"type": "keyword"},
                                                    "size": {"type": "long"}}},
    }},
}

DOCS = [
    {"title": "quick brown fox", "body": "the fox jumps over the lazy dog", "tag": "a",
     "n": 1, "price": 9.5, "when": "2021-01-01", "here": {"lat": 52.4, "lon": 4.9},
     "ip": "10.0.0.1", "flag": True, "nested": [{"name": "x", "size": 1}]},
    {"title": "lazy dog sleeps", "body": "a dog and a fox", "tag": "b",
     "n": 2, "price": 19.5, "when": "2021-02-01", "here": {"lat": 48.8, "lon": 2.3},
     "ip": "10.0.0.2", "flag": False, "nested": [{"name": "y", "size": 2}]},
    {"title": "brown bear", "body": "bears are not foxes", "tag": "a",
     "n": 3, "price": 29.5, "when": "2021-03-01", "here": {"lat": 51.5, "lon": -0.1},
     "ip": "192.168.0.1", "flag": True, "nested": [{"name": "x", "size": 3}]},
    {"title": "the quick fox", "body": "quick quick fox", "tag": "c",
     "n": 4, "price": 39.5, "when": "2021-04-01", "here": {"lat": 40.7, "lon": -74.0},
     "ip": "192.168.0.2", "flag": False, "nested": [{"name": "z", "size": 4}]},
]

QUERIES = {
    "match": {"query": {"match": {"title": "quick fox"}}},
    "match_and": {"query": {"match": {"title": {"query": "quick fox", "operator": "and"}}}},
    "match_phrase": {"query": {"match_phrase": {"body": "lazy dog"}}},
    "match_phrase_prefix": {"query": {"match_phrase_prefix": {"body": "lazy d"}}},
    "match_bool_prefix": {"query": {"match_bool_prefix": {"title": "qui fo"}}},
    "multi_match": {"query": {"multi_match": {"query": "fox", "fields": ["title", "body"]}}},
    "multi_match_phrase": {"query": {"multi_match": {"query": "quick fox", "fields": ["title^2", "body"], "type": "phrase"}}},
    "term": {"query": {"term": {"tag": "a"}}},
    "terms": {"query": {"terms": {"tag": ["a", "c"]}}},
    "range_n": {"query": {"range": {"n": {"gte": 2, "lt": 4}}}},
    "range_date": {"query": {"range": {"when": {"gte": "2021-02-01", "lte": "2021-03-31"}}}},
    "exists": {"query": {"exists": {"field": "tag"}}},
    "prefix": {"query": {"prefix": {"tag": "a"}}},
    "wildcard": {"query": {"wildcard": {"tag": "?"}}},
    "regexp": {"query": {"regexp": {"tag": "[ab]"}}},
    "fuzzy": {"query": {"fuzzy": {"tag": {"value": "ab", "fuzziness": 1}}}},
    "ids": {"query": {"ids": {"values": ["1", "3"]}}},
    "bool_must": {"query": {"bool": {"must": [{"match": {"title": "fox"}}], "filter": [{"term": {"tag": "a"}}]}}},
    "bool_should_min": {"query": {"bool": {"should": [{"term": {"tag": "a"}}, {"term": {"tag": "b"}}], "minimum_should_match": 1}}},
    "bool_must_not": {"query": {"bool": {"must_not": [{"term": {"tag": "a"}}]}}},
    "boosting": {"query": {"boosting": {"positive": {"match": {"title": "fox"}}, "negative": {"term": {"tag": "c"}}, "negative_boost": 0.2}}},
    "constant_score": {"query": {"constant_score": {"filter": {"term": {"tag": "a"}}, "boost": 2}}},
    "dis_max": {"query": {"dis_max": {"queries": [{"match": {"title": "fox"}}, {"match": {"body": "fox"}}]}}},
    "function_score_weight": {"query": {"function_score": {"query": {"match_all": {}}, "functions": [{"filter": {"term": {"tag": "a"}}, "weight": 3}]}}},
    "function_score_field": {"query": {"function_score": {"query": {"match_all": {}}, "field_value_factor": {"field": "n", "factor": 1.5}}}},
    "match_all_size": {"query": {"match_all": {}}, "size": 2},
    "match_none": {"query": {"match_none": {}}},
    "query_string": {"query": {"query_string": {"query": "title:fox AND tag:a"}}},
    "simple_query_string": {"query": {"simple_query_string": {"query": "fox +brown", "fields": ["title"]}}},
    "nested": {"query": {"nested": {"path": "nested", "query": {"term": {"nested.name": "x"}}}}},
    "geo_distance": {"query": {"geo_distance": {"distance": "500km", "here": {"lat": 52.0, "lon": 4.0}}}},
    "geo_bounding_box": {"query": {"geo_bounding_box": {"here": {"top_left": {"lat": 53.0, "lon": -1.0}, "bottom_right": {"lat": 48.0, "lon": 5.0}}}}},
    "ip_range": {"query": {"range": {"ip": {"gte": "10.0.0.0", "lte": "10.255.255.255"}}}},
    "more_like_this": {"query": {"more_like_this": {"fields": ["body"], "like": "fox dog", "min_term_freq": 1, "min_doc_freq": 1}}},
    "span_term": {"query": {"span_term": {"body": "fox"}}},
    "span_near": {"query": {"span_near": {"clauses": [{"span_term": {"body": "lazy"}}, {"span_term": {"body": "dog"}}], "slop": 1, "in_order": True}}},
    "span_first": {"query": {"span_first": {"match": {"span_term": {"body": "the"}}, "end": 2}}},
    "span_or": {"query": {"span_or": {"clauses": [{"span_term": {"body": "fox"}}, {"span_term": {"body": "bears"}}]}}},
    "span_not": {"query": {"span_not": {"include": {"span_term": {"body": "fox"}}, "exclude": {"span_term": {"body": "dog"}}}}},
    "sort_desc": {"query": {"match_all": {}}, "sort": [{"n": "desc"}]},
    "sort_multi": {"query": {"match_all": {}}, "sort": [{"tag": "asc"}, {"n": "desc"}]},
    "from_size": {"query": {"match_all": {}}, "sort": [{"n": "asc"}], "from": 1, "size": 2},
    "source_filter": {"query": {"match_all": {}}, "_source": ["title", "n"], "size": 1, "sort": [{"n": "asc"}]},
    "highlight": {"query": {"match": {"body": "fox"}}, "highlight": {"fields": {"body": {}}}},
    "collapse": {"query": {"match_all": {}}, "collapse": {"field": "tag"}, "sort": [{"n": "asc"}]},
    "min_score": {"query": {"match": {"title": "fox"}}, "min_score": 0.5},
    "track_total_hits": {"query": {"match_all": {}}, "track_total_hits": True},
    "post_filter": {"query": {"match_all": {}}, "post_filter": {"term": {"tag": "a"}}},
    "explain": {"query": {"term": {"tag": "a"}}, "explain": False},
}

AGGS = {
    "terms": {"aggs": {"a": {"terms": {"field": "tag"}}}},
    "terms_order": {"aggs": {"a": {"terms": {"field": "tag", "order": {"_key": "desc"}}}}},
    "terms_size": {"aggs": {"a": {"terms": {"field": "tag", "size": 1}}}},
    "stats": {"aggs": {"a": {"stats": {"field": "n"}}}},
    "extended_stats": {"aggs": {"a": {"extended_stats": {"field": "n"}}}},
    "avg": {"aggs": {"a": {"avg": {"field": "price"}}}},
    "sum": {"aggs": {"a": {"sum": {"field": "price"}}}},
    "min_max": {"aggs": {"a": {"min": {"field": "n"}}, "b": {"max": {"field": "n"}}}},
    "value_count": {"aggs": {"a": {"value_count": {"field": "tag"}}}},
    "cardinality": {"aggs": {"a": {"cardinality": {"field": "tag"}}}},
    "percentiles": {"aggs": {"a": {"percentiles": {"field": "n", "percents": [50, 95]}}}},
    "percentile_ranks": {"aggs": {"a": {"percentile_ranks": {"field": "n", "values": [2]}}}},
    "histogram": {"aggs": {"a": {"histogram": {"field": "n", "interval": 2}}}},
    "date_histogram": {"aggs": {"a": {"date_histogram": {"field": "when", "calendar_interval": "month"}}}},
    "date_range": {"aggs": {"a": {"date_range": {"field": "when", "ranges": [{"to": "2021-02-15"}, {"from": "2021-02-15"}]}}}},
    "range": {"aggs": {"a": {"range": {"field": "n", "ranges": [{"to": 2}, {"from": 2, "to": 4}, {"from": 4}]}}}},
    "filter": {"aggs": {"a": {"filter": {"term": {"tag": "a"}}}}},
    "filters": {"aggs": {"a": {"filters": {"filters": {"one": {"term": {"tag": "a"}}, "two": {"term": {"tag": "b"}}}}}}},
    "nested_agg": {"aggs": {"a": {"nested": {"path": "nested"}, "aggs": {"b": {"sum": {"field": "nested.size"}}}}}},
    "missing": {"aggs": {"a": {"missing": {"field": "nothing"}}}},
    "global": {"aggs": {"a": {"global": {}, "aggs": {"b": {"value_count": {"field": "tag"}}}}}},
    "top_hits": {"aggs": {"a": {"terms": {"field": "tag"}, "aggs": {"b": {"top_hits": {"size": 1, "_source": ["n"]}}}}}},
    "sub_agg": {"aggs": {"a": {"terms": {"field": "tag"}, "aggs": {"b": {"avg": {"field": "n"}}}}}},
    "bucket_script": {"aggs": {"a": {"terms": {"field": "tag"}, "aggs": {"s": {"sum": {"field": "n"}}}}}},
    "composite": {"aggs": {"a": {"composite": {"sources": [{"t": {"terms": {"field": "tag"}}}], "size": 2}}}},
    "significant_terms": {"aggs": {"a": {"significant_terms": {"field": "tag"}}}},
    "rare_terms": {"aggs": {"a": {"rare_terms": {"field": "tag", "max_doc_count": 1}}}},
    "multi_terms": {"aggs": {"a": {"multi_terms": {"terms": [{"field": "tag"}, {"field": "flag"}]}}}},
    "auto_date_histogram": {"aggs": {"a": {"auto_date_histogram": {"field": "when", "buckets": 2}}}},
    "geo_bounds": {"aggs": {"a": {"geo_bounds": {"field": "here"}}}},
    "geo_centroid": {"aggs": {"a": {"geo_centroid": {"field": "here"}}}},
    "geo_distance_agg": {"aggs": {"a": {"geo_distance": {"field": "here", "origin": {"lat": 52.0, "lon": 4.0}, "ranges": [{"to": 500000}, {"from": 500000}]}}}},
    "matrix_stats": {"aggs": {"a": {"matrix_stats": {"fields": ["n", "price"]}}}},
    "sampler": {"aggs": {"a": {"sampler": {"shard_size": 2}, "aggs": {"b": {"terms": {"field": "tag"}}}}}},
    "diversified_sampler": {"aggs": {"a": {"diversified_sampler": {"field": "tag", "shard_size": 2}, "aggs": {"b": {"value_count": {"field": "n"}}}}}},
    "derivative": {"aggs": {"a": {"histogram": {"field": "n", "interval": 1}, "aggs": {"s": {"sum": {"field": "price"}}}}, "d": {"derivative": {"buckets_path": "a>s"}}}},
    "cumulative_sum": {"aggs": {"a": {"histogram": {"field": "n", "interval": 1}, "aggs": {"s": {"sum": {"field": "price"}}, "c": {"cumulative_sum": {"buckets_path": "s"}}}}}},
    "max_bucket": {"aggs": {"a": {"terms": {"field": "tag"}, "aggs": {"s": {"sum": {"field": "n"}}}}, "m": {"max_bucket": {"buckets_path": "a>s"}}}},
    "stats_bucket": {"aggs": {"a": {"terms": {"field": "tag"}, "aggs": {"s": {"sum": {"field": "n"}}}}, "m": {"stats_bucket": {"buckets_path": "a>s"}}}},
    "bucket_selector": {"aggs": {"a": {"terms": {"field": "tag"}, "aggs": {"s": {"sum": {"field": "n"}}}}}},
    "bucket_sort": {"aggs": {"a": {"terms": {"field": "tag"}, "aggs": {"s": {"sum": {"field": "n"}}, "sort": {"bucket_sort": {"sort": [{"s": "desc"}]}}}}}},
    "ip_range_agg": {"aggs": {"a": {"ip_range": {"field": "ip", "ranges": [{"to": "10.255.255.255"}, {"from": "192.168.0.0"}]}}}},
    "adjacency_matrix": {"aggs": {"a": {"adjacency_matrix": {"filters": {"one": {"term": {"tag": "a"}}, "two": {"term": {"flag": True}}}}}}},
}


def call(base, path, body=None, method="POST"):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(base + path, data=data, method=method,
                                 headers={"content-type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=60) as answer:
            return json.load(answer)
    except urllib.error.HTTPError as e:
        try:
            return json.load(e)
        except Exception:
            return {"error": {"status": e.code}}
    except Exception as e:
        return {"error": {"reason": str(e)}}


def load(base):
    call(base, "/" + INDEX, method="DELETE")
    call(base, "/" + INDEX, MAPPING, method="PUT")
    lines = []
    for i, doc in enumerate(DOCS, start=1):
        lines.append(json.dumps({"index": {"_index": INDEX, "_id": str(i)}}))
        lines.append(json.dumps(doc))
    data = ("\n".join(lines) + "\n").encode()
    req = urllib.request.Request(base + "/_bulk", data=data, method="POST",
                                 headers={"content-type": "application/json"})
    urllib.request.urlopen(req, timeout=60).read()
    call(base, f"/{INDEX}/_refresh", method="POST")


def shape(answer, with_aggs):
    """What is compared: the documents in order, and the aggregation numbers."""
    if "error" in answer:
        error = answer["error"]
        return {"error": error.get("type") if isinstance(error, dict) else str(error)}
    hits = answer.get("hits", {})
    total = hits.get("total")
    if isinstance(total, dict):
        total = total.get("value")
    out = {"total": total, "ids": [h.get("_id") for h in hits.get("hits", [])]}
    if with_aggs:
        out["aggs"] = rounded(answer.get("aggregations"))
    return out


def rounded(value):
    """Numbers as they compare: a float that differs in the last bit is equal."""
    if isinstance(value, float):
        return round(value, 4)
    if isinstance(value, dict):
        return {k: rounded(v) for k, v in value.items() if k not in ("bg_count", "doc_count_error_upper_bound", "sum_other_doc_count")}
    if isinstance(value, list):
        return [rounded(v) for v in value]
    return value


def main():
    show = "-v" in sys.argv
    for _, base in (A, B):
        load(base)
    same = 0
    differences = []
    cases = [(f"query:{name}", body, False) for name, body in QUERIES.items()]
    cases += [(f"agg:{name}", dict(body, size=0), True) for name, body in AGGS.items()]
    for label, body, with_aggs in cases:
        theirs = shape(call(A[1], f"/{INDEX}/_search", body), with_aggs)
        ours = shape(call(B[1], f"/{INDEX}/_search", body), with_aggs)
        if theirs == ours:
            same += 1
        else:
            differences.append((label, theirs, ours))
    if show:
        for label, theirs, ours in differences:
            print(f"{label}\n    OpenSearch  {json.dumps(theirs)[:220]}\n"
                  f"    BoostSearch {json.dumps(ours)[:220]}")
    print(f"\n{same} of {len(cases)} identical  ({100 * same / len(cases):.1f}%)")
    return 0 if not differences else 1

sys.exit(main())
