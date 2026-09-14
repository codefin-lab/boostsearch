# Design notes

## The decision this example is really about

A person reviewing a contract does not want a list of documents. They want the
clause, and they want to see at a glance why it came back. That is two jobs:
**match on where words stand**, not only on whether they are there, and
**mark the words in the text** so the reader does not have to find them again.

Both need the same thing from the index: positions (which word is the fifth)
and, for marking, offsets (which characters it covers). The mapping is built
around keeping them.

## Why `body` stores term vectors, and `title` does not

```json
"body": { "type": "text", "term_vector": "with_positions_offsets",
          "fields": { "english": { "type": "text", "analyzer": "english",
                                   "term_vector": "with_positions_offsets" } } }
```

Positions are kept for every `text` field anyway; that is what phrase, span and
interval queries walk. `term_vector` is extra: a small inverted index per
document, stored beside it, holding each term with its positions and character
offsets.

| Highlighter | Where the offsets come from | Needs |
|---|---|---|
| `unified` | the postings, or analysing the text again | nothing |
| `plain` | analysing the text again, every time | nothing |
| `fvh` | the stored term vector | `term_vector: with_positions_offsets` |

`fvh` is refused on a field without term vectors, which is why `body` has
them. The cost is disk: roughly the size of the field's postings again. Titles
are short and never highlighted with `fvh` here, so `title` goes without.

`_termvectors` (step 17) reads the same structure back: for each term in c3,
its position and its start and end offset. It is the most direct way to see
what a positional query is counting.

## Why `body.english` is a sub-field, not a replacement

A reviewer types *terminates*; the text says *terminate* and *termination*.
The `english` analyser stems all three to `termin`, so a search on
`body.english` finds them. But a highlight on `body.english` would have to
mark stems, and a phrase on it would let *terminated notices* match *terminate
notice*, which is looser than a lawyer wants.

Keeping both lets each query pick: positional queries run on `body`, where a
word is the word; a forgiving search runs on both. `matched_fields` (step 6)
joins them again for the reader: the match came through `body.english`, and the
marks are put on the unstemmed text of `body`. Both sub-fields must be
analysed with the same positions and offsets for that to work, which a
multi-field over the same text guarantees.

## Highlighting against a query of its own

Step 5 finds clauses by `clause_type`, a keyword, and highlights them against
`damages negligence fraud`. The query that decides *what* comes back and the
query that decides *what is marked* are different questions: a review screen
lists every liability clause and marks the words the reviewer cares about,
whether or not they were part of the search. `highlight_query` is how that is
said without changing the hits.

`number_of_fragments: 0` asks for the whole field rather than excerpts. Clauses
are a few sentences long, and a reviewer reading one wants all of it; on a
field holding a whole contract, set `fragment_size` and `number_of_fragments`
instead so the answer is the passages around the hits, not the document.

## Phrases, spans and intervals: three ways to say "near"

| | `match_phrase` + `slop` | `span_near` | `intervals` |
|---|---|---|---|
| Order | may be swapped, at a cost in slop | `in_order` says | `ordered` says |
| Distance | edit distance, in moves | words between | `max_gaps`, words between |
| Composes | no | spans nest in spans | intervals nest in intervals |
| Prefix or wildcard inside | no | `span_multi` | `prefix`, `wildcard` rules |
| "but not ..." | no | `span_not` | `filter` |

`slop` in a phrase is a count of moves, not of words between: *notify in
writing* against *notify the supplier in writing* is two moves, because
`the supplier` has to be stepped over. `span_near` and `intervals` count the
words between directly, which is easier to reason about: step 8's five words
between *terminate* and *notice* in c4 are `this agreement immediately upon
written`.

`span_first` (step 9) is the question a phrase cannot ask: not "does the clause
mention the supplier" but "does it *open* with the supplier". A clause that
starts with a party's name is usually that party's obligation, and in a review
that distinction is the point.

`intervals` is the newer of the two and the one to prefer for new work. Step 13
shows why: `filter: not_containing` removes an interval that has *not* inside
it, so *liability under this warranty is not limited* drops out while *the
liability of the supplier ... shall be limited* stays. Expressing that with
spans takes a `span_not` around a `span_near`, and a `bool` with `must_not`
cannot say it at all, because *not* appears elsewhere in clauses that should
match.

## Why `more_like_this` finds boilerplate first

Step 14 asks for clauses like c1, the limitation of liability. The nearest by
shared terms is the termination clause, and the other liability cap is second.
That is not a fault: *either party*, *this agreement*, *the other*, *fees paid*
are shared, and on twelve clauses nothing is rare enough to be discounted.

On a real corpus of thousands of contracts those words appear everywhere, their
document frequency is high, and `max_doc_freq`, `stop_words` and the default
`minimum_should_match` of 30% are what push them out. Here the query sets
`minimum_should_match: 1` so every clause sharing a term is a candidate, and
the order is what to read.

## `combined_fields` over `title` and `body`

A clause titled *Termination for convenience* whose body says *notice* should
beat one that says *termination* only in passing. `multi_match` with
`best_fields` scores each field alone and keeps the best, so a word in the
title and a word in the body never add up. `combined_fields` (step 15) scores
the fields as if they were one, BM25F, with `title^2` counting each title word
twice. Both fields must share an analyser for that to be meaningful, which is
why `title` is not given the english one.

## What would change at scale

- **Term vectors double the text's index cost.** On a clause corpus it is
  cheap. On millions of full contracts, keep them only on the field a reader
  actually sees highlighted, and let `unified` handle the rest from postings.
- **Store clauses, not contracts.** One document per clause is what makes
  `span_first`, `more_like_this` and a whole-field highlight meaningful. A
  contract as one document makes "the clause" a highlighting problem instead of
  a search problem.
- **Positional queries cost more than term queries.** Every candidate's
  positions are read. Put a cheap `filter` (contract, clause type) beside them
  in a `bool` so they run over fewer documents.
