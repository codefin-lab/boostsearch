# API surface -- 3. A faceted listing page, done properly

Every request this example makes, in the order it makes them. Generated
from `run.sh`; if the two disagree, `run.sh` is right.

| Step | Method | Path | Body |
|---|---|---|---|
| products with variants, which are their own documents inside the product | `PUT` | `/$IDX` | `requests/01-products-with-variants-which-are-their.json` |
| the catalogue | `POST` | `/$IDX/_bulk?refresh=wait_for` | `data/01-the-catalogue.ndjson` |
| the naive nested query -- and why it is wrong | `GET` | `/$IDX/_search` | `requests/02-the-naive-nested-query-and-why.json` |
| the right one: ONE variant that is both size 42 and in stock | `GET` | `/$IDX/_search` | `requests/03-the-right-one-one-variant-that.json` |
| the listing page: results narrowed by colour, facets that are not | `GET` | `/$IDX/_search` | `requests/04-the-listing-page-results-narrowed-by.json` |
| a facet that IS narrowed, next to one that is not, in the same request | `GET` | `/$IDX/_search` | `requests/05-a-facet-that-is-narrowed-next.json` |
| price facets over the nested variants, and only the buyable ones | `GET` | `/$IDX/_search` | `requests/06-price-facets-over-the-nested-variants.json` |
| sort by the price of the cheapest variant that is actually in stock | `GET` | `/$IDX/_search` | `requests/07-sort-by-the-price-of-the.json` |
| page two, without counting past it | `GET` | `/$IDX/_search` | `requests/08-page-two-without-counting-past-it.json` |
| how many, without the hits | `GET` | `/$IDX/_count` | inline |
| what a client may query, field by field | `GET` | `/$IDX/_field_caps?fields=variants.price,colour,rating` | inline |

## Endpoints touched

Path parameters and query strings removed, deduplicated:

- `/<var>`
- `/<var>/_bulk`
- `/<var>/_count`
- `/<var>/_field_caps`
- `/<var>/_search`

## Request bodies

- [`requests/01-products-with-variants-which-are-their.json`](../requests/01-products-with-variants-which-are-their.json)
- [`requests/02-the-naive-nested-query-and-why.json`](../requests/02-the-naive-nested-query-and-why.json)
- [`requests/03-the-right-one-one-variant-that.json`](../requests/03-the-right-one-one-variant-that.json)
- [`requests/04-the-listing-page-results-narrowed-by.json`](../requests/04-the-listing-page-results-narrowed-by.json)
- [`requests/05-a-facet-that-is-narrowed-next.json`](../requests/05-a-facet-that-is-narrowed-next.json)
- [`requests/06-price-facets-over-the-nested-variants.json`](../requests/06-price-facets-over-the-nested-variants.json)
- [`requests/07-sort-by-the-price-of-the.json`](../requests/07-sort-by-the-price-of-the.json)
- [`requests/08-page-two-without-counting-past-it.json`](../requests/08-page-two-without-counting-past-it.json)
- [`data/01-the-catalogue.ndjson`](../data/01-the-catalogue.ndjson)
