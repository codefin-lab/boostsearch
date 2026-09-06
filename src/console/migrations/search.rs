//! A saved search's changes of shape, 6.7.2 to 7.9.3.
//!
//! Ports of `src/plugins/discover/server/saved_objects/search_migrations.ts`.

use serde_json::{Value, json};

use super::{Step, at, migrate_index_pattern, migrate_match_all_query, references, set};

pub const CHAIN: &[Step] = &[
    Step { version: "6.7.2", apply: migrate_match_all_query },
    Step { version: "7.0.0", apply: set_new_references },
    Step { version: "7.4.0", apply: migrate_search_sort_to_nested_array },
    Step { version: "7.9.3", apply: migrate_match_all_query },
];

fn set_new_references(mut doc: Value) -> Value {
    references(&mut doc);
    migrate_index_pattern(&mut doc);
    doc
}

/// 7.4: a sort is a list of sorts, each a `[field, direction]` pair.
fn migrate_search_sort_to_nested_array(mut doc: Value) -> Value {
    let Some(sort) = at(&doc, "attributes.sort").cloned().filter(|v| !v.is_null()) else {
        return doc;
    };
    if let Some(list) = sort.as_array()
        && list.first().is_some_and(|first| first.is_array())
    {
        return doc;
    }
    set(&mut doc, "attributes.sort", json!([sort]));
    doc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::migrations::migrate;

    #[test]
    fn a_single_sort_becomes_a_list_of_one() {
        let doc = json!({"id": "s", "type": "search", "references": [], "migrationVersion": {},
                         "attributes": {"sort": ["@timestamp", "desc"]}});
        let out = migrate(doc).expect("migrates");
        assert_eq!(out["attributes"]["sort"], json!([["@timestamp", "desc"]]));
        // and a list of sorts stays as it is
        let again = migrate(out).expect("migrates");
        assert_eq!(again["attributes"]["sort"], json!([["@timestamp", "desc"]]));
    }
}
