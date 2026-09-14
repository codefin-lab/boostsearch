//! Rollups: an index of pre-aggregated documents that searches of the raw
//! data can be answered from.
//!
//! A rollup job buckets its source by one date histogram and any number of
//! terms and histogram dimensions, and keeps per bucket the metrics it was
//! asked for -- sum, minimum, maximum, a value count, and an average kept as
//! its sum and its count so that averages can be combined again. The index it
//! writes says in its mapping which jobs wrote it, and a search of that index
//! is rewritten against those documents (see `rollup_search`).

use serde_json::{Value, json};

use super::jobs;
use crate::store::Store;

/// The metrics a rollup may keep.
const METRICS: [&str; 5] = ["sum", "avg", "min", "max", "value_count"];

const FIELDS: [&str; 17] = [
    "rollup_id",
    "enabled",
    "schedule",
    "last_updated_time",
    "enabled_time",
    "description",
    "schema_version",
    "source_index",
    "target_index",
    "metadata_id",
    "page_size",
    "delay",
    "continuous",
    "dimensions",
    "metrics",
    "roles",
    "user",
];

use super::transform::Refusal;

fn bad(reason: impl Into<String>) -> Refusal {
    (400, "illegal_argument_exception".into(), reason.into())
}

/// A rollup as the REST API receives it, checked and written out the way the
/// plugin stores it.
pub fn parse(id: &str, body: &Value, now: i64) -> Result<Value, Refusal> {
    let Some(raw) = body.get("rollup").and_then(|t| t.as_object()) else {
        return Err((
            400,
            "parsing_exception".into(),
            "Failed to parse object: expecting token of type [FIELD_NAME] but found [END_OBJECT]"
                .into(),
        ));
    };
    if let Some(k) = raw.keys().find(|k| !FIELDS.contains(&k.as_str())) {
        return Err(bad(format!("Invalid field [{k}] found in Rollup.")));
    }
    let mut dimensions = Vec::new();
    for d in raw.get("dimensions").and_then(|v| v.as_array()).into_iter().flatten() {
        let Some((kind, inner)) = d.as_object().and_then(|o| o.iter().next()) else {
            return Err(bad("Dimension type cannot be null"));
        };
        dimensions.push(super::transform::parse_dimension(kind, inner, "dimensions")?);
    }
    let mut metrics = Vec::new();
    for m in raw.get("metrics").and_then(|v| v.as_array()).into_iter().flatten() {
        let Some(source) = m.get("source_field").and_then(|v| v.as_str()) else {
            return Err(bad("Source field must not be null"));
        };
        let mut kinds: Vec<Value> = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        for one in m.get("metrics").and_then(|v| v.as_array()).into_iter().flatten() {
            let Some(kind) = one.as_object().and_then(|o| o.keys().next()) else {
                return Err(bad("Metric is null"));
            };
            if !METRICS.contains(&kind.as_str()) {
                return Err(bad(format!("Invalid metric type [{kind}] found in metrics")));
            }
            seen.push(kind.clone());
            kinds.push(json!({kind: {}}));
        }
        let mut distinct = seen.clone();
        distinct.sort();
        distinct.dedup();
        if distinct.len() != seen.len() {
            // the plugin names the metrics the way its classes print themselves
            let printed: Vec<&str> = seen
                .iter()
                .map(|k| match k.as_str() {
                    "sum" => "Sum()",
                    "avg" => "Average()",
                    "min" => "Min()",
                    "max" => "Max()",
                    _ => "ValueCount()",
                })
                .collect();
            return Err(bad(format!(
                "Cannot have multiple metrics of the same type in a single rollup metric [[{}]]",
                printed.join(", ")
            )));
        }
        if kinds.is_empty() {
            return Err(bad(format!(
                "Must specify at least one metric to aggregate on for {source}"
            )));
        }
        let target = m.get("target_field").and_then(|v| v.as_str()).unwrap_or(source);
        let mut entry = json!({"source_field": source, "metrics": kinds});
        if target != source {
            entry["target_field"] = json!(target);
        }
        metrics.push(entry);
    }
    let delay = raw.get("delay").and_then(|v| v.as_i64());
    let schedule = match raw.get("schedule") {
        Some(s) => jobs::parse_schedule(s, "Rollup", Some(delay.unwrap_or(0)))
            .map_err(|(k, r)| (400, k, r))?,
        None => return Err(bad("Rollup schedule is null")),
    };
    let text = |key: &str| raw.get(key).and_then(|v| v.as_str()).map(String::from);
    let Some(description) = text("description") else {
        return Err(bad("Rollup description is null"));
    };
    let Some(source) = text("source_index") else {
        return Err(bad("Rollup source index is null"));
    };
    let Some(target) = text("target_index") else {
        return Err(bad("Rollup target index is null"));
    };
    let Some(page_size) = raw.get("page_size").and_then(|v| v.as_i64()) else {
        return Err(bad("Rollup page size is null"));
    };
    if source == target {
        return Err(bad("Your source and target index cannot be the same"));
    }
    let histograms = dimensions.iter().filter(|d| d.get("date_histogram").is_some()).count();
    if histograms != 1 {
        return Err(bad("Must specify precisely one date histogram dimension"));
    }
    if dimensions.first().and_then(|d| d.get("date_histogram")).is_none() {
        return Err(bad("The first dimension must be a date histogram"));
    }
    if !(1..=10_000).contains(&page_size) {
        return Err(bad("Page size must be between 1 and 10,000"));
    }
    if let Some(d) = delay {
        if d < 0 {
            return Err(bad("Delay must be non-negative if set"));
        }
        if d > now {
            return Err(bad("Delay must be less than the current unix time"));
        }
    }
    if jobs::interval_millis(&schedule).map(|ms| ms <= 0).unwrap_or(false) {
        return Err(bad("Rollup job schedule interval must be greater than 0"));
    }
    let enabled = raw.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
    let mut out = json!({
        "rollup_id": id,
        "enabled": enabled,
        "schedule": schedule,
        "last_updated_time": now,
        "enabled_time": if enabled { json!(now) } else { Value::Null },
        "description": description,
        "schema_version": jobs::SCHEMA_VERSION,
        "source_index": source,
        "target_index": target,
        "metadata_id": raw.get("metadata_id").cloned().unwrap_or(Value::Null),
        "page_size": page_size,
        "delay": delay,
        "continuous": raw.get("continuous").and_then(|v| v.as_bool()).unwrap_or(false),
        "dimensions": dimensions,
        "metrics": metrics,
    });
    if let Some(roles) = raw.get("roles").filter(|v| v.is_array()) {
        out["roles"] = roles.clone();
    }
    Ok(out)
}

