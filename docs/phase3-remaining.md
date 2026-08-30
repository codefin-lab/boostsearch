# Phase 3 — ที่เหลือ และบทเรียนระหว่างทาง

Phase 3 อยู่ที่ **1,095 / 1,100** · Phase 1 อยู่ที่ **397 / 398**
รายชื่อที่ตกจริง ๆ ให้ดูจาก runner เสมอ ไฟล์นี้เก็บเฉพาะ *เหตุผล* ที่ห้าตัวสุดท้าย
ไปต่อไม่ได้ กับบทเรียนที่ไม่อยากให้หายไปกับ commit

## ห้าตัวที่เหลือ

### shard ที่นี่เป็นเรื่องของ routing ไม่ใช่ที่เก็บ — 4 sections

index หนึ่งใน obsearch คือ tantivy index เดียว `number_of_shards` มีผลกับการ
routing และถูกรายงานกลับไปตามที่ตั้ง แต่ไม่มี store แยกต่อ shard สี่ตัวนี้ต่างกัน
ตรงนั้น และต้องมี per-shard store จริง ๆ ถึงจะแก้ได้ ไม่ใช่แก้ตรงจุด:

- `delete/50_refresh` — refresh ลงที่ shard เดียว ของที่ลบบน shard อื่นจึงยัง
  มองเห็นอยู่ · ที่นี่ refresh ถึงทุกอย่างพร้อมกัน
- `search.aggregation/190_percentiles_hdr_metric :: Negative values test` —
  ค่าติดลบทำให้ shard ที่ถือมันพัง search จึงตอบ 4 hits จาก 5 พร้อม shard
  failure · ที่นี่ไม่มี shard ให้พัง
- `search.aggregation/370_multi_terms :: sum_other_doc_count` — `shard_size`
  ตัดคำตอบของแต่ละ shard *ก่อน* merge ตัวเลขที่ได้จึงน้อยกว่าความจริงโดยตั้งใจ ·
  ที่นี่ aggregation เห็นทั้ง index พร้อมกัน
- `scroll/12_slices :: Sliced scroll` — เอกสารตกอยู่ slice ไหน มาจาก shard ที่มันอยู่

เรื่อง routing hash ของ slice มีบันทึกแยกไว้ข้างล่าง

### วันที่ที่เกินกว่าที่ date เก็บได้ — 1 section

`search.aggregation/40_range :: Date Range Missing` เขียน epoch **seconds**
ระดับหลักแสนล้าน = ปี 11970 · tantivy เก็บ date เป็น i64 nanosecond ซึ่งหมดที่
ปี 2262 ⇒ เก็บไม่ได้เลย ไม่ใช่แค่ปัดทิ้งความละเอียด และถ้า clamp ก็เสียลำดับ
ซึ่งเป็นสิ่งเดียวที่ aggregation นี้ถาม

## Sliced search: การแบ่งถูก แต่ hash ไม่ตรง

slice แบ่ง index ให้ผู้อ่านหลายคน ถ้ามี shard ไม่น้อยกว่าจำนวน slice แต่ละ slice
รับ shard ที่เลขตกถึงมัน (`shard % max == id`) และเอกสารอยู่ shard ไหนก็มาจาก id
ของมัน โครงนี้ทำแล้วและให้จำนวนถูก

ที่ไม่ตรงคือเอกสารไหนไปอยู่ shard ไหน · hash ของเราเป็น murmurhash3_x86_32 ของ id
seed 0 พับด้วยจำนวน shard ของเขาแบ่งคนละแบบ · **14 seed ที่ต่างกันให้ผลตรงกับ
ตัวอย่าง 4 เอกสารในชุดทดสอบ** ซึ่งเป็นสัญญาณว่าการเลือกสักอันคือการ *เดาเลข*
ไม่ใช่การ *ทำตามกฎ*

## บทเรียนที่ยังใช้ได้

### closed index — ค่าเริ่มต้นของ `expand_wildcards` ต่างกันตาม API

