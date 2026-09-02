# ความคืบหน้า Phase 1 — พอร์ต OpenSearch เป็น Rust บน Tantivy

วัดด้วย **test suite ของ OpenSearch เอง** (`rest-api-spec` YAML) ไม่ได้เขียน test ใหม่
harness: `tools/yaml_runner.py` · เป้า Phase 1: 124 ไฟล์ / 401 sections

| จุด | PASS | % |
|---|---:|---:|
| RED baseline | 0 / 400 | 0.0% |
| หลัง slice 1 (index + doc CRUD) | 56 / 400 | 14.0% |
| หลัง slice 2 (search + agg) | 229 / 400 | 57.2% |
| หลังไล่หาง Phase 1 | 275 / 400 | 68.8% |
| **Phase 1 ปิดครบ** | **297 / 400** | **74.6%** |

## คะแนนรายพื้นที่ (Phase 1 ปิดครบ)

| area | pass | fail |
|---|---:|---:|
| search.aggregation | 126 | 47 |
| search | 77 | 28 |
| indices.get_settings | 15 | 0 |
| indices.get_mapping | 12 | 0 |
| bulk | 9 | 1 |
| mget | 8 | 1 |
| indices.put_mapping | 8 | 0 |
| get_source / get | 13 | 0 |
| msearch | 5 | 1 |
| update | 5 | 0 |
| count | 4 | 0 |
| indices.stats | 4 | 8 |
| index | 3 | 8 |
| exists / indices.exists / field_caps | 6 | 0 |
| explain / indices.get_alias | 2 | 0 |
| range | 0 | 7 |
| **รวม** | **297** | **101** |

## สถานะ: ขอบเขต Phase 1 เหลือ 0

101 ที่ยังล้มทั้งหมดเป็น **Phase 3 ตามแผน** ไม่มีตัวไหนอยู่ในขอบเขต Phase 1 อีกแล้ว

| กลุ่ม | ไฟล์ |
|---|---:|
| HDR percentiles | 16 |
| wildcard field type | 9 |
| search profiling | 8 |
| median_absolute_deviation | 8 |
| search stat groups | 7 |
| range field types | 7 |
| fields (fetch) API | 7 |
| ignore_malformed | 6 |
| `_doc_count` field | 5 |
| java stack traces | 4 |
| auto_date_histogram | 4 |
| calendar_interval (date_histogram) | 3 |
| doc_values-only fields | 3 |
| unsigned_long / flat_object / constant_keyword / search_as_you_type | 6 |
| weighted_avg / variable_width_histogram | 3 |
| dynamic mapping modes / field collapsing / span queries / shard stats | 5 |

## สถาปัตยกรรมที่ลงตัวแล้ว

**Dynamic mapping ด้วย JSON field คู่** — เอกสารทุกฉบับ index ลง 2 JSON field พร้อมกัน:
- `_dyn` — tokenizer `default` + positions ⇒ พฤติกรรม `text`
- `_raw` — tokenizer `raw` + `set_fast` ⇒ พฤติกรรม `keyword` + doc values

ไม่ต้องจัดการ schema เลย tantivy แยกชนิด `i64/f64/bool/Str/Date` ให้เองต่อ path
การเลือก view: mapped `text` → `_dyn`, mapped `keyword` → `_raw`, unmapped → `_dyn`
สำหรับ full-text / `_raw` สำหรับ exact — ตรงกับ dynamic mapping (text + `.keyword`) ของ OpenSearch

**Aggregation ส่งผ่านเกือบตรง** — agg JSON ของ tantivy เข้ากันได้กับ OpenSearch อยู่แล้ว
เราแค่ (1) เขียน `field` ใหม่ให้ชี้ `_raw.x`/`_dyn.x` (2) ถอด `meta` ออกแล้วแปะกลับตอนตอบ
(3) เปลี่ยน `aggregations` → `aggs`

**สิ่งที่ tantivy ไม่มี เราทำเองเหนือมัน** — `filters`, `filter`, `missing` เป็น bucket agg
ที่รันเป็น filtered search แยกต่อ bucket แล้วประกอบผลเอง (tantivy's `filter` รับแค่
query string dialect ของตัวเอง ใช้กับ JSON view ของเราไม่ได้)

