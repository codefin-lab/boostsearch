#!/usr/bin/env bash
# Search pipelines: the shop changes what a search asks and what it answers,
# and the app that sends the search is not touched.
source "$(dirname "$0")/lib.sh"
IDX=books
P=/_search/pipeline

# show -- one line per hit, naming the price field that is actually there, so
# a rename is visible without reading a whole document
show() {
  python3 -c 'import json,sys
d = json.load(sys.stdin)
print("   total %s, returned %d" % (d["hits"]["total"]["value"], len(d["hits"]["hits"])))
for h in d["hits"]["hits"]:
    s = h.get("_source", {})
    price = "".join("%s=%s " % (k, s[k]) for k in ("price", "price_eur") if k in s)
    extra = ""
    if "in_stock" in s: extra += " in_stock=%s" % s["in_stock"]
    if "formats" in s: extra += " formats=%s" % ",".join(s["formats"])
    score = "-" if h.get("_score") is None else "%.3f" % h["_score"]
    print("   %-3s %6s  %-34s %-14s %s%s" % (h["_id"], score, s.get("title", ""), s.get("author", ""), price, extra))'
}

# expect ANSWER PYTHON WHAT -- a check over an answer already received: `d` is
# the whole answer, `h` its hits, `src` their sources
expect() {
  local ok
  ok=$(printf '%s' "$1" | python3 -c 'import json,sys
d = json.load(sys.stdin); h = d.get("hits", {}).get("hits", []); src = [x.get("_source", {}) for x in h]
print("yes" if eval(sys.argv[1]) else "no")' "$2" 2>/dev/null || echo no)
  if [ "$ok" = yes ]; then
    printf '   \033[32mok\033[0m  %s\n' "$3"
  else
    printf '   \033[31mNOT WHAT WAS EXPECTED\033[0m  %s  (%s)\n' "$3" "$2" >&2
    _fails=$((_fails + 1))
  fi
}

# refused STATUS METHOD PATH [FILE] -- a request that is meant to fail, with
# the status it must fail with; the answer is printed, the script carries on
refused() {
  local want=$1 m=$2 p=$3 f=${4-} out code
  out=$(mktemp)
  if [ -n "$f" ]; then
    code=$(curl -sS ${AUTH:+-u "$AUTH"} -o "$out" -w '%{http_code}' -X "$m" "$VS$p" \
      -H 'Content-Type: application/json' --data-binary "@$f")
  else
    code=$(curl -sS ${AUTH:+-u "$AUTH"} -o "$out" -w '%{http_code}' -X "$m" "$VS$p")
  fi
  cat "$out"; echo; rm -f "$out"
  if [ "$code" = "$want" ]; then
    printf '   \033[32mok\033[0m  refused with %s, as it should be\n' "$code"
  else
    printf '   \033[31mNOT WHAT WAS EXPECTED\033[0m  answered %s, expected %s\n' "$code" "$want" >&2
    _fails=$((_fails + 1))
  fi
}

step "a bookshop catalogue: twenty-four books, some of them sold out"
gone "/$IDX"
for name in storefront one-per-author last-year; do gone "$P/$name"; done
reqf PUT "/$IDX" requests/01-a-bookshop-catalogue-an-index.json
green "$IDX"
ndjson "/$IDX/_bulk?refresh=wait_for" data/01-a-bookshop-catalogue-twenty-four-books.ndjson | clip 200
expect_docs "$IDX" 24 "books"

step "what the shop app sends, answered with no pipeline at all"
note "this request body is the app; nothing later in the example changes it"
out=$(reqf GET "/$IDX/_search" requests/02-what-the-app-sends.json)
printf '%s' "$out" | show
expect "$out" 'd["hits"]["total"]["value"] == 12' "twelve books match sea"
expect "$out" 'sum(1 for s in src if not s["in_stock"]) == 4' "four of them cannot be bought"
expect "$out" 'all("price_eur" in s and "price" not in s for s in src)' "the price is under price_eur, which the released app does not read"

step "a pipeline: only what is in stock, and the price under the name the app knows"
reqf PUT "$P/storefront" requests/03-storefront-in-stock-and-price.json
req GET "$P/storefront"

step "the same request, naming the pipeline on the URL"
out=$(reqf GET "/$IDX/_search?search_pipeline=storefront" requests/02-what-the-app-sends.json)
printf '%s' "$out" | show
expect "$out" 'd["hits"]["total"]["value"] == 8' "eight -- the four sold-out books are gone, from the total as well as the page"
expect "$out" 'all(s["in_stock"] for s in src)' "every hit is in stock"
expect "$out" 'all("price" in s and "price_eur" not in s for s in src)' "and every hit carries price, not price_eur"
note "filter_query put the app's query in a bool must and the stock term in its filter,"
note "so the scores are the scores the app's query gave; the filter adds nothing to them"

step "changing a pipeline is writing it again: version 2 caps the page and orders the formats"
reqf PUT "$P/storefront" requests/04-storefront-version-two.json
out=$(req GET "$P/storefront")
echo "$out"
expect "$out" 'd["storefront"]["version"] == 2 and len(d["storefront"]["request_processors"]) == 2' "the stored pipeline is version 2, with two request processors"
out=$(reqf GET "/$IDX/_search?search_pipeline=storefront" requests/02-what-the-app-sends.json)
printf '%s' "$out" | show
expect "$out" 'len(h) == 5 and d["hits"]["total"]["value"] == 8' "the app asked for 20 and got 5; the total still says 8"
expect "$out" 'all(s["formats"] == sorted(s["formats"]) for s in src)' "every hit's formats are in alphabetical order"
note "book 1 is stored with formats paperback,ebook,hardback -- the index was not changed, the answer was"

step "the home page carousel asks for four, and one author fills it"
out=$(reqf GET "/$IDX/_search" requests/05-the-carousel-asks-for-four.json)
printf '%s' "$out" | show
expect "$out" 'len(set(s["author"] for s in src)) < 4' "fewer than four authors in four slots"

step "one book per author: oversample, collapse, truncate"
reqf PUT "$P/one-per-author" requests/06-one-book-per-author.json
out=$(reqf GET "/$IDX/_search?search_pipeline=one-per-author" requests/05-the-carousel-asks-for-four.json)
printf '%s' "$out" | show
expect "$out" 'len(h) == 4 and len(set(s["author"] for s in src)) == 4' "four hits, four different authors"
note "oversample asked the index for 12, collapse kept the best-scoring book of each author,"
note "and truncate_hits cut back to the 4 the request asked for -- it read that number from"
note "what oversample wrote into the request context, not from the pipeline"

step "a default pipeline on the index, so the app need not name it"
reqf PUT "/$IDX/_settings" requests/07-the-default-pipeline-for-books.json
req GET "/$IDX/_settings/index.search.*"
out=$(reqf GET "/$IDX/_search" requests/02-what-the-app-sends.json)
printf '%s' "$out" | show
expect "$out" 'd["hits"]["total"]["value"] == 8 and len(h) == 5 and all("price" in s for s in src)' "no search_pipeline anywhere in the request, and it is filtered, capped and renamed"

step "the back office opts out with search_pipeline=_none"
out=$(reqf GET "/$IDX/_search?search_pipeline=_none" requests/02-what-the-app-sends.json)
printf '%s' "$out" | show
expect "$out" 'd["hits"]["total"]["value"] == 12 and len(h) == 12 and all("price_eur" in s for s in src)' "all twelve, as stored"

step "a pipeline written inline in the body, to try a processor before storing it"
out=$(reqf GET "/$IDX/_search" requests/08-an-inline-pipeline-tried-before.json)
printf '%s' "$out" | show
expect "$out" 'd["hits"]["total"]["value"] == 5' "five fiction books match sea"
expect "$out" 'sum(1 for s in src if not s["in_stock"]) == 2' "two of them sold out -- the inline pipeline replaced the default, it was not added to it"

step "a pipeline written for last year's mapping: one processor fails, the whole search fails"
reqf PUT "$P/last-year" requests/09-a-pipeline-written-for-last-year.json
refused 400 GET "/$IDX/_search?search_pipeline=last-year" requests/02-what-the-app-sends.json
note "the answer names the processor that raised it; no hits come back at all"

step "the same pipeline with ignore_failure on the processor that no longer applies"
reqf PUT "$P/last-year" requests/10-the-same-with-ignore-failure.json
out=$(reqf GET "/$IDX/_search?search_pipeline=last-year" requests/02-what-the-app-sends.json)
printf '%s' "$out" | show
expect "$out" 'd["hits"]["total"]["value"] == 12 and all("price" in s and "sku" not in s for s in src)' "the failing processor was skipped, and the one after it still ran"

step "the pipelines that exist, and deleting one"
req GET "$P" | python3 -c 'import json,sys
for name, p in sorted(json.load(sys.stdin).items()):
    kinds = lambda k: ",".join(next(iter(x)) for x in p.get(k, [])) or "-"
    print("   %-15s request: %-22s response: %s" % (name, kinds("request_processors"), kinds("response_processors")))'
req DELETE "$P/last-year"
refused 404 GET "$P/last-year"
out=$(req GET "$P")
expect "$out" 'sorted(k for k in d if k in ("last-year", "one-per-author", "storefront")) == ["one-per-author", "storefront"]' "two of this example's pipelines left (a shared node may hold others)"

step "what this example leaves behind, checked rather than assumed"
expect_docs "$IDX" 24 "books, unchanged by any of it"
done_
