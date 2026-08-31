# ที่เหลือ และบทเรียนระหว่างทาง

รันทั้ง suite (409 ไฟล์): **1,427 ผ่าน · 0 ตก · 77 skip = 100%**
Phase 1 manifest: **398/398** · Phase 3 manifest: 1,098/1,100

## บทเรียนใหญ่ที่สุด: อ่านของจริงดีกว่าเดา

หลายเรื่องเคยถูกบันทึกว่า "ไปต่อไม่ได้" เพราะเดาจากพฤติกรรมที่เห็นในเทส แล้ว
ปิดได้ทันทีเมื่อไปอ่าน source ของ OpenSearch ใน `study/OpenSearch`:

| เคยเดา | ของจริง |
|---|---|
| routing hash — ลอง 14 seed แล้วสรุปว่า "เลือกอันไหนก็เป็นการเดาเลข" | `Murmur3HashFunction.hash(String)` แฮช **UTF-16** (ตัวอักษรละ 2 ไบต์ low byte ก่อน) seed 0 |
| HDR scale เป็นค่าคงที่ที่ fit กับเทส | `DoubleHistogram` เปิดที่ 2^800 แล้วให้ค่าแรกดึงลงมา |
| `sum_other_doc_count` "single shard เลยได้คนละเลข" | `BucketSelectionStrategy`: shard นับ = ทั้งหมดบน shard ลบ top ที่ส่งกลับ แล้ว reduce บวกที่ตัดทิ้ง |
| alias ใน template merge กันยังไง | `resolveAliases` reverse list แล้วชื่อที่เจอก่อนชนะ = แทนที่ทั้งก้อน |
| sort บนฟิลด์ nested ควรทำยังไง | `SortBuilder.resolveNested` คืน null → อ่าน doc values บน parent ซึ่งไม่มีค่า |
| ลำดับ bucket ที่เสมอกัน | `BucketOrder.compound(count(false))` — คอมเมนต์บอกเอง "automatically adds tie-breaker key asc" |
| date กับ date_nanos เทียบกันยังไง | `DateFieldMapper.Resolution`: `date` เก็บ **millisecond**, `date_nanos` เก็บ **nanosecond** · sort value คือเลขที่เก็บ จึงเทียบข้ามหน่วยกันตรง ๆ |
| shard ล้มเพราะค่าติดลบ | `ExpandSearchPhase` / HDR: shard ที่ถือค่าที่ sketch ไม่รับ ล้มทั้ง shard และ search เดินต่อโดยไม่มีมัน |

## 3 ตัวที่ปิดไปด้วยการ fork — BoostCore

สามอันที่เคยเขียนไว้ว่า "แก้ได้ทางเดียวคือแก้ tantivy" ตอนนี้แก้ tantivy แล้ว
(`vendor/boostcore`, ดู `docs/boostcore.md`)

### field norm ต่อ path ของ JSON field

tantivy ไม่ได้บันทึก field norm ให้ JSON field เลย · เติมให้แล้วยังไม่พอ
เพราะเอกสารทั้งก้อนอยู่ใน JSON field เดียว ⇒ ความยาวของ path หนึ่ง
กลายเป็นความยาวของทั้งเอกสาร · Lucene เก็บ norm **ต่อฟิลด์** และ path ใน
JSON ก็คือฟิลด์ในสคีมาแบน ⇒ BoostCore เก็บ norm ต่อ path ด้วย
(รวมทั้งตอน merge และตอนคำนวณ block max ที่ index time
มิฉะนั้น block max ไม่ใช่ขอบบนอีกต่อไป และ pruning จะตัด hit ที่ควรได้)

### bucket ที่เป็นเลขเดียวกันต้องเป็น bucket เดียว

`IntermediateKey` แฮชและเทียบด้วย variant ⇒ `0` ที่มาจาก segment ที่เก็บเป็น
`i64` กับ segment ที่เก็บเป็น `u64` (segment ตัดสินจากค่าใหญ่ตัวแรกที่เห็น)
กลายเป็นคนละ bucket · เห็นทันทีที่ index เอกสารที่มีค่าเกิน `i64::MAX` เป็นตัวแรก

