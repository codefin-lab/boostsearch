# Troubleshooting

## Every document ends up in `failed-lines`

The grok pattern does not match. Find out which processor failed and why:

```bash
curl -s localhost:9267/failed-lines/_search | jq '.hits.hits[]._source | {error, message}'
```

Or run one line through `_simulate` with `verbose`:

```bash
curl -s localhost:9267/_ingest/pipeline/access-log/_simulate?verbose=true \
  -H 'content-type: application/json' -d '{"docs":[{"_source":{"message":"<your line>"}}]}' | jq
```

The usual culprits in a combined-log pattern are a missing referrer (a bare `-`
rather than `"-"`), and `%{DATA:...}` being greedy across a quote.

## `geoip` produces nothing, or `client.geo.located: false`

The databases are not where the node is looking. See `docs/geoip.md` in the
repository for where they go, and start the node with
`BOOSTSEARCH_GEOIP_PATH` pointing at them -- `make serve` sets it to
`/tmp/geoip-db`.

The `on_failure` on that processor means the rest of the example still works
without them; that is deliberate.

## `user_agent` leaves the field untouched

The processor needs its regexes file. Without it the field is copied rather
than parsed and the `browsers` aggregation in step 7 comes back empty. This is
the same class of problem as geoip: an optional data file the node needs.

## `_index` cannot be changed in `on_failure`

Some versions only allow `_index` to be set from an `on_failure` block, not
from an ordinary processor. If the redirect does not work, check the
`on_failure` is at the *pipeline* level -- indented at the same depth as
`processors`, not inside one.

## The `script` processor fails with `cannot access method/field`

Ingest scripts see `ctx`, which is the source document as a map. They do **not**
see `doc` -- that is a search-time construct reading doc values, and there are
no doc values for a document that has not been indexed yet. If a script works
in a `script_field` and fails in a pipeline, this is why.

## `rename_field` in the search pipeline does nothing

`response_processors` operate on the hits as they are returned. If `_source`
filtering (the `_source` list in the request) already dropped the field, there
is nothing to rename. Step 9 asks for both the old and new names so the effect
is visible.

## `_update_by_query` in step 11 returns a version conflict

Expected if something else is writing. `conflicts=proceed` counts them and
continues rather than aborting; the answer reports `version_conflicts`.

## Nothing at all is parsed, and the documents look like the raw input

`index.default_pipeline` was not applied, usually because the index already
existed. Step 2 deletes it; if you created it by hand first, delete it and
rerun.

## Cleaning up

```bash
make clean          # deletes access-log and failed-lines
curl -XDELETE localhost:9267/_ingest/pipeline/access-log
curl -XDELETE localhost:9267/_ingest/pipeline/enrich-client
curl -XDELETE localhost:9267/_search/pipeline/tidy-results
```
