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
