#!/usr/bin/env python3
"""Vector search, end to end.

OpenSearch keeps k-NN in a plugin with its own repository and its own suite,
which is not part of the corpus this repository runs. This is what stands in
for it. It checks the things that can be quietly wrong: that the nearest
document really is the nearest, that a filter narrows before the distances are
compared rather than after, that a vector keeps the order it was written in,
and that all of it survives the node being restarted.
"""
import json
import math
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request

NODE = os.environ.get("BOOST_URL", "http://127.0.0.1:9213")
failures = []


def req(method, path, body=None):
    request = urllib.request.Request(
        NODE + path,
        method=method,
        data=json.dumps(body).encode() if body is not None else None,
        headers={"content-type": "application/json"},
    )
    try:
        with urllib.request.urlopen(request) as response:
            return json.loads(response.read() or b"{}")
    except urllib.error.HTTPError as e:
        return {"error": e.code, "body": e.read()[:400].decode()}
    except urllib.error.URLError as e:
        # a node that is not answering yet, which is what a restart looks like
        return {"error": 0, "body": str(e)}


def expect(what, got, want):
    if got != want:
        failures.append(f"{what}: {got!r}, expected {want!r}")


def ids_of(answer):
    return [h["_id"] for h in answer.get("hits", {}).get("hits", [])]


def setup(index, dimension, space="l2", docs=()):
    req("DELETE", f"/{index}")
    req(
        "PUT",
        f"/{index}",
        {
            "settings": {"index": {"knn": True}},
            "mappings": {
                "properties": {
                    "v": {
                        "type": "knn_vector",
                        "dimension": dimension,
                        "method": {"name": "hnsw", "space_type": space, "engine": "faiss"},
                    },
                    "colour": {"type": "keyword"},
                }
            },
        },
    )
    for name, vector, colour in docs:
        req("POST", f"/{index}/_doc/{name}?refresh=true", {"v": vector, "colour": colour})


def nearest_is_nearest():
    setup(
        "knn-basic",
        3,
        docs=[
            ("exact", [1.0, 0.0, 0.0], "warm"),
            ("close", [0.9, 0.1, 0.0], "warm"),
            ("far", [0.0, 1.0, 0.0], "cool"),
            ("further", [0.0, 0.0, 1.0], "cool"),
        ],
    )
    found = req("POST", "/knn-basic/_search", {"query": {"knn": {"v": {"vector": [1.0, 0.0, 0.0], "k": 2}}}})
    expect("the two nearest, nearest first", ids_of(found), ["exact", "close"])
    expect(
        "the nearest scores highest",
        found["hits"]["hits"][0]["_score"] > found["hits"]["hits"][1]["_score"],
        True,
    )
    # k is a ceiling on how many come back
    expect(
        "k limits the answer",
        len(ids_of(req("POST", "/knn-basic/_search", {"query": {"knn": {"v": {"vector": [1.0, 0.0, 0.0], "k": 3}}}}))),
        3,
    )


def a_filter_narrows_first():
    """Asking for the two nearest cool ones must give two, not two minus the
    warm ones that were nearer."""
    found = req(
        "POST",
        "/knn-basic/_search",
        {
            "query": {
                "knn": {
                    "v": {
                        "vector": [1.0, 0.0, 0.0],
                        "k": 2,
                        "filter": {"term": {"colour": "cool"}},
                    }
                }
            }
        },
    )
    expect("two cool documents, not none", len(ids_of(found)), 2)
    expect("and they are the cool ones", sorted(ids_of(found)), ["far", "further"])


def a_radius_returns_everything_within_it():
    found = req(
        "POST",
        "/knn-basic/_search",
        {"query": {"knn": {"v": {"vector": [1.0, 0.0, 0.0], "max_distance": 0.05}}}},
    )
    expect("everything within the radius", sorted(ids_of(found)), ["close", "exact"])
    tighter = req(
        "POST",
        "/knn-basic/_search",
        {"query": {"knn": {"v": {"vector": [1.0, 0.0, 0.0], "max_distance": 0.0001}}}},
    )
    expect("a tighter radius holds only the exact one", ids_of(tighter), ["exact"])


def spaces_measure_differently():
    """Cosine ignores how long a vector is; l2 does not."""
    setup(
        "knn-cosine",
        2,
        space="cosinesimil",
        docs=[("same-direction", [10.0, 0.0], "x"), ("other-direction", [0.0, 10.0], "x")],
    )
    found = req(
        "POST", "/knn-cosine/_search", {"query": {"knn": {"v": {"vector": [1.0, 0.0], "k": 2}}}}
    )
    expect("a long vector pointing the same way is nearest", ids_of(found)[0], "same-direction")
    # under cosine a vector ten times as long is the same direction, so it
    # scores as an exact match would
    expect(
        "and it is as near as a vector can be",
        abs(found["hits"]["hits"][0]["_score"] - 1.0) < 0.0001,
        True,
    )


