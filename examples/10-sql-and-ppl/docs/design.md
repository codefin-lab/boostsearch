# Design notes

## What SQL and PPL are for, and what they are not for

Both compile to the same thing a JSON query would have been. Neither is a
different engine; they are front ends. That settles most arguments about them:

- **Use them** for reports, ad-hoc questions, anything a person types once, and
  anywhere a spreadsheet or a BI tool is the consumer.
- **Do not use them** on a hot path in an application. The JSON a query
  compiles to is fixed; writing it directly skips the parse and the planning,
  and lets you control exactly which aggregations run.

The eight-line `SELECT` in step 2 compiles to roughly thirty lines of nested
`terms` with a `bucket_selector`. That is a real productivity difference for a
question asked once, and no difference at all for a question asked ten thousand
times a second.

## Why the same questions are asked twice

Steps 2 and 10 are the same report. Steps 8 and 11 are the same kind of
derived-column work. The point is not that one language is better; it is that
they read differently and suit different readers.

```sql
SELECT country, COUNT(*) FROM orders WHERE status='paid' GROUP BY country
```

```
source=orders | where status='paid' | stats count() by country
```

SQL states the shape of the answer first and the filtering later. PPL reads in
the order it executes: source, filter, aggregate, sort. For an unfamiliar query
of any length, the pipeline is easier to check step by step -- which is why
step 11's four-stage query is written in PPL and not SQL.

## The formats are not cosmetic

| `format=` | Carries | Use |
|---|---|---|
| (none) | `schema` + `datarows` | the engine's own shape |
| `jdbc` | a type per column | a JDBC driver needs this and the others cannot be used |
| `table` | fixed-width text | a person reading a terminal |
| `csv` | comma separated with a header | a spreadsheet |
| `raw` | tab separated | a pipe into `cut`, `awk`, anything |

The one that matters is `jdbc`: it is the only shape carrying column types, so
a driver can present a `ResultSet`. The others are lossy on purpose.

## Why `_explain` is in the example twice

Because it is how you find out whether a query is doing what you think. The SQL
in step 8 -- `MONTH(placed)`, `SUM(CASE WHEN ...)`, a division -- looks like
row-by-row work and is not: it becomes a `date_histogram` with a `filter`
sub-aggregation and a `bucket_script`. Knowing that tells you it is cheap.

The converse also happens: a query that looks harmless and compiles to
something that fetches every document. `_explain` is the only way to tell, and
it costs nothing because it does not run.

## `top` and `rare`, and why PPL has an edge

```
source=orders | top 3 customer
```

is a `terms` aggregation with `size: 3`. In SQL it is a window function over a
grouped subquery, and at that point the SQL is longer and less clear than the
JSON would have been. PPL's verbs map onto aggregation primitives one for one,
which is its real advantage over SQL as a front end for this engine.

## What would change at scale

- **Approximation applies here too.** `COUNT(DISTINCT x)` becomes
  `cardinality`, which is a HyperLogLog estimate, and `PERCENTILE` becomes
  t-digest. Neither language warns you; the number in a financial report may
  not be exact.
- **A `terms` aggregation across shards is approximate** unless `shard_size`
  is raised, and neither language gives you a way to raise it. A report whose
  correctness matters wants the JSON form.
- **`LIMIT` is not a cheap way to sample.** `LIMIT 10` with a `GROUP BY` still
  computes every group before discarding.
