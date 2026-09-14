# Design notes

## Why the data is generated, and why the output is committed anyway

A sales report is only worth reading if its numbers can be checked. Real sales
data cannot be published, and hand-written data is too small for `rare_terms`
or `auto_date_histogram` to mean anything. So `data/make-sales.py` writes
3,004 lines from a fixed seed, with shapes put there on purpose:

| Shape in the data | The step that finds it |
|---|---|
| north is the busiest region, central the smallest | 3, 5, 7 |
| central and south discount deeply, west hardly at all | 13, 15 |
| November and December sell more | 5, 9, 17 |
| three products sold once or twice all year | 8 |
| one sale in five was never rated; most used no coupon | 16 |
| a few orders are for 10 or 20 units | 12, 14 |

The output is committed as well as the generator, for two reasons. The run
should not depend on the Python on the machine producing the same floats, and
`make check` can then prove the two still agree. If they ever do not, the file
is what the README's numbers were counted from.

The expected values in `run.sh` were counted from that file by a separate
Python script, not read off a run. That is the difference between a check and a
recording: a recording of a wrong answer passes forever.

## The mapping is strict, and has no text

Every field is a `keyword`, a number or a `date`, and `dynamic: strict` refuses
anything else. Aggregations read doc values; a `text` field has none, and a
dynamically mapped string becomes `text` with a `.keyword` beside it, which
doubles the index for a field nobody will search as words. A report index
should say exactly what it holds.

`revenue`, `cost` and `unit_price` are `double`. A real ledger would store
integer cents (`long`) or `scaled_float` with `scaling_factor: 100`, so that
sums do not pick up the `...99999999` tails visible in the answers here.
Rounding the output is fine for a report; it is not fine for accounts.

## `composite` is the export, `terms` is the page

A `terms` aggregation answers "the top N". Asked for everything, it needs a
`size` larger than the number of buckets, which you do not know in advance, and
it builds the whole answer in memory in one go. On three dimensions with high
cardinality that is how a node runs out of heap.

`composite` answers "the next N, in key order", with `after_key` as the
cursor. Step 3 asks for 100 at a time and follows the cursor until a page
comes back empty. The checks prove the property that matters: 240 buckets, 240
distinct keys, and the counts add back up to every sale. A missing or
repeated page would show as a wrong total.

The trade: a `composite` cannot be ordered by a metric. It pages in key order
only. "Top five pairs by revenue" is a `terms` or `multi_terms` question (step
7); "all of them" is a `composite` question.

## A summary index kept by a transform, and a rollup beside it

Step 4 is a transform. It runs the composite aggregation of step 3 in pages,
turns each bucket into a document -- the group keys under the names the
transform gives them, and each aggregation's result under its own name -- and
writes it into `sales-monthly` under an id hashed from the key. The id is what
makes the job safe to run again: 240 buckets are 240 documents however many
times it runs. A plain transform runs once and turns itself off; a
`continuous` one would run on every tick of its schedule and recompute only the
groups that new sales fall into.

The summary index is made before the transform runs, with a strict mapping of
its own: `month` is a date (the transform writes the bucket key in
milliseconds), and the `transform._id` and `transform._doc_count` fields the
transform adds to every document are named. Left to make the index itself, the
transform would map `month` as a number, and step 5's `date_histogram` over it
would be refused.

Step 5 is the reason to bother. The same request against 240 documents gives
the same regional totals as against 3,004. At a year of real sales the ratio
is closer to a million to one, and a dashboard that reads the summary stays
fast however long the history gets. What is given up is detail: the summary
cannot answer "the biggest single order", because no single order is in it.

Step 6 is the other shape of the same idea. A rollup keeps a fixed set of
metrics -- a sum, a minimum, a maximum, a count, and an average kept as its sum
and count so that averages can be combined -- and its index is searched with
the request the raw index would be sent. The search is rewritten against the
rollup documents, and a bucket counts the sales its documents stand for. The
difference from a transform is in what the reader has to know: a transform's
summary is an index of its own, with its own field names, while a rollup is
asked the question the raw data is asked. The price is that a rollup answers
only what its dimensions and metrics can: a `terms` on a field that is not a
dimension, a `top_hits`, or any `size` above 0 is refused, in so many words.

## `filters` buckets overlap; `range` buckets do not

`range` (step 10) puts each document in at most one bucket, because the ranges
are contiguous and `from` is inclusive, `to` exclusive. `filters` (step 14)
tests every filter against every document independently. Nine sales are both
bulk and deeply discounted and are counted in both buckets, so the buckets sum
to 3,013. `other_bucket_key` collects only what matched none of them.

That is correct behaviour and a common source of reports that do not add up.
If the buckets must partition the data, order the filters by precedence and
exclude earlier ones in each later filter.

## `global` and the query

Aggregations run over what the query matched. `global` is the one exception:
its sub-aggregations see every document in the index, whatever the query said.
Step 15 uses it to put central's numbers and the company's in one answer, which
is what a "this region against the average" panel needs.

`global` ignores the query, but not the index: a search across several indices
gets the global of all of them.

## `missing` is a decision about meaning

A sale with no `rating` has no value, not a value of zero. Left alone, `avg`
skips it (3.96). With `missing: 1` it counts as the worst possible rating
(3.40). Both are defensible: the first describes the customers who answered;
the second is a pessimist's view of the ones who did not. The engine cannot
choose, so the request has to.

The same parameter on `terms` makes absence a bucket of its own, which is how
2,553 sales with no coupon appear next to the three coupons.

## A scripted `terms` key, and its cost

Step 17 buckets by a quarter computed in Painless from `sold_at`. It works on
any field the documents already have, with no reindex, which is why it is
useful for a question someone thought of today.

It is also evaluated for every matching document on every request, and it
cannot use the terms dictionary. If the question survives the week, add the
field at ingest (an ingest pipeline, example 07) and aggregate on that. The
check here is instructive: the scripted quarters must match the `date_range`
and `auto_date_histogram` counts, and a script with an off-by-one month would
fail it.

## Estimates

`percentiles` and `median_absolute_deviation` are computed from a t-digest
sketch, not by sorting. Step 12's median is 72.87 here, 72.45 in OpenSearch,
and 72.82 counted exactly. `cardinality` is likewise approximate above its
`precision_threshold`. Put exact numbers in a financial report; put sketches
on a dashboard.

## What would change at scale

- **Shards.** On one shard `terms` counts are exact. On several, a `terms`
  aggregation asks each shard for its top `shard_size` and can miss a bucket
  that is modest everywhere; `doc_count_error_upper_bound` says by how much.
  `composite` pages through every bucket exactly, whatever the shard count;
  `rare_terms` keeps its own approximation, a filter per shard that can
  occasionally call a term rare when it is not.
- **The summary keeps up by itself.** A `continuous` transform runs on its
  schedule and recomputes only the groups that sales written since its last run
  fall into; a `continuous` rollup rolls up each day once the day is over, and
  its `delay` leaves time for late sales to arrive.
- **Time zones.** Months and quarters here are UTC. A business reports in its
  own time zone, and `date_histogram`, `composite` and `date_range` all take
  `time_zone`; without it, a sale late on 31 December in New York lands in
  January.
