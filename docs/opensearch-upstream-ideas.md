# ไปดูกระดานของ OpenSearch แล้วได้อะไรกลับมา

อ่าน issue/PR ฝั่ง OpenSearch เพื่อหาว่ายังเหลืออะไรให้ optimize

## สิ่งที่เขากำลังทำ

| งาน | ที่อ้างอิง | เคลมไว้ |
|---|---|---|
| Skip list บน doc values (Lucene 10) | [#19384](https://github.com/opensearch-project/OpenSearch/issues/19384) | สูงสุด **2×** สำหรับ aggregation |
| Bulk collection API (vectorized) | [#19324](https://github.com/opensearch-project/OpenSearch/issues/19324), [#19933](https://github.com/opensearch-project/OpenSearch/pull/19933) | ลด overhead ต่อเอกสาร |
| `collectRange` อ่านข้อมูล pre-aggregate | [#20009](https://github.com/opensearch-project/OpenSearch/pull/20009) | — |
| Percentile ที่มีประสิทธิภาพขึ้น | [#19622](https://github.com/opensearch-project/OpenSearch/issues/19622) | — |
| Streaming aggregation | [roadmap 2026](https://opensearch.org/blog/the-2026-opensearch-roadmap-four-pillars-for-ai-native-innovation/) | perceived latency **3×**, ทรัพยากร **2×** |
| Query engine รวม ๆ | [#20031](https://github.com/opensearch-project/OpenSearch/issues/20031) | throughput **2×** ภายในสิ้นปี 2026 |

**Skip list ทำอะไร**: เอา doc-id set จาก filter มาทาบกับ skip interval ของ field
ที่ agg ถ้าทั้งช่วงตกลงใน bucket เดียวกัน ก็นับทั้งช่วงด้วย `popcount`
แทนการวนทีละเอกสาร — เงื่อนไขคือ **ลำดับเอกสารต้องสัมพันธ์กับ field ที่ bucket**

## ที่ได้จริงจากการอ่าน: เจอ regression ของตัวเอง

tantivy **มี `collect_block` ให้ aggregation อยู่แล้ว** (คือสิ่งที่ OpenSearch
กำลังสร้างใน #19324) แต่ `MaybeAgg` ที่เพิ่งเขียนไปรอบก่อนไม่ได้ forward ต่อ
default implementation จึงคลี่กลับเป็นเรียก `collect` ทีละเอกสาร

| | ก่อน | หลัง forward |
|---|---:|---:|
| agg_nested | 0.646 ms | **0.556 ms** |
| terms only | 0.418 ms | **0.351 ms** |

⇒ 14–16% และ `agg_nested` เทียบ OpenSearch ขยับจาก 1.01× เป็น **1.12×**

## สองอย่างที่วัดแล้วไม่คุ้ม — วัดก่อนแก้

**1. ย้าย date ไปคอลัมน์ `_dyn`** — การ route ตัวเลขไป `_dyn` เคยได้ 25%
(`agg_terms` 230 → 172 µs) แต่ปัจจุบัน `date` ถูกกันออกจากกฎนั้น ลองวัดดู:

| | collect |
|---|---:|
| `_raw.@timestamp` | 191 µs |
| `_dyn.@timestamp` | 186 µs |

ต่างกัน 2.6% = noise **ไม่แก้**

**2. Skip list / block pre-aggregation** — เรามี `src/blockstats.rs` ที่เก็บ
min/max ต่อ block อยู่แล้ว ต่อยอดได้ แต่:

- dataset benchmark เรียงตามเวลา 100% ⇒ `date_histogram` จะได้ประโยชน์เต็ม
- `terms(region)` / `agg_nested` **ไม่ได้ประโยชน์เลย** เพราะ region กระจายสุ่ม
  ไม่สัมพันธ์กับลำดับเอกสาร — เงื่อนไขเดียวกับที่ฝั่ง OpenSearch ระบุไว้เอง
- เพดานของ `date_histogram` คือ 191 µs จาก query ที่ใช้ ~1.13 ms ⇒ ต่อให้
  bucketing ฟรี ก็ได้คืนไม่เกิน ~11% ของ query นั้น และ 0% กับตัวอื่นในชุด

งานระดับ sidecar เพื่อ ~11% ของ query แบบเดียว — **ยังไม่ทำ** บันทึกไว้เป็น
ทางเลือกถ้าภายหลัง workload จริงเป็น date_histogram เป็นหลัก

## ข้อควรรู้เชิงกลยุทธ์

OpenSearch ตั้งเป้า **2× ที่ aggregation** และ **2× throughput ภายในสิ้นปี 2026**
ตอนนี้เรานำด้าน aggregation อยู่ 1.12–1.33× — **ถ้างานของเขาลงจริง เขาจะแซง
ด้าน aggregation**

สิ่งที่เรานำแล้วเขาไล่ตามยากกว่าคือ **memory (9.5×) และ fan-out หลาย index
(1.3–3.4×)** ไม่ใช่ latency ของ aggregation บน index เดียว การวางแผนต่อควรยึด
สองอย่างนั้นเป็นจุดขาย ไม่ใช่ตัวเลข agg
