# API surface -- 14. Changing a mapping on a live index

Every request this example makes, in the order it makes them. Generated
from `run.sh`; if the two disagree, `run.sh` is right.

| Step | Method | Path | Body |
|---|---|---|---|
| the index as it was written in a hurry, two years ago | `PUT` | `/people-v1` | `requests/01-the-index-as-it-was-written.json` |
|  | `POST` | `/people-v1/_bulk?refresh=true` | `/tmp/people.ndjson` |
|  | `GET` | `/_cat/count/people-v1?v` | inline |
| what is wrong with it: you cannot aggregate, sort or range over any of this | `GET` | `/people-v1/_search` | inline |
| the alias that should have been there from the start | `POST` | `/_aliases` | inline |
|  | `GET` | `/_cat/aliases/people?v` | inline |
| the index as it should be | `PUT` | `/people-v2` | inline |
| a pipeline that fixes what the mapping alone cannot | `PUT` | `/_ingest/pipeline/people-fix` | inline |
|  | `POST` | `/_ingest/pipeline/people-fix/_simulate` | inline |
| the reindex itself -- sliced, so it uses more than one core | `POST` | `/_reindex?wait_for_completion=true&refresh=true&slices=auto` | inline |
| the same counts, and the questions that were impossible before | `GET` | `/_cat/count/people-v2?v` | inline |
|  | `GET` | `/people-v2/_search` | inline |
| the swap: one atomic action, no window where the alias points at nothing | `POST` | `/_aliases` | inline |
|  | `GET` | `/people/_search` | inline |
| a filtered alias, so one index can look like several | `POST` | `/_aliases` | inline |
|  | `GET` | `/people-th/_count` | inline |
|  | `GET` | `/people/_count` | inline |
| fixing what the reindex could not know: a script over what is written | `POST` | `/people/_update_by_query?wait_for_completion=true&refresh=true&conflicts=proceed` | inline |
|  | `GET` | `/people/_search` | inline |
| and removing what should never have been there | `POST` | `/people/_delete_by_query?wait_for_completion=true&refresh=true&conflicts=proceed` | inline |
| shrinking four shards to one, for an index that stopped growing | `PUT` | `/people-v1/_settings` | inline |
|  | `POST` | `/people-v1/_shrink/people-small` | inline |
|  | `GET` | `/_cat/shards/people-small?v&h=index,shard,prirep,docs,state` | inline |
| and splitting the other way | `PUT` | `/people-v2/_settings` | inline |
|  | `POST` | `/people-v2/_split/people-v3` | inline |
|  | `PUT` | `/people-v2/_settings` | inline |
|  | `GET` | `/_cat/indices/people*?v&h=index,pri,rep,docs.count,store.size` | inline |
| reindexing from another cluster entirely | `POST` | `/_reindex?wait_for_completion=true` | inline |
| a long reindex runs as a task you can watch and cancel | `GET` | `/_tasks?actions=*reindex*&detailed=true` | inline |

## Endpoints touched

Path parameters and query strings removed, deduplicated:

- `/_aliases`
- `/_cat/aliases/people`
- `/_cat/count/people-v1`
- `/_cat/count/people-v2`
- `/_cat/indices/people*`
- `/_cat/shards/people-small`
- `/_ingest/pipeline/people-fix`
- `/_ingest/pipeline/people-fix/_simulate`
- `/_reindex`
- `/_tasks`
- `/people-th/_count`
- `/people-v1`
- `/people-v1/_bulk`
- `/people-v1/_search`
- `/people-v1/_settings`
- `/people-v1/_shrink/people-small`
- `/people-v2`
- `/people-v2/_search`
- `/people-v2/_settings`
- `/people-v2/_split/people-v3`
- `/people/_count`
- `/people/_delete_by_query`
- `/people/_search`
- `/people/_update_by_query`

## Request bodies

- [`requests/01-the-index-as-it-was-written.json`](../requests/01-the-index-as-it-was-written.json)
