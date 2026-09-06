# Replacing OpenSearch, and moving between BoostSearch versions

Two different things are called an upgrade here. One is putting BoostSearch
where an OpenSearch cluster is now. The other is moving a BoostSearch cluster
from one version of itself to the next. This is both, in that order.

## Part one: replacing an OpenSearch you already run

The claim is that BoostSearch answers what OpenSearch answers. The claim is
worth exactly as much as the check, so the check comes first and the cutover
comes after it.

### 1. Find out what your cluster actually uses

```bash
python3 tools/compat_audit.py inventory \
    --cluster https://opensearch.example:9200 \
    --engine  http://boostsearch.example:9200
```

This reads the mappings, settings and analysis of every index in the cluster
and reports anything BoostSearch does not answer for: a field type, an
analyzer, a tokenizer, a filter. It writes `compat-inventory.json`, which is
the list to work through. It changes nothing on either side.

An empty report is not the same as a passing test. It says nothing your
indices *declare* is unanswered; it says nothing about the requests your
application makes.

### 2. Ask both engines the same questions

```bash
python3 tools/compat_audit.py corpus
python3 tools/compat_audit.py replay \
    --requests compat-corpus.ndjson \
    --a https://opensearch.example:9200 \
    --b http://boostsearch.example:9200 \
    --scores
```

`corpus` writes 183 canonical requests — the shapes an application actually
sends. `replay` sends each one to both engines and diffs the JSON that comes
back, with `--scores` comparing the order and the scores of the hits as well
as the documents. Anything that differs is printed as a diff.

Better than the canonical corpus is your own traffic: `replay` will take any
newline-delimited file of `{"method":…,"path":…,"body":…}`, so a day of real
requests is a far stronger check than 183 invented ones.

### 3. Move the data

There is no in-place conversion, and there should not be: the two engines
write different files, and a converter would be a second implementation of
both formats with nothing checking it. Move the documents instead, by one of:

- **Reindex from the old cluster**, which is one request per index:

  ```bash
  curl -XPOST $BOOSTSEARCH/_reindex -H 'content-type: application/json' -d '{
    "source": {"remote": {"host": "https://opensearch.example:9200",
                          "username": "…", "password": "…"},
               "index": "logs-2026.01"},
    "dest":   {"index": "logs-2026.01"}}'
  ```

  The node must be told it may read from that host — `reindex.remote.allowlist`
  in [settings.md](settings.md) — and the destination's mapping should be
  created first, from the source's own mapping, so that dynamic mapping does
  not have to guess.

- **Snapshot and restore**, where both clusters can reach one repository. A
  BoostSearch snapshot holds each index's mapping, settings and documents as
  they were written rather than somebody else's segment files, which is what
  makes it indifferent to which engine wrote it.

- **Re-index from the source of truth**, if you still have one. Slowest, and
  the only one that also fixes whatever was wrong with the old mapping.

### 4. Cut over with a way back

Run both, send reads to both and compare, then send writes to both, then stop
writing to the old one, then stop reading from it. Keep it until you are sure.
Nothing about this is specific to BoostSearch; it is what you would do for any
engine replacement, and it is the part that makes step 2's diff mean
something.

### What will not come across

- **Anything from a plugin this does not answer for.** `_cat/plugins` lists
  what it does.
- **The security index.** Users, roles and role mappings are written through
  `_plugins/_security/api/*`; export them from the old cluster and put them
  back through the API rather than copying the index.
- **Segment-level things.** Force-merge state, segment counts, and anything
  that reads `_segments` will differ, because the segments are different.

## Part two: moving between BoostSearch versions

### What is guaranteed

- **An index written by one version is read by the next.** The on-disk format
  carries its own version, and an index already on disk keeps the codec
  settings it was written with — a change to the default reaches new segments
  only.
- **Two adjacent versions can be in one cluster at once**, which is what makes
  a rolling upgrade possible at all.
- **A snapshot written by one version restores into any later one.** It holds
  documents rather than segments, so this costs a re-index on restore and buys
  not caring which version wrote it.

### The procedure

One node at a time, waiting for green between each:

1. Disable shard allocation so a node going down does not start a rebalance:
   `PUT _cluster/settings {"persistent":{"cluster.routing.allocation.enable":"primaries"}}`
2. Stop the node, replace the binary, start it.
3. Wait for the cluster to be green again.
4. Repeat for every node.
5. Turn allocation back on: `"enable": null`.

`tools/rolling_upgrade.py` does exactly this against a local cluster, with
writers and readers running throughout, and says whether the mixed-version
cluster kept answering and kept every acknowledged write:

```bash
tools/rolling_upgrade.py --from ./target/release/boostsearch --to ./build/new/boostsearch
```

Given the same binary twice it is a rolling restart, which is the same test
with nothing to upgrade — worth running before the upgrade, so that a failure
during one is known to be about the new version.

### Going back

Downgrading is not supported, and the reason is the ordinary one: a newer
version may have written something an older one cannot read. The way back from
a bad upgrade is the snapshot taken before it, which is why the first step of
any upgrade is taking one.
