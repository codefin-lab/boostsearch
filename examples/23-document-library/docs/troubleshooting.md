# Troubleshooting

## A document comes back with no `attachment.content`

The processor did not recognise the file. `content_type` says what it thought
the file was; `application/octet-stream` means it could not tell. Run the file
through `_simulate` on its own to see:

```bash
curl -s localhost:9283/_ingest/pipeline/_simulate -H 'content-type: application/json' \
  -d "{\"pipeline\":{\"processors\":[{\"attachment\":{\"field\":\"data\"}}]},
       \"docs\":[{\"_source\":{\"data\":\"$(base64 < some-file | tr -d '\n')\"}}]}" | jq
```

Note the `tr -d '\n'`: `base64` on some systems wraps its output every 76
characters, and a newline inside a JSON string is not valid JSON.

## `field [data] is not a valid base64 value`

The bytes were sent as they are, or the base64 was URL-safe (`-` and `_` rather
than `+` and `/`). The processor wants standard base64.

## `field [data] doesn't exist`

The document was written without a file -- typically one that only needed a
field changed. `library-extract` requires `data`; that is deliberate, so a
document with no file is noticed. A reindex or `_update_by_query` of documents
already extracted should name a pipeline that does not read files, or
`pipeline=_none`: the final pipeline still runs, and removes nothing that is
not there.

## Every document fails at `dissect`

The file name does not follow `<department>_<kind>_<yyyy-MM-dd>_<slug>.<extension>`.
Step 6 is the quickest way to see where it stops:

```bash
curl -s 'localhost:9283/_ingest/pipeline/library/_simulate?verbose=true' \
  -H 'content-type: application/json' \
  -d '{"docs":[{"_source":{"file":{"name":"your file name here"},"data":"aGVsbG8K"}}]}' | jq
```

The two usual causes. A name with no underscores at all
(`travel-policy.txt`) fails with `Unable to find match for dissect pattern`. A
slug with a dot in it does not fail, which is worse: `dissect` splits at the
first `.` after the slug begins, so `policy-v1.2.txt` gives the slug `policy-v1`
and the extension `2.txt`. An underscore in the slug is harmless -- the last
key before `.` takes everything up to it -- but one in the department is not.

## A phrase that is in the file is not found

It is past `indexed_chars`. Compare `attachment.content_length` with the
ceiling in `requests/02-...`: if they are equal, the file was cut. See
`design.md` for `indexed_chars_field`.

## The draft is in the index

The index was created without `index.default_pipeline`, or the document was
written with `?pipeline=` naming another pipeline. The `drop` is in `library`,
not in the final pipeline. `GET /library/_settings` shows which pipelines the
index has.

## `_simulate` shows fields the indexed document does not have, or lacks some

`_simulate` runs the pipeline it is given and nothing else. The index's final
pipeline does not run in it, so `library.extracted` never appears there.

## A search matches the board pack for words from two different files

`attachments` is an `object` array, so its values are flattened into one list
per field. See "One document for a bundle, or one per file" in `design.md`.

## Cleaning up

```bash
make clean          # deletes the index library and the four pipelines
```
