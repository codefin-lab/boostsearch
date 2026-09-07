# Production Readiness Review — BoostSearch

- วันที่รีวิว: 7 กันยายน 2026 (Asia/Bangkok)
- สถานะ: **ยังไม่พร้อมใช้ใน production ที่ต้องรับประกันความถูกต้องและความคงทนของข้อมูล**
- ข้อค้นพบ: P1 จำนวน 8 รายการ และ P2 จำนวน 1 รายการ
- การดำเนินการ: บันทึกผลรีวิวเท่านั้น ยังไม่ได้แก้ implementation
- HEAD ที่อ่านขณะจัดทำเอกสาร: `425528d9033ab6e9c022311e7709102e3f7c473d`

## ขอบเขตและข้อจำกัด

ตรวจภาพรวม repository และเจาะเส้นทางสำคัญ ได้แก่ document writes, storage/translog, restart/recovery, cluster coordination/replication, security/DLS, snapshots, vector persistence และการตั้งค่า CI/Docker เอกสารนี้ไม่ได้รับรองว่าตรวจทุกบรรทัดหรือพบปัญหาทั้งหมดแล้ว

working tree มีการแก้ไขอยู่ก่อนและระหว่างรีวิว จึงไม่ควรถือว่า HEAD ข้างต้นเป็น snapshot ที่ตรงกับโค้ดทดสอบทุกไฟล์ ผลทดสอบอ้างอิง source ณ เวลาที่ build และเลขบรรทัดด้านล่างอ้างอิงช่วงรีวิว ต้องตรวจซ้ำเมื่อโค้ดเปลี่ยน

ระหว่างจัดทำเอกสารยังพบการแก้ไฟล์ analysis, API และ knn เพิ่มเติม เอกสารนี้เก็บข้อค้นพบจากรอบรีวิวเดิม ไม่ได้ถือว่าการเปลี่ยนแปลงภายหลังแก้ปัญหาแล้ว

ไม่ได้รัน full OpenSearch conformance suite, production-scale soak/load test, fault injection บน cluster จริง, dependency vulnerability audit หรือการทดสอบ deployment ที่เปิด TLS/auth ในรอบนี้

## หลักฐานการตรวจ

| การตรวจ | ผล |
|---|---|
| `cargo test --release --lib --locked` | 176 passed, 0 failed, 0 ignored |
| `cargo build --release --lib --locked` | ผ่าน |
| `cargo fmt --check` | ผ่าน ณ เวลาที่ตรวจ |
| โปรแกรมจำลองที่เรียก release library โดยตรง | ยืนยันข้อ PR-01 ถึง PR-05 |
| Fault injection สำหรับ disk errors และ consensus | ยังไม่ได้รัน; PR-06 และ PR-07 เป็นข้อค้นพบจากการไล่โค้ด |

การทดสอบเริ่มจาก debug build แต่หยุดระหว่างดาวน์โหลดพจนานุกรมญี่ปุ่น แล้วเปลี่ยนเป็น release build ที่มี cache โดยยังเปิด default features ครบ

โปรแกรมจำลองทำงานกับข้อมูลชั่วคราวและเรียกฟังก์ชัน library โดยตรง ไม่ใช่การทดสอบ REST API แบบ end-to-end หรือการ restart process จริง กรณีเปิดข้อมูลกลับใช้การปิดและเปิด Store บนดิสก์ พร้อมเรียก recovery

ผลลัพธ์ที่ได้:

```text
invalid_overwrite: before=1, rejected=true, after=0
dls: invalid_json=true, filter_absent=true
snapshot_without_docs: result=Ok(0)
before_reopen: version=43, routing=Some("tenant-x"), vector_distance=0
after_reopen: version=1, routing=None, vector_distance=64
```

## ระดับความสำคัญ

- **P1:** ต้องแก้และทดสอบยืนยันก่อนอนุมัติ production ในขอบเขตการใช้งานที่ได้รับผลกระทบ
- **P2:** ปัญหาการปฏิบัติการที่ควรแก้ก่อน deployment รูปแบบนั้น
- **ทำซ้ำได้:** โปรแกรมจำลองแสดงพฤติกรรมผิดปกติจริง
- **ไล่โค้ด:** พบเส้นทางที่ทำให้เกิดปัญหา แต่ยังไม่ได้ทดสอบเหตุการณ์จริง

## PR-01 — การเขียนที่ถูกปฏิเสธสามารถลบเอกสารเดิม

**P1 · ทำซ้ำได้**

ตำแหน่ง: [src/api/doc/mod.rs](../src/api/doc/mod.rs), บรรทัด 128–158, ฟังก์ชัน `write_doc_versioned`

โค้ดเพิ่ม version/sequence และ queue การลบเอกสารเดิมก่อนตรวจ `document_complaint` และ `scan_malformed` หาก validation ล้มเหลว จะคืน error โดยไม่ย้อนการเปลี่ยนสถานะหรือเอาการลบออกจาก queue