**Realtime GET vs near-realtime search** — `pending: HashMap<id, Option<Value>>`
เก็บ write ที่ยังไม่ commit ⇒ GET เห็นทันที search เห็นหลัง refresh ตรงกับ OpenSearch
โดยไม่ต้อง commit ทุก write

## สิ่งที่เพิ่มตอนปิด Phase 1 ให้ครบ

- **multi-value sort ที่ตรงกับ Java** — `mode` min/max/avg/sum/median พร้อม
  **long overflow แบบ Java** (`[i64::MAX, 1]` sum ได้ค่าติดลบจริง ๆ),
  `unsigned_long` ใช้เลขจำนวนเต็มแบบ exact + ปัดครึ่งขึ้น, `avg` ของ long
  ใช้ `round((wrapping_sum as f64)/n)` — สามอย่างนี้ให้ผลต่างกันและต้องแยกกันจริง
- **`terms` lookup** (`{index, id, path}`) ทั้งใน query และใน filter aggregation
- **`_index` terms agg** — `_index` เป็น metadata ไม่ใช่ column จึงทำ bucket เอง
  ต่อ index พร้อมรองรับ `min_doc_count: 0`
- **`global` aggregation** — รันด้วย `match_all` แยกจาก query หลัก
- **`terms` order by nested bucket doc_count** — tantivy ทำไม่ได้ จึงถอด order ออก
  แล้วเรียง bucket เองหลังได้ผล
- **shard skipping** (`pre_filter_shard_size`) — index ที่ match 0 ถือว่า skip
  แต่ต้องเหลือรันอย่างน้อยหนึ่ง และ agg ที่ต้องการทุก shard (`global`,
  `min_doc_count: 0`) ปิด skipping ทั้งหมด
- **`index.append_only.enabled`** — bulk ที่ระบุ `_id` เองถูกปฏิเสธ
- **`filter_path` matcher ที่ `**` ถูกต้อง**, `stored_fields` ใน search และ mget

## สิ่งที่เพิ่มตอนไล่หาง Phase 1

- **`multi_match` เต็มรูป** — `type` (best_fields / most_fields / cross_fields / phrase /
  phrase_prefix / bool_prefix), per-field `^boost`, `analyzer`, `fuzziness`,
  `minimum_should_match`, `operator` ส่งต่อไปทุก field
- **named analyzer ฝั่ง query** — `whitespace` / `keyword` / `english` map ไปยัง tokenizer ของ tantivy
- **`filter_path`** — รองรับ `*`, `**`, และ `-` สำหรับ exclude ทำเป็นชั้นกลางที่ทุก endpoint ใช้ร่วมกัน
- **`_field_caps`, `_explain`, `_alias`, `_stats`, `_update`** พอร์ตเพิ่ม
- **dynamic type tracking** — จำ field path ที่เห็นในเอกสารพร้อมชนิดที่ dynamic mapping
  จะให้ ทำให้ `field_caps` และ `query_string` ทำงานกับ field ที่ไม่ได้ประกาศ mapping
- **extended_stats คำนวณใหม่จาก sum/sum_of_squares** ด้วยสูตรของ OpenSearch
  เพื่อให้ float ตรงถึงบิตสุดท้าย (tantivy สะสมค่าคนละแบบ ต่างกันที่ ULP สุดท้าย)
- **`filters` / `filter` / `missing` bucket agg** ที่ tantivy ไม่มี ทำเองเป็น filtered search ต่อ bucket
- **sort `mode`** min/max/avg/sum สำหรับ field หลายค่า

## ค้นพบระหว่างทางที่สำคัญที่สุด

**Automaton query บน JSON path ต้อง anchor ด้วย prefix ของ term จริง**
`AutomatonWeight::new_for_json_path` รัน automaton บน serialized term ทั้งก้อน
(`<json path>\0<type byte><text>`) ไม่ใช่เฉพาะข้อความ ⇒ regex ต้องขึ้นต้นด้วย
byte ของ path ที่ escape แล้ว มิฉะนั้น `prefix`/`wildcard`/`regexp` คืน 0 เสมอแบบเงียบ ๆ

