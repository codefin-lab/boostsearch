# Design notes

## What the client sends, and why so little

Each bulk item is a file name and the file's bytes, base64:

```json
{"file": {"name": "hr_policy_2026-03-02_travel-policy.txt"}, "data": "VHJhdmVsIHBvbGljeQo..."}
```

Anything more -- the department, the date, the text -- would have to be worked
out by every program that uploads a file, and kept the same in all of them. A
pipeline does it in one place, and changing it is a `PUT`. The cost is that the
text extraction runs on the node that indexes, competing with indexing for the
same CPU.

## Three pipelines, and what each is for

```
library          default    the file name, the draft rule, derived fields
  library-extract  called   the file to its text, and the base64 removed
library-bundle   by name    several files in one document, via foreach
library-final    final      runs after any of them, or after none
```

**Why the extraction is a pipeline of its own.** `library` and
`library-bundle` both read files. Putting the `attachment` settings -- how many
characters, which properties -- in one place means the two cannot drift apart.
`library-bundle` in fact carries its own `attachment` inside `foreach`, because
a processor inside `foreach` reads `_ingest._value` and a sub-pipeline would
read the top-level `data`; that is the one place the settings are repeated.

**Why the final pipeline exists.** `index.default_pipeline` is only a default:
`?pipeline=<name>` replaces it, and `?pipeline=_none` skips it. That is useful
(step 11 uses it for the bundle) and it is also how a document gets written
without anything being done to it. `index.final_pipeline` runs after the
request's pipeline, whichever it was, and there is no request parameter that
skips it. So the rules that must hold for every document go there:

- the base64 is removed. `library-extract` already removes it; the final
  pipeline removes it again with `ignore_missing`, so that a writer that
  bypassed extraction still does not store megabytes of base64 in `_source`;
- `library.extracted` records whether any text was read, so the documents that
  were not are one `term` query away (step 15).

The combination, as step 11 and step 12 show it:

| The request says | What runs |
|---|---|
| nothing | `library`, then `library-final` |
| `?pipeline=library-bundle` | `library-bundle`, then `library-final` |
| `?pipeline=_none` | `library-final` only |

## `dissect` for the file name, not `grok`

The name is `<department>_<kind>_<yyyy-MM-dd>_<slug>.<extension>`: the
delimiters are fixed and nothing is optional. That is what `dissect` is for --
a positional split, with no regular expression and nothing to backtrack. A
name that does not follow the convention fails the `dissect` processor, which
fails the document; that is deliberate, since a file with no department has no
shelf to go on. An `on_failure` redirecting it elsewhere, as example 07 does,
would be the next step.

## `drop` early

The `drop` is the second processor, straight after the name is taken apart and
before the file is read. A dropped document costs nothing after the processor
that drops it, and extracting the text of a draft only to throw it away is the
most expensive thing this pipeline could do. The bulk answer for a dropped item
is `result: noop` with status 200: not a failure, so a client does not retry it.

## `indexed_chars`, and what it costs

`indexed_chars` is a ceiling on how much of each file is read. It exists
because a 300-page PDF would otherwise become one enormous `text` field --
memory in the pipeline, a long analysis on the indexing thread, and a large
`_source`. The price is in step 14: anything past the ceiling is not in the
index, and a search for it finds nothing, with no sign that anything was
missed except `content_length` being exactly the ceiling.

This example sets 1000 so the effect can be seen in a small file. The default
is 100,000. `indexed_chars: -1` reads everything; `indexed_chars_field` names a
field in the document holding a per-document ceiling, which is how a few known
large files are read in full without raising the limit for everyone.

## `properties`

The processor can write `content`, `title`, `author`, `keywords`, `date`,
`content_type`, `content_length` and `language`. `library-extract` asks for
all but `keywords`, and each one it writes is in the mapping with a type chosen
for it: `author` and `content_type` are keywords to filter and count on,
`date` is a date. Asking only for what is mapped keeps dynamic mapping from
inventing a type for the rest.

## Why the base64 is removed rather than `excludes`d

`_source.excludes` in the mapping would also keep the base64 out of `_source`,
but it is still sent to the node, still parsed, and a reindex from that index
would have no file to read. Removing it in the pipeline says what is true: the
index holds the text, and the original file lives wherever the files live.

## One document for a bundle, or one per file

The board pack is one document with an `attachments` array, because the
question it answers is "which board pack mentions this?". The cost is that the
array is an `object` field, not `nested`: a query for `password` in one file
and `billing` in another file matches the pack even though no single file has
both. If the question is "which file", index one document per file with the
pack's name as a field -- which is what the rest of the library does.

## What would change at scale

- **Large files do not belong in a bulk request.** A 20 MB file is 27 MB of
  base64, all of which the node holds in memory while the pipeline runs. Keep
  bulk requests small by bytes, not by count.
- **Put ingest on nodes of its own** (`node.roles: [ingest]`) once extraction
  is a meaningful share of indexing time; text extraction is CPU that search
  and indexing would otherwise have.
- **The script processor compiles once** as long as its source does not change;
  it reads the title from the attachment and builds one from the slug where the
  file had none.
