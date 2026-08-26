# Phase 2 — Benchmark Baseline และรอบ optimize แรก

## Harness

- `tools/gen_dataset.py` — สร้าง dataset แบบ http-log **deterministic** (seed คงที่)
  200,000 เอกสาร / 53 MB รูปแบบเดียวกับ workload `http_logs` ของ OpenSearch Benchmark
- `tools/bench.py` — ยิงเข้า endpoint ที่เข้ากันได้กับ OpenSearch **ตัวไหนก็ได้**
  วัด indexing throughput, query latency (p50/p90/p99 ต่อ query type), qps,
  และ RSS ของ process จริง (`ps`) ที่ idle / หลัง index / หลัง search

Query mix ถ่วงน้ำหนักแบบงาน log analytics: term (25%), range (15%), bool+filter (15%),
match text (10%), match_all (10%), aggregation (20%), sort+paging (5%)

เครื่องทดสอบ: Apple Silicon 14 core / 36 GB

## ผลการวัด

| build | index docs/s | RSS หลัง index | RSS หลัง search | qps c=1 | p50 c=1 | qps c=8 | p99 c=8 | sort_paged p50 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| **baseline** | 82,737 | 867 MB | **5,133 MB** | 20.7 | 1.41 ms | 70 | **1,635 ms** | **930 ms** |
| + deferred `_source` | 82,517 | 886 MB | 915 MB | 384.6 | 1.38 ms | 1,283 | 34 ms | 19.2 ms |
| + address collector | 80,792 | 845 MB | 862 MB | 455.5 | 1.38 ms | 1,574 | 16.5 ms | 11.5 ms |
| + mmap (on-disk) | 56,178 | 800 MB | 826 MB | 427.3 | 1.44 ms | 1,567 | 16.0 ms | 12.3 ms |
| + bounded write buffer | 56,864 | 309 MB | 343 MB | 429.6 | 1.45 ms | 1,487 | 21.3 ms | 12.4 ms |
| + top-K sort collector | 27,688 | 307 MB | 322 MB | 882.3 | 0.91 ms | 1,619 | 19.1 ms | 6.0 ms |
| + live-id table | 57,003 | 307 MB | 326 MB | 876.8 | 0.86 ms | 1,624 | 17.9 ms | 6.4 ms |
| **+ writer tuning** | **71,458** | 377 MB | **428 MB** | **805.5** | **0.99 ms** | **1,415** | **20.1 ms** | **5.2 ms** |

**รวมจาก baseline:** qps c=1 **×39**, p99 ที่ c=8 ดีขึ้น **81 เท่า**, `sort_paged` เร็วขึ้น **179 เท่า**,
RSS หลัง search ลด **12 เท่า** — โดย indexing throughput ยังอยู่ระดับเดิม (−14%) ทั้งที่ตอนนี้เขียนลงดิสก์จริง

## baseline เปิดโปงอะไร

### 1. sort อ่าน `_source` ของทุก doc ที่ match — 930 ms
โค้ดเดิมดึง stored field ของ **ทุกเอกสารที่ตรง** (200,000 ครั้ง) มาสร้าง `Hit`
แล้วค่อยเรียงและตัดเอา 10 อัน

แก้เป็น: เก็บเฉพาะ `Cand { shard, addr, score, sort }` ที่เบา ๆ ระหว่าง collect,
prune ด้วย `select_nth_unstable_by` ทุกครั้งที่เกิน 4× ของหน้าที่ต้องการ
(amortised O(n) ไม่ใช่ sort เต็ม), **แล้วค่อยอ่าน `_source` เฉพาะ 10 อันที่จะส่งกลับ**

⇒ 930 ms → 19 ms

### 2. `DocSetCollector` เก็บ `HashSet<DocAddress>` ของทุก match
เราแค่ต้อง iterate ไม่เคยเช็ค membership เลย — เขียน `AddrCollector` ที่ push ลง
`Vec` ต่อ segment แทน

⇒ 19 ms → 11.5 ms, p99 ที่ c=8 ลดจาก 34 → 16.5 ms

