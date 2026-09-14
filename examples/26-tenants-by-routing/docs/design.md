# Design notes

## One index, and not one per tenant

Three ways to hold many customers:

| | An index per tenant | One index, no routing | One index, routed by tenant |
|---|---|---|---|
| Mappings and shard sets | one each, per tenant | one | one |
| Cluster state | grows with every sign-up | fixed | fixed |
| A search for one tenant asks | that tenant's shards | every shard | one shard |
| A tenant's data is removed by | deleting its index | `_delete_by_query` | `_delete_by_query` with its routing |
| Mapping change for one tenant | possible | not possible | not possible |

A few thousand small indices is a cluster that spends its time on metadata;
each shard has a fixed cost in heap and file handles whether it holds ten
documents or ten million. Routing keeps the one-index cost and gets back most
of the per-index read speed: the tenant's data is on one shard, and a request
that carries the tenant asks that shard only.

What it gives up is the per-tenant mapping, and the ability to drop a tenant by
deleting an index. When one tenant needs its own mapping, or its own retention,
it has outgrown the shared index -- see "The big tenant" below.

## Why routing, and the filter as well

Routing is a hash: `shard = hash(routing) mod shards`, folded through the
routing shards the index was created with. It decides where a document lives;
it says nothing about who owns it. Four shards and three tenants means some
tenants share a shard. Here `acme` and `globex` both hash to shard 3, so
`routing=acme` narrows a search to the shard that holds acme and globex,
and the `term` filter on `tenant` narrows it to acme.

The two together are the pattern: the routing is the speed, the filter is the
correctness. Leave out the routing and the answer is right but every shard is
asked. Leave out the filter and the answer is fast and wrong.

That is exactly what a filtered alias with routing packages. `tenant-acme`
holds the filter and the routing value, so a client that searches the alias
cannot forget either.

## Why `_routing.required`

Without it, a write that forgets the routing is accepted and placed by its id.
Every read that does carry the tenant then looks on the tenant's shard and does
not find it (step 3 shows the same miss with the wrong value), and a routed
`_update_by_query` or `_delete_by_query` silently skips it. `required: true`
turns that mistake into a refusal at write time, which is the only moment it is
cheap to fix. The `users` index is routed the same way for the same reason, and
the `terms` lookup in step 12 names the routing because of it.

## The big tenant: `routing_partition_size`

Routing puts all of one tenant on one shard. That is the goal for small
tenants and a problem for a large one: a tenant with a fifth of the data makes
its shard several times the size of the others.

`index.routing_partition_size: 2` changes the formula to

```
shard = (hash(routing) + hash(_id) mod 2) mod routing_shards / routing_factor
```

so one tenant spreads over 2 shards of the 4, chosen by the id. A search with
`routing=wayne` asks those 2; a get by id still needs only the tenant, because
the id supplies the rest. By that formula, `wayne` and the ids `w-1` to `w-12`
give six documents on shard 0 and six on shard 1.

It is fixed at index creation, must be smaller than `number_of_shards`, and an
index with it cannot be split later. That is why step 5 gives large tenants an
index of their own rather than setting it on the shared one: small tenants do
not need the spread, and should not pay for asking two shards.

## An alias per tenant, over two indices

Each tenant alias covers `tickets` (this year, `is_write_index: true`) and
`tickets-2025` (the archive, `is_write_index: false`). One name then serves
both jobs:

- a write through `tenant-acme` goes to `tickets` with routing `acme`;
- a search through `tenant-acme` covers both years, with the filter and the
  routing applied to each.

`routing` on its own sets both `index_routing` and `search_routing`, which is
what almost every tenant alias wants. They are separate for the rare case
where a tenant is written with one value and read with several -- a search
routing may be a comma-separated list, an index routing may not.

At the turn of the year the change is one `_aliases` call: add the new index
with `is_write_index: true`, set the old one to `false`. Clients keep the same
name.

## `preference`, and paging one tenant

Without a preference, each request may go to any copy of each shard. Copies
refresh independently, so page one can come from a copy that has seen a new
ticket and page two from one that has not, and a ticket appears twice or not at
all. A custom string -- a session id, a user id -- hashes to the same copies
every time, so one person's pages agree with each other. `_local` prefers the
copies on the node that received the request, which saves a network hop and
nothing more.

For deep listings, `search_after` with a point in time (example 15) is the
right tool; `preference` is for the first few pages a person clicks through.

## Several tenants in one request

- `_mget` takes a routing per document, so a dashboard showing tickets from
  several tenants is one request that asks exactly the shards needed.
- `_msearch` takes an index per search, and the index may be a tenant alias,
  so each search gets its tenant's filter and routing without repeating them.

## `terms` lookup for per-user access within a tenant

The user document holds the list of projects the user may see; the search
reads that list at query time. Changing what Ann may see is one write to her
document, and the next search follows it, with no query rewritten in the
application. The lookup is a get by id, so it needs the user's routing, and it
reads the document as last refreshed.

This is access within a tenant, a convenience for the application. It is not
a security boundary -- a client that can send the query can leave the lookup
out. Example 06 is how to make the boundary hold.

## Derived fields: a value per tenant, at search time

A cost that depends on the tenant's hourly rate could be written into each
ticket. Then a rate change means rewriting every ticket of that tenant. A
`derived` field in the request computes it at search time from `_source` and a
params map of rates, so the rates live in the application and change without a
reindex.

The price is the one every script pays: the value is computed for every
document the query reaches. Behind a tenant alias that is one tenant's
documents, which is what makes it affordable. Across the whole index, as in the
admin report in step 13, it is every document.

## `max_result_window`

`from + size` pages are built by collecting `from + size` hits on every shard
asked and throwing all but `size` away. The window caps that cost. A tenant
listing that lets users jump to page 500 should hit the limit and move to
`search_after`, not raise the limit. The refusal is a 400, not a shorter page,
so a client finds out rather than showing a silently truncated list.

## What would change at scale

- **Shard count against tenant count.** With thousands of tenants on a few
  shards, every shard holds many tenants and routing still cuts the fan-out by
  the shard count. More shards means a smaller fraction asked, and more fixed
  cost per shard.
- **Hot tenants move out.** When one tenant dominates, give it a partitioned
  index of its own, or an index of its own outright, and point its alias there.
  Clients do not notice.
- **Replicas and `preference`.** With replicas, routing narrows to one shard
  but still one of several copies; a stable `preference` per session is what
  keeps paging consistent.
- **Resharding keeps routing.** `_split` preserves where routed documents go
  because the routing shards are fixed at creation; `_shrink` does too. A
  partitioned index cannot be split.
