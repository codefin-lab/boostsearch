# BoostSearch

[![ci](https://github.com/codefin-lab/boostsearch/actions/workflows/ci.yml/badge.svg)](https://github.com/codefin-lab/boostsearch/actions/workflows/ci.yml)

A drop-in replacement for OpenSearch, written in Rust, on top of
[BoostCore](https://github.com/codefin-lab/boostcore) (a fork of tantivy).

It speaks the OpenSearch REST API — the same requests, the same JSON back, the
same words in its errors — and is checked against OpenSearch's own conformance
suite and against a running OpenSearch, rather than against a description of
either.

## Where it stands

Everything below is produced by a script in `tools/`, so it can be checked
rather than believed.

| | | how |
|---|---|---|
| OpenSearch's core suite | **1,100 of 1,100** not skipped (75 skipped) | `tools/yaml_runner.py --manifest tools/phase3_manifest.json` |
| its module and plugin suites | **880 of 890**, 4 skipped | `tools/module_gate.py` |
| the same answer as OpenSearch 3.1.0 | **160 of 183** canonical requests, byte for byte | `tools/compat_audit.py replay` |
| REST endpoints answered | **156 of 167** | the rest answer 501 rather than pretending |
| the bench matrix | **17 of 18 dimensions ahead** | `tools/bench_matrix.py` |
| who may reach what | **1,167 answers** over 239 routes and five callers | `tools/auth_matrix.py` |
| a refused write leaves the document alone | **30 refusals** through five write paths | `tools/refusal_check.py` |
| every acknowledged write survives `kill -9` | **10,001 writes**, five index shapes | `tools/restart_check.py` |
| malformed input at everything that parses | **2,000 probes**, node still answering | `tools/fuzz_check.py` |
| OpenSearch Dashboards' own API suite, against the console's server | **146 of 166**, none failed that the Node server passes (it scores 140) | `tools/dashboards_gate.py` |

The ten sections that do not pass are named, with the reason, in
`docs/progress.md`; five more are set aside as tests of the test framework
rather than of a server, and `tools/module_gate.py` prints those and why on
every run.

## What it does

| | |
|---|---|
| **Search** | every query the suite names, aggregations, sorting, highlighting, collapse, nested and parent-join, point-in-time, search templates, `rank_eval`, suggesters, profiling |
| **Writing** | documents, bulk, update, `_update_by_query`, `_delete_by_query`, `_reindex` including from another cluster over HTTP |
| **Analysis** | the built-in analyzers token for token, ICU, Japanese, Korean and Chinese by dictionary, phonetic and phone-number filters, Thai segmentation |
| **Scripting** | Painless — lexer, parser and evaluator — in every context the suite uses, plus Lucene expressions and Mustache |
| **Ingest** | the thirty processors the corpus names, grok and dissect, geoip, user-agent, attachment extraction, and search pipelines |
| **Cluster** | consensus, allocation, replication, peer recovery, cross-node search; checked in a seeded simulation and against real nodes with real partitions |
| **Security** | TLS, users and roles, API keys, document- and field-level security inside the query rather than in front of it, SAML, OIDC, LDAP, the audit log |
| **Snapshots** | filesystem, URL, S3, Google Cloud Storage and Azure repositories |
| **Index management** | ISM policies, transitions, rollover, snapshot management |
| **Vector search** | six distance spaces, exact and HNSW, filtered search, the k-NN API |
| **SQL and PPL** | both languages, in jdbc, json, csv, raw and table shapes |

`_cat/plugins` lists what it answers for, because a client asking whether it
may use `icu_tokenizer` deserves a true answer.

## Running it

```bash
cargo build --release
./target/release/boostsearch
```

It listens on `127.0.0.1:9200`. In Docker:

```bash
docker build -t boostsearch .
docker run -p 9200:9200 -v boostsearch-data:/var/lib/boostsearch boostsearch
```

The settings that matter most:

| | |
|---|---|
| `BOOSTSEARCH_ADDR` | where to listen (default `127.0.0.1:9200`) |
| `BOOSTSEARCH_DATA` | where indices live, mmapped and surviving a restart; unset keeps everything in RAM |
| `BOOSTSEARCH_CONFIG` | where `boostsearch.yml` and the plugins' data directories live |
| `BOOSTSEARCH_PATH_REPO` | where filesystem snapshot repositories may live (default `<data>/repo`) |

Everything else is a setting in `config/boostsearch.yml`, spelled the way
OpenSearch spells it, and readable from the environment as
`BOOSTSEARCH_` + the dotted name upper-cased. `docs/settings.md` lists them.

## The console

OpenSearch Dashboards is two things: a browser application and a Node server
it boots from. BoostSearch replaces the server and leaves the application as
it is -- the same bundles, served from a Dashboards distribution you point it
at, talking to a BoostSearch (or OpenSearch) engine:

```bash
BOOSTSEARCH_CONSOLE_PATH=/usr/share/opensearch-dashboards \
BOOSTSEARCH_ENGINE=http://127.0.0.1:9200 \
./target/release/console
```

It listens on `127.0.0.1:5601`. Discover, Visualize, dashboards, saved
objects and their migrations, the sample data, index patterns, the Dev Tools
and the Index Management page work through it; it starts in 45 ms and holds
14 MiB against the Node server's several hundred. `docs/console.md` says how
it is built and what it does not carry.

## The dictionaries

Japanese, Korean and Chinese are read with a dictionary rather than split on
spaces, and those dictionaries are built into the binary the way OpenSearch's
kuromoji, nori and smartcn plugins carry theirs. They are most of what the
binary weighs — 188 MB with them, 20 MB without:

```bash
cargo build --release --no-default-features
```

A build without them answers everything else the same way; the three analyzers
that need them find no words.

Three more sets of data are **not** vendored, because they are somebody else's
to redistribute: the MaxMind GeoLite2 databases (`docs/geoip.md`), the
Beider-Morse rule files for the phonetic filter (`docs/phonetic.md`), and the
Polish and Ukrainian stemmers' dictionaries. Without them those filters say so
rather than guessing.

## Replacing an OpenSearch you already run

`docs/upgrading.md` is the procedure. In short:

```bash
# what your cluster actually uses, and whether this answers all of it
python3 tools/compat_audit.py inventory --cluster $OPENSEARCH --engine $BOOSTSEARCH

# the same requests to both, compared answer by answer
python3 tools/compat_audit.py corpus
python3 tools/compat_audit.py replay --requests compat-corpus.ndjson \
    --a $OPENSEARCH --b $BOOSTSEARCH --scores
```

The first says whether anything your indices use is unanswered. The second
asks both engines the same 183 requests and diffs the JSON.

## Running the conformance suite

The suite is OpenSearch's own, so it has to be fetched:

```bash
git clone --depth 1 https://github.com/opensearch-project/OpenSearch study/OpenSearch
```

Then start the node the suites expect and run them:

```bash
tools/gate_node.sh &
python3 tools/yaml_runner.py --url http://127.0.0.1:9213 --manifest tools/phase3_manifest.json
```

`tools/gate_node.sh` is the one way to start it: the suites read back a node
attribute, the geoip databases, the phonetic rules, where a URL repository may
be read from and which clusters a reindex may read from, and a node started
without those fails sections that have nothing wrong with them.

The module suites need a second node with no ingest role, because one of them
is written against a cluster that has none — `tools/module_gate.py` runs both
passes and adds them up.

## What it is not

- **Not an OpenSearch product**, and not endorsed by the OpenSearch project. It
  implements the same HTTP API and says so.
- **Not the console's front end.** The browser application is OpenSearch
  Dashboards' own, served unchanged from a distribution; only the server
  behind it is this project's. The other plugins' server halves -- alerting,
  anomaly detection, observability -- are not written, and their pages say so.
- **Not tested at every scale.** The cluster is checked in simulation across
  ten thousand seeds and on real nodes with real partitions, and the bench
  matrix is measured on a developer machine. `tools/cloud_bench_gcp.sh` runs
  it on a machine rented for the run; the numbers from one are not in yet, and
  `docs/progress.md` says so.

## How it is built

- one BoostCore index per index, documents stored whole and written into views:
  tokenized for `match`, untouched for `term`, and a third for `fielddata`
- aggregations BoostCore can parse run inside it; the rest are peeled off the
  request and computed a bucket at a time through the ordinary query path
- dates are numbers, the way OpenSearch stores them: milliseconds for a `date`,
  nanoseconds for a `date_nanos`
- routing hashes the way `Murmur3HashFunction` does, so a document lands on the
  shard OpenSearch would put it on

`docs/adr/` records the decisions that were hard to reverse and why. `docs/progress.md`
is the working ledger: every task, what it took, and what was got wrong on the
way.

## The documents

| | |
|---|---|
| [docs/plan-v1.md](docs/plan-v1.md) | the work, in the order it was done, and the gate each phase was measured by |
| [docs/progress.md](docs/progress.md) | the ledger: every task, what it took, what was got wrong on the way |
| [docs/settings.md](docs/settings.md) | every setting, the server's and the console's |
| [docs/upgrading.md](docs/upgrading.md) | replacing an OpenSearch you run, and moving between versions of this |
| [docs/console.md](docs/console.md) | the console's server: what it serves, what it pins, what it leaves out |
| [docs/geoip.md](docs/geoip.md), [docs/phonetic.md](docs/phonetic.md) | the two processors that read data this does not ship |
| [docs/boostcore.md](docs/boostcore.md) | what was changed in the fork of tantivy, and why |
| [docs/adr/](docs/adr/) | the seven decisions that were hard to reverse |
| [CONTRIBUTING.md](CONTRIBUTING.md) | where things are, and what to run before you push |
| [CONTEXT.md](CONTEXT.md) | what the words mean |

## Licence

Dual licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you say otherwise, any contribution you send in is
licensed the same way, with no further conditions.

BoostCore, the engine underneath, is MIT, as the tantivy it forked is. The
Snowball stemmers for Catalan, Basque, Irish, Lithuanian, Estonian and
Armenian, and the original Porter algorithm, are generated from the Snowball
project's own definitions by its compiler and used under the BSD 3-clause
licence in `LICENSE-SNOWBALL`.
