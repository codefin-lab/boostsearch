# API surface -- 8. Painless, in every place it runs

Every request this example makes, in the order it makes them. Generated
from `run.sh`; if the two disagree, `run.sh` is right.

| Step | Method | Path | Body |
|---|---|---|---|
| a fleet of vehicles | `PUT` | `/$IDX` | `requests/01-a-fleet-of-vehiclesx.json` |
|  | `POST` | `/$IDX/_bulk?refresh=wait_for` | `data/01-a-fleet-of-vehicles.ndjson` |
| a field that is computed, not stored | `GET` | `/$IDX/_search` | `requests/02-a-field-that-is-computed-not.json` |
| a script that decides what matches, not what is shown | `GET` | `/$IDX/_search` | `requests/03-a-script-that-decides-what-matches.json` |
| a script that decides the order | `GET` | `/$IDX/_search` | `requests/04-a-script-that-decides-the-order.json` |
| a script that decides the score | `GET` | `/$IDX/_search` | `requests/05-a-script-that-decides-the-score.json` |
| a script that buckets -- and one that computes across buckets | `GET` | `/$IDX/_search` | `requests/06-a-script-that-buckets-and-one.json` |
| scripted_metric: an answer no built-in aggregation gives | `GET` | `/$IDX/_search` | `requests/07-scripted-metric-an-answer-no-built.json` |
| a script that writes: update one document without reading it first | `POST` | `/$IDX/_update/c1` | `requests/08-a-script-that-writes-update-one.json` |
|  | `GET` | `/$IDX/_doc/c1?_source_includes=plate,km,trips` | inline |
| upsert: write it if it is not there, change it if it is | `POST` | `/$IDX/_update/c9` | `requests/09-upsert-write-it-if-it-is.json` |
| a script over every matching document at once | `POST` | `/$IDX/_update_by_query?refresh=true&conflicts=proceed` | `requests/10-a-script-over-every-matching-document.json` |
|  | `GET` | `/$IDX/_search` | inline |
| stored scripts, so the body is not sent every time | `PUT` | `/_scripts/depreciated-value` | `requests/11-stored-scripts-so-the-body-is.json` |
|  | `GET` | `/$IDX/_search` | `requests/12-stored-scripts-so-the-body-is.json` |
| Lucene expressions, for the arithmetic that does not need a language | `GET` | `/$IDX/_search` | `requests/13-lucene-expressions-for-the-arithmetic-that.json` |
| what the engine will run, and where | `GET` | `/_script_language` | inline |
|  | `GET` | `/_script_context` | inline |
| a script that will not compile, and what it says | `POST` | `/$IDX/_search` | inline |

## Endpoints touched

Path parameters and query strings removed, deduplicated:

- `/<var>`
- `/<var>/_bulk`
- `/<var>/_doc/c1`
- `/<var>/_search`
- `/<var>/_update/c1`
- `/<var>/_update/c9`
- `/<var>/_update_by_query`
- `/_script_context`
- `/_script_language`
- `/_scripts/depreciated-value`

## Request bodies

- [`requests/01-a-fleet-of-vehiclesx.json`](../requests/01-a-fleet-of-vehiclesx.json)
- [`requests/02-a-field-that-is-computed-not.json`](../requests/02-a-field-that-is-computed-not.json)
- [`requests/03-a-script-that-decides-what-matches.json`](../requests/03-a-script-that-decides-what-matches.json)
- [`requests/04-a-script-that-decides-the-order.json`](../requests/04-a-script-that-decides-the-order.json)
- [`requests/05-a-script-that-decides-the-score.json`](../requests/05-a-script-that-decides-the-score.json)
- [`requests/06-a-script-that-buckets-and-one.json`](../requests/06-a-script-that-buckets-and-one.json)
- [`requests/07-scripted-metric-an-answer-no-built.json`](../requests/07-scripted-metric-an-answer-no-built.json)
- [`requests/08-a-script-that-writes-update-one.json`](../requests/08-a-script-that-writes-update-one.json)
- [`requests/09-upsert-write-it-if-it-is.json`](../requests/09-upsert-write-it-if-it-is.json)
- [`requests/10-a-script-over-every-matching-document.json`](../requests/10-a-script-over-every-matching-document.json)
- [`requests/11-stored-scripts-so-the-body-is.json`](../requests/11-stored-scripts-so-the-body-is.json)
- [`requests/12-stored-scripts-so-the-body-is.json`](../requests/12-stored-scripts-so-the-body-is.json)
- [`requests/13-lucene-expressions-for-the-arithmetic-that.json`](../requests/13-lucene-expressions-for-the-arithmetic-that.json)
- [`data/01-a-fleet-of-vehicles.ndjson`](../data/01-a-fleet-of-vehicles.ndjson)
