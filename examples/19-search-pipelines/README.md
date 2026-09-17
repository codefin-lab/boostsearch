# 19. Changing what a search asks and answers, without changing the client

A bookshop app was released a year ago. It sends one search body, and it will
go on sending that body for as long as people have the old version installed.
Since then the shop has decided that sold-out books are not results, the
catalogue has renamed `price` to `price_eur`, and the new page design shows
five books, not twenty.

None of that can be fixed in the app. All of it can be fixed between the app
and the index: a search pipeline rewrites the request on the way in and the
answer on the way out, and the app's request body -- `requests/02` -- is sent
unchanged by every step of this example.

## What it shows

| Step | Feature |
|---|---|
| 2 | the baseline: the app's request, no pipeline |
| 3 | `PUT` and `GET _search/pipeline/{name}`; `filter_query` as a request processor, `rename_field` as a response processor; `tag`, `description`, `version` |
| 4 | `?search_pipeline=` -- a pipeline chosen per request |
| 5 | replacing a pipeline; a `script` request processor with `params`; the `sort` response processor |
| 7 | `oversample`, `collapse` and `truncate_hits` together, sharing the request context |
| 8 | `index.search.default_pipeline` -- a pipeline nobody has to name |
| 9 | `search_pipeline=_none` -- opting out of the default |
| 10 | a temporary pipeline written inline in the request body |
| 11 | a failing processor fails the search, and says which processor it was |
| 12 | `ignore_failure` on one processor |
| 13 | `GET _search/pipeline`, `DELETE _search/pipeline/{name}` |

Not here: the `hybrid` query and the `normalization-processor` or
`score-ranker-processor` that combine its parts. The server runs them, but a
bookshop has no second way of scoring a title worth showing next to the first;
`docs/design.md` says what a pipeline for one looks like. The `split` response
processor is left out for the same reason.

## Running it

```bash
make serve      # a node configured for this example, port 9279, foreground
make run        # the example, in another terminal
```

`make check` validates the scripts and every request body without a server;
`make clean` deletes what the example left behind. Copy `.env.example` to
`.env` to change the address or anything else.

The longer form, and what this example needs of the node:

```bash
./target/release/velosearch &
examples/19-search-pipelines/run.sh
```

Nothing beyond a default node: search pipelines need no setting to switch
them on.

## What to look for

- **Step 2 against step 4.** The same body, `size: 20`, a `multi_match` for
  `sea`. Without a pipeline: 12 hits, 4 of them `in_stock=False`, every price
  under `price_eur`. With `?search_pipeline=storefront`: 8 hits, all in stock,
  every price under `price`. The scores do not move -- Sea Glass is 1.130 in
  both -- because `filter_query` wraps the app's query in a `bool` as `must`
  and puts the stock term in `filter`, which does not score.
- **Step 5** returns 5 hits with a total of 8: the `script` processor lowered
  `size` from 20 to 5 before the search ran, so the total still counts every
  match. Book 1 is stored with formats `paperback,ebook,hardback` and comes
  back as `ebook,hardback,paperback`; the index is untouched.
- **Step 6 against step 7.** A carousel of four: without a pipeline, three of
  the four are by Ines Varga. With `one-per-author`, four authors -- Olu
  Adeyemi, Ines Varga, Tomas Reyes, Priya Nair. `oversample` fetched 12,
  `collapse` kept the best book of each author, and `truncate_hits`, given no
  number, cut back to the 4 that `oversample` had recorded as the original
  size.
- **Step 8** is the one that matters for the app: after one settings change
  the request carries no `search_pipeline` anywhere and still comes back
  filtered, capped and renamed -- 8 total, 5 returned.
- **Step 10** proves the inline pipeline *replaces* the default rather than
  adding to it: 5 fiction books, 2 of them sold out. Had the default run as
  well, the sold-out two would be gone.
- **Step 11** is a 400 naming `processor_type: rename_field` and
  `processor_tag: old-sku`. **Step 12** is the same pipeline with
  `ignore_failure` on that processor: 12 hits, and the second processor, the
  price rename, still ran.

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
| `data/` | 1 bulk document set |

## Leaves behind

The index `books`, with `index.search.default_pipeline` set to `storefront`,
and the search pipelines `storefront` and `one-per-author`. The pipeline
`last-year` is deleted by the example itself. Rerunning deletes all of them
first; `make clean` removes them.
