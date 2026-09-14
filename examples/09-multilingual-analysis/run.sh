#!/usr/bin/env bash
# Text that is not English: Thai, Japanese, Korean, Chinese, and names that
# are spelled several ways.
source "$(dirname "$0")/lib.sh"

step "what the plugins say they can do"
req GET "/_cat/plugins?v" | clipl 20

step "Thai: no spaces between words, so the tokenizer has to know the language"
req POST "/_analyze" '{ "tokenizer": "thai", "text": "ร้านกาแฟเปิดเช้าทุกวัน" }'
note "compare with standard, which cannot find the word boundaries"
req POST "/_analyze" '{ "tokenizer": "standard", "text": "ร้านกาแฟเปิดเช้าทุกวัน" }'

step "Japanese: kuromoji, with the reading and the part of speech"
req POST "/_analyze" '{ "tokenizer": "kuromoji_tokenizer", "text": "東京都に住んでいます", "explain": true, "attributes": ["baseForm", "reading", "partOfSpeech"] }'

step "Korean: nori, which splits compounds"
req POST "/_analyze" '{ "tokenizer": "nori_tokenizer", "text": "한국어를 분석합니다" }'

step "Chinese: smartcn"
req POST "/_analyze" '{ "analyzer": "smartcn", "text": "北京市的天气很好" }'

step "ICU: folding, normalisation and script-aware breaking, all at once"
reqf POST "/_analyze" requests/01-icu-folding-normalisation-and-script-aware.json
note "full-width letters normalised, accents folded, the vulgar fraction expanded"

step "an index that handles four languages at once, a field per language"
IDX=multilingual
gone "/$IDX"
reqf PUT "/$IDX" requests/02-an-index-that-handles-four-languages.json
green "$IDX"

ndjson "/$IDX/_bulk?refresh=wait_for" data/01-an-index-that-handles-four-languages.ndjson
expect_docs "$IDX" 5 "one document per language, plus English"

step "searching each language in its own field"
for q in 'กาแฟ|title.th' '喫茶店|title.ja' '카페|title.ko' '咖啡|title.zh'; do
  term=${q%%|*}; field=${q##*|}
  note "--- $field : $term"
  req GET "/$IDX/_search" "{ \"size\": 3, \"_source\": [\"lang\"], \"query\": { \"match\": { \"$field\": \"$term\" } },
                             \"highlight\": { \"fields\": { \"$field\": {} } } }"
done
note "the HTML in the Thai document never reached the index: html_strip took it out"

step "names that are spelled several ways -- phonetic matching"
IDX2=people
gone "/$IDX2"
reqf PUT "/$IDX2" requests/03-names-that-are-spelled-several-ways.json
green "$IDX2"
ndjson "/$IDX2/_bulk?refresh=wait_for" data/02-names-that-are-spelled-several-ways.ndjson
expect_docs "$IDX2" 4 "four spellings"
req GET "/$IDX2/_search" '{ "size": 5, "query": { "match": { "name.sounds": "Kathrine Smithe" } } }'
note "three spellings of the same-sounding name, none of them an exact match"

step "the encoders, side by side on one name"
for enc in metaphone double_metaphone soundex refined_soundex caverphone2 koelnerphonetik beider_morse; do
  note "--- $enc"
  req POST "/_analyze" "{ \"tokenizer\": \"standard\",
     \"filter\": [{ \"type\": \"phonetic\", \"encoder\": \"$enc\", \"replace\": true }],
     \"text\": \"Schmidt\" }" || true
done

step "a character filter that rewrites before anything is tokenised"
reqf POST "/_analyze" requests/04-a-character-filter-that-rewrites-before.json

step "a normalizer: a keyword field that is still case-insensitive"
IDX3=tags
gone "/$IDX3"
reqf PUT "/$IDX3" requests/05-a-normalizer-a-keyword-field-that.json
quiet POST "/$IDX3/_doc?refresh=true" '{ "tag": "Café" }'
req GET "/$IDX3/_search" '{ "query": { "term": { "tag": "cafe" } } }'
note "an exact term match on text that was neither lower case nor ascii when it was written"

step "what this example leaves behind, checked rather than assumed"
done_
