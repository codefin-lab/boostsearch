# Design notes

## Two geo types, two different questions

| Type | Holds | Answers |
|---|---|---|
| `geo_point` | one location | "which shops are near *me*" |
| `geo_shape` | an area | "which shops' delivery areas contain *this address*" |

They are not interchangeable and the direction matters. Step 3 puts the point
in the query and the shops' points in the index. Step 6 puts the point in the
query and the shops' *areas* in the index -- the same input, the opposite
relation. A locator needs both, and conflating them is why "does he deliver
here" so often gets answered with "he is 4 km away".

## The three spellings of a point, and the one that catches people

```json
{"lat": 13.7279, "lon": 100.5241}    object
"13.7370,100.5601"                   string: lat first
[100.5860, 13.7280]                  array: lon first
```

The array is GeoJSON order -- longitude, then latitude -- and it is the
opposite of the string. This is not a quirk of any one engine; it is GeoJSON
against every-day convention, and it has put shops in the sea for as long as
both have existed. All three appear in step 2 on purpose, so the difference is
visible rather than remembered.

## Why the ranking is a decay, not a filter

Step 9 is the query a phone actually sends, and it has two parts doing
different jobs:

```
filter   geo_distance 12km, opens <= 10, closes >= 10     eligibility
gauss    origin=me, scale=2km, offset=200m, decay=0.5     order
```

A filter alone gives an unordered set: every shop within 12 km, tied. Sorting
by `_geo_distance` gives distance order but throws away every other signal --
a 3.8-star kiosk 100 m away beats a 4.7-star roastery 400 m away, always.

`gauss` turns distance into a multiplier: flat inside 200 m (`offset` --
anything this close is "here"), half weight at 2 km (`scale` and `decay`),
falling off after. Multiply that by a rating factor and both signals count,
with an explicit exchange rate between them. That exchange rate is the thing to
tune, and having it written down as two numbers is the point.

## Why `_geo_distance` sort rather than a script

The sort's `sort` array on each hit already carries the distance in the unit
asked for. A `script_field` computing `arcDistance` (step 10) gives the same
number and costs a script execution per hit. The script is shown because
sometimes the distance is wanted *without* sorting by it -- but if you are
sorting anyway, read it off the sort.

`distance_type: arc` is great-circle; `plane` is a flat approximation, cheaper
and wrong by a fraction of a percent at city scale. For a store locator either
is fine; for anything nautical it is not.

## Why `geohash_grid` and `geotile_grid` are both there

Both bucket points into cells for a heat map. They differ in what a cell is:

- `geohash_grid` -- base-32 geohash cells, which are not square and change
  aspect ratio with latitude;
- `geotile_grid` -- the Web Mercator tile scheme every slippy map already uses,
  so `precision` is the map's own zoom level and the cells line up with the
  tiles being drawn.

If the answer is going onto a Leaflet or Mapbox map, `geotile_grid` is the one
whose precision means something.

## Opening hours as two integers

`opens: 7, closes: 22` is crude and works for the filter in step 9. It breaks
for the Phuket bar, which closes at 02:00: `closes: 2` makes `closes >= 10`
false and the bar never appears as open at 10:00 -- which is correct, but it
would also never appear at 23:00, which is not.

A real schema stores a list of intervals per weekday, and the "open now" filter
becomes a `nested` query over them (example 3's shape). The two integers are
kept here because the example is about geography, and it is flagged here rather
than hidden.

## What would change at scale

- **`geo_shape` indexing is expensive** relative to `geo_point`, in both index
  size and query time. Six polygons is nothing; a delivery polygon per shop for
  ten thousand shops wants checking.
- **`geo_bounds` and `geo_centroid`** over a full index are cheap; over a
  filtered subset they are cheaper still, and a map should always pass its own
  viewport as a filter.
- **A `gauss` decay is computed per hit.** Behind a `rescore` window it is
  cheap at any index size; as part of the main query it is not.
