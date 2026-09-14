# API surface -- 18. Metrics that arrive forever, shaped by templates

Every request this example makes, in the order it makes them. Generated
from `run.sh`; if the two disagree, `run.sh` is right. The `gone` deletes of
step 1 and the `_count` reads behind each document check are left out of the
table and listed under the endpoints.

| Step | Method | Path | Body |
|---|---|---|---|
| a component template for the settings -- how big, how often refreshed | `PUT` | `/_component_template/metrics-settings` | `requests/01-a-component-template-for-the-settings.json` |
| a component template for the mappings -- the fields every sample carries | `PUT` | `/_component_template/metrics-mappings` | `requests/02-a-component-template-for-the-mappings.json` |
|  | `GET` | `/_component_template/metrics-*` | none |
| the index template: composed of both, a data stream, priority 100 | `PUT` | `/_index_template/metrics` | `requests/03-the-index-template-composed-of-both.json` |
|  | `GET` | `/_index_template/metrics` | none |
| what an index called metrics-node-cpu would be made from, asked before it exists | `POST` | `/_index_template/_simulate_index/metrics-node-cpu` | none |
| a narrower template for edge boxes, at priority 200 | `PUT` | `/_index_template/metrics-edge` | `requests/04-a-narrower-template-that-outranks-it.json` |
|  | `POST` | `/_index_template/_simulate_index/metrics-edge-cpu` | none |
|  | `POST` | `/_index_template/_simulate_index/metrics-node-cpu` | none |
| a second template at the same priority and the same pattern is refused | `PUT` | `/_index_template/metrics-rival` | `requests/05-a-rival-at-the-same-priority.json` |
| a morning of samples, written to a name that does not exist yet | `POST` | `/metrics-node-cpu/_bulk?refresh=true` | `data/01-a-morning-of-samples-from-four.ndjson` |
|  | `GET` | `/_data_stream/metrics-node-cpu` | none |
|  | `GET` | `/_cat/indices/.ds-metrics-node-cpu-*?v&h=index,docs.count&s=index` | none |
| the backing index was born from the template, not from the documents | `GET` | `/.ds-metrics-node-cpu-000001/_mapping` | none |
|  | `GET` | `/.ds-metrics-node-cpu-000001/_mapping` | none (one value checked) |
|  | `GET` | `/.ds-metrics-node-cpu-000001/_mapping` | none (one value checked) |
|  | `GET` | `/.ds-metrics-node-cpu-000001/_settings?filter_path=**.number_of_shards,**.number_of_replicas,**.refresh_interval,**.codec` | none |
|  | `GET` | `/.ds-metrics-node-cpu-000001/_settings` | none (one value checked) |
|  | `GET` | `/.ds-metrics-node-cpu-000001/_settings` | none (one value checked) |
| a sample without @timestamp is refused | `POST` | `/metrics-node-cpu/_doc` | inline |
|  | `POST` | `/metrics-node-cpu/_bulk?refresh=true&filter_path=errors,items.*.status,items.*.error.caused_by.reason` | inline (two items) |
| the stream, as _resolve/index sees the name | `GET` | `/_resolve/index/metrics-*` | none |
| what the stream holds, in numbers | `GET` | `/_data_stream/metrics-node-cpu/_stats?human=true` | none |
| writing a document by id is refused: a stream is append-only | `PUT` | `/metrics-node-cpu/_doc/sample-1` | inline |
| a rollover with a condition that does not hold changes nothing | `POST` | `/metrics-node-cpu/_rollover` | `requests/06-roll-over-only-past-a-million.json` |
| a manual rollover: a new generation, and the stream's name moves to it | `POST` | `/metrics-node-cpu/_rollover` | none |
|  | `GET` | `/_data_stream/metrics-node-cpu` | none |
| the next hour, written to the same name -- it lands in generation 2 | `POST` | `/metrics-node-cpu/_bulk?refresh=true` | `data/02-the-next-hour-after-the-rollover.ndjson` |
| the write index cannot be deleted out from under the stream | `DELETE` | `/.ds-metrics-node-cpu-000002` | none |
| searching across generations: cpu by the hour, by host, and which generation answered | `GET` | `/metrics-node-cpu/_search` | `requests/07-cpu-by-the-hour-across-both.json` |
| one host, over a window that straddles the rollover | `GET` | `/metrics-node-cpu/_search` | `requests/08-one-host-over-a-window-that.json` |
|  | `GET` | `/metrics-node-cpu/_search` | `requests/08-one-host-over-a-window-that.json` (hit count) |
| a stream made ahead of its first write, on the edge pattern | `PUT` | `/_data_stream/metrics-edge-cpu` | none |
|  | `GET` | `/.ds-metrics-edge-cpu-000001/_settings?filter_path=**.refresh_interval` | none |
|  | `GET` | `/.ds-metrics-edge-cpu-000001/_mapping` | none (one value checked) |
|  | `GET` | `/_data_stream/metrics-*` | none |
|  | `DELETE` | `/_index_template/metrics-edge` | none |
| the older way, for contrast: a legacy template, an alias, a numbered index | `PUT` | `/_template/legacy-metrics` | `requests/09-the-older-way-a-legacy-template.json` |
|  | `PUT` | `/legacy-metrics-000001` | `requests/10-the-first-index-bootstrapped-by-hand.json` |
|  | `POST` | `/legacy-metrics/_bulk?refresh=true` | `data/03-the-same-samples-the-older-way.ndjson` |
|  | `POST` | `/legacy-metrics/_rollover` | none |
|  | `GET` | `/_cat/aliases/legacy-metrics?v&h=alias,index,is_write_index&s=index` | none |
| deleting the data stream takes every generation with it | `DELETE` | `/_data_stream/metrics-node-cpu` | none |
|  | `GET` | `/_data_stream/metrics-node-cpu` | none |
|  | `GET` | `/.ds-metrics-node-cpu-000001` | none (status code only) |
|  | `GET` | `/.ds-metrics-node-cpu-000002` | none (status code only) |
|  | `GET` | `/_data_stream` | none |
| what this example leaves behind, checked rather than assumed | `GET` | `/_cat/indices/.ds-metrics-edge-cpu-*,legacy-metrics-*?v&h=index,docs.count&s=index&expand_wildcards=all` | none |
|  | `GET` | `/_cat/templates/metrics*?v&h=name,index_patterns,order,composed_of&s=name` | none |

