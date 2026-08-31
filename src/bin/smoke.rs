use boostcore::schema::*;
use boostcore::{Index, TantivyDocument, collector::TopDocs};
use boostcore::query::{TermQuery, RangeQuery};
use boostcore::aggregation::agg_req::Aggregations;
use boostcore::aggregation::AggregationCollector;
use std::ops::Bound;

fn main() -> boostcore::Result<()> {
    let mut sb = Schema::builder();
    let dyn_opts = JsonObjectOptions::default()
        .set_stored()
        .set_fast(None)
        .set_expand_dots_enabled()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("default")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        );
    let raw_opts = JsonObjectOptions::default()
        .set_fast(Some("raw"))
        .set_expand_dots_enabled()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("raw")
                .set_index_option(IndexRecordOption::Basic),
        );
    let f_dyn = sb.add_json_field("_dyn", dyn_opts);
    let f_raw = sb.add_json_field("_raw", raw_opts);
    let schema = sb.build();
    let index = Index::create_in_ram(schema.clone());
    let mut w = index.writer(50_000_000)?;

    for (i, (name, cnt)) in [("hello world", 1i64), ("goodbye world", 5), ("hello there", 10)].iter().enumerate() {
        let v = serde_json::json!({"title": name, "count": cnt, "tag": "a-b"});
        let obj: std::collections::BTreeMap<String, boostcore::schema::OwnedValue> = v.as_object().unwrap()
            .iter().map(|(k, x)| (k.clone(), boostcore::schema::OwnedValue::from(x.clone()))).collect();
        let mut d = TantivyDocument::default();
        d.add_object(f_dyn, obj.clone());
        d.add_object(f_raw, obj);
        w.add_document(d)?;
        let _ = i;
    }
    w.commit()?;
    let reader = index.reader()?;
    let s = reader.searcher();

    // 1. match query on analyzed json path
    let t = Term::from_field_json_path(f_dyn, "title", true);
    let mut t2 = t.clone(); t2.append_type_and_str("hello");
    let q = TermQuery::new(t2, IndexRecordOption::Basic);
    println!("match title:hello -> {}", s.search(&q, &TopDocs::with_limit(10).order_by_score())?.len());

    // 2. term query on raw (keyword) path
    let mut t3 = Term::from_field_json_path(f_raw, "tag", true);
    t3.append_type_and_str("a-b");
    let q3 = TermQuery::new(t3, IndexRecordOption::Basic);
    println!("term tag:a-b -> {}", s.search(&q3, &TopDocs::with_limit(10).order_by_score())?.len());

    // 3. range query on numeric json path
    let mut lo = Term::from_field_json_path(f_dyn, "count", true);
    lo.append_type_and_fast_value(4i64);
    let mut hi = Term::from_field_json_path(f_dyn, "count", true);
    hi.append_type_and_fast_value(100i64);
    let rq = RangeQuery::new(Bound::Included(lo), Bound::Included(hi));
    println!("range count>=4 -> {}", s.search(&rq, &TopDocs::with_limit(10).order_by_score())?.len());

    // 4. aggregation on json fast path
    let agg: Aggregations = serde_json::from_value(serde_json::json!({
        "tags": {"terms": {"field": "_raw.tag"}},
        "avg_count": {"avg": {"field": "_dyn.count"}}
    })).unwrap();
    let res = s.search(&boostcore::query::AllQuery, &AggregationCollector::from_aggs(agg, Default::default()))?;
    println!("aggs -> {}", serde_json::to_string(&res).unwrap());
    Ok(())
}
