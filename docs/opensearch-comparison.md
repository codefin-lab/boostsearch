# วัดเทียบกับ OpenSearch ตัวจริง

## วิธีวัด

| | obsearch | OpenSearch |
|---|---|---|
| เวอร์ชัน | build ปัจจุบัน (tantivy 0.26.1) | **3.1.0** (Lucene 10.2.1) |
| รันแบบ | container `debian:bookworm-slim` | container ทางการ `opensearchproject/opensearch` |
| storage | `OBSEARCH_DATA` (mmap, docker volume) | ค่าเริ่มต้น |
| heap / memory | ไม่มี heap (native) | `-Xms512m -Xmx512m` |
| shard | 1 | 1 |

ทั้งสองตัวรันใน Docker VM เดียวกัน ยิงจาก client เดียวกันบน host ผ่าน published port
ใช้ corpus เดียวกัน (200,000 http-log docs / 53 MB, seed คงที่) และ query mix เดียวกัน
**รันฝั่งละ 5 รอบ รายงานค่ามัธยฐานพร้อมช่วง min-max**

## ผล (หลังรอบ storage/execution)

วัดฝั่งละ 5 รอบในเซสชันเดียวกัน รายงานมัธยฐานพร้อมช่วง min-max

| | obsearch | OpenSearch 3.1.0 | |
|---|---:|---:|---|
| index docs/s | 59,276 `[57.5k-60.0k]` | **70,449** `[57.7k-70.8k]` | OpenSearch +19% |
| memory (MB) | **369** `[208-491]` | 1,134 `[1076-1187]` | obsearch **น้อยกว่า 3.1 เท่า** |
| qps c=1 | **424** `[405-426]` | 360 `[278-408]` | obsearch +18% |
| p50 c=1 | **2.26 ms** `[2.22-2.35]` | 2.62 ms `[2.30-3.45]` | obsearch |
| p99 c=1 | **3.69 ms** `[3.41-4.23]` | 4.09 ms `[3.68-5.33]` | เสมอ (ช่วงทับกัน) |
| qps c=8 | **1,826** `[1668-1864]` | 1,608 `[1399-1668]` | obsearch **+14%** |
| p50 c=8 | **4.18 ms** `[4.02-4.45]` | 4.64 ms `[4.35-5.32]` | obsearch |
| p99 c=8 | **6.70 ms** `[6.16-7.60]` | 7.83 ms `[6.58-15.16]` | obsearch |
| cold start (200k docs) | **13-638 ms** | ~6.3 s (restart container) | obsearch |

### ที่ขยับจากรอบก่อน

| | ก่อน | หลัง | ช่องว่างกับ OpenSearch |
|---|---:|---:|---|
| index docs/s | 55,164 | 60,124 | −29% → **−15%** |
| qps c=1 | 368 | 443 | −15% → **เสมอ** |
| p50 c=1 | 2.53 ms | 2.06 ms | −14% → **เสมอ** |
| p99 c=1 | 5.00 ms | 4.51 ms | −26% → **เสมอ** |
| p99 c=8 | 8.77 ms | 7.43 ms | −12% → **เสมอ** |

## สรุปตรง ๆ

**หน่วยความจำเราน้อยกว่า 3.1 เท่า · query เสมอกันทุกตัวชี้วัด · indexing ยังช้ากว่า 15%**

ช่วง min-max ของทุกตัวชี้วัดฝั่ง query ทับกันหมด แยกไม่ออกด้วยข้อมูลชุดนี้ —
พูดว่า "เสมอ" ถูกต้องกว่าพูดว่าใครชนะ

นี่ตรงกับที่ [การศึกษาตอนต้น](tantivy-study.md) สรุปไว้พอดี:

> กำไรใหญ่ที่สุดคือ **memory ไม่ใช่ raw search speed** ... p50 อาจไม่ต่างมาก
> เพราะ Lucene ถูก optimize มา 20 ปีและ JIT ทำงานดีใน hot loop

ตัวเลขยืนยันคำเตือนนั้น ไม่ใช่หักล้าง

### รายละเอียดที่ต้องพูด