### 3. RSS 5.1 GB หลัง search — และ 867 MB หลัง index
ตัวแรกคือผลพวงของข้อ 1 (สร้าง `Hit` 200k ตัวพร้อม `_source` ทุกครั้งที่ค้น)
ตัวหลังคือ `pending` map ที่เก็บ `_source` ของทุก write ไว้จนกว่าจะ refresh

แก้: เก็บ pending เป็น **raw JSON string** ไม่ใช่ `Value` ที่ parse แล้ว
และตั้งเพดาน 32 MB — เกินแล้ว flush writer

**จุดที่ต้องระวัง:** flush ตอนนั้นต้อง **ไม่** ทำให้ search มองเห็นเอกสาร
(มี test ยืนยันว่า write ที่ยังไม่ refresh ต้องมองไม่เห็น) จึงแยก reader เป็นสองตัว —
`realtime` ที่ reload ตอน flush ใช้สำหรับ GET, และ `reader` ที่ reload เฉพาะตอน
refresh จริงใช้สำหรับ search ตรงกับที่ OpenSearch ทำด้วย version map + translog

⇒ RSS หลัง index 867 → 309 MB

### 4. mmap / persistence
`OBSEARCH_DATA=<dir>` เปลี่ยนไปใช้ `MmapDirectory` — index อยู่ใน page cache ของ OS
ไม่ใช่ RSS ของ process และ **รอดจากการ restart** (metadata ของ index เก็บเป็น
`_meta.json` ต่อ index, reopen ตอนบูต) ไม่ตั้ง env var = อยู่ใน RAM เหมือนเดิม

ราคาที่จ่าย: indexing 80.8k → 56.2k docs/s (−30%) จากการเขียนลงดิสก์จริง

## Conformance ไม่ถอย

รัน test suite เดิมทั้งสองโหมดหลัง optimize:

| โหมด | PASS |
|---|---|
| in-RAM | 297-298 / 400 |
| on-disk (mmap) | 298 / 400 |

## ⚠️ ยังไม่ได้เทียบกับ OpenSearch ตัวจริง

เครื่องนี้ดาวน์โหลด OpenSearch ไม่ได้:

- `artifacts.opensearch.org` ตอบ **403 AccessDenied** ทั้ง bundle และ min distribution
  ทุกเวอร์ชัน/สถาปัตยกรรมที่ลอง (3.9.0 / 3.5.0 / 3.1.0 / 3.0.0 / 2.19.0, darwin-arm64 และ x64)
- Docker daemon ไม่ได้รัน (`Cannot connect to the Docker daemon`)

**ตัวเลขข้างบนจึงเป็นการเทียบตัวเองก่อน-หลังเท่านั้น ไม่ใช่การเทียบกับ OpenSearch**
`tools/bench.py` เป็น engine-agnostic อยู่แล้ว — ชี้ `--url` ไปที่ OpenSearch
node แล้วรันซ้ำได้ทันทีเมื่อมีทางเข้าถึง

ทางปลดล็อก (เรียงตามความง่าย):
1. เปิด Docker Desktop → `docker run -p 9201:9200 -e discovery.type=single-node opensearchproject/opensearch:3.1.0`
   แล้ว `python3 tools/bench.py --url http://127.0.0.1:9201 --proc java --label opensearch`
2. ดาวน์โหลด tarball จากเครือข่ายที่เข้าถึง artifacts.opensearch.org ได้
3. build จากซอร์สที่ clone ไว้: `./gradlew :distribution:archives:darwin-arm64-tar:assemble` (ช้าและต้องโหลด dependency)

## รอบสองของ Phase 2

### 5. top-K sort collector — sort ไม่ต้องแตะทุก match อีกต่อไป
รอบแรกยัง collect `Cand` ของทุกเอกสารที่ match แล้วค่อย prune ตอนท้าย
รอบนี้เขียน `SortCollector` ที่ประเมิน sort key **ระหว่าง collect** และ prune
ทุกครั้งที่ buffer โตเกิน 4× ของหน้าที่ต้องการ ⇒ หน่วยความจำเป็น O(K) ไม่ใช่ O(matched)
และเปิด fast-field column ครั้งเดียวต่อ segment แทนที่จะเปิดทั้ง searcher

