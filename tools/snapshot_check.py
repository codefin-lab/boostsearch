#!/usr/bin/env python3
"""Backup and restore, checked against what went in.

Documents with known content are indexed, a snapshot is taken into a
filesystem repository, and it is restored under another name and compared
document by document -- count, ids and a digest of every source. Then the
snapshot's documents file is cut short, has a line spoiled, and is taken away,
and each restore must refuse rather than bring back part of the index and call
it success.

A restore that refuses must also leave alone what it would have replaced. The
same damage is done while restoring over the original index, closed; over a
closed index under the name a rename gives; with a description whose analysis
cannot be built; and with two indices of which only the second is damaged.
Every time the restore must fail, and the index that was there must still be
there, closed as it was, and hold every document it held.

Global state goes with a snapshot that keeps it: templates of both kinds,
component templates, ingest and search pipelines, stored scripts and the
persistent settings come back with `include_global_state`, and not without it.

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

INDICES = ["snapcheck", "snapcheck-back", "snapcheck-short", "snapcheck-spoiled", "snapcheck-gone",
           "snapcheck-two", "snapcheck-over"]


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


def is_closed(index):
    status, r = call("GET", f"/_cat/indices/{index}?format=json&expand_wildcards=all")
    return status == 200 and bool(r) and r[0].get("status") == "close"


GLOBAL = [
    ("legacy template", "/_template/snapcheck-legacy", {"index_patterns": ["snapcheck-legacy-*"], "settings": {"number_of_replicas": 0}}),
    ("composable template", "/_index_template/snapcheck-composable", {"index_patterns": ["snapcheck-composable-*"], "template": {"settings": {"number_of_replicas": 0}}}),
    ("component template", "/_component_template/snapcheck-component", {"template": {"mappings": {"properties": {"k": {"type": "keyword"}}}}}),
    ("ingest pipeline", "/_ingest/pipeline/snapcheck-ingest", {"processors": [{"set": {"field": "restored", "value": True}}]}),
    ("search pipeline", "/_search/pipeline/snapcheck-search", {"request_processors": [{"filter_query": {"query": {"match_all": {}}}}]}),
    ("stored script", "/_scripts/snapcheck-script", {"script": {"lang": "painless", "source": "doc['n'].value * 2"}}),
]
SETTING = "search.max_buckets"


def global_present():
    out = {name: call("GET", path)[0] == 200 for name, path, _ in GLOBAL}
    _, r = call("GET", "/_cluster/settings?flat_settings=true")
    out["persistent setting"] = r.get("persistent", {}).get(SETTING) in ("4321", 4321)
    return out


def main():
    results = []

    def check(name, ok, detail=""):
        results.append(ok)
        print(f"  {'ok    ' if ok else 'FAILED'} {name}")
        if not ok and detail:
            print(f"    {detail}")
        return ok

    repo_dir = os.path.join(REPO_ROOT, "snapshot-check")
    shutil.rmtree(repo_dir, ignore_errors=True)
    for name in INDICES:
        call("DELETE", f"/{name}")
    for _name, path, _ in GLOBAL:
        call("DELETE", path)
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
        if i % 10 == 0:
            lines.append(json.dumps({"index": {"_index": "snapcheck-two", "_id": f"t{i}"}}))
            lines.append(json.dumps({"n": i, "second": True}))
    status, r = call("POST", "/_bulk?refresh=true", "\n".join(lines) + "\n", ndjson=True)
    check("the documents are indexed", status == 200 and not r.get("errors"), str(r)[:300])
    before = contents("snapcheck")
    before_two = contents("snapcheck-two")
    check("every document reads back before the snapshot", len(before) == DOCS, f"{len(before)} of {DOCS}")

    for name, path, body in GLOBAL:
        status, r = call("PUT", path, body)
        check(f"the {name} is made", status == 200, str(r)[:200])
    status, r = call("PUT", "/_cluster/settings", {"persistent": {SETTING: 4321}})
    check("the persistent setting is made", status == 200, str(r)[:200])

    status, r = call("PUT", "/_snapshot/snapcheck-repo", {"type": "fs", "settings": {"location": repo_dir}})
    check("the repository is registered", status == 200, str(r)[:300])
    status, r = call("PUT", "/_snapshot/snapcheck-repo/snap1?wait_for_completion=true", {"indices": "snapcheck,snapcheck-two"})
    snap = r.get("snapshot", {})
    check("the snapshot is taken", status == 200 and snap.get("state") == "SUCCESS", str(r)[:300])
    status, r = call("PUT", "/_snapshot/snapcheck-repo/bare?wait_for_completion=true",
                     {"indices": "snapcheck-two", "include_global_state": False})
    check("a snapshot without global state is taken", status == 200, str(r)[:300])
    _, r = call("GET", "/_snapshot/snapcheck-repo/snap1,bare")
    kept = {s["snapshot"]: s.get("include_global_state") for s in r.get("snapshots", [])}
    check("each snapshot says whether it kept global state", kept == {"snap1": True, "bare": False}, str(kept))

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

    # the documents file of each index in the snapshot, and its description
    def files_of(index):
        root = os.path.join(repo_dir, "snap1", index)
        docs = sorted(os.path.join(root, f) for f in os.listdir(root) if f.endswith(".ndjson")) if os.path.isdir(root) else []
        return docs, os.path.join(root, "meta.json")

    found, meta_file = files_of("snapcheck")
    found_two, _ = files_of("snapcheck-two")
    check("the snapshot's documents files are where the repository keeps them",
          len(found) == 1 and len(found_two) == 1 and os.path.exists(meta_file), str(found + found_two))
    if len(found) != 1 or len(found_two) != 1:
        return finish(results)
    docs_file = found[0]
    with open(docs_file, "rb") as f:
        good = f.read()
    with open(meta_file, "rb") as f:
        good_meta = f.read()

    def put_back():
        with open(docs_file, "wb") as f:
            f.write(good)
        with open(meta_file, "wb") as f:
            f.write(good_meta)

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

    def unbuildable_analysis():
        meta = json.loads(good_meta)
        index = meta.setdefault("settings", {}).setdefault("index", {})
        index.setdefault("analysis", {})["filter"] = {"broken": {"type": "no_such_filter"}}
        with open(meta_file, "w") as f:
            json.dump(meta, f)

    damages = [("short", "cut short", cut_short), ("spoiled", "with a spoiled line", spoil_a_line),
               ("gone", "taken away", take_away)]

    def restore_refused(name, damage):
        damage()
        status, r = call(
            "POST",
            "/_snapshot/snapcheck-repo/snap1/_restore?wait_for_completion=true",
            {"indices": "snapcheck", "rename_pattern": "snapcheck", "rename_replacement": f"snapcheck-{name}"},
        )
        st, _ = call("GET", f"/snapcheck-{name}/_count")
        put_back()
        return status >= 400 and st == 404, f"restore answered {status} {str(r)[:200]}; the index answers {st}"

    for name, how, damage in damages:
        ok, detail = restore_refused(name, damage)
        check(f"a documents file {how} is refused, and no index is left", ok, detail)

    def survives(target, expected, request, damage):
        """Restore over a closed index with the snapshot damaged: refused, and
        the index is still there, closed, holding what it held."""
        status, r = call("POST", f"/{target}/_close")
        if status != 200:
            return False, f"closing [{target}] answered {status} {str(r)[:200]}"
        damage()
        status, r = call("POST", "/_snapshot/snapcheck-repo/snap1/_restore?wait_for_completion=true", request)
        put_back()
        refused = status >= 400
        still_closed = is_closed(target)
        call("POST", f"/{target}/_open")
        call("GET", f"/_cluster/health/{target}?wait_for_status=yellow&timeout=10s")
        held = contents(target)
        return (refused and still_closed and held == expected,
                f"restore answered {status} {str(r)[:200]}; closed afterwards: {still_closed}; "
                f"{len(held)} of {len(expected)} documents")

    for name, how, damage in damages + [("analysis", "whose analysis cannot be built", unbuildable_analysis)]:
        what = "a description" if name == "analysis" else "a documents file"
        ok, detail = survives("snapcheck", before, {"indices": "snapcheck"}, damage)
        check(f"a restore over the closed original from {what} {how} fails, and the original is intact", ok, detail)

    ok, detail = survives("snapcheck-back", before,
                          {"indices": "snapcheck", "rename_pattern": "snapcheck", "rename_replacement": "snapcheck-back"},
                          cut_short)
    check("a renamed restore over a closed index from a damaged file fails, and that index is intact", ok, detail)

    # two indices, the first whole and the second damaged: neither is replaced
    two_file = found_two[0]
    with open(two_file, "rb") as f:
        good_two = f.read()
    call("POST", "/snapcheck/_close")
    call("POST", "/snapcheck-two/_close")
    with open(two_file, "wb") as f:
        f.write(good_two[: len(good_two) // 2])
    status, r = call("POST", "/_snapshot/snapcheck-repo/snap1/_restore?wait_for_completion=true",
                     {"indices": "snapcheck,snapcheck-two"})
    with open(two_file, "wb") as f:
        f.write(good_two)
    closed_both = is_closed("snapcheck") and is_closed("snapcheck-two")
    call("POST", "/snapcheck/_open")
    call("POST", "/snapcheck-two/_open")
    held, held_two = contents("snapcheck"), contents("snapcheck-two")
    check("a restore of two indices, the second damaged, fails and replaces neither",
          status >= 400 and closed_both and held == before and held_two == before_two,
          f"restore answered {status} {str(r)[:200]}; closed afterwards: {closed_both}; "
          f"{len(held)} of {len(before)} and {len(held_two)} of {len(before_two)}")

    # and whole, over both closed: both come back, open
    call("POST", "/snapcheck/_close")
    call("POST", "/snapcheck-two/_close")
    status, r = call("POST", "/_snapshot/snapcheck-repo/snap1/_restore?wait_for_completion=true",
                     {"indices": "snapcheck,snapcheck-two"})
    held, held_two = contents("snapcheck"), contents("snapcheck-two")
    check("an undamaged restore over both closed indices brings both back, open and whole",
          status == 200 and held == before and held_two == before_two,
          f"restore answered {status} {str(r)[:200]}; {len(held)} of {len(before)} and {len(held_two)} of {len(before_two)}")

    # global state
    for _name, path, _ in GLOBAL:
        call("DELETE", path)
    call("PUT", "/_cluster/settings", {"persistent": {SETTING: None}})
    gone = global_present()
    check("the global objects are deleted", not any(gone.values()), str(gone))
    status, r = call("POST", "/_snapshot/snapcheck-repo/snap1/_restore?wait_for_completion=true",
                     {"indices": "-*", "include_global_state": False})
    still = global_present()
    check("a restore without include_global_state leaves them deleted", status == 200 and not any(still.values()),
          f"{status} {str(r)[:200]} {still}")
    status, r = call("POST", "/_snapshot/snapcheck-repo/bare/_restore?wait_for_completion=true",
                     {"indices": "-*", "include_global_state": True})
    still = global_present()
    check("a snapshot taken without global state restores none", status == 200 and not any(still.values()),
          f"{status} {str(r)[:200]} {still}")
    call("PUT", "/_index_template/snapcheck-later", {"index_patterns": ["snapcheck-later-*"]})
    call("PUT", "/_template/snapcheck-later-legacy", {"index_patterns": ["snapcheck-later-legacy-*"]})
    status, r = call("POST", "/_snapshot/snapcheck-repo/snap1/_restore?wait_for_completion=true",
                     {"indices": "-*", "include_global_state": True})
    back = global_present()
    check("a restore with include_global_state brings every global object back",
          status == 200 and all(back.values()), f"{status} {str(r)[:200]} {back}")
    later = call("GET", "/_index_template/snapcheck-later")[0]
    later_legacy = call("GET", "/_template/snapcheck-later-legacy")[0]
    check("as the reference does: composable templates are replaced whole, legacy ones kept beside",
          later == 404 and later_legacy == 200, f"composable {later}, legacy {later_legacy}")

    for name in INDICES:
        call("DELETE", f"/{name}")
    for _name, path, _ in GLOBAL:
        call("DELETE", path)
    call("DELETE", "/_template/snapcheck-later-legacy")
    call("PUT", "/_cluster/settings", {"persistent": {SETTING: None}})
    call("DELETE", "/_snapshot/snapcheck-repo/snap1")
    call("DELETE", "/_snapshot/snapcheck-repo/bare")
    call("DELETE", "/_snapshot/snapcheck-repo")
    return finish(results)


def finish(results):
    passed = sum(results)
    print(f"\n  {passed}/{len(results)} checks passed")
    print("RESULT", "restore brings back what was taken, or refuses and changes nothing" if all(results) else "FAILED")
    return 0 if all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
