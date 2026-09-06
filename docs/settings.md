# Settings

Every setting is spelled the way OpenSearch spells it, and can be given three
ways: in `config/boostsearch.yml`, as a cluster setting where OpenSearch makes
it one, or in the environment as `BOOSTSEARCH_` followed by the dotted name
upper-cased with `_` for the dots. A node setting is a node setting here for
the same reason it is one there: it says what this process is allowed to do,
which is an operator's decision rather than a client's.

## Where it listens and what it keeps

| | |
|---|---|
| `BOOSTSEARCH_ADDR` | the address to listen on. Default `127.0.0.1:9200`. |
| `BOOSTSEARCH_DATA` | where indices live. Set, they are mmapped and survive a restart; unset, everything is in RAM and nothing is written down, which is what the test suite wants. |
| `BOOSTSEARCH_CONFIG` | where `boostsearch.yml` and the plugins' data directories are looked for. Defaults to `config/` beside the binary, and `<data>/config`. |
| `BOOSTSEARCH_PATH_REPO` / `path.repo` | the root filesystem snapshot repositories live under. Default `<data>/repo`. A repository that tries to climb out of it is refused. |
| `BOOSTSEARCH_MAX_CONTENT_MB` | the largest request body accepted, in MiB. Default 100, which is `http.max_content_length`'s default. |

## What this node is

| | |
|---|---|
| `BOOSTSEARCH_NODE_ROLES` / `node.roles` | `cluster_manager`, `data`, `ingest`, `remote_cluster_client`. Default is all four. A cluster with no ingest node refuses a write that names a pipeline, because there is nowhere to run it. |
| `BOOSTSEARCH_NODE_ATTRS` / `node.attr.*` | attributes as `name=value` pairs separated by commas, for allocation awareness and for anything that reads them back. |
| `node.name`, `network.host`, `transport.port` | as in OpenSearch. |
| `discovery.seed_hosts` | the nodes this one looks for. |

## What this node is allowed to reach

Nothing here can be set by a client, and nothing is allowed unless it is named.

| | |
|---|---|
| `BOOSTSEARCH_URL_ALLOWED` / `repositories.url.allowed_urls` | the URLs a `url` repository may be read from. `*` at the end of an entry stands for the rest of it. A `file://` repository is allowed by sitting under `path.repo` instead. |
| `BOOSTSEARCH_REINDEX_ALLOWLIST` / `reindex.remote.allowlist` | the clusters `_reindex` may read from, as `host:port` where either half may be `*`. |
| `BOOSTSEARCH_GEOIP_PATH` | the directory holding the MaxMind databases. See [geoip.md](geoip.md); they are not vendored. |
| `BOOSTSEARCH_PHONETIC_RULES` | the directory holding the Beider-Morse rule files. See [phonetic.md](phonetic.md); they are not vendored either. |

## Security

TLS and the rest are asked for the way the security plugin asks for them —
`plugins.security.*` in `boostsearch.yml`, or the same name in the environment
without the `plugins.security.` prefix:

| | |
|---|---|
| `BOOSTSEARCH_SSL_HTTP_ENABLED` / `plugins.security.ssl.http.enabled` | TLS on the HTTP layer. |
| `plugins.security.ssl.http.pemcert_filepath` and friends | the certificate, its key and the authority, as files under the config directory. |
| `plugins.security.authcz.admin_dn`, `plugins.security.restapi.roles_enabled`, … | as in OpenSearch. |

Users, roles and role mappings live in the security index and are written
through `_plugins/_security/api/*`, not in a file.

## How hard it works

These are ours rather than OpenSearch's: they name the same trades its
thread-pool and buffer settings name, but they are not the same settings and
are not claimed to be.

| | |
|---|---|
| `BOOSTSEARCH_SEARCH_THREADS` | how many threads a search may spread over. Default: the machine's parallelism. |
| `BOOSTSEARCH_WRITER_THREADS` | how many threads an index writer uses. |
| `BOOSTSEARCH_WRITER_BUDGET_MB` | how much an index writer may hold before it must flush. |
| `BOOSTSEARCH_MAX_LIVE_WRITERS` | how many indices may hold a writer open at once. Past it, the least recently written is closed. |
| `BOOSTSEARCH_WRITER_IDLE_SECS` | how long a writer with nothing to do is kept before its memory is given back. |
| `BOOSTSEARCH_ISM_INTERVAL_MS` | how often index management looks at what it manages. Default is a job's own schedule. |

## For finding things out

Not for production: each one either slows the node down or makes it behave
badly on purpose.

| | |
|---|---|
| `BOOSTSEARCH_CHAOS` | drop and delay messages between nodes, to see what survives it. |
| `BOOSTSEARCH_CLUSTER_DEBUG`, `BOOSTSEARCH_AUTH_DEBUG` | say out loud what the coordinator and the authenticator are deciding. |
| `BOOSTSEARCH_SERIAL_BULK` | run a bulk one line at a time, so a crash names the line. |
| `BOOSTSEARCH_NO_BLOCK_RANGE`, `BOOSTSEARCH_NO_BLOCK_SORT`, `BOOSTSEARCH_NO_KIND_NARROW` | turn off three optimisations, one at a time, to find out whether one of them is what made an answer wrong. |

## The console

The console is a second program — `boostsearch-console` — because the one it
replaces is one too: an engine and the console in front of it are deployed
apart as often as together, and a console that has to run beside its engine is
a worse console.

| | |
|---|---|
| `BOOSTSEARCH_CONSOLE_ADDR` | where to listen. Default `127.0.0.1:5601`, which is where OpenSearch Dashboards listens. |
| `BOOSTSEARCH_CONSOLE_PATH` | an OpenSearch Dashboards distribution: the built front end this serves. Pointed at rather than carried, the way the geoip databases are — it is the OpenSearch project's to publish and it is a gigabyte. In their container it is `/usr/share/opensearch-dashboards`. |
| `BOOSTSEARCH_CONSOLE_BASE_PATH` | the path everything is served under, for a console behind a proxy that gives it one. Empty by default. |
| `BOOSTSEARCH_ENGINE` | the engine behind it, which is where everything the console knows is kept. Default `http://127.0.0.1:9200`; credentials may be given in the URL. |
| `BOOSTSEARCH_CONSOLE_OVERRIDE` | settings an operator fixes, as `key=value` pairs separated by commas. A reader is shown them as `isOverridden` and refused when they try to change one — an operator's decision is not a reader's to undo. A value is JSON where it reads as JSON and the text it is otherwise, so `false` is a boolean and `Asia/Bangkok` is a string. |

The distribution's version decides which pinned contract is read from
`console/`. A distribution with no pin beside it is refused at startup and says
so, rather than serving a page that names files which are not there.
