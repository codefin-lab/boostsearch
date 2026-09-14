# Troubleshooting

## Steps 3 and 4 return the same thing

They should not -- step 3 returns Trail Runner GTX and step 4 does not. If both
return it, the documents were written without the `nested` mapping, and
`variants` was indexed as a plain object.

Check the mapping:

```bash
curl -s localhost:9263/catalogue/_mapping | jq '.catalogue.mappings.properties.variants.type'
```

It must print `"nested"`. If it prints nothing, the index was created by a
write rather than by step 1's `PUT` -- delete it and rerun.

## `[nested] failed to find nested object under path [variants]`

The path is wrong, or the field is not nested. Nested paths are absolute from
the document root: `variants`, and the fields inside it are `variants.price`,
not `price`.

## An aggregation over `variants.price` returns nothing

It must be inside a `nested` aggregation. Outside one, the collector is working
on parent documents, where those fields do not exist:

```json
"aggs": { "variants": { "nested": { "path": "variants" },
          "aggs": { "cheapest": { "min": { "field": "variants.price" } } } } }
```

## The colour facet shows only one colour

The colour clause is in `query` rather than `post_filter`. That is the bug
step 5 exists to show -- move it back and the facet returns.

## `inner_hits` is missing from the results

`inner_hits` only appears on hits that matched through the `nested` clause it
is attached to. If the nested query is inside a `filter` context of a `bool`
that also has a `must` matching for other reasons, the document can be a hit
without the nested clause contributing, and there is nothing to show.

## `search_after` in step 9 returns everything, or nothing

The values passed to `search_after` must match the `sort` array exactly, in
order and in type. Step 9 sorts by `rating` then `_id`, so the cursor is
`[4.5, "s1"]` -- a float and a string. A cursor of `["4.5", "s1"]` silently
returns the wrong page.

## `_field_caps` says a field is `unmapped` on some indices

Expected when the request covers more than one index. Here there is one, so an
unmapped field means the mapping did not take.

## Cleaning up

```bash
make clean          # deletes catalogue
```
