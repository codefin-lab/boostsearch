# API surface -- 2. Logs that manage their own life

Every request this example makes, in the order it makes them. Generated
from `run.sh`; if the two disagree, `run.sh` is right.

| Step | Method | Path | Body |
|---|---|---|---|
| a template, so every index the alias rolls into is shaped the same | `PUT` | `/_index_template/logs-template` | `requests/01-a-template-so-every-index-the.json` |
| the policy: roll at 5 documents, go cold after a moment, then be deleted | `PUT` | `/_plugins/_ism/policies/logs-lifecycle` | `requests/02-the-policy-roll-at-5-documents.json` |
| the first index, and the alias everything writes through | `PUT` | `/logs-000001` | inline |
|  | `POST` | `/_plugins/_ism/add/logs-000001` | inline |
| a morning of traffic, written through the alias | `POST` | `/logs/_bulk?refresh=true` | `/tmp/logs.ndjson` |
|  | `GET` | `/_cat/indices/logs-*?v&h=index,docs.count,status` | inline |
| the shape of the morning, by service | `GET` | `/logs/_search` | `requests/03-the-shape-of-the-morning-by.json` |
| only the half hours that were actually bad, and how fast they got worse | `GET` | `/logs/_search` | `requests/04-only-the-half-hours-that-were.json` |
| the latency an SLO is written against | `GET` | `/logs/_search` | `requests/05-the-latency-an-slo-is-written.json` |
| the noisiest paths, and what each of them costs in bytes | `GET` | `/logs/_search` | `requests/06-the-noisiest-paths-and-what-each.json` |
| the terms that mark out the bad window from the rest of the morning | `GET` | `/logs/_search` | `requests/07-the-terms-that-mark-out-the.json` |
| the policy at work -- it may take a few ticks | `GET` | `/_plugins/_ism/explain/logs-000001` | inline |
|  | `GET` | `/_cat/indices/logs-*?v&h=index,docs.count,status` | inline |
| the alias always points at the one index that takes writes | `GET` | `/_cat/aliases/logs?v` | inline |

## Endpoints touched

Path parameters and query strings removed, deduplicated:

- `/_cat/aliases/logs`
- `/_cat/indices/logs-*`
- `/_index_template/logs-template`
- `/_plugins/_ism/add/logs-000001`
- `/_plugins/_ism/explain/logs-000001`
- `/_plugins/_ism/policies/logs-lifecycle`
- `/logs-000001`
- `/logs/_bulk`
- `/logs/_search`

## Request bodies

- [`requests/01-a-template-so-every-index-the.json`](../requests/01-a-template-so-every-index-the.json)
- [`requests/02-the-policy-roll-at-5-documents.json`](../requests/02-the-policy-roll-at-5-documents.json)
- [`requests/03-the-shape-of-the-morning-by.json`](../requests/03-the-shape-of-the-morning-by.json)
- [`requests/04-only-the-half-hours-that-were.json`](../requests/04-only-the-half-hours-that-were.json)
- [`requests/05-the-latency-an-slo-is-written.json`](../requests/05-the-latency-an-slo-is-written.json)
- [`requests/06-the-noisiest-paths-and-what-each.json`](../requests/06-the-noisiest-paths-and-what-each.json)
- [`requests/07-the-terms-that-mark-out-the.json`](../requests/07-the-terms-that-mark-out-the.json)
