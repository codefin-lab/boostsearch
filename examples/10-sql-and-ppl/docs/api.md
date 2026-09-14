# API surface -- SQL and PPL over the same index

This example drives four endpoints only, so the interesting table is not
the paths but the queries. Generated from `run.sh`; if the two disagree,
`run.sh` is right.

| Endpoint | Purpose |
|---|---|
| `POST /_plugins/_sql` | run an SQL query; `?format=` picks the shape |
| `POST /_plugins/_sql/_explain` | what the SQL was turned into |
| `POST /_plugins/_ppl` | run a PPL pipeline; `?format=` picks the shape |
| `POST /_plugins/_ppl/_explain` | what the PPL was turned into |

## Every query, in order

| Step | Language | Format | Query |
|---|---|---|---|
| SQL: the report nobody wants to write as a JSON aggregation | SQL | json (default) | `SELECT country, customer, COUNT(*) AS orders, ROUND(SUM(total), 2) AS revenue, ROUND(AVG..` |
| the same answer as a table a human reads | SQL | table | `SELECT country, COUNT(*) AS orders, ROUND(SUM(total),2) AS revenue FROM orders WHERE sta..` |
| as CSV, for a spreadsheet | SQL | csv | `SELECT customer, COUNT(*) AS orders FROM orders GROUP BY customer ORDER BY orders DESC` |
| as jdbc, which carries the column types a driver needs | SQL | jdbc | `SELECT customer, total FROM orders LIMIT 3` |
| and raw, tab separated | SQL | raw | `SELECT customer, total FROM orders LIMIT 3` |
| SQL over text: the full-text operators are there too | SQL | json (default) | `SELECT order_id, note FROM orders WHERE MATCH(note, 'complained late') LIMIT 5` |
|  | SQL | json (default) | `SELECT order_id, customer FROM orders WHERE customer IN ('acme','initech') AND total BET..` |
| dates, arithmetic and CASE | SQL | json (default) | `SELECT MONTH(placed) AS month, COUNT(*) AS orders, SUM(CASE WHEN status = 'refunded' THE..` |
| PPL: the same question, written as a pipeline | PPL | json (default) | `source=orders \| where status = 'paid' \| stats count() as orders, sum(total) as revenue b..` |
| PPL's strength is the step-by-step shape | PPL | table | `source=orders \| where total > 500 \| eval band = if(total > 2000, 'large', 'medium') \| st..` |
| fields, rename, dedup, the small verbs | PPL | table | `source=orders \| fields customer, country, total \| rename total as amount \| sort - amount..` |
|  | PPL | table | `source=orders \| dedup customer \| fields customer, country` |
| top and rare, which SQL needs a window function for | PPL | table | `source=orders \| top 3 customer` |
|  | PPL | table | `source=orders \| rare 2 channel` |

## The formats

| `format=` | What it is for |
|---|---|
| (none) | the engine's own JSON, with `schema` and `datarows` |
| `jdbc` | JSON carrying a type per column, which a JDBC driver needs |
| `table` | fixed-width text, for a person |
| `csv` | for a spreadsheet |
| `raw` | tab separated, for a pipe |

## Bodies

Both endpoints take the same envelope: `{"query": "<the query text>"}`.
`run.sh` builds it with `python3 -c json.dumps` rather than by quoting, so
a query holding a quote or a newline survives the trip.
