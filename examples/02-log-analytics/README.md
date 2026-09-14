# 2. Logs that manage their own life

Nobody deletes last month's logs by hand. This example sets up the whole
arrangement an operations team runs on: a template so every index that appears
is shaped the same, an alias that writes always go through, a policy that rolls
the index over, makes it read-only and finally deletes it -- and on top of that
the aggregations a dashboard asks: an error rate per half hour, how fast it got
worse, the 99th percentile per host, and what marks out the bad window from the
rest of the morning.

The lifetimes in the policy are seconds rather than days, so the whole life can
be watched in one run.

## What it shows

| Step | Feature |
|---|---|
| 1 | `_index_template` with a mapping and settings |
| 2 | an ISM policy: `rollover`, `read_only`, `delete`, transitions on age and doc count |
| 3 | a write alias (`is_write_index`), `_plugins/_ism/add` |
| 4 | `_bulk` through an alias |
| 5 | `date_histogram` with `min_doc_count: 0`, nested `terms`, `filter`, `bucket_script` |
| 6 | `derivative`, `moving_fn` with a Painless `MovingFunctions` call, `cumulative_sum`, `bucket_selector` |
| 7 | `percentiles`, `percentile_ranks`, `extended_stats`, `cardinality`, `max_bucket` over a sibling |
| 8 | `terms` ordered by a sub-aggregation, `top_hits`, `min`, `histogram`, keyed `range` |
| 9 | `significant_terms` |
| 10 | `_plugins/_ism/explain`, `_cat/indices` |
| 11 | `_cat/aliases` |

## Running it

```bash
make serve      # a node configured for this example, port 9262, foreground
make run        # the example, in another terminal
```

`make check` validates the scripts and every request body without a server;
`make clean` deletes what the example left behind. Copy `.env.example` to
`.env` to change the address or anything else.

The longer form, and what this example needs of the node:

The policy only moves when the ISM job runs, so start the server with a short
interval or the last steps will just show the first index:

```bash
BOOSTSEARCH_ISM_INTERVAL_MS=2000 ./target/release/boostsearch &
examples/02-log-analytics/run.sh
```

## What to look for

- **Step 5** gives one row per half hour with an error rate computed in the
  engine, not in the dashboard. `min_doc_count: 0` keeps the quiet half hours
  in the series, which is what a chart needs and what a naive `terms` loses.
- **Step 6** is the same series read four ways. `bucket_selector` drops every
  half hour that was fine, so the answer is only the incident.
- **Step 7** asks `max_bucket` about a sibling aggregation's result -- the worst
  host's p99 -- which is a two-pass question answered in one request.
- **Step 9** should name `checkout`: it is the service over-represented among
  the 5xx compared with the morning as a whole. That is the whole point of
  `significant_terms` over `terms`, which would just name the busiest service.

## This directory

It is a project of its own: nothing here reaches outside the directory,
so it can be copied somewhere else and still run.

| | |
|---|---|
| `README.md` | this page |
| `docs/design.md` | why it is built this way, and what would change at scale |
| `docs/api.md` | every request it makes, and every endpoint it touches |
| `docs/troubleshooting.md` | what goes wrong, and what it means |
| `run.sh` | the example |
| `node.sh` | a node configured for exactly what this example needs |
| `lib.sh` | shell helpers; its own copy |
| `Makefile` | `serve`, `run`, `check`, `clean` |
| `.env.example` | the settings, with a line each on what they are for |
| `requests/` | 7 request bodies, one file each |

## Leaves behind

Nothing, if the policy gets far enough: it deletes its own indices. The
template `logs-template` and the policy `logs-lifecycle` stay, and rerunning
replaces them. `/tmp/logs.ndjson` is the generated corpus.