/// The name a dimension's value is written under in the rollup index.
pub fn dimension_field(d: &Value) -> Option<(String, String, String)> {
    let (kind, inner) = d.as_object()?.iter().next()?;
    let source = inner["source_field"].as_str()?.to_string();
    let target = inner["target_field"].as_str().unwrap_or(&source).to_string();
    Some((kind.clone(), source, format!("{target}.{kind}")))
}

/// The composite sources and metric aggregations a job's search is made of.
fn search_parts(r: &Value) -> (Value, Value) {
    let mut sources = Vec::new();
    for d in r["dimensions"].as_array().into_iter().flatten() {
        let Some((kind, source, name)) = dimension_field(d) else { continue };
        let inner = &d[kind.as_str()];
        let spec = match kind.as_str() {
            "terms" => json!({"terms": {"field": source, "missing_bucket": true}}),
            "histogram" => json!({"histogram": {"field": source, "interval": inner["interval"],
                "missing_bucket": true}}),
            _ => {
                let mut s = json!({"field": source, "missing_bucket": true,
                    "time_zone": inner["timezone"]});
                for key in ["fixed_interval", "calendar_interval"] {
                    if let Some(v) = inner.get(key) {
                        s[key] = v.clone();
                    }
                }
                json!({"date_histogram": s})
            }
        };
        sources.push(json!({name: spec}));
    }
    let mut aggs = serde_json::Map::new();
    for m in r["metrics"].as_array().into_iter().flatten() {
        let source = m["source_field"].as_str().unwrap_or_default();
        let target = m["target_field"].as_str().unwrap_or(source);
        for one in m["metrics"].as_array().into_iter().flatten() {
            let Some(kind) = one.as_object().and_then(|o| o.keys().next()) else { continue };
            if kind == "avg" {
                aggs.insert(format!("{target}.avg.sum"), json!({"sum": {"field": source}}));
                aggs.insert(
                    format!("{target}.avg.value_count"),
                    json!({"value_count": {"field": source}}),
                );
            } else {
                aggs.insert(format!("{target}.{kind}"), json!({kind: {"field": source}}));
            }
        }
    }
    (Value::Array(sources), Value::Object(aggs))
}

