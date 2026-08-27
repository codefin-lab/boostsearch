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

## รอบที่สอง — fan-out หลาย index (หลังปิด Phase 1)

วัดใหม่ทั้งหมดเพราะงาน Phase 1 เพิ่มงานบน write path เยอะ (coercion ตอน index,
date column, dynamic template, `_ignored`, flat_object) ผลคือ **ไม่ได้แย่ลงเลย**:
index 71,458 → 99,641 docs/s, RSS หลัง search 428 → 267 MB, p99 ที่ c=8
20.1 → 5.6 ms

แต่ single index ไม่ใช่รูปที่มีปัญหา — ปัญหาอยู่ที่ fan-out

### เอกสารชุดเดียวกัน 400,000 ตัว ต่างกันแค่จำนวน index

| layout | match_all | agg_nested | เฉพาะ agg |
|---|---:|---:|---:|
| 1 index × 400,000 | 1.11 ms | 1.99 ms | 0.88 ms |
| 10 index × 40,000 | 1.28 ms | 2.28 ms | 0.99 ms |
| 50 index × 8,000 | 1.77 ms | 4.88 ms | 3.11 ms |
| 200 index × 2,000 | 2.27 ms | 16.72 ms | **14.45 ms** |

⇒ ต้นทุนเป็น **ต่อ index** ไม่ใช่ต่อเอกสาร: index เดียว 400k docs ใช้ 708 µs
(1.77 ns/doc) ส่วน 200 index ใช้ CPU 13,334 µs กับเอกสารชุดเดียวกัน
หักส่วนที่แปรตามเอกสารออกแล้วเหลือ **~63 µs คงที่ต่อ index ต่อ query**

### สมมติฐานที่วัดแล้วผิด

**จำนวน segment** — forcemerge 400 → 200 segment แล้ว `run` ไม่ขยับ
(13,533 → 13,334 µs) ตรงกับที่เคยวัดไว้รอบก่อน

### วัดผิดครั้งหนึ่ง แล้วจับได้

timer ตัวแรกรายงาน `model=207,525 µs` ทั้งที่ทั้ง request ใช้ 5.29 ms —
เป็นไปไม่ได้ พอไปดูโค้ดพบว่า `fetch_add` ไปวางหลังการ collect hit ไม่ใช่หลัง
ก้อน model ตัวเลขนั้นจึงเป็น model + การค้น hit ต่อ shard **ตัวเลขที่ขัดกับ
wall time คือสัญญาณว่าเครื่องมือวัดพัง ไม่ใช่การค้นพบ**

### แก้สองอย่าง

**1. รวม hit กับ aggregation ให้เดินรอบเดียว**
เดิมยิง `searcher.search` สองครั้งต่อ index — ครั้งหนึ่งเก็บ hit อีกครั้งทำ agg
แปลว่าสร้าง weight และเดิน segment สองรอบ ที่ 200 index รอบที่สองคือต้นทุนหลัก
ตอนนี้ทั้งคู่อยู่ใน collector tuple เดียว (`MaybeAgg` รับหน้าที่ช่องว่างเมื่อ
request ไม่มี agg)

**2. merge ผลกลางแบบ tree ขนาน**
เดิมพับ intermediate result ทีละตัวตามลำดับ — เป็นงาน single-thread ที่
200 index กินราว 750 µs ตอนนี้ใช้ `reduce_with` ของ rayon บน pool เดิม
⇒ ~300 µs

### ผลที่ 200 index × 2,000 docs (ทั้งคู่รันใน Docker)

| query | obsearch ก่อน | obsearch หลัง | OpenSearch 3.1 |
|---|---:|---:|---:|
| match_all | 2.14 | 2.11 | 5.20 |
| term | 2.15 | 2.15 | 4.44 |
| agg_terms | 7.78 | **3.69** | 5.35 |
| agg_stats | 6.10 | **3.57** | 3.77 |
| agg_nested | 13.52 | **5.22** | 5.91 |
| sort_paged | 3.94 | 3.90 | 13.27 |

จากที่แพ้ agg ทั้งสามตัว (0.47–0.73×) กลายเป็นชนะทั้งหมด และ single index
ไม่ถอยเลย (qps c=1 1,230 → 1,216 อยู่ในระดับ noise, p99 c=8 6.14 → 5.62 ms)

