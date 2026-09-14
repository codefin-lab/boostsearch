#!/usr/bin/env bash
# Machines that log in, each allowed exactly one job, and a record of what
# every caller did -- the refusals as well as the successes.

# with no .env, the node `make serve` starts; anything in .env or the
# environment wins
[ -f "$(dirname "$0")/.env" ] || { BS="${BS:-http://127.0.0.1:9281}"; AUTH="${AUTH:-admin:admin}"; }
source "$(dirname "$0")/lib.sh"

IDX=orders-2026.09
INGEST=svc-ingest:ingest-example-passphrase-1
INGEST_NEW=svc-ingest:ingest-rotated-passphrase-4
DASH=dash-viewer:dashboard-example-passphrase-2
DASH_NEW=dash-viewer:dashboard-rotated-passphrase-3
LAST=$(mktemp); trap 'rm -f "$LAST"' EXIT

# expect_status WANT GOT WHAT -- a status code, checked
expect_status() {
  if [ "$2" = "$1" ]; then
    printf '   \033[32mok\033[0m  %s -- %s\n' "$2" "$3"
  else
    printf '   \033[31mNOT WHAT WAS EXPECTED\033[0m  %s, expected %s -- %s\n' "$2" "$1" "$3" >&2
    _fails=$((_fails + 1))
  fi
}

# as USER:PASS WANT METHOD PATH [BODY] -- a request as a caller other than the
# administrator. A refusal is an answer worth reading, so the body is printed
# whatever the status, and the status is checked against WANT.
as() {
  local who=$1 want=$2 m=$3 p=$4 b=${5-} code
  local args=(-sS -o "$LAST" -w '%{http_code}' -u "$who" -X "$m" "$BS$p")
  case "$b" in
    '')            ;;
    @*.ndjson)     args+=(-H 'Content-Type: application/x-ndjson' --data-binary "$b") ;;
    @*)            args+=(-H 'Content-Type: application/json' --data-binary "$b") ;;
    *)             args+=(-H 'Content-Type: application/json' -d "$b") ;;
  esac
  code=$(curl "${args[@]}")
  cat "$LAST"; echo
  expect_status "$want" "$code" "${who%%:*} $m $p"
}

# holds 'PYTHON EXPRESSION over j' WHAT -- a fact about the last answer `as` printed
holds() {
  if python3 -c 'import json,sys; j = json.load(open(sys.argv[1])); sys.exit(0 if eval(sys.argv[2]) else 1)' "$LAST" "$1" 2>/dev/null; then
    printf '   \033[32mok\033[0m  %s\n' "$2"
  else
    printf '   \033[31mNOT WHAT WAS EXPECTED\033[0m  %s  (%s)\n' "$2" "$1" >&2
    _fails=$((_fails + 1))
  fi
}

# audit_query BODY-JSON -- search every audit index, as the administrator
audit_query() { as "$AUTH" 200 POST "/security-auditlog-*/_search" "$1"; }

# since -- a range clause for "written during this run", so an audit log that
# already holds earlier runs does not answer for this one
since() { printf '{ "range": { "@timestamp": { "gte": "%s" } } }' "$T0"; }

step "is security on, and where does the audit log go"
req GET "/_plugins/_security/health"
req GET "/_plugins/_security/api/audit"
note "node.sh sets audit.type to internal_opensearch: every entry is a document in"
note "security-auditlog-YYYY.MM.dd on this node, searched like any other index"

step "record granted requests as well as refused ones, and writes to orders-*"
reqf PUT "/_plugins/_security/api/audit/config" requests/01-record-granted-requests-as-well-as.json
note "the shipped default leaves GRANTED_PRIVILEGES out -- a log of refusals alone cannot"
note "answer 'who read this', which is usually the question asked afterwards"
sleep 1
T0=$(python3 -c 'import datetime; print(datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z")')
note "this run's audit entries are the ones at or after $T0"

step "the orders index, created by the administrator rather than the service"
gone "/$IDX"
gone "/payroll"
for u in svc-ingest dash-viewer; do gone "/_plugins/_security/api/internalusers/$u"; done
for r in ingest_orders dashboard_ro; do
  gone "/_plugins/_security/api/rolesmapping/$r"; gone "/_plugins/_security/api/roles/$r"
