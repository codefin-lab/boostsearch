# Version one is everything

The alternative was a staged release: a single-node engine first, a cluster
after it, the plugin surfaces after that. We are not doing it that way. Version
one of VeloSearch answers the whole OpenSearch interface -- the search API, the
analysers, Painless, ingest, security including document- and field-level, a
real cluster, index management, vector search and SQL/PPL -- and is quicker or
lighter on every dimension of the bench matrix, measured beside OpenSearch on
the same machine.

The reason is what the project is for. A team can move a workload onto
VeloSearch, or it cannot; an implementation of the interface that is missing a
surface the workload uses cannot take it, and finding that out after the move
is worse than never starting. Shipping "compatible except for..." teaches people to distrust the
word, and the word is the whole product.

## Consequences

A first release takes longer, and the cluster work is the critical path that
no number of people shortens.

One claim has to be qualified, and the documentation qualifies it:
`ingest-attachment` extracts text from the formats people actually send --
HTML, RTF, PDF, Word, Excel, PowerPoint, OpenDocument, EPUB and plain text --
and not from the fourteen hundred that Tika, a stack of Java libraries,
reaches. Everywhere else
100% means 100%.