### ข้อควรระวังของการวัดบนเครื่องนี้ (ตอนวัดรอบนี้)

Docker Desktop บน macOS คิดค่า fan-out กว้างแพงกับเราเป็นพิเศษ และค่านี้
**โตตามความกว้างของ fan-out**:

| layout | native | docker | penalty |
|---|---:|---:|---:|
| 1 index × 400,000 | 1.24 ms | 2.08 ms | 1.68× |
| 10 index × 40,000 | 1.28 ms | 2.43 ms | 1.89× |
| 200 index × 2,000 | 4.71 ms | 16.93 ms | 3.59× |

ตอนนั้นสรุปว่าเป็นค่า schedule thread ข้าม hypervisor และตัวเลข Docker-vs-Docker
จึงเอนเข้าข้าง OpenSearch — **ข้อสรุปนั้นผิด ดูรอบที่สี่**

## รอบที่สาม — ค่าคงที่ต่อ index

หลังรวม hit กับ agg เป็น pass เดียว ต้นทุน agg คงที่ต่อ index ลดจาก ~63 µs
เหลือ ~6 µs ที่เหลือกลายเป็นค่า fan-out เปล่า ๆ — `match_all` บน 200 index
ยังใช้ 1.18 ms

### วัดกับ index ว่าง

| | match_all | ต่อ index |
|---|---:|---:|
| 200 index × 0 docs | 1.13 ms | 5.67 µs |
| 200 index × 2,000 docs | 1.03 ms | 5.17 µs |

index ที่ **ไม่มีเอกสารเลย** ก็ยังคิดค่าเท่าเดิม ⇒ เป็นค่าเครื่องจักรล้วน ๆ

### สมมติฐานที่วัดแล้วผิด (อีกครั้ง)

**rayon แจกงานทีละ index แพงเกินไป** — ลองรวมเป็น chunk ให้แต่ละ worker
รับหลาย index ผลคือ **แย่ลง 4 เท่า** (500 µs → 1,800 µs) revert

### ตัวเลขที่ขัดกันเองชี้ทางให้

ใส่ timer วัด CPU รวมใน `run_shard` ได้ **31,063 µs บน 200 index ว่าง
ในเวลา wall 551 µs** — เป็นไปไม่ได้บน 14 core แปลว่า task **รอ** ไม่ใช่ทำงาน

ไล่ต่อทีละ phase: `lock=82 µs`, `searcher=29 µs`, **`search=46,029 µs`**
⇒ `searcher.search` บน index ว่างใช้ elapsed ~147 µs ต่อครั้ง

**สาเหตุ: pool ซ้อน pool** `Searcher::search` ส่งงานราย segment เข้า executor
ที่ทุก index ใช้ร่วมกัน เมื่อ fan-out เรียกจากใน rayon task อยู่แล้ว แต่ละ shard
จึงไปต่อคิวรอ pool เดียวกันกับอีก 199 ตัว

### แก้: เลือก executor ตามความกว้างของ fan-out

fan-out หลาย index → per-segment เป็น `Executor::single_thread()` เพราะงาน
ขนานถูกดึงไปที่ระดับ index แล้ว · index เดียว → ใช้ pool ร่วมต่อ เพราะเป็นจุดที่
per-segment parallelism คุ้ม

| 200 index ว่าง | ก่อน | หลัง |
|---|---:|---:|
| fan wall | 551 µs | **163 µs** |
| shard CPU รวม | 29,604 µs | **462 µs** |
| ในนั้นเป็น `search` | 29,357 µs | **82 µs** |

ตัวเลขกลับมาสมเหตุสมผล: 462 µs CPU ใน 163 µs wall ≈ ขนาน 2.8 เท่า

### ผลรวมของ Phase 2 รอบนี้ (200 index × 2,000 docs)

native:

| query | เริ่มรอบ | จบรอบ |
|---|---:|---:|
| match_all | 2.27 ms | **0.55 ms** |
| agg_terms | 3.14 ms | **0.93 ms** |
| agg_nested | 16.72 ms | **1.56 ms** |
| sort_paged | 1.67 ms | **1.16 ms** |

ทั้งคู่ใน Docker เทียบ OpenSearch 3.1:

