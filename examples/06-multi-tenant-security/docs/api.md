# API surface -- 6. One index, several customers, no proxy in front

Every request this example makes, in the order it makes them. Generated
from `run.sh`; if the two disagree, `run.sh` is right.

| Step | Method | Path | Body |
|---|---|---|---|
| is security actually on | `GET` | `/_plugins/_security/health` | inline |
| support tickets belonging to two customers, with a field nobody but support may read | `PUT` | `/$IDX` | `requests/01-support-tickets-belonging-to-two-customersx.json` |
|  | `POST` | `/$IDX/_bulk?refresh=wait_for` | `data/01-support-tickets-belonging-to-two-customers.ndjson` |
| a role per tenant: what they may do, which documents, and which fields are hidden | `PUT` | `/_plugins/_security/api/roles/tenant_$t` | inline |
| a user for each, and the mapping that gives them the role | `PUT` | `/_plugins/_security/api/internalusers/nw-reader` | `requests/02-a-user-for-each-and-the.json` |
|  | `PUT` | `/_plugins/_security/api/internalusers/co-reader` | `requests/03-a-user-for-each-and-the.json` |
|  | `PUT` | `/_plugins/_security/api/rolesmapping/tenant_northwind` | inline |
|  | `PUT` | `/_plugins/_security/api/rolesmapping/tenant_contoso` | inline |
| an action group, so the permission list is written once | `PUT` | `/_plugins/_security/api/actiongroups/ticket_reader` | `requests/04-an-action-group-so-the-permission.json` |
| support, who may see everything including the notes | `PUT` | `/_plugins/_security/api/roles/support` | `requests/05-support-who-may-see-everything-including.json` |
|  | `PUT` | `/_plugins/_security/api/internalusers/agent-smith` | inline |
|  | `PUT` | `/_plugins/_security/api/rolesmapping/support` | inline |
| the whole configuration, read back | `GET` | `/_plugins/_security/api/roles/tenant_northwind` | inline |
|  | `GET` | `/_plugins/_security/api/rolesmapping` | inline |
| what the security API refuses, and how it says so | `PUT` | `/_plugins/_security/api/roles/bad_role` | inline |
|  | `PUT` | `/_plugins/_security/api/internalusers/tiny` | inline |

## Endpoints touched

Path parameters and query strings removed, deduplicated:

- `/<var>`
- `/<var>/_bulk`
- `/_plugins/_security/api/actiongroups/ticket_reader`
- `/_plugins/_security/api/internalusers/agent-smith`
- `/_plugins/_security/api/internalusers/co-reader`
- `/_plugins/_security/api/internalusers/nw-reader`
- `/_plugins/_security/api/internalusers/tiny`
- `/_plugins/_security/api/roles/bad_role`
- `/_plugins/_security/api/roles/support`
- `/_plugins/_security/api/roles/tenant_<var>`
- `/_plugins/_security/api/roles/tenant_northwind`
- `/_plugins/_security/api/rolesmapping`
- `/_plugins/_security/api/rolesmapping/support`
- `/_plugins/_security/api/rolesmapping/tenant_contoso`
- `/_plugins/_security/api/rolesmapping/tenant_northwind`
- `/_plugins/_security/health`

## Request bodies

- [`requests/01-support-tickets-belonging-to-two-customersx.json`](../requests/01-support-tickets-belonging-to-two-customersx.json)
- [`requests/02-a-user-for-each-and-the.json`](../requests/02-a-user-for-each-and-the.json)
- [`requests/03-a-user-for-each-and-the.json`](../requests/03-a-user-for-each-and-the.json)
- [`requests/04-an-action-group-so-the-permission.json`](../requests/04-an-action-group-so-the-permission.json)
- [`requests/05-support-who-may-see-everything-including.json`](../requests/05-support-who-may-see-everything-including.json)
- [`data/01-support-tickets-belonging-to-two-customers.ndjson`](../data/01-support-tickets-belonging-to-two-customers.ndjson)