done
gone "/_plugins/_security/api/actiongroups/orders_writer"
gone "/_plugins/_security/api/tenants/ops_dashboards"
reqf PUT "/$IDX" requests/02-the-orders-index-created-by-the.json
req PUT "/payroll/_doc/p-1?refresh=true" '{ "employee": "e-3", "monthly": 4200 }'
note "a strict mapping, made here, so the service needs no right to create indices or add fields"

step "an action group for writing, and nothing but writing"
reqf PUT "/_plugins/_security/api/actiongroups/orders_writer" requests/03-an-action-group-for-writing-and.json

step "the ingest service's role: write to orders-*, and that is all"
reqf PUT "/_plugins/_security/api/roles/ingest_orders" requests/04-the-ingest-service-s-role-write.json
note "a bulk is checked twice: indices:data/write/bulk as a cluster action, then"
note "indices:data/write/bulk[s] and the item's own action on each index it names"

step "the dashboard's role: read orders-*, and read-only in one tenant"
req PUT "/_plugins/_security/api/tenants/ops_dashboards" '{ "description": "saved objects the operations team shares" }'
reqf PUT "/_plugins/_security/api/roles/dashboard_ro" requests/05-the-dashboard-s-role-read-orders.json

step "two machine users, and the mappings that give them their roles"
reqf PUT "/_plugins/_security/api/internalusers/svc-ingest" requests/06-two-machine-users-and-the-mappings.json
reqf PUT "/_plugins/_security/api/internalusers/dash-viewer" requests/07-two-machine-users-and-the-mappings.json
req PUT "/_plugins/_security/api/rolesmapping/ingest_orders" '{ "users": ["svc-ingest"] }'
req PUT "/_plugins/_security/api/rolesmapping/dashboard_ro" '{ "backend_roles": ["dashboards"] }'
note "the service is mapped by name; the dashboard by a backend role, so a second dashboard"
note "account needs a user and nothing else"

step "who each caller is, as the node sees it"
as "$INGEST" 200 GET "/_plugins/_security/authinfo"
holds '"ingest_orders" in j["roles"] and "dashboard_ro" not in j["roles"]' "svc-ingest has ingest_orders and not dashboard_ro"
holds 'j["custom_attribute_names"] == ["attr.internal.owner", "attr.internal.rotated"]' "its attributes are there to be used in a DLS query"
as "$DASH" 200 GET "/_plugins/_security/authinfo"
holds '"dashboard_ro" in j["roles"] and j["backend_roles"] == ["dashboards"]' "dash-viewer has dashboard_ro, through its backend role"
holds 'j["tenants"].get("ops_dashboards") is False' "ops_dashboards is listed as false: read-only, not absent"

step "the ingest service does its job"
as "$INGEST" 200 POST "/$IDX/_bulk?refresh=wait_for" @data/01-orders-written-by-the-ingest-service.ndjson
holds 'j["errors"] is False and len(j["items"]) == 5' "five items, no errors"
as "$INGEST" 201 PUT "/$IDX/_doc/o-1006?refresh=true" \
  '{ "order_id": "o-1006", "customer": "c-08", "sku": "MS-220", "quantity": 2, "amount": 49.00, "placed_at": "2026-09-05T10:00:00Z" }'
expect_docs "$IDX" 6 "five from the bulk, one on its own"

step "and nothing else: five things it may not do, and what each refusal looks like"
as "$INGEST" 403 POST "/$IDX/_search" '{ "size": 1 }'
holds 'j["error"]["type"] == "security_exception" and "indices:data/read/search" in j["error"]["reason"]' "reading is refused, naming the action"
as "$INGEST" 403 DELETE "/$IDX/_doc/o-1001"
holds '"indices:data/write/delete" in j["error"]["reason"]' "deleting a document it wrote is refused too -- index-only means index-only"
as "$INGEST" 403 PUT "/payroll/_doc/p-2" '{ "employee": "e-9", "monthly": 1 }'
holds '"indices:data/write/index" in j["error"]["reason"]' "the same write, outside orders-*, is refused"
as "$INGEST" 200 POST "/_bulk?refresh=wait_for" @data/02-a-bulk-that-strays-outside-orders.ndjson
holds 'j["errors"] is True and j["items"][0]["index"]["status"] == 201 and j["items"][1]["index"]["status"] == 403' "inside a bulk the refusal is per item: 200 overall, the orders item written, the payroll item 403"
as "$INGEST" 403 GET "/_plugins/_security/api/internalusers"
holds 'j["status"] == "FORBIDDEN"' "the security API refuses in a shape of its own, not security_exception"
expect_docs "$IDX" 7 "six, plus the one bulk item that was allowed; the refused delete deleted nothing"
expect_docs payroll 1 "neither refused write reached payroll"

