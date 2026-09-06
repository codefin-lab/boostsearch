//! A visualization's changes of shape, 6.7.2 to 7.10.0.
//!
//! Ports of `src/plugins/visualizations/server/saved_objects/visualization_migrations.ts`.
//! Most of them read `visState` -- the JSON a visualization keeps its
//! configuration in -- change one thing in it, and write it back; a `visState`
//! that does not parse is left as it is, every time, because that is what the
//! originals do.

use serde_json::{Value, json};

use super::{
    Step, at, attributes, migrate_index_pattern, migrate_match_all_query, parsed, references,
    set_json,
};

pub const CHAIN: &[Step] = &[
    Step {
        version: "6.7.2",
        apply: |d| remove_date_histogram_time_zones(migrate_match_all_query(d)),
    },
    Step {
        version: "7.0.0",
        apply: |d| {
            migrate_table_splits(migrate_controls(migrate_saved_search(migrate_index_pattern_(
                add_doc_references(d),
            ))))
        },
    },
    Step { version: "7.0.1", apply: remove_date_histogram_time_zones },
    Step {
        version: "7.2.0",
        apply: |d| migrate_date_histogram_aggregation(migrate_percentile_rank_aggregation(d)),
    },
    Step {
        version: "7.3.0",
        apply: |d| {
            replace_mov_avg_to_mov_fn(migrate_filters_agg_query(
                transform_filter_string_to_query_object(migrate_gauge_vertical_split_to_alignment(
                    d,
                )),
            ))
        },
    },
    Step { version: "7.3.1", apply: migrate_filters_agg_query_string_queries },
    Step { version: "7.4.2", apply: transform_split_filters_string_to_query_object },
    Step { version: "7.7.0", apply: |d| migrate_split_by_chart_row(migrate_operator_key_typo(d)) },
    Step { version: "7.8.0", apply: migrate_tsvb_default_color_palettes },
    Step { version: "7.9.3", apply: migrate_match_all_query },
    Step { version: "7.10.0", apply: |d| remove_tsvb_search_source(migrate_filter_ratio_query(d)) },
];

/// `visState`, parsed, or nothing where it is missing or not JSON.
fn vis_state(doc: &Value) -> Option<Value> {
    parsed(doc, "attributes.visState")
}

fn write_vis_state(doc: &mut Value, state: &Value) {
    set_json(doc, "attributes.visState", state);
}

/// Whether a state is a TSVB visualization, which keeps its series under
/// `params` rather than in `aggs`.
fn is_metrics(state: &Value) -> bool {
    state.get("type").and_then(|v| v.as_str()) == Some("metrics")
}

fn series_of(state: &mut Value) -> Option<&mut Vec<Value>> {
    state.get_mut("params")?.get_mut("series")?.as_array_mut()
}

fn add_doc_references(mut doc: Value) -> Value {
    references(&mut doc);
    doc
}

fn migrate_index_pattern_(mut doc: Value) -> Value {
    migrate_index_pattern(&mut doc);
    doc
}

fn migrate_saved_search(mut doc: Value) -> Value {
    let saved = at(&doc, "attributes.savedSearchId").cloned().filter(|v| !v.is_null());
    if let Some(id) = saved
        && doc.get("references").is_some_and(|v| v.is_array())
    {
        references(&mut doc).push(json!({"type": "search", "name": "search_0", "id": id}));
        attributes(&mut doc).insert("savedSearchRefName".into(), json!("search_0"));
    }
    attributes(&mut doc).remove("savedSearchId");
    doc
}

fn migrate_controls(mut doc: Value) -> Value {
    let Some(mut state) = vis_state(&doc) else { return doc };
    let has_references = doc.get("references").is_some_and(|v| v.is_array());
    let mut found = Vec::new();
    if let Some(controls) = state.pointer_mut("/params/controls").and_then(|v| v.as_array_mut()) {
        for (i, control) in controls.iter_mut().enumerate() {
            let Some(pattern) = control.get("indexPattern").cloned().filter(|v| !v.is_null())
            else {
                continue;
            };
            if !has_references {
                continue;
            }
            let name = format!("control_{i}_index_pattern");
            control["indexPatternRefName"] = json!(name);
            control.as_object_mut().map(|o| o.remove("indexPattern"));
            found.push(json!({"name": name, "type": "index-pattern", "id": pattern}));
        }
    }
    if has_references {
        references(&mut doc).extend(found);
    }
    write_vis_state(&mut doc, &state);
    doc
}

