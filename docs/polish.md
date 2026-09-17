# Polish, and the table Stempel reads

`analyzer: polish` and `type: polish_stem` are what OpenSearch's
analysis-stempel plugin installs. Polish is not stemmed by an algorithm
written out in code the way the Snowball languages are: Stempel is a trie of
patch commands, learned from a dictionary of words and their stems, and
shipped as a table. `src/analysis/stempel.rs` is the reader for that table and
the interpreter for the commands in it -- the Egothor `MultiTrie2` and `Diff`,
which is what Lucene runs.

The analyzer is the standard tokenizer, lowercased, with Polish stop words
dropped and what is left put through the table. The filter on its own is the
table and nothing else, and leaves a word of fewer than three characters
alone: the table was learned from whole words, and what it says about two
characters is noise.

A word the table has nothing to say about is the word itself. So is a word
whose commands would reach outside it, which is how Lucene's `Diff` behaves
as well -- it catches the exception rather than checking first.

## The table is in this repository

`src/analysis/stemmer_20000.tbl`, two and a bit megabytes, taken from Apache
Lucene's `lucene-analysis-stempel` jar (10.2.1), where it lives at
`org/apache/lucene/analysis/pl/stemmer_20000.tbl`. It was built by the Egothor
project and is BSD-licensed; the licence is carried in `LICENSE-STEMPEL`
beside `LICENSE-SNOWBALL`, which is the same arrangement the Snowball
stemmers already have here.

It is vendored rather than read from a fixtures directory because it is the
whole of the implementation. Without it `polish_stem` is not a stemmer that
finds nothing, it is not a stemmer at all, and an analyzer that silently
stopped stemming would be worse than one that was never there. Two megabytes
is also a size a source tree can carry, which the seven megabytes of the
Ukrainian dictionary is not -- see [ukrainian.md](ukrainian.md) for the other
half of that decision.

The stop word list is vendored for the same reason and in the same way:
`src/analysis/polish_stopwords.txt`, the list the Carrot2 project wrote, which
Lucene ships with its Polish analyzer and which is BSD-licensed. `LICENSE-STEMPEL`
says so.

## What the suites are run against

Nothing outside the repository. `analysis_stempel/10_basic.yml` and
`20_search.yml` pass on a node started with nothing set.
