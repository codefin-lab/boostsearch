# Design notes

## Why a template, an alias and a policy rather than one index

An index that grows forever has three problems and they arrive in this order:
the shard gets too big to move, the old data costs as much to keep as the new,
and deleting the old data means `_delete_by_query` over a live index, which
rewrites segments rather than freeing them.

Rolling over solves all three at once. Deleting a whole index is a directory
removal; deleting half an index is work.

The three pieces each do one job:

| Piece | Job |
|---|---|
| `logs-template` | every index the rollover creates is shaped the same |
| the `logs` alias | writers never learn an index name |
| `logs-lifecycle` | the index rolls, cools and is deleted with nobody watching |

Leave out the template and the second index has a dynamic mapping -- `status`
becomes `long`, `@timestamp` might become `text`, and an aggregation that
worked yesterday fails today on half the data. This is the single most common
way a rollover setup breaks, and it breaks silently.

## The timings are seconds, and should not be

```
rollover      min_doc_count: 5
hot -> cold   min_index_age: 5s
cold -> gone  min_state_age: 10s
```

A real policy reads `min_size: 50gb` or `min_index_age: 1d` for the rollover
and `min_index_age: 30d` for the delete. Seconds are used here so the whole
life can be watched in one run, which also means **this example's policy would
destroy real data**. It is named `logs-lifecycle` and attached only to indices
this example creates.

## Why `read_only` before `delete`

The cold state does nothing a delete would not do anyway. It is there because
it is the shape of a real policy: hot (being written), warm or cold (read-only,
possibly force-merged and moved to cheaper nodes), then deleted. Making the
index read-only also lets the engine merge it down without racing a writer.

## Why `min_doc_count: 0` on the date histogram

A `date_histogram` returns only buckets that have documents unless told
otherwise. For a chart that is wrong: a half hour with no traffic is a data
point at zero, not a missing point, and a line chart will draw straight through
the gap as though nothing happened.

The cost is real -- a sparse series over a long range produces many empty
buckets -- so `extended_bounds` or a coarser interval is the answer for a wide
window, not dropping the option.

## Why the error rate is computed in the engine

```
errors      filter  status >= 500
error_rate  bucket_script  bad / all
```

The dashboard could divide two numbers itself. Doing it here means the
`bucket_selector` in step 6 can filter on the result, which the dashboard
cannot -- the engine would have already sent every bucket over the wire. On a
24-hour window at one-minute resolution that is 1,440 buckets sent to show
three.

## `significant_terms` against `terms`

Step 9 asks which service is over-represented among the 5xx *compared with the
morning as a whole*. A plain `terms` aggregation over the errors would name the
busiest service, which is usually the busiest service in general and tells you
nothing. `significant_terms` divides by the background rate.

The trade is that it needs a reasonable background: on a corpus of 30
documents it names whatever is there. 1,200 is about the floor for it to mean
anything.

## What would change at scale

- **One shard, no replicas.** A real logs index is sized so each shard lands
  between 20 and 50 GB, and rollover is on `min_size` rather than doc count.
- **`refresh_interval: 1s`** is the default and is expensive for a
  write-heavy index nobody searches in real time; 30s is the usual setting.
- **The `percentiles` aggregation is approximate** (t-digest). At the
  precision this example prints, that is invisible; on an SLO report the
  approximation and its error bound are worth stating.
