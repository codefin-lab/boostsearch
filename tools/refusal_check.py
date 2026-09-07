#!/usr/bin/env python3
"""A write that is refused leaves what was there alone.

The second review found a write that queued the delete of the document it was
replacing before the validation that might refuse it: a refused write destroyed
the document it failed to replace. Nothing in the suites noticed, because they
check what a request answers and not what the index holds afterwards.

So this writes a document, sends a request that must be refused, and reads the
document back. Every way a write can be refused, through every path that writes:
`_doc`, `_bulk`, `_update`, `_update_by_query` and `_reindex`. A document that is
gone, or changed, after a refusal is a failure -- and so is a refusal that was
answered as a success.

    tools/refusal_check.py            # starts a node of its own
    tools/refusal_check.py --url http://127.0.0.1:9200
"""

import argparse
import json
import os
import pathlib
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent
KEPT = {"n": 1, "s": "keep me", "v": [1.0, 2.0, 3.0]}


def call(url, method, path, body=None, ndjson=None, timeout=30):
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
            return e.code, {"raw": raw[:200].decode("utf-8", "replace")}
    except Exception as e:
        return 0, {"no answer": str(e)[:160]}


def start_node(binary, port, transport):
    data = tempfile.mkdtemp(prefix="boost-refusal-")
    env = dict(os.environ)
    env.update(
        {
            "BOOSTSEARCH_ADDR": f"127.0.0.1:{port}",
            "BOOSTSEARCH_DATA": data,
            "BOOSTSEARCH_TRANSPORT_PORT": str(transport),
        }
    )
    log = open(pathlib.Path(data) / "node.log", "w")
    node = subprocess.Popen([binary], env=env, stdout=log, stderr=subprocess.STDOUT)
    url = f"http://127.0.0.1:{port}"
    for _ in range(60):
        status, _ = call(url, "GET", "/")
        if status:
            time.sleep(1)  # the node elects itself a moment after it listens
            return node, data, url
        if node.poll() is not None:
            break
        time.sleep(1)
    node.kill()
    print(f"the node did not start; its log is in {data}/node.log")
    sys.exit(2)


def fresh(url, index, mapping=None, settings=None):
    """An index holding one document, ready to be written over."""
    call(url, "DELETE", f"/{index}")
    body = {}
    if mapping:
        body["mappings"] = {"properties": mapping}
    if settings:
        body["settings"] = settings
    call(url, "PUT", f"/{index}", body)
    call(url, "PUT", f"/{index}/_doc/1?refresh=true", KEPT)


def held(url, index):
    """The document as the index holds it now."""
    call(url, "POST", f"/{index}/_refresh")
    status, answer = call(url, "GET", f"/{index}/_doc/1")
    if status != 200 or not answer.get("found"):
        return None
    return answer.get("_source")


# every refusal worth checking: a name, the mapping the index needs, and the
# request that must be refused, through each path that writes
def cases():
    mapping = {"n": {"type": "integer"}, "s": {"type": "keyword"}}
    vectors = {
        "n": {"type": "integer"},
        "s": {"type": "keyword"},
        "v": {"type": "knn_vector", "dimension": 3},
    }
    bad_value = {"n": "not a number", "s": "x"}
    out_of_range = {"n": 99999999999, "s": "x"}
    wrong_vector = {"n": 1, "s": "x", "v": [1.0, 2.0]}
    object_for_scalar = {"n": {"deep": 1}, "s": "x"}
    yield ("a value the type will not take", mapping, None, bad_value)
    yield ("a number past the type's range", mapping, None, out_of_range)
    yield ("an object where a value belongs", mapping, None, object_for_scalar)
    yield ("a vector of the wrong width", vectors, None, wrong_vector)