### terms query ให้คะแนนคงที่

`MappedFieldType.termsQuery` เขียนไว้ตรง ๆ ว่า "build a constant-scoring query"
⇒ แมตช์สองคำไม่ได้บอกอะไรมากกว่าแมตช์คำเดียว ลำดับจึงตกไปที่ doc id

## refresh ทีละ shard — ปิดแล้ว

`delete/50_refresh` เคยเขียนไว้ว่าแก้ไม่ได้ เพราะ index เดียว = writer เดียว
`refresh` จึงเห็นทุกอย่างพร้อมกัน · ที่จริงแก้ได้ ถ้าเลื่อนตัว **operation** เอง
ไม่ใช่เลื่อน commit:

- write ทุกตัวเข้าคิว `deferred` พร้อมเลข shard (routing เดิม ๆ ที่มีอยู่แล้ว)
- `refresh=true` บน write → apply เฉพาะ op ของ shard นั้น แล้ว commit + reload
- `_refresh` ทั้ง index → apply ทุก shard
- realtime GET ไม่พัง เพราะอ่านจาก `pending` (สำเนา source) อยู่แล้ว ไม่ได้อ่านจาก writer

ลำดับภายใน shard เดียวกันคงไว้ครบ (delete/add ของ id เดียวกันอยู่ shard เดียวกันเสมอ
เพราะ routing เป็นฟังก์ชันของ id) ข้าม shard ไม่ต้องสนใจลำดับ

คิวไม่ถูกเก็บไว้ยาว: เกิน 2,048 op หรือ writer ถูก reap หรือ pending budget เต็ม
→ ส่งเข้า writer ทั้งหมด (ซึ่ง**ไม่ใช่**การทำให้เห็น — ยังต้อง commit + reload อยู่ดี)
memory ตอน bulk 200k docs: 297 MB → 312 MB · index rate ไม่ขยับ (80.7k → 83.3k docs/s)

เจอบั๊กเก่าระหว่างทางด้วย: `scaled_field: 1` (integer) กับ `1.53` (float) ลงคนละ
column type ทำให้ range agg มองไม่เห็นตัวที่เป็น integer — เป็น flake ที่โผล่มา
เพราะ deferral เปลี่ยนจังหวะที่เอกสารถึง writer · แก้โดย coerce ตัวเลขให้ตรงชนิด
ที่ mapping บอกตั้งแต่ตอน index (1 → 1.0 สำหรับ float, 1.0 → 1 สำหรับ long)

## date เก็บเป็น millisecond ทั้งระบบแล้ว

เคยเขียนไว้ว่า "แก้ให้ตรงต้องเก็บ date เป็น millisecond ทั้งระบบ ซึ่งกระทบ
range query, histogram, sort, format ทั้งหมด" — ทำแล้ว

เดิม date ถูกเขียนเป็น ISO text ในดัชนี แล้ว BoostCore อ่านกลับเป็น DateTime
(i64 **nanosecond** หมดที่ปี 2262) · และ text เทียบด้วยการสะกด ปีเกิน 9999
ก็เรียงผิด · OpenSearch เก็บเป็น long: `DateFieldMapper.Resolution`
บอกว่า `date` = millisecond, `date_nanos` = nanosecond และ sort คืนเลขนั้นตรง ๆ

ที่ขยับตามไป: bound ของ query, ปลายทั้งสองของ `date_range`, ช่วงที่
date/auto date histogram เดิน, composite date source, key ของ
terms/multi_terms กับ key_as_string, สำเนาที่ multi-field ของ date ถือ,
และ sort value ที่ไม่ต้อง rescale อีกแล้ว · `_source` ไม่แตะ — ยังเป็นสิ่งที่
client ส่งมา

ผลพลอยได้:

- `search.aggregation/40_range :: Date Range Missing` ผ่าน — epoch seconds
  หลักแสนล้าน = ปี 11970 ไม่ overflow อีก
