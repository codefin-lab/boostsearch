#!/usr/bin/env bash
# Finding the clause, and showing the reader where in it the words are.
source "$(dirname "$0")/lib.sh"
IDX=clauses

# hl -- print each hit as id, title and its highlighted fragments, one line
# each, so a highlighted answer can be read without scrolling through JSON
hl() {
  python3 -c 'import json,sys
d = json.load(sys.stdin)
for h in d["hits"]["hits"]:
    src = h.get("_source") or {}
    print("   %-4s %-32s" % (h["_id"], src.get("title", "")))
    for f, frags in (h.get("highlight") or {}).items():
        for fr in frags:
            print("        %s: %s" % (f, fr))'
}

# ids -- the ids of the hits, in order, on one line
ids() {
  python3 -c 'import json,sys; print("   hits:", " ".join(h["_id"] for h in json.load(sys.stdin)["hits"]["hits"]))'
}

# matched -- the ids of the hits in id order: for a positional query the
# question is which clauses match, and the order is left to the score
matched() {
  python3 -c 'import json,sys; print("   matched:", " ".join(sorted((h["_id"] for h in json.load(sys.stdin)["hits"]["hits"]), key=lambda i: int(i[1:]))))'
}

# expect_marked ID FIELD TEXT BODYFILE [what] -- the highlight of one hit must
# contain TEXT. A highlight that comes back empty is not an error to the
# server, so without this a broken highlighter would pass.
expect_marked() {
  local id=$1 field=$2 want=$3 file=$4 what=${5-}
  local got
  got=$("${CURL[@]}" -X GET "$VS/$IDX/_search" -H 'Content-Type: application/json' --data-binary "@$file" 2>/dev/null \
    | python3 -c 'import json,sys
id, field, want = sys.argv[1:4]
try:
    d = json.load(sys.stdin)
    h = [x for x in d["hits"]["hits"] if x["_id"] == id][0]
    print("yes" if any(want in fr for fr in h["highlight"][field]) else "no")
except Exception:
    print("no")' "$id" "$field" "$want")
  if [ "$got" = yes ]; then
    printf '   \033[32mok\033[0m  %s is marked in %s%s\n' "$want" "$id" "${what:+ -- $what}"
  else
    printf '   \033[31mNOT WHAT WAS EXPECTED\033[0m  %s is not marked in %s%s\n' "$want" "$id" "${what:+ -- $what}" >&2
    _fails=$((_fails + 1))
  fi
  return 0
}

step "an index that keeps positions and offsets for the clause text"
gone "/$IDX"
reqf PUT "/$IDX" requests/01-an-index-that-keeps-positions-and.json
green "$IDX"

step "twelve clauses from three contracts"
ndjson "/$IDX/_bulk?refresh=wait_for" data/01-twelve-clauses-from-three-contracts.ndjson | clip 300
expect_docs "$IDX" 12 "clauses"

step "the clause, and the words in it that found it"
reqf GET "/$IDX/_search" requests/02-the-clause-and-the-words-that.json | hl
note "c10 is a hit on 'written' alone, and only that word is marked"
expect_hits 3 GET "/$IDX/_search" '{"query": {"match": {"body": "written notice"}}}' "two notice clauses and the consent clause"
expect_marked c4 body "<em>written</em> <em>notice</em>" requests/02-the-clause-and-the-words-that.json

step "the same hit through each of the three highlighters, with the tags a web page wants"
for t in unified plain fvh; do
  note "--- $t"
  req GET "/$IDX/_search" "$(sed "s/\"plain\"/\"$t\"/" requests/03-the-same-hit-through-each-highlighter.json)" | hl
done
note "fvh reads the offsets stored by term_vector; plain and unified analyse the text again"
expect_marked c1 body "<mark>consequential</mark> <mark>damages</mark>" requests/03-the-same-hit-through-each-highlighter.json

step "found by what the clause is, marked by the words a reviewer is looking for"
reqf GET "/$IDX/_search" requests/04-find-by-type-mark-by-words.json | hl
note "the query is a keyword filter with no words in it; highlight_query supplies the words"
expect_marked c9 body "<mark>negligence</mark>" requests/04-find-by-type-mark-by-words.json "a word the query never asked for"

step "a stemmed match, marked on the unstemmed text"
reqf GET "/$IDX/_search" requests/05-a-stemmed-match-marked-on-the.json | hl
note "'terminates' and 'notices' are nowhere in the text; the english sub-field matched their stems"
expect_marked c5 body "<em>termination</em>" requests/05-a-stemmed-match-marked-on-the.json "found through body.english, marked in body"

step "a phrase, with room for words in between"
note "--- slop 0: the words exactly as typed"
req GET "/$IDX/_search" '{"_source": ["title"], "query": {"match_phrase": {"body": {"query": "notify in writing"}}}}' | ids
expect_hits 0 GET "/$IDX/_search" '{"query": {"match_phrase": {"body": {"query": "notify in writing"}}}}' "nobody wrote it that way"
note "--- slop 2: 'notify the supplier in writing' is two moves away"
reqf GET "/$IDX/_search" requests/06-a-phrase-with-room-for-words.json | hl
expect_hits 1 GET "/$IDX/_search" "$(cat requests/06-a-phrase-with-room-for-words.json)" "the indemnity clause"

