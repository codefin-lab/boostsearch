# Design notes

## The method, which matters more than any of the queries

Relevance work goes wrong when it is done by argument. Somebody looks at the
first page, says it is bad, a boost is added, somebody else says it is better,
and nobody knows whether it is. Six months later the query has fourteen boosts,
nobody can remove one, and the search is worse than it was.

The method here is:

1. **Find a concrete failure.** Not "results feel off" -- a specific query
   whose specific answer is specifically wrong. Here: a page deprecated three
   years ago outranks the current one.
2. **Explain it.** `_explain` says why, term by term. Most relevance fixes are
   applied to the wrong cause because this step was skipped.
3. **Write down what right looks like.** A judgement list: for each query, which
   documents are good, graded. This is the expensive part and the only part
   that cannot be automated.
4. **Change one thing, and score it.** `_rank_eval` gives a number. If the
   number did not move, the change did nothing, whatever it looks like.

Steps 9 and 10 are the whole point of the example. Everything before them is
material to score.

## Why `d3` wins, and why that is not a bug

`d3` is the deprecated page. Its body says "snapshot" three times in a long
paragraph. BM25's term frequency component rewards that, and the length
normalisation only partly offsets it.

The scorer did exactly what it was asked. The information that `d3` is useless
-- that it is deprecated, three years old, and superseded -- is in the
document and not in the query. Fixing this means putting that information into
the query, which is what steps 4 to 6 do, three different ways.

## `boosting` rather than `must_not`

```json
"boosting": { "positive": {...}, "negative": { "term": { "deprecated": true } },
              "negative_boost": 0.15 }
```

A `must_not` on `deprecated` makes the page unfindable -- including by someone
searching for it by name, who has a legitimate reason to want it.
`negative_boost` multiplies its score by 0.15: it goes to the bottom and stays
reachable.

The general principle: **demote, do not exclude**, unless the document must
genuinely never be returned. Exclusion turns a ranking problem into a recall
problem, and recall problems are invisible -- nobody reports the result they
did not see.

## Per-field similarity

```json
"flat_length": { "type": "BM25", "b": 0.0 }    on title
"long_form":   { "type": "BM25", "b": 0.9 }    on body
```

`b` controls length normalisation: how much a long field is penalised for
being long.

- On a **title**, length is not noise -- a six-word title is not worse than a
  three-word one -- so `b: 0` turns normalisation off.
- On a **body**, a term in a short paragraph is a stronger signal than the same
  term in a long one, so `b: 0.9` is close to full normalisation.

Step 12 measures both. The point is not that these values are right; it is that
the choice exists per field and defaulting it is a decision too.

## `rescore`, and the shape of every expensive signal

```json
"query":   { "match": ... },              cheap, runs over everything
"rescore": { "window_size": 20,
             "rescore_query": { "match_phrase": ... } }   expensive, top 20 only
```

A phrase query, a `function_score` with decays, a script, a vector rerank --
anything costly belongs behind a rescore window. The window trades a little
recall at the boundary (a document at rank 21 cannot be lifted into the top 10)
for a large cost saving.

On six documents the rescore is pointless. It is in the example because it is
the pattern that makes every other technique here affordable on a real index.

## The metrics, and which to use

| Metric | Answers | Use when |
|---|---|---|
| nDCG | is the good stuff near the top, graded | graded judgements, general purpose |
| precision@k | what fraction of the top k is relevant | binary judgements |
| recall@k | what fraction of the relevant is in the top k | you care about missing things |
| MRR | how far down is the *first* good result | one right answer per query |
| ERR | like nDCG, but a great result discounts the ones below | navigational search |

nDCG is the default for a reason: it uses the grades (3, 2, 1, 0), and it
discounts by position, which is what a ranking is judged on. MRR is the right
one for "find the documentation page for X", where there is one answer and
everything else is noise.

## What a judgement list costs

Three queries and eight judgements here. A real one is a few hundred queries
sampled from real traffic, judged by people who know the domain, refreshed
yearly.

That is expensive, and it is the only thing that makes relevance work
falsifiable. A team that will not pay for it is choosing to tune by argument,
and should at least know that is the choice.

## What would change at scale

- **Judgements go stale** as the corpus changes. A document deleted or rewritten
  invalidates its grade.
- **`_rank_eval` runs every request in the list**, so a 500-query list is 500
  searches. It belongs in CI, not in a loop.
- **Click data is not a judgement list** but it is a cheap proxy, and the usual
  production answer: interleave two rankings, measure clicks, and use explicit
  judgements to check the proxy has not drifted.
