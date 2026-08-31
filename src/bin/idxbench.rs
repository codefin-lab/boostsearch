//! Where does bulk indexing time actually go?
//!
//! Drives the same store/write path the HTTP handler uses, timing each stage,
//! so optimisation targets come from measurement rather than intuition.
use serde_json::Value;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).unwrap_or("bench/data/http_logs.ndjson".into());
    let raw = std::fs::read_to_string(&path)?;
    let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
    println!("{} docs from {path}", lines.len());

    // 1. JSON parsing alone
    let t = Instant::now();
    let docs: Vec<Value> = lines.iter().map(|l| serde_json::from_str(l).unwrap()).collect();
    let parse = t.elapsed();

    let store = boostsearch::store::Store::new();
    store.create("bench", &serde_json::json!({}))?;
    let st = store.get("bench").unwrap();

    // 2. lock acquisition, once per document (what the bulk handler does today)
    let t = Instant::now();
    for _ in 0..docs.len() {
        let g = st.write();
        std::hint::black_box(&g.name);
    }
    let lock_per_doc = t.elapsed();

    // 3. store.ensure() per document -- map lookup plus a possible alias scan
    let t = Instant::now();
    for _ in 0..docs.len() {
        std::hint::black_box(store.ensure("bench").unwrap());
    }
    let ensure_per_doc = t.elapsed();

    // 4. the write itself, split by stage
    let mut g = st.write();
    let mut t_observe = std::time::Duration::ZERO;
    let mut t_serialize = std::time::Duration::ZERO;
    let mut t_makedoc = std::time::Duration::ZERO;
    let mut t_add = std::time::Duration::ZERO;
    let mut t_pending = std::time::Duration::ZERO;
    let mut t_bump = std::time::Duration::ZERO;

    let total = Instant::now();
    for (i, doc) in docs.iter().enumerate() {
        let id = format!("{i}");
        let t = Instant::now();
        g.bump(&id, true, false);
        t_bump += t.elapsed();

        let t = Instant::now();
        g.observe(doc);
        t_observe += t.elapsed();

        let t = Instant::now();
        let raw = doc.to_string();
        t_serialize += t.elapsed();

        let t = Instant::now();
        let d = boostsearch::store::make_doc(&g.fields, &id, doc.clone(), &raw, i as u64);
        t_makedoc += t.elapsed();

        let t = Instant::now();
        g.writer()?.add_document(d)?;
        t_add += t.elapsed();

        let t = Instant::now();
        g.note_pending(&id, Some(raw));
        t_pending += t.elapsed();
    }
    let write_total = total.elapsed();

    let t = Instant::now();
    g.refresh()?;
    let commit = t.elapsed();

    let n = docs.len() as f64;
    let row = |name: &str, d: std::time::Duration| {
        println!(
            "  {name:<22} {:>8.0} ms   {:>7.2} us/doc   {:>5.1}%",
            d.as_secs_f64() * 1000.0,
            d.as_secs_f64() * 1e6 / n,
            d.as_secs_f64() / write_total.as_secs_f64() * 100.0
        );
    };
    println!("\n-- make_doc variants --");
    variants(&g.fields, &docs);

    println!("\n-- per-document overheads the handler adds --");
    row("json parse", parse);
    row("write lock x1/doc", lock_per_doc);
    row("store.ensure x1/doc", ensure_per_doc);
    println!("\n-- inside write_doc --");
    row("bump (versions)", t_bump);
    row("observe", t_observe);
    row("serialize _source", t_serialize);
    row("make_doc", t_makedoc);
    row("writer.add_document", t_add);
    row("note_pending", t_pending);
    println!("  {:<22} {:>8.0} ms", "WRITE TOTAL", write_total.as_secs_f64() * 1000.0);
    row("commit + refresh", commit);
    println!(
        "\n  effective {:.0} docs/s (write only), {:.0} docs/s (with commit)",
        n / write_total.as_secs_f64(),
        n / (write_total + commit).as_secs_f64()
    );
    Ok(())
}

// Variants of the document build, timed against each other.
#[allow(dead_code)]
fn variants(fields: &boostsearch::store::Fields, docs: &[Value]) {
    use boostcore::TantivyDocument;
    use boostcore::schema::OwnedValue;
    use std::collections::BTreeMap;
    let n = docs.len() as f64;

    let t = Instant::now();
    for doc in docs {
        let mut d = TantivyDocument::default();
        if let Value::Object(obj) = doc.clone() {
            let converted: BTreeMap<String, OwnedValue> =
                obj.into_iter().map(|(k, v)| (k, OwnedValue::from(v))).collect();
            d.add_object(fields.dynamic, converted.clone());
            d.add_object(fields.raw, converted);
        }
        std::hint::black_box(d);
    }
    println!("  move + clone           {:>7.2} us/doc", t.elapsed().as_secs_f64() * 1e6 / n);

    let t = Instant::now();
    for doc in docs {
        let mut d = TantivyDocument::default();
        if let Some(obj) = doc.as_object() {
            let a: BTreeMap<String, OwnedValue> =
                obj.iter().map(|(k, v)| (k.clone(), OwnedValue::from(v.clone()))).collect();
            let b: BTreeMap<String, OwnedValue> =
                obj.iter().map(|(k, v)| (k.clone(), OwnedValue::from(v.clone()))).collect();
            d.add_object(fields.dynamic, a);
            d.add_object(fields.raw, b);
        }
        std::hint::black_box(d);
    }
    println!("  two conversions        {:>7.2} us/doc", t.elapsed().as_secs_f64() * 1e6 / n);

    // only string leaves need the raw view; numerics are identical in both
    let t = Instant::now();
    for doc in docs {
        let mut d = TantivyDocument::default();
        if let Value::Object(obj) = doc.clone() {
            let strings: BTreeMap<String, OwnedValue> = obj
                .iter()
                .filter(|(_, v)| v.is_string())
                .map(|(k, v)| (k.clone(), OwnedValue::from(v.clone())))
                .collect();
            let all: BTreeMap<String, OwnedValue> =
                obj.into_iter().map(|(k, v)| (k, OwnedValue::from(v))).collect();
            d.add_object(fields.dynamic, all);
            d.add_object(fields.raw, strings);
        }
        std::hint::black_box(d);
    }
    println!("  strings-only raw view  {:>7.2} us/doc", t.elapsed().as_secs_f64() * 1e6 / n);

    let t = Instant::now();
    for doc in docs {
        let mut d = TantivyDocument::default();
        if let Value::Object(obj) = doc.clone() {
            let all: BTreeMap<String, OwnedValue> =
                obj.into_iter().map(|(k, v)| (k, OwnedValue::from(v))).collect();
            d.add_object(fields.dynamic, all);
        }
        std::hint::black_box(d);
    }
    println!("  single view (floor)    {:>7.2} us/doc", t.elapsed().as_secs_f64() * 1e6 / n);
}
