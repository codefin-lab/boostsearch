# Troubleshooting

## `PUT /_data_stream/{name}` answers "no matching index template found"

```
no matching index template found for data stream [metrics-node-cpu]
```

A stream can only be made from a composable index template whose
`index_patterns` match the name **and** that carries `data_stream: {}`. A
template without `data_stream` makes plain indices, and a legacy `_template`
never makes a stream. Check which template the name reaches:

```bash
curl -s -XPOST localhost:9278/_index_template/_simulate_index/metrics-node-cpu | jq
```

## A write by id is refused

```
only write ops with an op_type of create are allowed in data streams
```

Expected (step 13). A stream only appends. In `_bulk` use `create` rather than
`index`; with `_doc`, use `POST /{stream}/_doc` for a generated id or
`PUT /{stream}/_create/{id}` for one of your own. Documents that must be updated
in place do not belong in a stream.

## A template is refused at the same priority

```
index template [metrics-rival] has index patterns [metrics-*] matching patterns
from existing templates [metrics] ... that have the same priority [100]
```

Expected (step 7). Two composable templates whose patterns overlap must have
different priorities. Give the more specific pattern the higher number.

## A template cannot be deleted

```
unable to remove composable templates [metrics-edge] as they are in use by a data streams [metrics-edge-cpu]
```

The template a stream was made from is read again at every rollover, so it
cannot go while the stream exists. Delete the stream first, then the template
-- the order `run.sh` uses in its first step.

## The write index cannot be deleted

```
index [.ds-metrics-node-cpu-000002] is the write index for data stream [metrics-node-cpu] and cannot be deleted
```

Roll the stream over first; the old generation can then be deleted. To remove
everything, delete the stream itself.

## `_rollover` answers `rolled_over: false`

The request had `conditions` and none of them held (step 14). A document-count
condition is measured on what has been refreshed, so writes made a moment ago
without `refresh` may not count yet. Send the request with no body to roll over
unconditionally.

## A document is refused: "documents must contain a single-valued timestamp field"

```
mapper_parsing_exception: failed to parse
  caused by: documents must contain a single-valued timestamp field '@timestamp' of date type
```

Expected (step 10). Every document in a stream needs exactly one `@timestamp`
that parses as a date. Check the shipper: a field named `timestamp` or `time`
is not the same field, an array of two times is refused, and a string in a
format the `date` mapping does not accept fails the same way. To use another
field, name it in the template: `"data_stream": { "timestamp_field": { "name":
"event_time" } }`.

In a `_bulk` the refusal is per item and the request as a whole answers 200
with `"errors": true`. Read the items, not only the status code.

## Writes to a new name make a plain index, not a stream

If `POST /metrics-something/_doc` answers with `"_index": "metrics-something"`
rather than a `.ds-` name, no template with `data_stream` matched the name when
the write arrived: the pattern is wrong, the template lacks `data_stream: {}`,
or it was added after an index of that name already existed. Simulate the name
to see which template it reaches, delete the plain index, and write again.

## The backing index does not have the fields you expected

Compare the simulation with what the stream's index actually has:

```bash
curl -s -XPOST localhost:9278/_index_template/_simulate_index/metrics-node-cpu | jq .template.mappings
curl -s localhost:9278/.ds-metrics-node-cpu-000001/_mapping | jq
```

If they differ, the template changed after the backing index was made. A
template is read when a backing index is created -- at the first write and at
each rollover -- and never applied to an existing generation. Roll the stream
over to get a generation with the new mapping. If `host` shows as `text` with a
`keyword` sub-field in both, the field is missing from `metrics-mappings` and
was mapped dynamically; a `terms` aggregation on `host` is then refused.

## A search finds documents from only one generation

Search the stream's name, not a backing index:

```bash
curl -s localhost:9278/metrics-node-cpu/_count          # both generations
curl -s localhost:9278/.ds-metrics-node-cpu-000002/_count   # only one
```

`_cat/indices` without `expand_wildcards=all` may not list `.ds-` indices at
all, since backing indices are hidden.

## Cleaning up

```bash
make clean          # streams, the legacy indices, and all four templates
```
