# Design notes

## Why the rules are documents

The obvious way to build alerting is a list of queries kept by the
application, run against the index on a timer. It has three faults:

1. **It is late.** An event is found at the next tick, not when it arrives.
2. **It repeats itself.** Every tick finds the same events again, so the
   application has to remember what it already reported.
3. **It grows with the rules.** A thousand rules is a thousand searches a
   tick, most of them finding nothing.

A `percolator` field inverts it. The rules are stored in an index, and an
event is judged at the moment it is written: one search, with the event as
the query's input, and the rules that match as the hits. Nothing is polled,
nothing is found twice, and the rules are an index like any other -- they can
be searched, filtered, aggregated, backed up and updated one at a time.

## Why the rules index maps the event fields

```json
"query":      { "type": "percolator" },
"service":    { "type": "keyword" },
"latency_ms": { "type": "integer" },
"message":    { "type": "text" }
```

A stored query is parsed against the mapping of the index it is stored in, not
against the index the events live in. `term` on `service` has to know that
`service` is a keyword and `range` on `latency_ms` that it is a number; a
`match` on `message` has to know which analyzer to run. The event document
sent to `percolate` is indexed, for the length of the request, under that same
mapping.

This is why step 3 exists. A rule naming a field the mapping does not have is
refused when it is stored. The alternative -- accepting it -- produces a rule
that can never match, and an alerting rule that silently never fires is the
worst failure an alerting system has.

It also means the two mappings must agree. If `events` maps `host` as `text`
and `alerts` maps it as `keyword`, `prefix` on `db-` behaves differently in
the two places, and a rule tested against a search of `events` will not match
the same way when it percolates. Keep the event fields in one place and put
them into both indices; step 1 and step 7 use identical property blocks for
that reason.

## The metadata beside the query

Each rule carries `owner`, `severity` and `channel` as ordinary keyword
fields. They are not part of the query and play no part in whether it
matches; they are what the matching rules are filtered and counted by.

| Question | Where it is answered |
|---|---|
| which rules does this event trip? | the `percolate` clause |
| only the critical ones | a `term` beside it, step 9 |
| what does this mean for one person | a `term` on `owner`, step 9 |
| who is told, and on what channel | `terms` aggregations, step 10 |

Putting `percolate` in `filter` rather than `must` is deliberate: the router
does not want the rules ranked, it wants all of them, and a filter clause is
cached and not scored.

## One document, several, or one already stored

| Form | Use it when |
|---|---|
| `document` | an event is being judged as it is written, before or instead of indexing it |
| `documents` | a batch arrived together and one round trip is cheaper than many |
| `index` + `id` | the event is already stored, and re-sending it would be waste |

The `documents` form answers "which rules does any of these trip?", one hit
per rule, and `_percolator_document_slot` on each hit says which of the
documents did -- their positions in the array, counted from 0. That is what
lets a router put the right event into the notification without percolating
the batch one event at a time (step 5). A single `document` is slot `[0]`.

The `index` + `id` form reads the stored `_source`, so a field excluded from
`_source` is not there to be matched. A document that is not there is a 404,
not an empty answer, and that matters: an empty hit list means "no rule
matched", and a router that could not tell a failed lookup from that would
drop the event without a word (step 8).

## Scored, or filtered

In a scoring context -- `percolate` as the query on its own, or in `must` --
each matching rule scores what its own query scores against the event; with
several documents, the best of them. Step 6 shows every kind at once:

| Rule's query | Score | Why |
|---|---|---|
| `match_phrase` on `message` (`r10`) | 1.1343 | BM25 over three words |
| `range` on `latency_ms` (`r08`) | 1.0 | a constant-score query |
| `match` on `message` (`r06`, `r09`) | 0.4575 | BM25 over one word |
| only `filter` clauses (`r01`, `r05`) | 0.0 | a filter does not score |

The numbers are BM25 over the handful of documents being percolated, so they
are comparable within one request and mean nothing across two -- step 4's
`r06` scores 0.1308 on one event against step 6's 0.4575 on three. Use the
order when a person reads the list, so the rule that matched the most specific
words is first; put `percolate` in a `filter` when a program routes the
alerts (steps 9 and 10), where every match matters and none outranks another.

## Highlighting the event

A `highlight` on a percolate search marks the *event*, not the stored rule:
each rule's query is used to highlight the document it matched. With one
`document` the key is the field name; with `documents` it is
`<slot>_<field>`, so `r06` in step 6 carries both `1_message` and `2_message`.
A rule that matched on keywords or a number -- `r08`'s range, `r01`'s terms --
marks nothing, and its hit has no `highlight` at all. This is how a
notification shows the words that tripped the rule rather than the whole
event.

## Why a rule change needs nothing else

Step 11 updates `r08` and the next percolation uses the new threshold. There
is no compiled rule set to rebuild and nothing to reindex: the events being
judged are not stored against the rules, and the rules are read at the moment
of asking. That is the operational difference from a system that precomputes
matches at write time.

The corollary: changing a rule does not re-judge events that have already been
percolated. If an event from an hour ago should be judged by today's rules,
percolate it again by `index` and `id` (step 8).

## What would change at scale

- **Percolation cost grows with the rules, not the events.** The reference
  narrows the candidates by extracting terms from each stored query at write
  time, so a document containing none of a rule's required terms never runs
  it. A rule with no extractable terms -- a bare `range`, a `wildcard` -- is
  run for every document. Twelve rules make this invisible; twelve thousand do
  not. Prefer rules that include at least one `term` or `match` clause.
- **Percolate on write, not after.** In production the event is percolated as
  part of the write path (or by a consumer reading the same stream), and the
  hits are handed to the notifier. Indexing the event and percolating it are
  two independent calls, and either can be skipped.
- **Shard the rules by who owns them** only when there are very many; a
  `percolate` query fans out to every shard of the rules index like any other
  search.
- **Rules and mappings evolve together.** Adding a field to the events means
  adding it to the rules index before any rule may use it (step 3 is the guard
  that makes forgetting visible). Changing a field's type means reindexing the
  rules, since each stored query was parsed against the old type.