**Indexing ช้ากว่า 29%** — ระหว่างการวัดรอบนี้แก้ไปสามจุดแล้ว (ไม่ queue delete
ต่อเอกสารใหม่, ไม่ deep-copy JSON สองรอบ, ข้าม dynamic-type walk สำหรับ shape ซ้ำ)
ได้จาก 44.7k → 55.2k docs/s แต่ยังตามหลัง Lucene's indexing chain อยู่

**p99 ที่ concurrency 1 แพ้ 26%** — ทดสอบแล้วว่าไม่ใช่เรื่อง search executor
(วัด 0 / 2 / 4 / default threads แล้ว default ดีสุดทุกมิติ) ยังไม่รู้สาเหตุ

**ที่ concurrency 8 เสมอกัน** ช่วง min-max ทับกันหมด แยกไม่ออกด้วยข้อมูลชุดนี้

### JVM heap: ใหญ่ไม่ได้แปลว่าดี

ลอง OpenSearch ที่ heap 2 GB ก่อน แล้วได้ **แย่กว่า** heap 512 MB ทุกด้าน:

| heap | index docs/s | memory | qps c=1 |
|---|---:|---:|---:|
| 2 GB | 50,783 | 2,819 MB | 247 |
| **512 MB** | **69,145** | **1,048 MB** | **287** |

ถ้าเทียบกับ 2 GB เราจะดู "ประหยัดกว่า 7 เท่า" ซึ่งจะเป็นการเลือกคู่เทียบที่เข้าข้างตัวเอง
ตัวเลข 3 เท่าที่รายงานคือเทียบกับ config ที่ OpenSearch ทำได้ดีที่สุด

## ข้อจำกัดของการวัดครั้งนี้ (ต้องอ่าน)

1. **เครื่องมีงานอื่นรันอยู่หนักมาก** — Docker VM เดียวกันมี container อีก **19 ตัว**
   (k8s stack เต็ม ๆ: keycloak, istio, apollo-router, airflow scheduler/triggerer/dag-processor,
   customer-service) การแย่งทรัพยากรทำให้ช่วง min-max กว้างและกดความต่างให้แคบลง
   ตัวเลขชุดนี้ **ไม่ควรใช้อ้างอิงเป็นค่าสัมบูรณ์**
2. **obsearch ยังไม่สมบูรณ์** — ผ่าน conformance 74.6% ของ Phase 1 suite
   ขณะที่ OpenSearch ทำได้ครบ ระบบที่ทำงานน้อยกว่าต่อ request ย่อมได้เปรียบบางด้าน
   การเทียบนี้จึงเข้าข้างเราอยู่แล้วในเชิงโครงสร้าง
3. **workload เดียว** — http-log analytics, 200k docs, shard เดียว, node เดียว
   ยังไม่ได้วัด: dataset ที่ใหญ่กว่า RAM, cold cache, หลาย shard, การ merge ระยะยาว,
   nested/geo/percolate (ที่เรายังไม่รองรับ)
4. **Docker overhead สูงมาก** — obsearch แบบ native ทำได้ 71,458 docs/s และ qps c=1 805
   พอเข้า container เหลือ 55,164 และ 368 ทั้งคู่จ่ายค่านี้เท่ากันจึงยุติธรรม
   แต่ตัวเลขสัมบูรณ์ถูกกดลงมาก

## ทำซ้ำได้

```bash
docker run -d --name os-bench -p 9201:9200 -e discovery.type=single-node \
  -e DISABLE_SECURITY_PLUGIN=true -e DISABLE_INSTALL_DEMO_CONFIG=true \
  -e "OPENSEARCH_JAVA_OPTS=-Xms512m -Xmx512m" opensearchproject/opensearch:3.1.0

docker run --rm -v "$PWD":/src -w /src rust:1.88-slim \
  cargo build --release --target-dir /src/target-linux
cp target-linux/release/obsearch bench/docker/ && docker build -t obsearch:bench bench/docker
docker run -d --name obsearch-bench -p 9202:9200 -e OBSEARCH_DATA=/data \
  -v obsearch-data:/data obsearch:bench

python3 tools/gen_dataset.py --docs 200000
python3 tools/bench.py --url http://127.0.0.1:9202 --proc docker:obsearch-bench --label obsearch
python3 tools/bench.py --url http://127.0.0.1:9201 --proc docker:os-bench --label opensearch
```

## ผลต่อการตัดสินใจ

