#!/usr/bin/env bash
# Raw lines in, structured documents out -- and what happens to the ones that
# do not parse.
source "$(dirname "$0")/lib.sh"
IDX=access-log

step "try the pipeline before writing anything with it"
reqf POST "/_ingest/pipeline/_simulate" requests/01-try-the-pipeline-before-writing-anything.json
note "the second document fails -- that is what on_failure is for"

step "the pipeline, properly, with everything a log line needs doing to it"
gone "/_ingest/pipeline/access-log"
gone "/_ingest/pipeline/enrich-client"
gone "/$IDX"; gone "/failed-lines"
reqf PUT "/_ingest/pipeline/enrich-client" requests/02-the-pipeline-properly-with-everything-a.json
reqf PUT "/_ingest/pipeline/access-log" requests/03-the-pipeline-properly-with-everything-a.json

step "the default pipeline, so a writer need not name it"
reqf PUT "/$IDX" requests/04-the-default-pipeline-so-a-writer.json
green "$IDX"

step "raw lines, written without the writer knowing anything about them"
ndjson "/$IDX/_bulk?refresh=wait_for" data/01-raw-lines-written-without-the-writer.ndjson
expect_docs "$IDX" 3 "three of the four lines parsed"
expect_docs failed-lines 1 "and the fourth went to the dead-letter index"

step "what came out"
req GET "/$IDX/_search" '{ "size": 10, "sort": [{ "@timestamp": "asc" }] }'

step "and where the line that would not parse went"
req GET "/failed-lines/_search?ignore_unavailable=true" '{ "size": 5 }'
note "it was not dropped and it did not stop the bulk -- it was put somewhere it can be looked at"

step "what the enrichment is worth: browsers, sections, outcomes"
reqf GET "/$IDX/_search" requests/05-what-the-enrichment-is-worth-browsers.json

step "a grok pattern, tried on its own"
req GET "/_ingest/processor/grok" | clip 400

step "search pipelines: the answer reshaped on its way out"
gone "/_search/pipeline/tidy-results"
reqf PUT "/_search/pipeline/tidy-results" requests/06-search-pipelines-the-answer-reshaped-on.json
req GET "/$IDX/_search?search_pipeline=tidy-results" '{ "size": 2, "_source": ["http.kb", "kilobytes", "url.path"] }'

step "the pipelines that exist"
req GET "/_ingest/pipeline" | clip 300

step "reprocessing what is already written, without re-reading the source"
reqf POST "/$IDX/_update_by_query?refresh=true&conflicts=proceed" requests/07-reprocessing-what-is-already-written-without.json
req GET "/$IDX/_search" '{ "size": 5, "_source": ["outcome", "reprocessed"], "query": { "exists": { "field": "reprocessed" } } }'

step "what this example leaves behind, checked rather than assumed"
done_
