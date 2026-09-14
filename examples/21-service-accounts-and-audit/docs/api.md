# API surface -- 21. Machines that log in, and a record of what everyone did

Every request this example makes, in the order it makes them. Generated
from `run.sh`; if the two disagree, `run.sh` is right.

The caller is the administrator (`AUTH`) unless the row says otherwise.

| Step | Method | Path | Body |
|---|---|---|---|
| is security on, and where does the audit log go | `GET` | `/_plugins/_security/health` | none |
|  | `GET` | `/_plugins/_security/api/audit` | none |
| record granted requests as well as refused ones, and writes to orders-* | `PUT` | `/_plugins/_security/api/audit/config` | `requests/01-record-granted-requests-as-well-as.json` |
| the orders index, created by the administrator rather than the service | `DELETE` | `/$IDX`, `/payroll` | none |
|  | `DELETE` | `/_plugins/_security/api/internalusers/{svc-ingest,dash-viewer}` | none |
|  | `DELETE` | `/_plugins/_security/api/rolesmapping/{ingest_orders,dashboard_ro}` | none |
|  | `DELETE` | `/_plugins/_security/api/roles/{ingest_orders,dashboard_ro}` | none |
|  | `DELETE` | `/_plugins/_security/api/actiongroups/orders_writer` | none |
|  | `DELETE` | `/_plugins/_security/api/tenants/ops_dashboards` | none |
|  | `PUT` | `/$IDX` | `requests/02-the-orders-index-created-by-the.json` |
|  | `PUT` | `/payroll/_doc/p-1?refresh=true` | inline |
| an action group for writing, and nothing but writing | `PUT` | `/_plugins/_security/api/actiongroups/orders_writer` | `requests/03-an-action-group-for-writing-and.json` |
| the ingest service's role: write to orders-*, and that is all | `PUT` | `/_plugins/_security/api/roles/ingest_orders` | `requests/04-the-ingest-service-s-role-write.json` |
| the dashboard's role: read orders-*, and read-only in one tenant | `PUT` | `/_plugins/_security/api/tenants/ops_dashboards` | inline |
|  | `PUT` | `/_plugins/_security/api/roles/dashboard_ro` | `requests/05-the-dashboard-s-role-read-orders.json` |
| two machine users, and the mappings that give them their roles | `PUT` | `/_plugins/_security/api/internalusers/svc-ingest` | `requests/06-two-machine-users-and-the-mappings.json` |
|  | `PUT` | `/_plugins/_security/api/internalusers/dash-viewer` | `requests/07-two-machine-users-and-the-mappings.json` |
|  | `PUT` | `/_plugins/_security/api/rolesmapping/ingest_orders` | inline |
|  | `PUT` | `/_plugins/_security/api/rolesmapping/dashboard_ro` | inline |
| who each caller is, as the node sees it | `GET` | `/_plugins/_security/authinfo` as svc-ingest | none |
|  | `GET` | `/_plugins/_security/authinfo` as dash-viewer | none |
| the ingest service does its job | `POST` | `/$IDX/_bulk?refresh=wait_for` as svc-ingest | `data/01-orders-written-by-the-ingest-service.ndjson` |
|  | `PUT` | `/$IDX/_doc/o-1006?refresh=true` as svc-ingest | inline |
|  | `GET` | `/$IDX/_count` | none |
| and nothing else: five things it may not do, and what each refusal looks like | `POST` | `/$IDX/_search` as svc-ingest, 403 | inline |
|  | `DELETE` | `/$IDX/_doc/o-1001` as svc-ingest, 403 | none |
|  | `PUT` | `/payroll/_doc/p-2` as svc-ingest, 403 | inline |
|  | `POST` | `/_bulk?refresh=wait_for` as svc-ingest, 200 with one item 403 | `data/02-a-bulk-that-strays-outside-orders.ndjson` |
|  | `GET` | `/_plugins/_security/api/internalusers` as svc-ingest, 403 | none |
|  | `GET` | `/$IDX/_count`, `/payroll/_count` | none |
| the dashboard reads what it is for | `POST` | `/orders-*/_search` as dash-viewer | inline |
| and is refused the rest | `PUT` | `/$IDX/_doc/o-9999` as dash-viewer, 403 | inline |
|  | `POST` | `/orders-*,payroll/_search` as dash-viewer, 403 | inline |
|  | `POST` | `/security-auditlog-*/_search` as dash-viewer, 403 | inline |
|  | `GET` | `/_plugins/_security/api/permissionsinfo` as dash-viewer | none |
| the dashboard changes its own password | `PUT` | `/_plugins/_security/api/account` as dash-viewer, 400 | inline |
|  | `PUT` | `/_plugins/_security/api/account` as dash-viewer | `requests/08-the-dashboard-changes-its-own-password.json` |
|  | `GET` | `/orders-*/_count` as dash-viewer, old password, 401 | none |
|  | `GET` | `/orders-*/_count` as dash-viewer, new password | none |
| an administrator rotates the ingest service's password | `PATCH` | `/_plugins/_security/api/internalusers/svc-ingest` | `requests/09-an-administrator-rotates-the-ingest-service.json` |
|  | `GET` | `/_plugins/_security/authinfo` as svc-ingest, old password, 401 | none |
|  | `GET` | `/_plugins/_security/authinfo` as svc-ingest, new password | none |
|  | `GET` | `/_plugins/_security/api/internalusers/svc-ingest` | none |
| the audit log: what happened during this run, by category and by caller | `POST` | `/security-auditlog-*/_refresh` | none |
|  | `POST` | `/security-auditlog-*/_count` | inline, repeated until the last entry has arrived |
|  | `POST` | `/security-auditlog-*/_search` | inline |
| one refused request, as the log records it | `POST` | `/security-auditlog-*/_search` | inline |
| one granted request, and the document write it caused | `POST` | `/security-auditlog-*/_search` | inline |
| a failed login: the old password, still being tried | `POST` | `/security-auditlog-*/_search` | inline |
| the security changes, and who made them | `POST` | `/security-auditlog-*/_search` | inline |
| what this example leaves behind, checked rather than assumed | `GET` | `/$IDX/_count` | none |