/// The document a bucket becomes, with the id the plugin gives it.
fn bucket_doc(r: &Value, bucket: &Value) -> (String, Value) {
    let id = r["rollup_id"].as_str().unwrap_or_default();
    let mut doc = serde_json::Map::new();
    doc.insert("rollup._id".into(), json!(id));
    doc.insert("_doc_count".into(), bucket.get("doc_count").cloned().unwrap_or(json!(0)));
    doc.insert("rollup._schema_version".into(), json!(jobs::SCHEMA_VERSION));
    let mut texts = Vec::new();
    let key = bucket.get("key").and_then(|k| k.as_object()).cloned().unwrap_or_default();
    for d in r["dimensions"].as_array().into_iter().flatten() {
        let Some((_, _, name)) = dimension_field(d) else { continue };
        let v = key.get(&name).cloned().unwrap_or(Value::Null);
        texts.push(jobs::key_text(&v));
        doc.insert(name, v);
    }
    let (_, aggs) = search_parts(r);
    for (name, def) in aggs.as_object().into_iter().flatten() {
        let kind = def.as_object().and_then(|o| o.keys().next()).map(|k| k.as_str()).unwrap_or("");
        let got = bucket.get(name).and_then(|a| a.get("value")).and_then(|v| v.as_f64());
        let value = match kind {
            "value_count" => json!(got.unwrap_or(0.0) as i64),
            "sum" => jobs::double_value(got.unwrap_or(0.0)),
            // a minimum or maximum of nothing is written as nothing
            _ => got.filter(|v| v.is_finite()).map(|v| json!(v)).unwrap_or(Value::Null),
        };
        doc.insert(name.clone(), value);
    }
    (jobs::hash_id(&format!("{id}#{}", texts.join("#"))), Value::Object(doc))
}

/// Whether an index is a rollup index: made by a rollup job, or told it is one.
pub fn is_rollup_index(store: &Store, index: &str) -> bool {
    let Some(st) = store.get(index) else { return false };
    let g = st.read();
    let flag = g
        .settings
        .pointer("/index/plugins/rollup_index")
        .or_else(|| g.settings.get("index.plugins.rollup_index"))
        .or_else(|| g.settings.pointer("/index/opendistro/rollup_index"));
    matches!(flag, Some(Value::Bool(true))) || flag.and_then(|v| v.as_str()) == Some("true")
}

/// The jobs a rollup index's mapping says wrote it.
pub fn jobs_of_index(store: &Store, index: &str) -> Vec<Value> {
    let Some(st) = store.get(index) else { return Vec::new() };
    let g = st.read();
    g.mapping
        .raw
        .pointer("/_meta/rollups")
        .and_then(|r| r.as_object())
        .map(|o| o.values().cloned().collect())
        .unwrap_or_default()
}

/// The rollup index a job writes into, made where it is not there and told of
/// the job where it is.
fn ensure_target(store: &Store, r: &Value) -> Result<(), String> {
    let target = r["target_index"].as_str().unwrap_or_default();
    let id = r["rollup_id"].as_str().unwrap_or_default();
    let mut job = r.clone();
    if let Some(o) = job.as_object_mut() {
        o.insert("user".into(), r.get("user").cloned().unwrap_or(Value::Null));
    }
    if store.get(target).is_none() {
        let mappings = json!({
            "_meta": {"rollups": {id: job}},
            "dynamic_templates": [
                {"strings": {"match_mapping_type": "string", "mapping": {"type": "keyword"}}},
                {"date_histograms": {"path_match": "*.date_histogram",
                    "mapping": {"type": "date"}}},
                {"cardinality_sketches": {"path_match": "*.hll",
                    "mapping": {"type": "hll", "doc_values": true}}},
            ],
        });
        return store
            .create(
                target,
                &json!({"settings": {"index": {"plugins": {"rollup_index": true}}},
                    "mappings": mappings}),
            )
            .map_err(|e| format!("Failed to create target index [{target}]: {e}"));
    }
    if !is_rollup_index(store, target) {
        return Err(format!("Target index [{target}] is a non rollup index"));
    }
    let Some(st) = store.get(target) else { return Ok(()) };
    let mut g = st.write();
    if g.mapping.raw.pointer(&format!("/_meta/rollups/{id}")).is_none() {
        g.mapping.merge(&json!({"_meta": {"rollups": {id: job}}}));
    }
    Ok(())
}

