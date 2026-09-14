# Troubleshooting

## A tokenizer returns `unknown tokenizer [thai]` (or kuromoji, nori, smartcn, icu)

That analysis plugin is not answered for by this node:

```bash
curl -s 'localhost:9269/_cat/plugins?v'
```

The steps for the missing language will fail; the rest still run.

## Thai search matches nothing, and there is no error

The classic silent failure. The field was analysed with `standard`, so the
whole sentence is one token and only an identical whole sentence matches.
Confirm by asking what the field's analyser produces:

```bash
curl -s localhost:9269/multilingual/_analyze -H 'content-type: application/json' \
  -d '{"field":"title.th","text":"ร้านกาแฟเปิดเช้าทุกวัน"}' | jq '.tokens[].token'
```

Several tokens: correct. One long token: the analyser is not the Thai one.

Analysis settings cannot be changed on an existing index -- delete it and
recreate, or reindex (example 14).

## Japanese search misses inflected forms

`kuromoji_baseform` is missing from the chain. Without it, 開きます and 開く are
unrelated tokens. Check:

```bash
curl -s localhost:9269/_analyze -H 'content-type: application/json' \
  -d '{"tokenizer":"kuromoji_tokenizer","filter":["kuromoji_baseform"],"text":"開きます"}'
```

It should produce 開く.

## Step 10 fails for `beider_morse` (or another encoder)

Some encoders need rule files the node loads at start-up. See
`docs/phonetic.md` in the repository; `make serve` points
`BOOSTSEARCH_PHONETIC_RULES` at `/tmp/phonetic-rules`. The step tolerates the
failure and continues.

## Phonetic search matches far too much

That is what phonetic matching does, and why it belongs on a sub-field with the
exact field boosted above it:

```json
{"bool": {"should": [
  {"match": {"name": {"query": "Kathryn Smith", "boost": 3}}},
  {"match": {"name.sounds": "Kathryn Smith"}}]}}
```

If it is still too loose, the encoder is probably wrong for the language --
`soundex` on German names is very lossy; use `koelnerphonetik`.

## `html_strip` did not remove the tags

`html_strip` is a **char filter**, not a token filter. It must be in
`char_filter`, and char filters run before the tokenizer. In `filter` it is
rejected as an unknown token filter.

## `term` query on the normalized keyword returns nothing

The normalizer is applied at index time *and* at query time for a `term` query
on that field -- so `term: "cafe"` finds a document written as `Café`. If it
does not, the field was mapped without the normalizer, or the document was
written before it was added. Check:

```bash
curl -s localhost:9269/tags/_mapping | jq '.tags.mappings.properties.tag'
```

## `icu_folding` changed more than expected

It folds aggressively -- accents, width, case, and some symbol expansions
(½ becomes 1⁄2). If that is too much, `icu_normalizer` alone normalises without
folding.

## Cleaning up

```bash
make clean          # deletes multilingual, people and tags
```
