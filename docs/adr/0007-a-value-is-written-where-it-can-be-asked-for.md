# A value is written where it can be asked for, and nowhere else

Every value in every document has been written into both of the two JSON views
the schema keeps: `_dyn`, which is analysed into words with frequencies and
positions, and `_raw`, which is untouched and carries the column that sorting
and aggregation read. Both views also carried a column, though only one of
them was ever read from.

That is what an index costs us 1.7 times what OpenSearch charges for the same
documents. Measured on 200,000 log documents, force-merged to a single
segment, with the stored source counted once:

| what is written | on disk |
|---|---|
| untouched view only | 30 MiB |
| analysed view only | 39 MiB |
| both (what we have shipped) | 56 MiB |
| OpenSearch 3.1.0, same documents | 27 MiB |

Nothing in that gap is fragmentation and nothing in it is a setting. Both
views were tried and neither is removable: the analysed view is what `match`
searches, and the untouched view is what `term`, sorting and every aggregation
read. The duplication is the cost, and the duplication is unnecessary, because
the mapping already says which of the two a given field can be asked through.

## The decision

**A value is written into the view its field can be queried through, and not
into the other one.**

| the field's declared type | analysed `_dyn` | untouched `_raw` |
|---|---|---|
| `text`, `match_only_text`, `search_as_you_type` | yes | only if it declares a keyword sub-field |
| `keyword`, `constant_keyword`, `wildcard`, `ip`, `flat_object` | no | yes |
| numeric, date, boolean, and the rest | no | yes |
| a string with no declared type | yes | yes |
| anything else with no declared type | no | yes |

The last two rows are not a hedge. A string that arrives without a mapping is
exactly OpenSearch's dynamic mapping, which gives it a `text` field *and* a
`.keyword` sub-field -- two indexed copies, for the same reason we keep two.
Declaring the field is what removes the copy, in OpenSearch and here alike.

**Columns live on the untouched view only.** A column is read to sort by a
field or to aggregate over it, and neither is ever done on analysed words.
Numeric aggregations read the analysed view's column for a while because a
path holding only numbers resolves without a string column beside it; measured
again over 200,000 documents that is worth 0.14ms on a date histogram and
nothing measurable on avg, stats, histogram, numeric terms or numeric range.
It is not worth a second column of every value in the index.

## What this costs, and what has to be built to pay it

Three things stop working when a value is only in one place, and each is a
piece of work rather than a caveat:

**`exists` has to ask the right view.** It has been asking the column, which
only the untouched view now has. A field whose values are analysed-only is
asked through its postings instead -- has this document any term under this
path -- which is what OpenSearch does through `_field_names`.

**A `.keyword` sub-field has to be honoured at write time, not only at query
time.** `title.keyword` is the untouched view of `title`, so a `text` field
that declares one is written to both views. A mapping that changes after
documents are indexed changes what should have been written for documents
already in the index; those documents keep what they were written with, which
is what a mapping change has always meant here and in OpenSearch.

**The profiler names an aggregator after the column it reads.** With numerics
on the untouched view it began reporting `GlobalOrdinalsStringTermsAggregator`
where OpenSearch reports `NumericTermsAggregator`. The name is decided from
the field's mapped type instead, which is where OpenSearch decides it too.

## What it came to

Measured after the change, same 200,000 documents, both engines force-merged
into a single segment, both given the same mapping:

| mapping | BoostSearch | OpenSearch 3.1.0 |
|---|---|---|
| every field declared | **22.0 MiB** | 22.7 MiB |
| seven of ten declared (what the bench uses) | 30.7 MiB | 27.1 MiB |
| nothing declared | 33.0 MiB | -- |

From 45.3MiB to 30.7 on the bench's own mapping, and from behind to ahead on
an index whose fields are declared. The three fields the bench leaves
undeclared are what remains: a string nobody has typed is written twice by
both engines, and ours is the more expensive of the two duplicates. The bench
mapping is deliberately left as it was rather than being completed to flatter
the number.

Nothing was traded for it that the gates can see: unit 71/71, phase 1 398/398,
the core corpus 1,100/1,100 and the module corpus at its unchanged 820/895,
file for file. Indexing, updates and deletes are within noise of where they
were, and every query shape got faster, because a smaller index is a smaller
thing to read.

## Consequences

An index of declared fields stops paying twice. An index of undeclared fields
pays what OpenSearch pays for the same undeclared fields, which is also twice
-- so the gap closes where a mapping exists and stays where one does not, and
the advice that follows from that ("declare your fields") is the same advice
OpenSearch gives.

A mapping is now load-bearing at write time in a way it was not: the same
document written under two mappings is two different indexes on disk. That was
always true of OpenSearch and is why reindexing exists.

The corpus is what says whether a view was chosen wrongly, and it says it
loudly: routing columns to one view alone moved 142 sections from passing to
failing in one build, each of them naming the field and the view it could not
find. That is the check this change is made under.
