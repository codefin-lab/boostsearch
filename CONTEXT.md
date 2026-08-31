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
