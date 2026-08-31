# BoostCore

BoostCore is the search library obsearch is built on. It is a fork of
[tantivy](https://github.com/quickwit-oss/tantivy) **0.26.1** (upstream tag
`0.26.1`), vendored at `vendor/boostcore` and depended on by path.

## Why it is vendored

obsearch has to behave the way OpenSearch behaves, and some of that is decided
below the query layer -- in how a segment is written and how a document is
scored. Those are one-line differences in the engine and unreachable from
outside it. Vendoring turns "we cannot do that" into a patch, and leaves room
to optimise the parts of the engine obsearch leans on hardest.

## What the fork changes

### The rename

The crate and its workspace members are renamed, so nothing in the tree is
called tantivy any more:

| upstream | here |
| --- | --- |
| `tantivy` | `boostcore` |
| `tantivy-bitpacker` | `boostcore-bitpacker` |
| `tantivy-columnar` | `boostcore-columnar` |
| `tantivy-common` | `boostcore-common` |
| `tantivy-query-grammar` | `boostcore-query-grammar` |
| `tantivy-sstable` | `boostcore-sstable` |
| `tantivy-stacker` | `boostcore-stacker` |
| `tantivy-tokenizer-api` | `boostcore-tokenizer-api` |

`tantivy-fst` is still used from crates.io, aliased to `boostcore-fst`.

The upstream benchmark corpora (`hdfs.json`, `gh.json`, `wiki.json`,
`alice.txt` -- 24 MB) and the `.git` directory were dropped.

### Field norms for JSON fields

obsearch stores every document in one JSON field (`_dyn`, tokenized, and
`_raw`, not). Upstream never records a field norm for a JSON field, so BM25
sees every document as the same length and scores by term frequency alone.
Ordering then disagrees with OpenSearch wherever length is what separates two
hits -- a short address beating a long one that repeats the term.

Three changes fix that:

- `src/schema/field_type.rs` -- `has_fieldnorms()` returns what the JSON
  field's text options say, instead of `false`.
- `src/core/json_utils.rs` -- `IndexingPositionsPerPath::total_tokens()`, the
  number of tokens indexed across every path of one JSON field.
- `src/indexer/segment_writer.rs` -- the `JsonObject` branch records that count
  as the document's field norm, as the text branch already does.

The length is the whole JSON field, which for obsearch is the whole document.
Lucene's norm is per field, so a document whose other fields are long still
scores lower here than it would there; per-path norms are the next step.

## Upgrading

There is no upstream remote: the fork is a snapshot. To move to a newer
tantivy, clone it at the new tag, redo the rename, and re-apply the patches
above -- each one is a few lines and carries a comment saying why it is there.