fn migrate_table_splits(doc: Value) -> Value {
    let Some(mut state) = vis_state(&doc) else { return doc };
    if state.get("type").and_then(|v| v.as_str()) != Some("table") {
        return doc;
    }
    let mut splits = 0;
    if let Some(aggs) = state.get_mut("aggs").and_then(|v| v.as_array_mut()) {
        for agg in aggs.iter_mut() {
            if agg.get("schema").and_then(|v| v.as_str()) != Some("split") {
                continue;
            }
            splits += 1;
            if splits == 1 {
                continue;
            }
            agg["schema"] = json!("bucket");
            if let Some(params) = agg.get_mut("params").and_then(|v| v.as_object_mut()) {
                params.remove("row");
            }
        }
    }
    if splits <= 1 {
        return doc;
    }
    let mut out = doc;
    write_vis_state(&mut out, &state);
    out
}

fn remove_date_histogram_time_zones(mut doc: Value) -> Value {
    let Some(mut state) = vis_state(&doc) else { return doc };
    let Some(aggs) = state.get_mut("aggs").and_then(|v| v.as_array_mut()) else { return doc };
    for agg in aggs.iter_mut() {
        if agg.get("type").and_then(|v| v.as_str()) == Some("date_histogram")
            && let Some(params) = agg.get_mut("params").and_then(|v| v.as_object_mut())
        {
            params.remove("time_zone");
        }
        if agg.pointer("/params/customBucket/type").and_then(|v| v.as_str())
            == Some("date_histogram")
            && let Some(params) =
                agg.pointer_mut("/params/customBucket/params").and_then(|v| v.as_object_mut())
        {
            params.remove("time_zone");
        }
    }
    write_vis_state(&mut doc, &state);
    doc
}

fn migrate_percentile_rank_aggregation(mut doc: Value) -> Value {
    let Some(mut state) = vis_state(&doc) else { return doc };
    if !is_metrics(&state) {
        return doc;
    }
    if let Some(series) = series_of(&mut state) {
        for part in series.iter_mut() {
            if let Some(metrics) = part.get_mut("metrics").and_then(|v| v.as_array_mut()) {
                for metric in metrics.iter_mut() {
                    if metric.get("type").and_then(|v| v.as_str()) == Some("percentile_rank")
                        && let Some(value) = metric.get("value").cloned()
                    {
                        metric["values"] = json!([value]);
                        metric.as_object_mut().map(|o| o.remove("value"));
                    }
                }
            }
        }
    }
    write_vis_state(&mut doc, &state);
    doc
}

fn migrate_filter_ratio_query(mut doc: Value) -> Value {
    let Some(mut state) = vis_state(&doc) else { return doc };
    if !is_metrics(&state) {
        return doc;
    }
    if let Some(series) = series_of(&mut state) {
        for part in series.iter_mut() {
            if let Some(metrics) = part.get_mut("metrics").and_then(|v| v.as_array_mut()) {
                for metric in metrics.iter_mut() {
                    if metric.get("type").and_then(|v| v.as_str()) != Some("filter_ratio") {
                        continue;
                    }
                    for key in ["numerator", "denominator"] {
                        if let Some(text) =
                            metric.get(key).and_then(|v| v.as_str()).map(String::from)
                        {
                            metric[key] = json!({"query": text, "language": "lucene"});
                        }
                    }
                }
            }
        }
    }
    write_vis_state(&mut doc, &state);
    doc
}

fn migrate_operator_key_typo(mut doc: Value) -> Value {
    let Some(mut state) = vis_state(&doc) else { return doc };
    if !is_metrics(&state) {
        return doc;
    }
    if let Some(rules) =
        state.pointer_mut("/params/gauge_color_rules").and_then(|v| v.as_array_mut())
    {
        for rule in rules.iter_mut() {
            if let Some(o) = rule.as_object_mut() {
                o.remove("opperator");
            }
        }
    }
    write_vis_state(&mut doc, &state);
    doc
}

fn migrate_split_by_chart_row(mut doc: Value) -> Value {
    let Some(mut state) = vis_state(&doc) else { return doc };
    if state.get("aggs").is_none() || state.get("params").is_none() {
        return doc;
    }
    let mut row = None;
    if let Some(aggs) = state.get_mut("aggs").and_then(|v| v.as_array_mut()) {
        for agg in aggs.iter_mut() {
            let is_split = agg.get("type").and_then(|v| v.as_str()) == Some("terms")
                && agg.get("schema").and_then(|v| v.as_str()) == Some("split");
            if is_split
                && let Some(params) = agg.get_mut("params").and_then(|v| v.as_object_mut())
                && let Some(found) = params.remove("row")
            {
                row = Some(found);
            }
        }
    }
    if let Some(row) = row {
        state["params"]["row"] = row;
    }
    write_vis_state(&mut doc, &state);
    doc
}

