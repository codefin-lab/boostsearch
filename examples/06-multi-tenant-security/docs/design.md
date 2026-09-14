# Design notes

## Three ways to keep tenants apart, and why this one

| | Index per tenant | A proxy that rewrites queries | A filter in the role |
|---|---|---|---|
| Cost per tenant | a shard, minimum | none | none |
| Leak surface | none, if routing is right | every endpoint the proxy forgot | none |
| Cross-tenant report | fan-out over N indices | possible | one query |
| Breaks when | tenants number in thousands | a new endpoint ships | -- |

An index per tenant is the safest and does not scale: shards have a fixed cost
in memory and cluster state, and a thousand small tenants is a thousand mostly
empty shards.

A proxy is the usual compromise and is the one that leaks. It has to rewrite
`_search`, and also `_count`, and every aggregation, and `_mget`, and
`_termvectors`, and `_explain`, and whatever endpoint was added last release.
Every one it misses is a silent cross-tenant read.

Document-level security puts the filter inside the query the engine runs. There
is no path around it because there is nothing to go around -- the collector
itself never sees the other tenant's documents. Step 7 is the demonstration:
`_count` and a `cardinality` aggregation both report one tenant, because
counting is done by the same collector.

## `dls`, `fls` and `masked_fields` are three different controls

```json
"dls": "{\"term\": {\"tenant\": \"northwind\"}}"   which documents
"fls": ["~internal_note"]                          which fields, removed
"masked_fields": ["contact_email"]                 which fields, hashed
```

- **DLS** decides visibility of whole documents. The value is a query, as a
  JSON *string* -- a nesting that catches everyone once.
- **FLS** removes a field from the answer. `~field` excludes; a bare list of
  field names is an allow-list instead. Mixing the two forms in one list is an
  error.
- **Masking** keeps the field present but replaces the value with a hash. The
  difference from FLS matters: a masked field can still be grouped on, so
  "how many tickets per contact" works without revealing any address.

## Why two role mappings, done two ways

```json
rolesmapping/tenant_northwind  { "users": ["nw-reader"] }
rolesmapping/tenant_contoso    { "backend_roles": ["contoso"] }
```

Mapping by user name is fine for a handful of accounts and becomes a
maintenance job. Mapping by backend role means the identity provider -- LDAP,
SAML, OIDC -- decides who is a Contoso user, and the cluster never holds that
list. The second is what a real deployment does; the first is shown because it
is what everyone writes first.

## Why the roles grant `read` and not `crud`

The tenant roles are read-only on purpose, and step 9 shows a write being
refused. A tenant that may write needs more thought than this example gives:
DLS filters what is *read*, and does not stop a writer from writing a document
with somebody else's `tenant` value. Enforcing that needs either an ingest
pipeline that overwrites the field from the caller's identity, or a separate
write path.

This is the one real gap in the "filter in the role" model and it is worth
stating rather than glossing.

## The password rules

Step 15 shows `Weak password` on a short password, and the rule that refuses a
password containing the user's name. The name rule applies at four characters
and longer, which is why a user called `dee` can have `dee-password-1` and a
user called `tenant` cannot have anything containing "tenant". This has caught
out at least one test script in this repository's own history.

## What would change at scale

- **A DLS filter is part of every query**, so its cost is paid on every
  request. A `term` on a keyword field is free; a DLS filter with a script or a
  `terms` lookup is not.
- **Roles and mappings live in cluster state.** A few hundred are fine;
  a role per tenant with tens of thousands of tenants is not, and the answer is
  one parameterised role using a user attribute in the DLS query.
- **The audit log** is not exercised here and is the other half of a real
  deployment: DLS says what a caller may see, the audit log says what they did
  see.
