# Phase 3 — ของที่ยังไม่ได้ทำ

บันทึกไว้ระหว่างไล่ทีละเรื่อง จะได้ไม่หายไปในกอง 453

## alias — ปิดแล้ว 95/99 เหลือ 4 ตัว

| section | ต้องการอะไร |
|---|---|
| `indices.get_alias :: Get alias against closed indices` | ต้องมี open/close index API ก่อน — alias view ต้องกรอง index ที่ปิดอยู่ออก |
| `mget/14_alias_to_multiple_indices :: Multi Get with alias that resolves to multiple indices` | mget ผ่าน alias ที่ชี้หลาย index ต้องตอบ `found: false` พร้อมระบุว่า alias กำกวม |
| `indices.put_alias :: Basic test for put alias` (assertion สุดท้าย) | client ส่ง `PUT //_alias/` (path segment ว่าง) รูปที่ถูกต้อง `PUT /_alias` ตอบ 400 อยู่แล้ว · ลอง middleware normalize path แล้ว **ไม่ได้ผลเพราะ layer ทำงานหลัง routing** และการครอบจากข้างนอกต้องเพิ่ม `tower` เป็น dependency — ประเมินว่าไม่คุ้มกับ 1 section |
| `indices.put_alias :: Index and alias in request body can be overridden by path` | path ชี้ index ที่ไม่มีจริง ต้องดูว่า OpenSearch ให้ path ชนะ body หรือ error |

## เรียงตามขนาดที่เหลือ (453)

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
