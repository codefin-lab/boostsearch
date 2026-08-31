# Custom columnar fast-field layout — วัดแล้วคุ้มไหม

**คำถาม:** ออกแบบ column format เองให้ block-skipping + compression + SIMD + planner + aggregation
ใช้ข้อมูลชุดเดียวกันตั้งแต่ disk ถึง CPU — ทำแล้วเร็วขึ้นจริงไหม

**คำตอบสั้น:** เร็วขึ้นจริงและมาก **แต่เฉพาะ column ที่ค่าเรียงเป็นกลุ่ม (เช่นเวลา)**
และ **ไม่ต้องเขียน format เองก็ได้ประโยชน์เกือบทั้งหมด** — วัดได้ **40 เท่า** ที่ 2M docs

## 1. tantivy มีอะไรอยู่แล้ว

| ที่ขอ | สถานะจริง |
|---|---|
| bitpacking | **มี** — `CodecType::Bitpacked` + GCD + `BlockwiseLinear` (บล็อกละ 512) เลือกอัตโนมัติ |
| dictionary encoding | **มี** — sstable term dictionary + term ordinals |
| column-level min/max skip | **มี** — range ที่ไม่ทับช่วงของทั้ง column ถูกตัดทิ้งทันที |
| batch access | **มี** — `ColumnBlockAccessor::fetch_block` (เราเพิ่งเริ่มใช้) |
| **per-block min/max สำหรับ skip** | **ไม่มี** ← ช่องว่างจริง |
| **SIMD ใน columnar decode** | **ไม่มี** — ใช้ `bitpacker1x` ไม่ใช่ `bitpacker4x` |
| bloom filter | ไม่มี |

TODO ของ tantivy เองก็เขียนไว้ว่า `SIMD range? (see blog post)` และ
`improv perf of select using PDEP` — ต้นน้ำรู้ว่ายังมีช่อง

## 2. หลักฐานว่าปัญหามีอยู่จริง

query ที่กรองด้วยช่วงเวลาแคบ ๆ (คืน 2,000 hits เท่าเดิม) เมื่อ index โตขึ้น 10 เท่า:

| ขนาด index | เวลา | hits |
|---|---:|---:|
| 200,000 docs | 153 µs | 2,000 |
| 2,000,000 docs | **641 µs** | 2,000 |

**ต้นทุนโตตามขนาด index ไม่ใช่ตามจำนวนผลลัพธ์** — คือลายเซ็นของการ scan ที่ skip ไม่ได้

เทียบกับ term query ที่คืน 399,689 hits ใช้แค่ 323 µs — เพราะ postings list
เก็บ doc id ตรง ๆ จึง skip ได้อยู่แล้ว ⇒ **range บน fast field scan ส่วน postings skip**

## 3. วัดเพดาน (2,000,000 docs, column `@timestamp`, segment เดียวกัน, ผลลัพธ์เท่ากัน 194,704 hits)

| วิธี | เวลา | เทียบ |
|---|---:|---|
| tantivy `get_docids_for_value_range` (ที่ใช้อยู่) | 521 µs | baseline |
| scalar scan บน `Vec<u64>` ที่ materialize แล้ว | 657 µs | ช้ากว่า |
| chunked "auto-vectorised" ที่ผมเขียนเอง | 1,118 µs | **ช้ากว่าเกือบ 2 เท่า** |
| block min/max skipping (ค่าดิบ) | 30 µs | 17x |
| **sidecar block-stats + API เดิมของ tantivy** | **13 µs** | **40x** |

ที่ 200,000 docs: 33 µs → 1 µs (**33x**)

### สิ่งที่ต้องพูดให้ชัด

**ได้เฉพาะ column ที่ค่าเรียงเป็นกลุ่ม** — `@timestamp` เรียงตามเวลา ⇒ 2,923 จาก 3,907
บล็อก skip ได้ทั้งบล็อก และอีก 972 บล็อกอยู่ในช่วงทั้งหมด (คืน doc id ได้เลยไม่ต้องเทียบค่า)

ส่วน `response_ms` ที่ค่ากระจายสุ่ม: **0 บล็อกที่ skip ได้** ⇒ block skipping ไม่ช่วยอะไรเลย
(3,994 µs เทียบกับ scalar 4,173 µs คือ noise)

