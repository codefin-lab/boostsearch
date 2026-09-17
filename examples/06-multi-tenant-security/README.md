# 6. One index, several customers, no proxy in front

The usual way to keep one customer's data from another is an index each, or a
service in front that rewrites every query. Both are expensive. This example
does it the other way: the tenant filter is part of the query the engine runs,
so there is no path -- `_search`, `_count`, an aggregation, `_mget`,
`_termvectors` -- that can step around it, because there is nothing to step
around.

Two customers share one index. Each sees their own tickets, never the internal
note, and the contact address only as a hash. Support sees all of it. The
difference between those three views is a role, not an index.

## What it shows

| Step | Feature |
|---|---|
| 1 | `_plugins/_security/health` |
| 3 | a role with `dls`, `fls` (`~field` to exclude) and `masked_fields` |
| 4 | internal users with attributes and backend roles; role mappings by user and by backend role |
| 5-6 | the same request, two callers, two answers |
| 7 | the filter holds for `_count`, `terms` and `cardinality` |
| 8 | `_doc` and `_mget` on a document the caller may not see |
| 9 | a write refused by a read-only role |
| 10 | `permissionsinfo`, `authinfo` |
| 11 | action groups |
| 12 | a second role with full access to the same index |
| 13 | reading the configuration back |
| 14 | a wrong password, an unknown user |
| 15 | what the security API refuses, and the shape of its refusal |

## Running it

```bash
make serve      # a node configured for this example, port 9266, foreground
make run        # the example, in another terminal
```

`make check` validates the scripts and every request body without a server;
`make clean` deletes what the example left behind. Copy `.env.example` to
`.env` to change the address or anything else.

The longer form, and what this example needs of the node:

Security is off by default, so this example needs a node started with it on:

```bash
VELOSEARCH_DISABLED=false \
VELOSEARCH_RESTAPI_ROLES_ENABLED=all_access \
VELOSEARCH_DATA=/tmp/velo-security \
./target/release/velosearch &

examples/06-multi-tenant-security/run.sh
```

`AUTH` sets the administrator credentials (default `admin:admin`).

## What to look for

- **Steps 5 and 6** are the same request sent twice. Put them side by side:
  two tickets each, disjoint, `internal_note` absent from both, and
  `contact_email` a hash rather than an address.
- **Step 7** is the point of doing it this way. A proxy that rewrote `_search`
  would still have to rewrite `_count`, every aggregation, `_mget` and
  `_termvectors`, and would leak through whichever one it forgot. Here the
  aggregation reports one tenant and a cardinality of one, because the filter
  is in the query the collector ran.
- **Step 14** returns 401, not a 200 with no results -- the distinction matters
  for a client that retries.
- **Step 15** shows the refusal envelope: `Wrong datatype` naming the field and
  what was expected, and `Weak password`. A role that silently accepted the
  string in the first case would grant nothing while looking as though it
  granted something.

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
| `requests/` | 5 request bodies, one file each |
| `data/` | 1 bulk document set |

## Leaves behind

The index `tickets`, roles `tenant_northwind`, `tenant_contoso`, `support`,
the action group `ticket_reader`, and the users `nw-reader`, `co-reader`,
`agent-smith`. Rerunning replaces them. Throwing away the data directory
removes the lot.
