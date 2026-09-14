# Examples

Twenty-six use cases. Each one is **a project of its own**: its own documentation,
its own request bodies, its own node configuration, its own `Makefile`. Nothing
in an example directory reaches outside it, so any one of them can be copied
somewhere else and still run.

Between them they touch every part of the feature table in the top-level
README: search and relevance, aggregations, analysis in five languages,
scripting in all of its contexts, ingest and search pipelines, vectors,
security, snapshots, index management, the cluster, SQL and PPL.

## The examples

| | What it is about | Port | The thing worth reading it for |
|---|---|---|---|
| [01](01-product-search/) | A product search that has to be good | 9261 | synonyms, fuzziness, `function_score`, `collapse`, suggesters, templates |
| [02](02-log-analytics/) | Logs that manage their own life | 9262 | ISM rollover and delete, pipeline aggregations, `significant_terms` |
| [03](03-faceted-commerce/) | A faceted listing page, done properly | 9263 | one `nested` clause versus two; `post_filter` versus a filter |
| [04](04-geo-store-locator/) | Where is the nearest one that is open | 9264 | `geo_distance` sort, `geo_shape`, grids, `gauss` decay on a point |
| [05](05-vector-search/) | Vector search that is not only vector search | 9265 | filtered k-NN, hybrid with `match`, reranking the neighbours |
| [06](06-multi-tenant-security/) | One index, several customers, no proxy | 9266 | DLS, FLS, field masking, and why every read path honours it |
| [07](07-ingest-pipelines/) | Raw lines in, documents out | 9267 | grok, geoip, user-agent, and a dead-letter index via `on_failure` |
| [08](08-painless-scripting/) | Painless, in every place it runs | 9268 | `doc` against `ctx`, and `scripted_metric` |
| [09](09-multilingual-analysis/) | Text that is not English | 9269 | Thai, Japanese, Korean, Chinese, ICU, phonetic |
| [10](10-sql-and-ppl/) | SQL and PPL over the same index | 9270 | the same report in both languages, in five output shapes |
| [11](11-parent-join/) | One-to-many, modelled twice | 9271 | `join` against `nested`, and the update cost that decides it |
| [12](12-snapshot-restore/) | A backup that is proved, not merely taken | 9272 | restore beside the original and compare the numbers |
| [13](13-cluster-failover/) | Three nodes, and one of them killed | 9340-2 | green to yellow, writing through the loss, recovery, copies compared |
| [14](14-reindex-migration/) | Changing a mapping on a live index | 9274 | alias first, sliced reindex, atomic swap, shrink and split |
| [15](15-deep-pagination/) | Page 500, and exporting the lot | 9275 | `search_after`, point-in-time, scroll, and what each one fixes |
| [16](16-relevance-tuning/) | Making search better, and proving it | 9276 | `_rank_eval` before and after, so the argument is about a number |
| [17](17-percolator-alerts/) | Saved searches that find the documents | 9277 | `percolate` over one event and several, document slots, highlighting the event |
| [18](18-data-streams-and-templates/) | Metrics that arrive forever | 9278 | component and composable templates, a data stream made by writing, rollover |
| [19](19-search-pipelines/) | Changing a search without changing the client | 9279 | request and response processors, a default pipeline, `ignore_failure` |
| [20](20-long-running-work/) | Work that takes longer than a request | 9280 | `_update_by_query` and `_delete_by_query` in the background, `_tasks`, conflicts |
| [21](21-service-accounts-and-audit/) | Machines that log in, and a record of it | 9281 | least-privilege roles, `authinfo`, password rotation, the audit log |
| [22](22-contract-clause-search/) | Finding the clause, and showing where it is | 9282 | three highlighters, spans and intervals, `more_like_this`, `_explain` |
| [23](23-document-library/) | Files in, searchable text out | 9283 | `attachment`, `foreach`, `dissect`, verbose simulate, default and final pipelines |
| [24](24-sales-analytics/) | A sales report from aggregations alone | 9284 | `composite` export, a transform and a rollup, `multi_terms`, `rare_terms`, bucket scripts and selectors |
| [25](25-operations-runbook/) | The questions an operator asks at 3 a.m. | 9285 | health, `_cat`, allocation explain, blocks, force merge, stats |
| [26](26-tenants-by-routing/) | Many small customers in one index | 9286 | custom routing, filtered aliases with routing, `preference`, `terms` lookup |

Each has a port of its own, so several can be running at once without
colliding.

## Running one

```bash
cargo build --release          # once, in the repository root

cd examples/05-vector-search
make serve                     # a node with exactly what this example needs
make run                       # in another terminal
```

`make` on its own lists the targets:

| | |
|---|---|
| `make serve` | start a node for this example, in the foreground |
| `make run` | run the example against it |
| `make check` | syntax and JSON checks; needs no server |
| `make clean` | delete what the example left in the server |

Four examples need the node started differently -- a short index-management
interval, security switched on, a snapshot path, a reindex allowlist. `node.sh`
in each of those does it, so `make serve` is all you need; `.env.example` says
what each setting is for and what happens without it.

Example 13 is the odd one: it starts and stops its own three nodes and ignores
`BS` entirely. Do not run it at the same time as `tools/cluster_chaos.py`.

## Running all of them

```bash
examples/run-all.sh
```

This starts one node with everything the shared examples need and runs the
twenty-two that can share it, in order. 06 and 21 (security-enabled nodes of
their own), 13 (its own cluster) and 25 (it reads the health of the whole
node) are skipped, and their READMEs say how to run them.

## What is in an example

```
05-vector-search/
├── README.md               what it is, how to run it, what to look for
├── docs/
│   ├── design.md           why it is built this way; what would change at scale
│   ├── api.md              every request it makes, every endpoint it touches
│   └── troubleshooting.md  what goes wrong, and what it means
├── run.sh                  the example
├── node.sh                 a node configured for exactly this example
├── lib.sh                  shell helpers -- its own copy, so the directory stands alone
├── Makefile                serve, run, check, clean
├── .env.example            the settings, one comment each
├── requests/               request bodies, one file per request
└── data/                   bulk document sets
```

`run.sh` prints every request it makes and every answer it gets, with a note in
between saying why the step is there -- they are written to be read as much as
run. The request bodies are files rather than inline JSON so they can be edited
and reused:

```bash
cd examples/05-vector-search
curl -s localhost:9265/papers/_search -H 'content-type: application/json' \
  --data-binary @requests/02-the-three-nearest-papers-to-a.json | jq
```

## Conventions

Every example is written to be run more than once: it deletes what it creates
before creating it, so a run that failed halfway leaves nothing in the way of
the next one. Each README ends with a "Leaves behind" section saying exactly
what is still there afterwards, and `make clean` removes the indices.

`docs/api.md` is generated from `run.sh`. If the two ever disagree, `run.sh` is
right.
