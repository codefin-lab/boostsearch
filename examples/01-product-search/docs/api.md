# API surface -- 1. A product search that has to be good

Every request this example makes, in the order it makes them. Generated
from `run.sh`; if the two disagree, `run.sh` is right.

| Step | Method | Path | Body |
|---|---|---|---|
| an index whose analysis chain knows the shop's vocabulary | `PUT` | `/$IDX` | `requests/01-an-index-whose-analysis-chain-knows.json` |
| the catalogue | `POST` | `/$IDX/_bulk?refresh=wait_for` | `data/01-the-catalogue.ndjson` |
| the synonym does its work: 'notebook' finds what is called a laptop, and the other way round | `GET` | `/$IDX/_search` | `requests/02-the-synonym-does-its-work-notebook.json` |
| what the analyser actually did to the text | `POST` | `/$IDX/_analyze` | inline |
| the search a search box makes: several fields, weighted, with a typo allowed | `GET` | `/$IDX/_search` | `requests/03-the-search-a-search-box-makes.json` |
| relevance shaped by the business: popular, well rated and in stock beat merely matching | `GET` | `/$IDX/_search` | `requests/04-relevance-shaped-by-the-business-popular.json` |
| why that document scored what it scored | `GET` | `/$IDX/_explain/p3` | inline |
| one row per brand, rather than four rows of the same brand | `GET` | `/$IDX/_search` | `requests/05-one-row-per-brand-rather-than.json` |
| as-you-type, from the completion field | `GET` | `/$IDX/_search` | `requests/06-as-you-type-from-the-completion.json` |
| did you mean: a term the index does hold | `GET` | `/$IDX/_search` | `requests/07-did-you-mean-a-term-the.json` |
| the same search, stored once and called by name | `PUT` | `/_scripts/product-search` | `requests/08-the-same-search-stored-once-and.json` |
|  | `GET` | `/$IDX/_search/template` | inline |
| what the template renders to, without running it | `POST` | `/_render/template` | inline |
| four searches in one round trip, the way a page with four widgets asks | `POST` | `/$IDX/_msearch` | `data/02-four-searches-in-one-round-trip.ndjson` |
| where the time went | `GET` | `/$IDX/_search?profile=true` | `requests/09-where-the-time-went.json` |

## Endpoints touched

Path parameters and query strings removed, deduplicated:

- `/<var>`
- `/<var>/_analyze`
- `/<var>/_bulk`
- `/<var>/_explain/p3`
- `/<var>/_msearch`
- `/<var>/_search`
- `/<var>/_search/template`
- `/_render/template`
- `/_scripts/product-search`

## Request bodies

- [`requests/01-an-index-whose-analysis-chain-knows.json`](../requests/01-an-index-whose-analysis-chain-knows.json)
- [`requests/02-the-synonym-does-its-work-notebook.json`](../requests/02-the-synonym-does-its-work-notebook.json)
- [`requests/03-the-search-a-search-box-makes.json`](../requests/03-the-search-a-search-box-makes.json)
- [`requests/04-relevance-shaped-by-the-business-popular.json`](../requests/04-relevance-shaped-by-the-business-popular.json)
- [`requests/05-one-row-per-brand-rather-than.json`](../requests/05-one-row-per-brand-rather-than.json)
- [`requests/06-as-you-type-from-the-completion.json`](../requests/06-as-you-type-from-the-completion.json)
- [`requests/07-did-you-mean-a-term-the.json`](../requests/07-did-you-mean-a-term-the.json)
- [`requests/08-the-same-search-stored-once-and.json`](../requests/08-the-same-search-stored-once-and.json)
- [`requests/09-where-the-time-went.json`](../requests/09-where-the-time-went.json)
- [`data/01-the-catalogue.ndjson`](../data/01-the-catalogue.ndjson)
- [`data/02-four-searches-in-one-round-trip.ndjson`](../data/02-four-searches-in-one-round-trip.ndjson)