## Endpoints touched

Path parameters and query strings removed, deduplicated:

- `/<var>`
- `/<var>/_bulk`
- `/<var>/_count`
- `/<var>/_doc`
- `/<var>/_doc/<var>`
- `/<var>/_mapping`
- `/<var>/_rollover`
- `/<var>/_search`
- `/<var>/_settings`
- `/_cat/aliases/<var>`
- `/_cat/indices/<var>`
- `/_cat/templates/<var>`
- `/_component_template/<var>`
- `/_data_stream`
- `/_data_stream/<var>`
- `/_data_stream/<var>/_stats`
- `/_index_template/<var>`
- `/_index_template/_simulate_index/<var>`
- `/_resolve/<var>/<var>`
- `/_template/<var>`

## Request bodies

- [`requests/01-a-component-template-for-the-settings.json`](../requests/01-a-component-template-for-the-settings.json)
- [`requests/02-a-component-template-for-the-mappings.json`](../requests/02-a-component-template-for-the-mappings.json)
- [`requests/03-the-index-template-composed-of-both.json`](../requests/03-the-index-template-composed-of-both.json)
- [`requests/04-a-narrower-template-that-outranks-it.json`](../requests/04-a-narrower-template-that-outranks-it.json)
- [`requests/05-a-rival-at-the-same-priority.json`](../requests/05-a-rival-at-the-same-priority.json)
- [`requests/06-roll-over-only-past-a-million.json`](../requests/06-roll-over-only-past-a-million.json)
- [`requests/07-cpu-by-the-hour-across-both.json`](../requests/07-cpu-by-the-hour-across-both.json)
- [`requests/08-one-host-over-a-window-that.json`](../requests/08-one-host-over-a-window-that.json)
- [`requests/09-the-older-way-a-legacy-template.json`](../requests/09-the-older-way-a-legacy-template.json)
- [`requests/10-the-first-index-bootstrapped-by-hand.json`](../requests/10-the-first-index-bootstrapped-by-hand.json)
- [`data/01-a-morning-of-samples-from-four.ndjson`](../data/01-a-morning-of-samples-from-four.ndjson)
- [`data/02-the-next-hour-after-the-rollover.ndjson`](../data/02-the-next-hour-after-the-rollover.ndjson)
- [`data/03-the-same-samples-the-older-way.ndjson`](../data/03-the-same-samples-the-older-way.ndjson)
