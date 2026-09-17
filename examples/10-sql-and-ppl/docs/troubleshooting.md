# Troubleshooting

## `/_plugins/_sql` returns 404

`VS` is pointing at a server without the SQL plugin, or through a proxy that
does not pass `_plugins/` paths on. Check that `opensearch-sql` is listed by
the server the example is talking to:

```bash
curl -s 'localhost:9270/_cat/plugins?v'
```

The same plugin answers `/_plugins/_ppl`, so the PPL steps fail the same way.

## `SemanticCheckException` or "can't resolve Symbol"

The column does not exist, or exists with a different type than the function
wants. Two things catch people:

- a `text` field cannot be grouped on or sorted by; it needs a `keyword`
  sub-field, and SQL will name the field you asked for rather than the
  sub-field it needed;
- an aggregate over a field the mapping made a `text` fails the same way.

Check the mapping:

```bash
curl -s localhost:9270/orders/_mapping | jq '.orders.mappings.properties'
```

## `GROUP BY` returns fewer groups than expected

A `terms` aggregation has a default `size` and SQL does not always raise it.
On a high-cardinality column the tail is silently missing. Verify with
`_explain` and, if it matters, ask the question as JSON with an explicit
`size`.

## `HAVING` is ignored, or errors

`HAVING` becomes a `bucket_selector`, which runs after the buckets are
collected. It can only refer to aggregates, not to raw columns -- `HAVING
country = 'TH'` is a `WHERE`, not a `HAVING`, and the error message is not
always clear about which one you wrote.

## A PPL pipeline returns "unknown command"

The verbs are a fixed set: `source`, `where`, `fields`, `rename`, `eval`,
`stats`, `sort`, `head`, `dedup`, `top`, `rare`, and a few more. Step 15 runs
`frobnicate` on purpose to show what the refusal looks like.

## `format=csv` output has the wrong number of columns

CSV quotes fields containing commas. A `note` field with a comma in it is
quoted, which is correct CSV and looks wrong in a terminal. Pipe it through a
CSV reader, or use `raw` (tab separated) if the data has no tabs.

## `format=jdbc` and `format=table` return the same thing

They should not. If they do, the `format` parameter is not reaching the server
-- check it is in the query string (`/_plugins/_sql?format=table`), not in the
body.

## The example's `sql`/`ppl` helper mangles a query with quotes

The helper builds the JSON body with `python3 -c json.dumps`, precisely so a
query containing quotes or newlines survives. If you add a query by hand, use
the helper rather than interpolating into a JSON string.

## Numbers disagree with the same question asked as JSON

Likely `cardinality` (approximate) or `percentile` (t-digest). See
`design.md`. Neither front end warns.

## Cleaning up

```bash
make clean          # deletes orders
```