step "the dashboard reads what it is for"
as "$DASH" 200 POST "/orders-*/_search" '{
  "size": 0,
  "aggs": { "revenue": { "sum": { "field": "amount" } },
            "by_sku":  { "terms": { "field": "sku", "size": 3 } } }
}'
holds 'j["hits"]["total"]["value"] == 7 and abs(j["aggregations"]["revenue"]["value"] - 531.55) < 0.005' "seven orders, 531.55 in total"

step "and is refused the rest"
as "$DASH" 403 PUT "/$IDX/_doc/o-9999" '{ "order_id": "o-9999", "customer": "c-00", "sku": "X", "quantity": 1, "amount": 0 }'
holds '"indices:data/write/index" in j["error"]["reason"]' "a read-only role may not write"
as "$DASH" 403 POST "/orders-*,payroll/_search" '{ "size": 0 }'
holds '"indices:data/read/search" in j["error"]["reason"]' "one forbidden index in the list refuses the whole search, not a partial answer"
as "$DASH" 403 POST "/security-auditlog-*/_search" '{ "size": 0 }'
holds 'j["status"] == 403' "the audit log is not readable by the people it records"
as "$DASH" 200 GET "/_plugins/_security/api/permissionsinfo"
holds 'j["has_api_access"] is False' "and it can find out for itself that it may not administer security"

step "the dashboard changes its own password"
as "$DASH" 400 PUT "/_plugins/_security/api/account" '{ "current_password": "not-the-password", "password": "dashboard-rotated-passphrase-3" }'
holds '"current password" in j["message"]' "the current password is checked first"
as "$DASH" 200 PUT "/_plugins/_security/api/account" @requests/08-the-dashboard-changes-its-own-password.json
as "$DASH" 401 GET "/orders-*/_count"
note "the old password: 401, not 403 -- the caller is not known at all now"
as "$DASH_NEW" 200 GET "/orders-*/_count"
holds 'j["count"] == 7' "the new one works at once"

step "an administrator rotates the ingest service's password"
reqf PATCH "/_plugins/_security/api/internalusers/svc-ingest" requests/09-an-administrator-rotates-the-ingest-service.json
as "$INGEST" 401 GET "/_plugins/_security/authinfo"
as "$INGEST_NEW" 200 GET "/_plugins/_security/authinfo"
holds 'j["user_name"] == "svc-ingest" and "ingest_orders" in j["roles"]' "same user, same roles, new secret"
req GET "/_plugins/_security/api/internalusers/svc-ingest"
note "the hash is reported as an empty string: the API never hands a hash back"

