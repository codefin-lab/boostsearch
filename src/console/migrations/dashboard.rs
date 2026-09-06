//! A dashboard's changes of shape, 6.7.2 to 7.9.3.
//!
//! Ports of `src/plugins/dashboard/server/saved_objects/` and
//! `src/plugins/dashboard/common/migrate_to_730_panels.ts`.

use serde_json::{Value, json};

use super::{
    Step, at, attributes, migrate_index_pattern, migrate_match_all_query, parsed, references,
    set_json,
};

pub const CHAIN: &[Step] = &[
    Step { version: "6.7.2", apply: migrate_match_all_query },
    Step { version: "7.0.0", apply: migrations_700 },
    Step { version: "7.3.0", apply: migrations_730 },
    Step { version: "7.9.3", apply: migrate_match_all_query },
];

/// 7.0: the index pattern and each panel become references, so an export
/// can carry them and an import can renumber them.
fn migrations_700(mut doc: Value) -> Value {
    references(&mut doc);
    migrate_index_pattern(&mut doc);
    let Some(Value::Array(mut panels)) = parsed(&doc, "attributes.panelsJSON") else {
        return doc;
    };
    for (i, panel) in panels.iter_mut().enumerate() {
        let (Some(kind), Some(id)) = (panel.get("type").cloned(), panel.get("id").cloned()) else {
            continue;
        };
        if kind.is_null() || id.is_null() {
            continue;
        }
        panel["panelRefName"] = json!(format!("panel_{i}"));
        references(&mut doc).push(json!({"name": format!("panel_{i}"), "type": kind, "id": id}));
        if let Some(o) = panel.as_object_mut() {
            o.remove("type");
            o.remove("id");
        }
    }
    set_json(&mut doc, "attributes.panelsJSON", &Value::Array(panels));
    doc
}

/// 7.3: the filters move out of the search source into its query, and the
/// panel layout moves out of `uiStateJSON` into each panel.
fn migrations_730(mut doc: Value) -> Value {
    if !is_dashboard_doc(&doc) {
        return doc;
    }
    // the filters, into the query
    match parsed(&doc, "attributes.kibanaSavedObjectMeta.searchSourceJSON") {
        Some(source) => {
            let moved = move_filters_to_query(source);
            set_json(&mut doc, "attributes.kibanaSavedObjectMeta.searchSourceJSON", &moved);
        }
        // the original logs and hands the document back untouched
        None => return doc,
    }
    let ui_state = at(&doc, "attributes.uiStateJSON")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .unwrap_or_else(|| json!({}));
    let Some(Value::Array(panels)) = parsed(&doc, "attributes.panelsJSON") else {
        return doc;
    };
    let use_margins = at(&doc, "attributes.useMargins").and_then(|v| v.as_bool()).unwrap_or(true);
    let migrated = match migrate_panels_to_730(panels, "7.3.0", use_margins, &ui_state) {
        Ok(p) => p,
        Err(_) => return doc,
    };
    set_json(&mut doc, "attributes.panelsJSON", &Value::Array(migrated));
    attributes(&mut doc).remove("uiStateJSON");
    doc
}

fn is_dashboard_doc(doc: &Value) -> bool {
    doc.get("id").is_some_and(|v| v.is_string())
        && doc.get("type").is_some_and(|v| v.is_string())
        && doc.get("attributes").is_some_and(|v| v.is_object())
        && doc.get("references").is_some_and(|v| !v.is_null())
        && at(doc, "attributes.panelsJSON").is_some_and(|v| v.is_string())
}

/// Before 6.0 a query lived in the filter list as a filter with no `meta`;
/// afterwards it is the search source's own query.
fn move_filters_to_query(source: Value) -> Value {
    let mut out = source.clone();
    let mut kept = Vec::new();
    let mut query =
        source.get("query").cloned().unwrap_or_else(|| json!({"query": "", "language": "kuery"}));
    for filter in source.get("filter").and_then(|v| v.as_array()).into_iter().flatten() {
        let is_query = filter.get("query").is_some() && filter.get("meta").is_none();
        if is_query {
            let text =
                filter.pointer("/query/query_string/query").cloned().unwrap_or_else(|| json!(""));
            query = json!({"query": text, "language": "lucene"});
        } else {
            kept.push(filter.clone());
        }
    }
    out["filter"] = Value::Array(kept);
    out["query"] = query;
    out
}

