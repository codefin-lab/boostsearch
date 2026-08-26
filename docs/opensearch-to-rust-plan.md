# แผนศึกษา OpenSearch และเปลี่ยนไปเป็น Rust

> เป้าหมาย: ลด **Resource Consumption** (RAM/CPU/instance count) และเพิ่ม **Performance** (p99 latency, indexing throughput)
> อ้างอิงซอร์ส: OpenSearch `3.9.0` @ `9d0abd94` (shallow clone ใน `study/OpenSearch`)

---

## 1. สิ่งที่ได้จากการศึกษาซอร์สจริง

### 1.1 ขนาดของงาน (นับจากซอร์สจริง, `src/main/java` เท่านั้น)

| โมดูล | ไฟล์ | LOC |
|---|---:|---:|
| `server/` | 5,070 | 930,300 |
| `modules/` | 939 | 134,474 |
| `plugins/` | 406 | 72,851 |
| `libs/` | 488 | 58,273 |
| `client/` | 176 | 32,327 |
| **รวม (ไม่นับ test)** | **~7,000** | **~1.23M** |

แยกย่อยใน `server/src/main/java/org/opensearch/`:

| แพ็กเกจ | LOC | ความหมายต่อการพอร์ต |
|---|---:|---|
| `index/` | 214,594 | หัวใจ — mapper 38k, engine 31k, query 29k, store 16k, shard 15k, translog 11k |
| `search/` | 170,746 | query phase / fetch phase / aggregations |
| `action/` | 147,838 | 157 TransportAction — logic กระจายงานไป shard |
| `common/` | 91,788 | utility, settings, serialization (`StreamInput/StreamOutput`) |
| `cluster/` | 88,356 | state machine + coordination (11,940 LOC, 50 ไฟล์) |
| `rest/` | 25,334 | REST layer |
| `transport/` | 22,598 | node-to-node binary protocol |

### 1.2 พื้นผิว API ที่ต้องคง compatibility

- **185** `Rest*Action` / **170** endpoint ใน `rest-api-spec`
- **53** `QueryBuilder` (query DSL clauses)
- **63** `AggregationBuilder`
- **53** `FieldMapper` (field types)
- **138** tokenizer / token filter / analyzer factory
- **157** `TransportAction`

### 1.3 จุดสำคัญที่พบ

1. **`modules/transport-grpc` มีอยู่แล้ว** (206 ไฟล์ Java, ใช้ `org.opensearch:protobufs`, grpc 1.75, expose `SearchService` + `DocumentService`) — นี่คือ **ประตูที่สำคัญที่สุด** สำหรับแผนนี้ เพราะมี schema แบบ protobuf ที่ Rust พูดได้ทันทีผ่าน `tonic` โดยไม่ต้อง reverse-engineer binary transport ของ Java
2. **ผูกกับ Lucene 10.5.0 อย่างลึก** — 1,043 จาก 5,070 ไฟล์ใน `server` import `org.apache.lucene` (≈21%) ไม่ใช่แค่ layer เดียว แต่รั่วเข้าไปถึง mapper, query, aggregation, fielddata, codec
3. **`storage/` แยกออกมาแล้ว** (tiering, directory, indexinput, prefetch) — remote-store/segment-replication ทำให้ compute แยกจาก storage ได้ ⇒ เขียน search node ใหม่ได้โดยไม่ต้องแตะ write path
4. **Thread pool มีมากกว่า 25 ตัว** (`ThreadPool.java`) — `write`/`get` = fixed ตามจำนวน core, `search` = resizable, `translog_sync` = `cores*4` ⇒ ต้นเหตุของ context switching และ memory ต่อ thread stack

---

## 2. Resource Consumption มาจากไหน (วิเคราะห์ต้นเหตุ)

| ต้นเหตุ | ผลกระทบ | Rust แก้ได้แค่ไหน |
|---|---|---|
| JVM heap + GC (G1) | ต้องกัน heap ~50% ของ RAM, GC pause กระทบ p99, heap มักตั้ง ≤31GB (compressed oops) | **แก้ได้เต็ม** — ไม่มี GC, RAM ที่เหลือคืนให้ page cache ทั้งหมด |
| Object header + boxing | `Long`, `Map<String,Object>` ใน parse path กิน RAM 2-4x เทียบ struct | **แก้ได้เต็ม** — struct packed, `SmallVec`, arena |
| JSON parse ผ่าน Jackson → `Map<String,Object>` | allocation ต่อ document สูงมากใน bulk indexing | **แก้ได้มาก** — `simd-json` / `sonic-rs` + zero-copy borrow |
| Serialization `StreamInput/Output` ระหว่าง node | copy หลายรอบต่อ hop | **แก้ได้มาก** — `rkyv`/Arrow zero-copy |
| Thread-per-request model (25+ pool) | stack memory + context switch | **แก้ได้มาก** — tokio async + bounded worker |
| FieldData / doc values cache บน heap | OOM classic ของ ES/OS | **แก้ได้** — mmap + off-heap by default |
| Lucene segment merge | CPU burst | **แก้ได้บางส่วน** — algorithm เดิม แต่ไม่มี GC pressure ซ้อน |
| JVM startup + baseline RSS | ~1-2GB ก่อนทำอะไร | **แก้ได้เต็ม** — baseline หลัก 10-50MB |

