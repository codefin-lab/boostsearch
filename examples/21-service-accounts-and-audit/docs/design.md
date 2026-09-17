# Design notes

## A role per job, not a role per service

The two roles are named for what they do -- `ingest_orders`, `dashboard_ro` --
and the users are mapped onto them. When a second ingest service appears it gets
a user and a mapping, not a copy of the role, and when the job changes the role
changes once.

The dashboard is mapped by backend role (`dashboards`) and the ingest service by
user name, to show both. For machine accounts the user mapping is less of a
maintenance burden than it is for people, because there are few of them and they
change rarely; for people it is the backend role, supplied by the identity
provider, that scales.

## What "index-only" has to exclude

The ingest role is two lines and most of the design is in what it leaves out:

| Left out | Because |
|---|---|
| `read` | a writer that can read is a data export waiting for a leaked password |
| `indices:data/write/delete`, `update` | an ingest service adds; a compromised one should not be able to remove the evidence |
| `indices:admin/create`, `mapping/put` | the index and its mapping are made by the administrator (step 3), with `dynamic: strict` |
| any pattern but `orders-*` | the same write aimed at `payroll` must fail, and step 10 checks it does |

Creating the index in advance is what makes the last two possible. A writer that
may auto-create indices may create any index its pattern matches, with whatever
mapping its first document implies. In production an index template does the
same job for `orders-2026.10` and every month after.

`indices:data/write/bulk` is granted at cluster level because that is where the
bulk request as a whole is checked; each item is then checked against the index
it names, for `indices:data/write/bulk[s]` and the item's own action. The action
group grants both halves on the index. That two-level check is why a bulk can be
partly refused (step 10) -- and why the answer is 200.

## Read-only means refused, not filtered

A search over `orders-*,payroll` by the dashboard is refused outright. The
alternative -- answer for the indices it may read and drop the rest, which the
plugin offers as `do_not_fail_on_forbidden` -- makes a dashboard that asked for
the wrong thing report a smaller number with no sign that anything was left
out. For a machine account the refusal is the better default: the person who
configured it finds out on the first request.

## Tenants

`tenant_permissions` decide which saved-object spaces a user may open in a
dashboards front end, and whether read-only (`kibana_all_read`) or read-write
(`kibana_all_write`). They say nothing about indices. `authinfo` reports them as
a map from tenant name to `true` (writeable) or `false` (read-only), with the
user's private tenant always present. A service account that never logs into
the front end has no need of one; the dashboard account here does, because the
saved searches it runs live in `ops_dashboards`.

## Two ways to change a password, and why both exist

| | Who | Needs |
|---|---|---|
| `PUT _plugins/_security/api/account` | the user, for themselves | the current password |
| `PATCH _plugins/_security/api/internalusers/<name>` | an administrator, for anyone | the right to use the security API |

The first is for a service that rotates its own credential on a schedule: it
needs no administrative rights to do so, and cannot change anyone else's. The
second is for rotating a credential after it has leaked, when the current
password can no longer be trusted as proof of anything. The PATCH form also
changes one field at a time -- step 14 updates the `rotated` attribute beside
the password, without resending the user's backend roles.

Either way the change is immediate: the old password is refused on the next
request. There is no grace period in which both work, so a real rotation writes
the new secret to the service before changing it on the cluster, and accepts
one failed request in between -- or uses a second account for the overlap.

## Tokens

Every caller here authenticates with HTTP basic auth. That is the plugin's
internal user database, and it is what a service account on a cluster without an
identity provider uses. `POST _plugins/_security/api/authtoken` exists, but it
is the SAML token exchange and answers 401 without a SAML domain configured.

The security plugin has no API-key endpoint. Its token-based alternatives are
not for a plain service either: `POST
_plugins/_security/api/generateonbehalfoftoken` answers 400 with `The
OnBehalfOf token generating API has been disabled` while on-behalf-of tokens
are off in the security configuration, which is the default; and a user
written with `attributes.service: "true"` and no password is a service account
for extensions, for which `POST .../internalusers/<name>/authtoken` answers
`An auth token could not be generated for the specified account.` A
long-lived basic-auth password, rotated, is the service credential this
example shows.

## The audit log

### Where it goes

`plugins.security.audit.type` -- `VELOSEARCH_AUDIT_TYPE` in the environment --
chooses the sink:

| Type | Where |
|---|---|
| `internal_opensearch` | an index on this node, `security-auditlog-YYYY.MM.dd` by default (`audit.config.index`) |
| `external_opensearch` | an index on another cluster, over HTTP (`audit.config.http_endpoints`) |
| `webhook` | a POST per entry (`audit.config.webhook.url`, `.format`) |
| `log4j`, `debug` | the node's standard error, one JSON object per line |

