# API surface -- 22. Finding the clause, and showing where it is

Every request this example makes, in the order it makes them. Generated
from `run.sh`; if the two disagree, `run.sh` is right.

| Step | Method | Path | Body |
|---|---|---|---|
| an index that keeps positions and offsets for the clause text | `DELETE` | `/$IDX` | none |
|  | `PUT` | `/$IDX` | `requests/01-an-index-that-keeps-positions-and.json` |
|  | `GET` | `/_cluster/health/$IDX?wait_for_status=yellow&timeout=5s` | none |
| twelve clauses from three contracts | `POST` | `/$IDX/_bulk?refresh=wait_for` | `data/01-twelve-clauses-from-three-contracts.ndjson` |
|  | `GET` | `/$IDX/_count` | none |
| the clause, and the words in it that found it | `GET` | `/$IDX/_search` | `requests/02-the-clause-and-the-words-that.json` |
|  | `GET` | `/$IDX/_search` | inline (the hit count) |
| the same hit through each of the three highlighters | `GET` | `/$IDX/_search` | `requests/03-the-same-hit-through-each-highlighter.json`, with `type` set to `unified`, `plain`, `fvh` |
| found by what the clause is, marked by the words a reviewer is looking for | `GET` | `/$IDX/_search` | `requests/04-find-by-type-mark-by-words.json` |
| a stemmed match, marked on the unstemmed text | `GET` | `/$IDX/_search` | `requests/05-a-stemmed-match-marked-on-the.json` |
| a phrase, with room for words in between | `GET` | `/$IDX/_search` | inline (slop 0) |
|  | `GET` | `/$IDX/_search` | `requests/06-a-phrase-with-room-for-words.json` |
| terminate, then notice, within so many positions | `GET` | `/$IDX/_search` | `requests/07-terminate-before-notice-within-n-positions.json`, with `slop` 5 and 8 |
| clauses that open with the supplier, not merely mention it | `GET` | `/$IDX/_search` | `requests/08-clauses-that-open-with-the-supplier.json` |
|  | `GET` | `/$IDX/_search` | inline (`term` on `supplier`, for comparison) |
| a prefix inside a positional query | `GET` | `/$IDX/_search` | `requests/09-a-prefix-inside-a-positional-query.json` |
| intervals: notify, then 'without undue delay' | `GET` | `/$IDX/_search` | `requests/10-notify-then-without-undue-delay-close.json`, with `max_gaps` 4 and 8 |
| intervals: a way out of the contract, followed by what triggers it | `GET` | `/$IDX/_search` | `requests/11-a-way-out-followed-by-its.json` |
| intervals: liability ... limited, but not 'liability is not limited' | `GET` | `/$IDX/_search` | inline (without the filter) |
|  | `GET` | `/$IDX/_search` | `requests/12-liability-limited-but-not-not-limited.json` |
| clauses like this one | `GET` | `/$IDX/_search` | `requests/13-clauses-like-this-one.json` |
| title and body scored as one field | `GET` | `/$IDX/_search` | `requests/14-title-and-body-scored-as-one.json` |
| why c3 scored what it scored | `GET` | `/$IDX/_explain/c3` | inline |
| what the index holds for c3 | `GET` | `/$IDX/_termvectors/c3?fields=body&positions=true&offsets=true&field_statistics=false` | none |
| what this example leaves behind | `GET` | `/$IDX/_count` | none |

`$IDX` is `clauses`. Most searches are sent twice: once to print the answer,
and once more by `expect_hits` or `expect_marked` to check it.

## Endpoints touched

Path parameters and query strings removed, deduplicated:

- `/`
- `/<var>`
- `/<var>/_bulk`
- `/<var>/_count`
- `/<var>/_explain/<var>`
- `/<var>/_search`
- `/<var>/_termvectors/<var>`
- `/_cluster/health/<var>`

## Request bodies

- [`requests/01-an-index-that-keeps-positions-and.json`](../requests/01-an-index-that-keeps-positions-and.json)
- [`requests/02-the-clause-and-the-words-that.json`](../requests/02-the-clause-and-the-words-that.json)
- [`requests/03-the-same-hit-through-each-highlighter.json`](../requests/03-the-same-hit-through-each-highlighter.json)
- [`requests/04-find-by-type-mark-by-words.json`](../requests/04-find-by-type-mark-by-words.json)
- [`requests/05-a-stemmed-match-marked-on-the.json`](../requests/05-a-stemmed-match-marked-on-the.json)
- [`requests/06-a-phrase-with-room-for-words.json`](../requests/06-a-phrase-with-room-for-words.json)
- [`requests/07-terminate-before-notice-within-n-positions.json`](../requests/07-terminate-before-notice-within-n-positions.json)
- [`requests/08-clauses-that-open-with-the-supplier.json`](../requests/08-clauses-that-open-with-the-supplier.json)
- [`requests/09-a-prefix-inside-a-positional-query.json`](../requests/09-a-prefix-inside-a-positional-query.json)
- [`requests/10-notify-then-without-undue-delay-close.json`](../requests/10-notify-then-without-undue-delay-close.json)
- [`requests/11-a-way-out-followed-by-its.json`](../requests/11-a-way-out-followed-by-its.json)
- [`requests/12-liability-limited-but-not-not-limited.json`](../requests/12-liability-limited-but-not-not-limited.json)
- [`requests/13-clauses-like-this-one.json`](../requests/13-clauses-like-this-one.json)
- [`requests/14-title-and-body-scored-as-one.json`](../requests/14-title-and-body-scored-as-one.json)
- [`data/01-twelve-clauses-from-three-contracts.ndjson`](../data/01-twelve-clauses-from-three-contracts.ndjson)