- bucket ของ date_range เรียงตามจุดเริ่ม ตาม `AbstractRangeBuilder.sortRanges`
- เอกสารที่ไม่มีค่าไปอยู่ bucket ที่ `missing` ชี้
- range query อ่าน `format` ของตัวเองแทนของ mapping · `gte: 2019` บน date
  คือ **ปี** ตามที่ default format อ่าน
- ใต้ flat_object ไม่ถูกตีความเป็น date อีก — ค่าคงการสะกดที่ส่งมา

เรื่อง perf: date_histogram แบบ fixed step ถูกเขียนใหม่เป็น `histogram`
ก่อนส่งให้ BoostCore เดินรอบเดียว (ถ้าปล่อยให้ engine เดินเองทีละ bucket
จะเหลือ 840 req/s จาก 11,500) · calendar unit, zone ที่ไม่ใช่ UTC และ
date_nanos ยังเดินเองอยู่

| aggregation | ก่อน (date เป็น text) | หลัง |
| --- | --- | --- |
| date_histogram 1h | 15,000/s | 10,700/s |
| histogram บน long | 14,200/s | 15,300/s |
| terms บน keyword | 15,100/s | 23,700/s |

date histogram เสียไปบ้างเพราะอ่าน column ตัวเลขแทน column วันที่ ·
ที่เหลือเร็วขึ้นเพราะ 200k วันที่ไม่ได้เป็น 200k สตริงใน term dictionary อีกแล้ว

## บทเรียนอื่นที่ยังใช้ได้

### closed index — ค่าเริ่มต้นของ `expand_wildcards` ต่างกันตาม API

`open` สำหรับอ่านเอกสารและ `_resolve/index` · `open,closed` สำหรับ
delete/cat/stats · **`all` สำหรับ cluster health** ⇒ ทำเป็น opt-in ต่อ endpoint

### aggregation ที่ต้องรันเองลากตัวแม่ออกมาด้วย

`peelable()` เช็คลูกหลานทั้งต้น ถ้าลูกต้องรันเองตัวแม่ก็ต้องออกจาก tantivy ด้วย
แล้ว `run_field_terms_agg` รับหน้าที่หา bucket ให้ ส่วน `order` ที่ชี้ไป sub-agg
ที่ peel ไปแล้วต้องเรียงเองหลังได้คำตอบ

### nested = object ไม่ใช่ document

nested aggregation นับ object ที่ path นั้น · filter คัด object,
terms/composite group object, metric อ่านค่าของ object, reverse_nested
กลับออกมาเป็น document · inner_hits list เฉพาะ object ที่ query แมตช์
และ highlight จาก object ตัวเอง · sort ต้องบอกว่าอ่านใน object ไหน

### `_cat` — ช่องว่างท้ายบรรทัดคือคอลัมน์ที่หายไป

เคยสรุปว่า `_cat` เติมช่องว่างท้ายทุก cell แล้วลองสองครั้ง ครั้งละ −20 sections ·
ที่จริง regex กำลังบอกว่ามีคอลัมน์ว่างต่อท้ายที่ยังไม่มี

### positions ไม่ได้อยู่ใน column

`intervals` กับ `significant_text` ต้องรู้ว่าคำอยู่ตรงไหน ⇒ อ่าน text กลับมา
analyse ใหม่แล้วประเมินกฎบน token (OpenSearch ก็ทำแบบเดียวกันกับ significant_text)

## node ที่ชุดทดสอบคาดหวัง

    BOOSTSEARCH_NODE_ATTRS=testattr=test ./target/release/boostsearch

## เทสช้าเพราะ client ไม่ใช่เพราะ server

`yaml_runner.py` เปิด `requests.Session` ใหม่ทุก section (1,175 ครั้งต่อรอบ)
และไม่เคยปิด · socket ค้างจนเปิดใหม่ไม่ได้ แล้วอ่านออกมาเหมือน server ตาย
(`ConnectionError` เป็นสิบ ๆ section ติดกัน) · ใช้ pool เดียวทั้งรอบแล้ว
phase 3 จาก **สิบนาที เหลือแปดวินาที**
