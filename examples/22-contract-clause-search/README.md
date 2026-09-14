# 22. Finding the clause, and showing where it is

Twelve clauses from three invented contracts, one document per clause. A
reviewer looking for "termination on notice" does not want the contract that
mentions both words somewhere; they want the clause where the one is followed
by the other, and they want those words marked when it comes back.

That makes this example about two things that belong together: queries that
care where words stand -- phrases with slop, spans, intervals -- and
highlighting, which uses the same positions and offsets to show the reader
what matched. It ends with the tools that explain an answer: `_explain` for a
score and `_termvectors` for what the index holds.

## What it shows

| Step | Feature |
|---|---|
| 1 | `term_vector: with_positions_offsets`, an `english` sub-field beside the plain one |
| 3 | the `unified` highlighter, `number_of_fragments: 0` for the whole clause |
| 4 | `unified`, `plain` and `fvh` side by side; `pre_tags` / `post_tags` |
| 5 | `highlight_query` -- found by a keyword, marked by other words |
| 6 | `matched_fields` with `fvh` -- matched on stems, marked on the text |
| 7 | `match_phrase` with `slop` |
| 8 | `span_near` with `in_order` and a `slop` of words between |
| 9 | `span_first` -- the clause *opens* with the word |
| 10 | `span_multi` over a `prefix`, inside a `span_near` |
| 11 | `intervals` `all_of` with `max_gaps` |
| 12 | `intervals` `any_of` inside an `all_of` |
| 13 | `intervals` `filter` with `not_containing` |
| 14 | `more_like_this` from a given clause |
| 15 | `combined_fields` across `title^2` and `body` |
| 16 | `_explain` for one hit |
| 17 | `_termvectors` with positions and offsets for one document |

## Running it

```bash
make serve      # a node configured for this example, port 9282, foreground
make run        # the example, in another terminal
```

`make check` validates the scripts and every request body without a server;
`make clean` deletes what the example left behind. Copy `.env.example` to
`.env` to change the address or anything else.

The longer form, and what this example needs of the node:

```bash
./target/release/boostsearch &
examples/22-contract-clause-search/run.sh
```

`BS` sets the address if the server is not on `http://127.0.0.1:9200`. The
node needs nothing special; `node.sh` only gives it a port of its own.

## What to look for

- **Step 3** returns c10, the assignment clause, for *written notice*: it says
  *prior written consent*. Only *written* is marked in it, which is the
  highlight doing its job -- the reader sees at once that this is a weak hit.
- **Step 5** asks for every clause of type `liability` and marks
  *negligence* and *fraud* in c9, words the query never contained:
  `...caused by <mark>negligence</mark>, or for <mark>fraud</mark>.`
- **Step 6** searches for *terminates notices*, words that appear in no clause.
  The english sub-field finds c3, c4 and c5 by their stems, and
  `matched_fields` marks `<em>termination</em>` in c5's unstemmed text.
- **Step 7**: *notify in writing* finds nothing as a phrase and c2 with
  `slop: 2`, because *the supplier* stands between.
- **Step 8** is the difference between "near" and "near enough": with five
  words allowed between *terminate* and *notice* only c4 (for cause) matches;
  at eight, c3 (for convenience, *ninety days written notice*) joins it.
- **Step 9** matches c2, c11 and c12, where *supplier* is the second word. c8
  and c9 also mention the supplier, but later, and `span_first` leaves them out.
- **Step 13** is the one a `bool` cannot do. *liability ... limited* matches
  c9 and c12; `not_containing: not` drops c12, whose text is *liability under
  this warranty is not limited*, and keeps the real cap.
- **Step 14** puts c3, a termination clause, first among clauses like the
  limitation of liability, and c9, the other cap, second. On twelve clauses the
  shared boilerplate outweighs the shared subject; `docs/design.md` says what
  changes that on a real corpus.
- **Step 17** shows why step 8 needed a slop of 8 for c3: *terminate* is at
  position 3 and *notice* at position 12 (offsets 83 to 89).

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
| `requests/` | 14 request bodies, one file each |
| `data/` | 1 bulk document set |

## Leaves behind

The index `clauses`. Rerunning deletes it first.
