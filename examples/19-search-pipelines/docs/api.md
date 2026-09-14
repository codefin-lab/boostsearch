# API surface -- 19. Changing what a search asks and answers, without changing the client

Every request this example makes, in the order it makes them. Generated
from `run.sh`; if the two disagree, `run.sh` is right.

| Step | Method | Path | Body |
|---|---|---|---|
| a bookshop catalogue: twenty-four books, some of them sold out | `DELETE` | `/$IDX` | inline |
|  | `DELETE` | `$P/storefront`, `$P/one-per-author`, `$P/last-year` | inline |
|  | `PUT` | `/$IDX` | `requests/01-a-bookshop-catalogue-an-index.json` |
|  | `GET` | `/_cluster/health/$IDX?wait_for_status=yellow&timeout=5s` | inline |
|  | `POST` | `/$IDX/_bulk?refresh=wait_for` | `data/01-a-bookshop-catalogue-twenty-four-books.ndjson` |
|  | `GET` | `/$IDX/_count` | inline |
| what the shop app sends, answered with no pipeline at all | `GET` | `/$IDX/_search` | `requests/02-what-the-app-sends.json` |
| a pipeline: only what is in stock, and the price under the name the app knows | `PUT` | `$P/storefront` | `requests/03-storefront-in-stock-and-price.json` |
|  | `GET` | `$P/storefront` | inline |
| the same request, naming the pipeline on the URL | `GET` | `/$IDX/_search?search_pipeline=storefront` | `requests/02-what-the-app-sends.json` |
| changing a pipeline is writing it again: version 2 caps the page and orders the formats | `PUT` | `$P/storefront` | `requests/04-storefront-version-two.json` |
|  | `GET` | `$P/storefront` | inline |
|  | `GET` | `/$IDX/_search?search_pipeline=storefront` | `requests/02-what-the-app-sends.json` |
| the home page carousel asks for four, and one author fills it | `GET` | `/$IDX/_search` | `requests/05-the-carousel-asks-for-four.json` |
| one book per author: oversample, collapse, truncate | `PUT` | `$P/one-per-author` | `requests/06-one-book-per-author.json` |
|  | `GET` | `/$IDX/_search?search_pipeline=one-per-author` | `requests/05-the-carousel-asks-for-four.json` |
| a default pipeline on the index, so the app need not name it | `PUT` | `/$IDX/_settings` | `requests/07-the-default-pipeline-for-books.json` |
|  | `GET` | `/$IDX/_settings/index.search.*` | inline |
|  | `GET` | `/$IDX/_search` | `requests/02-what-the-app-sends.json` |
| the back office opts out with search_pipeline=_none | `GET` | `/$IDX/_search?search_pipeline=_none` | `requests/02-what-the-app-sends.json` |
| a pipeline written inline in the body, to try a processor before storing it | `GET` | `/$IDX/_search` | `requests/08-an-inline-pipeline-tried-before.json` |
| a pipeline written for last year's mapping: one processor fails, the whole search fails | `PUT` | `$P/last-year` | `requests/09-a-pipeline-written-for-last-year.json` |
|  | `GET` | `/$IDX/_search?search_pipeline=last-year` (answers 400) | `requests/02-what-the-app-sends.json` |
| the same pipeline with ignore_failure on the processor that no longer applies | `PUT` | `$P/last-year` | `requests/10-the-same-with-ignore-failure.json` |
|  | `GET` | `/$IDX/_search?search_pipeline=last-year` | `requests/02-what-the-app-sends.json` |
| the pipelines that exist, and deleting one | `GET` | `$P` | inline |
|  | `DELETE` | `$P/last-year` | inline |
|  | `GET` | `$P/last-year` (answers 404) | inline |
|  | `GET` | `$P` | inline |
| what this example leaves behind, checked rather than assumed | `GET` | `/$IDX/_count` | inline |

`$IDX` is `books`; `$P` is `/_search/pipeline`.

## Endpoints touched

Path parameters and query strings removed, deduplicated:

- `/`
- `/<var>`
- `/<var>/_bulk`
- `/<var>/_count`
- `/<var>/_search`
- `/<var>/_settings`
- `/<var>/_settings/<var>`
- `/_cluster/health/<var>`
- `/_search/pipeline`
- `/_search/pipeline/<var>`

## Query parameters that select a pipeline

- `search_pipeline=<name>` -- use this stored pipeline instead of the index default
- `search_pipeline=_none` -- use no pipeline, not even the index default

## Request bodies

- [`requests/01-a-bookshop-catalogue-an-index.json`](../requests/01-a-bookshop-catalogue-an-index.json)
- [`requests/02-what-the-app-sends.json`](../requests/02-what-the-app-sends.json)
- [`requests/03-storefront-in-stock-and-price.json`](../requests/03-storefront-in-stock-and-price.json)
- [`requests/04-storefront-version-two.json`](../requests/04-storefront-version-two.json)
- [`requests/05-the-carousel-asks-for-four.json`](../requests/05-the-carousel-asks-for-four.json)
- [`requests/06-one-book-per-author.json`](../requests/06-one-book-per-author.json)
- [`requests/07-the-default-pipeline-for-books.json`](../requests/07-the-default-pipeline-for-books.json)
- [`requests/08-an-inline-pipeline-tried-before.json`](../requests/08-an-inline-pipeline-tried-before.json)
- [`requests/09-a-pipeline-written-for-last-year.json`](../requests/09-a-pipeline-written-for-last-year.json)
- [`requests/10-the-same-with-ignore-failure.json`](../requests/10-the-same-with-ignore-failure.json)
- [`data/01-a-bookshop-catalogue-twenty-four-books.ndjson`](../data/01-a-bookshop-catalogue-twenty-four-books.ndjson)
