# An analyzer belongs to a path, so BoostCore learns to pick one per path

Every document here lives in one JSON field, and BoostCore takes one analyzer
per schema field -- so a mapping that gives `title` the `english` analyzer and
`name_th` a Thai one has nowhere to say so. We are changing BoostCore instead
of working around it: `index_json_value` takes a resolver from path to
analyzer rather than a single analyzer, which is what Lucene's
`PerFieldAnalyzerWrapper` does and what the fork exists for.

## Considered options

Analysing the text ourselves and writing the tokens back as text was the
cheapest and needs no change to the engine, but the field's own tokenizer then
cuts those tokens again -- a synonym with a space in it, a `keyword` analyzer
on a text field, `u.s.a` -- and the positions are no longer the positions the
text had, which phrase queries and highlighting both read.

One JSON field per analyzer keeps the analysis exact without touching the
engine, but a tantivy schema is fixed when the index is created: every
analyzer an index will ever use would have to be known on the day it is made,
and `PUT _mapping` naming a new one later would mean reindexing. Each analyzer
would also carry its own columns.

## Consequences

This is in the write path, so it is the decision that would be most expensive
to revisit: changing it later means reindexing. It also means the analyzer
registry has to be reachable from the segment writer, which is a small amount
of plumbing through BoostCore that upstream does not have and that a future
rebase will have to re-apply.
