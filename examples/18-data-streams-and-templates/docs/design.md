# Design notes

## Why metrics, and why a stream rather than an index

A host sample has three properties that decide the design: it always carries a
time, it is never changed after it is written, and its value falls with age.
An index that grows forever serves none of that well -- deleting last month
means a delete-by-query over a large index, and a mapping mistake is fixed only
by reindexing everything.

A data stream turns those properties into structure. Writes go to one name;
behind the name is a row of indices, and only the newest takes writes. Old data
is dropped by deleting an old index, which is instant. A mapping fix goes into
the template and arrives with the next generation, without touching the ones
before it.

The price is the append-only rule step 13 shows: a write must be `create`, and
an update or overwrite by id is refused. For metrics that costs nothing. For
data that is corrected after the fact -- orders, tickets, profiles -- a stream
is the wrong tool; use an index behind an alias (example 14).

## Component templates: the parts that are shared

Two component templates, split by what they are about:

| Component | Holds | Why it is its own piece |
|---|---|---|
| `metrics-settings` | shards, replicas, refresh interval | sizing changes on its own schedule, and by different people |
| `metrics-mappings` | `@timestamp`, `host`, `region`, three gauges | the field list is what every metrics template must agree on |

A component template has no `index_patterns` and never matches a name on its
own. It is only ever used by being named in an index template's `composed_of`,
in order, with the index template's own `template` block applied last. Step 5
shows the result of that merge.

The reason to split them at all is step 6: composable index templates are
**not layered**. When two match, the higher priority makes the index and the
other contributes nothing. If the edge template had copied the mappings rather
than naming `metrics-mappings`, the two field lists would drift apart the first
time someone edited one. Shared parts live in components; index templates say
only what is different.

## Priority, and why a tie is refused

| | Legacy `_template` | Composable `_index_template` |
|---|---|---|
| Several match | all of them apply, merged by `order` | exactly one applies, the highest `priority` |
| Same rank, overlapping patterns | allowed; the merge order is arbitrary | refused when the template is written (step 7) |
| Reusable pieces | none | `composed_of` component templates |
| Can make a data stream | no | yes, with `data_stream: {}` |

Refusing a tie at write time is the important property. With legacy templates,
two teams could each add a template for `logs-*` and find out which one won
only by reading a mapping after an index had been made. A composable template
that could produce that ambiguity never gets stored.

`metrics-edge-*` at 200 and `metrics-*` at 100 overlap, and that is allowed
because the priorities differ: the more specific pattern is given the higher
priority, so it wins exactly where it applies and nowhere else. Step 6's
simulation of `metrics-node-cpu` confirms the narrower template does not reach
names it does not match.

## A stream is made by its first write

Nothing in step 8 creates `metrics-node-cpu`. The bulk goes to a name that
does not exist; the server finds the highest-priority template matching the
name, sees `data_stream: {}`, and makes the stream and its first backing index
before writing. That is what lets a fleet of agents start sending
`metrics-<anything>` without anyone provisioning each name.

The explicit `PUT /_data_stream/{name}` in step 20 is still worth having. It
makes the stream, and the backing index with its mapping, before any data
arrives -- so a template mistake is found by whoever sets the stream up, not by
the first agent whose samples are refused, and a dashboard pointed at the name
answers empty rather than 404.

## The time field is not optional

The template's `data_stream` block adds `_data_stream_timestamp` to every
backing index (step 9), and that makes `@timestamp` required: exactly one value,
of type `date`. Step 10 shows a sample without one refused. The reason is
structural -- a stream is ordered by time, its generations cover spans of time,
and `_stats` reports the newest time it holds -- so a document with no time has
no place in it. In a bulk the refusal is per item, which is why step 10's check
counts 73: one good sample written beside the refused one. A shipper that reads
only the top-level `errors` flag would miss that half the batch failed.

## Simulating before creating

`POST /_index_template/_simulate_index/{name}` answers what an index of that
name would be made from, with nothing created. On a stream this matters more
than on an index, because the template is not read when it is written -- it is
read at the next rollover, possibly days later, and a mistake shows up then as
a mapping conflict in a generation nobody was watching. Simulating the stream's
name after every template change moves that failure to the moment of the
change. Step 9 then reads the backing index that was actually made and finds
the same settings and mappings the simulation promised.

## Why the rollover is manual here

Example 02 rolls plain indices over with an ISM policy. This example rolls the
stream by hand, twice: once with a condition that does not hold (step 14), once
unconditionally (step 15), so both answers can be read. In production the same
`rollover` action goes into an ISM policy attached through the template, and
nothing about the stream changes -- it is the same call, made by a job rather
than by a person.

The rollover itself is where the stream earns its keep. Compare step 15 with
step 21: the alias rollover needed the first index created by hand with a name
ending in a number, and an alias on it marked `is_write_index`. Get either
wrong and the rollover is refused or writes go to the wrong index. The stream
needs neither; it names its own backing indices `.ds-<stream>-<generation>`.

## The data

Four hosts, sampled every ten minutes. The morning -- 06:00 to 08:50, 72
samples, plus the one 08:55 sample step 10 writes -- is written before the
rollover; the next hour, 24 samples, after it.
`db01` has a bad hour in the second batch, around 90% CPU where it usually sits
near 55%, so the hourly series in step 18 has a visible jump that lies entirely
in generation 2, and step 19's window straddles the rollover with three samples
on each side. The numbers are generated with a fixed seed and stored in
`data/`, so every run gives the same answers.

## What would change at scale

- **Rollover by size, not by hand.** A generation is usually capped by primary
  shard size (tens of gigabytes) or by age, through ISM. The reason is
  recovery and merge time per shard, not document count.
- **Delete, don't expire documents.** Retention is "delete backing indices
  older than N days", which is instant. Never a delete-by-query on a stream.
- **Shards per generation.** One shard here. A stream multiplies whatever is in
  the template by the number of generations kept, so a template with five
  shards and ninety daily generations is 450 shards. Size the template for one
  generation's volume.
- **Search is still over every generation.** A query with a time range skips
  shards whose `@timestamp` range cannot match, but a query without one reads
  them all. Dashboards should always carry a range.
- **Template changes are not retroactive.** A new field added to
  `metrics-mappings` appears in the next generation; older generations keep
  the mapping they were born with, and a search over both sees the field as
  missing in the old ones.