const PANEL_HEIGHT_SCALE_FACTOR: i64 = 5;
const PANEL_HEIGHT_SCALE_FACTOR_WITH_MARGINS: i64 = 4;
const PANEL_WIDTH_SCALE_FACTOR: i64 = 4;

/// The panels of every dashboard since 6.0, each brought to the 7.3 shape.
///
/// Which shape a panel is in is read off it: a `row` means before 6.1, a
/// `version` says which of 6.1, 6.2, 6.3 or later, and each of those laid
/// its grid out differently.
fn migrate_panels_to_730(
    panels: Vec<Value>,
    version: &str,
    use_margins: bool,
    ui_state: &Value,
) -> Result<Vec<Value>, String> {
    panels
        .into_iter()
        .map(|panel| {
            let stated = panel.get("version").and_then(|v| v.as_str()).unwrap_or("");
            if panel.get("row").is_some() {
                migrate_pre61(panel, version, use_margins, ui_state)
            } else if stated.starts_with("6.1.") {
                migrate_610(panel, version, use_margins, ui_state)
            } else if stated.starts_with("6.2.") {
                Ok(migrate_620(panel, version, use_margins))
            } else if stated.starts_with("6.3.") {
                Ok(migrate_630(panel, version))
            } else if between_640_and_720(stated) {
                Ok(migrate_640_to_720(panel, version))
            } else {
                Ok(panel)
            }
        })
        .collect()
}

fn between_640_and_720(stated: &str) -> bool {
    // `semver.coerce` reads the first three numbers it finds
    let nums: Vec<u64> = stated
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .take(3)
        .filter_map(|p| p.parse().ok())
        .collect();
    let Some(major) = nums.first() else { return false };
    let minor = nums.get(1).copied().unwrap_or(0);
    let v = (*major, minor);
    v > (6, 3) && v < (7, 3)
}

/// What `uiState["P-<index>"]` held for a panel, which 7.3 keeps beside it.
fn embeddable_config(panel: &Value, ui_state: &Value) -> Value {
    let index = panel.get("panelIndex").map(text).unwrap_or_default();
    let mut config = ui_state.get(format!("P-{index}")).cloned().unwrap_or_else(|| json!({}));
    if !config.is_object() {
        config = json!({});
    }
    if panel.get("columns").is_some() || panel.get("sort").is_some() {
        config["columns"] = panel.get("columns").cloned().unwrap_or(Value::Null);
        config["sort"] = panel.get("sort").cloned().unwrap_or(Value::Null);
    }
    config
}

/// A number or a string, as a string, the way JavaScript's `toString` does.
fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn panel_index(panel: &Value) -> String {
    match panel.get("panelIndex") {
        Some(v) if !v.is_null() => text(v),
        _ => crate::console::saved::random_id(),
    }
}

fn without(mut panel: Value, keys: &[&str]) -> Value {
    if let Some(o) = panel.as_object_mut() {
        for key in keys {
            o.remove(*key);
        }
    }
    panel
}