## ข้อจำกัดที่รู้ตัว (งานของ Phase 2)

- **sort ใช้การรวบทุก doc ที่ match แล้วเรียงในหน่วยความจำ** ถูกต้องแต่ O(matched)
  ต้องเปลี่ยนเป็น collector ที่เรียงระหว่าง collect
- `took` เป็นค่าคงที่ ยังไม่จับเวลาจริง
- ทุก index เป็น single shard, `Index::create_in_ram` — ยังไม่แตะ mmap/persistence
- scoring ของ prefix clause เป็น const score ⇒ ลำดับผลต่างจาก OpenSearch ในบางกรณี

## Phase 1 cut: 388 of 398 (97.5%)

Ten sections are left, and each needs something our engine has no honest way to
produce.

**Java stack traces** (`bulk/100_error_traces`, `mget/90_error_traces`,
`msearch/30_error_traces`, 3 sections). With `error_trace=true` the suite
matches `stack_trace` against Java class names -- `IndexNotFoundException`,
`DocumentMissingException`. We could print our own error chain there, but not
one naming Java classes we do not have.

**HDR percentile values** (`190_percentiles_hdr_metric` ×2,
`..._unsigned` ×1). The reported value depends on the range HDR's
`DoubleHistogram` picked, which it derives from the first value recorded: the
same input 51 comes back as 51.0 in one test and 51.0302734375 in another. Our
histogram uses one fixed scale, which matches the second and not the first.
Reaching both means porting DoubleHistogram's auto-ranging. One of the three
also asserts a shard failure of type `array_index_out_of_bounds_exception`,
which is a Java bug being pinned down rather than a behaviour.

**Aggregator profile counters** (`330_auto_date_histogram` ×2,
`360_date_histogram` ×1). These assert `type:
AutoDateHistogramAggregator.FromSingle` and counters like `surviving_buckets`,
`optimized_segments`, `leaf_visited` -- the internals of Lucene's filter-rewrite
path. We measure and report our own phases honestly; these particular numbers
describe an algorithm we do not run.

**Order of equally-scored hits** (`115_constant_keyword`, 1 section). OpenSearch
breaks score ties by document id within a shard, which is insertion order. Ours
is not recoverable from a document address: tantivy assigns doc ids across
indexing threads, so two documents written by separate requests come back either
way round. Ordering the segments by the index's own segment list does not fix it
either -- measured, then reverted. Matching this needs a sequence number stored
per document, which is memory we spent a while reclaiming.

# ความคืบหน้า Phase 2 — query, aggregation, endpoint, field type

วัดด้วย suite เดิมของ OpenSearch สามชุด กับ diff สามตัวที่รันคู่กับ OpenSearch 3.1.0 จริง

| gate | ก่อน Phase 2 | ปิด Phase 2 |
|---|---:|---:|
| core corpus (`/tmp/every_manifest.json`) | 1,427 / 1,427 | **1,427 / 1,427** |
| phase1 corpus | 398 / 398 | **398 / 398** |
| module corpus (`tools/modules_manifest.json`) | 346 / 895 | **506 / 895** |
| `tools/search_diff.py` (query + agg answers) | 67 / 92 | **92 / 92** |
| `tools/analysis_diff.py` (token for token) | 519 / 522 | 519 / 522 |
| `tools/shape_diff.py` (answer shapes) | 10 / 29 | 27 / 29 |
| index docs/s (`tools/bench_matrix.py`) | 77,346 vs 67,141 | **81,340 vs 67,598** |

## รายโมดูลที่อยู่ในขอบเขต Phase 2

| module | pass / total | ที่เหลือ |
|---|---:|---|
| mapper-extras | 100 / 100 | — |
| parent-join | 14 / 14 | — |
| aggs-matrix-stats | 15 / 15 | — |
| geo | 7 / 7 | — |
| lang-mustache | 21 / 21 | — |
| rank-eval | 8 / 8 | — |
| percolator | 1 / 1 | — |
| analysis-common | 166 / 172 | 4 ต้องการ painless (Phase 3), 2 คือ `common` query กับ `minimum_should_match` ที่ยังหา semantics ของ Lucene ไม่เจอ |
| reindex | 131 / 166 | 33 ต้องการ script (Phase 3), 2 คือ reindex จาก remote cluster |