**SIMD ไม่ใช่ของฟรี** — chunked scan ที่ผมเขียนให้ LLVM auto-vectorise **ช้าลง 70%**
การเขียนให้ compiler vectorise ได้จริงต้องระวังกว่านี้มาก และ tantivy ก็ decode
จากรูปแบบบีบอัดอยู่แล้ว ไม่ได้อ่านจาก array ดิบ

## 4. ข่าวดี: ไม่ต้องเขียน format เอง

`Column::get_docids_for_value_range(value_range, selected_docid_range, out)` **รับช่วง doc id เข้ามาได้อยู่แล้ว**

จึงทำ **sidecar** เก็บ min/max ต่อบล็อก 512 doc แยกไฟล์ต่อ segment แล้ว:
1. บล็อกที่ `max < lo` หรือ `min > hi` → ข้ามทั้งบล็อก ไม่เรียก tantivy เลย
2. บล็อกที่ `min >= lo && max <= hi` → คืน doc id ทั้งช่วงตรง ๆ **ไม่ต้องเทียบค่าเลยสักตัว**
3. เหลือเฉพาะบล็อกที่คาบเกี่ยว → เรียก API เดิมด้วยช่วง doc id แคบ ๆ

ได้ 40 เท่า **โดยไม่แตะ format ของ tantivy** ไม่ต้อง fork `tantivy-columnar` (~7k LOC)

## 5. ผลต่อ query จริงคาดว่าเท่าไหร่

ที่ 2M docs, query mix ปัจจุบัน (รวม 34,625 µs):

| query | ตอนนี้ | คาดหลังทำ |
|---|---:|---:|
| time_range_1pct | 641 µs | ~60 µs |
| time_range_25pct | 748 µs | ~150 µs |
| time_range_agg | 1,461 µs | ~800 µs (agg ครองเวลา) |
| range_numeric (ค่าสุ่ม) | 2,040 µs | ไม่เปลี่ยน |

**ผลทบต้นที่สำคัญกว่า:** ใน log workload จริง แทบทุก query มี time filter
และ aggregation ทำงานบน subset ที่กรองแล้ว ถ้า filter ถูกลงจาก 748 → 150 µs
aggregation ที่ตามมาก็ทำงานบนชุดที่เล็กลงด้วย — นี่คือ "วิ่งทะลุทั้ง stack" ที่ว่า

แต่ query mix ที่ผมใช้วัดอยู่ **ไม่สมจริงสำหรับ log**: 47% ของเวลาเป็น aggregation
ที่ scan ทั้ง index โดยไม่มี time filter ซึ่งไม่ใช่รูปแบบที่คนใช้จริง

## 6. สรุปสำหรับการตัดสินใจ

| | |
|---|---|
| **ทำแล้วเร็วขึ้นไหม** | เร็วขึ้น **40 เท่า** บน time-range ที่ 2M docs และช่องว่างโตตามขนาด index |
| **ต้องเขียน columnar format เองไหม** | **ไม่ต้อง** — sidecar block-stats ได้ผลเท่ากันโดยใช้ API เดิม |
| **SIMD คุ้มไหม** | ยังไม่มีหลักฐาน — ที่ลองเองช้าลง 70% ต้องวัดก่อนทำ |
| **compression / dictionary encoding** | tantivy ทำแล้ว ทำซ้ำไม่ได้อะไร |
| **planner ใช้ข้อมูลชุดเดียวกัน** | sidecar เป็นข้อมูลที่ planner ใช้ตัดสินได้ด้วย (จำนวน block ที่รอด = cost estimate ที่แม่นและได้มาฟรี) |
| **ได้เฉพาะเมื่อไหร่** | column ที่ค่าเรียงเป็นกลุ่ม — เวลา, id ที่เพิ่มขึ้นเรื่อย ๆ, ค่าที่สัมพันธ์กับลำดับการ index ⇒ ค่าสุ่มไม่ได้อะไร |

## 7. ถ้าจะทำจริง ต้องทำอะไรบ้าง

1. คำนวณ min/max ต่อบล็อกของทุก numeric fast-field column ตอน commit แล้วเขียนลงไฟล์ข้าง segment
2. สร้างใหม่ตอน merge (segment ใหม่ = สถิติใหม่)
3. เสียบเข้า `build_range` ให้สร้าง DocSet จาก sidecar แทน `RangeQuery` ของ tantivy
4. จัดการ multi-valued column, ค่าที่ขาด, และ deleted docs
5. ให้ planner อ่านจำนวนบล็อกที่รอดไปประเมิน cost

