//! The settings document's one change of shape, at 7.9.0.
//!
//! A port of `src/core/server/ui_settings/saved_objects/migrations.ts`: every
//! `siem:` setting was renamed `securitySolution:`, and the document gained
//! a references list like everything else.

use serde_json::{Map, Value};

use super::{Step, references};

pub const CHAIN: &[Step] = &[Step { version: "7.9.0", apply: rename_siem }];

fn rename_siem(mut doc: Value) -> Value {
    if let Some(attributes) = doc.get("attributes").and_then(|v| v.as_object()).cloned() {
        let renamed: Map<String, Value> = attributes
            .into_iter()
            .map(|(key, value)| match key.strip_prefix("siem:") {
                Some(rest) => (format!("securitySolution:{rest}"), value),
                None => (key, value),
            })
            .collect();
        doc["attributes"] = Value::Object(renamed);
    }
    references(&mut doc);
    doc
}

#[cfg(test)]
mod tests {
    use crate::console::migrations::migrate;
    use serde_json::json;

    #[test]
    fn the_archived_config_comes_up_to_date() {
        // the one in the suite's archive: a build number and nothing else
        let doc = json!({"id": "7.0.0-alpha1", "type": "config", "migrationVersion": {},
                         "attributes": {"buildNum": 8467, "siem:defaultIndex": "x"}});
        let out = migrate(doc).expect("migrates");
        assert_eq!(out["attributes"]["buildNum"], 8467);
        assert_eq!(out["attributes"]["securitySolution:defaultIndex"], "x");
        assert!(out["attributes"].get("siem:defaultIndex").is_none());
        assert_eq!(out["references"], json!([]));
        assert_eq!(out["migrationVersion"]["config"], "7.9.0");
    }
}