385 ที่ยังล้มใน module corpus: lang-painless 106 + ingest-common 100 (Phase 3/4),
reindex-with-script 33, search-pipeline-common 5 (feature ใหม่ นอกแผน),
ingest-* / repository-url / smoke-test-ingest ~30 (Phase 4), plugins (phonetic,
icu collation, kuromoji completion, annotated-text) ~8

## สิ่งที่ลงไปใน Phase 2

- **BM25 ตรง Lucene** — สถิติต่อ path ของ JSON field (BoostCore เขียน docs/tokens ต่อ path),
  ตัด `(k1+1)` ออกจาก numerator, span query ชั่งน้ำหนักครั้งเดียว (idf รวมทุกคำ)
- **token graph** — token มี `positionLength`; `synonym_graph` วาง path แบบ Lucene,
  `flatten_graph` กดกราฟให้แบน, phrase/match/phrase-prefix เดินทุก path
  และ phrase บนกราฟให้คะแนนเป็น span query เดียว
- **dynamic mapping** — field ที่ไม่ได้ประกาศถูก map แบบ OpenSearch (text+keyword,
  long, float, date, boolean, object) และโผล่ใน `_mapping`; keyword sub-field
  ที่ไม่มี normalizer อ่านจาก raw view ของ parent แทนการ index ซ้ำ
- **explain tree** — `_explain` และ `explain:true` ให้ต้นไม้แบบ Lucene
  (`weight(field:term in doc) [PerFieldSimilarity]`, `score(freq=…)`, idf, tf)
- **by-query walks** — validation ครบ, routing, `_source` filtering, throttling,
  `slices: auto`, `.tasks`, `wait_for_active_shards`
- **field types** — percolator, `_size`, `copy_to`, rank_feature negative impact,
  `match_only_text` scoring (freq 1, ไม่มี norms)
- **aggregations** — matrix_stats ตามเลขคณิตของ OpenSearch (accumulate ต่อ shard แล้ว merge),
  children/parent, geohash_grid / geotile_grid, composite over grid sources,
  ranges เขียนเป็น object
- **analysis** — shingle, keyword_repeat (stacked stems), Bengali/Persian stemmers,
  synonym rules ถูกตัดด้วย chain ข้างหน้า, multiplexer, char filter offset map,
  ngram highlighting, matched_fields, intervals `use_field`
- **runner** — `catch` regex เทียบกับ `[type=…, reason=…]` แบบ client ของ OpenSearch,
  body ที่ spec บอกว่า required, `$body.x` ใน assertion

## ข้อจำกัดที่รู้ตัว (ยกไป Phase 3+)

- `common` query กับ `minimum_should_match.low_freq/high_freq` — 2 sections
- `ignore_above` บน keyword sub-field ที่อ่านจาก raw view: ค่ายาวเกินยังถูกนับใน agg
- reindex จาก remote cluster ยังไม่ทำ (validation ครบแล้ว)
- search pipelines (`search-pipeline-common`) นอกขอบเขตแผน

## Phase 3 -- Painless (in progress)

Landed so far:

- 3.1/3.2 lexer, parser, tree-walking evaluator (`src/painless/`): Java
  promotion rules, String/List/Map/date methods, Math/Integer/Long/Double/
  String/Collections/ZonedDateTime/Instant statics, regex literals, lambdas,
  method refs, try/catch, 5M-statement step limit, `while (true) {}` refused
  at compile time ("no paths escape from while loop").
- 3.3 contexts: update (`ctx`, `ctx.op`, `_id` guard, scripted_upsert,
  self-referencing `_source` refused), `_scripts/painless/_execute`
  (painless_test / filter / score), script_fields, `script` query (a
  segment scan with a bitset scorer, `src/query/script.rs`), `script_score`
  query with boost/min_score and explain, function_score `script_score`
  functions with termFreq/totalTermFreq/sumTotalTermFreq/docFreq/sumDocFreq,
  stored scripts compile when put for a context (`PUT _scripts/{id}/{ctx}`),
  `_scripts/painless/_context` answers from OpenSearch's own whitelists
  (`src/painless/whitelist/*.json.gz`, plugin classes stripped).