fn migrate_pre61(
    panel: Value,
    version: &str,
    use_margins: bool,
    ui_state: &Value,
) -> Result<Value, String> {
    let (Some(col), Some(row)) =
        (panel.get("col").and_then(|v| v.as_i64()), panel.get("row").and_then(|v| v.as_i64()))
    else {
        return Err("Unable to migrate panel data for \"6.1.0\" backwards compatibility, panel does not contain expected col and/or row fields".into());
    };
    let config = embeddable_config(&panel, ui_state);
    let height_scale = if use_margins {
        PANEL_HEIGHT_SCALE_FACTOR_WITH_MARGINS
    } else {
        PANEL_HEIGHT_SCALE_FACTOR
    };
    let size_x = panel.get("size_x").and_then(|v| v.as_i64());
    let size_y = panel.get("size_y").and_then(|v| v.as_i64());
    let index = panel_index(&panel);
    let mut out = without(panel, &["columns", "sort", "row", "col", "size_x", "size_y"]);
    out["version"] = json!(version);
    out["panelIndex"] = json!(index);
    out["gridData"] = json!({
        "x": (col - 1) * PANEL_WIDTH_SCALE_FACTOR,
        "y": (row - 1) * height_scale,
        "w": size_x.map(|x| x * PANEL_WIDTH_SCALE_FACTOR).unwrap_or(24),
        "h": size_y.map(|y| y * height_scale).unwrap_or(15),
        "i": index,
    });
    out["embeddableConfig"] = config;
    Ok(out)
}

fn migrate_610(
    panel: Value,
    version: &str,
    use_margins: bool,
    ui_state: &Value,
) -> Result<Value, String> {
    for key in ["w", "x", "h", "y"] {
        if panel.pointer(&format!("/gridData/{key}")).is_none() {
            return Err(format!(
                "Unable to migrate panel data for \"6.3.0\" backwards compatibility, panel does not contain expected field: {key}"
            ));
        }
    }
    let config = embeddable_config(&panel, ui_state);
    let grid = scaled_grid(&panel, use_margins);
    let index = panel_index(&panel);
    let mut out = without(panel, &["columns", "sort"]);
    out["version"] = json!(version);
    out["panelIndex"] = json!(index);
    out["gridData"] = grid;
    out["embeddableConfig"] = config;
    Ok(out)
}

fn migrate_620(panel: Value, version: &str, use_margins: bool) -> Value {
    let mut config = panel.get("embeddableConfig").cloned().unwrap_or_else(|| json!({}));
    if panel.get("columns").is_some() || panel.get("sort").is_some() {
        config["columns"] = panel.get("columns").cloned().unwrap_or(Value::Null);
        config["sort"] = panel.get("sort").cloned().unwrap_or(Value::Null);
    }
    let grid = scaled_grid(&panel, use_margins);
    let index = panel_index(&panel);
    let mut out = without(panel, &["columns", "sort"]);
    out["version"] = json!(version);
    out["panelIndex"] = json!(index);
    out["gridData"] = grid;
    out["embeddableConfig"] = config;
    out
}

fn migrate_630(panel: Value, version: &str) -> Value {
    let mut config = panel.get("embeddableConfig").cloned().unwrap_or_else(|| json!({}));
    if panel.get("columns").is_some() || panel.get("sort").is_some() {
        config["columns"] = panel.get("columns").cloned().unwrap_or(Value::Null);
        config["sort"] = panel.get("sort").cloned().unwrap_or(Value::Null);
    }
    let index = panel_index(&panel);
    let mut out = without(panel, &["columns", "sort"]);
    out["version"] = json!(version);
    out["panelIndex"] = json!(index);
    out["embeddableConfig"] = config;
    out
}

fn migrate_640_to_720(mut panel: Value, version: &str) -> Value {
    let index = panel_index(&panel);
    let config = panel.get("embeddableConfig").cloned().unwrap_or_else(|| json!({}));
    panel["version"] = json!(version);
    panel["panelIndex"] = json!(index);
    panel["embeddableConfig"] = config;
    panel
}

