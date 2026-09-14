# Troubleshooting

## `Pipeline <name> is not defined`

A 400 on a search that names a pipeline with `?search_pipeline=` means no
pipeline by that name is stored. Names are exact and case-sensitive. List what
there is:

```bash
curl -s localhost:9279/_search/pipeline | jq 'keys'
```

A missing pipeline named as an index's `index.search.default_pipeline` fails
every search on the index the same way, so a default is deleted after the
setting that names it, never before. Check the setting:

```bash
curl -s 'localhost:9279/books/_settings/index.search.*'
```

## `Document with id N is missing field X`

`rename_field` found a hit without the field. Three usual causes:

- the field really is gone from some documents (step 11 of the example) --
  add `"ignore_missing": true` to the processor;
- the request filters `_source`, and the field is not in the list. Source
  filtering happens before the response processors run, so a client asking
  for `"_source": ["title", "price"]` has already lost `price_eur` by the time
  the rename looks for it. Ask for the stored name, or for the whole document;
- the pipeline is older than the mapping, and names a field by a spelling the
  documents no longer use.

## A pipeline seems to do nothing

In order of likelihood:

1. The request passed `search_pipeline=_none`, or carries an inline
   `search_pipeline` object: either replaces the index default entirely.
2. The index has no `index.search.default_pipeline`, or it names something
   else.
3. The processor is under the wrong key. Request processors go under
   `request_processors`, response processors under `response_processors`;
   `filter_query` under `response_processors` is refused with
   `Invalid processor type filter_query`.
4. It is a `phase_results_processors` entry on a search without a `hybrid`
   query: those only ever act on a hybrid query's scores.
5. The search names several indices whose defaults disagree, or none at all
   (`/_search`): a default applies only when every index that has one agrees,
   and a search that names no index takes none.

## The page is shorter than `size`

Expected with three processors in this example:

- the `script` in `storefront` caps `size` at 5, whatever the client asked;
- `collapse` removes hits after the search, so a page can shrink below `size`
  unless `oversample` asked for more first;
- `truncate_hits` cuts to `target_size`, or to the size `oversample` recorded.

`hits.total` still counts every match in all three cases. A total larger than
the page is not a bug.

## `collapse` in the pipeline returns duplicates of an author

The response processor keeps the first hit of each value **in the page it was
given**. If the page is sorted by something other than relevance, the one kept
is the first in that order, not the best. And it compares the values in
`_source`; a request whose `_source` list leaves out the collapse field makes
every hit look like the same missing value, and only one survives.

## `script_exception` from the script request processor

The script sees the search body as `ctx._source`, but only its scalar settings:
`from`, `size`, `explain`, `version`, `seq_no_primary_term`, `track_scores`,
`track_total_hits`, `min_score`, `terminate_after`, `profile`. The query is not
there, so `ctx._source.query` is null and calling anything on it fails. The
answer names `processor_type: script` and, if the processor has one, its `tag`
-- give every processor a tag, it is how the error points at the right one.

## A `hybrid` query answers with scores like `-9549512000`

The search ran without a pipeline holding a `normalization-processor` or a
`score-ranker-processor`, so nothing combined the parts: what comes back is
each sub-query's list as the shard collected it, with marker scores between
the lists. Name the pipeline, or make it the index default. A `hybrid` query
paged past the first page also needs `pagination_depth`.

## Cleaning up

```bash
make clean          # deletes books and the search pipelines storefront, one-per-author, last-year
```
