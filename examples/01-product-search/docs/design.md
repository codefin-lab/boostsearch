# Design notes

## The decision this example is really about

A shop search has two jobs that pull against each other: **find everything the
customer might have meant**, and **put the right three things at the top**. The
first is an analysis problem, the second a scoring one, and mixing them is the
usual cause of a search that is either too narrow or unrankable.

This example keeps them apart. Analysis decides *what matches*; `function_score`
decides *what wins*. Nothing in the analysis chain knows about sales figures,
and nothing in the scoring knows about synonyms.

## Why synonyms are applied at index time here

`shop_synonyms` sits in the `shop_text` analyser, which is used for both
indexing and searching. The alternative -- a `search_analyzer` that expands the
query instead -- has a real advantage: changing the synonym list does not
require reindexing.

It was not chosen here because `synonym_graph` at search time interacts badly
with `fuzziness` and with phrase matching, and this example wants both. On a
real shop the trade is worth revisiting: if the merchandising team edits
synonyms weekly, put them on the search side and accept the loss.

## Why `name` has three shapes

| Field | Analyser | For |
|---|---|---|
| `name` | `shop_text` | ordinary matching, stemmed, with synonyms |
| `name.prefix` | `shop_prefix` indexing, `standard` searching | as-you-type |
| `name.raw` | none (keyword) | sorting, faceting, exact lookup |

The asymmetry on `name.prefix` matters: `edge_ngram` at index time and
`standard` at search time. Put the n-gram filter on the search side as well and
the query "meridian" becomes `me`, `mer`, `meri`... and matches everything that
starts with `me`.

## Why `scaled_float` for price

`price` is money, and money in a `double` is a bug waiting for an aggregation.
`scaled_float` with `scaling_factor: 100` stores baht as an integer number of
satang, which sums exactly. A `float` would drift; a `keyword` could not be
ranged or summed at all.

## The scoring functions, and what each is for

```
field_value_factor(sold_last_month, log1p, 0.4)   popularity, damped
field_value_factor(rating, none, 0.5)             quality, linear
filter(in_stock: false) -> weight 0.2             availability, punitive
```

`log1p` on the sales figure is deliberate. Without it a product with 1,500
sales outranks one with 150 by ten times, which swamps every other signal;
`log1p` makes it about twice, which is roughly how much a shopper cares.

The out-of-stock weight is a filter rather than a `must_not` for the same
reason example 16 uses `boosting` rather than exclusion: a customer searching
for a specific product by name should still find it and be told it is out of
stock, not be shown nothing.

## What would change at scale

- **Six documents, two shards.** Both numbers are wrong for a real shop and
  right for an example. With a real catalogue, `sold_last_month` would come
  from a nightly job writing into the index rather than being a stored field,
  and the `function_score` would move behind a `rescore` window (example 16,
  step 7) so it runs over the top few hundred rather than everything.
- **The completion suggester holds its whole structure in memory.** That is
  fine for product names and not fine for, say, every review sentence.
- **`collapse` is a per-request cost** that grows with the number of distinct
  values. On six brands it is nothing; on a million sellers it is not.
