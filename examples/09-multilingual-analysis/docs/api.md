# API surface -- 9. Text that is not English

Every request this example makes, in the order it makes them. Generated
from `run.sh`; if the two disagree, `run.sh` is right.

| Step | Method | Path | Body |
|---|---|---|---|
| what the plugins say they can do | `GET` | `/_cat/plugins?v` | inline |
| Thai: no spaces between words, so the tokenizer has to know the language | `POST` | `/_analyze` | inline |
|  | `POST` | `/_analyze` | inline |
| Japanese: kuromoji, with the reading and the part of speech | `POST` | `/_analyze` | inline |
| Korean: nori, which splits compounds | `POST` | `/_analyze` | inline |
| Chinese: smartcn | `POST` | `/_analyze` | inline |
| ICU: folding, normalisation and script-aware breaking, all at once | `POST` | `/_analyze` | `requests/01-icu-folding-normalisation-and-script-aware.json` |
| an index that handles four languages at once, a field per language | `PUT` | `/$IDX` | `requests/02-an-index-that-handles-four-languages.json` |
|  | `POST` | `/$IDX/_bulk?refresh=wait_for` | `data/01-an-index-that-handles-four-languages.ndjson` |
| searching each language in its own field | `GET` | `/$IDX/_search` | inline |
| names that are spelled several ways -- phonetic matching | `PUT` | `/$IDX2` | `requests/03-names-that-are-spelled-several-ways.json` |
|  | `POST` | `/$IDX2/_bulk?refresh=wait_for` | `data/02-names-that-are-spelled-several-ways.ndjson` |
|  | `GET` | `/$IDX2/_search` | inline |
| the encoders, side by side on one name | `POST` | `/_analyze` | inline |
| a character filter that rewrites before anything is tokenised | `POST` | `/_analyze` | `requests/04-a-character-filter-that-rewrites-before.json` |
| a normalizer: a keyword field that is still case-insensitive | `PUT` | `/$IDX3` | `requests/05-a-normalizer-a-keyword-field-that.json` |
|  | `POST` | `/$IDX3/_doc?refresh=true` | inline |
|  | `GET` | `/$IDX3/_search` | inline |

## Endpoints touched

Path parameters and query strings removed, deduplicated:

- `/<var>`
- `/<var>/_bulk`
- `/<var>/_doc`
- `/<var>/_search`
- `/_analyze`
- `/_cat/plugins`

## Request bodies

- [`requests/01-icu-folding-normalisation-and-script-aware.json`](../requests/01-icu-folding-normalisation-and-script-aware.json)
- [`requests/02-an-index-that-handles-four-languages.json`](../requests/02-an-index-that-handles-four-languages.json)
- [`requests/03-names-that-are-spelled-several-ways.json`](../requests/03-names-that-are-spelled-several-ways.json)
- [`requests/04-a-character-filter-that-rewrites-before.json`](../requests/04-a-character-filter-that-rewrites-before.json)
- [`requests/05-a-normalizer-a-keyword-field-that.json`](../requests/05-a-normalizer-a-keyword-field-that.json)
- [`data/01-an-index-that-handles-four-languages.ndjson`](../data/01-an-index-that-handles-four-languages.ndjson)
- [`data/02-names-that-are-spelled-several-ways.ndjson`](../data/02-names-that-are-spelled-several-ways.ndjson)
