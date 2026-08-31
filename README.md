# BoostSearch

An OpenSearch-compatible search server written in Rust, on top of
[BoostCore](https://github.com/codefin-lab/boostcore) (a fork of tantivy).

It speaks the OpenSearch REST API — the same requests, the same JSON back — and
is checked against OpenSearch's own conformance suite rather than against a
description of it.

## Where it stands

Run against the YAML tests in OpenSearch's `rest-api-spec`, all 409 files:

| | |
|---|---|
| sections passed | **1,426 of 1,427** (99.9%) |
| skipped | 77 (features the suite itself marks as not applicable) |
| not passing | `delete/50_refresh` — refresh reaching one shard at a time |

Measured against OpenSearch 3.1.0 in the same Docker VM, same corpus (200k
http-log documents), same query mix, five runs a side, median:

| | BoostSearch | OpenSearch 3.1.0 | |
|---|---:|---:|---|
| index docs/s | **102,366** | 72,704 | 1.41x |
| memory (MB) | **257** | 1,112 | 4.3x |
| qps (c=1) | **526** | 381 | 1.38x |
| p50 (c=1) | **1.84 ms** | 2.46 ms | 1.34x |
| qps (c=8) | **1,871** | 1,622 | 1.15x |

See `docs/opensearch-comparison.md` for how that was measured.

## Running it

```bash
cargo build --release
./target/release/boostsearch
```

It listens on `127.0.0.1:9200`. Two environment variables matter:

| | |
|---|---|
| `BOOSTSEARCH_ADDR` | where to listen (default `127.0.0.1:9200`) |
| `BOOSTSEARCH_DATA` | a directory to keep indices in, mmapped and surviving a restart; unset keeps everything in RAM |

## Running the conformance suite

The suite is OpenSearch's own, so it has to be fetched:

```bash
git clone --depth 1 https://github.com/opensearch-project/OpenSearch study/OpenSearch
```

Then, with the server running:

```bash
BOOSTSEARCH_NODE_ATTRS=testattr=test ./target/release/boostsearch &
python3 tools/yaml_runner.py --manifest tools/phase3_manifest.json
```

`BOOSTSEARCH_NODE_ATTRS` stands in for the `node.attr.testattr=test` the suite's
own cluster is started with. A full run takes about ten seconds.

## How it is built

- one tantivy — BoostCore — index per index, documents stored whole in one JSON
  field, in two views: tokenized for `match`, untouched for `term`
- aggregations BoostCore can parse run inside it; the rest are "peeled" off the
  request and computed a bucket at a time through the ordinary query path
- dates are numbers, the way OpenSearch stores them: milliseconds for a `date`,
  nanoseconds for a `date_nanos`
- shards are modelled, not distributed: routing hashes the way
  `Murmur3HashFunction` does, so a document lands on the shard OpenSearch would
  put it on

`docs/` carries the working notes, including what is left and why.

## Licence

Dual licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you say otherwise, any contribution you send in is
licensed the same way, with no further conditions.

BoostCore, the engine underneath, is MIT, as the tantivy it forked is.

This is not an OpenSearch product and is not endorsed by the OpenSearch
project; it implements the same HTTP API, and says so.

## BoostCore

The engine is a dependency, pinned to a commit of
[codefin-lab/boostcore](https://github.com/codefin-lab/boostcore).
`docs/boostcore.md` says what the fork changes and how to work on both at once.