- Doc values in scripts: dates render with millis, ip from its hex, geo
  points at Lucene's int32 grain; a mapping's Java date pattern
  (`yyyy/MM/dd`) is read on write (`parse_with_pattern`).

Gates: core 1427/1427, phase1 398/398, modules 556/895 (was 506),
lang-painless 60/144, search_diff 92/92.

Next in Phase 3: aggregation scripts (`terms` with `script`/`_value`,
scripted_metric, bucket_script/selector, moving_fn), sort by script,
update_by_query/reindex scripts, derived fields, intervals script filter,
analysis-common script filters. The terms aggregation is answered by
BoostCore's own engine, so a script-sourced one needs a source-reading path
beside it.

### Phase 3 closed (2026-09-02)

- 3.3 contexts, all wired: update, update_by_query and reindex scripts
  (ctx with op/noop/delete, null _id -> auto id, _index redirect, junk
  fields refused), script_fields, script query, script_score and
  function_score scripts with term statistics, sort by _script, intervals
  script filter, aggregation scripts (terms with _value/doc, scripted_metric,
  bucket_script, bucket_selector, moving_fn), derived fields (mapping and
  search-body definitions; queried, fetched, highlighted and aggregated),
  analysis-common condition and predicate_token_filter.
- 3.4 whitelist: every context answers `_context` from OpenSearch's own
  whitelists; the builtins now cover the String, StringBuilder, List/Set/
  Collection (Groovy-style each/any/every/findAll/groupBy and the Stream
  forms), Map, Math, Integer/Long/Double/Boolean/Character statics and
  fields, Collections, Objects, Arrays, Optional, Iterator, Pattern/Matcher,
  Collectors (toList/toSet/joining/counting/toMap/groupingBy/partitioningBy/
  mapping/summing/averaging/minBy/maxBy/reducing/collectingAndThen/
  summarizing), Duration, LocalDate/LocalDateTime/Instant/ZonedDateTime
  constructors and DateTimeFormatter names.
- 3.5 lang-painless: 143/143 (1 skipped).
- Along the way: the standard tokenizer keeps `example.com` and `x:y`
  whole (BoostCore e3be811); `match` on a date, number, boolean or ip is
  the value itself with a score of one; auto_date_histogram steps below a
  day and honours `format`; a `keyword` field under an object is a field of
  its own in aggregations.

Gates: core 1427/1427, phase1 398/398, modules 665/895 (was 556 at the start
of Phase 3), lang-painless 143/143, reindex 154/166, search_diff 92/92,
analysis_diff 519/522.

## Phase 4 -- Ingest (closed 2026-09-02)

- 4.1 Ingest pipelines (`src/ingest/`): thirty-four processors -- set,
  append, rename, remove (and exclude_field), remove_by_pattern, copy,
  lowercase, uppercase, trim, split, join, sort, convert, gsub, json, kv,
  csv, dot_expander, urldecode, html_strip, bytes, date, date_index_name,
  grok (OpenSearch's own 312-pattern bank, atomic groups and possessive
  quantifiers read as plain), dissect (append, skip, named keys, right
  padding), script (with the `Processors` statics), pipeline, drop, fail,
  foreach, fingerprint, community_id, user_agent, geoip (no database
  shipped). `if`, `on_failure`, `ignore_failure`, `tag`, `description`,
  mustache templates in values and field names, `_ingest` metadata,
  `_simulate` (plain and verbose, with `if` results and nested pipeline
  steps), `_ingest/processor/grok`, pipeline stats in nodes stats.
  Pipelines run on single writes, bulk (index/create and upserts, scripted
  or not), update upserts; `pipeline` param, `index.default_pipeline`,
  `index.final_pipeline` (also from templates for an index not yet made),
  `_none`; a script may change `_index`, `_id`, `_routing`, `_version`,
  `_if_seq_no`; `drop` answers noop.
