# BoostSearch v1 — the work

What version one is, how it is measured, and what is left to build. Written
2026-08-31, after the decisions in `docs/adr/`.

## What v1 means

BoostSearch answers everything OpenSearch 3.x answers, and beats it on every
dimension of the bench matrix. Elasticsearch 8/9 is a separate target and is
version two (ADR 0006, 0001–0005 for the rest).

Phase 13 is the exception to that sentence. The other twelve are about
answering the same questions as OpenSearch; the thirteenth answers the ones
its console asks, so that the console runs on a Rust server rather than behind
two and a half gigabytes of Node. The console's own front end is left as it
is.

## Where it stands today

| | |
|---|---|
| OpenSearch's own tests | **1,516 of 2,322 sections** (core spec 1,427/1,427, modules 89/895) |
| Behavioural diff vs OpenSearch 3.1.0 | 122 of 183 canonical requests identical |
| REST endpoints answered | 141 of 167 |
| Bench matrix | 12 of 12 dimensions ahead — index 1.15x, memory 4.4x, every one of the ten query shapes 1.6–3.8x |

`tools/yaml_runner.py`, `tools/compat_audit.py` and `tools/bench_matrix.py` are
how each of those numbers is produced, and they are the gates below.

## The gates

- **The corpus**: 2,322 sections from OpenSearch's own YAML tests, plus what of
  the security plugin's suite can be pointed at an HTTP server.
- **The diff**: `compat_audit replay` reports no difference in an answer -- the
  documents returned, in order, the numbers over them, the tokens a text was
  cut into.
- **The commit gate**: the bench matrix against our own last numbers. A 5% fall
  in any dimension is red (ADR 0004).
- **The release gate**: the bench matrix against OpenSearch. Every dimension
  ahead, or there is no release.

## The phases

Days are working days on one stream.

### 0. The tree the rest of the work grows in — 10 days

Three streams cannot edit `api.rs` at nine thousand lines without meeting each
other every day, and a new pair of hands cannot find anything in it. This is
pure movement: no behaviour changes, and the corpus says so after every step.

| | | days |
|---|---|---:|
| 0.1 | `search.rs` (10,984 lines, 184 functions) into `search/`: the request pipeline, collection, sorting, hits and fetch, highlighting, collapse, nested, suggest, profile, and `search/aggs/` beneath it | 3 |
| 0.2 | `api.rs` (9,283 lines, 245 handlers) into `api/`: documents, search, indices, mapping, settings, aliases, templates, cat, cluster, nodes, snapshot, ingest, data streams, stats, tasks | 2 |
| 0.3 | `store.rs` and `query.rs` along the same lines: index state, mapping, coercion, translog, routing; and term, text, range, bool, geo, nested, intervals | 2 |
| 0.4 | The 44 `unwrap()`s on request paths, given errors that say what happened | 2 |
| 0.5 | `clippy` and `fmt` as CI gates, and a module map in `CONTRIBUTING.md` so three streams put things in the same places | 1 |

Gate: 2,322 sections unchanged, the bench matrix unchanged, and no file over
about two thousand lines.

### 1. Analysis — 22 days

An analyzer belongs to a path, and BoostCore learns to pick one per path
(ADR 0001).

| | | days |
|---|---|---:|
| 1.1 | BoostCore: a resolver from path to analyzer in the write path | 2 |
| 1.2 | The chain wired: registry per index, indexing, querying, `_analyze`, `search_analyzer`, `normalizer` | 4 |
| 1.3 | The 18 built-in analysers and the ~25 filters the tests name, token for token | 5 |
| 1.4 | Japanese, Korean, Chinese (lindera), ICU (icu4x), Thai segmentation | 8 |
| 1.5 | `analysis-common` green, analyser diff empty | 3 |

Gate: ~1,700 of 2,296 sections; no analyser difference.

### 2. Query, aggregation, endpoint — 32 days

