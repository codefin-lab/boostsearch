//! Transforms: a summary index kept from a source index by grouping its
//! documents and writing one document per group.
//!
//! A transform names the groups -- terms, histogram and date histogram
//! buckets -- and the aggregations to compute in each, and the documents it
//! writes carry the group keys and the aggregation results under the names
//! the transform gives them. A plain transform runs once over everything and
//! turns itself off; a continuous one runs on its schedule and recomputes
//! only the groups that documents written since its last run fall into.

use serde_json::{Value, json};

use super::jobs;
use crate::store::Store;

/// The aggregations a transform may compute.
const AGGREGATIONS: [&str; 7] =
    ["sum", "max", "min", "value_count", "avg", "scripted_metric", "percentiles"];

/// The fields a transform body may carry; anything else is refused by name.
const FIELDS: [&str; 17] = [
    "transform_id",
    "schema_version",
    "schedule",
    "metadata_id",
    "updated_at",
    "enabled",
    "enabled_at",
    "description",
    "source_index",
    "data_selection_query",
    "target_index",
    "roles",
    "page_size",
    "groups",
    "aggregations",
    "continuous",
    "user",
];

/// A refusal: the error type, the reason, and the status it is answered with.
pub type Refusal = (u16, String, String);

fn bad(reason: impl Into<String>) -> Refusal {
    (400, "illegal_argument_exception".into(), reason.into())
}

/// A group or a rollup dimension as it is written back, with its defaults.
pub fn parse_dimension(kind: &str, raw: &Value, what: &str) -> Result<Value, Refusal> {
    let o = raw.as_object().cloned().unwrap_or_default();
    let source = o.get("source_field").and_then(|v| v.as_str());
    let allowed: &[&str] = match kind {
        "terms" => &["source_field", "target_field"],
        "histogram" => &["source_field", "target_field", "interval"],
        "date_histogram" => &[
            "source_field",
            "target_field",
            "fixed_interval",
            "calendar_interval",
            "timezone",
            "format",
        ],
        other => return Err(bad(format!("Invalid dimension type [{other}] found in {what}"))),
    };
    if let Some(k) = o.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(bad(match kind {
            "terms" => format!("Invalid field [{k}] found in terms dimension."),
            "histogram" => format!("Invalid field [{k}] found in histogram dimension."),
            _ => format!("Invalid field [{k}] found in date histogram"),
        }));
    }
    let Some(source) = source else {
        return Err(bad(match kind {
            "terms" => "Source field cannot be null",
            _ => "Source field must not be null",
        }));
    };
    let target = o.get("target_field").and_then(|v| v.as_str()).unwrap_or(source);
    if source.is_empty() || target.is_empty() {
        return Err(bad("Source and target field must not be empty"));
    }
    Ok(match kind {
        "terms" => json!({"terms": {"source_field": source, "target_field": target}}),
        "histogram" => {
            let Some(interval) = o.get("interval").and_then(|v| v.as_f64()) else {
                return Err(bad("Interval field must not be null"));
            };
            if interval <= 0.0 {
                return Err(bad("Interval must be a positive decimal"));
            }
            json!({"histogram": {"source_field": source, "target_field": target,
                "interval": interval}})
        }
        _ => {
            let fixed = o.get("fixed_interval").and_then(|v| v.as_str());
            let calendar = o.get("calendar_interval").and_then(|v| v.as_str());
            let mut out = serde_json::Map::new();
            match (fixed, calendar) {
                (Some(_), Some(_)) => {
                    return Err(bad("Can only specify a fixed or calendar interval"));
                }
                (None, None) => return Err(bad("Must specify a fixed or calendar interval")),
                (Some(f), None) => {
                    out.insert("fixed_interval".into(), json!(f));
                }
                (None, Some(c)) => {
                    out.insert("calendar_interval".into(), json!(c));
                }
            }
            out.insert("source_field".into(), json!(source));
            out.insert("target_field".into(), json!(target));
            out.insert(
                "timezone".into(),
                json!(o.get("timezone").and_then(|v| v.as_str()).unwrap_or("UTC")),
            );
            out.insert("format".into(), o.get("format").cloned().unwrap_or(Value::Null));
            json!({"date_histogram": Value::Object(out)})
        }
    })
}

/// Whether an index's mapping has a field a dimension of this kind can group.
pub fn realizable(store: &Store, index: &str, kind: &str, field: &str) -> bool {
    let Some(ty) = jobs::field_type(store, index, field) else { return false };
    match kind {
        "terms" => ty != "text",
        "histogram" => matches!(
            ty.as_str(),
            "long"
                | "integer"
                | "short"
                | "byte"
                | "double"
                | "float"
                | "half_float"
                | "unsigned_long"
        ),
        _ => ty == "date",
    }
}

