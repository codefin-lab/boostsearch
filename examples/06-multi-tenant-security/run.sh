#!/usr/bin/env bash
# One index, several tenants, and a filter that is part of the query rather
# than a proxy in front of it.
source "$(dirname "$0")/lib.sh"
AUTH="${AUTH:-admin:admin}"
CURL=(curl -sS --fail-with-body -u "$AUTH")
IDX=tickets

as() { # as USER PASS METHOD PATH [BODY]
  local u=$1 p=$2 m=$3 path=$4 b=${5-}
  if [ -n "$b" ]; then
    curl -sS -u "$u:$p" -X "$m" "$VS$path" -H 'Content-Type: application/json' -d "$b"
  else
    curl -sS -u "$u:$p" -X "$m" "$VS$path"
  fi
  echo
}

step "is security actually on"
req GET "/_plugins/_security/health"
note "if this is a 404 the node was started without security -- see the README"

step "support tickets belonging to two customers, with a field nobody but support may read"
gone "/$IDX"
reqf PUT "/$IDX" requests/01-support-tickets-belonging-to-two-customersx.json
ndjson "/$IDX/_bulk?refresh=wait_for" data/01-support-tickets-belonging-to-two-customers.ndjson
expect_docs "$IDX" 4 "two tickets each for two customers"

step "a role per tenant: what they may do, which documents, and which fields are hidden"
for t in northwind contoso; do
  req PUT "/_plugins/_security/api/roles/tenant_$t" "{
    \"cluster_permissions\": [\"cluster_composite_ops_ro\"],
    \"index_permissions\": [{
      \"index_patterns\": [\"$IDX\"],
      \"dls\": \"{\\\"term\\\": {\\\"tenant\\\": \\\"$t\\\"}}\",
      \"fls\": [\"~internal_note\"],
      \"masked_fields\": [\"contact_email\"],
      \"allowed_actions\": [\"read\"]
    }]
  }"
done

step "a user for each, and the mapping that gives them the role"
reqf PUT "/_plugins/_security/api/internalusers/nw-reader" requests/02-a-user-for-each-and-the.json
reqf PUT "/_plugins/_security/api/internalusers/co-reader" requests/03-a-user-for-each-and-the.json
req PUT "/_plugins/_security/api/rolesmapping/tenant_northwind" '{ "users": ["nw-reader"] }'
req PUT "/_plugins/_security/api/rolesmapping/tenant_contoso"   '{ "backend_roles": ["contoso"] }'
note "one mapped by user name, one by a backend role -- both are how it is done"

step "what the Northwind reader sees: two tickets, no internal note, a masked address"
as nw-reader correct-horse-battery-1 POST "/$IDX/_search" '{ "query": { "match_all": {} } }'

step "and what Contoso sees of the very same index"
as co-reader another-long-passphrase-2 POST "/$IDX/_search" '{ "query": { "match_all": {} } }'

step "the filter is inside the query, so counting cannot get round it"
as nw-reader correct-horse-battery-1 POST "/$IDX/_count" '{ "query": { "match_all": {} } }'
as nw-reader correct-horse-battery-1 POST "/$IDX/_search" '{
  "size": 0, "aggs": { "tenants": { "terms": { "field": "tenant" } },
                       "how_many": { "cardinality": { "field": "tenant" } } }
}'
note "one bucket, cardinality one -- an aggregation cannot count what it may not see"

step "nor can a direct get, nor mget, nor a termvector"
as nw-reader correct-horse-battery-1 GET "/$IDX/_doc/t3"
as nw-reader correct-horse-battery-1 POST "/_mget" '{ "docs": [{ "_index": "tickets", "_id": "t3" }, { "_index": "tickets", "_id": "t1" }] }'

step "and writing is refused outright: the role grants read"
as nw-reader correct-horse-battery-1 PUT "/$IDX/_doc/t9" '{ "tenant": "northwind", "subject": "sneaky" }'

step "what this caller is allowed, as the caller may ask"
as nw-reader correct-horse-battery-1 GET "/_plugins/_security/api/permissionsinfo"
as nw-reader correct-horse-battery-1 GET "/_plugins/_security/authinfo"

step "an action group, so the permission list is written once"
reqf PUT "/_plugins/_security/api/actiongroups/ticket_reader" requests/04-an-action-group-so-the-permission.json

step "support, who may see everything including the notes"
reqf PUT "/_plugins/_security/api/roles/support" requests/05-support-who-may-see-everything-including.json
req PUT "/_plugins/_security/api/internalusers/agent-smith" '{ "password": "support-passphrase-3" }'
req PUT "/_plugins/_security/api/rolesmapping/support" '{ "users": ["agent-smith"] }'
as agent-smith support-passphrase-3 POST "/$IDX/_search" '{
  "size": 4, "_source": ["tenant", "internal_note", "contact_email"], "query": { "match_all": {} } }'
note "the same documents, unmasked and with the note -- the difference is the role, not the index"

step "the whole configuration, read back"
req GET "/_plugins/_security/api/roles/tenant_northwind"
req GET "/_plugins/_security/api/rolesmapping"

step "a wrong password, and a user who does not exist"
as nw-reader not-the-password GET "/$IDX/_count"
as nobody at-all GET "/$IDX/_count"

step "what the security API refuses, and how it says so"
req PUT "/_plugins/_security/api/roles/bad_role" '{ "index_permissions": "this should be a list" }' || true
req PUT "/_plugins/_security/api/internalusers/tiny" '{ "password": "short" }' || true

step "what this example leaves behind, checked rather than assumed"
done_