| query | obsearch | OpenSearch | |
|---|---:|---:|---|
| match_all | 1.85 | 4.25 | 2.30× |
| term | 1.61 | 4.61 | 2.86× |
| agg_terms | 3.01 | 5.32 | 1.77× |
| agg_stats | 3.24 | 3.91 | 1.21× |
| agg_nested | 3.99 | 5.83 | 1.46× |
| sort_paged | 2.95 | 10.17 | 3.45× |
| index build | 7.5 s | 19.8 s | 2.62× |

single index ไม่ถอย: qps c=1 1,230 → 1,300, p99 c=8 6.14 → 5.53 ms,
RSS 278.9 → 261.9 MB

## รอบที่สี่ — ข้อควรระวังที่เคยเขียนไว้นั้นผิด

หลังแก้ pool ซ้อน pool วัด penalty ของ Docker ใหม่:

| layout | native | docker | penalty (ก่อนแก้) |
|---|---:|---:|---:|
| 1 index × 400,000 | 1.06 ms | 1.88 ms | 1.78× (1.68×) |
| 10 index × 40,000 | 0.83 ms | 1.58 ms | 1.89× (1.89×) |
| 200 index × 2,000 | 1.57 ms | 3.24 ms | **2.06×** (3.59×) |

ค่าที่เคย "โตตามความกว้างของ fan-out" ตอนนี้เกือบแบน ⇒ **ที่โตไม่ใช่เพราะ
hypervisor แต่เป็นบั๊กของเราเอง** — shard ไปต่อคิว pool เดียวกัน ยิ่ง index เยอะ
คิวยิ่งยาว การโทษเครื่องมือวัดตอนนั้นคือการมองข้ามบั๊กจริง

### ค่าคงที่ของ round-trip

| | GET / p50 |
|---|---:|
| obsearch native | 0.162 ms |
| obsearch docker | 0.435 ms |
| opensearch docker | 0.476 ms |

Docker เพิ่มค่า round-trip ~0.27 ms เท่ากันทั้งสองฝั่ง (HTTP path ของสองเครื่องยนต์
ใกล้เคียงกันมาก 0.435 vs 0.476) แต่เมื่อคิดเป็นสัดส่วนแล้วกินของฝั่งที่เร็วกว่ามากกว่า —
8.4% ของ 3.24 ms เทียบกับ 4.7% ของ 5.83 ms

หักค่านี้ออกทั้งสองฝั่ง: 2.97 vs 5.56 ms = 1.87× (จากที่วัดได้ 1.80×)
⇒ **เอนน้อยมาก ตัวเลข Docker-vs-Docker ใช้ได้**

ที่ยังไม่รู้คือตัวเลข **absolute** บน Linux host จริง — penalty ~1.9× ที่เหลือ
กระทบทั้งสองฝั่ง จะรู้ต้องวัดบนเครื่อง Linux ซึ่งยังไม่ได้ทำ

## รอบที่ห้า — top-k ที่ตัดกิ่งไม่ได้

วัดต้นทุน CPU ต่อ request แยกตามรูปของ response (k6, saturate):

| kind | qps | µs CPU/req |
|---|---:|---:|
| `GET /` | 150,958 | 93 |
| `size:0` (ค้นแต่ไม่เอา hit) | 128,966 | 109 |
| `size:1` | **19,723** | **710** |
| `size:10` | 17,569 | 797 |
| `size:10, _source:false` | 18,410 | 760 |

คืน hit **แค่ตัวเดียว** แพงกว่าทั้ง query ที่เหลือ 6 เท่า และปิด `_source`
แทบไม่ช่วย ⇒ ไม่ใช่ค่าอ่าน stored field แต่เป็นค่า**เดินทุกเอกสาร**

### สาเหตุ

`SortBySimilarityScore` ของ tantivy รองรับ block-WAND ผ่าน
`collect_segment_top_k` — พอ heap เต็มแล้ว block ที่คะแนนสูงสุดสู้ตัวที่แย่ที่สุด
ไม่ได้จะถูกข้ามทั้ง block

แต่เรามัด `(Count, TopDocs, MaybeAgg)` เป็น tuple เดียว **tuple collector ใช้
`collect_segment_top_k` ไม่ได้** เพราะสมาชิกตัวอื่นต้องเห็นทุกเอกสาร
⇒ ตกไปทางเดินทีละเอกสาร เสียการตัดกิ่งทั้งหมด

### แก้: ตัดกิ่งได้เมื่อไม่มีใครต้องการทุกเอกสาร

