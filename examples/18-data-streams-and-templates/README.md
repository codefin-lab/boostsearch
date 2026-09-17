# 18. Metrics that arrive forever, shaped by templates

Host metrics never stop coming and are never corrected: a sample is written
once, read for a few weeks, and thrown away. A single index is the wrong shape
for that, and the older answer -- a template, a numbered index made by hand, an
alias marked as the write index, all kept in step -- is three things to get
wrong. A data stream is one name to write to and read from, with a row of
indices behind it that the name rolls through, every one of them made from a
template.

This example builds the templates first, out of reusable pieces, asks the
server what an index would be made from before any index exists, and settles a
contest between two templates that both match. Then it makes a stream, fills
it, rolls it over, fills the new generation, searches across both, sets the
older alias arrangement beside it for contrast, and deletes the stream.

Rolling over on a schedule is example 02's subject (ISM over plain indices);
here every rollover is asked for by hand, so each generation can be watched
arriving.

## What it shows

| Step | Feature |
|---|---|
| 2 | `_component_template` holding settings, with `_meta` |
| 3 | `_component_template` holding mappings; `GET` with a wildcard |
| 4 | `_index_template` with `composed_of`, `data_stream: {}`, `priority`, its own `template` block |
| 5 | `_index_template/_simulate_index` -- the merged result, before the index exists |
| 6 | a second template at a higher priority; `overlapping` in the simulation |
| 7 | two templates at the same priority over the same pattern, refused |
| 8 | a stream created by its first write: `_bulk` with `create` actions; `GET /_data_stream/{name}`; `.ds-` backing indices in `_cat/indices` |
| 9 | the backing index's `_mapping` and `_settings`, from the templates; `_data_stream_timestamp` |
| 10 | a document without `@timestamp`, refused -- alone, and as one item of a bulk |
| 11 | `_resolve/index` naming a data stream |
| 12 | `GET /_data_stream/{name}/_stats` |
| 13 | a write by id to a stream, refused (`op_type` must be `create`) |
| 14 | `_rollover` with `conditions` that do not hold |
| 15 | `_rollover` on the stream, unconditional; generation 2 |
| 16 | writes after the rollover landing in the new backing index |
| 17 | deleting the stream's write index, refused |
| 18 | a search over both generations: `terms` on `_index` and on `host`, `date_histogram` with `avg` / `max` |
| 19 | a time window that straddles the rollover, sorted, with `stats` |
| 20 | `PUT /_data_stream/{name}` ahead of any write, from the higher-priority template; `GET /_data_stream/metrics-*`; deleting a template in use, refused |
| 21 | the contrast: `_template` (legacy, `order`), a bootstrapped `-000001` index with `is_write_index`, alias `_rollover`, `_cat/aliases` |
| 22 | `DELETE /_data_stream/{name}` and every generation with it |
| 23 | `_cat/indices`, `_cat/templates` |

## Running it

```bash
make serve      # a node configured for this example, port 9278, foreground
make run        # the example, in another terminal
```

`make check` validates the scripts and every request body without a server;
`make clean` deletes what the example left behind -- streams, indices and
templates. Copy `.env.example` to `.env` to change the address or anything
else.

The longer form, and what this example needs of the node:

```bash
./target/release/velosearch &
examples/18-data-streams-and-templates/run.sh
```

Nothing special: data streams, templates and rollover need no setting.

## What to look for

- **Step 5** answers with the index a name *would* get -- one shard, no
  replicas, `refresh_interval: 1s`, `codec: best_compression`, and the six
  fields -- with nothing created. It is the cheapest way to find out what two
  component templates and an index template add up to, and the way to check a
  template change before the next rollover picks it up.
- **Step 6** asks the same question for `metrics-edge-cpu`, which both
  templates match. The answer has `refresh_interval: 30s`, the extra `site`
  field, and no `codec` -- the priority 200 template made it on its own, and
  `metrics` appears only under `overlapping`. Composable templates are not
  layered the way legacy templates are; the highest priority wins outright,
  which is why the shared parts belong in component templates.
