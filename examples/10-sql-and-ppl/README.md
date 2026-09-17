# 10. SQL and PPL over the same index

Four hundred orders, and the questions a finance team asks about them, written
three ways: as SQL, as PPL, and -- by implication -- as the nested JSON
aggregation you would otherwise have had to write. The SQL in step 2 is eight
lines; the equivalent `terms`-inside-`terms`-with-`bucket_selector` is about
thirty.

Each language is also asked for its answer in every shape the server offers,
because which one you want depends entirely on who is reading it.

## What it shows

| Step | Feature |
|---|---|
| 2 | SQL: `GROUP BY` on two fields, `HAVING`, `ORDER BY`, `LIMIT` |
| 3-6 | `format=table`, `csv`, `jdbc`, `raw` |
| 7 | `MATCH()`, `IN`, `BETWEEN` |
| 8 | `MONTH()`, `CASE WHEN`, arithmetic in the projection |
| 9 | `_plugins/_sql/_explain` |
| 10 | PPL: `source`, `where`, `stats ... by`, `sort` |
| 11 | `eval` with `if`, grouping by two fields |
| 12 | `fields`, `rename`, `dedup`, `head` |
| 13 | `top`, `rare` |
| 14 | `_plugins/_ppl/_explain` |
| 15 | a parse error from each |

## Running it

```bash
make serve      # a node configured for this example, port 9270, foreground
make run        # the example, in another terminal
```

`make check` validates the scripts and every request body without a server;
`make clean` deletes what the example left behind. Copy `.env.example` to
`.env` to change the address or anything else.

The longer form, and what this example needs of the node:

```bash
./target/release/velosearch &
examples/10-sql-and-ppl/run.sh
```

## What to look for

- **Step 3** (`table`) is the one to paste into a chat; **step 4** (`csv`) is
  the one to open in a spreadsheet; **step 5** (`jdbc`) carries a `schema`
  block with a type per column, which is what a JDBC driver needs and what the
  others do not have.
- **Step 8** shows SQL doing things in the projection -- a month extracted from
  a date, a conditional counted, a division -- that map onto a `date_histogram`
  with a `filter` sub-aggregation and a `bucket_script`. Run
  `_plugins/_sql/_explain` on it to see which.
- **Steps 10 and 11** are the same kind of question in PPL. The difference from
  SQL is that a pipeline reads in the order it runs, so an unfamiliar query is
  easier to check: filter, then derive a column, then aggregate, then sort.
- **Step 13** is where PPL wins outright. `top 3 customer` is a `terms`
  aggregation; in SQL it is a window function over a grouped subquery.

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
| `requests/` | 1 request bodies, one file each |

## Leaves behind

The index `orders`, and `/tmp/orders.ndjson`. Rerunning deletes the index
first.
