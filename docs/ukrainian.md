# Ukrainian, and the dictionary it reads

`analyzer: ukrainian` is what OpenSearch's analysis-ukrainian plugin
installs. It is not a stemmer. A Morfologik dictionary is a finite-state
automaton holding every inflected form of every word, each followed by an
instruction for turning the form back into its lemma, and a form is often
several words at once: `колу` is the dative of `кола`, of `коло` and of `кіл`.
So one token becomes three, all standing in the same place, and a search for
any of them finds the document.

`src/analysis/morfologik.rs` is the reader for the `CFSA2` container the
dictionary is stored in and for the four ways a lemma may be written against
its form -- whole, or as a piece cut off one end, both ends, or the middle.
The analyzer around it is the standard tokenizer, lowercased, with Ukrainian
stop words dropped; before any of that, the several apostrophes Ukrainian is
written with are made one, the stress mark and the soft hyphen come off, and
`ґ` is written `г`, which is how the dictionary holds it.

A word the dictionary does not know stands for itself.

## Where the dictionary is looked for

In this order, first hit wins. Each is a directory holding `ukrainian.dict`
and, beside it, `ukrainian.info`:

1. `$VELOSEARCH_UKRAINIAN_DICT`
2. `$VELOSEARCH_CONFIG/analysis-ukrainian/`
3. `$VELOSEARCH_DATA/config/analysis-ukrainian/`
4. `./config/analysis-ukrainian/`
5. `$HOME/velo-fixtures/ukrainian-dict/`

The second, third and fourth are where OpenSearch keeps what a plugin reads.
The last is the documented default on a development machine, and is the same
shape the geoip databases and the Beider-Morse rule files already use --
`$VELO_FIXTURES` (default `~/velo-fixtures`), which `tools/gate_node.sh`
passes as `VELOSEARCH_UKRAINIAN_DICT`.

`ukrainian.info` says which separator byte the automaton uses and how a lemma
is encoded against its form. It is read rather than assumed: a dictionary
that declares an encoding other than UTF-8, or an encoder this does not
implement, is refused rather than read as something it is not.

## Why the dictionary is not in this repository

It is seven megabytes of somebody else's data. The licence would not stop it
being carried -- `morfologik-ukrainian-search` is Apache-2.0, from the
[dict_uk](https://github.com/brown-uk/dict_uk) project, and the file is
`ua/net/nlp/ukrainian.dict` in the 4.9.1 jar -- but seven megabytes of a
generated automaton is not something a source tree should grow by, and
whether a release carries it is a decision to make rather than something to
slip into a commit. The Polish table is vendored, at two megabytes, because
without it `polish_stem` would not be a stemmer at all; see
[polish.md](polish.md).

**Without the dictionary the analyzer still works and finds nothing.** Text
is tokenized, lowercased and has its stop words dropped, and every word
stands for itself. Nothing fails, and neither does an index that names the
analyzer. What is missing is the data, not the code.

## What the suites are run against

`ukrainian.dict` and `ukrainian.info` as they come out of the
`morfologik-ukrainian-search` 4.9.1 jar, copied to `ukrainian-dict` under
`$VELO_FIXTURES` (default `~/velo-fixtures`). With them in place
`analysis_ukrainian/10_basic.yml` and `20_search.yml` pass. Keep that
directory somewhere a restart does not empty: without the file, those
sections fail for want of data rather than of code.