step "the audit log: what happened during this run, by category and by caller"
# delivery is asynchronous and in order, so wait for the last entry this run
# expects -- the ingest service's old password, refused in step 14
for i in $(seq 1 20); do
  quiet POST "/security-auditlog-*/_refresh"
  n=$("${CURL[@]}" -X POST "$BS/security-auditlog-*/_count" -H 'Content-Type: application/json' -d "{
    \"query\": { \"bool\": { \"filter\": [ $(since),
      { \"term\": { \"audit_category.keyword\": \"FAILED_LOGIN\" } },
      { \"term\": { \"audit_request_effective_user.keyword\": \"svc-ingest\" } } ] } } }" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["count"])')
  [ "$n" -ge 1 ] && break
  sleep 0.5
done
audit_query "{
  \"size\": 0, \"query\": $(since),
  \"aggs\": {
    \"category\": { \"terms\": { \"field\": \"audit_category.keyword\", \"size\": 20 },
      \"aggs\": { \"who\": { \"terms\": { \"field\": \"audit_request_effective_user.keyword\" } } } } }
}"
holds 'any(b["key"] == "MISSING_PRIVILEGES" for b in j["aggregations"]["category"]["buckets"])' "refusals are there"
holds 'any(b["key"] == "GRANTED_PRIVILEGES" for b in j["aggregations"]["category"]["buckets"])' "and, with the change in step 2, the granted requests"
holds 'any(b["key"] == "FAILED_LOGIN" for b in j["aggregations"]["category"]["buckets"])' "and the logins that failed"

step "one refused request, as the log records it"
audit_query "{
  \"size\": 1, \"sort\": [ { \"@timestamp\": \"asc\" } ],
  \"query\": { \"bool\": { \"filter\": [ $(since),
    { \"term\": { \"audit_category.keyword\": \"MISSING_PRIVILEGES\" } },
    { \"term\": { \"audit_request_effective_user.keyword\": \"svc-ingest\" } },
    { \"term\": { \"audit_request_privilege.keyword\": \"indices:data/write/delete\" } } ] } }
}"
holds 'j["hits"]["total"]["value"] == 1' "exactly one entry for the refused delete"
holds 'j["hits"]["hits"][0]["_source"]["audit_trace_doc_id"] == "o-1001" and j["hits"]["hits"][0]["_source"]["audit_trace_indices"] == ["'"$IDX"'"]' "naming the document and the index it was aimed at"

step "one granted request, and the document write it caused"
audit_query "{
  \"size\": 5, \"sort\": [ { \"@timestamp\": \"asc\" } ],
  \"_source\": [ \"@timestamp\", \"audit_category\", \"audit_request_effective_user\", \"audit_request_privilege\",
                 \"audit_transport_request_type\", \"audit_trace_indices\", \"audit_trace_resolved_indices\",
                 \"audit_trace_doc_id\", \"audit_compliance_operation\", \"audit_compliance_doc_version\" ],
  \"query\": { \"bool\": { \"filter\": [ $(since),
    { \"term\": { \"audit_request_effective_user.keyword\": \"svc-ingest\" } },
    { \"terms\": { \"audit_category.keyword\": [ \"GRANTED_PRIVILEGES\", \"COMPLIANCE_DOC_WRITE\" ] } },
    { \"terms\": { \"audit_trace_doc_id.keyword\": [ \"o-1006\" ] } } ] } }
}"
holds 'sorted(h["_source"]["audit_category"] for h in j["hits"]["hits"]) == ["COMPLIANCE_DOC_WRITE", "GRANTED_PRIVILEGES"]' "the privilege granted, and the document written"
holds 'any(h["_source"].get("audit_compliance_operation") == "CREATE" for h in j["hits"]["hits"])' "the write is recorded as a CREATE, at version 1"

step "a failed login: the old password, still being tried"
audit_query "{
  \"size\": 1, \"sort\": [ { \"@timestamp\": \"asc\" } ],
  \"query\": { \"bool\": { \"filter\": [ $(since),
    { \"term\": { \"audit_category.keyword\": \"FAILED_LOGIN\" } },
    { \"term\": { \"audit_request_effective_user.keyword\": \"dash-viewer\" } } ] } }
}"
holds 'j["hits"]["hits"][0]["_source"]["audit_request_layer"] == "REST" and j["hits"]["hits"][0]["_source"]["audit_rest_request_path"] == "/orders-*/_count"' "a REST-layer entry, with the path that was tried"
holds '"authorization" not in j["hits"]["hits"][0]["_source"]["audit_rest_request_headers"]' "and without the Authorization header: the password is not written into the log"

step "the security changes, and who made them"
audit_query "{
  \"size\": 10, \"sort\": [ { \"@timestamp\": \"asc\" } ],
  \"_source\": [ \"@timestamp\", \"audit_category\", \"audit_request_effective_user\", \"audit_trace_doc_id\", \"audit_compliance_operation\" ],
  \"query\": { \"bool\": { \"filter\": [ $(since),
    { \"term\": { \"audit_category.keyword\": \"COMPLIANCE_INTERNAL_CONFIG_WRITE\" } },
    { \"term\": { \"audit_trace_doc_id.keyword\": \"internalusers\" } } ] } }
}"
holds 'any(h["_source"]["audit_request_effective_user"] == "dash-viewer" for h in j["hits"]["hits"])' "dash-viewer's own password change is a configuration write by dash-viewer"
note "what changed is 'internalusers', not which user or to what -- see docs/design.md"

step "what this example leaves behind, checked rather than assumed"
expect_docs "$IDX" 7 "the orders"
done_
