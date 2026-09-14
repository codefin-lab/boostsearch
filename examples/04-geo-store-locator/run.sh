#!/usr/bin/env bash
# Where is the nearest one that is open, and how far away is it.
source "$(dirname "$0")/lib.sh"
IDX=stores

step "shops as points, delivery areas as shapes"
gone "/$IDX"
reqf PUT "/$IDX" requests/01-shops-as-points-delivery-areas-as.json
green "$IDX"

step "shops around Bangkok, written three of the ways a geo_point may be written"
ndjson "/$IDX/_bulk?refresh=wait_for" data/01-shops-around-bangkok-written-three-of.ndjson
expect_docs "$IDX" 6 "and every one of the three point spellings was accepted"

step "within 3 km of where I am standing, nearest first, with the distance"
reqf GET "/$IDX/_search" requests/02-within-3-km-of-where-i.json
note "the sort value on each hit is the distance in metres"

step "what is inside the rectangle the map is showing"
reqf GET "/$IDX/_search" requests/03-what-is-inside-the-rectangle-the.json

step "an arbitrary drawn area, not a rectangle"
reqf GET "/$IDX/_search" requests/04-an-arbitrary-drawn-area-not-a.json

step "who delivers to this address -- a point against every shop's delivery area"
reqf GET "/$IDX/_search" requests/05-who-delivers-to-this-address-a.json
note "the areas overlap on purpose: an address can have more than one shop"

step "rings: how many are near, how many are a ride away"
reqf GET "/$IDX/_search" requests/06-rings-how-many-are-near-how.json

step "a heat map, as a map draws one: a grid of cells at a zoom level"
reqf GET "/$IDX/_search" requests/07-a-heat-map-as-a-map.json

step "open now, near me, and good -- the query a phone actually sends"
reqf GET "/$IDX/_search" requests/08-open-now-near-me-and-good.json
note "gauss decay is the difference between 'within 12 km' and 'sorted by how near'"

step "the distance, as a field the client can read without recomputing it"
reqf GET "/$IDX/_search" requests/09-the-distance-as-a-field-the.json

step "what this example leaves behind, checked rather than assumed"
done_