- **มี aggregation** → ยังต้องเห็นทุกเอกสารอยู่ดี ใช้ tuple เหมือนเดิม
- **ไม่มี** → ใช้ `TopDocs` ตัวเดียว แล้วเอา count จาก `Weight::count`
  ซึ่งอ่านจาก postings header ได้เลยสำหรับ query ที่ตอบได้ (term query
  ที่ไม่มี deletion รู้ doc frequency ของตัวเอง)

| kind | ก่อน | หลัง |
|---|---:|---:|
| `size:1` | 19,723 | **82,809** |
| `size:10` | 17,569 | **60,967** |
| `size:100` | 8,972 | **11,349** |

mix เต็ม native ที่ vus=64: 23,245 → **35,845 qps (+54%)**

### ผลเทียบ OpenSearch (k6, ทั้งคู่ใน Docker, saturate)

| | ก่อน | หลัง |
|---|---:|---:|
| obsearch | 18,000 | **22,500** |
| OpenSearch | 26,500 | 26,400 |
| | 0.68× | **0.85×** |

ยังแพ้อยู่ 1.18× (จากเดิม 1.47×) ส่วนที่เหลือส่วนหนึ่งเป็นเพราะใน mix มี
aggregation ซึ่งบังคับให้ใช้ทาง tuple ที่ตัดกิ่งไม่ได้

## รอบที่หก — แยกตามรูปของ query ไม่ใช่ตามโหลด

หลังแก้ top-k ให้ตัดกิ่งได้ ต้นทุนต่อ query กลายเป็น:

| query | qps | µs CPU |
|---|---:|---:|
| match_all size:10 | 60,652 | 231 |
| term size:10 | 71,140 | 197 |
| **terms agg size:0** | **20,360** | **688** |

aggregation กิน CPU ของ mix ไป 62% ⇒ เป็นตัวที่ต้องได้ core

### วัดสองทางสุดขั้ว

| vus | pool ทุก query (default) | thread เดียวทุก query |
|---:|---:|---:|
| 1 | 4,695 / 198 µs | 3,456 / **171 µs** |
| 8 | **27,675** / 258 µs | 26,195 / **166 µs** |
| 64 | 37,308 / 1.66 ms | **42,658** / **1.41 ms** |

thread เดียว median ดีกว่า**ทุกระดับ** แต่ qps แย่กว่าที่โหลดต่ำ — เพราะมันไม่
ขนาน query หนัก (agg) ทำให้ค่าเฉลี่ยถูกหางลาก

⇒ ตัวแปรที่ถูกต้องไม่ใช่ **โหลด** แต่เป็น **รูปของ query**

### แก้: top-k ใช้ thread เดียว aggregation ใช้ pool

- **top-k อย่างเดียว** — ตัดกิ่งอยู่แล้ว เก็บแค่ `want` ตัว ค่า coordinate
  แพงกว่าตัวงานเอง และยังไปแย่ง core จาก agg
- **มี aggregation** — ต้องเดินทุกเอกสารจริง ๆ ใช้ pool คุ้ม

| vus | ก่อน | หลัง |
|---:|---:|---:|
| 1 | 4,695 / 198 µs | **5,930 / 151 µs** |
| 8 | 27,675 / 258 µs | **29,767 / 207 µs** |
| 64 | 37,308 / 1.66 ms | **40,358 / 1.49 ms** |

ชนะทั้งสองแบบตายตัวที่ vus 1–8 ซึ่งใกล้การใช้งานจริงที่สุด

### throughput เทียบ OpenSearch — เสมอแล้ว

| | qps |
|---|---:|
| เริ่มไล่ | 18,000 (0.68×) |
| หลัง top-k ตัดกิ่ง | 22,500 (0.85×) |
| **หลังแยกตามรูป query** | **26,100 (1.00×)** |
| OpenSearch 3.1 | 26,000 |

**เคยลองแล้วไม่ได้ผล** — สลับตามจำนวน query ที่กำลังทำอยู่ (in-flight):
ได้ 21,364 ที่ vus=64 แย่กว่าทั้งสองแบบตายตัว เพราะตัดสินทีละ query
ตามสภาพชั่วขณะ ทำให้ปนกันแล้วแย่งกันเอง · ตัวแปรที่ใช้ได้คือรูปของ query
ซึ่งรู้แน่นอนตั้งแต่ต้น ไม่ใช่สภาพโหลดที่แกว่ง
