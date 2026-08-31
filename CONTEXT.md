# Context

The language this project uses, and what each word is bound to. Not a spec.

## The things

**BoostSearch** — the search server. Speaks the OpenSearch HTTP API. One
process, one node. Formerly called obsearch; the name changed on 2026-08-31
and nothing else did.

**BoostCore** — the search engine library BoostSearch is built on. A fork of
tantivy 0.26.1, kept in its own repository so the parts of the engine the
server depends on can be changed. Not a general-purpose fork: it exists to
serve BoostSearch.

**The compatibility target** — OpenSearch 3.x. When this project says
"compatible", it means "answers what OpenSearch 3.x answers, to the request
OpenSearch 3.x was given". Elasticsearch 8/9 is a separate target, deferred to
a second version, and no claim is made about it.

## The measures

**The conformance corpus** — OpenSearch's own YAML REST tests, run against
BoostSearch by `tools/yaml_runner.py`. Two parts, and a section is the unit:

- *the core spec*: 409 files, 1,427 sections, from `rest-api-spec`
- *the module corpus*: 206 files, 895 sections, shipped by the modules and
  plugins that apply to a single node

**The v1 target** — 2,296 of those 2,322 sections. The 26 left out are the ones
that need something this project does not carry: a geoip database, Tika, Lucene
expressions, a URL repository, phonetic and phone-number filters. They answer
501 and say why. Everything else, including the Japanese, Korean, Chinese and
ICU analysers, is in.

**A section** — one named test in a YAML file. The number that matters is
sections passing out of 2,322, not files.

**The behavioural diff** — what `tools/compat_audit.py replay` reports: the
same request put to OpenSearch and to BoostSearch, with the answers compared.
An answer is the documents that came back in the order they came back, the
numbers over them, and the tokens a text was cut into. Everything else --
`took`, `_shards`, ids that are allowed to differ -- is not an answer.

**The bench matrix** — `tools/bench_matrix.py`: index throughput, resident
memory, and the median latency of ten query shapes, both engines, same corpus,
same machine. "Winning a dimension" means a better number in that row.

**The commit gate** — the matrix against our own previous numbers. Red at a 5%
fall in any dimension.

**The release gate** — the matrix against OpenSearch. Every dimension ahead, or
there is no release.

## The analysis words

**An analyzer** — a tokenizer and the chain of filters after it, named by an
index's settings or built in. A field names one; a search has to use the same
one, or the query and the document never meet.

**A chain** — the analyzer's parts in order, as this project models them:
one source of tokens, then the steps that change, drop or add to them.

**The registry** — the analyzers one index can name: the ones it defined, over
the built-in ones. Per index, because two indices may define the same name
differently.

## The security words

**The caller** — who a request is from, once it has been authenticated. Carried
into the query path, because what a caller may see decides which documents a
query may match and which fields anything may read.

**Document-level security** — a filter the caller's role adds to every query.
Not a filter applied to the results: a document a caller may not see must not
be counted, aggregated or scored either.

**Field-level security** — the fields a caller's role hides. Hidden means
invisible to `_source`, to aggregations, to sorts, to `fields`, to highlighting
and to `field_caps`, not merely stripped on the way out.

## The cluster words

**A node** — one BoostSearch process. In version 1 a cluster is several of
them, and which shard lives where is decided by the cluster itself rather than
by a file someone edits.

**Acknowledged** — a write the client was told succeeded. In version 1 that
means what it means in OpenSearch: the primary took it and as many copies as
`wait_for_active_shards` asked for. It does not mean every replica has it, and
a read from a replica may not show it yet.

**The consistency mode** — which promise an index makes about that. Version 1
ships one, `opensearch`; `linearizable` is version 2, and the replication path
carries the parameter from the start so that adding it is not a rewrite.

**The simulation** — the whole cluster running in one process on a controlled
clock and a controlled network, driven by a seed. Where the cluster's own tests
live, because a real network cannot be asked for a particular interleaving.

## The storage words

**Refresh** — making writes visible to search. Here it is also a commit, and a
commit is an fsync; OpenSearch separates the two, and this is the reason a
refresh costs what it costs.

**The translog** — the per-index record of writes that have been acknowledged
but not yet committed. Emptied at every commit that leaves nothing waiting.

**A snapshot** — an index's mapping, settings and documents written to a
repository as they were sent. Not a copy of the engine's files, which is why a
restore re-indexes and why a snapshot survives a change of format.

**A shard** — here, a number computed from a document's routing, used to decide
which writes a refresh makes visible and what `_shards` reports. It is not a
separate store and it does not live on another machine.