/// An aggregation as the plugin prints it back.
fn canonical_aggregation(name: &str, def: &Value) -> Result<Value, Refusal> {
    let Some((kind, body)) = def.as_object().and_then(|o| o.iter().next()) else {
        return Err(bad(format!("Unsupported aggregation [{name}]")));
    };
    if !AGGREGATIONS.contains(&kind.as_str()) {
        return Err(bad(format!("Unsupported aggregation [{kind}]")));
    }
    let mut body = body.clone();
    match kind.as_str() {
        "percentiles" => {
            let percents: Vec<f64> = body
                .get("percents")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|p| p.as_f64()).collect())
                .unwrap_or_else(|| vec![1.0, 5.0, 25.0, 50.0, 75.0, 95.0, 99.0]);
            body["percents"] = json!(percents);
            if body.get("keyed").is_none() {
                body["keyed"] = json!(true);
            }
            if body.get("hdr").is_none() && body.get("tdigest").is_none() {
                body["tdigest"] = json!({"compression": 100.0});
            }
        }
        "scripted_metric" => {
            for key in ["init_script", "map_script", "combine_script", "reduce_script"] {
                if let Some(Value::String(source)) = body.get(key).cloned() {
                    body[key] = json!({"source": source, "lang": "painless"});
                } else if let Some(obj) = body.get_mut(key).and_then(|v| v.as_object_mut()) {
                    obj.entry("lang").or_insert(json!("painless"));
                }
            }
        }
        _ => {}
    }
    Ok(json!({kind: body}))
}

/// A transform as the REST API receives it, checked and written out the way
/// the plugin stores it. `now` stands in for the times it has none of.
pub fn parse(id: &str, body: &Value, now: i64) -> Result<Value, Refusal> {
    let Some(raw) = body.get("transform").and_then(|t| t.as_object()) else {
        return Err((
            400,
            "parsing_exception".into(),
            "Failed to parse object: expecting token of type [FIELD_NAME] but found [END_OBJECT]"
                .into(),
        ));
    };
    if let Some(k) = raw.keys().find(|k| !FIELDS.contains(&k.as_str())) {
        return Err(bad(format!("Invalid field [{k}] found in Transforms.")));
    }
    let query = match raw.get("data_selection_query") {
        Some(q) => jobs::canonical_query(q).map_err(|(kind, reason)| (400, kind, reason))?,
        None => json!({"match_all": {"boost": 1.0}}),
    };
    let mut groups = Vec::new();
    for g in raw.get("groups").and_then(|v| v.as_array()).into_iter().flatten() {
        let Some((kind, inner)) = g.as_object().and_then(|o| o.iter().next()) else {
            return Err(bad("Dimension type cannot be null"));
        };
        groups.push(parse_dimension(kind, inner, "dimensions")?);
    }
    let mut aggregations = serde_json::Map::new();
    for (name, def) in raw.get("aggregations").and_then(|v| v.as_object()).into_iter().flatten() {
        aggregations.insert(name.clone(), canonical_aggregation(name, def)?);
    }
    let schedule = match raw.get("schedule") {
        Some(s) => jobs::parse_schedule(s, "Transform", None).map_err(|(k, r)| (400, k, r))?,
        None => return Err(bad("Transform schedule is null")),
    };
    let text = |key: &str| raw.get(key).and_then(|v| v.as_str()).map(String::from);
    let Some(description) = text("description") else {
        return Err(bad("Transform description is null"));
    };
    let Some(source) = text("source_index") else {
        return Err(bad("Transform source index is null"));
    };
    let Some(target) = text("target_index") else {
        return Err(bad("Transform target index is null"));
    };
    let Some(page_size) = raw.get("page_size").and_then(|v| v.as_i64()) else {
        return Err(bad("Transform page size is null"));
    };
    if jobs::interval_millis(&schedule).map(|ms| ms <= 0).unwrap_or(false) {
        return Err(bad("Transform job schedule interval must be greater than 0"));
    }
    if groups.is_empty() {
        return Err(bad("Groupings are Empty"));
    }
    if !(1..=10_000).contains(&page_size) {
        return Err(bad("Page size must be between 1 and 10,000"));
    }
    if source == target {
        return Err(bad("Source and target indices cannot be the same"));
    }
    let enabled = raw.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
    let enabled_at = if enabled {
        raw.get("enabled_at").filter(|v| !v.is_null()).cloned().unwrap_or(json!(now))
    } else {
        Value::Null
    };
    let mut out = json!({
        "transform_id": id,
        "schema_version": jobs::SCHEMA_VERSION,
        "schedule": schedule,
        "metadata_id": raw.get("metadata_id").cloned().unwrap_or(Value::Null),
        "updated_at": now,
        "enabled": enabled,
        "enabled_at": enabled_at,
        "description": description,
        "source_index": source,
        "data_selection_query": query,
        "target_index": target,
        "page_size": page_size,
        "groups": groups,
        "aggregations": aggregations,
        "continuous": raw.get("continuous").and_then(|v| v.as_bool()).unwrap_or(false),
    });
    if let Some(roles) = raw.get("roles").filter(|v| v.is_array()) {
        // kept for the jobs that still carry them, as the plugin keeps them;
        // they are not shown and do not decide anything
        out["roles"] = roles.clone();
    }
    Ok(out)
}