**ประมาณการที่สมเหตุสมผล** (อ้างอิงจากผลที่ Quickwit/Tantivy รายงาน และธรรมชาติของ JVM): RSS ลด **50-70%** สำหรับ search node, indexing throughput ต่อ core เพิ่ม **1.5-3x**, p99 ดีขึ้นชัดเพราะไม่มี GC pause — **แต่ query latency ตัวกลาง (p50) อาจไม่ต่างมาก** เพราะ Lucene ถูก optimize มาแล้ว 20 ปี และ JIT ทำงานดีใน hot loop

> ⚠️ ข้อควรระวังที่ต้องพูดตรง ๆ: กำไรใหญ่ที่สุดคือ **memory** ไม่ใช่ raw search speed ถ้า KPI คือ "query เร็วขึ้น 10x" แผนนี้จะไม่ตอบโจทย์ ถ้า KPI คือ "ลด instance/ลดค่า cloud/p99 นิ่ง" แผนนี้ตอบโจทย์เต็ม ๆ

---

## 3. ทางเลือกสถาปัตยกรรม

### ทางเลือก A — Full rewrite 1.23M LOC เป็น Rust
- ❌ **ไม่แนะนำ** ประเมิน 60-150 คน-ปี, ต้องเขียน Lucene ใหม่ทั้งก้อน, compatibility gap จะฆ่าโครงการก่อนถึง production

### ทางเลือก B — Rust sidecar/proxy หน้า OpenSearch เดิม
- ✅ ทำได้ใน 1-2 เดือน — routing, caching, query rewrite, coordinator role ใน Rust
- ⚠️ กำไรจำกัด (ลด coordinator node เท่านั้น) แต่ **เป็นด่านแรกที่ดีมาก** เพราะได้ของจริงเร็วและสร้าง test harness ไว้ใช้ต่อ

### ทางเลือก C — **Strangler Fig: แทนที่ read path ก่อน แล้วค่อยขยับเข้า write path** ⭐ แนะนำ
- ใช้ `transport-grpc` + remote-store เป็นรอยต่อ
- Rust node อ่าน segment จาก object store โดยตรง, ทำ search/aggregation เอง
- Java node ยังทำ cluster coordination + indexing ไปก่อน
- ตัดสินใจภายหลังว่าจะพอร์ต write path หรืออยู่แบบ hybrid ถาวร (hybrid ถาวรก็เป็นคำตอบที่ยอมรับได้)

---

## 4. Roadmap (ทางเลือก C)

### Phase 0 — Baseline & Harness (3-4 สัปดาห์)
เป้าหมาย: **ห้ามเริ่มเขียน Rust ก่อนมีตัวเลข**
- [ ] รัน OpenSearch Benchmark (`big5`, `nyc_taxis`, `http_logs`) บน OpenSearch 3.9 stock → เก็บ p50/p90/p99, RSS, CPU-seconds/query, docs/s
- [ ] เก็บ query mix จริงจาก production (top 50 query shapes) — จะเป็น scope ที่แท้จริงของ Phase 2
- [ ] สร้าง **differential test harness**: ยิง request เดียวกันเข้า Java + Rust แล้ว diff JSON response (ตัวนี้คือหัวใจของทั้งโปรเจกต์)
- [ ] ตัดสิน KPI ที่ผูกกับเงิน: `$/1M queries` และ `$/TB indexed`

### Phase 1 — Rust Coordinator / Gateway (6-8 สัปดาห์)
- `axum` + `tokio` รับ REST, parse query DSL เป็น IR ของเรา
- แปลง IR → gRPC (`tonic` + `opensearch-protobufs`) ยิงเข้า Java data node
- ทำ scatter-gather + merge/reduce ผลลัพธ์ใน Rust
- ✅ ได้กำไรจริงทันที: coordinator node ที่เคยต้อง 16-32GB heap → เหลือ ~1-2GB
- ✅ ได้ query-DSL parser ที่จะใช้ยาวไปถึง Phase 2

### Phase 2 — Rust Search Node บน Remote Store (4-6 เดือน)
- ต่อ object store ด้วย `object_store`/`opendal` อ่าน segment ที่ Java node เขียนไว้
- **จุดตัดสินใจใหญ่ที่สุด**: อ่าน Lucene segment format ตรง ๆ ใน Rust
  - ทางเลือก 2a: เขียน Lucene 10 codec reader ใน Rust (postings/doc-values/kNN) — งานหนัก แต่ compat 100%
  - ทางเลือก 2b: ใช้ `tantivy` 0.26 เป็น engine แล้ว **re-index** ข้อมูลลง format ของ tantivy — เร็วกว่ามาก แต่ต้อง dual-write และยอมรับความต่างของ scoring
  - 👉 แนะนำ **2b ก่อน** สำหรับ index ที่ยอม re-index ได้ (logs/metrics/observability) แล้วค่อยประเมิน 2a ถ้าจำเป็นต้อง in-place
