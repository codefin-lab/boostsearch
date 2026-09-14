# API surface -- 26. Many small customers in one index, found quickly

Every request this example makes, in the order it makes them, including the
deletes that clear the way and the `_count` calls behind each `expect_docs`
check. Generated from `run.sh`; if the two disagree, `run.sh` is right.

`$IDX` is `tickets`, `$BIG` is `tickets-big`, `$OLD` is `tickets-2025`,
`$USERS` is `users`.

| Step | Method | Path | Body |
|---|---|---|---|
| an index of tickets: four shards, and a routing value no write may leave out | `DELETE` | `/$IDX` | -- |
|  | `DELETE` | `/$BIG` | -- |
|  | `DELETE` | `/$OLD` | -- |
|  | `DELETE` | `/$USERS` | -- |
|  | `PUT` | `/$IDX` | `requests/01-tickets-four-shards-routing-required.json` |
|  | `GET` | `/$IDX/_mapping?filter_path=*.mappings._routing` | -- |
| fifteen tickets for three tenants, each written with its tenant as the routing | `POST` | `/$IDX/_bulk?refresh=wait_for` | `data/01-fifteen-tickets-three-tenants-each-routed.ndjson` |
|  | `GET` | `/$IDX/_count` | -- |
| a ticket by id: found with its tenant's routing, not found with another's | `GET` | `/$IDX/_doc/t-1001?routing=acme` | -- |
|  | `GET` | `/$IDX/_doc/t-1001?routing=initech` | -- |
| one tenant's open tickets: the routing picks the shard, the filter picks the tenant | `GET` | `/$IDX/_search?routing=acme&filter_path=hits.total,hits.hits._id,hits.hits._source` | `requests/02-one-tenants-open-tickets.json` |
|  | `GET` | `/$IDX/_count?routing=acme&filter_path=count` | `requests/03-count-one-tenant.json` |
| a tenant too big for one shard: routing_partition_size spreads it over two | `DELETE` | `/$BIG` | -- |
|  | `PUT` | `/$BIG` | `requests/04-a-partitioned-index-for-large-tenants.json` |
|  | `GET` | `/$BIG/_settings/index.routing_partition_size,index.number_of_shards?flat_settings=true` | -- |
|  | `POST` | `/$BIG/_bulk?refresh=wait_for` | `data/02-one-large-tenant-twelve-tickets.ndjson` |
|  | `GET` | `/$BIG/_count` | -- |
|  | `GET` | `/$BIG/_doc/w-7?routing=wayne` | -- |
|  | `GET` | `/$BIG/_count?routing=wayne&filter_path=count` | inline |
| last year's tickets in an archive index, then an alias per tenant over both | `DELETE` | `/$OLD` | -- |
|  | `PUT` | `/$OLD` | `requests/01-tickets-four-shards-routing-required.json` |
|  | `POST` | `/$OLD/_bulk?refresh=wait_for` | `data/03-last-years-tickets-the-archive.ndjson` |
|  | `GET` | `/$OLD/_count` | -- |
|  | `POST` | `/_aliases` | `requests/05-an-alias-per-tenant.json` |
|  | `GET` | `/_cat/aliases/tenant-*?v&s=alias,index&h=alias,index,routing.index,routing.search,is_write_index` | -- |
|  | `GET` | `/_alias/tenant-acme` | -- |
| a ticket written through the alias: the client names neither index nor routing | `PUT` | `/tenant-acme/_doc/t-1100?refresh=wait_for` | `requests/06-a-ticket-written-through-the-alias.json` |
|  | `GET` | `/$IDX/_count` | -- |
|  | `GET` | `/$OLD/_count` | -- |
| reading through the alias: one tenant, both indices, nobody else's tickets | `GET` | `/tenant-acme/_search?filter_path=hits.total,hits.hits._index,hits.hits._id,hits.hits._source` | `requests/07-search-through-the-alias.json` |
|  | `GET` | `/tenant-acme/_count?filter_path=count` | -- |
|  | `GET` | `/tenant-initech/_count?filter_path=count` | -- |
| paging with preference: the same copies answer every page of one session | `GET` | `/tenant-acme/_search?preference=ann-session-42&filter_path=hits.hits._id` | `requests/08-page-one.json` |
|  | `GET` | `/tenant-acme/_search?preference=ann-session-42&filter_path=hits.hits._id` | `requests/09-page-two.json` |
|  | `GET` | `/tenant-acme/_count?preference=_local&filter_path=count` | -- |
| several tenants at once: _mget with a routing per id, _msearch with an alias per search | `GET` | `/$IDX/_mget?filter_path=docs._id,docs._routing,docs.found` | `requests/10-several-tenants-by-id.json` |
|  | `GET` | `/_msearch?filter_path=responses.hits.total.value,responses.status` | `data/04-open-tickets-per-tenant-one-round.ndjson` |
| _update_by_query on one routing value: escalate acme's long open tickets | `POST` | `/$IDX/_update_by_query?routing=acme&refresh=true` | `requests/11-escalate-one-tenants-long-tickets.json` |
|  | `GET` | `/tenant-acme/_search` | inline |
|  | `GET` | `/$IDX/_search` | inline |
| a terms lookup: the projects a user may see, read from the user's own document | `DELETE` | `/$USERS` | -- |
|  | `PUT` | `/$USERS` | `requests/12-users-index-routed-by-tenant.json` |
|  | `POST` | `/$USERS/_bulk?refresh=wait_for` | `data/05-users-and-the-projects-each-may.ndjson` |
|  | `GET` | `/$USERS/_count` | -- |
|  | `GET` | `/tenant-acme/_search?filter_path=hits.total,hits.hits._id,hits.hits._source` | `requests/13-tickets-in-the-projects-ann-may-see.json` |
| derived fields: a per-tenant cost computed at search time, never stored | `GET` | `/$IDX/_search?filter_path=aggregations` | `requests/14-cost-per-tenant-at-search-time.json` |
|  | `GET` | `/tenant-acme/_search?filter_path=hits.hits._index,hits.hits._id,hits.hits.fields` | `requests/15-one-tenants-expensive-tickets.json` |
| index.max_result_window: how deep one tenant's listing may page | `PUT` | `/$IDX/_settings` | inline |
|  | `GET` | `/$IDX/_search?routing=acme` | `requests/16-past-the-window.json` |
|  | `GET` | `/$IDX/_search?routing=acme&filter_path=hits.hits._id` | `requests/17-inside-the-window.json` |
|  | `PUT` | `/$IDX/_settings` | inline |
|  | `GET` | `/$IDX/_settings/index.max_result_window?include_defaults=true&flat_settings=true` | -- |
| what this example leaves behind, checked rather than assumed | `GET` | `/$IDX/_count` | -- |
|  | `GET` | `/$BIG/_count` | -- |
|  | `GET` | `/$OLD/_count` | -- |
|  | `GET` | `/$USERS/_count` | -- |