This example uses the first so it can read the entries back with `_search`.
It is the wrong choice for a real deployment for the obvious reason: an
administrator of the cluster can delete the index that records what the
administrator did. A log meant to hold up afterwards goes somewhere the people
it records cannot reach -- another cluster, or a log pipeline.

Delivery is asynchronous: an entry is queued and written by a separate thread,
so a request is never slowed by its own audit record. That is why step 15 waits
for the last expected entry before reading, and why a sink that cannot keep up
loses entries (they are dropped and counted) rather than stalling requests.

### What is recorded by default, and what step 2 changes

The shipped configuration disables `AUTHENTICATED` and `GRANTED_PRIVILEGES` on
both layers. What is left is refusals, failed logins, index administration and
security-configuration changes -- a log of what went wrong. That is cheap, and
it cannot answer the question usually asked afterwards: *who read this, and
when*. Step 2 turns `GRANTED_PRIVILEGES` on, keeps `AUTHENTICATED` off (it
duplicates the grant for every request), ignores `indices:admin/refresh` so the
example's own refreshes of the log do not fill it, and watches writes to
`orders-*` so each document written leaves a `COMPLIANCE_DOC_WRITE`.

The cost is volume. On this run the administrator's setup alone is dozens of
entries; on a busy cluster every search is one. `ignore_users` and
`ignore_requests` are the controls, and the usual shape is to ignore the
high-volume, low-risk machine reads and keep everything else.

### The categories this example produces

| Category | Layer | Written when |
|---|---|---|
| `MISSING_PRIVILEGES` | transport | an action was refused |
| `GRANTED_PRIVILEGES` | transport, or REST for the security API | an action was allowed (only after step 2) |
| `FAILED_LOGIN` | REST | the credentials were wrong or unknown |
| `INDEX_EVENT` | transport | an index-administration action ran |
| `COMPLIANCE_DOC_WRITE` | -- | a document in a watched index was written |
| `COMPLIANCE_INTERNAL_CONFIG_WRITE` | -- | users, roles, mappings, action groups or tenants changed |
| `COMPLIANCE_INTERNAL_CONFIG_READ` | -- | the security configuration was read |

A transport-layer entry names the action (`audit_request_privilege`), the
request type, the indices as named and as resolved, and the document id where
there is one. A REST-layer entry names the method, the path, the parameters and
the headers, less `Authorization` while `exclude_sensitive_headers` is on.

### What the configuration-change entry does not say

`COMPLIANCE_INTERNAL_CONFIG_WRITE` records that `internalusers` was updated and
by whom, not which user or what changed. When an administrator makes the change,
the REST-layer `GRANTED_PRIVILEGES` entry for the same request carries the path
(`/_plugins/_security/api/internalusers/svc-ingest`), so the two together say
which user. The body of that entry is recorded as `"__SENSITIVE__"` rather than
the JSON that was sent, because it holds a password -- `log_request_body` does
not reach into a credential change. When a user changes their own password
through `account`, the configuration write naming them as the effective user
is the only record.

### What was not found in the log

Two refusals the example makes leave no `MISSING_PRIVILEGES` entry on this
server: the security REST API refusing `svc-ingest` in step 10, and the payroll
item refused inside the bulk in step 10. The bulk item's refusal is visible only
in the bulk's own answer. The example checks the log only for the refusals it
does record.

## Reading the log back without being fooled by earlier runs

The audit index is never deleted by the example -- deleting an audit log to make
a demonstration tidy is the wrong lesson. Instead step 2 notes the time, and
every audit query filters on `@timestamp` from then. Rerunning adds a second
run's entries beside the first and the checks still count only one run.

Every string field in the audit index is dynamically mapped as `text` with a
`.keyword` subfield, so exact matches and aggregations use
`audit_category.keyword`, not `audit_category`.

## What would change at scale

- **Send the log off the cluster.** `external_opensearch` or `webhook`, to a
  store the cluster's administrators cannot write to.
- **One index a day grows without limit.** Put an index-management policy on
  `security-auditlog-*` (example 02) to roll it over and delete it after the
  retention period the auditors ask for, and no sooner.
- **Granted-privilege logging on reads is the expensive part.** Keep it for
  people and for writes; for high-volume machine readers, `ignore_users` on the
  specific account is usually the right trade, and it should be a decision
  written down rather than a default nobody chose.
- **Machine credentials should not be shared.** One user per service instance
  class, so the `audit_request_effective_user` in an entry names something you
  can go and look at.
