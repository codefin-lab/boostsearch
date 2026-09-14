# Troubleshooting

## `has_child` returns nothing, and the children are definitely there

Routing. Parent and child must be on the same shard, and they are only there if
both writes carried the same `routing` value.

```bash
curl -s 'localhost:9271/_cat/shards/qa?v&h=index,shard,prirep,docs'
curl -s 'localhost:9271/qa/_doc/a1?routing=q1' | jq '._routing'
```

If `_routing` is absent on a child, it was written without it. There is no fix
but to rewrite it; a document cannot change shards in place.

This example uses a single shard, so routing cannot break it here. On a
multi-shard index it is the first thing to check.

## `[join] field can only have one relation`, or a mapping error on `join`

One `join` field per index, one `relations` object, all relations in it:

```json
"thread": { "type": "join", "relations": { "question": ["answer", "comment"] } }
```

A second join field is not allowed.

## Writing a child fails with `routing is required`

Expected and correct: the engine refuses rather than silently putting the child
on the wrong shard. Add `?routing=<parent id>`.

## `_update` on a child returns 404

The update must carry the routing too, or it looks on the wrong shard:

```bash
curl -XPOST 'localhost:9271/qa/_update/a2?routing=q1' -H 'content-type: application/json' \
  -d '{"doc":{"votes":3}}'
```

## `inner_hits` is empty on a `has_parent` query

`inner_hits` on `has_parent` returns the *parent*, and on `has_child` returns
the *children*. If it comes back empty, check which way round the query is.

## A `children` aggregation returns zero buckets

It must be nested inside an aggregation whose bucket contains parents, and the
`type` must name a child relation:

```json
"aggs": { "answers": { "children": { "type": "answer" }, "aggs": {...} } }
```

A `children` aggregation at the top level of a request with no query works, but
counts across the whole index rather than per parent bucket, which is usually
not what was meant.

## The counts look too high

Probably the unit-of-counting trap: inside a `children` block the buckets count
children, not parents. Use `parent` (or `reverse_nested` in the nested model)
to climb back out. See `design.md`.

## The nested update in step 9 does nothing

The script iterates `ctx._source.answers` and matches on `author`. If the
document's array field is named differently, or the author does not match, the
script runs successfully and changes nothing -- scripts do not error on a
loop that matches no element. Read the document back and check.

## `total_hits` differs between the join and nested indices

It should: they hold different numbers of documents. `qa` holds questions,
answers and comments as separate documents; `qa-nested` holds two documents
whose nested children are invisible to a normal count. That is the models
working as intended, not a discrepancy.

## Cleaning up

```bash
make clean          # deletes qa and qa-nested
```
