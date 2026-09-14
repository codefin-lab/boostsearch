# API surface -- 24. A sales report built from aggregations alone

Every request this example makes, in the order it makes them. Generated
from `run.sh`; if the two disagree, `run.sh` is right.

| Step | Method | Path | Body |
|---|---|---|---|
| an index shaped for aggregation: every field a keyword, a number or a date | `POST` | `/_plugins/_transform/$SUM/_stop`, `/_plugins/_rollup/jobs/$ROLL/_stop` | none (may 404) |
|  | `DELETE` | `/_plugins/_transform/$SUM?force=true`, `/_plugins/_rollup/jobs/$ROLL`, `/$IDX`, `/$SUM`, `/$ROLLED` | none (may 404) |
|  | `PUT` | `/$IDX` | `requests/01-an-index-shaped-for-aggregation.json` |
|  | `GET` | `/_cluster/health/$IDX?wait_for_status=yellow&timeout=5s` | none |
| a year of sales lines, one document per line sold | `POST` | `/$IDX/_bulk?refresh=wait_for` | `data/01-a-year-of-sales-lines-one.ndjson` |
|  | `GET` | `/$IDX/_count` | none |
| every region, category and month: a composite, paged with after_key until it runs out | `GET` | `/$IDX/_search` | `requests/02-every-region-category-and-month-a.json`, with `after` set to the previous page's `after_key`, repeated until a page is empty |
| a summary index kept by a transform | `PUT` | `/$SUM` | `requests/03-a-summary-index-built-from-those.json` |
|  | `GET` | `/_cluster/health/$SUM?wait_for_status=yellow&timeout=5s` | none |
|  | `PUT` | `/_plugins/_transform/$SUM` | `requests/16-a-transform-that-keeps-the-summary.json`, with `start_time` set to just under a minute ago |
|  | `GET` | `/_plugins/_transform/$SUM/_explain` | none, repeated until the run is finished |
|  | `POST` | `/$SUM/_refresh` | none |
|  | `GET` | `/$SUM/_count` | none |
|  | `GET` | `/_plugins/_transform/$SUM` | none |
| the summary answers what the raw index answers, from 240 documents instead of 3004 | `GET` | `/$IDX/_search` | `requests/04-the-summary-answers-what-the-raw.json` |
|  | `GET` | `/$SUM/_search` | `requests/04-the-summary-answers-what-the-raw.json` |
|  | `GET` | `/$SUM/_search` | inline: `date_histogram` by month with `bucket_sort`, size 3 |
| a rollup of every day and region, searched as though it were the sales | `PUT` | `/_plugins/_rollup/jobs/$ROLL` | `requests/17-a-rollup-of-every-day-by-region.json`, with `start_time` set to just under a minute ago |
|  | `GET` | `/_plugins/_rollup/jobs/$ROLL/_explain` | none, repeated until the run is finished |
|  | `GET` | `/$ROLLED/_search` | `requests/04-the-summary-answers-what-the-raw.json` |
|  | `GET` | `/$ROLLED/_search` | inline: `avg` and `value_count` of `revenue` under a `term` on `region` |
|  | `GET` | `/$ROLLED/_search` | inline: `size: 10`, refused |
| which region and channel pairs sell the most | `GET` | `/$IDX/_search` | `requests/05-which-region-and-channel-pairs-sell.json` |
| the products almost nobody bought | `GET` | `/$IDX/_search` | `requests/06-the-products-almost-nobody-bought.json` |
| the year in six buckets, whatever width that takes | `GET` | `/$IDX/_search` | `requests/07-the-year-in-six-buckets-whatever.json` |
| order sizes and quarters, named by hand and keyed | `GET` | `/$IDX/_search` | `requests/08-order-sizes-and-quarters-named-by.json` |
| the biggest and the latest sale in each region | `GET` | `/$IDX/_search` | `requests/09-the-biggest-and-the-latest-sale.json` |
| the shape of price, discount and revenue | `GET` | `/$IDX/_search` | `requests/10-the-shape-of-price-discount-and.json` |
| regions ranked by margin, the weak ones dropped | `GET` | `/$IDX/_search` | `requests/11-regions-ranked-by-margin-the-weak.json` |
| bulk orders, deep discounts, and everything else | `GET` | `/$IDX/_search` | `requests/12-bulk-orders-deep-discounts-and-everything.json` |
| central against the whole company, in one request | `GET` | `/$IDX/_search` | `requests/13-central-against-the-whole-company.json` |
| what a missing field counts as | `GET` | `/$IDX/_search` | `requests/14-what-a-missing-field-counts-as.json` |
| a value the documents do not store: the quarter, computed | `GET` | `/$IDX/_search` | `requests/15-a-value-the-documents-do-not.json` |
| what this example leaves behind, checked rather than assumed | `GET` | `/$IDX/_count`, `/$SUM/_count`, `/$ROLLED/_count` | none |

