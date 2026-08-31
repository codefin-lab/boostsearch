# ที่เหลือ และบทเรียนระหว่างทาง

รันทั้ง suite (409 ไฟล์): **1,414 ผ่าน · 14 ตก · 77 skip = 99.1%**
Phase 1 manifest: 397/398 · Phase 3 manifest: 1,097/1,100

รายชื่อที่ตกให้ดูจาก runner เสมอ ไฟล์นี้เก็บ *เหตุผล* ที่เหลือไปต่อไม่ได้ กับบทเรียน

## บทเรียนใหญ่ที่สุด: อ่านของจริงดีกว่าเดา

สาม-สี่เรื่องเคยถูกบันทึกว่า "ไปต่อไม่ได้" เพราะเดาจากพฤติกรรมที่เห็นในเทส แล้ว
กลับปิดได้ทันทีเมื่อไปอ่าน source ของ OpenSearch ใน `study/OpenSearch`:

- **routing hash** — `Murmur3HashFunction.hash(String)` แฮชเป็น **UTF-16**
  (ตัวอักษรละ 2 ไบต์ low byte ก่อน) seed 0 ไม่ใช่ UTF-8 · เคยเขียนไว้ว่า
  "14 seed ให้ผลตรงกับตัวอย่าง เลือกอันไหนก็เป็นการเดาเลข" — ที่จริงไม่ต้องเดาเลย
  แก้แล้ว sliced scroll ผ่านทันที
- **HDR scale** — `DoubleHistogram` เปิดที่ 2^800 แล้วให้ค่าแรกดึงลงมา
- **sum_other_doc_count** — `BucketSelectionStrategy` บอกว่า shard นับ
  otherDocCount = ทั้งหมดบน shard ลบ top ที่ส่งกลับ แล้ว reduce บวกที่ตัดทิ้งอีก
- **alias ใน template** — `MetadataIndexTemplateService.resolveAliases` reverse
  list แล้วให้ชื่อที่เจอก่อนชนะ = นิยาม alias ทั้งก้อน ไม่ใช่ merge
- **inner collapse** — `InnerHitBuilder` รับแค่ `field` อย่างเดียว

## 14 ตัวที่เหลือ

**nested แบบลึก (6)** — `search.inner_hits :: Inner hits with disabled _source`
(nested ใน nested), `20_fvh :: Highlight multiple nested documents`,
`230_composite :: filtered nested parent`, `410_nested_aggs`,
`200_top_hits :: sequence numbers`, `240_date_nanos :: nested sort now` ·
ที่นี่เอกสารถูกเก็บทั้งก้อน ไม่ได้แยก parent/child เหมือน Lucene จึงไม่มี
"ขอบเขต nested" ให้ sort หรือ highlight อ้างถึงแยกจาก document

**resolution ของ date (3)** — `40_range :: Date Range Missing` (epoch seconds
ปี 11970 เกิน i64 nanosecond), `230_composite :: date_histogram on date_nanos`,
`240_date_nanos :: doc value fields across date and date_nanos` (OpenSearch
เทียบข้าม index ด้วย *ตัวเลขดิบ* คนละหน่วย millis กับ nanos เมื่อไม่ระบุ
`numeric_type` — ที่นี่เก็บเป็น nanosecond หน่วยเดียว)

**flat_object partial (2)** — ลำดับ hit ต่างกันเพราะคะแนน

**shard เป็นเรื่อง routing ไม่ใช่ที่เก็บ (2)** — `delete/50_refresh`
(refresh ถึง shard เดียว), `190_percentiles_hdr :: Negative values`
(shard ที่ถือค่าติดลบพังทั้ง shard) · ตอนนี้ routing hash ตรงแล้ว จะเอาจริง
ต้องมี store แยกต่อ shard

**collapse สองชั้น (1)** — `115_multiple_field_collapsing` ต้อง collapse
ชั้นในของ inner_hits อีกชั้น

## บทเรียนอื่นที่ยังใช้ได้

### closed index — ค่าเริ่มต้นของ `expand_wildcards` ต่างกันตาม API

`open` สำหรับอ่านเอกสารและ `_resolve/index` · `open,closed` สำหรับ
delete/cat/stats · **`all` สำหรับ cluster health** ⇒ ทำเป็น opt-in ต่อ endpoint

### aggregation ที่ต้องรันเองลากตัวแม่ออกมาด้วย

`peelable()` เช็คลูกหลานทั้งต้น ถ้าลูกต้องรันเองตัวแม่ก็ต้องออกจาก tantivy ด้วย
แล้ว `run_field_terms_agg` รับหน้าที่หา bucket ให้ ส่วน `order` ที่ชี้ไป sub-agg
ที่ peel ไปแล้วต้องเรียงเองหลังได้คำตอบ

### `_cat` — ช่องว่างท้ายบรรทัดคือคอลัมน์ที่หายไป

เคยสรุปว่า `_cat` เติมช่องว่างท้ายทุก cell แล้วลองสองครั้ง ครั้งละ −20 sections ·
ที่จริง regex กำลังบอกว่ามี **คอลัมน์ว่างต่อท้าย** ที่ยังไม่มี (`composed_of`
ของ template, replica ที่ยัง unassigned ของ shards) · เติมคอลัมน์แล้วผ่านหมด

### positions ไม่ได้อยู่ใน column

`intervals` กับ `significant_text` ต้องรู้ว่าคำอยู่ตรงไหน ซึ่ง column เก็บแต่ค่า
ไม่เก็บตำแหน่ง ⇒ อ่าน text กลับมา analyse ใหม่ แล้วประเมินกฎบน token
(OpenSearch ก็ทำแบบเดียวกันกับ significant_text)

## node ที่ชุดทดสอบคาดหวัง

    OBSEARCH_NODE_ATTRS=testattr=test ./target/release/obsearch
