# ที่เหลือ และบทเรียนระหว่างทาง

รันทั้ง suite (409 ไฟล์): **1,422 ผ่าน · 5 ตก · 77 skip = 99.6%**
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

## 5 ตัวที่เหลือ — ทั้งหมดติดข้อจำกัดของชั้นล่าง

### tantivy ไม่เก็บ field norm ให้ JSON field — 3 sections

`index/100_partial_flat_object`, `index/105_partial_flat_object_nested`,
`search/115_multiple_field_collapsing` ตกเพราะ **ลำดับ hit** ล้วน ๆ:
เอกสารที่มีคำเดียวกันแต่ฟิลด์สั้นกว่า ต้องได้คะแนนมากกว่า (BM25 field length norm)

ที่นี่เอกสารทั้งก้อนอยู่ใน JSON field เดียว และ
`SegmentWriter` ของ tantivy 0.26 ในสาขา `FieldType::JsonObject`
**ไม่มีการเรียก `fieldnorms_writer.record` เลย** (ทุก type อื่นเรียกหมด) ⇒
`set_fieldnorms(true)` ไม่มีผล และ BM25 คิดเหมือนทุกเอกสารยาวเท่ากัน

แก้ได้ทางเดียวคือแก้ tantivy หรือเลิกใช้ JSON field

### date เกินช่วงที่ i64 nanosecond เก็บได้ — 1 section

`search.aggregation/40_range :: Date Range Missing` เขียน epoch **seconds**
ระดับแสนล้าน = ปี 11970 · OpenSearch เก็บ `date` เป็น millisecond จึงรับได้สบาย
ที่นี่ค่าไปอยู่ใน DateTime ของ tantivy ซึ่งเป็น i64 **nanosecond** หมดที่ปี 2262

แก้ให้ตรงต้องเก็บ date เป็น millisecond ทั้งระบบ ซึ่งกระทบ range query,
histogram, sort, format ทั้งหมด

### refresh ถึงทีละ shard — 1 section

`delete/50_refresh` — index หนึ่งที่นี่คือ tantivy index เดียว มี writer เดียว
`refresh` จึงทำให้ทุกอย่างเห็นพร้อมกัน ไม่มีทางให้ delete บน shard หนึ่ง
ยังมองไม่เห็นขณะที่อีก shard เห็นแล้ว · ต้องมี store แยกต่อ shard จริง ๆ

(ส่วนอีกเคสที่เคยอยู่กลุ่มนี้ — HDR negative values — ปิดได้แล้ว เพราะ
routing hash ตรงแล้ว จึงรู้ว่าเอกสารตัวปัญหาอยู่ shard ไหนและตัดทั้ง shard ทิ้งได้)

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

    OBSEARCH_NODE_ATTRS=testattr=test ./target/release/obsearch
