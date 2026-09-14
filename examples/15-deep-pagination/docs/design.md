# Design notes

## Why `from: 11000` is refused rather than slow

To return hits 11,000 to 11,020 sorted by a field, **every shard** must collect
its own top 11,020 and send them to the coordinator, which merges 3 × 11,020
and throws away all but twenty.

The cost is linear in `from`, paid on every shard, in memory, for a page nobody
reaches by clicking. `index.max_result_window` (default 10,000) is a guard
against an accidental denial of service, and raising it (step 11) does not make
the query cheap -- it makes it permitted.

## The three alternatives, and what each actually fixes

| | Fixes | Does not fix | Cost |
|---|---|---|---|
| `search_after` | the memory blow-up | results shifting under you | same as page 1 |
| `search_after` + PIT | both | -- | a held view |
| `scroll` | both, for a full pass | anything user-facing | a held context per scroll |

`search_after` is a cursor: instead of "skip 11,000", it says "start after this
exact sort position". Each shard collects `size` hits, not `from + size`. The
cost of page 500 is the cost of page 1.

A point in time freezes the view the cursor walks. Without it, a document
written between page 3 and page 4 that sorts before the cursor shifts
everything after it by one -- so page 4 repeats a row from page 3, or skips
one. Step 6 makes this concrete by writing three documents *during* the paging
and showing that none of them appear.

`scroll` does both but holds a search context per scroll and is meant to be
walked once, front to back. It is the export tool.

## The tiebreak, and the bug it prevents

```json
"sort": [{ "seq": "asc" }, { "_id": "asc" }]
```

`search_after` positions by sort values. If two documents share the same value
for every sort key, there is no defined "after" between them, and at a page
boundary one is skipped or repeated.

This is the bug that never appears in testing, because test data rarely has
duplicates in the sort field, and appears immediately in production, where a
timestamp with second resolution has thousands of them.

Every `search_after` sort must end in a unique key. `_id` always works; a
sequence number is cheaper if there is one.

## `sort: ["_doc"]` in the scroll

`_doc` is not an ordering. It means "whatever order the segments happen to
hand documents over in", which costs nothing because no comparison is done.

That is exactly right for an export -- every document comes back once, and
nobody cares in what order -- and exactly wrong for anything a person sees.

## `track_total_hits`

| Value | Behaviour | Cost |
|---|---|---|
| `true` | exact count | full collection |
| a number | count up to n, then report `gte: n` | bounded |
| `false` | no count at all | free |

The default caps counting, which is why a result sometimes says
`"relation": "gte"` -- that is not an error or an approximation of the *hits*,
only of the total.

For page two onward, the total is already on the screen. `track_total_hits:
false` is the correct setting there and is the single cheapest improvement
available to most paginated searches.

## Why the PIT must be released

A point in time holds the segments it was opened against, so they cannot be
merged away. A PIT that is never released keeps disk space and file handles
until its `keep_alive` expires. Step 7 deletes it explicitly; the `keep_alive`
is the backstop, not the plan.

The same is true of a scroll context, which is why step 8 deletes the scroll id
rather than letting it time out.

## What would change at scale

- **Slicing** (step 9) splits an export into independent scrolls that can run
  on different machines. `max` should be a multiple of the shard count.
- **A scroll's `size` is per shard**: `size: 1000` on three shards returns up
  to 3,000 per batch. Sizing by memory means remembering that.
- **PIT and scroll both pin segments.** A cluster running many long exports
  ends up unable to merge, and its search performance degrades in a way that
  looks unrelated to the exports.