| | | days |
|---|---|---:|
| 2.1 | The missing queries: the seven span queries, `function_score`, `more_like_this`, `common`, `wrapper`, `match_phrase_prefix` | 6 |
| 2.2 | The aggregations that differ: four geo ones, `matrix_stats`, `sampler`, `diversified_sampler`, `significant_terms`, `top_hits`, `auto_date_histogram` | 6 |
| 2.3 | `_delete_by_query`, `_update_by_query`, `_reindex` | 5 |
| 2.4 | Point-in-time, search templates, `rank_eval`, `parent-join` | 6 |
| 2.5 | The field types in `mapper-extras`: binary, token_count, rank_feature(s), percolator, completion | 6 |
| 2.6 | Answer shapes: whole numbers where OpenSearch writes whole numbers, `_settings`, `_field_caps`, `_termvectors`, default order | 3 |

Gate: ~2,000 sections; the only differences left are the ones that need scripts.

### 3. Painless — 19 days

A subset with statements: the tests are 189 uses of `params` and 82 of
`doc[]`, and a handful of `if`, `for` and lambdas.

| | | days |
|---|---|---:|
| 3.1 | Lexer, parser, AST | 4 |
| 3.2 | Evaluator: `def`, Java coercion, operators, `?.` and `?:` | 5 |
| 3.3 | Contexts: score, filter, field, `ctx._source`, bucket pipelines, ingest, `scripted_metric` | 4 |
| 3.4 | The whitelist: doc values, `Math`, `String`, `List`, `Map`, dates, term statistics; limits on time and steps | 4 |
| 3.5 | `lang-painless` green | 2 |

### 4. Ingest — 11 days

| | | days |
|---|---|---:|
| 4.1 | The thirty processors the corpus names, including grok and dissect | 8 |
| 4.2 | Search pipelines and the user-agent processor | 3 |

Gate: **2,296 of 2,296 on a single node.**

### 5. Security — 35 days

In the query path, not in front of it (ADR 0005).

| | | days |
|---|---|---:|
| 5.1 | TLS, certificates, HTTP and transport layers | 4 |
| 5.2 | Users, roles, role mappings, API keys, the internal user store | 6 |
| 5.3 | The caller threaded from handler to query builder | 3 |
| 5.4 | Document-level security: the role's filter inside every query | 6 |
| 5.5 | Field-level security: hidden in `_source`, aggregations, sorts, `fields`, highlighting, `field_caps` | 8 |
| 5.6 | SAML, OIDC, LDAP | 5 |
| 5.7 | The audit log | 3 |

### 6. Cluster — 80 days

Written against a transport it does not own, so it can be simulated
(ADR 0002); one consistency mode shipped, two designed for (ADR 0003).

| | | days |
|---|---|---:|
| 6.1 | `Transport` and `Clock` as traits, message framing, node identity | 4 |
| 6.2 | The simulation: a seeded scheduler, partitions, crashes, clock skew | 8 |
| 6.3 | Cluster state: versioned metadata, the shard map, join and leave | 6 |
| 6.4 | Consensus: election, log, commit index, membership change | 12 |
| 6.5 | Allocation, rebalancing, the deciders | 8 |
| 6.6 | Replication with the mode as a parameter: primary and replica writes, acknowledgement policy, read routing | 8 |
| 6.7 | Peer recovery: seed from a snapshot, replay the translog, catch up, track who is in sync | 8 |
| 6.8 | The coordinator: fan out a search across nodes, merge, partial results, `_shards` | 6 |
| 6.9 | Invariants inside the simulation: nothing acknowledged is lost, no two primaries accept writes, no divergence after recovery | 5 |
| 6.10 | Linearizability against real nodes, with real partitions | 6 |
| 6.11 | Chaos, soak, rolling restart | 5 |
| 6.12 | The corpus and the diff on three nodes; the rolling-upgrade tests | 4 |

Gate: 2,296 sections on three nodes; ten thousand seeds with no invariant
broken; no anomaly from the linearizability checker.

### 7. Proof and release — 29 days

| | | days |
|---|---|---:|
| 7.1 | OpenSearch Dashboards, end to end | 6 |
| 7.2 | The Python, JavaScript, Java and Go clients, running their own suites | 4 |
| 7.3 | The matrix widened to eighteen dimensions, on cloud hardware, as a release gate | 5 |
| 7.4 | Tuning, until every dimension is ahead again | 8 |
| 7.5 | Documentation, packaging, the upgrade path, the release itself | 6 |

### 8. The last twenty-six sections — 17 days

geoip (3), the URL repository (2), Lucene expressions (3), phonetic and
phone-number filters (3), attachment extraction (6).

### 9. Repositories — 6 days

