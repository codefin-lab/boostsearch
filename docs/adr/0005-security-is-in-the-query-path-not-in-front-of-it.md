# Security is in the query path, not in front of it

BoostSearch carries the whole of what OpenSearch's security plugin does:
TLS, users and roles, API keys, SAML, OIDC, LDAP, an audit log, and -- the part
that decides the architecture -- document-level and field-level security.

Authentication could sit in front as middleware, and does. Authorization cannot.
Document-level security is a filter that has to be inside every query, before
the query is scored, or a user learns what a document contains from how many
documents matched. Field-level security is not `_source` filtering either: a
hidden field must be invisible to aggregations, to sorts, to `fields`, to
highlighting and to `field_caps`, and each of those reads the index directly.
So the identity of the caller is a parameter of a search, threaded from the
handler to the query builder, and every path that can observe a field asks
whether this caller may see it.

## Consequences

This is the second decision in this project that cannot be retrofitted -- the
first is the analyser one -- and it is why security is in version 1 rather than
after it. Building the search path without a caller in it and adding one later
means visiting every aggregation, every sort and every fetch phase a second
time, with the certainty that one of them will be missed and will leak.

The security plugin is a separate repository with its own tests, so the
conformance corpus grows: 2,296 sections from OpenSearch's own tests plus
whatever of the security plugin's suite can be pointed at an HTTP server.
