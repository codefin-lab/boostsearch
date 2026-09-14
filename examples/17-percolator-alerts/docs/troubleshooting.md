# Troubleshooting

## Storing a rule answers 400 `mapper_parsing_exception`

```
failed to parse
caused by: query_shard_exception: No field mapping can be found for the field with name [hostname]
```

The rule's query names a field the rules index does not map. That is step 3,
and it is working as intended. Either the name is a typo (`hostname` for
`host`), or the field is new: add it to the rules index mapping first, with
the same type it has in the events index, then store the rule:

```bash
curl -s -X PUT localhost:9277/alerts/_mapping -H 'content-type: application/json' \
  -d '{"properties":{"hostname":{"type":"keyword"}}}'
```

## A rule that should match does not

Search the events index with the rule's query. If the search finds the event
and the percolation does not, the two mappings disagree about the field:

```bash
curl -s localhost:9277/alerts/_mapping/field/host
curl -s localhost:9277/events/_mapping/field/host
```

The usual cases: `keyword` in one and `text` in the other (a `term` on
`"db-1"` matches a keyword and not the analysed tokens `db` and `1`); a number
mapped as `keyword` (a `range` compares strings, so `"900" > "5000"`); a
different analyzer on `message`.

## `percolate` returns nothing

- `field` must name the `percolator` field -- `query` here.
- `percolate` must be sent to the rules index (`/alerts/_search`), not to the
  events index.
- The document sent must use the field names of the mapping. An event with
  `msg` instead of `message` is indexed for the request without complaint, and
  no rule on `message` can match it.

## Percolating by `index` and `id` answers 404

```
resource_not_found_exception: indexed document [events/e99] couldn't be found
```

The document is not there: a wrong id, a wrong index, or an event that was
never written because its bulk item failed. A missing index is
`index_not_found_exception` instead. Either way it is an error to
handle, not an answer meaning "no rule matched" -- see `design.md`.

## The slots do not line up with the events

`_percolator_document_slot` counts positions in the `documents` array from 0,
in the order they were sent. A router that sorts, deduplicates or drops events
between building the request and reading the answer must keep the array it
sent to translate slots back. A single `document` is always `[0]`.

## A matching rule has no `highlight`

Expected when its query matched on something that has no words to mark: a
`term` on a keyword, a `range` on a number, a `prefix` on a host. In step 6,
`r08` (a range) and `r01` and `r05` (terms) have none. Only fields named in the
`highlight` section are marked, and only by the parts of the rule's query that
touch them.

With several documents, look for `<slot>_<field>` -- `1_message`, not
`message`. Code that reads `highlight.message` finds it only when one
`document` was sent.

## The scores look arbitrary

They are the score of each rule's own query against the percolated documents
only, which is a tiny index, so the numbers change with what was sent and are
not comparable between requests. A rule made only of `filter` clauses scores
0.0 however well it matches; a `range` scores a constant 1.0. If the order
does not matter, put `percolate` in a `filter` (steps 9 and 10) and every hit
scores 0.0.

## A rule update does not seem to take effect

The update was not refreshed before the next percolation. `run.sh` passes
`?refresh=true` on the `_update` in step 11; without it the old version of the
rule is what the next search sees until the next refresh.

## Step 11 changed the results of the other steps

It does, on purpose, and only after they have run. `r08` is left at 3000 ms;
rerunning deletes `alerts` and stores the rules again at 5000 ms.

## Cleaning up

```bash
make clean          # deletes alerts and events
```
