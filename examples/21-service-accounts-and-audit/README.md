# 21. Machines that log in, and a record of what everyone did

Most of the accounts on a search cluster are not people. An ingest service
writes, a dashboard reads, a backup job snapshots, and each of them holds a
password in a configuration file somewhere. The question worth asking of each
is not "can it do its job" but "what else can it do", because that is what a
leaked credential will be used for.

This example gives two machine users exactly one job each -- an ingest service
that may add orders and nothing else, a dashboard that may read them and
nothing else -- and then tries every other thing with their credentials to show
each refusal. It changes one password the way a user does and rotates the other
the way an administrator does. Then it reads the audit log back and finds the
refusals, the successes, the failed logins and the password changes in it.

Example 06 is about which documents and fields a reader sees; this one is about
which actions a caller may take at all, and the record of what they took.

## What it shows

| Step | Feature |
|---|---|
| 1 | `_plugins/_security/health`; `_plugins/_security/api/audit` and where entries go |
| 2 | `PUT _plugins/_security/api/audit/config`: granted requests, `ignore_requests`, `write_watched_indices` |
| 3 | an index with a strict mapping, so the writer needs no index-admin rights |
| 4 | an action group holding exactly the actions a writer needs |
| 5 | a least-privilege role: `indices:data/write/bulk` at cluster level, the action group on `orders-*` |
| 6 | a read-only role with `tenant_permissions`; `_plugins/_security/api/tenants` |
| 7 | internal users with backend roles and attributes; role mappings by user and by backend role |
| 8 | `_plugins/_security/authinfo` as each caller: roles, backend roles, attributes, tenants |
| 9 | the ingest service writing, by `_bulk` and by `_doc` |
| 10 | four refused actions and a partly refused bulk; the `security_exception` body and the REST API's own refusal |
| 11-12 | the dashboard reading, and refused a write, a search naming one forbidden index, and the audit log |
| 12 | `_plugins/_security/api/permissionsinfo` |
| 13 | `PUT _plugins/_security/api/account`: a user changes their own password; the old one gets 401 |
| 14 | `PATCH _plugins/_security/api/internalusers/<name>`: an administrator rotates one |
| 15 | the audit index, aggregated by `audit_category` and `audit_request_effective_user` |
| 16 | a `MISSING_PRIVILEGES` entry |
| 17 | a `GRANTED_PRIVILEGES` entry and the `COMPLIANCE_DOC_WRITE` beside it |
| 18 | a `FAILED_LOGIN` entry, and the Authorization header left out of it |
| 19 | `COMPLIANCE_INTERNAL_CONFIG_WRITE`: who changed the security configuration |

## Running it

```bash
make serve      # a node configured for this example, port 9281, foreground
make run        # the example, in another terminal
```

`make check` validates the scripts and every request body without a server;
`make clean` deletes what the example left behind. Copy `.env.example` to
`.env` to change the address or anything else.

The longer form, and what this example needs of the node:

Security is off by default, so this example needs a node started with it on,
and with the audit log written to an index on the node itself:

```bash
VELOSEARCH_DISABLED=false \
VELOSEARCH_RESTAPI_ROLES_ENABLED=all_access \
VELOSEARCH_AUDIT_TYPE=internal_opensearch \
VELOSEARCH_DATA=/tmp/velo-audit \
./target/release/velosearch &

examples/21-service-accounts-and-audit/run.sh
```

`AUTH` sets the administrator credentials (default
`admin:Example-Passphrase-2026`, the password `node.sh` gives the node in
`VELOSEARCH_INITIAL_ADMIN_PASSWORD`; a node with security on has no default
administrator). Every
password in the example is an obvious example value; `.env.example` lists them.

## What to look for

- **Step 8** is the first thing to run against any new service account.
  `svc-ingest` has `roles: ["ingest_orders", "own_index"]`, `dash-viewer` has
  `["dashboard_ro", "own_index"]` through its backend role `dashboards`, and
  `tenants` shows `"ops_dashboards": false` -- false meaning read-only, not
  absent. `own_index` is there because the default configuration maps it to
  every user.
- **Step 10** is the point of least privilege. The ingest service wrote seven
  orders and still cannot read one back (`no permissions for
  [indices:data/read/search]`), cannot delete one it wrote
  (`[indices:data/write/delete]`), and cannot write the same document shape to
  `payroll`. The refusal names the missing action, which is what you add to the
  role if the refusal was wrong.
- **Step 10's bulk** answers 200 with `"errors": true`: the orders item is 201
  and the payroll item is 403 with `no permissions for
  [indices:data/write/bulk[s], indices:data/write/index]`. A client that checks
  only the HTTP status of a bulk will not notice it was refused.
- **The security API refuses differently.** `GET
  _plugins/_security/api/internalusers` as the service is 403 with
  `{"status":"FORBIDDEN","message":"No permission to access REST API: ..."}`,
  not a `security_exception`. A client parsing refusals needs both shapes.
- **Step 12**: a search over `orders-*,payroll` is refused outright rather than
  answered with the orders alone. The dashboard learns it asked for too much
  instead of getting a quietly smaller number.
- **Steps 13 and 14** are 401 for the old password, not 403. The caller is not
  known at all, and `authinfo` with the new one reports the same user and roles.
  The administrator's `GET` of the user shows `"hash": ""` -- the API never
  returns a hash.
- **Step 15** on a fresh node counts 69 entries for the run (77 on a rerun,
  where step 3's deletions are configuration writes too).
  `GRANTED_PRIVILEGES` is the largest category at 37, 31 of them the
  administrator setting up and reading; `COMPLIANCE_DOC_WRITE` is 7, one per
  order; `MISSING_PRIVILEGES` is 6, three for each machine user;
  `FAILED_LOGIN` is 2, one each, from the old passwords. Two refusals are
  missing from those six -- see "What was not found in the log" in
  `docs/design.md`.
- **Step 16** records the refused delete with
  `audit_request_privilege: "indices:data/write/delete"`,
  `audit_transport_request_type: "DeleteRequest"`, `audit_trace_doc_id:
  "o-1001"` and the index. Nothing in it says the document survived; the 7
  from `_count` in step 10 does.
- **Step 18** records the failed login at the REST layer with the method, the
  path `/orders-*/_count` and the headers -- and no `authorization` header, so
  the wrong password is not itself written into the log.
- **Step 19** shows `dash-viewer` as the effective user of a
  `COMPLIANCE_INTERNAL_CONFIG_WRITE` on `internalusers`: a user changing their
  own password is a change to the security configuration, and is logged as one.

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
| `requests/` | 9 request bodies, one file each |
| `data/` | 2 bulk document sets |

## Leaves behind

The indices `orders-2026.09` (7 documents) and `payroll` (1), the roles
`ingest_orders` and `dashboard_ro`, their role mappings, the action group
`orders_writer`, the tenant `ops_dashboards`, and the users `svc-ingest` and
`dash-viewer` with their rotated passwords. The audit configuration stays as
step 2 set it, and `security-auditlog-YYYY.MM.dd` keeps every entry from every
run -- the example reads only the entries from its own run and never deletes
the log. Rerunning replaces everything but the log; `make clean` removes the
indices and security objects; throwing away the data directory removes the lot.