/// What a job cannot be run over: a dimension or a metric whose field the
/// source does not have in a form it can be rolled up by.
fn source_issues(store: &Store, r: &Value) -> Option<String> {
    let source = r["source_index"].as_str().unwrap_or_default();
    let indices = store.resolve(source);
    let Some(index) = indices.first() else {
        return Some(format!("No indices found for [{source}]"));
    };
    let mut issues = Vec::new();
    for d in r["dimensions"].as_array().into_iter().flatten() {
        let Some((kind, field, _)) = dimension_field(d) else { continue };
        // a field that is not there and a field of a kind the dimension
        // cannot group are the same complaint
        if !super::transform::realizable(store, index, &kind, &field) {
            issues.push(format!("missing field {field}"));
        }
    }
    for m in r["metrics"].as_array().into_iter().flatten() {
        let field = m["source_field"].as_str().unwrap_or_default();
        if jobs::field_type(store, index, field).is_none() {
            issues.push(format!("missing field {field}"));
        }
    }
    (!issues.is_empty())
        .then(|| format!("Invalid mappings for index [{index}] because [{}]", issues.join(", ")))
}

/// Where a window of time begins, for the job's date histogram.
fn window_floor(hist: &Value, at: i64) -> i64 {
    let zone = hist["timezone"].as_str().unwrap_or("UTC");
    let offset = |t: i64| crate::tz::offset_at(zone, t.div_euclid(1000)).unwrap_or(0) as i64 * 1000;
    if let Some(calendar) = hist["calendar_interval"].as_str()
        && let Some(unit) = crate::search::CalendarUnit::parse(calendar)
    {
        let local = at + offset(at);
        let floored = unit.floor(to_datetime(local));
        let local_floor = floored.unix_timestamp_nanos() as i64 / 1_000_000;
        return local_floor - offset(local_floor);
    }
    let step = interval_ms(hist).max(1);
    let local = at + offset(at);
    local.div_euclid(step) * step - offset(at)
}

/// Where the window beginning at `start` ends.
pub(crate) fn window_end(hist: &Value, start: i64) -> i64 {
    let zone = hist["timezone"].as_str().unwrap_or("UTC");
    let offset = |t: i64| crate::tz::offset_at(zone, t.div_euclid(1000)).unwrap_or(0) as i64 * 1000;
    if let Some(calendar) = hist["calendar_interval"].as_str()
        && let Some(unit) = crate::search::CalendarUnit::parse(calendar)
    {
        let local = start + offset(start);
        let next = unit.advance(to_datetime(local));
        let local_next = next.unix_timestamp_nanos() as i64 / 1_000_000;
        return local_next - offset(local_next);
    }
    start + interval_ms(hist).max(1)
}

fn interval_ms(hist: &Value) -> i64 {
    hist["fixed_interval"]
        .as_str()
        .or_else(|| hist["calendar_interval"].as_str())
        .and_then(crate::search::aggs::parse_offset)
        .map(|d| d.whole_milliseconds() as i64)
        .unwrap_or(86_400_000)
}

fn to_datetime(ms: i64) -> boostcore::time::OffsetDateTime {
    boostcore::time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000)
        .unwrap_or(boostcore::time::OffsetDateTime::UNIX_EPOCH)
}

fn zero_stats() -> Value {
    json!({"pages_processed": 0, "documents_processed": 0, "rollups_indexed": 0,
        "index_time_in_millis": 0, "search_time_in_millis": 0})
}

fn add_stat(meta: &mut Value, key: &str, n: u64) {
    let now = meta["stats"][key].as_u64().unwrap_or(0);
    meta["stats"][key] = json!(now + n);
}