**วิธีทำซ้ำ:** สร้าง field `n` เป็น integer → เขียนเอกสาร ID `1` ด้วย `{"n":7}` → refresh → เขียน ID เดิมด้วย `{"n":"not-an-integer"}` → พบ error → refresh อีกครั้ง

**ผลจริง:** จำนวนเอกสารลดจาก 1 เป็น 0 แม้การเขียนครั้งที่สองถูกปฏิเสธ

**ผลกระทบ:** request ที่ข้อมูลผิดสามารถทำให้ข้อมูลเดิมหาย รวมถึงสถานะ version ที่เปลี่ยนทั้งที่ write ไม่สำเร็จ

**แนวทางแก้ภายหลัง:** ตรวจและเตรียมข้อมูลให้ครบก่อนเปลี่ยนสถานะ หรือทำให้การเปลี่ยนสถานะเป็น atomic พร้อม rollback

**เกณฑ์ตรวจซ้ำ:** validation error ต้องไม่เปลี่ยนเอกสารเดิม ทั้งก่อน/หลัง refresh และหลังเปิดข้อมูลกลับ รวมถึง bulk/update paths

## PR-02 — DLS ที่ parse ไม่สำเร็จกลายเป็นไม่มีตัวกรอง

**P1 · ทำซ้ำได้**

ตำแหน่ง: [src/security/mod.rs](../src/security/mod.rs), `substitute` บรรทัด 695 และ `dls_query` บรรทัด 933

`substitute` แทนค่า caller ลงในข้อความ JSON โดยไม่ escape ส่วน `dls_query` ข้าม JSON ที่ parse ไม่ผ่าน และคืน `None` เมื่อไม่มี query เหลือ ซึ่ง downstream ใช้เป็นกรณีไม่มี DLS

**วิธีทำซ้ำ:** ใช้ DLS `{"term":{"owner":"${user.name}"}}` กับ caller ที่มีชื่อ `user"quote`

**ผลจริง:** JSON หลังแทนค่าผิดรูป และ `dls_query()` คืน `None`

**ผลกระทบ:** role ที่ควรจำกัดเอกสารอาจอ่านได้โดยไม่มีตัวกรอง เมื่อค่าที่นำมาแทนมี quote หรืออักขระที่ทำให้ JSON ผิดรูป การทดสอบนี้ยืนยันระดับฟังก์ชัน ไม่ได้พิสูจน์ว่าผู้ใช้ทุกระบบสามารถเลือกค่า caller ได้เอง

**แนวทางแก้ภายหลัง:** แทนค่าผ่าน JSON structure/escaping ที่ถูกต้อง และปฏิเสธการอ่านเมื่อสร้าง DLS ไม่สำเร็จ

**เกณฑ์ตรวจซ้ำ:** ชื่อและ attributes ที่มี quote, backslash หรืออักขระพิเศษต้องไม่ทำให้ข้อจำกัดการอ่านหายไป รวมการทดสอบผ่าน authentication และ REST API จริง

## PR-03 — Version และ routing ของเอกสารไม่ถูกกู้คืนหลังเปิดข้อมูลกลับ

**P1 · ทำซ้ำได้**

ตำแหน่ง: [src/store/registry.rs](../src/store/registry.rs), บรรทัด 546–547; [src/store/writer.rs](../src/store/writer.rs), `save_meta`; [src/store/ids.rs](../src/store/ids.rs), `version_of` บรรทัด 82

Store ที่เปิดกลับเริ่ม `versions` และ `routing` เป็น map ว่าง ขณะที่ metadata ที่บันทึกไม่ได้เก็บสองส่วนนี้ และ translog ถูกล้างหลัง commit

**วิธีทำซ้ำ:** เขียนเอกสารบนดิสก์ที่มี routing `tenant-x` และ external version → commit ผ่าน refresh/idle writer release → ปิด Store → เปิด Store เดิมและเรียก recovery

**ผลจริง:** version เปลี่ยนจาก 43 เป็น 1 และ routing จาก `Some("tenant-x")` เป็น `None`

**ผลกระทบ:** external versioning อาจยอมรับข้อมูลเก่า การเลือก shard และการเข้าถึงเอกสารที่ใช้ custom routing อาจผิดหลัง restart

**แนวทางแก้ภายหลัง:** เก็บ version/routing เป็นส่วนหนึ่งของ durable document state และกู้คืนก่อนรับงาน

**เกณฑ์ตรวจซ้ำ:** version/routing ต้องคงเดิมหลัง clean restart, crash recovery และ peer recovery; external version เก่าต้องยังถูกปฏิเสธ

## PR-04 — Vector cache เก่าถูกยอมรับหลังเปิดข้อมูลกลับ

