# The phonetic filter, and the rules Beider-Morse reads

`type: phonetic` writes a word the way it sounds, so that a search for
`helllo` finds `hello`. The encoders are the Apache commons-codec ones, which
is what OpenSearch's phonetic plugin uses, so a word encoded here is the word
encoded there:

`metaphone`, `double_metaphone` (with `max_code_len`, four by default),
`soundex`, `refined_soundex`, `caverphone1`, `caverphone2`, `cologne`,
`nysiis`, `daitch_mokotoff`, `beider_morse`.

`replace` says what happens to the word a code was made from: `true`, the
default, drops it; `false` keeps it beside the code, in the same position, so
that a search for either finds the document.

## Beider-Morse, and the language rules

Beider-Morse is not one algorithm but a set of rule files, one per language
and tradition -- generic, Ashkenazi, Sephardic, each in an approximate and an
exact form. The library carries the language-agnostic ones, which is what a
filter that names no language uses:

```json
{ "type": "phonetic", "encoder": "beider_morse" }
```

Naming a language needs the per-language files commons-codec ships
(`ash_rules_polish.txt` and its fifty siblings). They are Apache-2.0, so
nothing stops them being carried here; they are simply not vendored yet.
Until they are, a filter that names a language is looked for on disk:

1. `$BOOSTSEARCH_PHONETIC_RULES`
2. `$BOOSTSEARCH_CONFIG/analysis-phonetic/`
3. `$BOOSTSEARCH_DATA/config/analysis-phonetic/`
4. `./config/analysis-phonetic/`

Point one of those at a directory holding commons-codec's `bm` rule files and
`languageset` works. Without them, a word the rules cannot be found for is
left as it is rather than encoded -- the filter does not fail, and neither
does anything else.

**With those files in place the whole phonetic suite passes**, including
`30_beider_morse.yml` and its `languageset: polish` -- checked against
commons-codec 1.18.0's own rule files, pointed at from outside this
repository. What is missing is the data, not the code, and whether a release
carries a hundred and twenty-seven files of somebody else's data -- even under
a licence that allows it -- is a decision to make rather than something to
slip into a commit.

## A note on why the encoders are guarded

These encoders read rule sets as data, and a combination no rule was written
for makes the library give up where it stands rather than return an error. A
token is not worth a node, so the call is guarded: a word that cannot be
encoded is a word left alone.
