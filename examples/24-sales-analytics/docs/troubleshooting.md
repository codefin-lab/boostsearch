# Troubleshooting

## `no server at http://127.0.0.1:9200`

`run.sh` on its own defaults to port 9200. `make run` points it at 9284, where
`make serve` starts the node; running the script directly needs the address:

```bash
VS=http://127.0.0.1:9284 ./run.sh
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

The transform names each document by a hash of its key, so a rerun overwrites
rather than adds. Fewer than 240 means the run has not finished: ask what it is
doing,

```bash
curl -s localhost:9284/_plugins/_transform/sales-monthly/_explain | jq
```

and read `status` and `failure_reason`. `failed` with a reason about the
source index means `sales` was not there when the job ran.

## The transform or the rollup never finishes

A job runs on its schedule, and the first run of one written with a
`start_time` of now is a whole period away. `run.sh` sets the start to just
under a minute ago so that the first run is a second or two off. Written by
hand, either wait the minute or do the same. A job that has been stopped with
`_stop` does not run until `_start`.

## A search of `sales-daily-rollup` answers 400

A rollup index answers only what its job kept. The reason says which part of
the request it could not answer: `Rollup search must have size explicitly set
to 0`, `The top_hits aggregation is not currently supported in rollups`, or
`Could not find a rollup job that can answer this query because [missing field
channel]` for a field the job has no dimension or metric for. Search `sales`
for those.

## `strict_dynamic_mapping_exception` on the bulk

The mapping is `dynamic: strict`: a document with a field it does not name is
refused. That is on purpose -- see `design.md`. Add the field to
`requests/01-an-index-shaped-for-aggregation.json` if it belongs there.

## `bucket_sort` or `bucket_selector` complain about `buckets_path`

A path names a sibling aggregation inside the same bucket. `margin_pct` is a
`bucket_script` result, so it can be used by the selector and the sort that
follow it, but not by an aggregation outside `regions`.

## `top_metrics` answers 400

Not implemented on this node. `top_hits` with `size: 1` and a `sort` gives the
same answer with more around it (step 11).

## `top_hits` under `rare_terms` or `composite` answers 400

On this node `top_hits` inside those two aggregations needs a `sort` and then
refuses `_source`; without `_source` it answers with hits that carry no
document. Inside `terms` and `multi_terms` it works. Use a `min` or `max`
sub-aggregation for a single value (step 8 uses `sum` and `min`), or run a
second search filtered to the bucket's key.

## A median that is not the median

`percentiles` and `median_absolute_deviation` are estimates. See "Estimates" in
`design.md`; they are not bugs, and they do not agree across engines.

## Cleaning up

```bash
make clean          # deletes both jobs, sales, sales-monthly and sales-daily-rollup
rm -f /tmp/sales-24-buckets.ndjson
```
