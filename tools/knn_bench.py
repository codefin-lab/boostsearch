#!/usr/bin/env python3
"""What the graph buys, and where it starts buying it.

An exact search compares against every vector: always right, and linear. A
graph walks towards the query instead: approximate, and sublinear. Which is
faster depends entirely on how much there is, so this measures both at
several sizes rather than assuming the graph wins.

Recall is measured against the exact answer -- computed once, outside the
timing, because computing it in the loop measures Python.
"""
import json
import os
import random
import sys
import time
import urllib.request

NODE = os.environ.get("BOOST_URL", "http://127.0.0.1:9213")
DIMENSIONS = int(os.environ.get("BOOST_KNN_DIMS", "64"))
K = 10
QUERIES = 30


def req(method, path, body=None, content="application/json"):
    payload = body if isinstance(body, str) else (json.dumps(body) if body is not None else None)
    request = urllib.request.Request(
        NODE + path,
        method=method,
        data=payload.encode() if payload is not None else None,
        headers={"content-type": content},
    )
    with urllib.request.urlopen(request) as response:
        return json.loads(response.read() or b"{}")


def load(index, vectors):
    req("DELETE", f"/{index}") if False else None
    try:
        req("DELETE", f"/{index}")
    except Exception:
        pass
    req(
        "PUT",
        f"/{index}",
        {
            "settings": {"index": {"knn": True, "number_of_shards": 1}},
            "mappings": {
                "properties": {
                    "v": {
                        "type": "knn_vector",
                        "dimension": DIMENSIONS,
                        "method": {"name": "hnsw", "space_type": "l2"},
                    }
                }
            },
        },
    )
    lines = []
    started = time.time()
    for i, v in enumerate(vectors):
        lines.append(json.dumps({"index": {"_id": str(i)}}))
        lines.append(json.dumps({"v": v}))
        if len(lines) >= 4000:
            req("POST", f"/{index}/_bulk", "\n".join(lines) + "\n", "application/x-ndjson")
            lines = []
    if lines:
        req("POST", f"/{index}/_bulk", "\n".join(lines) + "\n", "application/x-ndjson")
    req("POST", f"/{index}/_refresh")
    return time.time() - started


def exact_top(vectors, query, k):
    scored = sorted(
        (sum((a - b) ** 2 for a, b in zip(query, v)), i) for i, v in enumerate(vectors)
    )
    return {str(i) for _, i in scored[:k]}


def timed(index, body, times=20):
    started = time.time()
    for _ in range(times):
        req("POST", f"/{index}/_search", body)
    return (time.time() - started) / times * 1000


if __name__ == "__main__":
    sizes = [int(s) for s in (sys.argv[1:] or ["1000", "10000", "50000"])]
    random.seed(7)
    print(f"{DIMENSIONS} dimensions, k={K}\n")
    print(f"{'vectors':>9} {'indexed':>9} {'graph':>9} {'exact':>9} {'recall':>8}")
    for n in sizes:
        vectors = [[random.gauss(0, 1) for _ in range(DIMENSIONS)] for _ in range(n)]
        index = f"knnbench{n}"
        seconds = load(index, vectors)
        queries = [[random.gauss(0, 1) for _ in range(DIMENSIONS)] for _ in range(QUERIES)]
        # the truth, worked out before anything is timed
        truth = [exact_top(vectors, q, K) for q in queries]
        hits = 0
        for q, want in zip(queries, truth):
            found = req(
                "POST", f"/{index}/_search", {"size": K, "query": {"knn": {"v": {"vector": q, "k": K}}}}
            )
            got = {h["_id"] for h in found["hits"]["hits"]}
            hits += len(got & want)
        graph_ms = timed(index, {"size": K, "query": {"knn": {"v": {"vector": queries[0], "k": K}}}})
        # a radial search compares against everything, which is the exact path
        exact_ms = timed(
            index, {"size": K, "query": {"knn": {"v": {"vector": queries[0], "max_distance": 1e9}}}}
        )
        print(
            f"{n:>9,} {seconds:>8.1f}s {graph_ms:>8.2f}ms {exact_ms:>8.2f}ms "
            f"{hits / (QUERIES * K):>8.3f}"
        )
        try:
            req("DELETE", f"/{index}")
        except Exception:
            pass