def a_vector_keeps_its_order():
    """The order of a vector is its meaning: [1, 0] and [0, 1] are opposite
    corners, not the same set of numbers."""
    setup("knn-order", 2, docs=[("one-zero", [1.0, 0.0], "x"), ("zero-one", [0.0, 1.0], "x")])
    found = req(
        "POST",
        "/knn-order/_search",
        {
            "query": {
                "script_score": {
                    "query": {"match_all": {}},
                    "script": {
                        "source": "cosineSimilarity(params.q, doc['v']) + 10",
                        "params": {"q": [1.0, 0.0]},
                    },
                }
            }
        },
    )
    scores = {h["_id"]: h["_score"] - 10 for h in found.get("hits", {}).get("hits", [])}
    expect("a vector matching exactly has similarity one", round(scores.get("one-zero", 0), 4), 1.0)
    expect("one at right angles has none", round(scores.get("zero-one", 9), 4), 0.0)


def scripts_can_measure_distance():
    setup("knn-script", 2, docs=[("a", [1.0, 0.0], "x"), ("b", [0.0, 1.0], "x")])
    for name, source, wanted in [
        ("l2Squared", "l2Squared(params.q, doc['v'])", {"a": 0.0, "b": 2.0}),
        ("l1Norm", "l1Norm(params.q, doc['v'])", {"a": 0.0, "b": 2.0}),
        ("innerProduct", "innerProduct(params.q, doc['v'])", {"a": 1.0, "b": 0.0}),
    ]:
        found = req(
            "POST",
            "/knn-script/_search",
            {
                "query": {
                    "script_score": {
                        "query": {"match_all": {}},
                        "script": {"source": f"{source} + 100", "params": {"q": [1.0, 0.0]}},
                    }
                }
            },
        )
        got = {h["_id"]: round(h["_score"] - 100, 4) for h in found.get("hits", {}).get("hits", [])}
        expect(f"{name} in a script", got, wanted)


def the_mapping_is_checked():
    req("DELETE", "/knn-bad")
    made = req(
        "PUT",
        "/knn-bad",
        {"mappings": {"properties": {"v": {"type": "knn_vector"}}}},
    )
    expect("a vector field with no dimension is refused", made.get("error"), 400)
    found = req(
        "POST", "/knn-basic/_search", {"query": {"knn": {"v": {"vector": [1.0, 0.0], "k": 1}}}}
    )
    expect("a query vector of the wrong length is refused", found.get("error"), 400)
    missing = req(
        "POST", "/knn-basic/_search", {"query": {"knn": {"colour": {"vector": [1.0], "k": 1}}}}
    )
    expect("a field that holds no vectors is refused", missing.get("error"), 400)


def vectors_outlive_the_node():
    """The table lives beside the index, and is worked out again from the
    documents when what was written down does not match them."""
    binary = os.environ.get("BOOST_BINARY")
    start = os.environ.get("BOOST_START")
    if not start:
        print("       (skipped the restart: set BOOST_START to the script that starts the node)")
        return
    before = ids_of(
        req("POST", "/knn-basic/_search", {"query": {"knn": {"v": {"vector": [1.0, 0.0, 0.0], "k": 2}}}})
    )
    subprocess.run(["pkill", "-9", "-f", binary or "release/boostsearch"], check=False)
    time.sleep(3)
    subprocess.Popen(
        [start],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        cwd=os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
    )
    for _ in range(30):
        time.sleep(1)
        if req("GET", "/_cluster/health").get("status"):
            break
    after = ids_of(
        req("POST", "/knn-basic/_search", {"query": {"knn": {"v": {"vector": [1.0, 0.0, 0.0], "k": 2}}}})
    )
    expect("the same answer after a restart", after, before)


if __name__ == "__main__":
    for name, check in [
        ("the nearest documents are the nearest ones", nearest_is_nearest),
        ("a filter narrows before the distances are compared", a_filter_narrows_first),
        ("a radius returns everything within it", a_radius_returns_everything_within_it),
        ("different spaces measure differently", spaces_measure_differently),
        ("a vector keeps the order it was written in", a_vector_keeps_its_order),
        ("a script can measure distance itself", scripts_can_measure_distance),
        ("the mapping and the query are checked", the_mapping_is_checked),
        ("vectors outlive the node", vectors_outlive_the_node),
    ]:
        before = len(failures)
        check()
        print(f"  {'ok    ' if len(failures) == before else 'FAILED'} {name}")
    for line in failures:
        print("   ", line)
    sys.exit(1 if failures else 0)
