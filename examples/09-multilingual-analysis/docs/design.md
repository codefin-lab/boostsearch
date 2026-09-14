# Design notes

## Why `_analyze` comes before any mapping

Half of this example is `_analyze` calls with no index involved. That is the
order the work should be done in: a mapping decision is a commitment (analysis
settings are fixed at index creation), and `_analyze` is free.

The specific failure it prevents is silent. Index Thai with the `standard`
tokenizer and nothing errors: documents are written, searches run, and they
match nothing, because the entire sentence became one token. There is no
warning, no exception, and no way to notice except by looking at the tokens --
which is step 2.

## One field with four analysers, rather than four fields

```json
"title": { "type": "text",
  "fields": { "th": {...}, "ja": {...}, "ko": {...}, "zh": {...} } }
```

The alternative -- `title_th`, `title_ja` and so on -- requires the *writer* to
know the language, and to put the text in the right field. Sub-fields mean the
document is written once into `title` and the engine produces four token
streams; which one you query decides which language's rules apply.

The cost is index size: four analysers means four postings lists for the same
text. For short fields like titles that is cheap. For document bodies it is
not, and then language detection at ingest and a field per language is the
right trade.

A `multi_match` across all four sub-fields is the usual query, and it works
without knowing the language of either the document or the query.

## What each language's chain is doing

| Analyser | Tokenizer | Then | Because |
|---|---|---|---|
| `th` | `thai` | lowercase, `decimal_digit`, stop words | Thai has no spaces; Thai digits need folding to Arabic |
| `ja` | `kuromoji_tokenizer` | `kuromoji_baseform`, `ja_stop`, `kuromoji_stemmer` | inflected verbs must reduce to dictionary form |
| `ko` | `nori_tokenizer` | `nori_readingform` | Korean compounds split; hanja folded to hangul |
| `zh` | `icu_tokenizer` | `cjk_bigram` | overlapping two-character grams, no dictionary needed |

The Japanese chain is the one worth reading closely. `kuromoji_baseform` is why
開きます (polite, present) is found by 開く (dictionary form): without it they
are unrelated strings.

The Chinese chain deliberately uses bigrams rather than `smartcn`'s dictionary.
Bigrams over-generate -- 北京市 produces 北京, 京市 -- which costs precision and
never misses a word the dictionary does not know. For a corpus with proper
nouns and product names that is usually the better trade; step 5 shows the
dictionary alternative.

## Phonetic matching belongs on a sub-field

```json
"name": { "type": "text",
  "fields": { "sounds": { "analyzer": "sounds_like" },
              "de": { "analyzer": "sounds_like_de" } } }
```

Phonetic filters index how a word sounds, so `Catherine`, `Kathryn` and
`Katharina` collide on purpose. The cost is false positives -- plenty of
unrelated names sound alike -- which is why it is never the only field. The
query is a `should` across the exact field and the phonetic one, with the exact
match boosted, so a correct spelling still wins.

`replace: false` in the filter keeps the original token alongside the code, so
one field does both jobs. `replace: true` (step 10) throws the original away
and is what you want when comparing encoders, not when searching.

The encoder matters more than people expect: `soundex` is English and crude,
`koelnerphonetik` is built for German, `beider_morse` for Slavic and Jewish
surnames. Using the wrong one is worse than using none.

## A `normalizer` is not an `analyzer`

Step 12 is small and catches everyone once. A `keyword` field matches exact
bytes, so `Café` and `cafe` are different terms and a `term` query for one
never finds the other. A `normalizer` applies a filter chain -- lowercase,
asciifolding -- *without tokenising*, so the field stays a single term and
becomes case- and accent-insensitive.

You cannot use an `analyzer` here: it would split the value into tokens and
the field would stop being a keyword.

## What would change at scale

- **Dictionary-based tokenizers hold their dictionary in memory** per node --
  kuromoji and nori both. One copy each, not per index, but it is real memory.
- **Analysis settings are fixed at index creation.** Adding a language means
  reindexing (example 14), so it is worth adding sub-fields for languages you
  expect before you need them: an unused sub-field on text that never contains
  that language costs almost nothing.
- **`cjk_bigram` roughly doubles the token count** for CJK text. Budget for it.
