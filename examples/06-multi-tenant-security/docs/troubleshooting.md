# Troubleshooting

## Step 1 returns 404, and everything after it fails

Security is off. It is off by default; this example needs a node started with
it on:

```bash
make serve
```

which sets `BOOSTSEARCH_DISABLED=false` and
`BOOSTSEARCH_RESTAPI_ROLES_ENABLED=all_access`. If you are using your own node,
start it with both.

## Every request returns 401

The administrator credentials are wrong. They default to `admin:admin`; set
`AUTH` in `.env` or the environment:

```bash
AUTH=admin:mypassword ./run.sh
```

## `PUT /_plugins/_security/api/roles/...` returns 403

The caller's role is not allowed to use the security REST API.
`BOOSTSEARCH_RESTAPI_ROLES_ENABLED` names which role may, and it must include
the role the administrator has -- `all_access` in this example.

## Creating a user fails with `Password is similar to user name`

The rule refuses a password containing the user name, for names of four
characters or more. `nw-reader` with a password containing "reader" is refused.
The example's passwords avoid this; if you change one, avoid it too.

## Creating a user fails with `Weak password`

Nothing under nine characters is accepted, and beyond that the strength is
judged rather than counted. Use a passphrase.

## Step 5 shows both tenants' tickets

The DLS filter is not being applied. Check that the role has it and that the
value is a **string** containing JSON, not a JSON object:

```bash
curl -su admin:admin localhost:9266/_plugins/_security/api/roles/tenant_northwind | jq
```

`"dls"` must read `"{\"term\": {\"tenant\": \"northwind\"}}"`. An object there
is accepted by some versions and silently ignored.

Then check the user actually has the role:

```bash
curl -su nw-reader:correct-horse-battery-1 localhost:9266/_plugins/_security/authinfo | jq '.roles'
```

## `internal_note` is still in the results

`fls` is `["~internal_note"]` -- with the tilde, which means *exclude*. Without
it, the list is an allow-list and `internal_note` becomes the only field kept,
which is the opposite of what was wanted and looks like the filter is ignored.

## The masked field is empty rather than hashed

`masked_fields` hashes; `fls` removes. If a field appears in both, the removal
wins. Check the role does not list `contact_email` in its `fls`.

## Step 14 returns 200 with an empty result rather than 401

That would be a real problem, and it is what this step checks. A wrong password
must be an authentication failure, not an authorisation-shaped empty answer,
because a client cannot tell the difference between "your password is wrong"
and "you have no tickets" and will not retry.

## Cleaning up

Roles, users and mappings live in the node's configuration, so `make clean`
only removes the index. To remove the rest, throw away the data directory the
node was started with, or delete them individually:

```bash
make clean
for r in tenant_northwind tenant_contoso support; do
  curl -su admin:admin -XDELETE localhost:9266/_plugins/_security/api/roles/$r
done
for u in nw-reader co-reader agent-smith; do
  curl -su admin:admin -XDELETE localhost:9266/_plugins/_security/api/internalusers/$u
done
```
