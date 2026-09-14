# Troubleshooting

## `no server at http://127.0.0.1:9200`

`run.sh` on its own defaults to port 9200. `make run` points it at 9284, where
`make serve` starts the node; running the script directly needs the address:

```bash
BS=http://127.0.0.1:9284 ./run.sh
```

or a `.env` copied from `.env.example`.

## `NOT WHAT WAS EXPECTED` on a number

The expected numbers are counted from `data/01-a-year-of-sales-lines-one.ndjson`.
First make sure that file is the one the numbers came from:

```bash
make check          # "data: matches its generator"
```

If `make data` was run with a different Python and the file changed, restore
it from version control. If the file is right and the engine's answer differs,
count the number yourself before believing either:

```bash
python3 -c '
import json
d = [json.loads(l) for i, l in enumerate(open("data/01-a-year-of-sales-lines-one.ndjson")) if i % 2]
print(len(d), round(sum(x["revenue"] for x in d), 2))'
```

## Step 3 loops, or stops after one page

The loop sends the previous page's `after_key` back as `after`. It stops on an
empty page, or on a page with no `after_key`. If it never stops, the server is
returning the same `after_key` twice; the script gives up after 50 pages
rather than spin. Ask one page by hand and compare its first key with the
`after` you sent:

```bash
curl -s localhost:9284/sales/_search -H 'content-type: application/json' \
  --data-binary @requests/02-every-region-category-and-month-a.json | jq '.aggregations.lines.after_key'
```

## `sales-monthly` holds a different number of documents from 240

The summary ids are `region|category|month`, so a rerun overwrites rather than
adds. More than 240 means the index was not deleted and the id scheme changed;
fewer means a composite page was lost. `make clean` and run again.

## `strict_dynamic_mapping_exception` on the bulk

The mapping is `dynamic: strict`: a document with a field it does not name is
refused. That is on purpose -- see `design.md`. Add the field to
`requests/01-an-index-shaped-for-aggregation.json` if it belongs there.

## `bucket_sort` or `bucket_selector` complain about `buckets_path`

A path names a sibling aggregation inside the same bucket. `margin_pct` is a
`bucket_script` result, so it can be used by the selector and the sort that
follow it, but not by an aggregation outside `regions`.

## `_plugins/_transform` or `_plugins/_rollup` answers 501

Not implemented on this node. Step 4 builds the same summary by paging a
composite and writing the buckets back; `design.md` says what a transform job
would add.

## `top_metrics` answers 400

Not implemented on this node. `top_hits` with `size: 1` and a `sort` gives the
same answer with more around it (step 10).

## `top_hits` under `rare_terms` or `composite` answers 400

On this node `top_hits` inside those two aggregations needs a `sort` and then
refuses `_source`; without `_source` it answers with hits that carry no
document. Inside `terms` and `multi_terms` it works. Use a `min` or `max`
sub-aggregation for a single value (step 7 uses `sum` and `min`), or run a
second search filtered to the bucket's key.

## A median that is not the median

`percentiles` and `median_absolute_deviation` are estimates. See "Estimates" in
`design.md`; they are not bugs, and they do not agree across engines.

## Cleaning up

```bash
make clean          # deletes sales and sales-monthly
rm -f /tmp/sales-24-buckets.ndjson /tmp/sales-24-monthly.ndjson
```
