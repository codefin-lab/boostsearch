#!/usr/bin/env bash
# A faceted listing page: the facets must count what the user would get if they
# clicked, not what they have already narrowed away.
source "$(dirname "$0")/lib.sh"
IDX=catalogue

step "products with variants, which are their own documents inside the product"
gone "/$IDX"
reqf PUT "/$IDX" requests/01-products-with-variants-which-are-their.json
green "$IDX"

step "the catalogue"
ndjson "/$IDX/_bulk?refresh=wait_for" data/01-the-catalogue.ndjson
expect_docs "$IDX" 6 "six products, fourteen variants between them"

step "the naive nested query -- and why it is wrong"
note "a product where SOME variant is size 42 and SOME variant is in stock"
reqf GET "/$IDX/_search" requests/02-the-naive-nested-query-and-why.json

step "the right one: ONE variant that is both size 42 and in stock"
reqf GET "/$IDX/_search" requests/03-the-right-one-one-variant-that.json
note "Trail Runner GTX is gone: its 42 is out of stock, and its in-stock sizes are 40 and 44"

step "the listing page: results narrowed by colour, facets that are not"
note "post_filter narrows the hits after the aggregations have counted"
reqf GET "/$IDX/_search" requests/04-the-listing-page-results-narrowed-by.json
note "the colour facet still counts brown, which is what a user needs in order to click it"

step "a facet that IS narrowed, next to one that is not, in the same request"
reqf GET "/$IDX/_search" requests/05-a-facet-that-is-narrowed-next.json

step "price facets over the nested variants, and only the buyable ones"
reqf GET "/$IDX/_search" requests/06-price-facets-over-the-nested-variants.json

step "sort by the price of the cheapest variant that is actually in stock"
reqf GET "/$IDX/_search" requests/07-sort-by-the-price-of-the.json

step "page two, without counting past it"
reqf GET "/$IDX/_search" requests/08-page-two-without-counting-past-it.json

step "how many, without the hits"
req GET "/$IDX/_count" '{ "query": { "term": { "brand": "Quay" } } }'

step "what a client may query, field by field"
req GET "/$IDX/_field_caps?fields=variants.price,colour,rating"

step "what this example leaves behind, checked rather than assumed"
expect_hits 4 GET "/$IDX/_search" '{"query": {"nested": {"path": "variants", "query": {"bool": {"filter": [{"term": {"variants.size": "42"}}, {"range": {"variants.stock": {"gt": 0}}}]}}}}}' "shoes with a size 42 that is actually in stock -- s1 has a 42 and stock, but not in the same variant"
done_
