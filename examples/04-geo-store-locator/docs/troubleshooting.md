# Troubleshooting

## A shop is in the wrong place, or in the sea

The array spelling of a point is `[lon, lat]`. Almost every other thing in the
world says latitude first. Check the document:

```bash
curl -s localhost:9264/stores/_doc/g3 | jq '._source.location'
```

Thonglor Bakery should be near 13.72 N, 100.58 E. If it has come out near
100 N it is the wrong way round -- and 100 N is not a latitude, so the write
would have been refused; the dangerous case is when both numbers are valid
latitudes.

## `geo_distance` returns nothing

Three usual causes:

- the field is not a `geo_point`. A location written into an index with no
  mapping becomes an object with two numeric fields, and the geo queries do not
  see it. Check `curl -s localhost:9264/stores/_mapping | jq`;
- the distance has no unit. `"distance": "3"` is metres in some engines and an
  error in others -- always write `"3km"`;
- the origin is the wrong way round, as above.

## `geo_shape` query returns nothing

A GeoJSON polygon must be closed -- the first and last coordinate pairs
identical -- and wound as GeoJSON expects. The polygons in `data/` are closed;
if you edit one, keep the last pair the same as the first.

Also check the `relation`: `intersects` (any overlap), `within` (entirely
inside), `contains`, `disjoint`. Asking `within` when you meant `intersects`
returns an empty answer, not an error.

## `geohash_grid` returns one enormous bucket

`precision` is too low. Precision 1 is a cell thousands of kilometres across;
5 is about 5 km, which is what step 8 uses. For `geotile_grid` the number is
the map zoom level, so it is whatever the map is showing.

## Step 9 returns shops that are closed

The opening-hours model is two integers and cannot express a place that closes
after midnight. See "Opening hours as two integers" in `design.md` -- this is a
known limitation of the example, not a bug in the query.

## `script_fields` in step 10 fails with `No field found for [location]`

`doc["location"]` needs the field to have doc values, which a `geo_point` has
by default. If the mapping was changed to `"doc_values": false`, the script
cannot read it. The guard in the script -- `doc["location"].size() == 0` --
handles a document with no location, not a field with no doc values.

## Distances look wrong by about 0.2%

`distance_type` defaults to `arc`. If it was set to `plane` the answer is a
flat-earth approximation, which at city scale is out by roughly that much.

## Cleaning up

```bash
make clean          # deletes stores
```