Go/No-Go gate ที่ตั้งไว้ใน [แผนแรก](opensearch-to-rust-plan.md) บอกว่า:
Phase 2 ต้องเห็น RSS ลด ≥50% พร้อม latency ไม่แย่ลง

- ✅ memory ลด **66%** (404 vs 1,204 MB)
- ⚠️ latency **แย่ลงเล็กน้อย** ที่ concurrency ต่ำ (p50 +14%, p99 +26%), เสมอที่ concurrency สูง
- ❌ indexing **ช้ากว่า 29%**

⇒ ถ้าเป้าหมายคือ **ลด instance / ลดค่า cloud** เส้นทางนี้ยังคุ้ม
⇒ ถ้าเป้าหมายคือ **query เร็วขึ้น** ยังไม่มีหลักฐานสนับสนุน และควรทบทวนก่อนลงทุนต่อ

---

# ภาคผนวก: ไล่หา indexing กับ p99 (วัดก่อนแก้ทุกครั้ง)

## เครื่องมือ

`src/bin/idxbench.rs` ขับ write path เดียวกับ HTTP handler แต่จับเวลาแยกทีละขั้น
ทำให้เห็นว่าเวลาไปอยู่ไหนจริง ๆ แทนที่จะเดา

## สมมติฐานแรกผิด

เดาว่าคอขวดคือการจับ write lock ต่อเอกสารและ `store.ensure()` ต่อเอกสารในลูป bulk
วัดแล้วได้ **0.00 และ 0.01 µs/doc** — ไม่มีนัยสำคัญเลย ดีที่วัดก่อน

## เวลาไปอยู่ไหนจริง (200k docs, 2,140 ms wall)

| ขั้น | เวลา | % ของ wall |
|---|---:|---:|
| `make_doc` (สร้าง `_dyn` + `_raw`) | 506 ms | 23.6% |
| `writer.add_document` | 140 ms | 6.5% |
| serialize `_source` | 134 ms | 6.3% |
| parse doc JSON | 189 ms | 8.8% |
| `note_pending` | 92 ms | 4.3% |
| `bump` + `observe` | 60 ms | 2.8% |
| อื่น ๆ ใน `bulk()` (ensure/lock/สร้าง item JSON) | ~295 ms | 13.8% |
| นอก `bulk()` (HTTP + client parse response) | 578 ms | 27.0% |

ตรวจแล้วว่า response ของ bulk **ขนาดเท่ากับ OpenSearch เป๊ะ** (33,615 vs 33,616 bytes ต่อ 200 items)
ดังนั้น 27% นั้นเป็นภาระที่ทั้งสองฝั่งจ่ายเท่ากัน ปิดช่องว่างต้องแก้ฝั่ง server

## ที่แก้แล้วได้ผล

- **ใช้ข้อความ `_source` ที่ client ส่งมาแทนการ serialize `Value` ใหม่** — ตัด 134 ms ทิ้ง
  และตรงกับ OpenSearch มากกว่าด้วย (มันเก็บ source ตามที่ส่งมา)
- **ตัดเอกสารที่ไม่มีทางติด top-K ทิ้งก่อนจัดสรรหน่วยความจำ** — เดิม sort จัดสรร
  `Vec<SortValue>` ต่อทุกเอกสารที่ match (200k ครั้ง) แล้วค่อย prune
  ตอนนี้อ่าน sort key ตัวแรกมาเทียบกับ "ตัวที่แย่ที่สุดที่เก็บไว้" ก่อน ไม่ผ่านก็ทิ้งเลย

  ⇒ `sort_paged` p50 4.54 → ~2 ms, qps c=1 368 → 443

## ที่ลองแล้วไม่คุ้ม — บันทึกไว้กันลองซ้ำ

**เอา fast field ออกจาก `_raw`** เหตุผลคือ `_dyn` มี fast field แบบไม่ tokenize อยู่แล้ว
จึงรับงาน sort/agg แทนได้ ส่วน `_raw` เหลือหน้าที่แค่ exact term matching

วัดได้ `make_doc` 506 → 440 ms (write core เร็วขึ้น 5.5%) **แต่พัง range query ทั้งหมด**:

```
RangeQuery on JSON is only supported for fast fields currently
```

