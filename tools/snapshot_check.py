#!/usr/bin/env python3
"""Backup and restore, checked against what went in.

The production-readiness review of 2026-09-07 asked for a backup and restore
tested with data whose count and content can be compared, including a file
that is damaged or missing. This is that test: documents with known content
are indexed, a snapshot is taken into a filesystem repository, and it is
restored under another name and compared document by document -- count, ids
and a digest of every source. Then the snapshot's documents file is cut
short, has a line spoiled, and is taken away, and each restore must refuse
rather than bring back part of the index and call it success.

Run against a node whose `path.repo` holds the repository directory, which
is what `tools/gate_node.sh` arranges (`VELO_URL_REPO`, /tmp/velo-url-repo
by default):

    VELO_URL=http://127.0.0.1:9380 VELO_REPO_DIR=/tmp/velo-url-repo \\
        python3 tools/snapshot_check.py
"""

import hashlib
import json
import os
import shutil
import sys
import urllib.error
import urllib.request

NODE = os.environ.get("VELO_URL", "http://127.0.0.1:9213")
REPO_ROOT = os.environ.get("VELO_REPO_DIR", "/tmp/velo-url-repo")
DOCS = int(os.environ.get("VELO_SNAPSHOT_DOCS", "2500"))


def call(method, path, body=None, ndjson=False):
    data = None
    headers = {}
    if body is not None:
        data = body.encode() if isinstance(body, str) else json.dumps(body).encode()
        headers["content-type"] = "application/x-ndjson" if ndjson else "application/json"
    req = urllib.request.Request(NODE + path, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            raw = r.read()
            return r.status, json.loads(raw) if raw else {}
    except urllib.error.HTTPError as e:
        raw = e.read()
        try:
            return e.code, json.loads(raw)
        except ValueError:
            return e.code, {"raw": raw.decode(errors="replace")}


def contents(index):
    """Every document of an index: id -> digest of its source."""
    out = {}
    status, r = call("POST", f"/{index}/_search?scroll=1m", {"size": 1000, "sort": ["_doc"]})
    while status == 200:
        hits = r.get("hits", {}).get("hits", [])
        if not hits:
            break
        for h in hits:
            src = json.dumps(h["_source"], sort_keys=True)
            out[h["_id"]] = hashlib.sha256(src.encode()).hexdigest()
        status, r = call("POST", "/_search/scroll", {"scroll": "1m", "scroll_id": r.get("_scroll_id")})
    return out


def main():
    results = []

    def check(name, ok, detail=""):
        results.append(ok)
        print(f"  {'ok    ' if ok else 'FAILED'} {name}")
        if not ok and detail:
            print(f"    {detail}")

    repo_dir = os.path.join(REPO_ROOT, "snapshot-check")
    shutil.rmtree(repo_dir, ignore_errors=True)
    for name in ["snapcheck", "snapcheck-back", "snapcheck-short", "snapcheck-spoiled", "snapcheck-gone"]:
        call("DELETE", f"/{name}")
    call("DELETE", "/_snapshot/snapcheck-repo")

    # documents whose content says which they are, routed some of them, so a
    # restore that loses routing or a field is seen
    lines = []
    for i in range(DOCS):
        action = {"index": {"_index": "snapcheck", "_id": f"d{i}"}}
        if i % 7 == 0:
            action["index"]["routing"] = f"r{i % 3}"
        lines.append(json.dumps(action))
        lines.append(json.dumps({"n": i, "text": f"document {i} " + "x" * (i % 50), "tags": [i % 5, i % 11]}))
    status, r = call("POST", "/_bulk?refresh=true", "\n".join(lines) + "\n", ndjson=True)
    check("the documents are indexed", status == 200 and not r.get("errors"), str(r)[:300])
    before = contents("snapcheck")
    check("every document reads back before the snapshot", len(before) == DOCS, f"{len(before)} of {DOCS}")

    status, r = call("PUT", "/_snapshot/snapcheck-repo", {"type": "fs", "settings": {"location": repo_dir}})
    check("the repository is registered", status == 200, str(r)[:300])
    status, r = call("PUT", "/_snapshot/snapcheck-repo/snap1?wait_for_completion=true", {"indices": "snapcheck"})
    state = r.get("snapshot", {}).get("state")
    check("the snapshot is taken", status == 200 and state == "SUCCESS", str(r)[:300])

    status, r = call(
        "POST",
        "/_snapshot/snapcheck-repo/snap1/_restore?wait_for_completion=true",
        {"indices": "snapcheck", "rename_pattern": "snapcheck", "rename_replacement": "snapcheck-back"},
    )
    check("the snapshot restores under another name", status == 200, str(r)[:300])
    call("POST", "/snapcheck-back/_refresh")
    after = contents("snapcheck-back")
    missing = sorted(set(before) - set(after))[:5]
    extra = sorted(set(after) - set(before))[:5]
    changed = sorted(k for k in before if k in after and before[k] != after[k])[:5]
    check(
        "the restored index holds every document, unchanged",
        before == after,
        f"{len(after)} of {len(before)}; missing {missing}, extra {extra}, changed {changed}",
    )
    status, r = call("GET", "/snapcheck-back/_doc/d7?routing=r1")
    check("a routed document keeps its routing", status == 200 and r.get("_routing") == "r1", str(r)[:300])

    # the documents file of the index in the snapshot
    found = []
    for root, _dirs, files in os.walk(repo_dir):
        if "docs.ndjson" in files:
            found.append(os.path.join(root, "docs.ndjson"))
    check("the snapshot's documents file is where the repository keeps it", len(found) == 1, str(found))
    if len(found) != 1:
        return finish(results)
    docs_file = found[0]
    with open(docs_file, "rb") as f:
        good = f.read()

    def restore_refused(name, damage):
        damage()
        status, r = call(
            "POST",
            "/_snapshot/snapcheck-repo/snap1/_restore?wait_for_completion=true",
            {"indices": "snapcheck", "rename_pattern": "snapcheck", "rename_replacement": f"snapcheck-{name}"},
        )
        st, _ = call("GET", f"/snapcheck-{name}/_count")
        with open(docs_file, "wb") as f:
            f.write(good)
        return status >= 400 and st == 404, f"restore answered {status} {str(r)[:200]}; the index answers {st}"

    def cut_short():
        with open(docs_file, "wb") as f:
            f.write(good[: len(good) * 2 // 3])

    def spoil_a_line():
        lines = good.split(b"\n")
        lines[len(lines) // 2] = b'{"_id": "d-spoiled", "_source": "{not json'
        with open(docs_file, "wb") as f:
            f.write(b"\n".join(lines))

    def take_away():
        os.remove(docs_file)

    ok, detail = restore_refused("short", cut_short)
    check("a documents file cut short is refused, and no index is left", ok, detail)
    ok, detail = restore_refused("spoiled", spoil_a_line)
    check("a documents file with a spoiled line is refused, and no index is left", ok, detail)
    ok, detail = restore_refused("gone", take_away)
    check("a documents file taken away is refused, and no index is left", ok, detail)

    for name in ["snapcheck", "snapcheck-back", "snapcheck-short", "snapcheck-spoiled", "snapcheck-gone"]:
        call("DELETE", f"/{name}")
    call("DELETE", "/_snapshot/snapcheck-repo/snap1")
    call("DELETE", "/_snapshot/snapcheck-repo")
    return finish(results)


def finish(results):
    passed = sum(results)
    print(f"\n  {passed}/{len(results)} checks passed")
    print("RESULT", "restore brings back what was taken, or refuses" if all(results) else "FAILED")
    return 0 if all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