step "terminate, then notice, within so many positions"
note "--- slop 5"
reqf GET "/$IDX/_search" requests/07-terminate-before-notice-within-n-positions.json | matched
expect_hits 1 GET "/$IDX/_search" "$(cat requests/07-terminate-before-notice-within-n-positions.json)" "for cause: five words between"
note "--- slop 8"
req GET "/$IDX/_search" "$(sed 's/"slop": 5/"slop": 8/' requests/07-terminate-before-notice-within-n-positions.json)" | matched
expect_hits 2 GET "/$IDX/_search" "$(sed 's/"slop": 5/"slop": 8/' requests/07-terminate-before-notice-within-n-positions.json)" "for convenience too: eight words between"

step "clauses that open with the supplier, not merely mention it"
reqf GET "/$IDX/_search" requests/08-clauses-that-open-with-the-supplier.json | matched
expect_hits 3 GET "/$IDX/_search" "$(cat requests/08-clauses-that-open-with-the-supplier.json)" "supplier in position 1"
expect_hits 5 GET "/$IDX/_search" '{"query": {"term": {"body": "supplier"}}}' "anywhere, for comparison"

step "a prefix inside a positional query: 'shall', then any word that begins notif"
reqf GET "/$IDX/_search" requests/09-a-prefix-inside-a-positional-query.json | matched
expect_hits 2 GET "/$IDX/_search" "$(cat requests/09-a-prefix-inside-a-positional-query.json)" "force majeure and the data breach clause"

step "intervals: notify, then 'without undue delay', at most four words apart"
reqf GET "/$IDX/_search" requests/10-notify-then-without-undue-delay-close.json | matched
expect_hits 1 GET "/$IDX/_search" "$(cat requests/10-notify-then-without-undue-delay-close.json)" "three words between, in force majeure"
note "--- max_gaps 8 lets in the data breach clause, where six words stand between"
req GET "/$IDX/_search" "$(sed 's/"max_gaps": 4/"max_gaps": 8/' requests/10-notify-then-without-undue-delay-close.json)" | matched
expect_hits 2 GET "/$IDX/_search" "$(sed 's/"max_gaps": 4/"max_gaps": 8/' requests/10-notify-then-without-undue-delay-close.json)" "both notification duties"

step "intervals: a way out of the contract, followed by what triggers it"
reqf GET "/$IDX/_search" requests/11-a-way-out-followed-by-its.json | matched
expect_hits 3 GET "/$IDX/_search" "$(cat requests/11-a-way-out-followed-by-its.json)" "two terminations and a suspension"

step "intervals: liability ... limited, but not 'liability is not limited'"
note "--- without the filter"
req GET "/$IDX/_search" '{"_source": ["title"], "query": {"intervals": {"body": {"match": {"query": "liability limited", "ordered": true}}}}}' | matched
expect_hits 2 GET "/$IDX/_search" '{"query": {"intervals": {"body": {"match": {"query": "liability limited", "ordered": true}}}}}' "the cap and the warranty"
note "--- with not_containing 'not'"
reqf GET "/$IDX/_search" requests/12-liability-limited-but-not-not-limited.json | matched
expect_hits 1 GET "/$IDX/_search" "$(cat requests/12-liability-limited-but-not-not-limited.json)" "only the cap"

step "clauses like this one: what else reads like the limitation of liability"
reqf GET "/$IDX/_search" requests/13-clauses-like-this-one.json
expect_hits 11 GET "/$IDX/_search" "$(cat requests/13-clauses-like-this-one.json)" "every clause shares a word; the order is what counts"
note "boilerplate ('either party', 'this agreement', 'fees paid') puts c3 above the other liability cap"

step "title and body scored as one field"
reqf GET "/$IDX/_search" requests/14-title-and-body-scored-as-one.json | ids
expect_hits 3 GET "/$IDX/_search" "$(cat requests/14-title-and-body-scored-as-one.json)" "both terminations, and confidentiality's 'survives termination'"

step "why c3 scored what it scored for 'written notice'"
req GET "/$IDX/_explain/c3" '{ "query": { "match": { "body": "written notice" } } }'

step "what the index holds for c3: every term, where it stands, and where it is in the text"
req GET "/$IDX/_termvectors/c3?fields=body&positions=true&offsets=true&field_statistics=false" \
  | python3 -c 'import json,sys
d = json.load(sys.stdin)
print("   found: %s, %d distinct terms in body" % (d["found"], len(d["term_vectors"]["body"]["terms"])))
rows = [(tok["position"], term, tok["start_offset"], tok["end_offset"])
        for term, v in d["term_vectors"]["body"]["terms"].items() for tok in v["tokens"]]
for pos, term, start, end in sorted(rows):
    print("   %3d  %-12s %3d-%d" % (pos, term, start, end))'
note "the answer is keyed by term; sorted by position here it reads as the clause again"

step "what this example leaves behind, checked rather than assumed"
expect_docs "$IDX" 12 "the clauses are still there"
done_
