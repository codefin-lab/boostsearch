#!/usr/bin/env bash
# Data streams: metrics that arrive forever, written to one name, kept in a
# row of indices the name rolls through, every one of them shaped by templates.
source "$(dirname "$0")/lib.sh"
DS=metrics-node-cpu
EDGE=metrics-edge-cpu

# expect_status CODE PATH [what] -- a GET that must answer with this status;
# the checks in lib.sh count documents, and a deleted index has none to count
expect_status() {
  local got
  got=$(curl -s -o /dev/null -w '%{http_code}' ${AUTH:+-u "$AUTH"} "$VS$2")
  if [ "$got" = "$1" ]; then
    printf '   \033[32mok\033[0m  GET %s answers %s%s\n' "$2" "$got" "${3:+ -- $3}"
  else
    printf '   \033[31mNOT WHAT WAS EXPECTED\033[0m  GET %s answers %s, expected %s%s\n' "$2" "$got" "$1" "${3:+ -- $3}" >&2
    _fails=$((_fails + 1))
  fi
  return 0
}

# expect_field PATH POINTER VALUE [what] -- one value in a JSON answer, found by
# a slash-separated path, must be exactly this
expect_field() {
  local got
  got=$("${CURL[@]}" "$VS$1" 2>/dev/null | python3 -c 'import json,sys
d = json.load(sys.stdin)
try:
    for k in sys.argv[1].strip("/").split("/"): d = d[int(k)] if isinstance(d, list) else d[k]
    print(d if isinstance(d, str) else json.dumps(d))
except Exception: print("<missing>")' "$2")
  if [ "$got" = "$3" ]; then
    printf '   \033[32mok\033[0m  %s is %s%s\n' "$2" "$got" "${4:+ -- $4}"
  else
    printf '   \033[31mNOT WHAT WAS EXPECTED\033[0m  %s is %s, expected %s%s\n' "$2" "$got" "$3" "${4:+ -- $4}" >&2
    _fails=$((_fails + 1))
  fi
  return 0
}

step "a clean start: streams first, then the templates they were made from"
# a template a stream was made from cannot be deleted while the stream is there
gone "/_data_stream/$DS"; gone "/_data_stream/$EDGE"
gone "/_index_template/metrics-edge"; gone "/_index_template/metrics"; gone "/_index_template/metrics-rival"
gone "/_component_template/metrics-settings"; gone "/_component_template/metrics-mappings"
gone "/legacy-metrics-000001"; gone "/legacy-metrics-000002"; gone "/_template/legacy-metrics"
note "nothing to see; this is what makes the example safe to run twice"

step "a component template for the settings -- how big, how often refreshed"
reqf PUT "/_component_template/metrics-settings" requests/01-a-component-template-for-the-settings.json

step "a component template for the mappings -- the fields every sample carries"
reqf PUT "/_component_template/metrics-mappings" requests/02-a-component-template-for-the-mappings.json
req GET "/_component_template/metrics-*"
note "neither of these matches anything on its own: a component has no index_patterns"

step "the index template: composed of both, a data stream, priority 100"
reqf PUT "/_index_template/metrics" requests/03-the-index-template-composed-of-both.json
req GET "/_index_template/metrics"
note "data_stream: {} came back with timestamp_field filled in -- @timestamp unless you say otherwise"

step "what an index called $DS would be made from, asked before it exists"
req POST "/_index_template/_simulate_index/$DS"
note "the two components and the template's own setting, merged into one answer"

step "a narrower template for edge boxes, at priority 200"
reqf PUT "/_index_template/metrics-edge" requests/04-a-narrower-template-that-outranks-it.json
req POST "/_index_template/_simulate_index/$EDGE"
note "both templates match $EDGE; 'overlapping' names the other template whose patterns meet these."
note "priority 200 wins outright -- templates are not layered, so its 30s refresh"
note "and its extra field 'site' are there, and nothing of 'metrics' is merged in"
req POST "/_index_template/_simulate_index/$DS"
note "and $DS still gets 'metrics': 'metrics-edge-*' does not match it at all"

step "a second template at the same priority and the same pattern is refused"
reqf PUT "/_index_template/metrics-rival" requests/05-a-rival-at-the-same-priority.json || true
note "two templates that could both make an index, with nothing to choose between them,"
note "are refused when they are written -- not discovered later when an index is made"

step "a morning of samples, written to a name that does not exist yet"
note "four hosts, every ten minutes from 06:00 to 08:50: 72 samples"
note "no stream called $DS exists: the first write makes it, because 'metrics' matches the name"
note "and says data_stream. A data stream only appends, so every bulk action is 'create'"
ndjson "/$DS/_bulk?refresh=true" data/01-a-morning-of-samples-from-four.ndjson | clip 420
expect_docs "$DS" 72 "read through the stream's name"
expect_docs ".ds-$DS-000001" 72 "all of them in generation 1"
req GET "/_data_stream/$DS"
note "one backing index, generation 1, and the template it was made from"
req GET "/_cat/indices/.ds-$DS-*?v&h=index,docs.count&s=index"

step "the backing index was born from the template, not from the documents"
req GET "/.ds-$DS-000001/_mapping"
expect_field "/.ds-$DS-000001/_mapping" ".ds-$DS-000001/mappings/properties/host/type" keyword \
  "from metrics-mappings; left to dynamic mapping it would have been text"
expect_field "/.ds-$DS-000001/_mapping" ".ds-$DS-000001/mappings/_data_stream_timestamp/enabled" true \
  "added because the template says data_stream"
req GET "/.ds-$DS-000001/_settings?filter_path=**.number_of_shards,**.number_of_replicas,**.refresh_interval,**.codec"
expect_field "/.ds-$DS-000001/_settings" ".ds-$DS-000001/settings/index/number_of_replicas" 0 \
  "from metrics-settings"
expect_field "/.ds-$DS-000001/_settings" ".ds-$DS-000001/settings/index/codec" best_compression \
  "from the index template's own block"
note "the same answer step 5 simulated before there was anything to look at"

step "a sample without @timestamp is refused"
req POST "/$DS/_doc" '{ "host": "web01", "cpu_pct": 40.0 }' || true
printf '%s\n' '{ "create": {} }' '{ "@timestamp": "2026-09-14T08:55:00Z", "host": "web01", "cpu_pct": 40.0 }' \
               '{ "create": {} }' '{ "host": "web02", "cpu_pct": 41.0 }' \
  | bulk "/$DS/_bulk?refresh=true&filter_path=errors,items.*.status,items.*.error.caused_by.reason"
expect_docs "$DS" 73 "in a bulk only the item without a time is refused; the other is written"
note "a stream orders and rolls by time, so a document with no time has nowhere to go"

step "the stream, as _resolve/index sees the name"
req GET "/_resolve/index/metrics-*"
note "metrics-* resolves to a data stream, with its backing index and time field --"
note "not to an index, and not to an alias"

step "what the stream holds, in numbers"
req GET "/_data_stream/$DS/_stats?human=true"
note "maximum_timestamp is the newest @timestamp in any backing index, in milliseconds"

step "writing a document by id is refused: a stream is append-only"
req PUT "/$DS/_doc/sample-1" '{ "@timestamp": "2026-09-14T08:55:00Z", "host": "web01", "cpu_pct": 40.0 }' || true
note "an update or an overwrite would need to know which generation holds the id;"
note "a stream does not promise to know that, so it takes only op_type create"

step "a rollover with a condition that does not hold changes nothing"
reqf POST "/$DS/_rollover" requests/06-roll-over-only-past-a-million.json
note "rolled_over is false and the stream is still on generation 1"

step "a manual rollover: a new generation, and the stream's name moves to it"
req POST "/$DS/_rollover"
req GET "/_data_stream/$DS"
note "no alias to bootstrap, no -000001 index to create first: the stream names its own indices"

step "the next hour, written to the same name -- it lands in generation 2"
ndjson "/$DS/_bulk?refresh=true" data/02-the-next-hour-after-the-rollover.ndjson | clip 420
expect_docs ".ds-$DS-000001" 73 "generation 1 took no more writes"
expect_docs ".ds-$DS-000002" 24 "the new hour, all in generation 2"
expect_docs "$DS" 97 "and the stream's name reads both"

step "the write index cannot be deleted out from under the stream"
req DELETE "/.ds-$DS-000002" || true
note "the newest backing index is the stream's write index; it goes when the stream goes"

step "searching across generations: cpu by the hour, by host, and which generation answered"
reqf GET "/$DS/_search" requests/07-cpu-by-the-hour-across-both.json
note "one query, two indices. 06:00-08:00 come from generation 1, 09:00 from generation 2;"
note "the 09:00 maximum is db01, which had a bad hour just after the rollover"
note "by_host names it: a terms aggregation on host works because the template made host a keyword"

step "one host, over a window that straddles the rollover"
reqf GET "/$DS/_search" requests/08-one-host-over-a-window-that.json
expect_hits 6 GET "/$DS/_search" "$(cat requests/08-one-host-over-a-window-that.json)" \
  "db01 from 08:30 to 09:20: three samples in each generation"
note "the reader never says which generation; the stream's name is enough"

step "a stream made ahead of its first write, on the edge pattern"
req PUT "/_data_stream/$EDGE"
note "an explicit PUT makes the stream empty: the template is checked, and the backing index"
note "and its mapping exist, before any edge box has sent a sample"
req GET "/.ds-$EDGE-000001/_settings?filter_path=**.refresh_interval"
expect_field "/.ds-$EDGE-000001/_mapping" ".ds-$EDGE-000001/mappings/properties/site/type" keyword \
  "the field only metrics-edge has: the priority 200 template made this index"
req GET "/_data_stream/metrics-*"
note "'template' says which one each stream was made from: metrics and metrics-edge"
req DELETE "/_index_template/metrics-edge" || true
note "and a template a stream was made from cannot be deleted while the stream is there"

step "the older way, for contrast: a legacy template, an alias, a numbered index"
reqf PUT "/_template/legacy-metrics" requests/09-the-older-way-a-legacy-template.json
reqf PUT "/legacy-metrics-000001" requests/10-the-first-index-bootstrapped-by-hand.json
ndjson "/legacy-metrics/_bulk?refresh=true" data/03-the-same-samples-the-older-way.ndjson | clip 200
expect_docs legacy-metrics 4 "written through the alias"
req POST "/legacy-metrics/_rollover"
req GET "/_cat/aliases/legacy-metrics?v&h=alias,index,is_write_index&s=index"
note "the same outcome, but three things had to be made by hand and kept in step:"
note "the template, the first index with its -000001 name, and the alias marked as the write index."
note "with a data stream the template is the whole of the set-up."

step "deleting the data stream takes every generation with it"
req DELETE "/_data_stream/$DS"
req GET "/_data_stream/$DS" || true
expect_status 404 "/.ds-$DS-000001" "generation 1 is gone"
expect_status 404 "/.ds-$DS-000002" "generation 2 is gone"
req GET "/_data_stream"
note "the edge stream, made from the other template, is untouched"

step "what this example leaves behind, checked rather than assumed"
req GET "/_cat/indices/.ds-$EDGE-*,legacy-metrics-*?v&h=index,docs.count&s=index&expand_wildcards=all"
req GET "/_cat/templates/metrics*?v&h=name,index_patterns,order,composed_of&s=name"
done_
