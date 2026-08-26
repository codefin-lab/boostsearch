# ศึกษา Tantivy 0.26.1 — ประเมินความพร้อมแทน Lucene

> อ้างอิงจากซอร์สจริงของ `tantivy-0.26.1` ที่ vendor อยู่ใน `~/.cargo/registry`
> เทียบกับ OpenSearch `3.9.0` (Lucene 10.5.0)

---

## 1. ภาพรวมขนาดและสถาปัตยกรรม

| | Tantivy 0.26 | OpenSearch server |
|---|---:|---:|
| LOC (Rust / Java) | **101,623** (268 ไฟล์) | 930,300 (5,070 ไฟล์) |

Tantivy คือ **library ระดับเดียวกับ Lucene** ไม่ใช่ระดับ OpenSearch — ไม่มี cluster, ไม่มี REST, ไม่มี mapping engine, ไม่มี query DSL แบบ JSON ⇒ ส่วนที่เราต้องเขียนเองคือ "ชั้น OpenSearch" ที่ครอบมัน

โครงสร้างที่ตรงกับ Lucene เกือบ 1:1:

| Tantivy | เทียบเท่าใน Lucene |
|---|---|
| `Index` / `Segment` / `SegmentMeta` | `IndexWriter` / segment |
| `directory/` (`MmapDirectory`, `RamDirectory`) | `Directory` / `MMapDirectory` |
| `postings/`, `positions/`, `termdict/` (FST + sstable) | postings + term dictionary |
| `columnar` (crate `tantivy-columnar`) | doc values |
| `fastfield/` | `NumericDocValues` |
| `indexer/log_merge_policy.rs` | `TieredMergePolicy` |
| `Executor::multi_thread` (rayon pool) | search thread pool |

---

## 2. สิ่งที่ Tantivy **มี** และใช้ได้เลย

### 2.1 Query (ทดสอบจริงแล้วใน `src/bin/smoke.rs` — ผ่านทั้งหมด)
`TermQuery`, `BooleanQuery` (+ `minimum_required_clauses`), `RangeQuery`, `PhraseQuery`,
`PhrasePrefixQuery`, `RegexQuery`, `FuzzyTermQuery`, `ExistsQuery`, `TermSetQuery`,
`BoostQuery`, `ConstScoreQuery`, `DisjunctionMaxQuery`, `AllQuery`, `EmptyQuery`,
`MoreLikeThisQuery`, `RegexPhraseQuery`

### 2.2 JSON field = dynamic mapping ฟรี ⭐ ค้นพบที่สำคัญที่สุด
`add_json_field` + `set_expand_dots_enabled()` ทำให้ index เอกสารรูปแบบใด ๆ ได้โดยไม่ต้องประกาศ schema
Tantivy แยกชนิด (`i64`/`u64`/`f64`/`bool`/`Str`/`Date`) ให้เองต่อ path

**เทคนิคที่ใช้แทน text/keyword duality ของ OpenSearch** — ทำ JSON field สองตัวบนเอกสารเดียวกัน:
- `_dyn` → tokenizer `default` (+positions) = พฤติกรรม `text`
- `_raw` → tokenizer `raw`, `set_fast(Some("raw"))` = พฤติกรรม `keyword` + doc values สำหรับ sort/agg

`Term::from_field_json_path(field, path, true)` ใช้ยิง term เข้า path ได้ตรง ๆ
`AutomatonWeight::new_for_json_path` ทำให้ prefix/wildcard/regexp ทำงานบน JSON path ได้ (ต้องเขียน `Query` wrapper เอง — ไม่มีให้สำเร็จรูป)

### 2.3 Aggregation — **JSON เข้ากันได้กับ OpenSearch อยู่แล้ว** ⭐
`Aggregations` deserialize จาก JSON ที่หน้าตาเหมือน ES/OS ทุกประการ และผลลัพธ์ก็คืน
`sum_other_doc_count` / `doc_count_error_upper_bound` มาให้เลย

- bucket: `terms`, `range`, `histogram`, `date_histogram`, `filter`, `composite` (+`after_key`)
- metric: `avg`, `min`, `max`, `sum`, `stats`, `extended_stats`, `value_count`, `percentiles`, `cardinality`, `top_hits`
- `DistributedAggregationCollector` มีให้ — เตรียมทางไว้สำหรับ scatter-gather หลายชาร์ด

### 2.4 อื่น ๆ
- Stored fields → เก็บ `_source` ได้ตรงไปตรงมา
- `SnippetGenerator` → highlight ได้แบบพื้นฐาน (ไม่ใช่ unified/fvh ของ OS)
- `Explanation` → รองรับ `_explain`
- `IndexWriter::delete_term` / `delete_query` / `delete_all_documents` / `rollback`
- `TopDocs::and_offset` → `from`/`size`
- Sorting: `order_by_fast_field`, `order_by_string_fast_field`, `order_by`, `tweak_score`
- Multi-value field รองรับในตัว
- Tokenizer: `raw`, `default`, `whitespace`, `en_stem`, ngram, regex, ascii-folding, stop words, stemmer **17 ภาษา**