fn migrate_date_histogram_aggregation(mut doc: Value) -> Value {
    let Some(mut state) = vis_state(&doc) else { return doc };
    let Some(aggs) = state.get_mut("aggs").and_then(|v| v.as_array_mut()) else { return doc };
    for agg in aggs.iter_mut() {
        if agg.get("type").and_then(|v| v.as_str()) == Some("date_histogram")
            && let Some(params) = agg.get_mut("params").and_then(|v| v.as_object_mut())
        {
            if params.get("interval").and_then(|v| v.as_str()) == Some("custom") {
                let custom = params.get("customInterval").cloned().unwrap_or(Value::Null);
                params.insert("interval".into(), custom);
            }
            params.remove("customInterval");
        }
        if agg.pointer("/params/customBucket/type").and_then(|v| v.as_str())
            == Some("date_histogram")
            && let Some(params) =
                agg.pointer_mut("/params/customBucket/params").and_then(|v| v.as_object_mut())
        {
            if params.get("interval").and_then(|v| v.as_str()) == Some("custom") {
                let custom = params.get("customInterval").cloned().unwrap_or(Value::Null);
                params.insert("interval".into(), custom);
            }
            params.remove("customInterval");
        }
    }
    write_vis_state(&mut doc, &state);
    doc
}

fn migrate_gauge_vertical_split_to_alignment(mut doc: Value) -> Value {
    let Some(mut state) = vis_state(&doc) else { return doc };
    if state.get("type").and_then(|v| v.as_str()) != Some("gauge") {
        return doc;
    }
    // the original reads `params.gauge.alignment` and throws (then logs and
    // returns the document) if `params.gauge` is not there
    let Some(gauge) = state.pointer_mut("/params/gauge") else { return doc };
    if gauge.get("alignment").is_some_and(|v| !v.is_null()) {
        return doc;
    }
    let vertical = gauge.get("verticalSplit").and_then(|v| v.as_bool()).unwrap_or(false);
    gauge["alignment"] = json!(if vertical { "vertical" } else { "horizontal" });
    gauge.as_object_mut().map(|o| o.remove("verticalSplit"));
    write_vis_state(&mut doc, &state);
    doc
}

const TSVB_TYPES: &[&str] = &["metric", "markdown", "top_n", "gauge", "table", "timeseries"];

fn is_tsvb(state: &Value) -> bool {
    state.pointer("/params/type").and_then(|v| v.as_str()).is_some_and(|t| TSVB_TYPES.contains(&t))
}

fn as_lucene(value: &mut Value) {
    if let Some(text) = value.as_str().map(String::from) {
        *value = json!({"query": text, "language": "lucene"});
    }
}

fn transform_filter_string_to_query_object(mut doc: Value) -> Value {
    let Some(mut state) = vis_state(&doc) else { return doc };
    if !is_tsvb(&state) {
        return doc;
    }
    if let Some(filter) = state.pointer_mut("/params/filter") {
        as_lucene(filter);
    }
    if let Some(annotations) =
        state.pointer_mut("/params/annotations").and_then(|v| v.as_array_mut())
    {
        for item in annotations.iter_mut() {
            if let Some(q) = item.get_mut("query_string").filter(|v| !v.is_null()) {
                as_lucene(q);
            }
        }
    }
    if let Some(series) = series_of(&mut state) {
        for item in series.iter_mut() {
            let Some(filter) = item
                .get_mut("filter")
                .filter(|v| !v.is_null() && !v.as_str().is_some_and(str::is_empty))
            else {
                continue;
            };
            as_lucene(filter);
            if let Some(splits) = item.get_mut("split_filters").and_then(|v| v.as_array_mut()) {
                for split in splits.iter_mut() {
                    if let Some(f) = split.get_mut("filter").filter(|v| !v.is_null()) {
                        as_lucene(f);
                    }
                }
            }
        }
    }
    write_vis_state(&mut doc, &state);
    doc
}

fn transform_split_filters_string_to_query_object(mut doc: Value) -> Value {
    let Some(mut state) = vis_state(&doc) else { return doc };
    if !is_tsvb(&state) {
        return doc;
    }
    if let Some(series) = series_of(&mut state) {
        for item in series.iter_mut() {
            if let Some(splits) = item.get_mut("split_filters").and_then(|v| v.as_array_mut()) {
                for split in splits.iter_mut() {
                    if let Some(f) = split.get_mut("filter") {
                        as_lucene(f);
                    }
                }
            }
        }
    }
    write_vis_state(&mut doc, &state);
    doc
}

