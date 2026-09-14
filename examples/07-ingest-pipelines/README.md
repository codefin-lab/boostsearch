# 7. Raw lines in, documents out

A log shipper sends a line of text. What arrives should be a document with an
IP, a timestamp, a status, a browser name and a country -- and the line that
does not parse should end up somewhere it can be looked at rather than
stopping the batch or vanishing.

## What it shows

| Step | Feature |
|---|---|
| 1 | `_ingest/pipeline/_simulate` -- try it before you write with it |
| 2 | `grok`, `date`, `dissect`, a nested `pipeline` processor, `script`, `lowercase`, `set`, `remove`, `convert` |
| 2 | `geoip` and `user_agent`, each with its own `on_failure` |
| 2 | a pipeline-level `on_failure` that redirects the document to another index |
| 3 | `index.default_pipeline` |
| 4 | a bulk of raw lines, no structure at the client |
| 6 | the failed line, in `failed-lines`, with the reason and the processor that raised it |
| 7 | aggregations over fields that did not exist in the input |
| 8 | `_ingest/processor/grok` -- the pattern library |
| 9 | a search pipeline: `rename_field` as a response processor |
| 11 | `_update_by_query` with a script |

## Running it

```bash
make serve      # a node configured for this example, port 9267, foreground
make run        # the example, in another terminal
```

`make check` validates the scripts and every request body without a server;
`make clean` deletes what the example left behind. Copy `.env.example` to
`.env` to change the address or anything else.

The longer form, and what this example needs of the node:

`geoip` needs its databases, and without them that processor's `on_failure`
sets `client.geo.located: false` rather than failing -- the rest of the example
still works. `docs/geoip.md` says where the databases go.

```bash
./target/release/boostsearch &
examples/07-ingest-pipelines/run.sh
```

## What to look for

- **Step 1** runs the pipeline against two documents without an index in
  sight. The second one fails, and the simulate answer says which processor
  and why. This is the loop to develop a pipeline in.
- **Step 2** is worth reading as a whole: `on_failure` appears at three levels.
  On `geoip` it substitutes a value; on `user_agent`, `ignore_missing` lets a
  document without one through; and at the pipeline level it rewrites
  `_index`, which is how a dead-letter index is done without a dead-letter
  queue.
- **Step 4** writes four raw strings. Nothing in that bulk request knows what
  a log line is. **Step 5** shows what came out -- and step 6 shows the fourth
  one, in `failed-lines`, carrying `error.reason` and `error.processor`.
- **Step 9** is the mirror image: an ingest pipeline changes the document on
  the way in, a search pipeline changes the answer on the way out.

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
| `requests/` | 7 request bodies, one file each |
| `data/` | 1 bulk document set |

## Leaves behind

The indices `access-log` and `failed-lines`, the ingest pipelines `access-log`
and `enrich-client`, and the search pipeline `tidy-results`. Rerunning
replaces them.
