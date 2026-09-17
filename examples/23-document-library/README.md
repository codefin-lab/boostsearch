# 23. Files in, searchable text out

A company keeps its policies, handbooks and reports as files: a Word document
here, a text file there, a board pack that is several files at once. The files
are named by a convention -- `hr_policy_2026-03-02_travel-policy.txt` -- and
the name says more about the file than anything else does. What should come
out is a document per file that can be found by the words inside it, with the
department, the kind and the date read from the name, and without the file
itself sitting in `_source` as base64 for ever after.

The client in this example knows nothing about any of that. It sends a file
name and the file's bytes, base64. Everything else is done by pipelines.

Example 07 takes log lines apart with `grok`, `geoip` and `user_agent`; this
one is about files, and about how pipelines are put together: one calling
another, a default one a writer can replace, and a final one it cannot.

## What it shows

| Step | Feature |
|---|---|
| 1 | `_nodes/ingest` -- which processors the node has, `attachment` among them |
| 2 | the `attachment` processor in `_simulate`: `target_field`, `indexed_chars`, `properties` |
| 3 | a pipeline that reads the file and then `remove`s the base64 |
| 4 | `dissect` on a file-name convention, `drop` with `if`, `date`, a `pipeline` processor calling the one from step 3, `script`, `set` with a template |
| 5 | a final pipeline: `remove` with `ignore_missing`, and a `script` that records whether anything was extracted |
| 6 | `_ingest/pipeline/<name>/_simulate?verbose=true` -- every processor's effect, the sub-pipeline's included, and a document dropped |
| 7 | `index.default_pipeline` and `index.final_pipeline` on one index |
| 8 | a bulk of base64 files; the draft comes back `result: noop` |
| 10 | what a Word file says about itself: `title`, `author`, `date`, `content_type` |
| 11 | `foreach` over an array of files, `_ingest._value`, and `?pipeline=` replacing the default pipeline |
| 12 | `?pipeline=_none`, and the final pipeline running anyway |
| 13 | a `multi_match` over the extracted text, with `highlight` |
| 14 | what `indexed_chars` costs: a phrase past the limit is not there to find |
| 15 | aggregations over fields derived from the file name |

## Running it

```bash
make serve      # a node configured for this example, port 9283, foreground
make run        # the example, in another terminal
```

`make check` validates the script, every request body and the files in
`data/files/` without a server; `make clean` deletes the index and the four
pipelines. Copy `.env.example` to `.env` to change the address.

The longer form. The node needs nothing special: the `attachment` processor is
built in.

```bash
./target/release/velosearch &
VS=http://127.0.0.1:9200 examples/23-document-library/run.sh
```

## What to look for

- **Step 2** reads one file twice in one pipeline. The first `attachment`
  processor writes everything it knows; the second has `indexed_chars: 20` and
  `properties: ["content", "content_length"]`, and writes exactly that:

  ```json
  "attachment": {"content_type": "text/plain; charset=ISO-8859-1", "language": "en",
                 "content": "Rotate every password every ninety days, and report a lost laptop within the hour.",
                 "content_length": 84},
  "preview":    {"content": "Rotate every passwor", "content_length": 20}
  ```

- **Step 6** is the loop to build a pipeline in. With `verbose=true` every
  processor reports the document as it left it. The `pipeline` processor
  appears as a line of its own, `{"processor_type": "pipeline", "status":
  "success"}`, followed by the sub-pipeline's `attachment` and `remove`, each
  carrying `"_ingest": {"pipeline": "library-extract"}` so it is clear whose
  processor it was. The second document, a draft, stops two lines in:

  ```json
  {"processor_type": "drop", "description": "a draft is not published, so it is not in the library",
   "if": {"condition": "ctx.file.kind == 'draft'", "result": true}, "status": "dropped"}
  ```

- **Step 8** sends four files and gets three documents. The draft is not an
  error: its bulk item answers `"result": "noop", "status": 200`, so a client
  that retries failures will not retry it.
- **Step 10** is the Word file's own metadata, read from `docProps/core.xml`
  inside the zip: `"title": "Security handbook", "author": "Priya Nair", "date":
  "2026-01-15T09:00:00Z"`. The two text files have no title of their own, so the
  `script` in step 4 makes one from the slug: `travel-policy` becomes `Travel
  policy`.
- **Step 11 and step 12** are how the two index-level pipelines combine. The
  board pack is written with `?pipeline=library-bundle`, which *replaces* the
  default pipeline for that one request; the final pipeline still runs after
  it, and marks the document `"extracted": true`. The document written with
  `?pipeline=_none` skipped the default pipeline altogether -- and still comes
  back with its base64 removed:

  ```json
  "_source": {"file": {"name": "ops_runbook_2026-06-01_on-call.txt"}, "library": {"extracted": false}}
  ```

- **Step 13** finds `password` in two documents: in the handbook's
  `attachment.content`, highlighted as `Use a [password] manager, and never
  reuse a [password] between two services`, and in the board pack, where it is
  in the second file of the `attachments` array.
- **Step 14** is the price of `indexed_chars: 1000`. The annual report is 1,395
  characters long; `content_length` says `1000`, a phrase from its fourth
  paragraph is found, and `reserve fund`, in its last, is not found at all.
- **Step 15** counts shelves (`board/pack`, `finance/report`, `hr/policy`,
  `it/handbook`, one each) that no client ever sent, and `not_extracted:
  {"doc_count": 1}` -- the list of documents to go back to.

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
| `data/files/` | the files: three text files and a Word document, one of them a draft |
| `data/files/board-pack/` | a text file and a Word document sent together as one document |
| `data/make-docx.py` | how the two Word files were made; they are committed, so it need not be run |

## Leaves behind

The index `library`, holding five documents, and the ingest pipelines
`library`, `library-extract`, `library-final` and `library-bundle`. Rerunning
deletes and replaces all of them; `make clean` removes them.