fn migrate_filters_agg_query(mut doc: Value) -> Value {
    let Some(mut state) = vis_state(&doc) else { return doc };
    let Some(aggs) = state.get_mut("aggs").and_then(|v| v.as_array_mut()) else { return doc };
    for agg in aggs.iter_mut() {
        if agg.get("type").and_then(|v| v.as_str()) != Some("filters") {
            continue;
        }
        // the original reaches into `params.filters` without looking and
        // throws if it is not there, which is caught and returns the document
        let Some(filters) = agg.pointer_mut("/params/filters").and_then(|v| v.as_array_mut())
        else {
            return doc;
        };
        for filter in filters.iter_mut() {
            let Some(input) = filter.get_mut("input") else { return doc };
            if input.get("language").is_some_and(|v| !v.is_null()) {
                continue;
            }
            input["language"] = json!("lucene");
        }
    }
    write_vis_state(&mut doc, &state);
    doc
}

fn migrate_filters_agg_query_string_queries(mut doc: Value) -> Value {
    let Some(mut state) = vis_state(&doc) else { return doc };
    let Some(aggs) = state.get_mut("aggs").and_then(|v| v.as_array_mut()) else { return doc };
    for agg in aggs.iter_mut() {
        if agg.get("type").and_then(|v| v.as_str()) != Some("filters") {
            continue;
        }
        let Some(filters) = agg.pointer_mut("/params/filters").and_then(|v| v.as_array_mut())
        else {
            return doc;
        };
        for filter in filters.iter_mut() {
            let Some(text) = filter.pointer("/input/query/query_string/query").cloned() else {
                continue;
            };
            filter["input"]["query"] = text;
        }
    }
    write_vis_state(&mut doc, &state);
    doc
}

fn replace_mov_avg_to_mov_fn(mut doc: Value) -> Value {
    let Some(mut state) = vis_state(&doc) else { return doc };
    if !is_metrics(&state) {
        return doc;
    }
    if let Some(series) = series_of(&mut state) {
        for part in series.iter_mut() {
            let Some(metrics) = part.get_mut("metrics").and_then(|v| v.as_array_mut()) else {
                continue;
            };
            for metric in metrics.iter_mut() {
                if metric.get("type").and_then(|v| v.as_str()) != Some("moving_average") {
                    continue;
                }
                let settings = metric.get("settings").cloned().unwrap_or(Value::Null);
                let number = |key: &str, fallback: f64| {
                    settings.get(key).and_then(|v| v.as_f64()).unwrap_or(fallback)
                };
                metric["model_type"] = metric.get("model").cloned().unwrap_or(Value::Null);
                metric["alpha"] = json!(number("alpha", 0.3));
                metric["beta"] = json!(number("beta", 0.1));
                metric["gamma"] = json!(number("gamma", 0.3));
                metric["period"] = json!(number("period", 1.0));
                metric["multiplicative"] =
                    json!(settings.get("type").and_then(|v| v.as_str()) == Some("mult"));
                if let Some(o) = metric.as_object_mut() {
                    for key in ["minimize", "model", "settings", "predict"] {
                        o.remove(key);
                    }
                }
            }
        }
    }
    write_vis_state(&mut doc, &state);
    doc
}

fn migrate_tsvb_default_color_palettes(mut doc: Value) -> Value {
    let Some(mut state) = vis_state(&doc) else { return doc };
    if !is_metrics(&state) {
        return doc;
    }
    if let Some(series) = series_of(&mut state) {
        for part in series.iter_mut() {
            if !part.get("split_color_mode").is_some_and(|v| !v.is_null() && v != "") {
                part["split_color_mode"] = json!("gradient");
            }
        }
    }
    write_vis_state(&mut doc, &state);
    doc
}

