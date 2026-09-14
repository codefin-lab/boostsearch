# Troubleshooting

## `Result window is too large`

Working as intended -- step 3 triggers it. `from + size` exceeds
`index.max_result_window`. The fix is `search_after`, not a bigger window; see
`design.md`.

## `search_after` returns the same page every time

The cursor is not being advanced. Each request must pass the `sort` array of
the **last hit of the previous page**, not a fixed value:

```bash
... | jq '.hits.hits[-1].sort'
```

## `search_after` skips or repeats documents

The sort has no unique tiebreak. Add `_id` (or another unique field) as the
last sort key, on every request. See `design.md`.

## `search_after` returns nothing at all

The values must match the `sort` array element for element, in order and in
type. `[4.5, "s1"]` is a number and a string; `["4.5", "s1"]` is two strings
and silently returns an empty page.

## `Search context missing` on a scroll or a PIT

It expired. `keep_alive` starts from the *last* request, so a slow consumer
must either raise it or request more often. On a PIT, pass `keep_alive` in
every request as the example does.

## The PIT paging in step 6 shows the newly written documents

Then the PIT is not being used. Check the `pit.id` is in the body and that the
request goes to `/_search` (no index in the path) -- a PIT search names its
target through the PIT, and putting an index in the path as well is an error in
some versions and ignored in others.

```json
{"pit": {"id": "<id>", "keep_alive": "2m"}, "sort": [...], "search_after": [...]}
```

## `_search/point_in_time` returns 404

Point-in-time is not answered for by this node. `search_after` without a PIT
and `scroll` both still work; the freezing behaviour is what is lost.

## A scroll returns fewer documents than expected per batch

`size` is per shard. On three shards, `size: 1000` returns up to 3,000. The
batch ends when the hits array is empty, not when it is smaller than `size`.

## The sliced scroll returns overlapping documents

`id` must be unique within `max`, and every slice of the same scroll must use
the same `max`. Slicing by a field (`slice.field`) requires that field to have
doc values and a good distribution; the default slices by `_id`, which is
always safe.

## Step 1 is slow

It writes 50,000 documents in one bulk with `refresh=true`. That is one
refresh, not 50,000, so it should be a few seconds. If it is much slower the
machine is busy with something else.

## Cleaning up

```bash
make clean          # deletes events
```

If a run was interrupted, a PIT or a scroll context may still be held. They
expire on their own; to clear PITs immediately:

```bash
curl -XDELETE localhost:9275/_search/point_in_time/_all
```
