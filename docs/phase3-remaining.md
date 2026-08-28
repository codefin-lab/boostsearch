# Phase 3 — ของที่ยังไม่ได้ทำ

บันทึกไว้ระหว่างไล่ทีละเรื่อง จะได้ไม่หายไปในกอง 453

## alias — ปิดแล้ว 95/99 เหลือ 4 ตัว

| section | ต้องการอะไร |
|---|---|
| `indices.get_alias :: Get alias against closed indices` | ต้องมี open/close index API ก่อน — alias view ต้องกรอง index ที่ปิดอยู่ออก |
| `mget/14_alias_to_multiple_indices :: Multi Get with alias that resolves to multiple indices` | mget ผ่าน alias ที่ชี้หลาย index ต้องตอบ `found: false` พร้อมระบุว่า alias กำกวม |
| `indices.put_alias :: Basic test for put alias` (assertion สุดท้าย) | client ส่ง `PUT //_alias/` (path segment ว่าง) รูปที่ถูกต้อง `PUT /_alias` ตอบ 400 อยู่แล้ว · ลอง middleware normalize path แล้ว **ไม่ได้ผลเพราะ layer ทำงานหลัง routing** และการครอบจากข้างนอกต้องเพิ่ม `tower` เป็น dependency — ประเมินว่าไม่คุ้มกับ 1 section |
| `indices.put_alias :: Index and alias in request body can be overridden by path` | path ชี้ index ที่ไม่มีจริง ต้องดูว่า OpenSearch ให้ path ชนะ body หรือ error |

## range aggregations — 8/11 ใน 40_range, adjacency_matrix 2/2

| section | ต้องการอะไร |
|---|---|
| `Date Range Missing` | ค่าใน test คือ epoch **seconds** ระดับ 3×10¹¹ = ปี 11972 · date column ของเราเป็น i64 nanosecond ซึ่งเก็บได้ราว ±292 ปีรอบ epoch (1678–2262) ⇒ **เก็บไม่ได้** จะรองรับต้องเปลี่ยนความละเอียดของ date column ทั้งระบบ |
| `Date range unmapped with children` | ยังไม่ได้ไล่ |
| `Double range profiler shows filter rewrite info` | counter ภายในของ Lucene filter-rewrite เหมือนกลุ่มที่ Phase 1 ไปไม่ถึง |

## aggregation ที่ไล่แล้วเหลือ

**`20_terms` 16/26** — `partitioned terms` (2) · `unmapped booleans` (1) ·
`mixing longs and doubles` (2) · `deprecated _term order` (1) ·
`fielddata memory_size_in_bytes` (1) · `unmapped dates` (1) ·
`deferred_aggregators` (2) — **ไม่รายงานโดยตั้งใจ** เพราะมันอธิบายการเลื่อน
คำนวณ sub-agg แบบสองเฟสที่เราไม่ได้ทำ

**`10_histogram` 4/11** — `histogram with hard bounds` บน range field
(ต้อง peel `histogram` เหมือนที่ทำกับ calendar date_histogram) ·
`date_histogram profiler` (2) — calendar_interval ถูก peel ออกไปก่อนถึง
tantivy จึงไม่มี profile entry · `_time order` · `time_zone` ·
`total_buckets` ต่างกัน 1 (เรานับ bucket ว่างที่เติมด้วย)

## เรียงตามขนาดที่เหลือ (413)

| จำนวน | กลุ่ม | หมายเหตุ |
|---:|---|---|
| 76 | `search.aggregation` | **กำลังทำ** |
| 36 | `search` | |
| 29 | `indices.stats` | |
| 14 | `cluster.state` | |
| 11 | `wlm_stats` | workload management — อาจไม่ต้องทำ |
| 10 | `search.highlight` | ยังไม่มี highlight เลย |
| 9 | `suggest` | ยังไม่มี suggest เลย |
| 8 | `scroll` / `bulk` / `update` / `delete` / `cat.templates` | อย่างละ 8 |

## endpoint ที่ยังไม่มี (54 sections)

`_cluster/stats` · `_cluster/voting_config_exclusions` · `_rollover` ·
`_data_stream` · `_resolve/index` · `_shrink` / `_split` / `_clone` ·
`_index_template/_simulate` · `_mtermvectors` · point-in-time ·
`_remote/info` · `_block/write` · `_segments` · `_shard_stores` · `wlm_stats`

ส่วนใหญ่เป็น cluster/index management ที่แต่ละตัวได้ 1-6 sections

## multi_terms — 13/17 เหลือ 4

| section | ต้องการอะไร |
|---|---|
| `multiple multi_terms bucket` | multi_terms ซ้อนใน multi_terms |
| `aggregate over multi-terms test` | เหมือนกัน |
| `min_doc_count` (assertion หลัง) | `min_doc_count: 0` ต้องคืน bucket ของคู่ค่าที่ไม่มีเอกสารเลยด้วย |
| `sum_other_doc_count` | ตัดยอดต่อ shard (`shard_size` กับ 2 shard) เราเป็น single shard จึงได้คนละตัวเลข |

### ข้อจำกัดเชิงโครงสร้าง: aggregation ที่ peel ออกมาซ้อนกันไม่ได้

`multi_terms`, `composite`, `date_histogram` แบบ calendar, `rare_terms`,
percentiles แบบ HDR — ทั้งหมดถูก **peel** ออกจาก request ก่อนส่งให้ tantivy
เพราะ tantivy ไม่รู้จัก sub-aggregation ของมันวิ่งผ่าน `filtered_count`
ซึ่งส่งต่อให้ tantivy parser ⇒ **ถ้า sub-agg เป็นตัวที่ peel เหมือนกัน จะพัง**

แก้ได้ด้วยการวนทีละ bucket แล้วยิง sub-agg พร้อม filter ของ bucket นั้น
(แบบเดียวกับ calendar histogram) แต่เป็นการรื้อ ควรทำทีเดียวให้ครบทุกตัว
ไม่ใช่ไล่แก้ทีละอัน