fn remove_tsvb_search_source(mut doc: Value) -> Value {
    let Some(state) = vis_state(&doc) else { return doc };
    if !is_metrics(&state) {
        return doc;
    }
    let source =
        at(&doc, "attributes.kibanaSavedObjectMeta.searchSourceJSON").and_then(|v| v.as_str());
    if source == Some("{}") {
        return doc;
    }
    let mut meta =
        at(&doc, "attributes.kibanaSavedObjectMeta").cloned().unwrap_or_else(|| json!({}));
    meta["searchSourceJSON"] = json!("{}");
    super::set(&mut doc, "attributes.kibanaSavedObjectMeta", meta);
    doc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::migrations::migrate;

    fn with_state(state: Value) -> Value {
        json!({"id": "v", "type": "visualization", "references": [], "migrationVersion": {},
               "attributes": {"title": "t", "visState": state.to_string(),
                              "kibanaSavedObjectMeta": {"searchSourceJSON": "{}"}}})
    }

    fn state_of(doc: &Value) -> Value {
        serde_json::from_str(doc["attributes"]["visState"].as_str().unwrap()).unwrap()
    }

    #[test]
    fn the_archived_visualization_comes_up_to_date() {
        // the one in the suite's `saved_objects/basic` archive
        let doc = json!({"id": "dd7caf20", "type": "visualization", "references": [], "migrationVersion": {},
            "attributes": {"title": "Count of requests", "visState": "{\"title\":\"Count of requests\",\"type\":\"area\",\"params\":{},\"aggs\":[{\"id\":\"1\",\"type\":\"count\",\"schema\":\"metric\",\"params\":{}}]}",
                           "uiStateJSON": "{}", "description": "", "version": 1,
                           "kibanaSavedObjectMeta": {"searchSourceJSON": "{\"index\":\"91200a00-9efd-11e7-acb3-3dab96693fab\",\"query\":{\"query_string\":{\"query\":\"*\"}},\"filter\":[]}"}}});
        let out = migrate(doc).expect("migrates");
        assert_eq!(out["migrationVersion"]["visualization"], "7.10.0");
        // 7.0 moved the index into a reference
        assert_eq!(out["references"][0]["type"], "index-pattern");
        assert_eq!(out["references"][0]["id"], "91200a00-9efd-11e7-acb3-3dab96693fab");
        let source: Value = serde_json::from_str(
            out["attributes"]["kibanaSavedObjectMeta"]["searchSourceJSON"].as_str().unwrap(),
        )
        .unwrap();
        assert!(source.get("index").is_none());
        assert_eq!(source["indexRefName"], "kibanaSavedObjectMeta.searchSourceJSON.index");
    }

    #[test]
    fn a_moving_average_becomes_a_moving_function() {
        let doc = with_state(json!({"type": "metrics", "params": {"series": [{"metrics": [
            {"type": "moving_average", "model": "holt", "settings": {"alpha": 0.5, "type": "mult"}, "predict": 3}]}]}}));
        let out = migrate(doc).expect("migrates");
        let metric = &state_of(&out)["params"]["series"][0]["metrics"][0];
        assert_eq!(metric["model_type"], "holt");
        assert_eq!(metric["alpha"], 0.5);
        assert_eq!(metric["beta"], 0.1, "the default");
        assert_eq!(metric["multiplicative"], true);
        assert!(metric.get("settings").is_none() && metric.get("predict").is_none());
    }

    #[test]
    fn a_tsvb_filter_written_as_text_becomes_a_lucene_query() {
        let doc = with_state(
            json!({"type": "metrics", "params": {"type": "timeseries", "filter": "status:500",
            "series": [{"filter": "host:a", "split_filters": [{"filter": "b"}]}]}}),
        );
        let out = migrate(doc).expect("migrates");
        let params = &state_of(&out)["params"];
        assert_eq!(params["filter"], json!({"query": "status:500", "language": "lucene"}));
        assert_eq!(params["series"][0]["filter"]["language"], "lucene");
        assert_eq!(params["series"][0]["split_filters"][0]["filter"]["query"], "b");
        // and 7.10 empties a TSVB search source
        assert_eq!(out["attributes"]["kibanaSavedObjectMeta"]["searchSourceJSON"], "{}");
    }

    #[test]
    fn a_table_with_two_splits_keeps_only_the_first() {
        let doc = with_state(json!({"type": "table", "aggs": [
            {"schema": "split", "params": {"row": true}},
            {"schema": "split", "params": {"row": false}}]}));
        let out = migrate(doc).expect("migrates");
        let aggs = &state_of(&out)["aggs"];
        assert_eq!(aggs[0]["schema"], "split");
        assert_eq!(aggs[1]["schema"], "bucket");
        assert!(aggs[1]["params"].get("row").is_none());
    }

    #[test]
    fn a_state_that_is_not_json_is_left_alone() {
        let doc = json!({"id": "v", "type": "visualization", "references": [], "migrationVersion": {},
                         "attributes": {"visState": "not json"}});
        let out = migrate(doc).expect("migrates");
        assert_eq!(out["attributes"]["visState"], "not json");
        assert_eq!(out["migrationVersion"]["visualization"], "7.10.0", "but it is marked as seen");
    }
}