/// Explain one rollup: its metadata as it stands.
pub fn explain(store: &Store, id: &str) -> Value {
    let Some(job) = jobs::read(store, id).filter(|h| h.body.get("rollup").is_some()) else {
        return Value::Null;
    };
    let r = &job.body["rollup"];
    match jobs::metadata_of(store, "rollup", id, r["metadata_id"].as_str()) {
        Some(meta) => {
            let mut m = meta.body["rollup_metadata"].clone();
            if m.get("after_key").map(|v| v.is_null()).unwrap_or(false)
                && let Some(o) = m.as_object_mut()
            {
                o.remove("after_key");
            }
            json!({"metadata_id": r["metadata_id"], "rollup_metadata": m})
        }
        None => json!({"metadata_id": r["metadata_id"], "rollup_metadata": Value::Null}),
    }
}

/// Run one rollup job once, if it is enabled and has something to do.
pub fn run(store: &Store, id: &str) {
    let Some(job) = jobs::read(store, id) else { return };
    let mut r = job.body["rollup"].clone();
    if r["enabled"].as_bool() != Some(true) {
        return;
    }
    let continuous = r["continuous"].as_bool() == Some(true);
    let user = r.get("user").filter(|u| !u.is_null()).cloned();
    let source = r["source_index"].as_str().unwrap_or_default().to_string();
    let target = r["target_index"].as_str().unwrap_or_default().to_string();
    let hist = r["dimensions"][0]["date_histogram"].clone();
    let hist_field = hist["source_field"].as_str().unwrap_or_default().to_string();

    let (meta_id, mut meta) =
        match jobs::metadata_of(store, "rollup", id, r["metadata_id"].as_str()) {
            Some(held) => (
                r["metadata_id"].as_str().map(String::from).unwrap_or_default(),
                held.body["rollup_metadata"].clone(),
            ),
            None => {
                let now = crate::store::now_millis();
                let mut meta = json!({"rollup_id": id, "last_updated_time": now});
                if continuous {
                    // a continuous job starts at the window its earliest document
                    // falls in; with no documents there is nothing to start from,
                    // and the job waits for some
                    let earliest = jobs::run_as(user.as_ref(), || {
                        jobs::search(
                            store,
                            &source,
                            &json!({"size": 0,
                        "aggs": {"earliest": {"min": {"field": hist_field}}}}),
                        )
                    });
                    let Some(first) = earliest.ok().and_then(|a| {
                        a.pointer("/aggregations/earliest/value").and_then(|v| v.as_f64())
                    }) else {
                        return;
                    };
                    let start = window_floor(&hist, first as i64);
                    meta["continuous"] = json!({"next_window_start_time": start,
                    "next_window_end_time": window_end(&hist, start)});
                }
                meta["status"] = json!("init");
                meta["failure_reason"] = Value::Null;
                meta["stats"] = zero_stats();
                let meta_id = jobs::new_metadata_id(id);
                let _ = jobs::write(
                    store,
                    &meta_id,
                    json!({"rollup_metadata": meta.clone()}),
                    false,
                    None,
                );
                if let Some(mut now) = jobs::read(store, id) {
                    now.body["rollup"]["metadata_id"] = json!(meta_id);
                    r["metadata_id"] = json!(meta_id);
                    let _ = jobs::write(store, id, now.body, false, None);
                }
                (meta_id, meta)
            }
        };
    let status = meta["status"].as_str().unwrap_or("init").to_string();
    if matches!(status.as_str(), "finished" | "stopped") {
        return;
    }
    let save = |meta: &mut Value| {
        meta["last_updated_time"] = json!(crate::store::now_millis());
        let _ = jobs::write(store, &meta_id, json!({"rollup_metadata": meta.clone()}), false, None);
    };
    let outcome: Result<(), String> = (|| {
        if status == "failed" {
            return Err(meta["failure_reason"].as_str().unwrap_or("Failed to rollup").to_string());
        }
        if let Some(issue) = source_issues(store, &r) {
            return Err(issue);
        }
        ensure_target(store, &r)?;
        let (sources, aggs) = search_parts(&r);
        let page = r["page_size"].as_u64().unwrap_or(1).max(1) as usize;
        let delay = r["delay"].as_i64().unwrap_or(0);
        let one_window = |meta: &mut Value, query: Value| -> Result<(), String> {
            if let Some(why) = jobs::permission_refusal(
                store,
                user.as_ref(),
                "indices:data/read/search",
                &store.resolve(&source),
            ) {
                return Err(format!(
                    "Cannot search data in source index/s - missing required index permissions: {why}"
                ));
            }
            let (buckets, took) = jobs::run_as(user.as_ref(), || {
                jobs::all_buckets(store, &source, &query, &sources, &aggs)
            })?;
            add_stat(meta, "search_time_in_millis", took);
            for chunk in buckets.chunks(page) {
                let docs: Vec<(String, Value)> = chunk.iter().map(|b| bucket_doc(&r, b)).collect();
                if let Some(why) = jobs::permission_refusal(
                    store,
                    user.as_ref(),
                    "indices:data/write/index",
                    std::slice::from_ref(&target),
                ) {
                    return Err(format!("Failed to index {} documents: {why}", docs.len()));
                }
                let took = jobs::index_docs(store, &target, &docs)?;
                add_stat(meta, "pages_processed", 1);
                add_stat(
                    meta,
                    "documents_processed",
                    chunk.iter().map(|b| b["doc_count"].as_u64().unwrap_or(0)).sum(),
                );
                add_stat(meta, "rollups_indexed", docs.len() as u64);
                add_stat(meta, "index_time_in_millis", took);
            }
            // the empty page that tells the job the window is done
            add_stat(meta, "pages_processed", 1);
            Ok(())
        };
        if !continuous {
            one_window(&mut meta, json!({"match_all": {}}))?;
            meta["status"] = json!("finished");
            return Ok(());
        }
        loop {
            let start = meta.pointer("/continuous/next_window_start_time").and_then(|v| v.as_i64());
            let end = meta.pointer("/continuous/next_window_end_time").and_then(|v| v.as_i64());
            let (Some(start), Some(end)) = (start, end) else { break };
            if end - delay > crate::store::now_millis() {
                break;
            }
            let query = json!({"range": {hist_field.clone(): {"gte": start, "lt": end,
                "format": "epoch_millis"}}});
            one_window(&mut meta, query)?;
            meta["continuous"] = json!({"next_window_start_time": end,
                "next_window_end_time": window_end(&hist, end)});
            meta["status"] = json!("started");
            save(&mut meta);
        }
        Ok(())
    })();
    let failed = outcome.is_err();
    if let Err(reason) = outcome {
        meta["status"] = json!("failed");
        meta["failure_reason"] = json!(reason);
    }
    jobs::refresh(store, &target);
    save(&mut meta);
    if (failed || !continuous)
        && let Some(mut now) = jobs::read(store, id)
        && now.body["rollup"]["enabled"].as_bool() == Some(true)
    {
        now.body["rollup"]["enabled"] = json!(false);
        now.body["rollup"]["enabled_time"] = Value::Null;
        let _ = jobs::write(store, id, now.body, false, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rollup_index_answers_for_the_raw_documents() {
        let store = super::super::transform::tests::store_with(&[
            json!({"k": "a", "t": "2025-01-01T10:00:00Z", "v": 1.0}),
            json!({"k": "a", "t": "2025-01-01T11:00:00Z", "v": 2.0}),
            json!({"k": "b", "t": "2025-01-02T11:00:00Z", "v": 6.0}),
        ]);
        let r = parse(
            "r",
            &json!({"rollup": {"schedule": {"interval": {"period": 1, "unit": "Minutes"}},
                "description": "d", "source_index": "src", "target_index": "dst", "page_size": 10,
                "dimensions": [{"date_histogram": {"source_field": "t", "fixed_interval": "1d"}},
                    {"terms": {"source_field": "k"}}],
                "metrics": [{"source_field": "v", "metrics": [{"avg": {}}, {"value_count": {}}]}]}}),
            0,
        )
        .unwrap();
        assert!(jobs::write(&store, "r", json!({"rollup": r}), true, None).is_ok());
        run(&store, "r");
        let m = &explain(&store, "r")["rollup_metadata"];
        assert_eq!(m["status"], json!("finished"));
        assert_eq!(m["stats"]["rollups_indexed"], json!(2));
        assert!(is_rollup_index(&store, "dst"));
        let asked = json!({"size": 0, "aggs": {"k": {"terms": {"field": "k"},
            "aggs": {"a": {"avg": {"field": "v"}}, "c": {"value_count": {"field": "v"}}}}}});
        let targets = vec!["dst".to_string()];
        let Some(super::super::rollup_search::Intercepted::Rewritten { body, plan }) =
            super::super::rollup_search::intercept(&store, &targets, &asked, &Default::default())
        else {
            panic!("a search of a rollup index is rewritten");
        };
        let mut answer = jobs::search(&store, "dst", &body).unwrap();
        super::super::rollup_search::finish(&mut answer, &plan);
        let a = &answer["aggregations"]["k"]["buckets"][0];
        assert_eq!(a["key"], json!("a"));
        // counted by the documents the rollup stands for, not by its own
        assert_eq!(a["doc_count"], json!(2));
        assert_eq!(a["a"], json!({"value": 1.5}));
        assert_eq!(a["c"], json!({"value": 2}));
        let refused = super::super::rollup_search::intercept(
            &store,
            &targets,
            &json!({"size": 0, "aggs": {"x": {"sum": {"field": "nosuch"}}}}),
            &Default::default(),
        );
        assert!(matches!(
            refused,
            Some(super::super::rollup_search::Intercepted::Refused { reason, .. })
                if reason == "Could not find a rollup job that can answer this query because [missing field nosuch]"
        ));
    }

    fn job() -> Value {
        parse(
            "tfix-r1",
            &json!({"rollup": {"schedule": {"interval": {"period": 1, "unit": "Minutes",
                "start_time": 1}}, "description": "d", "source_index": "a", "target_index": "b",
                "page_size": 100, "dimensions": [
                    {"date_histogram": {"source_field": "sold_at", "fixed_interval": "1d"}},
                    {"terms": {"source_field": "region"}},
                    {"histogram": {"source_field": "quantity", "interval": 5}}],
                "metrics": [{"source_field": "revenue", "metrics": [{"sum": {}}, {"avg": {}}]}]}}),
            9,
        )
        .unwrap()
    }

    #[test]
    fn a_rollup_is_written_back_with_its_defaults() {
        let r = job();
        assert_eq!(r["schedule"]["interval"]["schedule_delay"], json!(0));
        assert_eq!(r["delay"], Value::Null);
        assert_eq!(
            r["dimensions"][1],
            json!({"terms": {"source_field": "region", "target_field": "region"}})
        );
        assert_eq!(r["enabled_time"], json!(9));
    }

    #[test]
    fn a_bucket_becomes_the_plugins_document() {
        let bucket = json!({"key": {"sold_at.date_histogram": 1735689600000_i64,
            "region.terms": "east", "quantity.histogram": 0.0}, "doc_count": 1,
            "revenue.sum": {"value": 230.12}, "revenue.avg.sum": {"value": 230.12},
            "revenue.avg.value_count": {"value": 1}});
        let (id, doc) = bucket_doc(&job(), &bucket);
        assert_eq!(id, "yls1zhr02AyVdcao4JRyLQ");
        assert_eq!(doc["revenue.avg.value_count"], json!(1));
        assert_eq!(doc["rollup._schema_version"], json!(30));
    }

    #[test]
    fn rollups_need_one_date_histogram_first() {
        let refusal = parse(
            "x",
            &json!({"rollup": {"schedule": {"interval": {"period": 1, "unit": "Minutes"}},
                "description": "d", "source_index": "a", "target_index": "b", "page_size": 10,
                "dimensions": [{"terms": {"source_field": "k"}}], "metrics": []}}),
            0,
        )
        .unwrap_err();
        assert_eq!(refusal.2, "Must specify precisely one date histogram dimension");
    }

    #[test]
    fn windows_follow_the_calendar() {
        let month = json!({"calendar_interval": "1M", "timezone": "UTC"});
        // 2025-02-14T10:00Z falls in February
        let start = window_floor(&month, 1_739_527_200_000);
        assert_eq!(start, 1_738_368_000_000);
        assert_eq!(window_end(&month, start), 1_740_787_200_000);
        let day = json!({"fixed_interval": "1d", "timezone": "UTC"});
        assert_eq!(window_floor(&day, 1_739_527_200_000), 1_739_491_200_000);
    }
}
