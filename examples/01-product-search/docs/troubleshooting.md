# Troubleshooting

## `no server at http://127.0.0.1:9261`

Nothing is listening. Start one:

```bash
make serve          # in another terminal; foreground, control-C to stop
```

Or point the example at a server you already have:

```bash
VS=http://127.0.0.1:9200 ./run.sh
```

## Step 3 returns nothing for "laptop"

The synonym filter did not run at index time. Two usual causes:

- the index was created before the analysis settings were edited -- settings
  under `analysis` are fixed at creation, so the index must be deleted and made
  again. `run.sh` deletes it at step 1, so this only happens if you edited the
  settings and reran a later step by hand;
- `synonym_graph` refused the list and the index creation failed. Check by
  asking what the analyser actually does:

  ```bash
  curl -s localhost:9261/shop-products/_analyze \
    -H 'content-type: application/json' \
    -d '{"analyzer":"shop_text","text":"laptop"}'
  ```

  If the output holds only `laptop`, the synonyms are not in the chain.

## Step 5 finds nothing for "meridan"

`fuzziness: AUTO` allows one edit for terms of 3-5 characters and two for
longer. `meridan` to `meridian` is one insertion, so it should match. If it
does not, the query is reaching a field whose analyser lower-cases differently
from the indexed text -- check `name` and `name.prefix` are both in the
`fields` list as written.

## Step 9 (completion suggester) returns an empty `options` array

The `suggest` field must be populated at index time, and it is not a normal
text field: it takes `{"input": [...], "weight": n}`. If documents were written
without it -- for instance by rerunning the bulk from a different source -- the
suggester has nothing to offer. Delete the index and rerun.

## Step 11 fails with `unable to find script [product-search]`

The stored script was not written, usually because step 1's `gone` deleted it
and the `PUT` at step 11 failed silently. Run it by hand and read the error:

```bash
curl -s -XGET localhost:9261/_scripts/product-search
```

## Everything works but the output is unreadable

Install `jq`, or pipe a single step:

```bash
curl -s localhost:9261/shop-products/_search \
  -H 'content-type: application/json' \
  --data-binary @requests/03-the-search-a-search-box-makes.json | jq .
```

## Cleaning up

```bash
make clean          # deletes shop-products
curl -XDELETE localhost:9261/_scripts/product-search
```