- **Step 7** is refused at write time: a second `metrics-*` template at
  priority 100 would leave nothing to choose between the two.
- **Step 8** writes 72 samples to `metrics-node-cpu`, a name nothing has been
  created under. The first write makes the stream, and every item answers
  `"_index": ".ds-metrics-node-cpu-000001"`.
- **Step 9** is step 5 come true: the backing index has `host` as `keyword`
  (dynamic mapping would have made it `text`), `number_of_replicas: 0`,
  `codec: best_compression`, and `_data_stream_timestamp: {enabled: true}`,
  which the template's `data_stream` added.
- **Step 10**: a sample without `@timestamp` is refused with 400,
  `mapper_parsing_exception` caused by "documents must contain a single-valued
  timestamp field '@timestamp' of date type". In a bulk only that item fails;
  the other is written, so the stream holds 73.
- **Step 11**: `_resolve/index/metrics-*` lists no index and no alias, but one
  data stream with its backing index and its time field.
- **Steps 14 to 16** are the rollover. A rollover conditioned on `max_docs:
  1000000` answers `rolled_over: false`; the unconditional one makes
  `.ds-metrics-node-cpu-000002` and `generation: 2`; the next 24 samples,
  written to the same name, all land there. Generation 1 still holds exactly
  73.
- **Step 18** reads 97 documents from two shards through one name.
  `by_generation` shows generation 1 covering 06:00 to 08:55 and generation 2
  09:00 to 09:50. The hourly maximum jumps from 55.0 at 08:00 to 92.3 at
  09:00, and `by_host` names the host: `db01`, maximum 92.3 and average 62.2,
  against 42.8, 37.9 and 32.8 for the web hosts. That aggregation works only
  because the template made `host` a keyword.
- **Step 19** asks for `db01` from 08:30 to 09:30 and gets 6 samples, 3 from
  each generation, with `cpu` running from 47.2 to 92.3. The query never names
  a generation.
- **Step 20** makes `metrics-edge-cpu` with an explicit `PUT` before any sample
  arrives. Its backing index has `refresh_interval: 30s` and a `site` field --
  it was made by `metrics-edge`, and `GET /_data_stream/metrics-*` says so.
- **Step 21** reaches the same place as step 15 the older way, and costs a
  legacy template, an index created by hand with a `-000001` name, and an alias
  with `is_write_index` -- three things that have to agree, where the stream
  needed one template.
- **Step 22**: after `DELETE /_data_stream/metrics-node-cpu`, both backing
  indices answer 404, and the edge stream is still there.

## This directory

It is a project of its own: nothing here reaches outside the directory,
so it can be copied somewhere else and still run.

| | |
|---|---|
| `README.md` | this page |
| `docs/design.md` | why it is built this way, and what would change at scale |
| `docs/api.md` | every request it makes, and every endpoint it touches |
| `docs/troubleshooting.md` | what goes wrong, and what it means |
| `run.sh` | the example |
| `node.sh` | a node configured for exactly what this example needs |
| `lib.sh` | shell helpers; its own copy |
| `Makefile` | `serve`, `run`, `check`, `clean` |
| `.env.example` | the settings, with a line each on what they are for |
| `requests/` | 10 request bodies, one file each |
| `data/` | 3 bulk document sets |

## Leaves behind

The data stream `metrics-edge-cpu` (one empty backing index), the indices
`legacy-metrics-000001` (4 documents) and `legacy-metrics-000002` with the
alias `legacy-metrics`, the index templates `metrics` and `metrics-edge`, the
component templates `metrics-settings` and `metrics-mappings`, and the legacy
template `legacy-metrics`. The stream `metrics-node-cpu` is deleted by the
example itself. Rerunning deletes all of it first; `make clean` deletes it
without rerunning.
