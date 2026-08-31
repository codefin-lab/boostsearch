# Working on BoostSearch

## Where things are

```
src/
  main.rs          the routing table: every endpoint, in one list
  lib.rs           the same code as a library, so benchmarks drive what the server does

  api/             one module per thing an endpoint is about
    doc            writing, reading and deleting documents, and bulk
    source         what of a document goes back, and in what shape
    search_api     _search and everything beside it: msearch, scroll, field_caps, analyze
    indices        an index as a thing: made, opened, resized, rolled over
    mapping        settings         alias         template
    cat            cluster          nodes         stats
    snapshot       ingest           datastream    tasks
    shared         errors, parameters, and the shapes of an answer

  search/          answering a search
    mod            the pipeline: run, the shard search, the limits, the types
    sort  page  nested  geo  highlight  suggest  extras  lookup  profile  routing
    aggs/
      plan         who answers which aggregation, and what is held back
      bucket  composite  histogram  metric  pipeline  format

  query/           the query DSL as BoostCore queries
    mod            Ctx, the dispatcher, and what a field resolves to
    text  range  terms  bool  pattern  analyze

  store/           what an index is, and what it holds
    mod            the types: Fields, Mapping, IdxState, Store
    registry       the indices this node has
    objects        scrolls, templates, repositories, snapshots, pipelines, data streams
    translog  writer  ids  settings  mapping  coerce  dates  net

  analysis.rs      the analysis chain: tokenizer, filters, the built-in analysers
  snapshot.rs      snapshots that copy documents
  source.rs        _source filtering and date formats
  blockstats.rs    per-block statistics, so a range scan can skip runs
  hdr.rs  tz.rs    percentile sketches, and the zone database
```

A type lives in its `mod.rs`; the functions that work on it live in the module
named after what they are for. A child module can see its parent's private
items, which is why moving a function rarely means widening anything.

## Before you push

```bash
cargo build --release          # no warnings
cargo clippy --all-targets     # no warnings
cargo fmt --check              # no diff
```

and the corpus, which is the point of the whole thing:

```bash
BOOSTSEARCH_NODE_ATTRS=testattr=test ./target/release/boostsearch &
python3 tools/yaml_runner.py --manifest tools/phase1_manifest.json    # 398/398
python3 tools/yaml_runner.py --manifest tools/phase3_manifest.json    # 1,100/1,100
python3 tools/yaml_runner.py --manifest tools/modules_manifest.json   # the work list
```

CI runs all of it, twice: once with indices in memory, once on disk.

## The tools

| | |
|---|---|
| `tools/yaml_runner.py` | OpenSearch's own YAML tests against this server |
| `tools/compat_audit.py` | what a cluster uses, and where two engines answer differently |
| `tools/bench_matrix.py` | index throughput, memory, and query latency, both engines |
| `tools/gen_dataset.py` | the http-log corpus the benchmarks use |

## What to read first

`docs/plan-v1.md` is the work and its order. `CONTEXT.md` is what the words
mean. `docs/adr/` is why five decisions were made the way they were -- read
0001 before touching analysis and 0002 before touching anything that will
become the cluster.

## The rules that are not style

- **No `unwrap()` on a request path.** If an invariant really holds, `expect`
  it and say why in the message.
- **A comment says why, not what.** The code says what.
- **A change that costs performance says so in its commit message.** CI
  measures every commit against the last one and goes red at a 5% fall.
- **The corpus is the specification.** If OpenSearch's test says a response
  looks a certain way, that is the answer, whatever seems more sensible.
