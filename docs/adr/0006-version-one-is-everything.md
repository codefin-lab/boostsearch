# Version one is everything

The alternative was a staged release: a single-node engine first, a cluster
after it, the plugin surfaces after that. We are not doing it that way. Version
one of BoostSearch answers everything OpenSearch answers -- the search API, the
analysers, Painless, ingest, security including document- and field-level, a
real cluster, index management, vector search and SQL/PPL -- and beats it on
every dimension of the bench matrix.

The reason is what the product is for. A team replaces OpenSearch with
BoostSearch, or it does not; a replacement missing a surface they use is not a
replacement, and finding that out after the migration is worse than never
starting. Shipping "compatible except for..." teaches people to distrust the
word, and the word is the whole product.

## Consequences

Roughly 346 working days on one stream -- sixteen months -- against about
thirty weeks on three, where the cluster work is the critical path that no
amount of people shortens. The plan in `docs/plan-v1.md` says which phases can
run beside each other.

One claim has to be qualified, and it is qualified in the plan and will be in
the documentation: `ingest-attachment` extracts text from the formats people
actually send -- pdf, html, docx, xlsx, pptx, txt, rtf, doc -- and not from the
fourteen hundred that Tika, a stack of Java libraries, reaches. Everywhere else
100% means 100%.
