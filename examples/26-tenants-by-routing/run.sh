#!/usr/bin/env bash
# Many small customers in one index: routing, an alias each, and found quickly.
source "$(dirname "$0")/lib.sh"
IDX=tickets
BIG=tickets-big
OLD=tickets-2025
USERS=users

# call WANT METHOD PATH [BODY] -- a request whose status is part of the answer.
# A refusal is worth reading, so the body is printed whatever the status, and
# the status is checked against WANT. BODY may be @file.
LAST=$(mktemp)
trap 'rm -f "$LAST"' EXIT
call() {
  local want=$1 m=$2 p=$3 b=${4-} code
  local args=(-sS -o "$LAST" -w '%{http_code}' -X "$m" "$VS$p")
  [ -n "$AUTH" ] && args+=(-u "$AUTH")
  case "$b" in
    '')         ;;
    @*.ndjson)  args+=(-H 'Content-Type: application/x-ndjson' --data-binary "$b") ;;
    @*)         args+=(-H 'Content-Type: application/json' --data-binary "$b") ;;
    *)          args+=(-H 'Content-Type: application/json' -d "$b") ;;
  esac
  code=$(curl "${args[@]}")
  cat "$LAST"; echo
  if [ "$code" = "$want" ]; then
    printf '   \033[32mok\033[0m  %s %s answered %s\n' "$m" "$p" "$code"
  else
    printf '   \033[31mNOT WHAT WAS EXPECTED\033[0m  %s %s answered %s, expected %s\n' "$m" "$p" "$code" "$want" >&2
    _fails=$((_fails + 1))
  fi
}

# holds 'PYTHON EXPRESSION over j' WHAT -- a fact about the last answer `call` printed
holds() {
  if python3 -c 'import json,sys; j = json.load(open(sys.argv[1])); sys.exit(0 if eval(sys.argv[2]) else 1)' "$LAST" "$1" 2>/dev/null; then
    printf '   \033[32mok\033[0m  %s\n' "$2"
  else
    printf '   \033[31mNOT WHAT WAS EXPECTED\033[0m  %s  (%s)\n' "$2" "$1" >&2
    _fails=$((_fails + 1))
  fi
}

ids='[h["_id"] for h in j["hits"]["hits"]]'

step "an index of tickets: four shards, and a routing value no write may leave out"
note "the tenant is the routing value, so one tenant's tickets sit together on one shard"
gone "/$IDX"; gone "/$BIG"; gone "/$OLD"; gone "/$USERS"
reqf PUT "/$IDX" requests/01-tickets-four-shards-routing-required.json
green "$IDX"
call 200 GET "/$IDX/_mapping?filter_path=*.mappings._routing"
holds 'j["tickets"]["mappings"]["_routing"]["required"] is True' "the mapping says routing is required"

step "fifteen tickets for three tenants, each written with its tenant as the routing"
ndjson "/$IDX/_bulk?refresh=wait_for" data/01-fifteen-tickets-three-tenants-each-routed.ndjson \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print({"errors": d["errors"], "items": len(d["items"])})'
expect_docs "$IDX" 15 "acme 8, initech 4, globex 3"

step "a ticket by id: found with its tenant's routing, not found with another's"
call 200 GET "/$IDX/_doc/t-1001?routing=acme"
holds 'j["found"] is True and j["_routing"] == "acme"' "found, and the answer names the routing it was written with"
note "initech routes to shard 0 and acme to shard 3: the id is looked for on the wrong shard"
call 404 GET "/$IDX/_doc/t-1001?routing=initech"
holds 'j["found"] is False' "not found -- a get by id asks one shard, the one the routing names"

step "one tenant's open tickets: the routing picks the shard, the filter picks the tenant"
note "globex routes to shard 3 as well; without the term filter its tickets would come back too"
call 200 GET "/$IDX/_search?routing=acme&filter_path=hits.total,hits.hits._id,hits.hits._source" \
  @requests/02-one-tenants-open-tickets.json
holds "$ids"' == ["t-1001","t-1002","t-1004","t-1005","t-1008"]' "acme's five open tickets, oldest first"
call 200 GET "/$IDX/_count?routing=acme&filter_path=count" @requests/03-count-one-tenant.json
holds 'j["count"] == 8' "_count with the same routing: acme has 8"

