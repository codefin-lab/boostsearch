# API surface -- 11. One-to-many, modelled twice

Every request this example makes, in the order it makes them. Generated
from `run.sh`; if the two disagree, `run.sh` is right.

| Step | Method | Path | Body |
|---|---|---|---|
| the join field: questions and answers in one index, separate documents | `PUT` | `/$IDX` | `requests/01-the-join-field-questions-and-answers.json` |
| a parent and its children -- all on one shard, by routing | `POST` | `/$IDX/_bulk?refresh=wait_for&routing=q1` | `data/01-a-parent-and-its-children-all.ndjson` |
|  | `POST` | `/$IDX/_bulk?refresh=wait_for&routing=q2` | `data/02-a-parent-and-its-children-all.ndjson` |
| questions that have an accepted answer mentioning a word | `GET` | `/$IDX/_search` | `requests/02-questions-that-have-an-accepted-answer.json` |
| answers whose question is tagged allocation | `GET` | `/$IDX/_search` | `requests/03-answers-whose-question-is-tagged-allocation.json` |
| the children of one known parent, cheaply | `GET` | `/$IDX/_search?routing=q1` | `requests/04-the-children-of-one-known-parent.json` |
| how many answers each question has, and the best of them | `GET` | `/$IDX/_search` | `requests/05-how-many-answers-each-question-has.json` |
| a question with no answer at all | `GET` | `/$IDX/_search` | `requests/06-a-question-with-no-answer-at.json` |
| the same thing modelled as nested, for comparison | `PUT` | `/$IDX2` | `requests/07-the-same-thing-modelled-as-nested.json` |
|  | `POST` | `/$IDX2/_bulk?refresh=wait_for` | `data/03-the-same-thing-modelled-as-nested.ndjson` |
|  | `GET` | `/$IDX2/_search` | `requests/08-the-same-thing-modelled-as-nested.json` |
| the difference that decides which to use: updating one child | `POST` | `/$IDX/_update/a2?routing=q1` | inline |
|  | `POST` | `/$IDX2/_update/n1` | `requests/09-the-difference-that-decides-which-to.json` |
| and the difference in reading: a nested aggregation stays in one document | `GET` | `/$IDX2/_search` | `requests/10-and-the-difference-in-reading-a.json` |
| what the shards actually hold | `GET` | `/_cat/shards/qa*?v&h=index,shard,prirep,docs,state` | inline |

## Endpoints touched

Path parameters and query strings removed, deduplicated:

- `/<var>`
- `/<var>/_bulk`
- `/<var>/_search`
- `/<var>/_update/a2`
- `/<var>/_update/n1`
- `/_cat/shards/qa*`

## Request bodies

- [`requests/01-the-join-field-questions-and-answers.json`](../requests/01-the-join-field-questions-and-answers.json)
- [`requests/02-questions-that-have-an-accepted-answer.json`](../requests/02-questions-that-have-an-accepted-answer.json)
- [`requests/03-answers-whose-question-is-tagged-allocation.json`](../requests/03-answers-whose-question-is-tagged-allocation.json)
- [`requests/04-the-children-of-one-known-parent.json`](../requests/04-the-children-of-one-known-parent.json)
- [`requests/05-how-many-answers-each-question-has.json`](../requests/05-how-many-answers-each-question-has.json)
- [`requests/06-a-question-with-no-answer-at.json`](../requests/06-a-question-with-no-answer-at.json)
- [`requests/07-the-same-thing-modelled-as-nested.json`](../requests/07-the-same-thing-modelled-as-nested.json)
- [`requests/08-the-same-thing-modelled-as-nested.json`](../requests/08-the-same-thing-modelled-as-nested.json)
- [`requests/09-the-difference-that-decides-which-to.json`](../requests/09-the-difference-that-decides-which-to.json)
- [`requests/10-and-the-difference-in-reading-a.json`](../requests/10-and-the-difference-in-reading-a.json)
- [`data/01-a-parent-and-its-children-all.ndjson`](../data/01-a-parent-and-its-children-all.ndjson)
- [`data/02-a-parent-and-its-children-all.ndjson`](../data/02-a-parent-and-its-children-all.ndjson)
- [`data/03-the-same-thing-modelled-as-nested.ndjson`](../data/03-the-same-thing-modelled-as-nested.ndjson)