⇒ qps c=1 430 → 882, `sort_paged` 12.4 → 6.0 ms, `bool_filter` 3.8 → 1.2 ms

### 6. เปิด multithread executor — แล้วเจอกับดัก
`set_default_multithread_executor()` ทำให้ query หนึ่งกระจายข้าม segment ได้
แต่ **indexing ตกครึ่งหนึ่งทันที** (57k → 27.7k docs/s)

สาเหตุ: `write_doc` เรียก `exists_doc` ซึ่งเดิมทำ **search** เพื่อเช็คว่า id นี้มีอยู่ไหม
พอมี executor แบบหลายเธรด การ search จิ๋ว ๆ ต่อเอกสารก็ fan out เข้า rayon pool ทุกครั้ง

แก้ที่ต้นเหตุ: เก็บ `DocMeta { version, live }` ต่อ id ไว้ในหน่วยความจำ ⇒ การเช็คว่า
มีเอกสารอยู่ไหมเป็นแค่ map lookup ไม่ต้อง search เลย (เดิมทุก ๆ เอกสารที่ index
ต้องยิง search หนึ่งครั้ง — เป็นภาระที่มีมาตั้งแต่ต้นแม้ยังไม่เปิด executor)
ตอน reopen index จากดิสก์ จะ rebuild ตารางนี้จาก `_id` fast field

⇒ indexing 27.7k → 57k docs/s โดย query ไม่เสียอะไรเลย

### 7. `took` เป็นเวลาจริง
เดิม hardcode `1` ตอนนี้จับจาก `Instant` ที่ต้นทางของ `run()`

### 8. ปรับ writer
tantivy ตั้งต้นที่ 8 เธรด แบ่ง budget กันคนละ ~6 MB ⇒ flush segment ถี่และ merge บ่อย
วัดหลายค่าแล้วได้ตารางนี้ (200k docs, on-disk):

| threads | budget | docs/s | RSS |
|---:|---:|---:|---:|
| 2 | 32 MB | 54,293 | 313 MB |
| 3 | 48 MB | 62,798 | 336 MB |
| 4 | 64 MB | 65,166 | 353 MB |
| **2** | **64 MB** | **69,822** | **382 MB** |
| 4 | 128 MB | 75,195 | 516 MB |
| 8 | 256 MB | 68,966 | 547 MB |

ตั้งค่าเริ่มต้นที่ **2 เธรด / 64 MB** และเปิดให้ปรับผ่าน
`OBSEARCH_WRITER_THREADS` / `OBSEARCH_WRITER_BUDGET_MB` — เป็นการแลกกันตรง ๆ
ระหว่าง indexing throughput กับ RSS

### 9. Persistence ใช้ได้จริง
ตรวจแล้ว: restart process แล้ว `_count` ยังคืน 200,000, query ยังทำงาน,
mapping/settings ยังอยู่ครบ

## งานถัดไปของ Phase 2

- **ยังไม่มี early termination จริง** — top-K collector ยังต้องเดินทุก doc ที่ match
  ขั้นถัดไปคือใช้ค่าที่ K ปัจจุบันไปตัด segment/block ที่เป็นไปไม่ได้ทิ้ง (WAND / block-max)
- **single shard ต่อ index** — ตอนนี้ขนานข้าม segment ได้แล้ว แต่ยังไม่มีการแบ่ง shard
- **`versions` เป็น HashMap ในหน่วยความจำ** — 200k id ≈ 16 MB และโตตามจำนวนเอกสาร
  ระยะยาวต้องย้ายไปโครงสร้างที่ compact กว่านี้
- ยังไม่ได้วัด: cold-cache latency, merge behaviour ระยะยาว, หน่วยความจำที่ concurrency สูงกว่านี้
