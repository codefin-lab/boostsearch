# Design notes

## The decision, in one table

|  | `nested` | `join` |
|---|---|---|
| Storage | children are hidden Lucene docs in the parent's block | children are ordinary documents |
| Update one child | rewrites the parent and every sibling | rewrites one small document |
| Query cost | low: one block, no lookup | higher: a relation to resolve, a global ordinals map |
| Shard placement | automatic (same document) | **manual**: routing must be set |
| Children searchable alone | no | yes |
| Aggregation across children | `nested` / `reverse_nested` | `children` / `parent` |

The short version: **`nested` if children are written with the parent and
rarely change; `join` if children arrive and change independently.**

A product with three sizes is `nested`. A question with answers people are
still posting is `join`. An order with line items is `nested`. A user with an
event stream is neither -- it is a separate index.

## Why routing is not optional

```
POST /qa/_bulk?refresh=wait_for&routing=q1
```

A join relation is resolved within a shard. Parent and child on different
shards are simply unrelated as far as `has_child` is concerned -- no error, no
warning, just no results.

Routing forces both to the shard the parent's id hashes to. Every write, every
`_update`, and every `parent_id` query on a child has to carry it. This example
uses one shard, so it would have worked without routing, and the routing is
written anyway because on any real index it is mandatory and omitting it
produces a bug that only appears at scale.

## `score_mode` on `has_child`, and why the default is misleading

```json
"has_child": { "type": "answer", "query": {...}, "score_mode": "max" }
```

The default is `none`: the parent matches, and its score is unaffected by how
well the child matched. For a "questions with a good answer about X" search
that is wrong -- every matching question ties.

`score_mode` options are `none`, `min`, `max`, `sum`, `avg`. `max` says "score
this question by its best answer", which is what a search over a Q&A site
wants. The equivalent on `nested` is also `score_mode`, with the same meaning
and a different default set.

## `children` and `parent` aggregations: which unit is being counted

Step 6 goes down and comes back:

```
terms on questions
  children(answer)          now counting ANSWERS
    max(votes)              the best answer's votes
    terms(author)           answer authors
    parent(answer)          back to QUESTIONS
      terms(tags)           question tags
```

Getting this wrong produces numbers that are plausible and false: a "questions
per tag" chart that is really "answers per tag", inflated by however many
answers the popular questions attracted. The same trap exists in `nested` and
`reverse_nested` (example 3), and it is the most common source of a wrong
dashboard number in either model.

## The update cost, made concrete

Step 9 does the same edit both ways.

```
join:    POST /qa/_update/a2?routing=q1     {"doc": {"votes": 3}}
nested:  POST /qa-nested/_update/n1         script over the whole answers array
```

The join version writes one small document. The nested version reads the
parent, runs a script over its array, and writes the parent and all its
children back as one block.

With two answers that difference is noise. With two thousand answers, and a
vote arriving every few seconds, it is the entire performance story: every vote
rewrites a large document, and the segment churn that produces is what
eventually makes people rebuild the index as a join.

## Parents with no children

```json
"must_not": [{ "has_child": { "type": "answer", "query": { "match_all": {} } } }]
```

There is no `nested` equivalent that is as direct. With nested you check the
array's existence or its length, which works but means the empty case and the
missing case have to be handled separately.

## What would change at scale

- **`join` builds global ordinals** for the relation, per shard, refreshed
  after writes. On a write-heavy index that is a real background cost, and it
  is the main reason not to reach for `join` by default.
- **One `join` field per index**, with all relations declared in it. Two
  independent hierarchies means two indices.
- **`nested` multiplies the document count.** Index size estimates must use
  parents + children, and `index.mapping.nested_objects.limit` exists to stop
  one document producing ten thousand.