`_close`/`_open` ตั้ง flag ถูกมาตลอด แต่ตอนแรกการ resolve ไม่เคยสนใจ flag นั้น ·
ครั้งแรกที่แก้โดยทำให้ทุก wildcard เป็น open-only → **พัง 229 sections** เพราะ
`DELETE /*` ของ runner ไม่ลบ closed index อีกต่อไป แล้วมันค้างข้ามไฟล์

ค่าเริ่มต้นต่างกันจริง ๆ: `open` สำหรับการอ่านเอกสารและ `_resolve/index` ·
`open,closed` สำหรับ delete/cat/stats · **`all` สำหรับ cluster health** ⇒ ทำเป็น
opt-in ต่อ endpoint ไม่ใช่เปลี่ยนค่าเริ่มต้นรวม

### aggregation ที่ peel ออกมา ซ้อนกันไม่ได้

`multi_terms`, `composite`, `date_histogram` แบบ calendar หรือแบบมี time zone,
`rare_terms`, percentiles — ถูก peel ออกจาก request ก่อนส่งให้ tantivy · sub-agg
ของมันวิ่งผ่าน `filtered_count` ซึ่งส่งต่อให้ tantivy parser ⇒ ถ้า sub-agg เป็น
ตัวที่ถูก peel เหมือนกันจะพัง · `run_peeled_agg` แก้กรณีที่พบแล้ว แต่การรื้อให้ครบ
ควรทำทีเดียวทั้งชุด ไม่ใช่ไล่ทีละอัน

### `_cat` — ช่องว่างท้ายบรรทัดไม่ใช่ padding แต่เป็นคอลัมน์ที่หายไป

เคยสรุปไว้ว่า `_cat` ของ OpenSearch เติมช่องว่างท้ายทุก cell แล้วลองทำสองครั้ง
ครั้งละ **−20 sections** · ที่จริง regex ที่ดูเหมือนต้องการช่องว่างท้ายบรรทัด
กำลังบอกว่ามี **คอลัมน์ว่างต่อท้าย** ที่เรายังไม่มี (`composed_of` ของ template,
replica ที่ยัง unassigned ของ shards) · เติมคอลัมน์ให้ครบแล้วผ่านหมดโดยไม่ต้อง
แตะ padding เลย

การจัดชิดขวาเป็นสมบัติของ *คอลัมน์* ไม่ใช่ของค่าที่บังเอิญเป็นตัวเลข

### HDR — scale มาจากข้อมูล ไม่ใช่ค่าคงที่

`DoubleHistogram` เปิดด้วยช่วงที่สูงเกินจริง (2^800) แล้วให้ค่าแรกที่บันทึกดึงมันลงมา
scale ที่ได้จึงมาจากข้อมูล · ค่าที่สูงกว่าช่วงจะเลื่อน scale **ก็ต่อเมื่อ** จำนวนเต็ม
ที่บันทึกไว้แล้วหารสองได้โดยไม่ตกลงไปครึ่งล่างของ bucket แรก มิฉะนั้นช่วงจะโตทางบน
โดย scale เท่าเดิม · ก่อนหน้านี้ใช้ค่าคงที่ที่ fit กับชุดทดสอบ อ่านกฎจริงจาก
HdrHistogram แล้วจึงได้ทั้งกลุ่ม

### t-digest ที่มีค่าไม่กี่ตัว เก็บครบทุกตัว

OpenSearch เก็บทุกค่าเมื่อจำนวนน้อย แล้วรายงานค่าที่อันดับตรงกับ percentile
(`ceil(p/100 * n)`) · sketch แบบประมาณตอบคนละคำถามเล็กน้อย (1.0 กลายเป็น 1.01)

## node ที่ชุดทดสอบคาดหวัง

test cluster ของ OpenSearch เปิดด้วย `node.attr.testattr: test` และมีเทสอ่านมันกลับ
ผ่าน `_cat/nodeattrs` กับ `_cluster/settings` ⇒ เปิดเซิร์ฟเวอร์แบบเดียวกัน:

    OBSEARCH_NODE_ATTRS=testattr=test ./target/release/obsearch