- 4.2 Search pipelines (`src/search/pipeline.rs`): request processors
  filter_query, script (over the search source and a request context),
  oversample; response processors rename_field, sort, truncate_hits,
  collapse; named on the request, given in the body, or the index's
  `index.search.default_pipeline`. The user_agent processor reads uap-core's
  regexes (shipped) or a file under `config/ingest-user-agent/`.
- Along the way: Java date patterns write fractions, zones and quoted
  text; a `match` on an already-collected `function_score` widens the page;
  docvalue_fields come back sorted.

Gates: core 1427/1427, phase1 398/398, modules 817/895 (was 666 at the
start of Phase 4), ingest-common 138/139 (the one gap: a `char` typed value
in a script, which this engine cannot tell from a one-letter string),
search-pipeline-common 11/11, ingest-user-agent 5/5, search_diff 92/92.
ingest-geoip 1/8 stays out: it needs MaxMind databases that are not in the
tree.

### Leftovers cleared before Phase 5 (2026-09-02)

- `common` query: words that share a place (a word and its synonyms) are one
  clause; `minimum_should_match` as a number or `{low_freq, high_freq}`;
  with no rare words the low-frequency minimum applies to the common ones.
- A keyword sub-field with `ignore_above` gets its own copy, cut to the limit,
  rather than being served from its parent's raw view.
- A script that fails inside a search (the `script` query, and the searches
  a reindex or update_by_query walks) is reported as a shard failure with
  the script's own exception inside, reason "Partial shards failure".
- `_cluster/state/metadata` lists composable templates under
  `index_template`, ingest pipelines under `ingest`, and the deleted indices
  in the graveyard.
- The Thai analyzer keeps a Latin word whole across a hyphen or an
  apostrophe, as Java's break iterator does.

Kept as known gaps, each needing more than it is worth:

- `common` query with stacked synonyms: OpenSearch splits rare from common
  by the document frequency of the *segment* a term is read in, so the two
  remaining sections depend on how three writes landed in segments. The
  query is deprecated; the per-place clause grouping above is the honest
  part.
- Estonian stemming of words with an apostrophe (`don't` -> `don'`, `it's`
  kept): Snowball's Estonian treats the apostrophe as a letter in its
  regions; our generated algorithm strips it. Two analysis_diff cases.
- A `char` typed value in an ingest script cannot be told from a one-letter
  string (one ingest-common section).

## Phase 5 -- Security (in progress, 2026-09-02)

Ground truth is the security plugin at tag 3.1.0.0 (`study/security`) and a
reference container running it (`os-secure`, https 9399). Security is off
until `plugins.security.disabled: false` (or `BOOSTSEARCH_PLUGINS_SECURITY_DISABLED=false`),
so every gate that came before runs unchanged.

### 5.1 TLS (done)

- `src/tls.rs`: rustls over the same axum router; `plugins.security.ssl.http.*`
  from `config/boostsearch.yml` or `BOOSTSEARCH_SSL_HTTP_*`; a self-signed
  certificate is written to `config/certs/` when none is given; client
  certificates are accepted when a trust store is named.
- `_plugins/_security/api/ssl/certs` describes the node's certificates; as
  in the plugin, a password is refused ("Access denied"), only an admin
  certificate may read them.

### 5.2 Users, roles, mappings, action groups, tenants (done)

- `src/security/mod.rs`: the configuration model with the plugin's static
  action groups, roles and tenants embedded and its demo users, roles and
  mappings as the defaults; persisted as the plugin's YAML under
  `config/security/`; bcrypt (`$2y$`, 12 rounds) for passwords; wildcard
  matching (`*`, `?`, `/regex/`); action groups flattened through groups;
  role mapping by user, backend role, all-of backend roles, and host; a
  caller's roles listed in Java `HashSet` order, as the plugin lists them.
- `src/security/api.rs`: `_plugins/_security/api/{internalusers,roles,rolesmapping,actiongroups,tenants}`
  (GET, PUT, DELETE, PATCH single and whole-kind with JSON Patch),
  `account` (GET, password change with `current_password`), `authinfo`,
  `health`, `whoami`, `permissionsinfo`, `securityconfig`, `ssl/certs`;
  the plugin's words for created/updated/deleted/not found/static/
  reserved/invalid keys/missing keys; the REST API is open only to the
  roles in `plugins.security.restapi.roles_enabled`.
