# API surface -- 4. Where is the nearest one that is open

Every request this example makes, in the order it makes them. Generated
from `run.sh`; if the two disagree, `run.sh` is right.

| Step | Method | Path | Body |
|---|---|---|---|
| shops as points, delivery areas as shapes | `PUT` | `/$IDX` | `requests/01-shops-as-points-delivery-areas-as.json` |
| shops around Bangkok, written three of the ways a geo_point may be written | `POST` | `/$IDX/_bulk?refresh=wait_for` | `data/01-shops-around-bangkok-written-three-of.ndjson` |
| within 3 km of where I am standing, nearest first, with the distance | `GET` | `/$IDX/_search` | `requests/02-within-3-km-of-where-i.json` |
| what is inside the rectangle the map is showing | `GET` | `/$IDX/_search` | `requests/03-what-is-inside-the-rectangle-the.json` |
| an arbitrary drawn area, not a rectangle | `GET` | `/$IDX/_search` | `requests/04-an-arbitrary-drawn-area-not-a.json` |
| who delivers to this address -- a point against every shop's delivery area | `GET` | `/$IDX/_search` | `requests/05-who-delivers-to-this-address-a.json` |
| rings: how many are near, how many are a ride away | `GET` | `/$IDX/_search` | `requests/06-rings-how-many-are-near-how.json` |
| a heat map, as a map draws one: a grid of cells at a zoom level | `GET` | `/$IDX/_search` | `requests/07-a-heat-map-as-a-map.json` |
| open now, near me, and good -- the query a phone actually sends | `GET` | `/$IDX/_search` | `requests/08-open-now-near-me-and-good.json` |
| the distance, as a field the client can read without recomputing it | `GET` | `/$IDX/_search` | `requests/09-the-distance-as-a-field-the.json` |

## Endpoints touched

Path parameters and query strings removed, deduplicated:

- `/<var>`
- `/<var>/_bulk`
- `/<var>/_search`

## Request bodies

- [`requests/01-shops-as-points-delivery-areas-as.json`](../requests/01-shops-as-points-delivery-areas-as.json)
- [`requests/02-within-3-km-of-where-i.json`](../requests/02-within-3-km-of-where-i.json)
- [`requests/03-what-is-inside-the-rectangle-the.json`](../requests/03-what-is-inside-the-rectangle-the.json)
- [`requests/04-an-arbitrary-drawn-area-not-a.json`](../requests/04-an-arbitrary-drawn-area-not-a.json)
- [`requests/05-who-delivers-to-this-address-a.json`](../requests/05-who-delivers-to-this-address-a.json)
- [`requests/06-rings-how-many-are-near-how.json`](../requests/06-rings-how-many-are-near-how.json)
- [`requests/07-a-heat-map-as-a-map.json`](../requests/07-a-heat-map-as-a-map.json)
- [`requests/08-open-now-near-me-and-good.json`](../requests/08-open-now-near-me-and-good.json)
- [`requests/09-the-distance-as-a-field-the.json`](../requests/09-the-distance-as-a-field-the.json)
- [`data/01-shops-around-bangkok-written-three-of.ndjson`](../data/01-shops-around-bangkok-written-three-of.ndjson)