step "a tenant too big for one shard: routing_partition_size spreads it over two"
gone "/$BIG"
reqf PUT "/$BIG" requests/04-a-partitioned-index-for-large-tenants.json
green "$BIG"
call 200 GET "/$BIG/_settings/index.routing_partition_size,index.number_of_shards?flat_settings=true"
holds 'j["tickets-big"]["settings"]["index.routing_partition_size"] == "2"' "partition size 2 of 4 shards"
ndjson "/$BIG/_bulk?refresh=wait_for" data/02-one-large-tenant-twelve-tickets.ndjson \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print({"errors": d["errors"], "items": len(d["items"])})'
expect_docs "$BIG" 12 "wayne, one routing value"
note "the shard is now the routing hash plus the id hash folded into 2 -- still one value to remember"
call 200 GET "/$BIG/_doc/w-7?routing=wayne"
holds 'j["found"] is True' "a get still needs only the tenant: the id supplies the rest"
call 200 GET "/$BIG/_count?routing=wayne&filter_path=count" '{"query":{"term":{"tenant":"wayne"}}}'
holds 'j["count"] == 12' "all twelve, from the two shards the tenant may be on"

step "last year's tickets in an archive index, then an alias per tenant over both"
gone "/$OLD"
reqf PUT "/$OLD" requests/01-tickets-four-shards-routing-required.json
green "$OLD"
ndjson "/$OLD/_bulk?refresh=wait_for" data/03-last-years-tickets-the-archive.ndjson \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print({"errors": d["errors"], "items": len(d["items"])})'
expect_docs "$OLD" 3 "acme 2, initech 1"
note "each alias carries the tenant's filter and routing; tickets is where it writes, the archive is read only"
reqf POST "/_aliases" requests/05-an-alias-per-tenant.json
call 200 GET "/_cat/aliases/tenant-*?v&s=alias,index&h=alias,index,routing.index,routing.search,is_write_index"
call 200 GET "/_alias/tenant-acme"
holds 'j["tickets"]["aliases"]["tenant-acme"]["is_write_index"] is True and j["tickets-2025"]["aliases"]["tenant-acme"]["is_write_index"] is False' \
  "tenant-acme writes to tickets and only reads tickets-2025"
holds 'j["tickets"]["aliases"]["tenant-acme"]["index_routing"] == "acme" and j["tickets"]["aliases"]["tenant-acme"]["search_routing"] == "acme"' \
  "routing given once is both index_routing and search_routing"

step "a ticket written through the alias: the client names neither index nor routing"
call 201 PUT "/tenant-acme/_doc/t-1100?refresh=wait_for" @requests/06-a-ticket-written-through-the-alias.json
holds 'j["_index"] == "tickets" and j["result"] == "created"' "it went to the write index"
expect_docs "$IDX" 16 "fifteen and the new one"
expect_docs "$OLD" 3 "the archive is untouched"

step "reading through the alias: one tenant, both indices, nobody else's tickets"
call 200 GET "/tenant-acme/_search?filter_path=hits.total,hits.hits._index,hits.hits._id,hits.hits._source" \
  @requests/07-search-through-the-alias.json
holds "$ids"' == ["t-1100","t-1005","t-1001","t-0901"]' "acme's four login tickets, this year's and last year's"
note "initech's t-2002 and globex's t-3001 also say login; the alias filter keeps them out"
call 200 GET "/tenant-acme/_count?filter_path=count"
holds 'j["count"] == 11' "9 in tickets and 2 in the archive"
call 200 GET "/tenant-initech/_count?filter_path=count"
holds 'j["count"] == 5' "initech: 4 and 1"

step "paging with preference: the same copies answer every page of one session"
note "a custom preference string sends each page to the same shard copies, so a document"
note "cannot move between pages because a different replica had not refreshed yet"
call 200 GET "/tenant-acme/_search?preference=ann-session-42&filter_path=hits.hits._id" @requests/08-page-one.json
p1=$(python3 -c 'import json,sys; print(" ".join(h["_id"] for h in json.load(open(sys.argv[1]))["hits"]["hits"]))' "$LAST")
holds "$ids"' == ["t-1100","t-1008","t-1007","t-1006"]' "page one, newest first"
call 200 GET "/tenant-acme/_search?preference=ann-session-42&filter_path=hits.hits._id" @requests/09-page-two.json
holds "$ids"' == ["t-1005","t-1004","t-1003","t-1002"] and not set('"$ids"') & set("'"$p1"'".split())' \
  "page two, and nothing from page one repeated"
call 200 GET "/tenant-acme/_count?preference=_local&filter_path=count"
holds 'j["count"] == 11' "_local: the copies on the node that took the request, same count"

