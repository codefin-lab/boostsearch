//! Where does aggregation time go?
//!
//! Splits one aggregation request into the stages our code actually performs,
//! so the expensive one can be found rather than guessed at.
use obsearch::store::Store;
use serde_json::json;
use std::time::Instant;
use boostcore::aggregation::agg_req::Aggregations;
use boostcore::aggregation::{
    AggContextParams, AggregationCollector, DistributedAggregationCollector,
};
use boostcore::query::AllQuery;

fn main() -> anyhow::Result<()> {
    let data = std::env::var("OBSEARCH_DATA").unwrap_or("/tmp/blk".into());
    let n: usize = std::env::var("ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(20);
    let store = Store::on_disk(&data)?;
    let st = store.get("bench_logs").expect("bench_logs");
    let g = st.read();
    let searcher = g.reader.searcher();
    println!("{} segments, {} docs", searcher.segment_readers().len(), searcher.num_docs());

    let cases = vec![
        ("agg_terms_raw", json!({"by_status": {"terms": {"field": "_raw.status", "size": 10}}})),
        ("agg_terms_dyn", json!({"by_status": {"terms": {"field": "_dyn.status", "size": 10}}})),
        ("agg_terms_kw_raw", json!({"by_region": {"terms": {"field": "_raw.region", "size": 10}}})),
        ("agg_terms_kw_dyn", json!({"by_region": {"terms": {"field": "_dyn.region", "size": 10}}})),
        ("agg_date_hist", json!({"over_time": {
            "date_histogram": {"field": "_raw.@timestamp", "fixed_interval": "1d"}}})),
        ("agg_date_hist_dyn", json!({"over_time": {
            "date_histogram": {"field": "_dyn.@timestamp", "fixed_interval": "1d"}}})),
        ("agg_nested", json!({"by_region": {"terms": {"field": "_raw.region"},
            "aggs": {"avg_ms": {"avg": {"field": "_raw.response_ms"}},
                     "p_size": {"stats": {"field": "_raw.size"}}}}})),
        ("agg_nested_dyn", json!({"by_region": {"terms": {"field": "_dyn.region"},
            "aggs": {"avg_ms": {"avg": {"field": "_dyn.response_ms"}},
                     "p_size": {"stats": {"field": "_dyn.size"}}}}})),
    ];

    let bench = |name: &str, f: &mut dyn FnMut()| {
        for _ in 0..3 {
            f();
        }
        let t = Instant::now();
        for _ in 0..n {
            f();
        }
        println!("    {name:<34}{:>9.0} us", t.elapsed().as_secs_f64() * 1e6 / n as f64);
    };

    for (name, req) in &cases {
        println!("\n  {name}");
        let parsed: Aggregations = serde_json::from_value(req.clone())?;

        // 1. turning the request JSON into BoostCore's aggregation model
        let mut b = || {
            let _: Aggregations = serde_json::from_value(req.clone()).unwrap();
        };
        bench("parse request", &mut b);

        // 2. the distributed collector we use today, plus its finalisation
        let mut b = || {
            let ctxp = AggContextParams::new(Default::default(), g.index.tokenizers().clone());
            let res = searcher
                .search(&AllQuery, &DistributedAggregationCollector::from_aggs(parsed.clone(), ctxp))
                .unwrap();
            let _ = res.into_final_result(parsed.clone(), Default::default()).unwrap();
        };
        bench("distributed collect + finalise", &mut b);

        // 3. the single-node collector, which finalises inline
        let mut b = || {
            let ctxp = AggContextParams::new(Default::default(), g.index.tokenizers().clone());
            let _ = searcher
                .search(&AllQuery, &AggregationCollector::from_aggs(parsed.clone(), ctxp))
                .unwrap();
        };
        bench("single-node collect", &mut b);

        // 4. serialising the result the way the response does
        let ctxp = AggContextParams::new(Default::default(), g.index.tokenizers().clone());
        let out = searcher
            .search(&AllQuery, &AggregationCollector::from_aggs(parsed.clone(), ctxp))
            .unwrap();
        let mut b = || {
            let _ = serde_json::to_value(&out).unwrap();
        };
        bench("serialise result", &mut b);
    }
    Ok(())
}
