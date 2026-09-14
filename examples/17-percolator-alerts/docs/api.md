# API surface -- 17. Saved searches that find the documents, not the other way round

Every request this example makes, in the order it makes them. Generated
from `run.sh`; if the two disagree, `run.sh` is right.

| Step | Method | Path | Body |
|---|---|---|---|
| an index of alert rules: the queries are the documents | `DELETE` | `/$IDX` | none |
|  | `PUT` | `/$IDX` | `requests/01-an-index-of-alert-rules-the.json` |
| twelve alert rules, each a stored query with an owner, a severity and a channel | `POST` | `/$IDX/_bulk?refresh=wait_for` | `data/01-twelve-alert-rules-each-a-stored.ndjson` |
| a rule that names a field nobody mapped is refused when it is stored | `PUT` | `/$IDX/_doc/r99?refresh=true` | `requests/02-a-rule-that-names-a-field.json` |
| one event, not yet indexed anywhere: which rules does it trip, best first? | `GET` | `/$IDX/_search` | `requests/03-one-event-which-rules-does-it.json` |
| three events at once: which rules, and which of the events tripped each | `GET` | `/$IDX/_search` | `requests/04-three-events-at-once-which-rules.json` |
| which words tripped which rule: highlighting the events, not the rules | `GET` | `/$IDX/_search` | `requests/05-which-words-tripped-which-rule-and.json` |
| an event index of its own, and ten events in it | `DELETE` | `/$EVENTS` | none |
|  | `PUT` | `/$EVENTS` | `requests/06-an-event-index-of-its-own.json` |
|  | `POST` | `/$EVENTS/_bulk?refresh=wait_for` | `data/02-an-hour-of-events-already-indexed.ndjson` |
| an event already stored, percolated by its index and id | `GET` | `/$IDX/_search` | `requests/07-an-event-already-stored-percolated-by.json` |
|  | `GET` | `/$IDX/_search` | `requests/08-an-event-that-is-not-there.json` (answers 404) |
| only the critical rules: the stored queries filtered by what is written beside them | `GET` | `/$IDX/_search` | `requests/09-only-the-critical-rules-the-stored.json` |
|  | `GET` | `/$IDX/_search` | inline: step 4's `percolate` plus `term` on `owner` |
| a burst of five events: who is told, and how | `GET` | `/$IDX/_search` | `requests/10-a-burst-of-five-events-who.json` |
| a rule is a document: change it, and the next event is judged by the new version | `POST` | `/$IDX/_update/r08?refresh=true` | inline |
|  | `GET` | `/$IDX/_search` | `requests/03-one-event-which-rules-does-it.json` |

`$IDX` is `alerts` and `$EVENTS` is `events`.

The checks between the steps make requests of their own: every `expect_hits`
and `expect_that` repeats the search it follows (the same body, read from the
same file) to read the answer, and every `expect_docs` asks
`GET /<index>/_count`. `green` waits on
`GET /_cluster/health/<index>?wait_for_status=yellow`, and `lib.sh` asks
`GET /` once to find out the server is there.

## Endpoints touched

Path parameters and query strings removed, deduplicated:

- `/`
- `/<var>`
- `/<var>/_bulk`
- `/<var>/_count`
- `/<var>/_doc/<var>`
- `/<var>/_search`
- `/<var>/_update/<var>`
- `/_cluster/health/<var>`

## Request bodies

- [`requests/01-an-index-of-alert-rules-the.json`](../requests/01-an-index-of-alert-rules-the.json)
- [`requests/02-a-rule-that-names-a-field.json`](../requests/02-a-rule-that-names-a-field.json)
- [`requests/03-one-event-which-rules-does-it.json`](../requests/03-one-event-which-rules-does-it.json)
- [`requests/04-three-events-at-once-which-rules.json`](../requests/04-three-events-at-once-which-rules.json)
- [`requests/05-which-words-tripped-which-rule-and.json`](../requests/05-which-words-tripped-which-rule-and.json)
- [`requests/06-an-event-index-of-its-own.json`](../requests/06-an-event-index-of-its-own.json)
- [`requests/07-an-event-already-stored-percolated-by.json`](../requests/07-an-event-already-stored-percolated-by.json)
- [`requests/08-an-event-that-is-not-there.json`](../requests/08-an-event-that-is-not-there.json)
- [`requests/09-only-the-critical-rules-the-stored.json`](../requests/09-only-the-critical-rules-the-stored.json)
- [`requests/10-a-burst-of-five-events-who.json`](../requests/10-a-burst-of-five-events-who.json)
- [`data/01-twelve-alert-rules-each-a-stored.ndjson`](../data/01-twelve-alert-rules-each-a-stored.ndjson)
- [`data/02-an-hour-of-events-already-indexed.ndjson`](../data/02-an-hour-of-events-already-indexed.ndjson)
