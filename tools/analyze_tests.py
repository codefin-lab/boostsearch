#!/usr/bin/env python3
"""Classify OpenSearch's YAML rest tests by the APIs they exercise.

Feeds the Phase 1 cut: we want the files whose APIs cover the traffic people
actually send, not the long tail.
"""
import re, sys, json, collections, pathlib

ROOT = pathlib.Path("study/OpenSearch/rest-api-spec/src/main/resources/rest-api-spec/test")

# hand-ranked: what a normal OpenSearch workload actually calls
PHASE1_APIS = {
    "indices.create", "indices.delete", "indices.exists", "indices.refresh",
    "indices.get_mapping", "indices.put_mapping", "indices.get_settings",
    "index", "create", "bulk", "get", "get_source", "exists", "delete", "update",
    "mget", "search", "count", "msearch", "search.aggregation", "field_caps",
}
DO_RE = re.compile(r"^\s{2,}(?:-\s+)?do:\s*$")
API_RE = re.compile(r"^\s+([a-z_]+(?:\.[a-z_]+)*):\s*$")
INLINE_DO_RE = re.compile(r"do:\s*\{\s*([a-z_]+(?:\.[a-z_]+)*)\s*:")
ASSERT_RE = re.compile(r"^\s*-\s+(match|length|is_true|is_false|gt|lt|gte|lte|set|contains):")
SECTION_RE = re.compile(r"^---\s*$")

def apis_in(path):
    apis = collections.Counter()
    asserts = 0
    sections = 0
    lines = path.read_text(errors="replace").splitlines()
    for i, line in enumerate(lines):
        if SECTION_RE.match(line):
            sections += 1
        if ASSERT_RE.match(line):
            asserts += 1
        m = INLINE_DO_RE.search(line)
        if m:
            apis[m.group(1)] += 1
            continue
        if DO_RE.match(line):
            for nxt in lines[i+1:i+4]:
                m2 = API_RE.match(nxt)
                if m2:
                    apis[m2.group(1)] += 1
                    break
    return apis, asserts, max(sections - 1, 0)

def main():
    rows = []
    for p in sorted(ROOT.rglob("*.yml")):
        apis, asserts, sections = apis_in(p)
        area = p.relative_to(ROOT).parts[0]
        rows.append({
            "file": str(p.relative_to(ROOT)),
            "area": area,
            "apis": dict(apis),
            "asserts": asserts,
            "sections": sections,
        })

    api_totals = collections.Counter()
    for r in rows:
        for a, n in r["apis"].items():
            api_totals[a] += n

    # a file is Phase 1 if every API it touches is in the Phase 1 set
    for r in rows:
        used = set(r["apis"])
        r["phase1"] = bool(used) and used <= PHASE1_APIS
        r["blockers"] = sorted(used - PHASE1_APIS)

    p1 = [r for r in rows if r["phase1"]]
    print(f"total files            : {len(rows)}")
    print(f"phase-1 eligible files : {len(p1)}")
    print(f"phase-1 assertions     : {sum(r['asserts'] for r in p1)}")
    print(f"all assertions         : {sum(r['asserts'] for r in rows)}")
    print()
    print("=== top 30 APIs by call count across the whole suite ===")
    for a, n in api_totals.most_common(30):
        mark = "P1" if a in PHASE1_APIS else "  "
        print(f"  {mark} {a:<34} {n:>5}")
    print()
    print("=== phase-1 files by area ===")
    by_area = collections.Counter(r["area"] for r in p1)
    for a, n in by_area.most_common():
        print(f"     {a:<28} {n:>3} files")
    print()
    print("=== near-misses: files blocked by exactly one out-of-scope API ===")
    near = collections.Counter()
    for r in rows:
        if not r["phase1"] and len(r["blockers"]) == 1:
            near[r["blockers"][0]] += 1
    for a, n in near.most_common(15):
        print(f"     {a:<34} blocks {n:>3} files")

    pathlib.Path("tools/test_inventory.json").write_text(json.dumps(rows, indent=1))
    print("\nwrote tools/test_inventory.json")

main()
