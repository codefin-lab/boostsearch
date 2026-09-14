#!/usr/bin/env bash
# Two ways to model one-to-many, and the cost of each.
source "$(dirname "$0")/lib.sh"

step "the join field: questions and answers in one index, separate documents"
IDX=qa
gone "/$IDX"
reqf PUT "/$IDX" requests/01-the-join-field-questions-and-answers.json
green "$IDX"

step "a parent and its children -- all on one shard, by routing"
ndjson "/$IDX/_bulk?refresh=wait_for&routing=q1" data/01-a-parent-and-its-children-all.ndjson
ndjson "/$IDX/_bulk?refresh=wait_for&routing=q2" data/02-a-parent-and-its-children-all.ndjson
expect_docs "$IDX" 6 "two questions, three answers, one comment -- each its own document"

step "questions that have an accepted answer mentioning a word"
reqf GET "/$IDX/_search" requests/02-questions-that-have-an-accepted-answer.json
note "score_mode: max carries the best child score up to the parent"

step "answers whose question is tagged allocation"
reqf GET "/$IDX/_search" requests/03-answers-whose-question-is-tagged-allocation.json

step "the children of one known parent, cheaply"
reqf GET "/$IDX/_search?routing=q1" requests/04-the-children-of-one-known-parent.json

step "how many answers each question has, and the best of them"
reqf GET "/$IDX/_search" requests/05-how-many-answers-each-question-has.json

step "a question with no answer at all"
reqf GET "/$IDX/_search" requests/06-a-question-with-no-answer-at.json

step "the same thing modelled as nested, for comparison"
IDX2=qa-nested
gone "/$IDX2"
reqf PUT "/$IDX2" requests/07-the-same-thing-modelled-as-nested.json
green "$IDX2"
ndjson "/$IDX2/_bulk?refresh=wait_for" data/03-the-same-thing-modelled-as-nested.ndjson
expect_docs "$IDX2" 2 "the same data as two documents: nested children do not count"
reqf GET "/$IDX2/_search" requests/08-the-same-thing-modelled-as-nested.json

step "the difference that decides which to use: updating one child"
note "join: one small document rewritten"
req POST "/$IDX/_update/a2?routing=q1" '{ "doc": { "votes": 3 } }'
note "nested: the whole parent and every sibling rewritten"
reqf POST "/$IDX2/_update/n1" requests/09-the-difference-that-decides-which-to.json

step "and the difference in reading: a nested aggregation stays in one document"
reqf GET "/$IDX2/_search" requests/10-and-the-difference-in-reading-a.json

step "what the shards actually hold"
req GET "/_cat/shards/qa*?v&h=index,shard,prirep,docs,state"

step "what this example leaves behind, checked rather than assumed"
done_