/// A 6.1 or 6.2 grid, which counted in coarser cells than 6.3's.
fn scaled_grid(panel: &Value, use_margins: bool) -> Value {
    let height_scale = if use_margins {
        PANEL_HEIGHT_SCALE_FACTOR_WITH_MARGINS
    } else {
        PANEL_HEIGHT_SCALE_FACTOR
    };
    let g = |key: &str| {
        panel.pointer(&format!("/gridData/{key}")).and_then(|v| v.as_i64()).unwrap_or(0)
    };
    json!({
        "w": g("w") * PANEL_WIDTH_SCALE_FACTOR,
        "h": g("h") * height_scale,
        "x": g("x") * PANEL_WIDTH_SCALE_FACTOR,
        "y": g("y") * height_scale,
        "i": panel.pointer("/gridData/i").cloned().unwrap_or(Value::Null),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::migrations::migrate;

    /// The dashboard in the suite's own `saved_objects/basic` archive, which is
    /// what was refusing to load.
    fn archived() -> Value {
        json!({
            "id": "be3733a0-9efe-11e7-acb3-3dab96693fab",
            "type": "dashboard",
            "attributes": {
                "title": "Requests",
                "hits": 0,
                "description": "",
                "panelsJSON": "[{\"id\":\"dd7caf20-9efd-11e7-acb3-3dab96693fab\",\"type\":\"visualization\",\"panelIndex\":1,\"version\":\"6.3.0\",\"gridData\":{\"x\":0,\"y\":0,\"w\":6,\"h\":3,\"i\":\"1\"}}]",
                "optionsJSON": "{\"darkTheme\":false}",
                "uiStateJSON": "{\"P-1\":{\"vis\":{\"legendOpen\":true}}}",
                "version": 1,
                "timeRestore": false,
                "kibanaSavedObjectMeta": {
                    "searchSourceJSON": "{\"query\":{\"query_string\":{\"query\":\"*\"}},\"filter\":[]}"
                }
            },
            "references": [],
            "migrationVersion": {}
        })
    }

    #[test]
    fn the_archived_dashboard_comes_up_to_date() {
        let out = migrate(archived()).expect("migrates");
        assert_eq!(out["migrationVersion"]["dashboard"], "7.9.3");
        // 7.3 took the layout out of uiStateJSON, which is the field the
        // strict mapping was refusing
        assert!(out["attributes"].get("uiStateJSON").is_none(), "uiStateJSON is still there");
        // 7.0 made the panel a reference
        assert_eq!(out["references"][0]["name"], "panel_0");
        assert_eq!(out["references"][0]["id"], "dd7caf20-9efd-11e7-acb3-3dab96693fab");
        let panels: Value =
            serde_json::from_str(out["attributes"]["panelsJSON"].as_str().unwrap()).unwrap();
        assert_eq!(panels[0]["panelRefName"], "panel_0");
        assert!(panels[0].get("id").is_none(), "the id moved into the reference");
        assert_eq!(panels[0]["version"], "7.3.0");
        // a 6.3 panel's grid is not rescaled, and it gains an embeddable config
        assert_eq!(panels[0]["gridData"]["w"], 6);
        assert_eq!(panels[0]["embeddableConfig"], json!({}));
    }

    #[test]
    fn a_pre_61_panel_is_laid_out_on_the_new_grid() {
        let panels = vec![json!({"id": "v", "type": "visualization", "panelIndex": 1,
                                 "row": 2, "col": 3, "size_x": 6, "size_y": 3})];
        let ui = json!({"P-1": {"vis": {"legendOpen": false}}});
        let out = migrate_panels_to_730(panels, "7.3.0", true, &ui).expect("migrates");
        assert_eq!(out[0]["gridData"], json!({"x": 8, "y": 4, "w": 24, "h": 12, "i": "1"}));
        assert_eq!(out[0]["embeddableConfig"]["vis"]["legendOpen"], false, "from uiState");
        assert!(out[0].get("row").is_none());
    }

    #[test]
    fn a_query_hiding_in_the_filters_becomes_the_query() {
        let source = json!({"filter": [
            {"query": {"query_string": {"query": "status:500"}}},
            {"meta": {"index": "x"}, "query": {"match": {}}},
        ]});
        let out = move_filters_to_query(source);
        assert_eq!(out["query"], json!({"query": "status:500", "language": "lucene"}));
        assert_eq!(out["filter"].as_array().unwrap().len(), 1, "the real filter stays");
    }
}