`$IDX` is `sales`, `$SUM` is `sales-monthly` (the summary index and the
transform that keeps it), `$ROLL` is the rollup job `sales-daily` and
`$ROLLED` the index it writes, `sales-daily-rollup`. `lib.sh` also makes one
`GET /` before the first step, to stop early if there is no server.

## Endpoints touched

Path parameters and query strings removed, deduplicated:

- `/`
- `/<var>`
- `/<var>/_bulk`
- `/<var>/_count`
- `/<var>/_refresh`
- `/<var>/_search`
- `/_cluster/health/<var>`
- `/_plugins/_transform/<var>`
- `/_plugins/_transform/<var>/_explain`
- `/_plugins/_transform/<var>/_stop`
- `/_plugins/_rollup/jobs/<var>`
- `/_plugins/_rollup/jobs/<var>/_explain`
- `/_plugins/_rollup/jobs/<var>/_stop`

## Request bodies

- [`requests/01-an-index-shaped-for-aggregation.json`](../requests/01-an-index-shaped-for-aggregation.json)
- [`requests/02-every-region-category-and-month-a.json`](../requests/02-every-region-category-and-month-a.json)
- [`requests/03-a-summary-index-built-from-those.json`](../requests/03-a-summary-index-built-from-those.json)
- [`requests/04-the-summary-answers-what-the-raw.json`](../requests/04-the-summary-answers-what-the-raw.json)
- [`requests/05-which-region-and-channel-pairs-sell.json`](../requests/05-which-region-and-channel-pairs-sell.json)
- [`requests/06-the-products-almost-nobody-bought.json`](../requests/06-the-products-almost-nobody-bought.json)
- [`requests/07-the-year-in-six-buckets-whatever.json`](../requests/07-the-year-in-six-buckets-whatever.json)
- [`requests/08-order-sizes-and-quarters-named-by.json`](../requests/08-order-sizes-and-quarters-named-by.json)
- [`requests/09-the-biggest-and-the-latest-sale.json`](../requests/09-the-biggest-and-the-latest-sale.json)
- [`requests/10-the-shape-of-price-discount-and.json`](../requests/10-the-shape-of-price-discount-and.json)
- [`requests/11-regions-ranked-by-margin-the-weak.json`](../requests/11-regions-ranked-by-margin-the-weak.json)
- [`requests/12-bulk-orders-deep-discounts-and-everything.json`](../requests/12-bulk-orders-deep-discounts-and-everything.json)
- [`requests/13-central-against-the-whole-company.json`](../requests/13-central-against-the-whole-company.json)
- [`requests/14-what-a-missing-field-counts-as.json`](../requests/14-what-a-missing-field-counts-as.json)
- [`requests/15-a-value-the-documents-do-not.json`](../requests/15-a-value-the-documents-do-not.json)
- [`requests/16-a-transform-that-keeps-the-summary.json`](../requests/16-a-transform-that-keeps-the-summary.json)
- [`requests/17-a-rollup-of-every-day-by-region.json`](../requests/17-a-rollup-of-every-day-by-region.json)
- [`data/01-a-year-of-sales-lines-one.ndjson`](../data/01-a-year-of-sales-lines-one.ndjson), written by [`data/make-sales.py`](../data/make-sales.py)
