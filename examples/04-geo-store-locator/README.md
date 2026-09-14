# 4. Where is the nearest one that is open

Everything a store locator needs, in the order the screen needs it: what is
within walking distance, what is inside the rectangle the map is showing, who
delivers to a typed address, how the pins cluster when the map zooms out, and
finally the ranking a phone actually wants -- not "within 12 km" but "sorted by
how near, weighted by how good".

## What it shows

| Step | Feature |
|---|---|
| 1 | `geo_point` and `geo_shape` mappings |
| 2 | the three accepted `geo_point` spellings: object, `"lat,lon"` string, `[lon, lat]` array |
| 3 | `geo_distance` query, `_geo_distance` sort with `unit` and `distance_type` |
| 4 | `geo_bounding_box` |
| 5 | `geo_polygon` |
| 6 | `geo_shape` query, point against polygon, `relation: intersects` |
| 7 | `geo_distance` aggregation with keyed ranges, `geo_centroid`, `geo_bounds` |
| 8 | `geohash_grid`, `geotile_grid` |
| 9 | `function_score` with `gauss` decay on a geo field, plus `field_value_factor` |
| 10 | `script_fields` calling `arcDistance` in Painless |

## Running it

```bash
make serve      # a node configured for this example, port 9264, foreground
make run        # the example, in another terminal
```

`make check` validates the scripts and every request body without a server;
`make clean` deletes what the example left behind. Copy `.env.example` to
`.env` to change the address or anything else.

The longer form, and what this example needs of the node:

```bash
./target/release/boostsearch &
examples/04-geo-store-locator/run.sh
```

## What to look for

- **Step 2** writes the same kind of value three different ways. Note the array
  form is `[lon, lat]` -- longitude first, which is GeoJSON's order and the
  opposite of the string form. This trips people up constantly; the example
  keeps all three next to each other so the difference is visible.
- **Step 3** puts the distance in each hit's `sort` array. There is no need for
  a separate distance query; the sort already computed it.
- **Step 6** asks the opposite question from step 3: not "which shops are near
  this point" but "which shops' delivery areas contain this point". The
  polygons overlap, so more than one shop answers.
- **Step 9** is the one worth reading twice. The `filter` decides who is
  eligible; the `gauss` decay decides the order, falling off over a 2 km scale
  with a 200 m flat zone. A filter alone gives you an unordered set of eligible
  shops; this gives you a ranked list.

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
| `requests/` | 9 request bodies, one file each |
| `data/` | 1 bulk document set |

## Leaves behind

The index `stores`. Rerunning deletes it first.