def probe(url, index, name, mapping, settings, document, path):
    """One refusal, through one path. Returns a list of complaints."""
    fresh(url, index, mapping, settings)
    before = held(url, index)
    if before is None:
        return [f"{name} / {path}: the fixture document was not there to begin with"]
    if path == "_doc":
        status, answer = call(url, "PUT", f"/{index}/_doc/1", document)
        refused = status >= 400
    elif path == "_bulk":
        line = json.dumps({"index": {"_index": index, "_id": "1"}})
        status, answer = call(
            url, "POST", "/_bulk?refresh=true", ndjson=f"{line}\n{json.dumps(document)}\n"
        )
        refused = answer.get("errors") is True or status >= 400
    elif path == "_update":
        status, answer = call(url, "POST", f"/{index}/_update/1", {"doc": document})
        refused = status >= 400
    elif path == "_update_by_query":
        sets = "; ".join(
            f"ctx._source.{k} = params.v{i}" for i, k in enumerate(document)
        )
        params = {f"v{i}": v for i, v in enumerate(document.values())}
        status, answer = call(
            url,
            "POST",
            f"/{index}/_update_by_query?refresh=true",
            {"query": {"match_all": {}}, "script": {"source": sets, "params": params}},
        )
        refused = status >= 400 or answer.get("version_conflicts") or answer.get("failures")
    elif path == "_reindex":
        # a copy of a document the destination's mapping will not take
        call(url, "DELETE", "/refusal_src")
        call(url, "PUT", "/refusal_src", {})
        call(url, "PUT", "/refusal_src/_doc/1?refresh=true", document)
        status, answer = call(
            url,
            "POST",
            "/_reindex?refresh=true",
            {"source": {"index": "refusal_src"}, "dest": {"index": index}},
        )
        refused = status >= 400 or answer.get("failures")
    else:
        raise AssertionError(path)

    after = held(url, index)
    bad = []
    if after is None:
        bad.append(f"{name} / {path}: the document is GONE after a write that was refused")
    elif after != before:
        bad.append(f"{name} / {path}: the document CHANGED: {before} -> {after}")
    if not refused:
        bad.append(f"{name} / {path}: the write was accepted ({status}) and should not have been")
    return bad


def held_still(url):
    """The states an operator holds an index in: nothing may change them."""
    bad = []
    for name, settings, closed in [
        ("index.blocks.write", {"index.blocks.write": True}, False),
        ("index.blocks.read_only", {"index.blocks.read_only": True}, False),
        ("a closed index", None, True),
    ]:
        index = "refusal_held"
        fresh(url, index)
        before = held(url, index)
        if settings:
            call(url, "PUT", f"/{index}/_settings", settings)
        if closed:
            call(url, "POST", f"/{index}/_close")
        for what, method, path, body in [
            ("a write", "PUT", f"/{index}/_doc/2", {"n": 2}),
            ("a delete", "DELETE", f"/{index}/_doc/1", None),
            ("a delete by query", "POST", f"/{index}/_delete_by_query", {"query": {"match_all": {}}}),
        ]:
            status, _ = call(url, method, path, body)
            if status < 400:
                bad.append(f"{name}: {what} was accepted ({status})")
        if closed:
            call(url, "POST", f"/{index}/_open")
        else:
            call(
                url,
                "PUT",
                f"/{index}/_settings",
                {"index.blocks.write": False, "index.blocks.read_only": False},
            )
        after = held(url, index)
        if after != before:
            bad.append(f"{name}: the document changed while the index was held: {before} -> {after}")
    return bad


def truncated_bulk(url):
    """A bulk whose last action has no document is a request that was cut."""
    index = "refusal_cut"
    fresh(url, index)
    before = held(url, index)
    line = json.dumps({"index": {"_index": index, "_id": "1"}})
    status, answer = call(url, "POST", "/_bulk?refresh=true", ndjson=f"{line}\n")
    after = held(url, index)
    bad = []
    if status < 400 and answer.get("errors") is not True:
        bad.append(f"a bulk with no document line was accepted ({status}): {answer}")
    if after != before:
        bad.append(f"a bulk with no document line changed the document: {before} -> {after}")
    return bad


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default="")
    ap.add_argument("--binary", default=str(ROOT / "target" / "release" / "boostsearch"))
    ap.add_argument("--port", type=int, default=9268)
    ap.add_argument("--transport", type=int, default=9368)
    args = ap.parse_args()

    node = data = None
    if args.url:
        url = args.url
    else:
        node, data, url = start_node(args.binary, args.port, args.transport)
    try:
        bad = []
        checked = 0
        for name, mapping, settings, document in cases():
            for path in ["_doc", "_bulk", "_update", "_update_by_query", "_reindex"]:
                bad += probe(url, "refusal", name, mapping, settings, document, path)
                checked += 1
        bad += held_still(url)
        bad += truncated_bulk(url)
        checked += 10
        print(f"  {checked} refusals checked through the paths that write")
        for row in bad:
            print(f"    {row}")
        if bad:
            print("\nRESULT a refused write did not leave the index as it found it")
            return 1
        print("\nRESULT every refused write left the document it could not replace")
        return 0
    finally:
        if node is not None:
            node.send_signal(signal.SIGTERM)
            try:
                node.wait(timeout=10)
            except subprocess.TimeoutExpired:
                node.kill()
            shutil.rmtree(data, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
