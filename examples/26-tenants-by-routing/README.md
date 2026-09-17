# 26. Many small customers in one index, found quickly

A support desk with hundreds of customers, most of them with a few dozen
tickets. An index per customer is hundreds of mappings, hundreds of shard sets
and a cluster state that grows with every sign-up. One index for all of them is
cheap to run, but a naive search for one customer asks every shard about
everyone.

Routing is the middle way. Each ticket is written with its customer as the
routing value, so all of one customer's tickets sit on one shard, and a search
that carries the same value asks that shard alone. An alias per customer puts
the routing and the tenant filter in one place, so the application names the
customer and nothing else. Example 06 keeps tenants apart with security roles,
and example 14 swaps an alias under a reindex; this one is about placement and
reaching one tenant's data quickly.

## What it shows

| Step | Feature |
|---|---|
| 1 | `number_of_shards` > 1, `_routing` with `required: true` in the mapping |
| 2 | `routing` on each `_bulk` action line |
| 3 | `GET _doc` with `?routing=`: found with the right value, not with another tenant's |
| 4 | `_search` and `_count` with `?routing=`, and why the tenant filter is still needed |
| 5 | `index.routing_partition_size`: a big tenant spread over two of four shards |
| 6 | filtered aliases per tenant with `routing`, `index_routing` / `search_routing`, `is_write_index` over a live and an archive index; `_cat/aliases`, `_alias` |
| 7 | writing through an alias: the write index and the routing come from the alias |
| 8 | `_search` and `_count` through an alias spanning two indices |
| 9 | `preference` with a custom string for consistent paging, and `_local` |
| 10 | `_mget` with a routing per document; `_msearch` with an alias per search |
| 11 | `_update_by_query` limited to one routing value |
| 12 | a `terms` lookup query: a user's allowed projects read from the user's document, with `routing` |
| 13 | `derived` fields defined in the search request: a per-tenant cost from per-tenant rates, aggregated and queried |
| 14 | `index.max_result_window`: set, hit, and unset again |

## Running it

```bash
make serve      # a node configured for this example, port 9286, foreground
make run        # the example, in another terminal
```

`make check` validates the scripts and every request body without a server;
`make clean` deletes what the example left behind. Copy `.env.example` to
`.env` to change the address or anything else.

The longer form, and what this example needs of the node:

```bash
./target/release/velosearch &
VS=http://127.0.0.1:9200 examples/26-tenants-by-routing/run.sh
```

Nothing beyond a plain node.

## What to look for

- **Step 3** is routing in one line. The same id, asked for with the wrong
  tenant's routing, is not found:

  ```
  GET /tickets/_doc/t-1001?routing=acme      -> "found":true,"_routing":"acme"
  GET /tickets/_doc/t-1001?routing=initech   -> {"_index":"tickets","_id":"t-1001","found":false}   (404)
  ```

  A get by id asks exactly one shard, and the routing value names it. `acme`
  hashes to shard 3 of 4 and `initech` to shard 0, so the second request looks
  on a shard the ticket was never written to. This is also why the mapping
  says `required: true`: a document written without its routing lands on the
  shard its id hashes to, and every later read that carries the tenant misses
  it.

- **Step 4** keeps the tenant filter even though the routing is there. Routing
  chooses a shard, not a tenant: `globex` also hashes to shard 3, so a search
  with `routing=acme` and no filter would return globex's tickets as well.
  With both, the answer is exactly acme's five open tickets:
  `t-1001, t-1002, t-1004, t-1005, t-1008`.

- **Step 7** writes through `tenant-acme` without naming an index or a
  routing value. The answer names the index the alias chose:
  `"_index":"tickets","result":"created"`, and the archive still holds 3.

- **Step 8** reads through the same alias across both indices. Four tickets
  mention a login, and only acme's come back, one of them from last year:
  `t-1100, t-1005, t-1001` from `tickets` and `t-0901` from `tickets-2025`.
  initech's `t-2002` and globex's `t-3001` match the words and are kept out by
  the alias filter. `_count` through the alias says 11: 9 and 2.

- **Step 9** pages with `preference=ann-session-42`. Page one is
  `t-1100 t-1008 t-1007 t-1006`, page two `t-1005 t-1004 t-1003 t-1002`, and
  the check confirms no id appears twice. With replicas, two pages sent to
  different copies can disagree about a document refreshed on one and not
  yet the other; a stable preference string keeps one session on one set of
  copies.

- **Step 10** asks for `t-3001` twice in one `_mget`: with `globex` it is
  found, with `initech` it is not. `_msearch` through three aliases counts open
  tickets for three tenants in one round trip: `[6, 2, 2]`.

- **Step 11** escalates acme's long open tickets:
  `"total":3,"updated":3,"failures":[]`, and a search for escalated tickets
  outside acme finds 0.

- **Step 13** bills each tenant at its own rate without storing a cost
  anywhere. The rates arrive as script params, and the derived field is
  aggregated like a mapped one:

  ```
  acme     495 minutes   cost 990.0   (120 an hour)
  globex   170 minutes   cost 425.0   (150 an hour)
  initech  110 minutes   cost 165.0   ( 90 an hour)
  ```

  Through `tenant-acme`, a range query on the same derived field finds the
  five tickets that cost 100 or more, `t-0902` from the archive among them.

- **Step 14** sets `max_result_window` to 10 and asks for `from 8 + size 5`:
  a 400, `Result window is too large, from + size must be less than or equal
  to: [10] but was [13]`. The request is refused, not cut short. `from 5 +
  size 5` returns the last four, and setting the value to `null` puts the
  default of 10000 back.

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
| `requests/` | 17 request bodies, one file each |
| `data/` | 4 bulk document sets and 1 `_msearch` body |

## Leaves behind

The indices `tickets` (16 documents), `tickets-big` (12), `tickets-2025` (3)
and `users` (3), and the aliases `tenant-acme`, `tenant-initech` and
`tenant-globex` over the first and third. `index.max_result_window` on
`tickets` is back at its default. Rerunning deletes the indices first, which
removes the aliases with them.
