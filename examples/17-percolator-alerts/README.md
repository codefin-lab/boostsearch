# 17. Saved searches that find the documents, not the other way round

A search takes a query and finds the documents that match it. An alert does
the reverse: a document arrives, and the question is which of the saved
queries it matches. Asking every saved query in turn does not scale past a
handful, and polling the index on a timer finds the event late and finds it
twice. The `percolator` field turns the saved queries into documents of an
index, so the question "which rules does this event trip?" is one search.

Twelve alert rules for a small platform -- payments, checkout, auth, database
hosts -- each with an owner, a severity and a channel. Ten events from one
hour. Every expected answer is written into `run.sh`, so a wrong answer stops
the example rather than scrolling past.

## What it shows

| Step | Feature |
|---|---|
| 1 | the `percolator` field type, beside the fields the stored queries use |
| 2 | storing queries: a bulk of rules, each an ordinary document |
| 3 | a query naming an unmapped field, refused at write time |
| 4 | `percolate` with one `document`, scored: the rules best first |
| 5 | `percolate` with several `documents`, and `_percolator_document_slot` |
| 6 | `highlight` inside `percolate`: the words in each event that tripped a rule |
| 7 | an ordinary index of events to percolate from |
| 8 | `percolate` an indexed document by `index` and `id`; the 404 when it is not there |
| 9 | `percolate` in a `bool` filter beside `term` on the rules' metadata |
| 10 | `percolate` plus `must_not`, with `terms` aggregations over the matching rules |
| 11 | `_update` on a rule, taking effect on the next percolation |

## Running it

```bash
make serve      # a node configured for this example, port 9277, foreground
make run        # the example, in another terminal
```

`make check` validates the scripts and every request body without a server;
`make clean` deletes what the example left behind. Copy `.env.example` to
`.env` to change the address or anything else.

The longer form, and what this example needs of the node:

```bash
./target/release/velosearch &
examples/17-percolator-alerts/run.sh
```

Nothing beyond a plain node: no plugin setting, no special path.

## What to look for

- **Step 3** is the reason the event fields are mapped in the rules index. A
  rule with `term` on `hostname` -- a typo for `host` -- is answered with a 400
  `mapper_parsing_exception`, caused by "No field mapping can be found for the
  field with name [hostname]", and the index still holds 12 rules. Accepted,
  that rule would never fire and nothing would say so.
- **Step 4** percolates a payments timeout in eu-west that is stored nowhere.
  Four of the twelve rules match, and they come back ranked by how well each
  rule's own query matched the event: `r06` and `r09`, which match the word
  "timeout", score 0.1308; `r01` and `r05`, which only filter on `service`,
  `level` and `region`, score 0.0. Every hit carries
  `_percolator_document_slot: [0]`.
- **Step 5** sends three events and gets 5 rules back, one hit per rule, and
  the slot on each hit says which event tripped it: `r02` slot `[0]` (the slow
  checkout), `r05` `r07` `r12` slot `[1]` (the refused database connection),
  `r04` slot `[2]` (the failed login).
- **Step 6** highlights the events, not the rules. `r06` ("timeouts,
  anywhere") was tripped by two of the three events and says by which word in
  each: `1_message` is "request `<em>timed</em>` out waiting for lock",
  `2_message` "upstream `<em>timeout</em>` calling card processor". `r08`, a
  `range` on latency, is tripped by slots `[0, 1]` and has nothing to mark.
  The order is the score: the phrase `r10` at 1.1343, the range `r08` at
  exactly 1.0, the words `r06` and `r09` at 0.4575, the filters at 0.0.
- **Step 8** names `events/e05` instead of carrying a document. The full disk
  on `db-1` trips `r03` and `r12`. Asked for `events/e99`, which does not
  exist, the answer is a 404 `resource_not_found_exception` -- not an empty
  hit list that would read as "no rule matched".
- **Step 9** filters the same four matches from step 4 down to the two
  `critical` ones, both `ana`'s, both paging. For `ben` it is one rule, `r06`.
  The filter is on fields written beside the query, not in it, which is the
  whole point of storing rules as documents.
- **Step 10** is the shape an alert router uses. Five events trip ten rules;
  without the three `info` rules, 7 remain. `by_severity` says 5 critical and
  2 warning; `by_owner` says `ops` gets 3 (2 pager, 1 slack), `ana` 2 (both
  pager), `ben` 1 (slack) and `chen` 1 (pager). The hits inside a `filter` all
  score 0.0, which is right: nothing here is being ranked.
- **Step 11** lowers `r08`'s threshold from 5000 ms to 3000 ms with an
  `_update`, and step 4's event -- latency 3100 -- trips 5 rules instead of 4.

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
| `requests/` | 10 request bodies, one file each |
| `data/` | 2 bulk document sets |

## Leaves behind

The indices `alerts` (12 rules, `r08` at its lowered threshold) and `events`
(10 events). Rerunning deletes both first.