tantivy รองรับ `RangeQuery` บน JSON field เฉพาะเมื่อ field นั้นมี fast field
⇒ **dual columnar ตัดออกไม่ได้** ต้นทุน ~0.88 µs/doc ของการ index สองมุมมอง
เป็นราคาเชิงโครงสร้างของทริค dynamic mapping ที่เลือกไว้ ไม่ใช่ของที่ optimize ทิ้งได้

วัดทางเลือกของ `make_doc` ครบแล้ว:

| วิธี | µs/doc |
|---|---:|
| move + clone (ที่ใช้อยู่) | 2.02 |
| แปลงสองรอบจาก source | 2.02 |
| `_raw` เก็บเฉพาะ string | 2.08 |
| มุมมองเดียว (พื้นล่างทางทฤษฎี) | 1.14 |

## ที่ยังเหลือ

- **indexing ยังช้ากว่า 15%** ส่วนที่เหลือส่วนใหญ่คือ dual-view (โครงสร้าง)
  กับ JSON parsing — ถ้าจะไล่ต่อคือเปลี่ยนไป `simd-json`/`sonic-rs` (~189 ms)
  และลดการจัดสรรใน item response
- **`range_numeric` p50 2.74 vs 2.11** — tantivy ใช้ range บน fast field
  ขณะที่ Lucene ใช้ BKD points ที่ skip ได้ เป็นความต่างเชิง data structure

---

# ภาคผนวก 2: รอบ storage / execution / startup

## วัดก่อนเลือกทำ

ก่อนแตะอะไร ตรวจว่า tantivy มีอะไรให้แล้วบ้าง (`tantivy-columnar 0.7`):

| ที่ขอ | สถานะ |
|---|---|
| bitpacking | **มีแล้ว** — `CodecType::Bitpacked` เลือกอัตโนมัติ |
| dictionary encoding | **มีแล้ว** — sstable term dict + term ordinals |
| block stats | **มีบางส่วน** — `BlockwiseLinear` เก็บพารามิเตอร์ต่อบล็อก 512 ค่า, มี column-level min/max/GCD แต่ไม่มี per-block min/max สำหรับ skip |
| batch/vectorized access | **มีแล้วแต่เราไม่ได้ใช้** — `ColumnBlockAccessor::fetch_block` |
| bloom filter | ไม่มี |

⇒ เขียน columnar format เองคือทำซ้ำของที่มี ช่องว่างจริงคือ **เราไม่ได้ใช้ block accessor**

จากนั้นวัดว่า query path เสียเวลาตรงไหน (instrument แล้วถอดออก):

| ขั้น | % ของเวลา server-side |
|---|---:|
| execution | **84.3%** |
| aggregation | 9.9% |
| fetch `_source` | 3.9% |
| **build query จาก JSON** | **0.5%** |

⇒ **query-plan cache แทบไม่ได้อะไร** (0.5%) ตัดทิ้งจากแผน
⇒ server-side ทั้งหมดแค่ ~0.42 ms/query จาก wall ~2 ms ⇒ **~75% ของ latency คือ HTTP + client**
ไม่ใช่ engine — งาน execution จึงต้องวัดที่ CPU/query ไม่ใช่ที่ wall clock

## ที่ทำ

### 1. Vectorized fast-field reads (`collect_block`)
tantivy เรียก `collect_block(&[DocId])` เมื่อไม่ต้องใช้คะแนน แต่เราใช้ default
ที่วนเรียก `collect()` ทีละ doc เปลี่ยนเป็นดึงทั้งบล็อกจาก columnar ครั้งเดียวด้วย
`ColumnBlockAccessor` — กันไว้เฉพาะ sort key เดียวบน column ตัวเลข **แบบค่าเดียวต่อ doc**
(หลายค่าต่อ doc บล็อกจะคืนหลายแถวและลดค่าไม่ได้)

### 2. Narrow typed range variants
`range` เดิมสร้าง `RangeQuery` หนึ่งตัวต่อชนิดที่เป็นไปได้ (I64/U64/F64) แล้ว union กัน
⇒ สอง scan ที่ไม่มีวันแมตช์อะไรเลย พิสูจน์ด้วยการยิง query เดียวกันด้วย bound
ที่เป็น int (3 variants) เทียบ float (1 variant): **559 vs 278 µs**