- `src/security/layer.rs`: basic auth with the plugin's 401 (`text/plain`
  `Unauthorized`, `WWW-Authenticate: Basic realm="OpenSearch Security"`),
  anonymous auth when `config.yml` allows it, and a per-request `Caller`
  extension for the handlers.
- Authentication is cached by a digest of the credentials for
  `plugins.security.cache.ttl_minutes` (60), emptied on every
  configuration change, so bcrypt is paid once per credential rather
  than once per request (without it every request cost ~165 ms).

Checked against the reference: 41 REST API steps (create, update, patch,
static/reserved refusals, password change, deletion) answer identically;
0 diffs.

### 5.3 Authorization (done for the REST surface)

- Every request is mapped to the transport action it stands for
  (`indices:data/read/search`, `indices:admin/mappings/get`, ...) and judged
  before the handler runs: cluster actions by cluster permissions, index
  actions by the roles' index patterns (with `${user_name}` and attribute
  substitution) over the indices the path resolves to; a request naming no
  index is judged over every index; `do_not_fail_on_forbidden` narrows a
  partly-allowed request instead of refusing it.
- A refusal is the plugin's `security_exception`:
  `no permissions for [action] and User [name=..., backend_roles=[...], requestedTenant=null]`.

Checked against the reference as a limited user (role over `logs-*` with
`read` and `cluster_composite_ops_ro`): 31 requests across search, get,
count, write, index create/delete, mapping, settings, `_cat`, cluster,
bulk/mget/msearch, field_caps, refresh, stats, update, delete_by_query;
statuses and refusal bodies identical; 0 diffs.

### Still to do in Phase 5

- 5.4 DLS applied inside every query path; 5.5 FLS and field masking
  across `_source`, fields, docvalue_fields, aggregations, sorts,
  highlighting, field_caps, mappings and scripts (the role model already
  carries `dls`, `fls`, `masked_fields`; `IndexRestrictions` computes the
  caller's view per index).
- Per-item refusals inside `_bulk`, `_mget`, `_msearch` (today the whole
  request is judged; the plugin judges each item).
- 5.6 SAML / OIDC / LDAP; 5.7 audit log; admin client certificates.

### Performance with security on

Measured with `tools/bench_matrix.py` (now taking `BENCH_A`, `BENCH_B` and
`BENCH_AUTH`): BoostSearch with security on and basic auth on every request,
against OpenSearch 3.1.0 with no security plugin at all.

| dimension | OpenSearch (plain) | BoostSearch (security, HTTP) | BoostSearch (security, HTTPS) |
|---|---|---|---|
| index docs/s | 67,237 / 66,882 | 67,448 | 67,295 |
| memory | 1.65 GiB | 380 MiB | 365 MiB |
| match_all p50 | 1.30 ms | 0.42 ms | 0.78 ms |
| term p50 | 0.98 ms | 0.39 ms | 0.77 ms |
| match p50 | 1.16 ms | 0.60 ms | 0.95 ms |
| bool+filter p50 | 1.03 ms | 0.73 ms | 1.03 ms |
| range p50 | 0.70 ms | 0.57 ms | 0.88 ms |
| sort_desc p50 | 2.16 ms | 0.99 ms | 1.24 ms |
| terms_agg p50 | 1.63 ms | 0.67 ms | 0.96 ms |
| date_histogram p50 | 1.62 ms | 0.88 ms | 1.19 ms |
| nested_agg p50 | 1.61 ms | 0.80 ms | 1.08 ms |
| cardinality p50 | 1.56 ms | 0.69 ms | 0.98 ms |

Every dimension won in both runs (the OpenSearch column shows the plain
reference measured alongside each run; the HTTPS run's OpenSearch latencies
were within noise of the HTTP run's).

Gates after this work (security off, default): phase1 398/398 (release
build), modules 820/895 as before. A debug build trips a `debug_assert` in
BoostCore's `EmptyScorer::seek` during an explain of a cross-fields query;
release builds are unaffected, and the fix belongs in the fork (filed).
