# Design notes

## Where the parsing belongs

A log line can be turned into a document in three places:

| Where | Cost | Problem |
|---|---|---|
| the shipper | client CPU, config on every host | a pattern change is a fleet deployment |
| a stream processor | another system to run | another system to run |
| an ingest pipeline | indexing-node CPU | the node does the work |

The pipeline wins on operations: the pattern lives in one place, changing it is
a `PUT`, and the shipper stays a program that reads files and posts strings.
The cost is that parsing competes with indexing for the same CPU, which is why
production clusters often give ingest its own nodes (`node.roles: [ingest]`).

## `on_failure` at three levels, doing three things

```
geoip       on_failure -> set client.geo.located = false     substitute a value
user_agent  ignore_missing: true                             tolerate absence
pipeline    on_failure -> set _index = failed-lines          redirect the document
```

This is the whole design of the example. Without any of them, one unparseable
line fails the bulk item and -- depending on the client -- either the line is
dropped or the shipper retries it forever.

The pipeline-level `on_failure` rewriting `_index` is the important one. It is
a dead-letter index without a dead-letter queue: the document is written, with
`error.reason` and `error.processor` attached, into somewhere a person can go
and look. Every logging setup needs this and most discover they need it after
losing a day of data.

## Why `_simulate` comes first

Step 1 runs the pipeline against two documents with no index in sight, and the
second document fails on purpose. This is the loop to develop a pipeline in:
the alternative is writing documents, finding them wrong, deleting the index
and starting again, with a mapping in the way.

`_simulate` also takes `verbose: true`, which returns the document after each
processor in turn -- the equivalent of stepping through.

## `grok` against `dissect`

Both appear in the pipeline, doing different jobs.

- `grok` parses the combined log line. It is a regular expression underneath,
  with named patterns, and it handles the variable shape of a log line: quoted
  fields, a dash where a user name should be, a timestamp with a timezone.
- `dissect` splits `url.path` on a fixed pattern. It is not a regular
  expression -- it is a positional split -- so it is much faster and cannot
  backtrack.

The rule: `dissect` when the delimiters are fixed, `grok` when they are not.
A pipeline that groks a path it could dissect is spending regex time per
document per request for no reason.

## Why the mapping and the pipeline must agree

`convert` makes `http.version` a float; the mapping declares it `float`. Either
alone would half-work: a pipeline that converts into an index with no mapping
gets dynamic typing from the first document seen, and a mapping without a
convert gets a string that fails to coerce.

The pipeline's `script` processor computes `outcome` and `http.kb`, neither of
which is in the input. These are the fields worth aggregating on (step 7), and
computing them once at write time is cheaper than computing them on every read.

## Ingest pipelines against search pipelines

Step 9 adds a search pipeline, which is the mirror image:

```
ingest pipeline    changes the document on its way IN
search pipeline    changes the request or the answer on its way OUT
```

A search pipeline's `response_processors` can rename or remove fields without
reindexing, which makes it the right tool for a client that wants a different
field name and the wrong tool for anything that should have been done at write
time -- it pays its cost on every query.

## What would change at scale

- **`geoip` holds its database in memory** per ingest node. One database is
  fine; a processor chain loading three of them on every node is worth
  measuring.
- **A `script` processor compiles once and caches**, as long as the source text
  does not change. A script built by string interpolation per document
  recompiles per document.
- **`index.default_pipeline`** means every write pays for the pipeline,
  including a `_update_by_query` that only wanted to set one field. `_reindex`
  can name a different pipeline, or none.