S3, GCS, Azure.

### 10. Index management — 15 days

Policies, transitions, rollover, snapshot management.

### 11. Vector search — 20 days

HNSW, filtered search, nested vectors, the on-disk format, the k-NN API.

### 12. SQL and PPL — 50 days

Two query languages: lexer, parser, planner, execution against the search
layer, and the response shapes their drivers expect.

### 13. The console's server — 30 days

OpenSearch Dashboards is two programs. One is a Node server: it keeps the
saved objects, migrates them, proxies the search API, serves the front end and
hands it the metadata it boots from. The other is the React application in the
browser. The first is where the two and a half gigabytes of image and the
several hundred megabytes of resident memory go; the second is fine as it is.

So the second is left alone. We do not fork the front end, do not rewrite it,
and do not touch a line of its JavaScript -- we serve the bundle the OpenSearch
project publishes, and replace the program behind it with a Rust one.

This is the smaller half of the work and nearly all of the win: the browser
does what it always did, and the server it talks to starts in milliseconds
instead of most of a minute and holds tens of megabytes instead of hundreds.

| | | days |
|---|---|---:|
| 13.1 | The shell: the built assets, the metadata the front end boots from, the base path, the CSP, the translations | 6 |
| 13.2 | Settings and status: `uiSettings` and the config object behind it, `/api/status` | 3 |
| 13.3 | Saved objects: the store, the migrations that make `.kibana_N` and move the alias, the API and the management routes | 8 |
| 13.4 | What the pages ask for: index patterns, `_fields_for_wildcard`, the internal search endpoints, the Dev Tools proxy, short URLs | 5 |
| 13.5 | The plugin routes the pages we answer for need; a plain refusal for the rest | 4 |
| 13.6 | The gate: every Phase 7.1 flow, through our server, against the same browser | 4 |

Gate: the flows Phase 7.1 drives -- migration, Discover, the Visualize
editor, a saved dashboard, the saved-object round trip, Index Management --
pass against our server with the front end unchanged; resident memory under
64MiB against the Node server's several hundred; ready to serve in under a
second against its thirty.

What is not claimed: the plugin system. Dashboards loads fifty-four plugins,
most of them for features this plan does not have. Our server answers the
routes the pages above need and says plainly that it does not answer the rest,
which is what it already does for `_plugins/_ism` and the alerting searches.

The risk is that the metadata the front end boots from is a contract between
two halves of one program, and nobody wrote it down. It changes when
Dashboards changes, so the version we serve is pinned and moving it is a
task, not an accident.

The licence allows this. OpenSearch Dashboards is Apache 2.0, the same licence
this repository already carries, and what we redistribute keeps its NOTICE.

The browser application in Rust and WebAssembly is a separate question and not
in this plan. If the server is answered first, that question can be asked one
page at a time afterwards, with the page it replaces still there to compare
against.

## The arithmetic

| | days | weeks |
|---|---:|---:|
| Phase 0, the tree | 10 | 2 |
| Phases 1–4, single node | 84 | 17 |
| Phase 5, security | 35 | 7 |
| Phase 6, cluster | 80 | 16 |
| Phase 7, proof and release | 29 | 6 |
| Phases 8–12, the rest of the surface | 108 | 22 |
| Phase 13, the console's server | 30 | 6 |
| **Total, one stream** | **376** | **75** |

| streams | critical path | calendar |
|---|---|---|
| one | everything in order | 75 weeks |
| two | search and ecosystem beside security and cluster | ~39 weeks |
| three | search · security then cluster · ecosystem, the console beside them | ~32 weeks |

The console is the one phase that needs nothing from the others: it talks to
the server over the same REST API any client uses, so it can be built beside
the cluster work rather than after it.

Phase 0 comes before the streams split, and is the reason they can.

The cluster is the critical path and no number of people shortens it.

## What is not claimed

`ingest-attachment` reads pdf, html, docx, xlsx, pptx, txt, rtf and doc. Tika
reaches about fourteen hundred formats through a stack of Java libraries, and
we do not. Everywhere else in this plan, 100% means 100%.

## What is riskiest

Phase 6. Consensus and recovery are where the bugs are rare, timing-dependent
and expensive, and twelve days for consensus is what it costs when it goes
well. The simulation exists so that when it does not go well, the failure has
a seed number and can be run again.
