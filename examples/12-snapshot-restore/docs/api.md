# API surface -- 12. A backup that is proved, not merely taken

Every request this example makes, in the order it makes them. Generated
from `run.sh`; if the two disagree, `run.sh` is right.

| Step | Method | Path | Body |
|---|---|---|---|
| the repository -- the path must be inside the node's allowed repo path | `GET` | `/_snapshot` | inline |
|  | `PUT` | `/_snapshot/$REPO` | inline |
| the repository answers, and is writable by every node | `POST` | `/_snapshot/$REPO/_verify` | inline |
| something worth backing up | `PUT` | `/$IDX` | `requests/01-something-worth-backing-up.json` |
|  | `PUT` | `/other` | inline |
|  | `POST` | `/$IDX/_bulk?refresh=true` | `/tmp/ledger.ndjson` |
|  | `POST` | `/other/_doc?refresh=true` | inline |
| take it, and wait for it | `PUT` | `/_snapshot/$REPO/nightly-1?wait_for_completion=true` | `requests/02-take-it-and-wait-for-it.json` |
| what is in the repository, and what the snapshot holds | `GET` | `/_snapshot/$REPO/_all` | inline |
|  | `GET` | `/_snapshot/$REPO/nightly-1/_status` | inline |
|  | `GET` | `/_cat/snapshots/$REPO?v` | inline |
| now do the damage | `POST` | `/$IDX/_delete_by_query?refresh=true` | inline |
|  | `GET` | `/$IDX/_count` | inline |
| restore beside the original, rather than over it | `POST` | `/_snapshot/$REPO/nightly-1/_restore?wait_for_completion=true` | inline |
| restoring over a live index is refused, which is the right answer | `POST` | `/_snapshot/$REPO/nightly-1/_restore?wait_for_completion=true` | inline |
| an incremental second snapshot: only what changed is written | `POST` | `/$IDX/_doc?refresh=true` | inline |
|  | `PUT` | `/_snapshot/$REPO/nightly-2?wait_for_completion=true` | inline |
|  | `GET` | `/_snapshot/$REPO/nightly-2/_status` | inline |
| a snapshot policy, so nobody has to remember | `POST` | `/_plugins/_sm/policies/nightly` | inline |
| throwing one away, and tidying the repository | `DELETE` | `/_snapshot/$REPO/nightly-1` | inline |
|  | `POST` | `/_snapshot/$REPO/_cleanup` | inline |
|  | `GET` | `/_cat/snapshots/$REPO?v` | inline |

## Endpoints touched

Path parameters and query strings removed, deduplicated:

- `/<var>`
- `/<var>/_bulk`
- `/<var>/_count`
- `/<var>/_delete_by_query`
- `/<var>/_doc`
- `/_cat/snapshots/<var>`
- `/_plugins/_sm/policies/nightly`
- `/_snapshot`
- `/_snapshot/<var>`
- `/_snapshot/<var>/_all`
- `/_snapshot/<var>/_cleanup`
- `/_snapshot/<var>/_verify`
- `/_snapshot/<var>/nightly-1`
- `/_snapshot/<var>/nightly-1/_restore`
- `/_snapshot/<var>/nightly-1/_status`
- `/_snapshot/<var>/nightly-2`
- `/_snapshot/<var>/nightly-2/_status`
- `/other`
- `/other/_doc`

## Request bodies

- [`requests/01-something-worth-backing-up.json`](../requests/01-something-worth-backing-up.json)
- [`requests/02-take-it-and-wait-for-it.json`](../requests/02-take-it-and-wait-for-it.json)
