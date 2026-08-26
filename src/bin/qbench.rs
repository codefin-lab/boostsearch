//! Engine CPU per query, measured in-process so HTTP and the client are out of
//! the picture. This is the number the execution work has to move.
use obsearch::store::Store;
use serde_json::json;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let data = std::env::var("OBSEARCH_DATA").unwrap_or("/tmp/startup-data".into());
    let store = Store::on_disk(&data)?;
    let index = std::env::args().nth(1).unwrap_or("bench_logs".into());
    let n: usize = std::env::var("ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(50);

    let queries: Vec<(&str, serde_json::Value)> = vec![
        ("match_all", json!({"query": {"match_all": {}}, "size": 10})),
        ("term_keyword", json!({"query": {"term": {"region": "eu-west-1"}}, "size": 10})),
        ("term_numeric", json!({"query": {"term": {"status": 404}}, "size": 10})),
        ("range_numeric", json!({"query": {"range": {"size": {"gte": 5000, "lt": 50000}}}, "size": 10})),
        ("match_text", json!({"query": {"match": {"request": "api orders"}}, "size": 10})),
        ("bool_filter", json!({"query": {"bool": {
            "must": [{"match": {"request": "api"}}],
            "filter": [{"term": {"method": "GET"}}, {"range": {"response_ms": {"gte": 20}}}]}}, "size": 10})),
        ("agg_terms", json!({"size": 0, "aggs": {"by_status": {"terms": {"field": "status", "size": 10}}}})),
        ("agg_date_hist", json!({"size": 0, "aggs": {"over_time": {
            "date_histogram": {"field": "@timestamp", "fixed_interval": "1d"}}}})),
        ("agg_nested", json!({"size": 0, "aggs": {"by_region": {"terms": {"field": "region"},
            "aggs": {"avg_ms": {"avg": {"field": "response_ms"}}, "p_size": {"stats": {"field": "size"}}}}}})),
        ("sort_paged", json!({"query": {"match_all": {}}, "sort": [{"size": "desc"}], "size": 10, "from": 100})),
        ("count_only", json!({"query": {"match_all": {}}, "size": 0})),
    ];

    let params = std::collections::HashMap::new();
    println!("{:<16}{:>12}{:>14}", "query", "us/query", "total hits");
    let mut total = 0.0;
    for (name, body) in &queries {
        // warm up before timing
        for _ in 0..5 {
            let _ = obsearch::search::run(&store, &index, body, &params);
        }
        let t = Instant::now();
        let mut hits = 0;
        for _ in 0..n {
            match obsearch::search::run(&store, &index, body, &params) {
                Ok(o) => hits = o.total,
                Err(_) => {
                    println!("{name:<16}{:>12}", "ERROR");
                    break;
                }
            }
        }
        let us = t.elapsed().as_secs_f64() * 1e6 / n as f64;
        total += us;
        println!("{name:<16}{us:>12.0}{hits:>14}");
    }
    println!("{:<16}{total:>12.0}  (sum)", "");
    Ok(())
}