แก้โดยจำว่าแต่ละ field path เคยเก็บค่าชนิดไหนบ้าง (bitmask) แล้วสร้างเฉพาะ variant
ที่มีทางแมตช์ เก็บลง `_meta.json` ให้รอด restart

**กับดักที่เจอ:** เดิม `observe` ข้ามเอกสารที่ shape ซ้ำ แต่ shape ดูแค่ชื่อ key
ไม่ดูชนิดค่า ⇒ ต้องเดินทุกเอกสาร ทำให้ observe แพงขึ้น 0.15 → 1.19 µs/doc (indexing ตก 23%)
ต้นเหตุคือ `format!("{prefix}.{k}")` สร้าง String ต่อ field ต่อ doc
เปลี่ยนเป็น buffer ที่ใช้ซ้ำและ allocate เฉพาะตอนเจอ path ใหม่ ⇒ 0.34 µs/doc

**ความถูกต้อง:** index ที่เขียนไว้ก่อนมีฟีเจอร์นี้จะมีข้อมูลชนิดไม่ครบ
การ narrow ด้วยข้อมูลไม่ครบจะทำให้ผลหาย จึงมี flag `kinds_complete`
ที่เป็น false เมื่อ `_meta.json` ไม่มี `observed_kinds` และจะไม่ narrow เลย

### 3. Startup: lazy id table
เดิม reopen ต้อง scan `_id` ของทุกเอกสารก่อนรับ traffic วัดแล้วเป็น **99% ของ startup**
(1,433-2,062 ms เทียบกับ 14-18 ms ถ้าข้าม) ย้ายไปทำใน background thread นอก write lock
ระหว่างนั้น `is_live()` ถามดัชนีโดยตรงแทน

⇒ cold start กับ index 200k docs: **1,433-2,062 ms → 13-638 ms**

## ผล (A/B ใน build เดียว ข้อมูลชุดเดียว)

`OBSEARCH_NO_BLOCK_SORT=1` และ `OBSEARCH_NO_KIND_NARROW=1` ปิดทีละตัวได้

| query | ปิดทั้งคู่ | เปิดทั้งคู่ | เปลี่ยน |
|---|---:|---:|---:|
| sort_paged | 4,898 µs | 414 µs | **−92%** |
| range_numeric | 485 µs | 253 µs | **−48%** |
| bool_filter | 639 µs | 398 µs | **−38%** |
| count_only | 211 µs | 151 µs | −28% |
| term_numeric | 104 µs | 95 µs | −9% |
| match_all | 224 µs | 210 µs | −6% |
| agg_terms | 359 µs | 378 µs | +5% (noise, ไม่แตะ path นี้) |
| agg_date_hist | 374 µs | 411 µs | +10% (noise) |
| **รวม** | **8,112 µs** | **3,118 µs** | **−62%** |

ที่ปลายทาง (ผ่าน HTTP) เห็นผลชัดที่ concurrency สูงซึ่ง CPU เป็นคอขวดจริง:
qps c=8 **1,702 → 1,826**, p99 c=8 **7.43 → 6.70 ms** ส่วนที่ c=1 แทบไม่ขยับ
เพราะ HTTP ครองเวลาอยู่แล้ว — ตรงกับที่วัดไว้ตั้งแต่ต้น

## ที่ยังไม่ได้ทำ และเหตุผล

| รายการ | สถานะ |
|---|---|
| custom columnar layout / bitpacking / dictionary encoding | tantivy ทำแล้ว — เขียนเองคือ fork `tantivy-columnar` |
| per-block min/max สำหรับ skip, bloom filter | ยังไม่มีใน tantivy เป็นงานระดับแก้ crate ต้นน้ำ |
| cost-based planner | ต้องมี execution path ทางเลือกให้เลือกก่อน ตอนนี้มีทางเดียว |
| operator fusion / selection bitmap | tantivy fuse ผ่าน scorer composition อยู่แล้ว |
| query-plan cache | **วัดแล้วว่าไม่คุ้ม** — build query = 0.5% ของเวลา |
| block cache / postings cache | mmap + page cache ของ OS ทำหน้าที่นี้อยู่ |
| async WAL, adaptive merge scheduler | ยังไม่ได้แตะ |
| zero-copy JSON parsing | ยังไม่ได้ทำ — วัดไว้ที่ 189 ms (8.8% ของ indexing) |
| distributed / scatter-gather | ยังเป็น single node |
