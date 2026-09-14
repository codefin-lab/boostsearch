# API surface -- 7. Raw lines in, documents out

Every request this example makes, in the order it makes them. Generated
from `run.sh`; if the two disagree, `run.sh` is right.

| Step | Method | Path | Body |
|---|---|---|---|
| try the pipeline before writing anything with it | `POST` | `/_ingest/pipeline/_simulate` | `requests/01-try-the-pipeline-before-writing-anything.json` |
| the pipeline, properly, with everything a log line needs doing to it | `PUT` | `/_ingest/pipeline/enrich-client` | `requests/02-the-pipeline-properly-with-everything-a.json` |
|  | `PUT` | `/_ingest/pipeline/access-log` | `requests/03-the-pipeline-properly-with-everything-a.json` |
| the default pipeline, so a writer need not name it | `PUT` | `/$IDX` | `requests/04-the-default-pipeline-so-a-writer.json` |
| raw lines, written without the writer knowing anything about them | `POST` | `/$IDX/_bulk?refresh=wait_for` | `data/01-raw-lines-written-without-the-writer.ndjson` |
| what came out | `GET` | `/$IDX/_search` | inline |
| and where the line that would not parse went | `GET` | `/failed-lines/_search?ignore_unavailable=true` | inline |
| what the enrichment is worth: browsers, sections, outcomes | `GET` | `/$IDX/_search` | `requests/05-what-the-enrichment-is-worth-browsers.json` |
| a grok pattern, tried on its own | `GET` | `/_ingest/processor/grok` | inline |
| search pipelines: the answer reshaped on its way out | `PUT` | `/_search/pipeline/tidy-results` | `requests/06-search-pipelines-the-answer-reshaped-on.json` |
|  | `GET` | `/$IDX/_search?search_pipeline=tidy-results` | inline |
| the pipelines that exist | `GET` | `/_ingest/pipeline` | inline |
| reprocessing what is already written, without re-reading the source | `POST` | `/$IDX/_update_by_query?refresh=true&conflicts=proceed` | `requests/07-reprocessing-what-is-already-written-without.json` |
|  | `GET` | `/$IDX/_search` | inline |

## Endpoints touched

Path parameters and query strings removed, deduplicated:

- `/<var>`
- `/<var>/_bulk`
- `/<var>/_search`
- `/<var>/_update_by_query`
- `/_ingest/pipeline`
- `/_ingest/pipeline/_simulate`
- `/_ingest/pipeline/access-log`
- `/_ingest/pipeline/enrich-client`
- `/_ingest/processor/grok`
- `/_search/pipeline/tidy-results`
- `/failed-lines/_search`

## Request bodies

- [`requests/01-try-the-pipeline-before-writing-anything.json`](../requests/01-try-the-pipeline-before-writing-anything.json)
- [`requests/02-the-pipeline-properly-with-everything-a.json`](../requests/02-the-pipeline-properly-with-everything-a.json)
- [`requests/03-the-pipeline-properly-with-everything-a.json`](../requests/03-the-pipeline-properly-with-everything-a.json)
- [`requests/04-the-default-pipeline-so-a-writer.json`](../requests/04-the-default-pipeline-so-a-writer.json)
- [`requests/05-what-the-enrichment-is-worth-browsers.json`](../requests/05-what-the-enrichment-is-worth-browsers.json)
- [`requests/06-search-pipelines-the-answer-reshaped-on.json`](../requests/06-search-pipelines-the-answer-reshaped-on.json)
- [`requests/07-reprocessing-what-is-already-written-without.json`](../requests/07-reprocessing-what-is-already-written-without.json)
- [`data/01-raw-lines-written-without-the-writer.ndjson`](../data/01-raw-lines-written-without-the-writer.ndjson)