ประเมิน ~400-600 LOC ไม่ใช่ fork — และวัดผลได้ด้วย harness ที่มีอยู่แล้ว

**โค้ดทดลองอยู่ที่ `src/bin/colbench.rs` ทำซ้ำได้ด้วย `BOOSTSEARCH_DATA=<dir> ./target/release/colbench`**

---

# ทำจริงแล้ว: `src/blockstats.rs`

## สิ่งที่ทำ

- เก็บ min/max ต่อบล็อก 512 doc ของทุก numeric fast-field column
- **สร้างตอนใช้ครั้งแรก แล้ว cache ตาม segment id** ไม่ต้อง persist:
  segment id ของ tantivy ไม่มีวันชี้ไปข้อมูลอื่น และการ merge สร้าง segment ใหม่
  ⇒ cache key เป๊ะโดยธรรมชาติ ไม่ต้องมี invalidation logic
- range query ใหม่: ข้ามบล็อกที่แมตช์ไม่ได้ · บล็อกที่อยู่ในช่วงทั้งหมดคืน doc id
  ตรง ๆ ไม่เทียบค่าเลย · เหลือเฉพาะบล็อกคาบเกี่ยวจึงเรียก API เดิมของ tantivy

## ความถูกต้องที่ต้องระวัง

- **เอกสารหลายค่า** — สถิติต้องรวมทุกค่าไม่ใช่แค่ค่าแรก ไม่งั้นค่าที่ซ่อนอยู่จะถูกข้ามทิ้ง
- **บล็อกที่อยู่ในช่วงทั้งหมด** ใช้ทางลัดได้เฉพาะเมื่อทุก doc มีค่า
  (`cardinality().is_full()`) ไม่งั้นจะกวาด doc ที่ไม่มีค่าติดมาด้วย
- **bound แบบ exclusive บน float** ขยับค่าไม่ได้แบบ integer จึงตกกลับไปทางเดิม
- ตรวจด้วย differential test: **45 range query ให้ผลตรงกันเป๊ะทั้งเปิดและปิด**

## กับดัก: เร็วขึ้นบางอย่าง แต่ทำอย่างอื่นช้าลง

เวอร์ชันแรกใช้ block scan เสมอ ผลคือ:

| | เปลี่ยน |
|---|---:|
| time_range_1pct | −80% |
| time_range_25pct | −53% |
| **bool_filter** | **+22%** |
| **range_float_bound** | **+12%** |
| range_numeric | +10% |

เพราะ block scan **materialize ผลทั้งหมดล่วงหน้า** ส่วน range ของ tantivy stream
แบบ lazy ให้ intersection ข้างนอกข้ามไปข้างหน้าได้ — บน field ที่ค่ากระจายสุ่ม
(skip ได้ 0 บล็อก) จึงจ่ายค่า materialize ฟรี ๆ

## ทางแก้: ให้สถิติเป็นตัวตัดสินเอง

`weight()` ถาม skip ratio จากสถิติก่อน ถ้าต่ำกว่า 25% ก็ส่งงานให้ range query
ทั่วไปทำแทน — **นี่คือ sidecar ทำหน้าที่เป็น planner input ในตัว**
ต้นทุนของการถามคือการวน block header ซึ่งถูกกว่าการอ่านค่าจริงหลายเท่า

| query | ปิด sidecar | เปิด sidecar | เปลี่ยน |
|---|---:|---:|---:|
| time_range_1pct | 623 µs | 225 µs | **−64%** |
| time_range_25pct | 742 µs | 430 µs | **−42%** |
| time_range_agg | 1,435 µs | 818 µs | **−43%** |
| range_numeric (ค่าสุ่ม) | 2,006 µs | 2,100 µs | +5% |
| bool_filter | 3,710 µs | 3,735 µs | +1% |
| **รวม** | 31,528 µs | 29,824 µs | **−5%** |

regression หายหมด เหลือแค่ค่าที่อยู่ในช่วง noise

## ผลปลายทาง

พอเพิ่ม time-filtered query เข้า benchmark mix (25% ของน้ำหนัก — ใกล้ log workload จริง):

| | boostsearch | OpenSearch |
|---|---:|---:|
| time_range p50 | **1.45 ms** | 2.07 ms |
| qps c=1 | **562** | 430 |
| p99 c=1 | **3.01 ms** | 4.06 ms |
| memory | **265 MB** | 1,533 MB |
