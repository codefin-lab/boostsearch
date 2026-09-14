# API surface -- 15. Page 500, and exporting the lot

Every request this example makes, in the order it makes them. Generated
from `run.sh`; if the two disagree, `run.sh` is right.

| Step | Method | Path | Body |
|---|---|---|---|
| fifty thousand events across three shards | `PUT` | `/$IDX` | `requests/01-fifty-thousand-events-across-three-shards.json` |
|  | `POST` | `/$IDX/_bulk?refresh=true` | `/tmp/events.ndjson` |
|  | `GET` | `/_cat/count/$IDX?v` | inline |
| page one is free | `GET` | `/$IDX/_search` | inline |
| page five hundred by from/size -- and why it is refused | `GET` | `/$IDX/_search` | inline |
| search_after: the cursor that costs the same on page 500 as on page 1 | `GET` | `/$IDX/_search` | inline |
|  | `GET` | `/$IDX/_search` | inline |
| paging through a frozen view | `POST` | `/$IDX/_doc?refresh=true` | inline |
| give the point in time back | `DELETE` | `/_search/point_in_time` | inline |
| scroll: the older way, still the right one for a full export | `DELETE` | `/_search/scroll` | inline |
| a sliced scroll, so an export can be run in parallel | `POST` | `/$IDX/_search?scroll=1m` | inline |
| counting: exact is expensive, and usually not needed | `GET` | `/$IDX/_search` | inline |
|  | `GET` | `/$IDX/_search` | inline |
| raising the window, if you really must | `PUT` | `/$IDX/_settings` | inline |
|  | `GET` | `/$IDX/_search` | inline |

## Endpoints touched

Path parameters and query strings removed, deduplicated:

- `/<var>`
- `/<var>/_bulk`
- `/<var>/_doc`
- `/<var>/_search`
- `/<var>/_settings`
- `/_cat/count/<var>`
- `/_search/point_in_time`
- `/_search/scroll`

## Request bodies

- [`requests/01-fifty-thousand-events-across-three-shards.json`](../requests/01-fifty-thousand-events-across-three-shards.json)
