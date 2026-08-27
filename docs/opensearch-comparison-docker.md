# obsearch vs OpenSearch 3.1 — วัดใน Docker ทั้งสองฝั่ง

เครื่อง: Apple Silicon 14 core / 36 GB · Docker Desktop 14 cpu / 16 GB
OpenSearch 3.1.0 heap 512 MB (ที่ 2 GB ช้ากว่า — เคยวัดไว้แล้ว จึงใช้ 512 MB
ซึ่งเป็นค่าที่เข้าข้าง OpenSearch)

ทั้งสองฝั่งอยู่ใน container เหมือนกัน ใช้ mapping เดียวกัน dataset เดียวกัน
harness เดียวกัน (`tools/bench.py`, `Dockerfile`)

## กรณีที่ 1 — index เดียว 200,000 docs (workload http_logs)

| | obsearch | OpenSearch | |
|---|---:|---:|---|
| index docs/s | 70,034 | 70,643 | 0.99× |
| RSS idle | **19.6 MB** | 1,855.4 MB | 94.7× |
| RSS หลัง index | **192.0 MB** | 1,873.7 MB | 9.8× |
| RSS หลัง search | **201.4 MB** | 1,910.2 MB | 9.5× |
| qps c=1 | 545.6 | 459.0 | 1.19× |
| p99 c=1 | 2.62 ms | 3.43 ms | 1.31× |
| qps c=8 | 1,940.7 | 1,621.7 | 1.20× |
| p99 c=8 | **5.80 ms** | 8.22 ms | 1.42× |

รายละเอียดต่อ query (p50, c=1):

| query | obsearch | OpenSearch | |
|---|---:|---:|---|
| match_all | 1.77 | 1.93 | 1.09× |
| term_keyword | 1.61 | 1.89 | 1.17× |
| term_numeric | 1.67 | 2.03 | 1.22× |
| range_numeric | 1.77 | 1.76 | **0.99×** |
| match_text | 1.69 | 2.24 | 1.33× |
| bool_filter | 1.91 | 2.52 | 1.32× |
| agg_terms | 1.97 | 2.00 | 1.02× |
| agg_date_hist | 1.78 | 1.98 | 1.11× |
| agg_nested | 2.08 | 1.88 | **0.90×** |
| sort_paged | 1.85 | 2.50 | 1.35× |
| time_range | 1.84 | 2.13 | 1.16× |
| time_range_agg | 1.94 | 2.04 | 1.05× |

**ยังแพ้อยู่สองตัว**: `agg_nested` บน index เดียว (0.90×) และ `range_numeric` เสมอ
indexing เสมอกันพอดี

## กรณีที่ 2 — 200 index × 2,000 docs (400,000 docs)

| query | obsearch | OpenSearch | |
|---|---:|---:|---|
| match_all | 1.86 | 4.45 | 2.39× |
| term | 1.70 | 4.74 | 2.78× |
| agg_terms | 3.16 | 5.92 | 1.87× |
| agg_stats | 3.32 | 4.17 | 1.26× |
| agg_nested | 3.89 | 5.63 | 1.45× |
| sort_paged | 3.17 | 10.67 | 3.37× |
| index build | 7.6 s | 13.1 s | 1.74× |
| memory | **414 MB** | 1,939 MB | 4.68× |

## อ่านผลอย่างไร

**เรื่องที่ชัด** — memory ต่างกันคนละชั้น (9.5× บน index เดียว, 4.7× บน 200 index)
และ fan-out หลาย index เราเร็วกว่าชัดเจนทุกตัว

**เรื่องที่ควรระวัง** — บน index เดียว ความต่างด้าน latency อยู่ราว 1.2× ซึ่งไม่มาก
และเราแพ้ `agg_nested` อยู่จริง การบอกว่า "เร็วกว่า OpenSearch" โดยไม่ระบุรูปงาน
จึงไม่ตรง: **จุดแข็งของเราคือ memory และ fan-out ไม่ใช่ latency บน index เดียว**

**indexing เสมอกันใน Docker แต่ไม่เสมอกันนอก Docker** — native เราทำได้
96,646 docs/s แต่ใน container เหลือ 70,034 (−27%) ส่วน OpenSearch แทบไม่ต่าง
ค่านี้เป็นของ Docker Desktop บน macOS ไม่ใช่ของเครื่องยนต์ ยังไม่ได้ยืนยันบน
Linux host

## สิ่งที่การวัดชุดนี้ยังไม่ตอบ

- ตัวเลข absolute บน Linux host จริง (penalty ~1.9× ที่เหลือกระทบทั้งสองฝั่ง)
- workload ที่ใหญ่กว่า memory (ทั้งคู่ยัง fit)
- ความถูกต้องของผลลัพธ์ไม่ได้ diff ในการวัดรอบนี้ — ใช้ชุด test ของ OpenSearch
  แยกต่างหาก (388/398)
