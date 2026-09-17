# Settings

Every setting is spelled the way OpenSearch spells it, and can be given three
ways: in `config/velosearch.yml`, as a cluster setting where OpenSearch makes
it one, or in the environment as `VELOSEARCH_` followed by the dotted name
upper-cased with `_` for the dots. A node setting is a node setting here for
the same reason it is one there: it says what this process is allowed to do,
which is an operator's decision rather than a client's.

## Where it listens and what it keeps

| | |
|---|---|
| `VELOSEARCH_ADDR` | the address to listen on. Default `127.0.0.1:9200`. |
| `VELOSEARCH_DATA` | where indices live. Set, they are mmapped and survive a restart; unset, everything is in RAM and nothing is written down, which is what the test suite wants. |
| `VELOSEARCH_CONFIG` | where `velosearch.yml` and the plugins' data directories are looked for. Defaults to `config/` beside the binary, and `<data>/config`. |
| `VELOSEARCH_PATH_REPO` / `path.repo` | the root filesystem snapshot repositories live under. Default `<data>/repo`. A repository that tries to climb out of it is refused. |
| `VELOSEARCH_MAX_CONTENT_MB` | the largest request body accepted, in MiB. Default 100, which is `http.max_content_length`'s default. |

## What this node is

| | |
|---|---|
| `VELOSEARCH_NODE_ROLES` / `node.roles` | `cluster_manager`, `data`, `ingest`, `remote_cluster_client`. Default is all four. An empty list, `node.roles: []`, is a coordinating-only node. A node without `data` is never given a copy of a shard, and an index made on one is placed on a data node. A cluster with no ingest node refuses a write that names a pipeline, because there is nowhere to run it. |
| `VELOSEARCH_NODE_ATTRS` / `node.attr.*` | attributes as `name=value` pairs separated by commas, for allocation awareness and for anything that reads them back. |
| `node.name`, `network.host`, `transport.port` | as in OpenSearch. |
| `plugins.security.ssl.transport.enabled` | mutual TLS between nodes. With it, a peer is a peer because its certificate says so; without it, a node only listens for transport connections on loopback. |
| `plugins.security.ssl.transport.pemcert_filepath`, `...pemkey_filepath`, `...pemtrustedcas_filepath` | this node's certificate and key, and the authority every peer's certificate must chain to. Relative paths are read from the config directory. |
| `plugins.security.nodes_dn` | the certificate subjects allowed to be nodes, as glob patterns (`CN=*.nodes.example.com`). A certificate the same authority issued to a person is then not a node. Transport TLS without it is refused at startup, since without it every certificate that authority signed would be a node. |
| `VELOSEARCH_TRANSPORT_INSECURE` | `true` lets a node listen for transport connections on a non-loopback address with transport TLS off. It is an operator saying the network itself is the boundary. |
| `discovery.seed_hosts` | the nodes this one looks for. |

Two cluster settings say which of those attributes the cluster is aware of.
They are set through `PUT _cluster/settings` like any other, and are read by
`_cluster/routing/awareness/<attribute>/weights` and
`_cluster/decommission/awareness/<attribute>/<value>`: a request naming an
attribute neither of them knows is refused rather than stored.

| | |
|---|---|
| `cluster.routing.allocation.awareness.attributes` | the node attributes the cluster is aware of, comma-separated. Only these may have weights put for them, or be decommissioned. |
| `cluster.routing.allocation.awareness.force.<attribute>.values` | every value that attribute may take, comma-separated, whether or not a node carries it now. A weight has to be given for each of them, and only a value named here can be decommissioned. |

## What this node is allowed to reach

Nothing here can be set by a client, and nothing is allowed unless it is named.

| | |
|---|---|
| `VELOSEARCH_URL_ALLOWED` / `repositories.url.allowed_urls` | the URLs a `url` repository may be read from. `*` at the end of an entry stands for the rest of it. A `file://` repository is allowed by sitting under `path.repo` instead. |
| `VELOSEARCH_REINDEX_ALLOWLIST` / `reindex.remote.allowlist` | the clusters `_reindex` may read from, as `host:port` where either half may be `*`. |
| `VELOSEARCH_GEOIP_PATH` | the directory holding the MaxMind databases. See [geoip.md](geoip.md); they are not vendored. |
| `VELOSEARCH_PHONETIC_RULES` | the directory holding the Beider-Morse rule files. See [phonetic.md](phonetic.md); they are not vendored either. |

## Security

TLS and the rest are asked for the way the security plugin asks for them —
`plugins.security.*` in `velosearch.yml`, or the same name in the environment
without the `plugins.security.` prefix:

| | |
|---|---|
| `VELOSEARCH_SSL_HTTP_ENABLED` / `plugins.security.ssl.http.enabled` | TLS on the HTTP layer. |
| `plugins.security.ssl.http.pemcert_filepath` and friends | the certificate, its key and the authority, as files under the config directory. |
| `VELOSEARCH_DISABLED` / `plugins.security.disabled` | `false` turns security on. |
| `VELOSEARCH_INITIAL_ADMIN_PASSWORD` | the password of the first administrator, `admin`, as OpenSearch's `OPENSEARCH_INITIAL_ADMIN_PASSWORD`. Read only when the node has no saved configuration: the node saves a configuration with that one user, mapped to `all_access`, and reads that from then on. It must be at least 8 characters with an uppercase letter, a lowercase letter, a digit and a special character, or the node refuses to start. There is no default administrator and no demo user: a node with security on, no saved configuration and no password lets nobody in and answers `503 OpenSearch Security not initialized.` |
| `plugins.security.authcz.admin_dn`, `plugins.security.restapi.roles_enabled`, … | as in OpenSearch. |

