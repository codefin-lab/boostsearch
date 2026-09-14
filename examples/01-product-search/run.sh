#!/usr/bin/env bash
# A product search that has to be good, not merely correct.
source "$(dirname "$0")/lib.sh"
IDX=shop-products

step "an index whose analysis chain knows the shop's vocabulary"
gone "/$IDX"
gone "/_scripts/product-search"
reqf PUT "/$IDX" requests/01-an-index-whose-analysis-chain-knows.json
green "$IDX"

step "the catalogue"
ndjson "/$IDX/_bulk?refresh=wait_for" data/01-the-catalogue.ndjson
expect_docs "$IDX" 6 "the catalogue"

step "the synonym does its work: 'notebook' finds what is called a laptop, and the other way round"
reqf GET "/$IDX/_search" requests/02-the-synonym-does-its-work-notebook.json

step "what the analyser actually did to the text"
req POST "/$IDX/_analyze" '{ "analyzer": "shop_text", "text": "Soundbar & Sub for a Notebook" }'

step "the search a search box makes: several fields, weighted, with a typo allowed"
reqf GET "/$IDX/_search" requests/03-the-search-a-search-box-makes.json

step "relevance shaped by the business: popular, well rated and in stock beat merely matching"
reqf GET "/$IDX/_search" requests/04-relevance-shaped-by-the-business-popular.json

step "why that document scored what it scored"
req GET "/$IDX/_explain/p3" '{ "query": { "match": { "blurb": "noise cancelling" } } }'

step "one row per brand, rather than four rows of the same brand"
reqf GET "/$IDX/_search" requests/05-one-row-per-brand-rather-than.json

step "as-you-type, from the completion field"
reqf GET "/$IDX/_search" requests/06-as-you-type-from-the-completion.json

step "did you mean: a term the index does hold"
reqf GET "/$IDX/_search" requests/07-did-you-mean-a-term-the.json

step "the same search, stored once and called by name"
quietf PUT "/_scripts/product-search" requests/08-the-same-search-stored-once-and.json
req GET "/$IDX/_search/template" '{ "id": "product-search", "params": { "q": "notebook", "max_price": 1000 } }'

step "what the template renders to, without running it"
req POST "/_render/template" '{ "id": "product-search", "params": { "q": "tv", "max_price": 700 } }'

step "four searches in one round trip, the way a page with four widgets asks"
ndjson "/$IDX/_msearch" data/02-four-searches-in-one-round-trip.ndjson

step "where the time went"
reqf GET "/$IDX/_search?profile=true" requests/09-where-the-time-went.json

note "done -- the index $IDX is left in place so you can poke at it"

step "what this example leaves behind, checked rather than assumed"
expect_hits 2 GET "/$IDX/_search" '{"query": {"match": {"blurb": "laptop"}}}' "the synonym still finds both notebooks"
done_
