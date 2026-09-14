# Troubleshooting

## `run.sh` stops at once with `no server at ...`

Either there is no node at `BS`, or security is on and the credentials are
wrong: the first request is made with `AUTH`, and a 401 looks the same as no
server to it. Start the node with `make serve`, and check `AUTH` (default
`admin:admin`).

## Step 1 answers 404, and everything after it fails

Security is off. It is off by default; this example needs a node started with
it on:

```bash
make serve
```

which sets `BOOSTSEARCH_DISABLED=false` and
`BOOSTSEARCH_RESTAPI_ROLES_ENABLED=all_access`. If you are using your own node,
start it with both.

## `PUT /_plugins/_security/api/...` returns 403 for the administrator

The administrator's role is not allowed to use the security REST API.
`BOOSTSEARCH_RESTAPI_ROLES_ENABLED` names the roles that may; it must include
`all_access` for this example.

## Creating a user fails with `Password is similar to user name`

A password may not contain the user name (for names of four characters or
more). `svc-ingest` may not have a password containing `svc-ingest`. If you
change the example's passwords, avoid it.

## The ingest service's bulk answers 200 but the documents are not there

Look at `errors` and each item's `status`. A bulk is checked as a whole and
then item by item; an item aimed at an index the role does not cover is
refused with 403 inside a 200 answer. The role needs, on the index pattern,
`indices:data/write/bulk*` as well as `indices:data/write/index` -- the
`orders_writer` action group has both.

## A write by the ingest service is refused with `indices:admin/mapping/put` or `indices:admin/create`

The document has a field the mapping does not, or the index does not exist.
The role deliberately cannot create either. Create the index (step 3) or add
the field to the mapping as the administrator; the `dynamic: strict` mapping
refuses an unknown field with a 400 before the permission question arises.

## A caller that should be allowed is refused

Read the refusal: `no permissions for [<action>]` names exactly what is missing.
Then ask the node what the caller has:

```bash
curl -su svc-ingest:ingest-rotated-passphrase-4 localhost:9281/_plugins/_security/authinfo | jq '.roles, .backend_roles'
```

If the role is missing, the mapping is wrong -- by user name, or by backend
role. If the role is there, read it back and compare its `index_patterns` and
`allowed_actions` with the action and index in the refusal:

```bash
curl -su admin:admin localhost:9281/_plugins/_security/api/roles/ingest_orders | jq
```

## The old password still works for a moment

It should not, and on this node it does not: the credential cache is dropped
when a user changes. If you see it on another deployment, it is the
authentication cache (`plugins.security.cache.ttl_minutes`) not being
invalidated across nodes.

## Step 13 fails with `Could not validate your current password.`

That is the first request of the step, and it is supposed to: it sends a wrong
current password. If the second request fails the same way, the user's password
is not what the example expects -- a run that stopped between steps 13 and 14
left the rotated one in place. Rerun from the beginning; step 3 recreates both
users with their original passwords.

## Steps 15-19 find nothing

- **The audit log is going somewhere else.** Step 1 shows the configuration but
  not the sink; check how the node was started. `BOOSTSEARCH_AUDIT_TYPE` must be
  `internal_opensearch` for the entries to be in `security-auditlog-*`. With
  `log4j` or `debug` they are on the node's standard error instead, one line
  each, beginning `[INFO][audit]` or `AUDIT`.
- **The clocks disagree.** Every audit query filters on `@timestamp` from the
  time `run.sh` noted in step 2, taken from the machine running the script. If
  that is not the machine running the node, and the node's clock is behind,
  entries from this run fall before the mark. Run the script on the node's host.
- **Entries have not arrived.** Delivery is asynchronous; step 15 waits up to
  ten seconds for the last one. A node under heavy load may take longer.

## A term query on `audit_category` matches nothing

The audit index is dynamically mapped; string fields are `text` with a
`.keyword` subfield. Use `audit_category.keyword`.

## The audit index grows on every run

It is meant to. The example never deletes the audit log, and filters its
queries to its own run instead. To start again, throw away the data directory
(`make serve` does that each time it starts), or delete the index yourself:

```bash
curl -su admin:admin -XDELETE 'localhost:9281/security-auditlog-*'
```

## Cleaning up

```bash
make clean
```

deletes both indices and every user, role, mapping, action group and tenant the
example created. It leaves the audit configuration as step 2 set it and the
audit index as it is. To put the audit configuration back to the shipped
default, `PUT _plugins/_security/api/audit/config` with
`disabled_rest_categories` and `disabled_transport_categories` both set to
`["AUTHENTICATED", "GRANTED_PRIVILEGES"]`, or throw away the data directory.