Users, roles and role mappings are written through `_plugins/_security/api/*`.
The configuration is the cluster's, as the security index is in OpenSearch:
every change is made on the cluster manager, saved to its
`config/security/*.yml` as one generation, and answered only once the cluster
has committed it -- a change that could not be saved is a `500`, and a change
asked of a cluster with no manager a `503`. Every node keeps a copy in its own
`config/security`, and a node of a cluster lets nobody in by that copy until
it has taken the manager's: a node that restarts, or loses its manager, answers
`503` until it is following a manager again and holds its configuration. A
configuration file that is missing beside the others, unreadable or not YAML
is not replaced by anything; the node lets nobody in until it is put right.

## How hard it works

These are ours rather than OpenSearch's: they name the same trades its
thread-pool and buffer settings name, but they are not the same settings and
are not claimed to be.

| | |
|---|---|
| `VELOSEARCH_SEARCH_THREADS` | how many threads a search may spread over. Default: the machine's parallelism. |
| `VELOSEARCH_WRITER_THREADS` | how many threads an index writer uses. |
| `VELOSEARCH_WRITER_BUDGET_MB` | how much an index writer may hold before it must flush. |
| `VELOSEARCH_MAX_LIVE_WRITERS` | how many indices may hold a writer open at once. Past it, the least recently written is closed. |
| `VELOSEARCH_WRITER_IDLE_SECS` | how long a writer with nothing to do is kept before its memory is given back. |
| `VELOSEARCH_ISM_INTERVAL_MS` | how often index management looks at what it manages. Default is a job's own schedule. |

## Asynchronous search

Cluster settings, changed with `PUT _cluster/settings`. The first is the
plugin's own; the other three are ours, and bound what one node and one user
may hold.

| | |
|---|---|
| `plugins.asynchronous_search.node_concurrent_running_searches` | how many asynchronous searches a node runs at once. Default 20. A submit past it is refused with 429. |
| `plugins.asynchronous_search.user_concurrent_running_searches` | how many of those one user may run. Default 10. |
| `plugins.asynchronous_search.node_retained_bytes` | how many bytes of results a node keeps for `keep_on_completion`, on disk under `<data>/_state/asynchronous_search`. Default `256mb`. A result that does not fit is answered `PERSIST_FAILED`, and a submit asking to keep another is refused with 429 until one is let go. |
| `plugins.asynchronous_search.user_retained_bytes` | how many of those bytes one user's results may take. Default `128mb`. |

## For finding things out

Not for production: each one either slows the node down or makes it behave
badly on purpose.

| | |
|---|---|
| `VELOSEARCH_CHAOS` | drop and delay messages between nodes, to see what survives it. |
| `VELOSEARCH_CLUSTER_DEBUG`, `VELOSEARCH_AUTH_DEBUG` | say out loud what the coordinator and the authenticator are deciding. |
| `VELOSEARCH_CONSOLE_DEBUG` | the console says out loud each search it runs over its own index, and what it did to that index and why. |
| `VELOSEARCH_SERIAL_BULK` | run a bulk one line at a time, so a crash names the line. |
| `VELOSEARCH_NO_BLOCK_RANGE`, `VELOSEARCH_NO_BLOCK_SORT`, `VELOSEARCH_NO_KIND_NARROW` | turn off three optimisations, one at a time, to find out whether one of them is what made an answer wrong. |

## The console

The console is a second program — `velosearch-console` — because the
Dashboards server is one too: an engine and the console in front of it are deployed
apart as often as together, and a console that has to run beside its engine is
a worse console.

| | |
|---|---|
| `VELOSEARCH_CONSOLE_ADDR` | where to listen. Default `127.0.0.1:5601`, which is where OpenSearch Dashboards listens. |
| `VELOSEARCH_CONSOLE_PATH` | an OpenSearch Dashboards distribution: the built front end this serves. Pointed at rather than carried, the way the geoip databases are — it is the OpenSearch project's to publish and it is a gigabyte. In their container it is `/usr/share/opensearch-dashboards`. |
| `VELOSEARCH_CONSOLE_BASE_PATH` | the path everything is served under, for a console behind a proxy that gives it one. Empty by default. |
| `VELOSEARCH_ENGINE` | the engine behind it, which is where everything the console knows is kept. Default `http://127.0.0.1:9200`; credentials may be given in the URL. |
| `VELOSEARCH_CONSOLE_ANONYMOUS_STATUS` | whether `/api/status` and the status page answer without a sign-in. Default `true`, which is what the reference's own suite starts it with. |
| `VELOSEARCH_CONSOLE_XSRF` | whether a request that changes something must carry the `osd-xsrf` header. Default `true`; `false` is `server.xsrf.disableProtection` in the Node server, which its own API suite needs. |
| `VELOSEARCH_CONSOLE_PROXY_FILTER` | the engine paths the Dev Tools proxy carries, as regular expressions separated by commas; `console.proxyFilter` in the Node server. Default `.*`. |
| `VELOSEARCH_CONSOLE_COMPRESSION_REFERRERS` | the hosts a page may be embedded from and still get compressed answers, separated by commas; `server.compression.referrerWhitelist` in the Node server. Empty by default, which compresses for every referrer. |
| `VELOSEARCH_CONSOLE_OVERRIDE` | settings an operator fixes, as `key=value` pairs separated by commas. A reader is shown them as `isOverridden` and refused when they try to change one — an operator's decision is not a reader's to undo. A value is JSON where it reads as JSON and the text it is otherwise, so `false` is a boolean and `Asia/Bangkok` is a string. |

The distribution's version decides which pinned contract is read from
`console/`. A distribution with no pin beside it is refused at startup and says
so, rather than serving a page that names files which are not there.