---

## 3. สิ่งที่ Tantivy **ไม่มี** (ยืนยันจากซอร์ส ไม่ใช่จากความจำ)

| ฟีเจอร์ OpenSearch | สถานะใน tantivy | ทางออก |
|---|---|---|
| `nested` / `inner_hits` | **ไม่มี** (0 ไฟล์อ้างถึง block-join) | flatten ตอน index หรือเขียน block-join เอง — งานใหญ่ |
| parent/child (`has_child`) | **ไม่มี** | เลี่ยง / denormalize |
| geo (`geo_point`, `geo_distance`) | **ไม่มี** (0 hit จริง) | ใช้ crate `geo` + s2/h3 เขียนเอง |
| field collapsing | **ไม่มี** | post-process บน coordinator |
| `rescore` | **ไม่มี** | ทำเองหลัง collect |
| suggesters (completion/phrase) | **ไม่มี** | FST มีอยู่แล้ว ต่อยอดได้ |
| `search_after` บน hits | **ไม่มี** (มีแค่ `after_key` ของ composite agg) | เขียน collector เอง |
| scroll / PIT | **ไม่มี** | ใช้ `IndexReader` snapshot ทำเองได้ |
| scripting (painless) | **ไม่มี** | ไม่ทำ / ใช้ rhai ถ้าจำเป็น |
| percolate | **ไม่มี** (0 hit) | ไม่ทำ |
| `significant_terms`, pipeline aggs, geo aggs, `multi_terms` | **ไม่มี** | เขียนบน framework agg ของ tantivy |
| `_seq_no` / `_primary_term` / optimistic concurrency | **ไม่มี** | จัดการที่ชั้นบน |
| partial update (`_update`) | **ไม่มี** doc update | อ่าน `_source` → merge → delete+add |
| `text`/`keyword` แยกกันจริง ๆ | ไม่มี — แก้ด้วยทริค `_dyn`/`_raw` | ✅ แก้แล้ว |

**ข้อจำกัดเชิงโครงสร้างที่ต้องยอมรับ:** segment format ของ tantivy ≠ Lucene ⇒ ต้อง **re-index ทั้งหมด** อ่าน index เดิมของ OpenSearch ไม่ได้ นี่คือราคาของการเลือกเส้นทางนี้ และเป็นเหตุผลที่ต้อง dual-write ระหว่าง migrate

---

## 4. ช่องว่างเหล่านี้กระทบ test set เดิมแค่ไหน (วัดจริง)

จากการสแกน test เดิม 409 ไฟล์ (`tools/analyze_tests.py`, `tools/gap_scan.py`):

```
ไฟล์ทั้งหมด                        409   (8,329 assertions)
อยู่ในกลุ่ม API ที่คนใช้บ่อย         171   (3,933 assertions)
  └─ ไม่แตะช่องว่างของ tantivy เลย  124   (2,568 assertions)  ← เป้า Phase 1
  └─ แตะช่องว่าง ≥1 อย่าง            47
```

ช่องว่างที่บล็อกไฟล์มากที่สุด:

| ช่องว่าง | บล็อก |
|---|---:|
| nested / inner_hits | 20 ไฟล์ |
| geo | 9 |
| suggesters | 7 |
| highlighting | 7 |
| significant_terms | 5 |
| _seq_no / versioning | 5 |
| rescore | 3 |
| pipeline aggs | 3 |

⇒ **สรุป: ไม่มีช่องว่างไหนของ tantivy ที่บล็อก Phase 1** ทุกตัวอยู่ในหาง 5-10% ที่ผลักไป Phase 3 ได้ตามแผน

---

## 5. คุณสมบัติด้าน Resource / Performance

- `MmapDirectory` — index อยู่ใน page cache ของ OS ไม่ใช่ heap ⇒ ไม่มี fielddata OOM แบบ JVM
- `columnar` (doc values) อ่านแบบ zero-copy จาก mmap
- Indexing memory เป็น **arena ที่กำหนดเพดานชัดเจน** (`memory_budget_per_thread`, ขั้นต่ำ 15MB, สูงสุด ~4GB) — เทียบกับ JVM ที่ต้องเผื่อ heap ให้ GC
- `Executor::multi_thread` (rayon) สำหรับ search ข้าม segment — ต่างจาก OpenSearch ที่มี thread pool 25+ ตัว
- ไม่มี GC ⇒ p99 ไม่มี pause

---

## 6. ข้อสรุปสำหรับการตัดสินใจ

✅ **Tantivy เพียงพอสำหรับ Phase 1** — 124 ไฟล์ / 2,568 assertions ทำได้ด้วยของที่มีอยู่
⚠️ ต้องเขียนเองเหนือ tantivy: query DSL parser, mapping engine, REST layer, `_source` handling, bulk/mget/msearch, update semantics, index management
❌ ต้องยอมรับ: re-index เท่านั้น อ่าน Lucene segment เดิมไม่ได้
