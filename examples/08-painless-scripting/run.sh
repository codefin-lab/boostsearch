#!/usr/bin/env bash
# Painless in every place it is allowed to run.
source "$(dirname "$0")/lib.sh"
IDX=fleet

step "a fleet of vehicles"
gone "/$IDX"
gone "/_scripts/depreciated-value"
gone "/_scripts/needs-service"
reqf PUT "/$IDX" requests/01-a-fleet-of-vehiclesx.json
green "$IDX"

ndjson "/$IDX/_bulk?refresh=wait_for" data/01-a-fleet-of-vehicles.ndjson
expect_docs "$IDX" 5 "vehicles"

step "a field that is computed, not stored"
reqf GET "/$IDX/_search" requests/02-a-field-that-is-computed-not.json

step "a script that decides what matches, not what is shown"
reqf GET "/$IDX/_search" requests/03-a-script-that-decides-what-matches.json

step "a script that decides the order"
reqf GET "/$IDX/_search" requests/04-a-script-that-decides-the-order.json

step "a script that decides the score"
reqf GET "/$IDX/_search" requests/05-a-script-that-decides-the-score.json

step "a script that buckets -- and one that computes across buckets"
reqf GET "/$IDX/_search" requests/06-a-script-that-buckets-and-one.json

step "scripted_metric: an answer no built-in aggregation gives"
note "the total fuel each driver would burn over their vehicles' remaining warranty km"
reqf GET "/$IDX/_search" requests/07-scripted-metric-an-answer-no-built.json

step "a script that writes: update one document without reading it first"
reqf POST "/$IDX/_update/c1" requests/08-a-script-that-writes-update-one.json
req GET "/$IDX/_doc/c1?_source_includes=plate,km,trips"

step "upsert: write it if it is not there, change it if it is"
reqf POST "/$IDX/_update/c9" requests/09-upsert-write-it-if-it-is.json

step "a script over every matching document at once"
reqf POST "/$IDX/_update_by_query?refresh=true&conflicts=proceed" requests/10-a-script-over-every-matching-document.json
req GET "/$IDX/_search" '{ "size": 5, "_source": ["plate", "model", "faults"], "query": { "term": { "model": "D-Max" } } }'

step "stored scripts, so the body is not sent every time"
quietf PUT "/_scripts/depreciated-value" requests/11-stored-scripts-so-the-body-is.json
reqf GET "/$IDX/_search" requests/12-stored-scripts-so-the-body-is.json

step "Lucene expressions, for the arithmetic that does not need a language"
note "an expression reads doc values through doc[...], and knows nothing else"
reqf GET "/$IDX/_search" requests/13-lucene-expressions-for-the-arithmetic-that.json

step "what the engine will run, and where"
req GET "/_script_language"
req GET "/_script_context" | clip 400

step "a script that will not compile, and what it says"
req POST "/$IDX/_search" '{ "script_fields": { "x": { "script": { "source": "this is not painless" } } } }' || true

step "what this example leaves behind, checked rather than assumed"
expect_docs "$IDX" 6 "five vehicles, plus the one the upsert created"
done_