- Aggregation: `datafusion` + `arrow` สำหรับ agg เชิงตัวเลข, เขียนเองสำหรับ bucket agg ที่ tantivy ไม่มี
- Scope ตามความถี่จริง: 63 agg types ไม่ต้องทำหมด — ปกติ 10 ตัวแรกครอบคลุม >90% ของ traffic

### Phase 3 — Rust Indexing Path (4-6 เดือน, ทำต่อเมื่อ Phase 2 ผ่าน)
- Bulk ingest, mapping/dynamic mapping, analysis chain (138 factory — ทำ 20 ตัวที่ใช้จริงก่อน)
- Translog + durability semantics — **ส่วนที่เสี่ยงที่สุดต่อ data loss** ต้องมี Jepsen-style test

### Phase 4 — Cluster Coordination (ทำท้ายสุด หรือไม่ทำเลย)
- 11,940 LOC ของ coordination = consensus ที่ผ่านการ battle-test มานาน
- ถ้าต้องทำ ใช้ `openraft` (ยัง 0.10-alpha — ความเสี่ยงสูง)
- 👉 **คำแนะนำ: อย่าพอร์ต** ปล่อยให้ Java 3 node ทำ cluster-manager ต่อไป ต้นทุนแค่ 3 instance เล็ก แต่ประหยัดความเสี่ยงมหาศาล

---

## 5. Crate ที่เลือก (เช็คเวอร์ชันจริงแล้ว)

| งาน | Crate | เวอร์ชัน |
|---|---|---|
| async runtime | `tokio` | 1.53 |
| HTTP/REST | `axum` | 0.8 |
| gRPC (คุยกับ Java node) | `tonic` | 0.14 |
| search engine | `tantivy` | 0.26 |
| columnar agg | `datafusion` / `arrow` | 55 / 59 |
| object store | `object_store` / `opendal` | 0.14 / 0.58 |
| zero-copy ser | `rkyv` | 0.8 |
| bitset | `roaring` | 0.11 |
| consensus (ถ้าจำเป็น) | `openraft` | 0.10-alpha ⚠️ |

อ้างอิงสถาปัตยกรรม: **Quickwit** (search บน object storage ด้วย tantivy) คือ prior art ที่ใกล้ที่สุด ควรอ่านก่อนออกแบบ Phase 2

---

## 6. Risk Register

| ความเสี่ยง | ระดับ | การรับมือ |
|---|---|---|
| Scoring/relevance ต่างจากเดิม → ผลลัพธ์เปลี่ยน | **สูง** | differential harness เทียบ top-K + NDCG ทุก build |
| Lucene codec compat (ถ้าเลือก 2a) | **สูง** | เริ่มที่ 2b (re-index) ลดความเสี่ยงลงทันที |
| Data loss ใน write path | **สูงมาก** | Phase 3 ต้องมี fault-injection/Jepsen ก่อน production |
| Plugin ecosystem (security, alerting, ISM, k-NN) ใช้ไม่ได้ | **สูง** | นับ plugin ที่ใช้จริงตั้งแต่ Phase 0 — บางตัวอาจ block ทั้งโครงการ |
| ทีมยังไม่ชำนาญ Rust | กลาง | Phase 1 เป็น scope เล็กและปลอดภัยพอสำหรับการเรียนรู้ |
| OpenSearch upstream เดินหน้าไปเรื่อย ๆ | กลาง | pin เวอร์ชัน, ทำ compat matrix, ยอมตามหลัง 1-2 minor |
| ประเมินกำไรเกินจริง | กลาง | Phase 0 ต้องได้ตัวเลข baseline ก่อนอนุมัติ Phase 2 |

---

## 7. Go / No-Go Gates

- **หลัง Phase 0**: ถ้า profiling ชี้ว่าคอขวดคือ I/O หรือ query shape ที่แย่ ไม่ใช่ JVM → **หยุด** แล้วไป optimize ของเดิม ถูกกว่ามาก
- **หลัง Phase 1**: ต้องเห็น coordinator RSS ลด ≥60% และ p99 ไม่แย่ลง มิฉะนั้นไม่ผ่าน
- **หลัง Phase 2 POC**: ต้องเห็น search node ที่ RSS ลด ≥50% พร้อม differential diff = 0 บน top-50 query shapes

---

## 8. งานถัดไปที่ทำได้ทันที

1. รัน OpenSearch Benchmark เก็บ baseline (Phase 0)
2. ยืนยันแล้วว่า `transport-grpc` expose `SearchServiceImpl` + `DocumentServiceImpl` (search + index/bulk) — เหลือแค่ตรวจว่า field ใน protobuf ครอบคลุม query DSL ที่เราใช้จริง
3. Prototype: `axum` + `tonic` ยิง `SearchRequest` เข้า OpenSearch node จริง — งาน 2-3 วัน พิสูจน์ Phase 1 ได้
