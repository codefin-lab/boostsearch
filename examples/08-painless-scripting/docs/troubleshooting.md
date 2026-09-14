# Troubleshooting

## `No field found for [x] in mapping` in a script

`doc['x']` needs doc values. Text fields do not have them. If `x` is a `text`
field, read `x.keyword` (or whatever the sub-field is called), or read
`params._source.x` -- which works but parses the stored JSON per document and
is slow enough to notice.

## `cannot access method [add] on a null value`

The field does not exist on that document. `doc[...]` gives an empty list
(check `.size() == 0`); `ctx._source.x` gives `null`. The elvis operator
handles the write case:

```painless
ctx._source.trips = (ctx._source.trips ?: 0) + 1;
```

For the read case, guard explicitly:

```painless
doc['location'].size() == 0 ? -1 : doc['location'].arcDistance(...)
```

## `Variable [ctx] is not defined`

A search-time context was given a write-time script. `ctx` exists in `_update`,
`_update_by_query`, `_reindex` and ingest pipelines, and nowhere else. See the
table in `design.md`.

## `Variable [doc] is not defined`

The reverse: a write-time or ingest context was given a search-time script.

## `Too many dynamic script compilations within...`

The script cache is thrashing. Almost always caused by building values into the
script text instead of passing them as `params` -- see `design.md`. Find the
offending query by looking for scripts whose `source` contains a literal that
varies between requests.

## A `scripted_metric` returns a fraction of the right answer

The `reduce_script` is not merging the shards. `states` is a list, one entry
per shard, and each entry is whatever that shard's `combine_script` returned.
A reduce that does `return states[0]` works on one shard and is wrong on more.

This example's index has one shard, so a broken reduce *passes here*. Test it
on a multi-shard index before trusting it.

## `_update` returns `document_missing_exception`

The document is not there and there is no `upsert`. Step 9 shows the form that
creates it:

```json
{ "script": {...}, "upsert": { ...the document to create... } }
```

## Scores from `script_score` are all zero, or the query returns nothing

A score may not be negative. A script returning a negative number is an error
in some contexts and clamped in others; add a constant large enough to keep the
result positive, and subtract it when reading.

## `expression` scripts fail with anything but arithmetic

Lucene expressions are numeric only: no strings, no conditionals, no
`_source`. Fields are read through `doc['name'].value`; a bare field name is a
`link error`. If you need an `if`, it has to be Painless.

## Date arithmetic gives a `ClassCastException`

`doc['bought'].value` is a `ZonedDateTime`, not a long. To compare with a
string you must parse the string; to compare with a number you must call
`.toInstant().toEpochMilli()`.

## Cleaning up

```bash
make clean          # deletes fleet
curl -XDELETE localhost:9268/_scripts/depreciated-value
```