**P1 · ทำซ้ำได้**

ตำแหน่ง: [src/store/writer.rs](../src/store/writer.rs), `release_idle_writer` บรรทัด 88 และ `load_vectors` บรรทัด 139

idle writer release commit เอกสารและอาจล้าง translog โดยไม่บันทึก vector ล่าสุด ส่วน `load_vectors` ตรวจความเพียงพอของจำนวน vector โดยไม่มี commit/generation ที่ยืนยันว่าเป็นข้อมูลชุดเดียวกับเอกสาร

**วิธีทำซ้ำ:** เขียน vector `[1,0]` และ refresh → อัปเดตเป็น `[9,0]` → release idle writer → เปิด Store กลับ → ค้นหาเทียบกับ `[9,0]`

**ผลจริง:** squared L2 distance เปลี่ยนจาก 0 เป็น 64 แสดงว่าโหลด vector เดิมกลับมา

**ผลกระทบ:** vector search ให้ผลลัพธ์ผิดหลัง restart แม้เอกสารล่าสุดถูก commit แล้ว

**แนวทางแก้ภายหลัง:** ผูก vector cache กับ commit/generation และ rebuild เมื่อไม่ตรงกัน หรือทำให้การบันทึกเอกสารและ vector สอดคล้องกันทุกเส้นทาง commit

**เกณฑ์ตรวจซ้ำ:** ผลค้นหาต้องเท่าเดิมหลังเปิดข้อมูลกลับ ครอบคลุม update, delete, idle writer eviction และการ commit จาก memory/translog thresholds

## PR-05 — Restore snapshot ที่ขาดข้อมูลคืนผลสำเร็จ

**P1 · ทำซ้ำได้**

ตำแหน่ง: [src/snapshot.rs](../src/snapshot.rs), `restore_index` บรรทัด 288–314; [src/api/snapshot.rs](../src/api/snapshot.rs), เส้นทาง `restore_snapshot`

หากอ่าน `docs.ndjson` ไม่ได้ ฟังก์ชันคืน `Ok(0)` หลังสร้าง index แล้ว นอกจากนี้ record ที่ parse ไม่ผ่านหรือเขียนไม่ได้ถูกข้าม และ error ของ refresh ถูกทิ้ง API ใช้ผล `Ok` เป็นการ restore สำเร็จ

**วิธีทำซ้ำ:** สร้าง snapshot source ที่มี `meta.json` แต่ไม่มี `docs.ndjson` แล้วเรียก `restore_index`

**ผลจริง:** คืน `Ok(0)` แทนข้อผิดพลาด

**ผลกระทบ:** ผู้ปฏิบัติการอาจเชื่อว่ากู้ข้อมูลสำเร็จทั้งที่ได้ index ว่างหรือข้อมูลไม่ครบ

**แนวทางแก้ภายหลัง:** ตรวจ manifest/checksum/count และส่ง error ของการอ่าน การเขียน และ refresh กลับ; หากรองรับ partial restore ต้องระบุให้ชัดเจน

**เกณฑ์ตรวจซ้ำ:** ไฟล์ขาด เสียหาย หรืออ่านไม่ได้ต้องไม่ถูกรายงานว่า restore สมบูรณ์ และจำนวน/เนื้อหาเอกสารหลัง restore ต้องตรงต้นฉบับ

## PR-06 — Translog กลืนข้อผิดพลาดจาก storage

**P1 · ไล่โค้ด; ยังไม่ได้จำลอง disk failure**

ตำแหน่ง: [src/store/translog.rs](../src/store/translog.rs), `open_translog` บรรทัด 7, `log_write` บรรทัด 43 และ `flush_translog` บรรทัด 71–86; [src/api/doc/mod.rs](../src/api/doc/mod.rs), `maybe_refresh`

มีการทิ้ง error ของ open, write, flush และ fsync ฟังก์ชันไม่คืน Result ให้ API จึงมีเส้นทางตอบ write สำเร็จโดยไม่ได้รับประกันว่าข้อมูล durable แม้ใช้ request durability

**ผลกระทบ:** เมื่อ disk เต็ม, permission ผิด หรือ I/O ล้มเหลว อาจสูญเสีย write ที่ตอบรับแล้วหลัง process หยุด

**แนวทางแก้ภายหลัง:** ส่ง error ถึง request และเปลี่ยน shard/node ไปสู่สถานะที่ไม่รับ write จนกู้ storage ได้

**เกณฑ์ตรวจซ้ำ:** fault injection สำหรับ open/write/flush/fsync ต้องไม่คืน success เมื่อรับประกัน durability ไม่ได้

## PR-07 — Coordination เดินหน้าต่อเมื่อ persist state ล้มเหลว

**P1 · ไล่โค้ด; ยังไม่ได้จำลอง consensus storage failure**

