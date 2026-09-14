# Troubleshooting

## Step 10 shows only `logs-000001`, and the policy never moves

The index-management job runs on an interval, and the default is minutes. This
example's policy is written in seconds, so it needs a node started with a short
interval:

```bash
make serve          # node.sh sets BOOSTSEARCH_ISM_INTERVAL_MS=2000 for you
```

If you are using your own node, start it with that variable, or wait several
minutes and ask again:

```bash
curl -s 'localhost:9262/_plugins/_ism/explain/logs-000001' | jq .
```

## `_plugins/_ism/...` returns 404

Index management is not answered by this node. Check what it does answer for:

```bash
curl -s 'localhost:9262/_cat/plugins?v'
```

The rest of the example -- everything from step 4 to step 9 -- does not need
it and still runs.

## The rollover happened but `logs-000002` has a different mapping

The template did not match. `index_patterns: ["logs-*"]` must cover the name
the rollover generates, and the template must exist *before* the index is
created. Check:

```bash
curl -s localhost:9262/_index_template/logs-template | jq .
curl -s localhost:9262/logs-000002/_mapping | jq .
```

If `@timestamp` came out as `text` rather than `date`, the template was not
applied and the aggregations in steps 5 to 8 will fail on that index.

## A date histogram returns `illegal_argument_exception`

The field is not a date. This follows from the problem above; it is also what
happens if the bulk in step 4 ran against an index created by hand without the
template.

## `bucket_selector` or `bucket_script` returns `buckets_path` errors

The path names a sibling aggregation by the name it was given, and `_count`
is a special path meaning the bucket's own document count. A path that names
something that is not there is an error rather than a zero, on purpose.

```
"buckets_path": { "bad": "errors>_count", "all": "_count" }
                          ^^^^^^ the filter agg's name, then its count
```

## Step 4 takes a long time

It writes 1,200 documents through a bulk, with `refresh=true`. That is a
refresh per bulk, not per document, so it should be a second or two. If it is
much slower the node is sharing the machine with something -- check nothing
else is running against it.

## Cleaning up

The policy deletes its own indices, so a complete run leaves only the template
and the policy:

```bash
make clean
curl -XDELETE localhost:9262/_index_template/logs-template
curl -XDELETE localhost:9262/_plugins/_ism/policies/logs-lifecycle
```