/// What the cluster says about a transform before it is stored or previewed:
/// the source has to be there, and each group has to name a field it can
/// group.
pub fn validate(store: &Store, t: &Value) -> Result<(), Refusal> {
    let source = t["source_index"].as_str().unwrap_or_default();
    let indices = store.resolve(source);
    if indices.is_empty() {
        return Err((
            404,
            "status_exception".into(),
            "No specified source index exist in the cluster".into(),
        ));
    }
    for index in &indices {
        for g in t["groups"].as_array().into_iter().flatten() {
            let Some((kind, inner)) = g.as_object().and_then(|o| o.iter().next()) else { continue };
            let field = inner["source_field"].as_str().unwrap_or_default();
            if !realizable(store, index, kind, field) {
                return Err((
                    400,
                    "status_exception".into(),
                    format!(
                        "Cannot find field [{field}] that can be grouped as [{kind}] in [{index}]."
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// The composite sources a transform's groups become.
fn sources_of(t: &Value) -> Value {
    let mut out = Vec::new();
    for g in t["groups"].as_array().into_iter().flatten() {
        let Some((kind, d)) = g.as_object().and_then(|o| o.iter().next()) else { continue };
        let target = d["target_field"].as_str().unwrap_or_default();
        let field = d["source_field"].clone();
        let source = match kind.as_str() {
            "terms" => json!({"terms": {"field": field, "missing_bucket": true}}),
            "histogram" => json!({"histogram": {"field": field, "interval": d["interval"],
                "missing_bucket": true}}),
            _ => {
                let mut s = json!({"field": field, "missing_bucket": true,
                    "time_zone": d["timezone"]});
                for key in ["fixed_interval", "calendar_interval"] {
                    if let Some(v) = d.get(key) {
                        s[key] = v.clone();
                    }
                }
                json!({"date_histogram": s})
            }
        };
        out.push(json!({target: source}));
    }
    Value::Array(out)
}

/// Every bucket of a transform's groups over a query, with the aggregations
/// computed in each when `with_aggs` says so.
///
/// A scripted metric is run bucket by bucket, over the bucket's own
/// documents: the composite aggregation here computes what it holds in each
/// bucket through the engine's aggregators, and a script is not one of them.
fn group_buckets(
    store: &Store,
    t: &Value,
    query: &Value,
    with_aggs: bool,
) -> Result<(Vec<Value>, u64), String> {
    let source = t["source_index"].as_str().unwrap_or_default();
    let mut plain = serde_json::Map::new();
    let mut scripted = serde_json::Map::new();
    if with_aggs {
        for (name, def) in t["aggregations"].as_object().into_iter().flatten() {
            if def.get("scripted_metric").is_some() {
                scripted.insert(name.clone(), def.clone());
            } else {
                plain.insert(name.clone(), def.clone());
            }
        }
    }
    let (mut buckets, mut took) =
        jobs::all_buckets(store, source, query, &sources_of(t), &Value::Object(plain))?;
    if !scripted.is_empty() {
        let started = std::time::Instant::now();
        let names: Vec<String> = scripted.keys().cloned().collect();
        let scripted = Value::Object(scripted);
        for b in buckets.iter_mut() {
            let body = json!({"size": 0, "aggs": scripted,
                "query": {"bool": {"must": [query], "filter": bucket_filter(t, &b["key"])}}});
            let answer = jobs::search(store, source, &body)?;
            for name in &names {
                b[name.as_str()] = answer["aggregations"][name.as_str()].clone();
            }
        }
        took += started.elapsed().as_millis() as u64;
    }
    // a date group asked for a format is written in it; the key was kept a
    // number until now so that each bucket's documents could be found by it
    for g in t["groups"].as_array().into_iter().flatten() {
        let Some(d) = g.get("date_histogram") else { continue };
        let Some(pattern) = d["format"].as_str() else { continue };
        let name = d["target_field"].as_str().unwrap_or_default();
        let zone = d["timezone"].as_str().unwrap_or("UTC");
        for b in buckets.iter_mut() {
            let Some(ms) = b["key"][name].as_i64() else { continue };
            let offset = crate::tz::offset_at(zone, ms.div_euclid(1000)).unwrap_or(0) as i64 * 1000;
            if let Some(text) = crate::store::format_millis_at(ms, pattern, offset) {
                b["key"][name] = json!(text);
            }
        }
    }
    Ok((buckets, took))
}

/// The documents one bucket holds, as a filter over its key.
fn bucket_filter(t: &Value, key: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    for g in t["groups"].as_array().into_iter().flatten() {
        let Some((kind, d)) = g.as_object().and_then(|o| o.iter().next()) else { continue };
        let field = d["source_field"].as_str().unwrap_or_default();
        let v = &key[d["target_field"].as_str().unwrap_or_default()];
        if v.is_null() {
            out.push(json!({"bool": {"must_not": [{"exists": {"field": field}}]}}));
            continue;
        }
        out.push(match kind.as_str() {
            "terms" => json!({"term": {field: v}}),
            "histogram" => {
                let from = v.as_f64().unwrap_or(0.0);
                let step = d["interval"].as_f64().unwrap_or(1.0);
                json!({"range": {field: {"gte": from, "lt": from + step}}})
            }
            _ => {
                let from = v.as_i64().unwrap_or(0);
                json!({"range": {field: {"gte": from, "lt": super::rollup::window_end(d, from),
                    "format": "epoch_millis"}}})
            }
        });
    }
    out
}

/// The names in the target index a transform writes dates under: a terms
/// group over a date field, and an aggregation over one. The plugin maps these
/// as dates when it makes the target index, because their values are the
/// milliseconds a date is counted in and would otherwise be mapped as numbers.
fn date_fields(store: &Store, t: &Value) -> Vec<String> {
    let source = t["source_index"].as_str().unwrap_or_default();
    let index = store.resolve(source).into_iter().next().unwrap_or_default();
    let is_date = |field: &str| {
        matches!(jobs::field_type(store, &index, field).as_deref(), Some("date" | "date_nanos"))
    };
    let mut out = Vec::new();
    for g in t["groups"].as_array().into_iter().flatten() {
        if let Some(d) = g.get("terms")
            && is_date(d["source_field"].as_str().unwrap_or_default())
        {
            out.push(d["target_field"].as_str().unwrap_or_default().to_string());
        }
    }
    for (name, def) in t["aggregations"].as_object().into_iter().flatten() {
        let field = def
            .as_object()
            .and_then(|o| o.values().next())
            .and_then(|b| b.get("field"))
            .and_then(|f| f.as_str());
        if field.map(is_date).unwrap_or(false) {
            out.push(name.clone());
        }
    }
    out
}

/// The document a bucket becomes: its key, its counts, and each aggregation's
/// result under the aggregation's name, with the id the plugin gives it.
fn bucket_doc(t: &Value, bucket: &Value, dates: &[String], mark: bool) -> (String, Value) {
    let id = t["transform_id"].as_str().unwrap_or_default();
    let mut doc = serde_json::Map::new();
    if mark {
        doc.insert("transform._id".into(), json!(id));
    }
    let count = bucket.get("doc_count").cloned().unwrap_or(json!(0));
    doc.insert("_doc_count".into(), count.clone());
    doc.insert("transform._doc_count".into(), count);
    let key = bucket.get("key").and_then(|k| k.as_object()).cloned().unwrap_or_default();
    let mut texts = Vec::new();
    // the id is hashed from the key in the order the groups are written
    for g in t["groups"].as_array().into_iter().flatten() {
        let Some(name) =
            g.as_object().and_then(|o| o.values().next()).and_then(|d| d["target_field"].as_str())
        else {
            continue;
        };
        let v = key.get(name).cloned().unwrap_or(Value::Null);
        texts.push(jobs::key_text(&v));
        doc.insert(name.to_string(), v);
    }
    for (name, def) in t["aggregations"].as_object().into_iter().flatten() {
        let kind = def.as_object().and_then(|o| o.keys().next()).map(|k| k.as_str()).unwrap_or("");
        let got = bucket.get(name).cloned().unwrap_or(Value::Null);
        doc.insert(name.clone(), aggregation_value(kind, &got, dates.contains(name)));
    }
    (jobs::hash_id(&format!("{id}#{}", texts.join(":"))), Value::Object(doc))
}

/// An aggregation's result the way the plugin reads it off a bucket: a single
/// value as a double -- or, for a minimum, maximum or average over a date, as
/// the whole milliseconds -- percentiles by their percent, and a scripted
/// metric as whatever its reduce returned.
fn aggregation_value(kind: &str, got: &Value, date: bool) -> Value {
    match kind {
        "percentiles" => {
            let mut out = serde_json::Map::new();
            let values = got.get("values").cloned().unwrap_or(json!({}));
            match values {
                Value::Object(o) => {
                    for (k, v) in o {
                        if k.ends_with("_as_string") {
                            continue;
                        }
                        let percent = k.parse::<f64>().map(jobs::java_double).unwrap_or(k);
                        out.insert(percent, jobs::double_value(v.as_f64().unwrap_or(f64::NAN)));
                    }
                }
                Value::Array(a) => {
                    for entry in a {
                        let percent =
                            entry["key"].as_f64().map(jobs::java_double).unwrap_or_default();
                        out.insert(
                            percent,
                            jobs::double_value(entry["value"].as_f64().unwrap_or(f64::NAN)),
                        );
                    }
                }
                _ => {}
            }
            Value::Object(out)
        }
        "scripted_metric" => got.get("value").cloned().unwrap_or(Value::Null),
        _ => {
            // an aggregation over no values has a value that is not a number:
            // nothing is summed to nothing, and nothing is the most or the least
            let value = got.get("value").and_then(|v| v.as_f64()).unwrap_or(match kind {
                "sum" | "value_count" => 0.0,
                "min" => f64::INFINITY,
                "max" => f64::NEG_INFINITY,
                _ => f64::NAN,
            });
            if date && matches!(kind, "min" | "max" | "avg") {
                // the JVM's conversion of a double to a long saturates
                let whole = if value.is_nan() {
                    0
                } else if value >= i64::MAX as f64 {
                    i64::MAX
                } else if value <= i64::MIN as f64 {
                    i64::MIN
                } else {
                    value as i64
                };
                json!(whole)
            } else {
                jobs::double_value(value)
            }
        }
    }
}

/// The preview of a transform: its first ten documents, without the mark that
/// says which transform wrote them.
pub fn preview(store: &Store, t: &Value) -> Result<Vec<Value>, String> {
    let dates = date_fields(store, t);
    let (buckets, _) = group_buckets(store, t, &t["data_selection_query"], true)?;
    Ok(buckets.iter().take(10).map(|b| bucket_doc(t, b, &dates, false).1).collect())
}

/// The target index a transform makes when it is not there.
fn ensure_target(store: &Store, t: &Value) -> Result<(), String> {
    let target = t["target_index"].as_str().unwrap_or_default();
    if store.get(target).is_some() || !store.resolve(target).is_empty() {
        return Ok(());
    }
    let mut mappings = json!({
        "_meta": {"schema_version": 1},
        "dynamic_templates": [{"strings": {"match_mapping_type": "string",
            "mapping": {"type": "keyword"}}}],
    });
    let dates = date_fields(store, t);
    if !dates.is_empty() {
        let props: serde_json::Map<String, Value> =
            dates.into_iter().map(|d| (d, json!({"type": "date"}))).collect();
        mappings["properties"] = Value::Object(props);
    }
    store
        .create(target, &json!({"mappings": mappings}))
        .map_err(|e| format!("Failed to create the target index {target}: {e}"))
}

// ------------------------------------------------------------------- metadata

fn zero_stats() -> Value {
    json!({"pages_processed": 0, "documents_processed": 0, "documents_indexed": 0,
        "index_time_in_millis": 0, "search_time_in_millis": 0})
}

fn add_stat(meta: &mut Value, key: &str, n: u64) {
    let now = meta["stats"][key].as_u64().unwrap_or(0);
    meta["stats"][key] = json!(now + n);
}

/// The checkpoint a source index's documents have reached: the highest
/// sequence number any write to it was given.
fn checkpoint(store: &Store, index: &str) -> i64 {
    store.get(index).map(|st| st.read().seq_no as i64 - 1).unwrap_or(-1)
}

/// The shard key a checkpoint is recorded under, the way the plugin writes a
/// shard id. This node keeps one sequence of writes per index, so every index
/// has the one.
fn shard_key(index: &str) -> String {
    format!("[{index}][0]")
}

/// Explain one transform: its metadata as it stands, and for a continuous one,
/// how many writes to its source it has not seen yet.
pub fn explain(store: &Store, id: &str) -> Value {
    let Some(job) = jobs::read(store, id).filter(|h| h.body.get("transform").is_some()) else {
        return Value::Null;
    };
    let t = &job.body["transform"];
    let metadata = jobs::metadata_of(store, "transform", id, t["metadata_id"].as_str());
    let Some(meta) = metadata else {
        return json!({"metadata_id": t["metadata_id"], "transform_metadata": Value::Null});
    };
    let mut m = meta.body["transform_metadata"].clone();
    let checkpoints = m.get("shard_id_to_global_checkpoint").cloned();
    if let Some(o) = m.as_object_mut() {
        o.remove("shard_id_to_global_checkpoint");
        o.remove("after_key");
    }
    if t["continuous"].as_bool() == Some(true) {
        let source = t["source_index"].as_str().unwrap_or_default();
        let mut behind = serde_json::Map::new();
        for index in store.resolve(source) {
            let held = checkpoints
                .as_ref()
                .and_then(|c| c.get(shard_key(&index)))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let n = behind.get(source).and_then(|v| v.as_i64()).unwrap_or(0);
            behind.insert(source.to_string(), json!(n + checkpoint(store, &index) - held));
        }
        let mut stats = serde_json::Map::new();
        if let Some(last) = m.pointer("/continuous_stats/last_timestamp").filter(|v| !v.is_null()) {
            stats.insert("last_timestamp".into(), last.clone());
        }
        stats.insert("documents_behind".into(), Value::Object(behind));
        m["continuous_stats"] = Value::Object(stats);
    }
    json!({"metadata_id": t["metadata_id"], "transform_metadata": m})
}

// ----------------------------------------------------------------- the runner

/// The state of one run, written to the metadata document as it goes.
struct Run<'a> {
    store: &'a Store,
    t: Value,
    user: Option<Value>,
    meta_id: String,
    meta: Value,
    dates: Vec<String>,
}

impl Run<'_> {
    fn save(&mut self) {
        self.meta["last_updated_at"] = json!(crate::store::now_millis());
        let _ = jobs::write(
            self.store,
            &self.meta_id,
            json!({"transform_metadata": self.meta.clone()}),
            false,
            None,
        );
    }

    fn fail(&mut self, reason: String) {
        self.meta["status"] = json!("failed");
        self.meta["failure_reason"] = json!(reason);
    }

    /// Every bucket over the transform's query, or over the documents written
    /// between two checkpoints.
    fn buckets(&self, since: Option<(i64, i64)>) -> Result<(Vec<Value>, u64), String> {
        let source = self.t["source_index"].as_str().unwrap_or_default();
        let query = match since {
            None => self.t["data_selection_query"].clone(),
            Some((from, to)) => json!({"bool": {"must": [self.t["data_selection_query"]],
                "filter": [{"range": {"_seq_no": {"gt": from, "lte": to}}}]}}),
        };
        if let Some(why) = jobs::permission_refusal(
            self.store,
            self.user.as_ref(),
            "indices:data/read/search",
            &self.store.resolve(source),
        ) {
            return Err(format!(
                "Failed to search data in source indices - missing required index permissions: {why}"
            ));
        }
        jobs::run_as(self.user.as_ref(), || {
            group_buckets(self.store, &self.t, &query, since.is_none())
        })
    }

    fn index(&self, docs: &[(String, Value)]) -> Result<u64, String> {
        let target = self.t["target_index"].as_str().unwrap_or_default().to_string();
        if let Some(why) = jobs::permission_refusal(
            self.store,
            self.user.as_ref(),
            "indices:data/write/index",
            std::slice::from_ref(&target),
        ) {
            return Err(format!(
                "Failed to index the documents - missing required index permissions: {why}"
            ));
        }
        jobs::index_docs(self.store, &target, docs)
            .map_err(|e| format!("Failed to index {} documents: {e}", docs.len()))
    }

    /// A plain transform: every bucket, a page at a time, then finished.
    fn everything(&mut self) -> Result<(), String> {
        let (buckets, took) = self.buckets(None)?;
        add_stat(&mut self.meta, "search_time_in_millis", took);
        let page = self.t["page_size"].as_u64().unwrap_or(1).max(1) as usize;
        for chunk in buckets.chunks(page) {
            let docs: Vec<(String, Value)> =
                chunk.iter().map(|b| bucket_doc(&self.t, b, &self.dates, true)).collect();
            let took = self.index(&docs)?;
            add_stat(&mut self.meta, "pages_processed", 1);
            add_stat(
                &mut self.meta,
                "documents_processed",
                chunk.iter().map(|b| b["doc_count"].as_u64().unwrap_or(0)).sum(),
            );
            add_stat(&mut self.meta, "documents_indexed", docs.len() as u64);
            add_stat(&mut self.meta, "index_time_in_millis", took);
            self.meta["status"] = json!("started");
            self.save();
        }
        // the page after the last full one comes back empty, and that is how
        // the plugin learns it has seen every bucket; it is counted
        add_stat(&mut self.meta, "pages_processed", 1);
        self.meta["status"] = json!("finished");
        Ok(())
    }

    /// A continuous transform: the groups the documents written since the
    /// last run fall into, and only those, recomputed over every document.
    fn changes(&mut self) -> Result<(), String> {
        let source = self.t["source_index"].as_str().unwrap_or_default().to_string();
        let checkpoints_at = crate::store::now_millis();
        let held =
            self.meta.get("shard_id_to_global_checkpoint").cloned().filter(|v| v.is_object());
        let page = self.t["page_size"].as_u64().unwrap_or(1).max(1) as usize;
        let mut reached = serde_json::Map::new();
        let mut processed: std::collections::HashSet<String> = Default::default();
        let mut whole: Option<Vec<Value>> = None;
        for index in self.store.resolve(&source) {
            let now = checkpoint(self.store, &index);
            let key = shard_key(&index);
            reached.insert(key.clone(), json!(now));
            let before = held.as_ref().and_then(|h| h.get(&key)).and_then(|v| v.as_i64());
            if before.map(|b| now <= b).unwrap_or(false) {
                continue;
            }
            let (changed, took) = self.buckets(Some((before.unwrap_or(-1), now)))?;
            add_stat(&mut self.meta, "search_time_in_millis", took);
            // every page of changed groups counts, and so does the empty page
            // after the last one that says there are no more; the groups
            // recomputed from a page are not a page of their own
            let pages: Vec<&[Value]> = changed.chunks(page).collect();
            add_stat(&mut self.meta, "pages_processed", pages.len() as u64 + 1);
            for chunk in pages {
                let fresh: Vec<String> = chunk
                    .iter()
                    .map(|b| b["key"].to_string())
                    .filter(|k| processed.insert(k.clone()))
                    .collect();
                if whole.is_none() && !fresh.is_empty() {
                    let (all, took) = self.buckets(None)?;
                    add_stat(&mut self.meta, "search_time_in_millis", took);
                    whole = Some(all);
                }
                let selected: Vec<&Value> = whole
                    .iter()
                    .flatten()
                    .filter(|b| fresh.contains(&b["key"].to_string()))
                    .collect();
                let docs: Vec<(String, Value)> =
                    selected.iter().map(|b| bucket_doc(&self.t, b, &self.dates, true)).collect();
                let took = self.index(&docs)?;
                add_stat(
                    &mut self.meta,
                    "documents_processed",
                    selected.iter().map(|b| b["doc_count"].as_u64().unwrap_or(0)).sum(),
                );
                add_stat(&mut self.meta, "documents_indexed", docs.len() as u64);
                add_stat(&mut self.meta, "index_time_in_millis", took);
                self.meta["status"] = json!("started");
                self.save();
            }
        }
        self.meta["shard_id_to_global_checkpoint"] = Value::Object(reached);
        self.meta["continuous_stats"] = json!({"last_timestamp": checkpoints_at});
        Ok(())
    }
}

/// Run one transform once, if it is enabled and has something to do.
pub fn run(store: &Store, id: &str) {
    let Some(job) = jobs::read(store, id) else { return };
    let t = job.body["transform"].clone();
    if t["enabled"].as_bool() != Some(true) {
        return;
    }
    let continuous = t["continuous"].as_bool() == Some(true);
    let user = t.get("user").filter(|u| !u.is_null()).cloned();
    // the metadata is made on the first run, and says `init` until the run
    // has done something
    let (meta_id, meta) = match jobs::metadata_of(store, "transform", id, t["metadata_id"].as_str())
    {
        Some(held) => (
            t["metadata_id"].as_str().map(String::from).unwrap_or_default(),
            held.body["transform_metadata"].clone(),
        ),
        None => {
            let now = crate::store::now_millis();
            let meta_id = jobs::hash_id(&format!("TransformMetadata#{id}#{now}"));
            let mut meta = json!({"transform_id": id, "last_updated_at": now, "status": "init",
                "failure_reason": Value::Null, "stats": zero_stats()});
            if continuous {
                meta["continuous_stats"] = json!({});
            }
            let _ = jobs::write(
                store,
                &meta_id,
                json!({"transform_metadata": meta.clone()}),
                false,
                None,
            );
            // the job is told which document is its metadata as soon as there
            // is one, so that explain finds it while the first run is going
            if let Some(mut now) = jobs::read(store, id) {
                now.body["transform"]["metadata_id"] = json!(meta_id);
                let _ = jobs::write(store, id, now.body, false, None);
            }
            (meta_id, meta)
        }
    };
    let status = meta["status"].as_str().unwrap_or("init").to_string();
    if status == "stopped" || status == "finished" && !continuous {
        return;
    }
    let mut run = Run {
        store,
        dates: date_fields(store, &t),
        t: t.clone(),
        user,
        meta_id: meta_id.clone(),
        meta,
    };
    run.meta["failure_reason"] = Value::Null;
    let outcome = validate(store, &t)
        .map_err(|(_, _, reason)| format!("Failed validation - [{reason}]"))
        .and_then(|_| ensure_target(store, &t))
        .and_then(|_| if continuous { run.changes() } else { run.everything() });
    if let Err(reason) = outcome {
        run.fail(reason);
    }
    jobs::refresh(store, t["target_index"].as_str().unwrap_or_default());
    run.save();
    let failed = run.meta["status"] == "failed";
    // a plain transform is done once it has run, and a failed one stays off
    // until somebody starts it again
    if let Some(mut now) = jobs::read(store, id) {
        let t = &mut now.body["transform"];
        let turn_off = (!continuous || failed) && t["enabled"].as_bool() == Some(true);
        if turn_off || t["metadata_id"].is_null() {
            t["metadata_id"] = json!(meta_id);
            if turn_off {
                t["enabled"] = json!(false);
                t["enabled_at"] = Value::Null;
                t["updated_at"] = json!(crate::store::now_millis());
            }
            let _ = jobs::write(store, id, now.body, false, None);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A store holding `src` with these documents, each written and searchable.
    pub(crate) fn store_with(docs: &[Value]) -> Store {
        let store = Store::scratch();
        store
            .create(
                "src",
                &json!({"mappings": {"properties": {"k": {"type": "keyword"},
                    "t": {"type": "date"}, "v": {"type": "double"}}}}),
            )
            .unwrap();
        let st = store.get("src").unwrap();
        let mut g = st.write();
        for (i, d) in docs.iter().enumerate() {
            let written = crate::api::write_doc_internal(
                &mut g,
                &i.to_string(),
                d.clone(),
                "index",
                None,
                None,
            );
            assert!(written.is_ok());
        }
        g.refresh().unwrap();
        drop(g);
        store
    }

    #[test]
    fn a_transform_writes_one_document_per_group_and_turns_itself_off() {
        let store = store_with(&[
            json!({"k": "a", "t": "2025-01-01T10:00:00Z", "v": 1.0}),
            json!({"k": "a", "t": "2025-01-01T11:00:00Z", "v": 2.0}),
            json!({"k": "b", "t": "2025-01-02T11:00:00Z", "v": 3.0}),
            json!({"t": "2025-01-03T00:00:00Z", "v": 4.0}),
        ]);
        let t = parse(
            "t",
            &json!({"transform": {"schedule": {"interval": {"period": 1, "unit": "Minutes"}},
                "description": "d", "source_index": "src", "target_index": "dst", "page_size": 2,
                "groups": [{"terms": {"source_field": "k"}}],
                "aggregations": {"s": {"sum": {"field": "v"}}}}}),
            0,
        )
        .unwrap();
        assert!(jobs::write(&store, "t", json!({"transform": t}), true, None).is_ok());
        run(&store, "t");
        let explained = explain(&store, "t");
        let m = &explained["transform_metadata"];
        assert_eq!(m["status"], json!("finished"));
        // two full pages of groups and the empty one after them
        assert_eq!(m["stats"]["pages_processed"], json!(3));
        assert_eq!(m["stats"]["documents_processed"], json!(4));
        assert_eq!(m["stats"]["documents_indexed"], json!(3));
        assert_eq!(jobs::read(&store, "t").unwrap().body["transform"]["enabled"], json!(false));
        let answer = jobs::search(&store, "dst", &json!({"size": 10, "sort": ["_id"]})).unwrap();
        let docs: Vec<(String, Value)> = answer["hits"]["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| (h["_id"].as_str().unwrap().to_string(), h["_source"].clone()))
            .collect();
        assert_eq!(docs.len(), 3);
        // the documents without a key are a group of their own
        let missing = docs.iter().find(|(_, d)| d["k"].is_null()).unwrap();
        assert_eq!(missing.0, jobs::hash_id("t##ODFE-MAGIC-NULL-MAGIC-ODFE#"));
        assert_eq!(missing.1["s"], json!(4.0));
        let a = docs.iter().find(|(_, d)| d["k"] == json!("a")).unwrap();
        assert_eq!(a.0, jobs::hash_id("t#a"));
        assert_eq!(a.1["_doc_count"], json!(2));
    }

    #[test]
    fn a_continuous_transform_recomputes_only_the_groups_that_changed() {
        let store = store_with(&[json!({"k": "a", "v": 1.0}), json!({"k": "b", "v": 2.0})]);
        let t = parse(
            "c",
            &json!({"transform": {"schedule": {"interval": {"period": 1, "unit": "Minutes"}},
                "description": "d", "source_index": "src", "target_index": "dst", "page_size": 10,
                "continuous": true, "groups": [{"terms": {"source_field": "k"}}],
                "aggregations": {"s": {"sum": {"field": "v"}}}}}),
            0,
        )
        .unwrap();
        assert!(jobs::write(&store, "c", json!({"transform": t}), true, None).is_ok());
        run(&store, "c");
        let first = explain(&store, "c");
        assert_eq!(first["transform_metadata"]["stats"]["documents_indexed"], json!(2));
        assert_eq!(
            first["transform_metadata"]["continuous_stats"]["documents_behind"]["src"],
            json!(0)
        );
        {
            let st = store.get("src").unwrap();
            let mut g = st.write();
            let more = json!({"k": "a", "v": 5.0});
            assert!(crate::api::write_doc_internal(&mut g, "9", more, "index", None, None).is_ok());
            g.refresh().unwrap();
        }
        assert_eq!(
            explain(&store, "c")["transform_metadata"]["continuous_stats"]["documents_behind"]["src"],
            json!(1)
        );
        run(&store, "c");
        let second = explain(&store, "c");
        let stats = &second["transform_metadata"]["stats"];
        // one changed group: it alone is written again, from all its documents
        assert_eq!(stats["documents_indexed"], json!(3));
        assert_eq!(stats["documents_processed"], json!(4));
        let answer =
            jobs::search(&store, "dst", &json!({"size": 10, "query": {"term": {"k": "a"}}}))
                .unwrap();
        assert_eq!(answer["hits"]["hits"][0]["_source"]["s"], json!(6.0));
    }

    #[test]
    fn a_transform_is_written_back_with_its_defaults() {
        let t = parse(
            "t",
            &json!({"transform": {"schedule": {"interval": {"period": 1, "unit": "minutes",
                "start_time": 5}}, "description": "d", "source_index": "a", "target_index": "b",
                "page_size": 10, "groups": [{"terms": {"source_field": "k"}},
                {"histogram": {"source_field": "n", "interval": 5}},
                {"date_histogram": {"source_field": "t", "calendar_interval": "1M"}}],
                "aggregations": {"p": {"percentiles": {"field": "n", "percents": [50]}}}}}),
            7,
        )
        .unwrap();
        assert_eq!(
            t["schedule"],
            json!({"interval": {"start_time": 5, "period": 1, "unit": "Minutes"}})
        );
        assert_eq!(t["enabled_at"], json!(7));
        assert_eq!(t["groups"][0], json!({"terms": {"source_field": "k", "target_field": "k"}}));
        assert_eq!(t["groups"][1]["histogram"]["interval"], json!(5.0));
        assert_eq!(t["groups"][2]["date_histogram"]["timezone"], json!("UTC"));
        assert_eq!(
            t["aggregations"]["p"],
            json!({"percentiles": {"field": "n", "percents": [50.0], "keyed": true,
                "tdigest": {"compression": 100.0}}})
        );
        assert_eq!(t["data_selection_query"], json!({"match_all": {"boost": 1.0}}));
    }

    #[test]
    fn refusals_name_what_is_wrong() {
        let base = json!({"transform": {"schedule": {"interval": {"period": 1, "unit": "Minutes"}},
            "description": "d", "source_index": "a", "target_index": "b", "page_size": 10,
            "groups": [{"terms": {"source_field": "k"}}], "aggregations": {}}});
        let refused = |edit: &dyn Fn(&mut Value)| {
            let mut b = base.clone();
            edit(&mut b);
            parse("t", &b, 0).unwrap_err().2
        };
        assert_eq!(
            refused(&|b| b["transform"]["bogus"] = json!(1)),
            "Invalid field [bogus] found in Transforms."
        );
        assert_eq!(refused(&|b| b["transform"]["groups"] = json!([])), "Groupings are Empty");
        assert_eq!(
            refused(&|b| b["transform"]["target_index"] = json!("a")),
            "Source and target indices cannot be the same"
        );
        assert_eq!(
            refused(&|b| b["transform"]["page_size"] = json!(10001)),
            "Page size must be between 1 and 10,000"
        );
        assert_eq!(
            refused(&|b| b["transform"]["aggregations"] = json!({"x": {"terms": {"field": "k"}}})),
            "Unsupported aggregation [terms]"
        );
    }

    #[test]
    fn a_bucket_becomes_the_plugins_document() {
        let t = json!({"transform_id": "tfix-d", "groups": [
            {"terms": {"target_field": "k"}}, {"terms": {"target_field": "tt"}},
            {"date_histogram": {"target_field": "day"}}, {"histogram": {"target_field": "n"}}],
            "aggregations": {
            "mn": {"min": {"field": "t"}}, "mx": {"max": {"field": "v"}},
            "av": {"avg": {"field": "v"}}, "vc": {"value_count": {"field": "v"}}}});
        let bucket = json!({"key": {"k": null, "tt": 1735862400000_i64, "day": "2025-01-02", "n": 10.0},
            "doc_count": 1, "mn": {"value": 1735862400000.0}, "mx": {"value": null},
            "av": {"value": null}, "vc": {"value": 0}});
        let (id, doc) = bucket_doc(&t, &bucket, &["mn".to_string()], true);
        assert_eq!(id, "JtAiM_iwh2S1F4kPelwDIQ");
        assert_eq!(doc["mn"], json!(1735862400000_i64));
        assert_eq!(doc["mx"], json!("-Infinity"));
        assert_eq!(doc["av"], json!("NaN"));
        assert_eq!(doc["vc"], json!(0.0));
        assert_eq!(doc["transform._doc_count"], json!(1));
    }
}
