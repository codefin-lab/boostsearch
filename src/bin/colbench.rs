//! What would a custom columnar layout actually buy?
//!
//! Pulls the real column values out of the index, then runs the same value-range
//! scan four ways: BoostCore's current path, a plain scalar scan, a scan with
//! per-block min/max skipping, and a chunked scan the compiler can vectorise.
//! The point is to size the opportunity before building anything.
use boostsearch::store::Store;
use std::time::Instant;

const BLOCK: usize = 512;

fn scalar_scan(vals: &[u64], lo: u64, hi: u64, out: &mut Vec<u32>) {
    out.clear();
    for (i, v) in vals.iter().enumerate() {
        if *v >= lo && *v <= hi {
            out.push(i as u32);
        }
    }
}

/// Per-block min/max lets an entire block be skipped, or accepted wholesale.
fn block_skip_scan(
    vals: &[u64],
    stats: &[(u64, u64)],
    lo: u64,
    hi: u64,
    out: &mut Vec<u32>,
) -> (usize, usize) {
    out.clear();
    let mut skipped = 0;
    let mut whole = 0;
    for (b, (bmin, bmax)) in stats.iter().enumerate() {
        let start = b * BLOCK;
        let end = (start + BLOCK).min(vals.len());
        if *bmax < lo || *bmin > hi {
            skipped += 1;
            continue;
        }
        if *bmin >= lo && *bmax <= hi {
            // every value qualifies: no comparisons needed at all
            whole += 1;
            out.extend(start as u32..end as u32);
            continue;
        }
        for (i, &v) in vals.iter().enumerate().take(end).skip(start) {
            if v >= lo && v <= hi {
                out.push(i as u32);
            }
        }
    }
    (skipped, whole)
}

/// Branch-free chunked form; LLVM auto-vectorises this to SIMD compares.
fn chunked_scan(vals: &[u64], lo: u64, hi: u64, out: &mut Vec<u32>) {
    out.clear();
    let mut mask = [false; 8];
    for (c, chunk) in vals.chunks(8).enumerate() {
        for (j, v) in chunk.iter().enumerate() {
            mask[j] = *v >= lo && *v <= hi;
        }
        for (j, hit) in mask.iter().enumerate().take(chunk.len()) {
            if *hit {
                out.push((c * 8 + j) as u32);
            }
        }
    }
}

fn block_stats(vals: &[u64]) -> Vec<(u64, u64)> {
    vals.chunks(BLOCK)
        .map(|c| {
            let mut lo = u64::MAX;
            let mut hi = 0u64;
            for v in c {
                lo = lo.min(*v);
                hi = hi.max(*v);
            }
            (lo, hi)
        })
        .collect()
}

fn time_it(name: &str, n: usize, mut f: impl FnMut() -> usize) {
    for _ in 0..3 {
        f();
    }
    let t = Instant::now();
    let mut hits = 0;
    for _ in 0..n {
        hits = f();
    }
    println!(
        "    {name:<28}{:>9.0} us{:>12} hits",
        t.elapsed().as_secs_f64() * 1e6 / n as f64,
        hits
    );
}

fn main() -> anyhow::Result<()> {
    let data = std::env::var("BOOSTSEARCH_DATA").unwrap_or("/tmp/kinds-data".into());
    let store = Store::on_disk(&data)?;
    let st = store.get("bench_logs").expect("index bench_logs");
    let g = st.read();
    let searcher = g.reader.searcher();

    for column in ["_dyn.size", "_dyn.@timestamp", "_dyn.response_ms"] {
        let mut vals: Vec<u64> = Vec::new();
        for seg in searcher.segment_readers() {
            let Ok(Some((col, _ty))) = seg.fast_fields().u64_lenient(column) else { continue };
            for doc in 0..seg.max_doc() {
                if let Some(v) = col.first(doc) {
                    vals.push(v);
                }
            }
        }
        if vals.is_empty() {
            println!("  {column}: no values");
            continue;
        }
        let stats = block_stats(&vals);
        let min = *vals.iter().min().unwrap();
        let max = *vals.iter().max().unwrap();
        // a middle slice of the value range, roughly a quarter of the rows
        let lo = min + (max - min) / 4;
        let hi = min + (max - min) / 2;

        let mut out = Vec::with_capacity(vals.len());
        let (skipped, whole) = block_skip_scan(&vals, &stats, lo, hi, &mut out);
        println!(
            "\n  {column}  ({} values, {} blocks: {} skippable, {} fully inside)",
            vals.len(),
            stats.len(),
            skipped,
            whole
        );

        let n = 200;
        time_it("scalar scan", n, || {
            scalar_scan(&vals, lo, hi, &mut out);
            out.len()
        });
        time_it("chunked (auto-vectorised)", n, || {
            chunked_scan(&vals, lo, hi, &mut out);
            out.len()
        });
        time_it("block min/max skipping", n, || {
            block_skip_scan(&vals, &stats, lo, hi, &mut out);
            out.len()
        });

        // Feasibility check: keep BoostCore's columnar format, but drive its
        // existing docid-range API one surviving block at a time using a
        // sidecar of per-block min/max. No fork required.
        for seg in searcher.segment_readers() {
            let Ok(Some((col, _))) = seg.fast_fields().u64_lenient(column) else { continue };
            let max_doc = seg.max_doc();
            let mut seg_vals: Vec<u64> = Vec::with_capacity(max_doc as usize);
            for doc in 0..max_doc {
                seg_vals.push(col.first(doc).unwrap_or(0));
            }
            let seg_stats = block_stats(&seg_vals);
            let mut docids: Vec<u32> = Vec::new();
            time_it("sidecar block-range dispatch", n, || {
                docids.clear();
                let mut b = 0usize;
                while b < seg_stats.len() {
                    let (bmin, bmax) = seg_stats[b];
                    let start_doc = (b * BLOCK) as u32;
                    let end_doc = (((b + 1) * BLOCK) as u32).min(max_doc);
                    if bmax < lo || bmin > hi {
                        b += 1; // block cannot contain a match
                        continue;
                    }
                    if bmin >= lo && bmax <= hi {
                        // whole block qualifies: emit the doc ids, no comparisons
                        docids.extend(start_doc..end_doc);
                        b += 1;
                        continue;
                    }
                    // only partially covered blocks need real value comparisons
                    let first = b;
                    while b < seg_stats.len() {
                        let (m, x) = seg_stats[b];
                        let partial = x >= lo && m <= hi && !(m >= lo && x <= hi);
                        if !partial {
                            break;
                        }
                        b += 1;
                    }
                    let from = (first * BLOCK) as u32;
                    let to = ((b * BLOCK) as u32).min(max_doc);
                    col.get_docids_for_value_range(lo..=hi, from..to, &mut docids);
                }
                docids.len()
            });
            break;
        }

        // what BoostCore does today, through the real column
        let mut docids: Vec<u32> = Vec::new();
        for seg in searcher.segment_readers() {
            let Ok(Some((col, _))) = seg.fast_fields().u64_lenient(column) else { continue };
            let max_doc = seg.max_doc();
            time_it("BoostCore get_docids_for_value_range", n, || {
                docids.clear();
                col.get_docids_for_value_range(lo..=hi, 0..max_doc, &mut docids);
                docids.len()
            });
        }
    }
    Ok(())
}