ตำแหน่ง: [src/cluster/runtime.rs](../src/cluster/runtime.rs), `save_durable` บรรทัด 97–121 และจุดเรียกก่อนส่ง outputs ประมาณบรรทัด 201

เมื่อเขียน durable coordination state ไม่สำเร็จ โค้ดเพียง log error แล้วเดินหน้าอัปเดต shared state และส่ง outputs ต่อ ไม่มี error gate หยุดคำตอบที่พึ่งพาการ persist

**ผลกระทบ:** เสี่ยงทำลายเงื่อนไข consensus เมื่อ node ตอบรับ vote/accepted state แล้ว crash แต่ไม่สามารถอ่านคำมั่นเดิมกลับมาได้ ยังไม่ได้สาธิต split-brain ในรอบนี้

**แนวทางแก้ภายหลัง:** persist ต้องสำเร็จก่อนส่งผลที่เกี่ยวข้อง มิฉะนั้นหยุด participation หรือ fail node อย่างชัดเจน

**เกณฑ์ตรวจซ้ำ:** จำลอง persistence failure ระหว่าง vote/accept/commit แล้ว restart ต้องรักษา consensus invariants และไม่ส่ง acknowledgement ที่ไม่มี durable state รองรับ

## PR-08 — Optimistic concurrency ตรวจ primary term เป็น 1 ตายตัว

**P1 · ไล่โค้ด; ยังไม่ได้ทดสอบ failover ผ่าน REST**

ตำแหน่ง: [src/api/doc/validate.rs](../src/api/doc/validate.rs), `seq_check` บรรทัด 90–105; [src/cluster/mod.rs](../src/cluster/mod.rs), `primary_term` บรรทัด 146

`seq_check` ตรวจ `if_primary_term` ด้วย `t == 1` แทน term ของ shard จริง ขณะที่ระบบ cluster มี primary term ที่เปลี่ยนหลัง failover

**ผลกระทบ:** request ที่ใช้ term ล่าสุดอาจถูกปฏิเสธ และ request ที่ถือ term เก่าอาจผ่านเมื่อ sequence ตรง

**แนวทางแก้ภายหลัง:** ใช้ term จาก durable shard/document state ให้สอดคล้องกับค่าที่ API รายงาน

**เกณฑ์ตรวจซ้ำ:** หลัง failover ต้องยอมรับเงื่อนไขล่าสุดที่ถูกต้องและปฏิเสธเงื่อนไขเก่า ครอบคลุม index/update/delete/bulk

## PR-09 — Docker healthcheck ไม่รองรับ TLS/auth

**P2 · ไล่ configuration; ยังไม่ได้รัน container**

ตำแหน่ง: [Dockerfile](../Dockerfile), บรรทัด 56–57

healthcheck เรียก `http://127.0.0.1:9200/_cluster/health` โดยไม่มี credentials ตายตัว เมื่อเปิด HTTPS หรือบังคับ authentication การตรวจนี้จะล้มเหลวได้ทั้งที่ node ทำงานปกติ

**ผลกระทบ:** monitoring หรือ orchestration ที่อาศัย health status อาจมอง node ที่ใช้งานได้เป็น unhealthy

**แนวทางแก้ภายหลัง:** ให้ probe รองรับ protocol, CA และ credentials ที่ deployment ใช้ หรือออกแบบ readiness endpoint ที่เหมาะสม

**เกณฑ์ตรวจซ้ำ:** ตรวจ image ทั้ง security-off, auth-on และ TLS-on โดยต้องแยก healthy/unready ได้ถูกต้อง

## สิ่งที่ต้องทำก่อนพิจารณา production อีกครั้ง

1. แก้และเพิ่ม regression coverage สำหรับ PR-01 ถึง PR-08 ตามขอบเขตการใช้งาน
2. รัน fault injection สำหรับ storage failure และการ restart จริง เพื่อยืนยันว่า acknowledged writes ไม่หาย
3. ตรวจ failover, optimistic concurrency, routing และ peer recovery บนหลาย node จริง
4. ทดสอบ backup/restore ด้วยข้อมูลที่มีจำนวนและ checksum ตรวจเทียบได้ รวมกรณีไฟล์เสียหรือขาด
5. ตรวจ DLS ผ่าน authentication/REST จริง และทดสอบ deployment ที่เปิด TLS/auth
6. รัน full conformance และ workload ของระบบเป้าหมายใน staging รวม soak/load test ก่อนอนุมัติ cutover

รายการนี้เป็นแผนตรวจซ้ำ ไม่ใช่งานแก้ไขที่ดำเนินการแล้ว การผ่าน unit tests เดิมเพียงอย่างเดียวยังไม่เพียงพอ เพราะโปรแกรมจำลองพบพฤติกรรมผิดปกติที่ tests เหล่านั้นไม่ครอบคลุม
