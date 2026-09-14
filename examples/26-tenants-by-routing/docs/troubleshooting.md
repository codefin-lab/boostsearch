# Troubleshooting

## A document is in the index, but a get by id says `"found": false`

The get carries a different routing value from the write, or none. A get by id
asks one shard, the one its routing names:

```bash
curl -s 'localhost:9286/tickets/_doc/t-1001?routing=acme'      # found
curl -s 'localhost:9286/tickets/_doc/t-1001?routing=initech'   # not found
```

Search for it by id, which asks every shard. If the search finds it and the
routed get does not, the document is there and the routing value is wrong:

```bash
curl -s localhost:9286/tickets/_search -H 'content-type: application/json' \
  -d '{"query":{"ids":{"values":["t-1001"]}}}' | jq '.hits.total'
```

## A write is refused with `routing_missing_exception`

The mapping says `"_routing": {"required": true}` and the request carried no
routing. That refusal is the point of the setting. Add `?routing=` to the
request, `"routing"` to the bulk action line, or write through the tenant's
alias, which supplies it.

## A search with `routing` returns another tenant's documents

Expected. Routing picks a shard, and a shard holds several tenants: `acme` and
`globex` share shard 3 here. Keep the `tenant` filter, or search through the
tenant alias, which has both. See `design.md`.

## A write through a tenant alias is refused: no write index

```
no write index is defined for alias [tenant-acme]
```

The alias covers more than one index and none has `is_write_index: true`.
Step 6 sets it on `tickets`. After a year-end change, check that exactly one
index is the write index:

```bash
curl -s 'localhost:9286/_cat/aliases/tenant-*?v&h=alias,index,is_write_index'
```

## A get by id through a tenant alias is refused

A get, update or delete by id needs exactly one index, and `tenant-acme` names
two. Search through the alias with an `ids` query, or get from `tickets`
directly with `?routing=acme`.

## An alias with several `index_routing` values is refused

`search_routing` may be a list (`"acme,globex"`); `index_routing` may not,
because a write goes to exactly one shard.

## `routing_partition_size` is refused at index creation

It must be smaller than `number_of_shards`, the mapping must say
`_routing.required: true`, and it cannot be changed after creation. Create the
index again with all three right.

## A `terms` lookup finds nothing

The lookup is a get by id on `users`, so it needs the user's routing:

```json
{ "terms": { "project": { "index": "users", "id": "ann", "path": "projects", "routing": "acme" } } }
```

Also check the document has been refreshed, and that `path` names an array of
the same values the searched field holds -- here keywords, matched exactly.

## A derived field comes back empty

The script in step 13 reads `params._source`. Check the field names in the
script match the source exactly, and that every document has a rate in the
params map -- a tenant missing from `rates` gives no value for its tickets. A
derived field only exists in the request that defines it; the next request
must define it again.

## `Result window is too large`

`from + size` is over `index.max_result_window`. Step 14 sets it to 10 and then
back. If it is still set on `tickets`, a run was stopped between the two:

```bash
curl -s -X PUT localhost:9286/tickets/_settings -H 'content-type: application/json' \
  -d '{"index":{"max_result_window":null}}'
```

For listings deeper than the window, use `search_after` (example 15) rather
than raising it.

## Paging shows a ticket twice

Pages are going to different shard copies that have refreshed at different
moments. Send the same `preference` string with every page of one session,
as step 9 does.

## Cleaning up

```bash
make clean          # deletes tickets, tickets-big, tickets-2025, users
```
