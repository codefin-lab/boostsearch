# API surface -- 23. Files in, searchable text out

Every request this example makes, in the order it makes them. Generated
from `run.sh`; if the two disagree, `run.sh` is right.

| Step | Method | Path | Body |
|---|---|---|---|
| the processors this node has -- attachment among them, or nothing below works | `GET` | `/_nodes/ingest?filter_path=nodes.*.ingest.processors.type` | none |
| the attachment processor on its own: one file, read in full and as a preview | `POST` | `/_ingest/pipeline/_simulate` | `requests/01-the-attachment-processor-on-its-own.json` |
| the sub-pipeline that reads a file, and then throws the file away | `DELETE` | `/$IDX` | none (may be absent) |
|  | `DELETE` | `/_ingest/pipeline/library`, `library-extract`, `library-final`, `library-bundle` | none (may be absent) |
|  | `PUT` | `/_ingest/pipeline/library-extract` | `requests/02-the-sub-pipeline-that-reads-a.json` |
| the pipeline every file goes through: its name taken apart, drafts dropped, fields derived | `PUT` | `/_ingest/pipeline/library` | `requests/03-the-pipeline-every-file-goes-through.json` |
| the final pipeline: the one a writer cannot skip | `PUT` | `/_ingest/pipeline/library-final` | `requests/04-the-final-pipeline-the-one-a.json` |
| one file, stepped through processor by processor, and a draft that is dropped | `POST` | `/_ingest/pipeline/library/_simulate?verbose=true` | `requests/05-one-file-stepped-through-processor-by.json` |
| the index, with a default pipeline and a final one | `PUT` | `/$IDX` | `requests/06-the-index-with-a-default-and.json` |
|  | `GET` | `/_cluster/health/$IDX?wait_for_status=yellow&timeout=5s` | none |
| four files in a bulk request, with nothing but a name and base64 in each | `POST` | `/$IDX/_bulk?refresh=wait_for` | built by `as_bulk` in `run.sh` from `data/files/*.txt` and `data/files/*.docx` |
|  | `GET` | `/$IDX/_count` | none |
| what came out of each file | `GET` | `/$IDX/_search` | inline |
| the Word document's own title, author and date, read from inside the file | `GET` | `/$IDX/_doc/it_handbook_2026-01-15_security-handbook?_source_includes=...` | none |
|  | `GET` | `/$IDX/_search` | inline (`term` on `attachment.author`) |
| a bundle: several files in one document, read by foreach | `PUT` | `/_ingest/pipeline/library-bundle` | `requests/07-a-bundle-several-files-in-one.json` |
|  | `PUT` | `/$IDX/_doc/board_pack_2026-02-10_february-meeting?pipeline=library-bundle&refresh=wait_for` | built by `as_bundle` in `run.sh` from `data/files/board-pack/` |
|  | `GET` | `/$IDX/_doc/board_pack_2026-02-10_february-meeting?_source_excludes=attachments.attachment.content` | none |
|  | `GET` | `/$IDX/_count` | none |
| a writer that skips the default pipeline, and what the final one does about it | `PUT` | `/$IDX/_doc/bypassed?pipeline=_none&refresh=wait_for` | inline |
|  | `GET` | `/$IDX/_doc/bypassed` | none |
|  | `GET` | `/$IDX/_count` | none |
|  | `GET` | `/$IDX/_search` | inline (`term` on `library.extracted`) |
| the words inside the files, highlighted where they were found | `GET` | `/$IDX/_search` | `requests/08-the-words-inside-the-files-highlighted.json` (sent twice: printed, then counted) |
| a phrase from the last paragraph of the annual report, past indexed_chars | `GET` | `/$IDX/_search` | `requests/09-a-phrase-past-indexed-chars.json` (sent twice: printed, then counted) |
|  | `GET` | `/$IDX/_search` | inline (`match_phrase` inside the limit) |
| what the library holds: shelves, file types, and what was never extracted | `GET` | `/$IDX/_search` | `requests/10-what-the-library-holds.json` |
| the pipelines that exist | `GET` | `/_ingest/pipeline/library*` | none |

`$IDX` is `library`. Every run also starts with a `GET /` from `lib.sh`, to
check a server is there.

## Endpoints touched

Path parameters and query strings removed, deduplicated:

- `/`
- `/<var>`
- `/<var>/_bulk`
- `/<var>/_count`
- `/<var>/_doc/<id>`
- `/<var>/_search`
- `/_cluster/health/<var>`
- `/_ingest/pipeline/<name>`
- `/_ingest/pipeline/<name>/_simulate`
- `/_ingest/pipeline/_simulate`
- `/_ingest/pipeline/library*`
- `/_nodes/ingest`

## Request bodies

- [`requests/01-the-attachment-processor-on-its-own.json`](../requests/01-the-attachment-processor-on-its-own.json)
- [`requests/02-the-sub-pipeline-that-reads-a.json`](../requests/02-the-sub-pipeline-that-reads-a.json)
- [`requests/03-the-pipeline-every-file-goes-through.json`](../requests/03-the-pipeline-every-file-goes-through.json)
- [`requests/04-the-final-pipeline-the-one-a.json`](../requests/04-the-final-pipeline-the-one-a.json)
- [`requests/05-one-file-stepped-through-processor-by.json`](../requests/05-one-file-stepped-through-processor-by.json)
- [`requests/06-the-index-with-a-default-and.json`](../requests/06-the-index-with-a-default-and.json)
- [`requests/07-a-bundle-several-files-in-one.json`](../requests/07-a-bundle-several-files-in-one.json)
- [`requests/08-the-words-inside-the-files-highlighted.json`](../requests/08-the-words-inside-the-files-highlighted.json)
- [`requests/09-a-phrase-past-indexed-chars.json`](../requests/09-a-phrase-past-indexed-chars.json)
- [`requests/10-what-the-library-holds.json`](../requests/10-what-the-library-holds.json)

## Files sent as base64

- [`data/files/finance_report_2025-12-31_annual-report.txt`](../data/files/finance_report_2025-12-31_annual-report.txt)
- [`data/files/hr_draft_2026-04-01_parental-leave.txt`](../data/files/hr_draft_2026-04-01_parental-leave.txt) -- dropped
- [`data/files/hr_policy_2026-03-02_travel-policy.txt`](../data/files/hr_policy_2026-03-02_travel-policy.txt)
- [`data/files/it_handbook_2026-01-15_security-handbook.docx`](../data/files/it_handbook_2026-01-15_security-handbook.docx)
- [`data/files/board-pack/agenda.txt`](../data/files/board-pack/agenda.txt)
- [`data/files/board-pack/risk-register.docx`](../data/files/board-pack/risk-register.docx)
