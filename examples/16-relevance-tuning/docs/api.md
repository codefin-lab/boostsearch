# API surface -- 16. Making search better, and proving it

Every request this example makes, in the order it makes them. Generated
from `run.sh`; if the two disagree, `run.sh` is right.

| Step | Method | Path | Body |
|---|---|---|---|
| a documentation site's pages | `PUT` | `/$IDX` | `requests/01-a-documentation-site-s-pagesx.json` |
|  | `POST` | `/$IDX/_bulk?refresh=wait_for` | `data/01-a-documentation-site-s-pages.ndjson` |
| the baseline: a plain match, and what it gets wrong | `GET` | `/$IDX/_search` | `requests/02-the-baseline-a-plain-match-and.json` |
| why -- the arithmetic of one document's score | `GET` | `/$IDX/_explain/d3` | inline |
| first fix: say what the fields are worth, and demote the deprecated | `GET` | `/$IDX/_search` | `requests/03-first-fix-say-what-the-fields.json` |
| second fix: a phrase is worth more than the two words apart | `GET` | `/$IDX/_search` | `requests/04-second-fix-a-phrase-is-worth.json` |
| third fix: fresh and popular, decayed rather than added | `GET` | `/$IDX/_search` | `requests/05-third-fix-fresh-and-popular-decayed.json` |
| rescoring: cheap query over everything, expensive one over the top few | `GET` | `/$IDX/_search` | `requests/06-rescoring-cheap-query-over-everything-expensive.json` |
| the same words asked four ways, side by side | `GET` | `/$IDX/_search` | inline |
| now prove it: a judgement list, and a score for each ranking | `GET` | `/$IDX/_rank_eval` | inline |
|  | `GET` | `/$IDX/_rank_eval` | inline |
| where the time actually goes | `GET` | `/$IDX/_search?profile=true` | inline |
| the two similarities, measured | `GET` | `/$IDX/_search` | inline |
|  | `GET` | `/$IDX/_search` | inline |

## Endpoints touched

Path parameters and query strings removed, deduplicated:

- `/<var>`
- `/<var>/_bulk`
- `/<var>/_explain/d3`
- `/<var>/_rank_eval`
- `/<var>/_search`

## Request bodies

- [`requests/01-a-documentation-site-s-pagesx.json`](../requests/01-a-documentation-site-s-pagesx.json)
- [`requests/02-the-baseline-a-plain-match-and.json`](../requests/02-the-baseline-a-plain-match-and.json)
- [`requests/03-first-fix-say-what-the-fields.json`](../requests/03-first-fix-say-what-the-fields.json)
- [`requests/04-second-fix-a-phrase-is-worth.json`](../requests/04-second-fix-a-phrase-is-worth.json)
- [`requests/05-third-fix-fresh-and-popular-decayed.json`](../requests/05-third-fix-fresh-and-popular-decayed.json)
- [`requests/06-rescoring-cheap-query-over-everything-expensive.json`](../requests/06-rescoring-cheap-query-over-everything-expensive.json)
- [`data/01-a-documentation-site-s-pages.ndjson`](../data/01-a-documentation-site-s-pages.ndjson)
