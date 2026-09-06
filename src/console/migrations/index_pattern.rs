//! An index pattern's changes of shape, 6.5.0 to 7.6.0.
//!
//! Ports of `src/plugins/data/server/saved_objects/index_pattern_migrations.ts`.

use serde_json::{Value, json};

use super::{Step, attributes, parsed, set_json};

pub const CHAIN: &[Step] = &[
    Step { version: "6.5.0", apply: migrate_attribute_type_and_type_meta },
    Step { version: "7.6.0", apply: migrate_sub_type_and_parent_field_properties },
];

/// 6.5: `type` and `typeMeta` are always present, as undefined where absent
/// -- which in JSON is absent, so there is nothing to write.
fn migrate_attribute_type_and_type_meta(mut doc: Value) -> Value {
    let a = attributes(&mut doc);
    for key in ["type", "typeMeta"] {
        if a.get(key).is_some_and(|v| v.is_null()) {
            a.remove(key);
        }
    }
    doc
}

/// 7.6: a multi-field's parent moves from `parent` to `subType.multi.parent`.
fn migrate_sub_type_and_parent_field_properties(mut doc: Value) -> Value {
    let Some(Value::Array(fields)) = parsed(&doc, "attributes.fields") else { return doc };
    let migrated: Vec<Value> = fields
        .into_iter()
        .map(|mut field| {
            if field.get("subType").and_then(|v| v.as_str()) == Some("multi") {
                let parent = field.get("parent").cloned().unwrap_or(Value::Null);
                field.as_object_mut().map(|o| o.remove("parent"));
                field["subType"] = json!({"multi": {"parent": parent}});
            }
            field
        })
        .collect();
    set_json(&mut doc, "attributes.fields", &Value::Array(migrated));
    doc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::migrations::migrate;

    #[test]
    fn a_multi_field_names_its_parent_under_sub_type() {
        let doc = json!({"id": "ip", "type": "index-pattern", "references": [], "migrationVersion": {},
            "attributes": {"title": "logs-*", "fields": "[{\"name\":\"a.keyword\",\"subType\":\"multi\",\"parent\":\"a\"},{\"name\":\"b\"}]"}});
        let out = migrate(doc).expect("migrates");
        let fields: Value =
            serde_json::from_str(out["attributes"]["fields"].as_str().unwrap()).unwrap();
        assert_eq!(fields[0]["subType"], json!({"multi": {"parent": "a"}}));
        assert!(fields[0].get("parent").is_none());
        assert_eq!(fields[1], json!({"name": "b"}), "untouched");
        assert_eq!(out["migrationVersion"]["index-pattern"], "7.6.0");
    }
}