## Endpoints touched

Path parameters and query strings removed, deduplicated:

- `/<var>`
- `/<var>/_bulk`
- `/<var>/_count`
- `/<var>/_doc/<id>`
- `/<var>/_mapping`
- `/<var>/_mget`
- `/<var>/_search`
- `/<var>/_settings`
- `/<var>/_settings/<var>`
- `/<var>/_update_by_query`
- `/_alias/<var>`
- `/_aliases`
- `/_cat/aliases/<var>`
- `/_msearch`

## Request bodies

- [`requests/01-tickets-four-shards-routing-required.json`](../requests/01-tickets-four-shards-routing-required.json)
- [`requests/02-one-tenants-open-tickets.json`](../requests/02-one-tenants-open-tickets.json)
- [`requests/03-count-one-tenant.json`](../requests/03-count-one-tenant.json)
- [`requests/04-a-partitioned-index-for-large-tenants.json`](../requests/04-a-partitioned-index-for-large-tenants.json)
- [`requests/05-an-alias-per-tenant.json`](../requests/05-an-alias-per-tenant.json)
- [`requests/06-a-ticket-written-through-the-alias.json`](../requests/06-a-ticket-written-through-the-alias.json)
- [`requests/07-search-through-the-alias.json`](../requests/07-search-through-the-alias.json)
- [`requests/08-page-one.json`](../requests/08-page-one.json)
- [`requests/09-page-two.json`](../requests/09-page-two.json)
- [`requests/10-several-tenants-by-id.json`](../requests/10-several-tenants-by-id.json)
- [`requests/11-escalate-one-tenants-long-tickets.json`](../requests/11-escalate-one-tenants-long-tickets.json)
- [`requests/12-users-index-routed-by-tenant.json`](../requests/12-users-index-routed-by-tenant.json)
- [`requests/13-tickets-in-the-projects-ann-may-see.json`](../requests/13-tickets-in-the-projects-ann-may-see.json)
- [`requests/14-cost-per-tenant-at-search-time.json`](../requests/14-cost-per-tenant-at-search-time.json)
- [`requests/15-one-tenants-expensive-tickets.json`](../requests/15-one-tenants-expensive-tickets.json)
- [`requests/16-past-the-window.json`](../requests/16-past-the-window.json)
- [`requests/17-inside-the-window.json`](../requests/17-inside-the-window.json)
- [`data/01-fifteen-tickets-three-tenants-each-routed.ndjson`](../data/01-fifteen-tickets-three-tenants-each-routed.ndjson)
- [`data/02-one-large-tenant-twelve-tickets.ndjson`](../data/02-one-large-tenant-twelve-tickets.ndjson)
- [`data/03-last-years-tickets-the-archive.ndjson`](../data/03-last-years-tickets-the-archive.ndjson)
- [`data/04-open-tickets-per-tenant-one-round.ndjson`](../data/04-open-tickets-per-tenant-one-round.ndjson)
- [`data/05-users-and-the-projects-each-may.ndjson`](../data/05-users-and-the-projects-each-may.ndjson)
