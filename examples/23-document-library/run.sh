#!/usr/bin/env bash
# Files in, searchable text out: a Word document and a few text files, sent as
# base64, read by the attachment processor, and found again by what they say.
source "$(dirname "$0")/lib.sh"
IDX=library

# as_bulk FILE... -- one bulk item per file: its name, and its bytes as base64.
# This is all a client has to know about a file; the rest is the pipeline's.
as_bulk() {
  python3 - "$@" <<'PY'
import base64, json, os, sys
for path in sys.argv[1:]:
    name = os.path.basename(path)
    print(json.dumps({"index": {"_id": name.rsplit(".", 1)[0]}}))
    with open(path, "rb") as f:
        print(json.dumps({"file": {"name": name}, "data": base64.b64encode(f.read()).decode()}))
PY
}

# as_bundle NAME FILE... -- one document carrying several files in an array
as_bundle() {
  python3 - "$@" <<'PY'
import base64, json, os, sys
name, paths = sys.argv[1], sys.argv[2:]
items = []
for path in paths:
    with open(path, "rb") as f:
        items.append({"name": os.path.basename(path), "data": base64.b64encode(f.read()).decode()})
print(json.dumps({"file": {"name": name}, "attachments": items}))
PY
}

step "the processors this node has -- attachment among them, or nothing below works"
req GET "/_nodes/ingest?filter_path=nodes.*.ingest.processors.type" | clip 700

step "the attachment processor on its own: one file, read in full and as a preview"
reqf POST "/_ingest/pipeline/_simulate" requests/01-the-attachment-processor-on-its-own.json
note "the second read stops at indexed_chars 20 and writes only the two properties asked for"

step "the sub-pipeline that reads a file, and then throws the file away"
gone "/$IDX"
for p in library library-extract library-final library-bundle; do gone "/_ingest/pipeline/$p"; done
reqf PUT "/_ingest/pipeline/library-extract" requests/02-the-sub-pipeline-that-reads-a.json

step "the pipeline every file goes through: its name taken apart, drafts dropped, fields derived"
reqf PUT "/_ingest/pipeline/library" requests/03-the-pipeline-every-file-goes-through.json

step "the final pipeline: the one a writer cannot skip"
reqf PUT "/_ingest/pipeline/library-final" requests/04-the-final-pipeline-the-one-a.json

step "one file, stepped through processor by processor, and a draft that is dropped"
reqf POST "/_ingest/pipeline/library/_simulate?verbose=true" requests/05-one-file-stepped-through-processor-by.json
note "the pipeline processor's own steps appear in the list too; the draft stops at drop"
note "_simulate runs the pipeline it is given -- it does not know about any index's final pipeline"

step "the index, with a default pipeline and a final one"
reqf PUT "/$IDX" requests/06-the-index-with-a-default-and.json
green "$IDX"

step "four files in a bulk request, with nothing but a name and base64 in each"
as_bulk data/files/*.txt data/files/*.docx | bulk "/$IDX/_bulk?refresh=wait_for"
note "the draft answers result: noop -- dropped by the pipeline, not refused"
expect_docs "$IDX" 3 "four files, one of them a draft"

step "what came out of each file"
req GET "/$IDX/_search" '{
  "size": 10, "sort": [{ "published_at": "asc" }],
  "_source": { "excludes": ["attachment.content"] }
}'

step "the Word document's own title, author and date, read from inside the file"
req GET "/$IDX/_doc/it_handbook_2026-01-15_security-handbook?_source_includes=title,attachment.title,attachment.author,attachment.date,attachment.content_type,attachment.content"
expect_hits 1 GET "/$IDX/_search" '{ "query": { "term": { "attachment.author": "Priya Nair" } } }' \
  "the author is a keyword now, found by an exact term"

step "a bundle: several files in one document, read by foreach"
reqf PUT "/_ingest/pipeline/library-bundle" requests/07-a-bundle-several-files-in-one.json
as_bundle board_pack_2026-02-10_february-meeting data/files/board-pack/agenda.txt data/files/board-pack/risk-register.docx \
  | "${CURL[@]}" -X PUT "$BS/$IDX/_doc/board_pack_2026-02-10_february-meeting?pipeline=library-bundle&refresh=wait_for" \
      -H 'Content-Type: application/json' --data-binary @-
echo
req GET "/$IDX/_doc/board_pack_2026-02-10_february-meeting?_source_excludes=attachments.attachment.content"
note "?pipeline= replaced the default pipeline for this request; the final pipeline still ran: library.extracted is true"
expect_docs "$IDX" 4 "the bundle is one document, however many files it carries"

step "a writer that skips the default pipeline, and what the final one does about it"
req PUT "/$IDX/_doc/bypassed?pipeline=_none&refresh=wait_for" '{
  "file": { "name": "ops_runbook_2026-06-01_on-call.txt" },
  "data": "T24tY2FsbCBydW5ib29rCg=="
}'
req GET "/$IDX/_doc/bypassed"
note "no attachment was read -- but the base64 is gone and the document says it was not extracted"
expect_docs "$IDX" 5
expect_hits 1 GET "/$IDX/_search" '{ "query": { "term": { "library.extracted": false } } }' \
  "the one document to go back and reprocess"

step "the words inside the files, highlighted where they were found"
reqf GET "/$IDX/_search" requests/08-the-words-inside-the-files-highlighted.json
expect_hits 2 GET "/$IDX/_search" "$(cat requests/08-the-words-inside-the-files-highlighted.json)" \
  "the Word handbook, and the Word file inside the board pack"
note "the board pack is found by the risk register, the second file in its attachments array"

step "a phrase from the last paragraph of the annual report, past indexed_chars"
reqf GET "/$IDX/_search" requests/09-a-phrase-past-indexed-chars.json
expect_hits 0 GET "/$IDX/_search" "$(cat requests/09-a-phrase-past-indexed-chars.json)" \
  "only the first 1000 characters of each file were read"
expect_hits 1 GET "/$IDX/_search" '{ "query": { "match_phrase": { "attachment.content": "graduate programme" } } }' \
  "a phrase from the fourth paragraph, inside the limit, is found"

step "what the library holds: shelves, file types, and what was never extracted"
reqf GET "/$IDX/_search" requests/10-what-the-library-holds.json

step "the pipelines that exist"
req GET "/_ingest/pipeline/library*" | clip 400

step "what this example leaves behind, checked rather than assumed"
done_