## Endpoints touched

Path parameters and query strings removed, deduplicated:

- `/<var>`
- `/<var>/_bulk`
- `/<var>/_count`
- `/<var>/_doc/<var>`
- `/<var>/_refresh`
- `/<var>/_search`
- `/_bulk`
- `/_plugins/_security/api/account`
- `/_plugins/_security/api/actiongroups/<var>`
- `/_plugins/_security/api/audit`
- `/_plugins/_security/api/audit/config`
- `/_plugins/_security/api/internalusers`
- `/_plugins/_security/api/internalusers/<var>`
- `/_plugins/_security/api/permissionsinfo`
- `/_plugins/_security/api/roles/<var>`
- `/_plugins/_security/api/rolesmapping/<var>`
- `/_plugins/_security/api/tenants/<var>`
- `/_plugins/_security/authinfo`
- `/_plugins/_security/health`

## Request bodies

- [`requests/01-record-granted-requests-as-well-as.json`](../requests/01-record-granted-requests-as-well-as.json)
- [`requests/02-the-orders-index-created-by-the.json`](../requests/02-the-orders-index-created-by-the.json)
- [`requests/03-an-action-group-for-writing-and.json`](../requests/03-an-action-group-for-writing-and.json)
- [`requests/04-the-ingest-service-s-role-write.json`](../requests/04-the-ingest-service-s-role-write.json)
- [`requests/05-the-dashboard-s-role-read-orders.json`](../requests/05-the-dashboard-s-role-read-orders.json)
- [`requests/06-two-machine-users-and-the-mappings.json`](../requests/06-two-machine-users-and-the-mappings.json)
- [`requests/07-two-machine-users-and-the-mappings.json`](../requests/07-two-machine-users-and-the-mappings.json)
- [`requests/08-the-dashboard-changes-its-own-password.json`](../requests/08-the-dashboard-changes-its-own-password.json)
- [`requests/09-an-administrator-rotates-the-ingest-service.json`](../requests/09-an-administrator-rotates-the-ingest-service.json)
- [`data/01-orders-written-by-the-ingest-service.ndjson`](../data/01-orders-written-by-the-ingest-service.ndjson)
- [`data/02-a-bulk-that-strays-outside-orders.ndjson`](../data/02-a-bulk-that-strays-outside-orders.ndjson)
