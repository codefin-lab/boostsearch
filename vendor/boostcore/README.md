# BoostCore

BoostCore is the search engine library [BoostSearch](https://github.com/codefin-lab/boostsearch)
is built on. It is a fork of [tantivy](https://github.com/quickwit-oss/tantivy)
**0.26.1**, kept as a fork so the parts of the engine an OpenSearch-compatible
server leans on can be fixed rather than worked around.

Everything tantivy does, BoostCore does. What follows is only what differs.

## What the fork changes

### Field norms for JSON fields, per path

A server that accepts arbitrary documents stores them in one JSON field.
Upstream records no field norm for a JSON field at all, so BM25 sees every
document as the same length and scores by term frequency alone — a long
address that repeats a word beats a short one that says it once, which is the
opposite of what Lucene does.

BoostCore records a norm per JSON **path**, which is what a field would be in a
flat schema:

- `src/schema/field_type.rs` — `has_fieldnorms()` answers what the JSON field's
  text options say, instead of `false`
- `src/core/json_utils.rs` — `IndexingPositionsPerPath::total_tokens()`
- `src/indexer/segment_writer.rs` — the `JsonObject` branch records the whole
  field's length and each path's own
- `src/fieldnorm/` — per-path norms are written beside the field's, at index 1
  of the same composite file, and read back by path
- `src/indexer/merger.rs` — they survive a merge
- `src/postings/serializer.rs` — the block maxima written at index time are
  computed against the same path's norms, so they stay an upper bound and
  pruning cannot drop a match

### The same number is one bucket

`IntermediateKey` hashed and compared by variant, so a `0` that one segment
stored as `i64` and another as `u64` — the first big value a segment sees
decides it — merged into two buckets of one instead of one bucket of two.
Numeric keys now compare, order and hash as numbers.

## Licence and attribution

MIT, as tantivy is. The copyright of the original work stays with the tantivy
authors listed in `AUTHORS`; see `LICENSE`. This fork carries their code with
the changes above.

## Upgrading

There is no upstream remote: the fork is a snapshot of tag `0.26.1`. To move to
a newer tantivy, clone it at the new tag, redo the rename (`tantivy` →
`boostcore` and each workspace member), and re-apply the changes above — each
is a few lines and carries a comment saying why it is there.
