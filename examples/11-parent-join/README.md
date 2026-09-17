# 11. One-to-many, modelled twice

A question with its answers can be one document with an array inside it, or a
parent document with child documents. The two look similar and behave very
differently, and choosing wrong is expensive to undo. This example builds the
same data both ways and then does the operations that separate them.

**Nested** keeps everything in one Lucene document block. Reading is fast;
updating one answer rewrites the question and every other answer.

**Join** keeps each answer as its own document, routed to the parent's shard.
Updating one answer is one small write; every read has to look the relation up,
which is why a `has_child` query costs more than a `nested` one.

## What it shows

| Step | Feature |
|---|---|
| 1 | a `join` field with two child relations |
| 2 | `_bulk` with `routing`, a parent and its children |
| 3 | `has_child` with `score_mode` and `inner_hits` |
| 4 | `has_parent` with `score: true` and `inner_hits` |
| 5 | `parent_id` query, with routing |
| 6 | `children` aggregation, and `parent` to climb back up |
| 7 | `must_not` + `has_child` -- the parents with no children |
| 8 | the same data as `nested`, with `nested` + `inner_hits` |
| 9 | updating one child, both ways |
| 10 | a `nested` aggregation |
| 11 | `_cat/shards` |

## Running it

```bash
make serve      # a node configured for this example, port 9271, foreground
make run        # the example, in another terminal
```

`make check` validates the scripts and every request body without a server;
`make clean` deletes what the example left behind. Copy `.env.example` to
`.env` to change the address or anything else.

The longer form, and what this example needs of the node:

```bash
./target/release/velosearch &
examples/11-parent-join/run.sh
```

## What to look for

- **Step 2** passes `routing=q1`. This is not optional: a join only works if
  parent and children live on the same shard, and routing is what puts them
  there. Forget it on a multi-shard index and the children are simply never
  found.
- **Step 3 versus step 8** ask the identical question and return the identical
  answer. Everything else about them differs.
- **Step 6** goes down and then back up: `children` moves from questions to
  answers, and `parent` inside it moves back, so the tag buckets count
  questions again. There is no nested equivalent of climbing out to a different
  document, because there is no different document.
- **Step 9** is the decision. The join update names one child by id. The nested
  update runs a script over the parent's whole array and rewrites the lot. With
  two answers that is nothing; with two thousand it is the whole argument.

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
| `data/` | 3 bulk document sets |

## Leaves behind

The indices `qa` and `qa-nested`. Rerunning deletes them first.
