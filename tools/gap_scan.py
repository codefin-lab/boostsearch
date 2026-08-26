#!/usr/bin/env python3
"""Cross the Phase-1 test files against features tantivy 0.26 does not have.

Confirmed absent in tantivy source: block-join/nested, geo, field collapsing,
rescore, suggesters, search_after over hits, scripting, percolate, inner_hits,
parent/child, doc-level update semantics, _seq_no/_primary_term.
"""
import json, re, pathlib, collections

ROOT = pathlib.Path("study/OpenSearch/rest-api-spec/src/main/resources/rest-api-spec/test")
rows = json.loads(pathlib.Path("tools/test_inventory.json").read_text())

# feature -> regex over the raw YAML
GAPS = {
    "nested/inner_hits": r"\bnested\b|inner_hits",
    "join/parent-child": r"\bhas_child\b|\bhas_parent\b|\bparent_id\b|\bjoin\b",
    "geo":               r"geo_point|geo_shape|geo_distance|geo_bounding_box",
    "field collapsing":  r"^\s*collapse:|\bcollapse\b",
    "rescore":           r"\brescore\b",
    "suggesters":        r"\bsuggest\b|completion|phrase_suggester",
    "search_after":      r"search_after",
    "scroll/PIT":        r"\bscroll\b|point_in_time",
    "scripting":         r"\bscript\b|painless",
    "highlighting":      r"\bhighlight\b",
    "seq_no/versioning": r"_seq_no|_primary_term|version_type",
    "runtime/derived":   r"runtime_field|derived",
    "percolate":         r"percolat",
    "significant terms": r"significant_te",
    "pipeline aggs":     r"bucket_script|bucket_selector|derivative|moving_|cumulative_",
    "adjacency/sampler": r"adjacency_matrix|\bsampler\b|diversified",
    "geo aggs":          r"geohash_grid|geotile_grid|geo_bounds|geo_centroid",
    "multi_terms":       r"multi_terms",
    "scripted_metric":   r"scripted_metric",
}
compiled = {k: re.compile(v, re.M) for k, v in GAPS.items()}

p1 = [r for r in rows if r["phase1"]]
blocked = collections.Counter()
clean = []
blocked_files = collections.defaultdict(list)

for r in p1:
    text = (ROOT / r["file"]).read_text(errors="replace")
    hits = [k for k, rx in compiled.items() if rx.search(text)]
    if hits:
        for h in hits:
            blocked[h] += 1
            blocked_files[h].append(r["file"])
    else:
        clean.append(r)

print(f"phase-1 eligible files      : {len(p1)}")
print(f"  ...clean of tantivy gaps  : {len(clean)}   ({sum(r['asserts'] for r in clean)} assertions)")
print(f"  ...touching >=1 gap       : {len(p1) - len(clean)}")
print()
print("=== which gap blocks how many phase-1 files ===")
for k, n in blocked.most_common():
    print(f"  {k:<20} {n:>3} files")
print()
print("=== clean files by area (this is the real Phase 1 target) ===")
by_area = collections.Counter(r["area"] for r in clean)
for a, n in by_area.most_common():
    print(f"  {a:<26} {n:>3} files  {sum(x['asserts'] for x in clean if x['area']==a):>5} asserts")

pathlib.Path("tools/phase1_manifest.json").write_text(json.dumps(
    {"clean": [r["file"] for r in clean],
     "blocked": {k: v for k, v in blocked_files.items()}}, indent=1))
print("\nwrote tools/phase1_manifest.json")