step "several tenants at once: _mget with a routing per id, _msearch with an alias per search"
call 200 GET "/$IDX/_mget?filter_path=docs._id,docs._routing,docs.found" @requests/10-several-tenants-by-id.json
holds '[d["found"] for d in j["docs"]] == [True, True, True, False]' \
  "three found; t-3001 asked for with initech's routing is not"
call 200 GET "/_msearch?filter_path=responses.hits.total.value,responses.status" @data/04-open-tickets-per-tenant-one-round.ndjson
holds '[r["hits"]["total"]["value"] for r in j["responses"]] == [6, 2, 2]' \
  "open tickets: acme 6, initech 2, globex 2 -- three tenants, one round trip"

step "_update_by_query on one routing value: escalate acme's long open tickets"
call 200 POST "/$IDX/_update_by_query?routing=acme&refresh=true" \
  @requests/11-escalate-one-tenants-long-tickets.json
holds 'j["updated"] == 3 and j["failures"] == []' "t-1002, t-1004 and t-1005, and nothing of anyone else's"
expect_hits 3 GET "/tenant-acme/_search" '{"query":{"term":{"status":"escalated"}}}' "acme has three escalated"
expect_hits 0 GET "/$IDX/_search" '{"query":{"bool":{"filter":[{"term":{"status":"escalated"}},{"bool":{"must_not":{"term":{"tenant":"acme"}}}}]}}}' \
  "no other tenant has any"

step "a terms lookup: the projects a user may see, read from the user's own document"
gone "/$USERS"
reqf PUT "/$USERS" requests/12-users-index-routed-by-tenant.json
green "$USERS"
ndjson "/$USERS/_bulk?refresh=wait_for" data/05-users-and-the-projects-each-may.ndjson \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print({"errors": d["errors"], "items": len(d["items"])})'
expect_docs "$USERS" 3 "ann and bob at acme, ian at initech"
note "the lookup names the routing too: the users index requires it, as tickets does"
call 200 GET "/tenant-acme/_search?filter_path=hits.total,hits.hits._id,hits.hits._source" \
  @requests/13-tickets-in-the-projects-ann-may-see.json
holds 'j["hits"]["total"]["value"] == 7 and all(h["_source"]["project"] in ("web","mobile") for h in j["hits"]["hits"])' \
  "seven acme tickets, all web or mobile -- change ann's document and the next search follows"

step "derived fields: a per-tenant cost computed at search time, never stored"
note "each tenant is billed at its own hourly rate; the rates travel with the request as params"
call 200 GET "/$IDX/_search?filter_path=aggregations" @requests/14-cost-per-tenant-at-search-time.json
holds '[(b["key"], b["minutes"]["value"], round(b["cost"]["value"], 2)) for b in j["aggregations"]["tenants"]["buckets"]] == [("acme", 495.0, 990.0), ("globex", 170.0, 425.0), ("initech", 110.0, 165.0)]' \
  "acme 495 min x 120/h = 990, globex 170 x 150/h = 425, initech 110 x 90/h = 165"
call 200 GET "/tenant-acme/_search?filter_path=hits.hits._index,hits.hits._id,hits.hits.fields" \
  @requests/15-one-tenants-expensive-tickets.json
holds "$ids"' == ["t-1007","t-1005","t-1004","t-1002","t-0902"]' "five acme tickets that cost 100 or more, one of them in the archive"

step "index.max_result_window: how deep one tenant's listing may page"
call 200 PUT "/$IDX/_settings" '{"index":{"max_result_window":10}}'
call 400 GET "/$IDX/_search?routing=acme" @requests/16-past-the-window.json
holds 'j["error"]["type"] == "illegal_argument_exception" and "[10]" in j["error"]["reason"]' \
  "from 8 + size 5 is past a window of 10: refused, not truncated"
call 200 GET "/$IDX/_search?routing=acme&filter_path=hits.hits._id" @requests/17-inside-the-window.json
holds "$ids"' == ["t-1006","t-1007","t-1008","t-1100"]' "from 5 + size 5 fits: the last four"
note "the window goes back to its default, so nothing else in the index is bound by it"
call 200 PUT "/$IDX/_settings" '{"index":{"max_result_window":null}}'
call 200 GET "/$IDX/_settings/index.max_result_window?include_defaults=true&flat_settings=true"
holds 'j["tickets"]["settings"] == {} and j["tickets"]["defaults"]["index.max_result_window"] == "10000"' "unset again: 10000, the default"

step "what this example leaves behind, checked rather than assumed"
expect_docs "$IDX" 16 "tickets"
expect_docs "$BIG" 12 "tickets-big"
expect_docs "$OLD" 3 "tickets-2025"
expect_docs "$USERS" 3 "users"
done_
