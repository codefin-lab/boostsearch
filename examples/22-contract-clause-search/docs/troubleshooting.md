# Troubleshooting

## A hit comes back with no `highlight` at all

Not an error: the server found nothing to mark in that field. The usual causes,
in order of likelihood:

- **The query did not search the highlighted field.** `require_field_match`
  is `true` by default, so a query on `body.english` marks nothing in `body`.
  Search both fields (step 6 uses `multi_match` over `body` and
  `body.english`), or name the other field in `matched_fields`.
- **The query has no words in it.** A `term` on a keyword, a `range`, an
  `ids` query: there is nothing to mark. Give the field a `highlight_query`
  (step 5).
- **The query is positional.** Span and interval queries find the clause, but
  on this node they do not yet feed the highlighter; step 8 onwards print ids
  for that reason. Highlight such a hit with a `highlight_query` made of the
  same words as a `match`.

## `fvh` is refused, or behaves like `unified`

OpenSearch refuses the `fvh` highlighter on a field that was not mapped with
`term_vector: with_positions_offsets`:

```
the field [title] should be indexed with term vector with position offsets
to be used with fast vector highlighter
```

The term vector is written at index time, so adding it means reindexing. Check
the mapping:

```bash
curl -s localhost:9282/clauses/_mapping | jq '.clauses.mappings.properties.body'
```

This node accepts `fvh` on any field and marks it the way `unified` would, so
a request that works here may be refused by OpenSearch.

## `fragment_size` and `number_of_fragments` seem to do nothing

Every highlight in this example sets `number_of_fragments: 0`, which asks for
the whole field; `fragment_size` is ignored in that case by design. With a
number above zero, OpenSearch returns up to that many passages of about
`fragment_size` characters around the matches.

On this node the whole value comes back whatever the two are set to, and
`no_match_size` returns nothing for a field without a match. Neither is used
in `run.sh` for that reason.

## A phrase highlight marks words outside the phrase

On this node a `match_phrase` for *other party* marks *party* in *Either party*
as well as in *the other party*. OpenSearch marks only the words where the
phrase matched. The hits are right; only the marking is broader.

## A phrase with `slop` matches nothing

`slop` counts moves, not words between. *notify in writing* against *notify
the supplier in writing* needs 2: one move over *the*, one over *supplier*.
If the words are in the other order in the text, a swap costs 2 more. Use
`span_near` or `intervals` when "N words between" is what you mean -- they count
exactly that.

## `span_near` returns a parse error

The clauses of a `span_near` must all name one field -- `Cannot add clause
... to SpanNearQuery for field ...` -- and each clause must itself be a span
query: a `match` inside `clauses` is refused. A boost belongs on the outer
query, not on a clause. `span_gap` is only allowed with `in_order: true`.

## Scores of span and interval queries

A span scores by BM25 over its sloppy frequency: each match adds
`1 / (1 + width)`, so *terminate ... notice* five words apart counts a sixth
of an adjacent pair. An interval hit scores `freq / (freq + 1)`, where each
interval adds one over how much longer it is than the rule needs. Both rank
tight matches first; `run.sh` still prints their ids sorted, which keeps its
output the same whichever order ties fall in.

## `more_like_this` returns almost everything

With `minimum_should_match: 1` any shared term is enough, and on twelve
clauses *the*, *party* and *agreement* are shared by nearly all of them.
Read the order, not the count. On a large corpus set `max_doc_freq` or
`stop_words` to drop the common words, or leave `minimum_should_match` at its
default of 30% of the chosen terms, which on these clauses finds nothing like
*c1* at all.

## `_termvectors` returns `found: false` or no `term_vectors`

`found: false` is the wrong id. An empty `term_vectors` is the wrong field
name, or a field that is not text. Only `body` and `body.english` store term
vectors here; other text fields are analysed on the fly when asked for.

## Cleaning up

```bash
make clean          # deletes clauses
```
