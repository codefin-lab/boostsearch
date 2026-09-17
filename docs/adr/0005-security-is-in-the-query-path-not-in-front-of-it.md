# Security is in the query path, not in front of it

VeloSearch carries the whole of what OpenSearch's security plugin does:
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

A filter that cannot be built is not a filter that is skipped. A document-level
rule is stored as the text of a query, and the text can be one this engine
cannot read -- a role written against a version that had a query this one does
not, or a substitution that produced something malformed. Dropping the filter
and answering the search without it is the one failure a document-level rule
cannot have: the caller would be shown every document the rule exists to hide.
So such a rule becomes a filter that matches nothing, and the log says so.

## Consequences

This is the second decision in this project that cannot be retrofitted -- the
first is the analyser one -- and it is why security is in version 1 rather than
after it. Building the search path without a caller in it and adding one later
means visiting every aggregation, every sort and every fetch phase a second
time, with the certainty that one of them will be missed and will leak.

The security plugin is a separate repository with its own tests, so the
conformance corpus grows: OpenSearch's own tests plus whatever of the security
plugin's suite can be pointed at an HTTP server.
