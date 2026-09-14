# 9. Text that is not English

Thai has no spaces between words. Japanese mixes three scripts. Korean glues
compounds together. Chinese needs either a dictionary or bigrams. And in every
language a person's name is spelled several ways by the people typing it.

This example walks each of those with `_analyze` first -- so you can see the
tokens before committing to a mapping -- then builds one index that handles
four languages in four sub-fields of a single field.

## What it shows

| Step | Feature |
|---|---|
| 1 | `_cat/plugins` -- what analysis the node actually answers for |
| 2 | the `thai` tokenizer against `standard`, on the same string |
| 3 | `kuromoji_tokenizer` with `explain` and `baseForm`/`reading`/`partOfSpeech` |
| 4 | `nori_tokenizer` |
| 5 | `smartcn` |
| 6 | `icu_tokenizer`, `icu_normalizer`, `icu_folding` |
| 7 | four custom analysers in one index; `html_strip`, `stop`, `decimal_digit`, `kuromoji_baseform`, `nori_readingform`, `cjk_bigram` |
| 8 | a `match` per language, with highlighting |
| 9 | `phonetic` with `double_metaphone` and `koelnerphonetik` |
| 10 | every encoder, on one name |
| 11 | `pattern_replace` char filter |
| 12 | a keyword `normalizer` |

## Running it

```bash
make serve      # a node configured for this example, port 9269, foreground
make run        # the example, in another terminal
```

`make check` validates the scripts and every request body without a server;
`make clean` deletes what the example left behind. Copy `.env.example` to
`.env` to change the address or anything else.

The longer form, and what this example needs of the node:

```bash
./target/release/boostsearch &
examples/09-multilingual-analysis/run.sh
```

`beider_morse` and some other encoders need their rule files; `docs/phonetic.md`
says where. The step that exercises them tolerates a missing one.

## What to look for

- **Step 2** is the clearest single demonstration in this directory. The Thai
  tokenizer returns words; `standard` returns the whole string as one token,
  because it has no spaces to break on. An index built on the wrong one
  matches nothing and gives no error.
- **Step 3** with `explain: true` returns the dictionary form and the reading
  of each token. That is why `kuromoji_baseform` in the analyser at step 7
  makes 開きます findable by 開く.
- **Step 7** puts all four analysers on sub-fields of one field. The document
  is written once; which sub-field you query decides which language's rules
  apply. Compare with a field per language, which needs the writer to know the
  language up front.
- **Step 9** matches "Kathrine Smithe" to three documents, none of which
  contains either word. Phonetic filters index how a word sounds; the trade is
  false positives, which is why it belongs on a sub-field beside the exact one
  rather than replacing it.
- **Step 12** is the small one people miss: a `keyword` field usually means
  exact bytes, so `Café` and `cafe` are different terms. A normalizer applies a
  filter chain to a keyword without tokenising it.

## This directory

It is a project of its own: nothing here reaches outside the directory,
so it can be copied somewhere else and still run.

| | |
|---|---|
| `README.md` | this page |
| `docs/design.md` | why it is built this way, and what would change at scale |
| `docs/api.md` | every request it makes, and every endpoint it touches |
| `docs/troubleshooting.md` | what goes wrong, and what it means |
| `run.sh` | the example |
| `node.sh` | a node configured for exactly what this example needs |
| `lib.sh` | shell helpers; its own copy |
| `Makefile` | `serve`, `run`, `check`, `clean` |
| `.env.example` | the settings, with a line each on what they are for |
| `requests/` | 5 request bodies, one file each |
| `data/` | 2 bulk document sets |

## Leaves behind

The indices `multilingual`, `people` and `tags`. Rerunning deletes them first.
