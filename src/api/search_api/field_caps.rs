//! What each field is, across the indices a search would touch.

use super::*;

pub(crate) fn caps_for(kind: &str) -> Value {
    // a container holds no values of its own: nothing to search it for, and
    // nothing to aggregate over
    let container = matches!(kind, "object" | "nested");
    let aggregatable = kind != "text" && !container;
    let searchable = !container;
    json!({"type": kind, "searchable": searchable, "aggregatable": aggregatable})
}

pub async fn field_caps(
    State(store): State<Store>,
    index: Option<Path<String>>,
    Query(p): Query<Params>,
    body: String,
) -> Response {
    let expr = index.map(|Path(i)| i).unwrap_or_else(|| "_all".into());
    let body: Value = parse_body(&body).unwrap_or(json!({}));
    // every index the cluster holds: a field's capabilities belong to the
    // index, wherever its copies are
    let targets = crate::api::cluster_resolve(&store, &expr);
    if targets.is_empty() && !expr.contains('*') && expr != "_all" {
        return no_such_index(&expr);
    }
    let patterns: Vec<String> = p
        .get("fields")
        .map(|f| f.split(',').map(|s| s.trim().to_string()).collect())
        .or_else(|| {
            body.get("fields")
                .and_then(|f| f.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        })
        .unwrap_or_else(|| vec!["*".into()]);

    // an index_filter drops indices whose documents don't match
    let index_filter = body.get("index_filter").cloned();
    let mut kept: Vec<String> = Vec::new();
    for n in &targets {
        if let Some(f) = &index_filter {
            let probe = json!({"query": f, "size": 0});
            let hit = crate::search::run(&store, n, &probe, &Params::new())
                .map(|o| o.total > 0)
                .unwrap_or(false);
            if !hit {
                continue;
            }
        }
        kept.push(n.clone());
    }

    let mut fields: serde_json::Map<String, Value> = serde_json::Map::new();
    // every document carries these whether its mapping names them or not
    const CARRIED: &[(&str, &str, bool, bool)] = &[
        ("_index", "_index", true, true),
        ("_id", "_id", true, true),
        ("_routing", "_routing", true, false),
        ("_seq_no", "_seq_no", true, true),
        ("_version", "_version", false, false),
        ("_source", "_source", false, false),
        ("_field_names", "_field_names", true, false),
        ("_ignored", "_ignored", true, false),
        ("_nested_path", "_nested_path", true, false),
        ("_doc_count", "long", false, false),
        ("_feature", "_feature", false, false),
        ("_data_stream_timestamp", "_data_stream_timestamp", false, false),
    ];
    if !kept.is_empty() {
        for (name, kind, searchable, aggregatable) in CARRIED {
            if !patterns.iter().any(|pat| {
                pat == "*" || pat == name || crate::store::wildcard_to_regex(pat).is_match(name)
            }) {
                continue;
            }
            fields.insert(
                name.to_string(),
                json!({ *kind: {
                    "type": kind, "searchable": searchable, "aggregatable": aggregatable,
                }}),
            );
        }
    }
    let published = crate::cluster::current_state();
    for n in &kept {
        let Some(st) = store.get(n) else {
            // held elsewhere: the published mapping says what its fields are
            if let Some(m) = published.indices.get(n) {
                let mut types: std::collections::HashMap<String, String> = Default::default();
                crate::search::aggs::published_field_types(&m.mappings, "", &mut types);
                for (name, kind) in types {
                    let asked = patterns.iter().any(|pat| {
                        pat == "*"
                            || *pat == name
                            || crate::store::wildcard_to_regex(pat).is_match(&name)
                    });
                    if !asked {
                        continue;
                    }
                    let entry = fields.entry(name.clone()).or_insert_with(|| json!({}));
                    let e = entry.as_object_mut().unwrap();
                    let per = e.entry(kind.clone()).or_insert_with(
                        || json!({"type": kind, "searchable": true, "aggregatable": true}),
                    );
                    let _ = per;
                }
            }
            continue;
        };
        let g = st.read();
        let view = crate::security::view::view_for(&store, &g.name);
        for (name, kind) in g.all_field_types() {
            // a field the caller may not see is not there
            if view.as_ref().map(|v| v.hidden(&name)).unwrap_or(false) {
                continue;
            }
            let asked = |n: &str| {
                patterns.iter().any(|pat| {
                    pat == "*" || *pat == n || crate::store::wildcard_to_regex(pat).is_match(n)
                })
            };
            let child_asked = matches!(kind.as_str(), "object" | "nested")
                && g.all_field_types()
                    .iter()
                    .any(|(other, _)| other.starts_with(&format!("{name}.")) && asked(other));
            if !asked(&name) && !child_asked {
                continue;
            }
            let kinds: Vec<String> = vec![kind.clone()];
            let meta = g
                .mapping
                .raw
                .pointer(&format!("/properties/{}/meta", name.replace('.', "/properties/")))
                .cloned();
            // a field with no doc values cannot be aggregated over
            let has_doc_values = g
                .mapping
                .raw
                .pointer(&format!("/properties/{}/doc_values", name.replace('.', "/properties/")))
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            // a field the mapping says not to index cannot be searched for
            let indexed = g
                .mapping
                .raw
                .pointer(&format!("/properties/{}/index", name.replace('.', "/properties/")))
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            for kind in kinds {
                let entry = fields.entry(name.clone()).or_insert_with(|| json!({}));
                let slot = entry_of(entry, &kind, || caps_for(&kind));
                if !has_doc_values {
                    let where_not = entry_of(slot, "__unaggregatable", || json!([]));
                    if let Some(a) = where_not.as_array_mut() {
                        a.push(json!(n));
                    }
                }
                if !indexed {
                    // remember where it is not searchable; if that is everywhere,
                    // the field simply is not searchable
                    let where_not = entry_of(slot, "__unsearchable", || json!([]));
                    if let Some(a) = where_not.as_array_mut() {
                        a.push(json!(n));
                    }
                }
                if let Some(m) = meta.clone().and_then(|m| m.as_object().cloned()) {
                    let dst = entry_of(slot, "meta", || json!({}));
                    for (mk, mv) in m {
                        let list = entry_of(dst, &mk, || json!([]));
                        if let Some(a) = list.as_array_mut()
                            && !a.contains(&mv)
                        {
                            a.push(mv);
                        }
                    }
                }
                // a type seen in only some indices lists the ones it came from
                let indices = entry_of(slot, "__indices", || json!([]));
                if let Some(a) = indices.as_array_mut() {
                    a.push(json!(n));
                }
            }
        }
    }

    // `include_unmapped` names the indices a field is missing from, which
    // means the ones it is present in have to be named too
    let unmapped = flag(&p, "include_unmapped");
    if unmapped {
        let mut extra: Vec<(String, Value)> = Vec::new();
        for (name, per_type) in fields.iter() {
            let mut has: Vec<String> = Vec::new();
            for (_, v) in per_type.as_object().into_iter().flatten() {
                for i in v.get("__indices").and_then(|i| i.as_array()).into_iter().flatten() {
                    if let Some(s) = i.as_str() {
                        has.push(s.to_string());
                    }
                }
            }
            let missing: Vec<String> = kept.iter().filter(|n| !has.contains(n)).cloned().collect();
            if !missing.is_empty() {
                extra.push((name.clone(), json!(missing)));
            }
        }
        for (name, missing) in extra {
            if let Some(o) = fields.get_mut(&name).and_then(|v| v.as_object_mut()) {
                // the mapped types now have to say where they are, since one
                // of the entries says where the field is not
                for (_, v) in o.iter_mut() {
                    if let Some(i) = v.get("__indices").cloned() {
                        v["indices"] = i;
                    }
                }
                o.insert(
                    "unmapped".to_string(),
                    json!({
                        "type": "unmapped",
                        "searchable": false,
                        "aggregatable": false,
                        "indices": missing,
                    }),
                );
            }
        }
    }

    // only report `indices` on a field whose type is not uniform
    for (_, per_type) in fields.iter_mut() {
        let type_count = per_type.as_object().map(|o| o.len()).unwrap_or(0);
        if let Some(o) = per_type.as_object_mut() {
            for (_, v) in o.iter_mut() {
                let Some(o) = v.as_object_mut() else { continue };
                let idx = o.remove("__indices");
                let unsearchable = o.remove("__unsearchable");
                let unaggregatable = o.remove("__unaggregatable");
                if let (Some(Value::Array(no)), Some(Value::Array(all))) =
                    (unaggregatable, idx.clone())
                {
                    if no.len() == all.len() {
                        v["aggregatable"] = json!(false);
                    } else if !no.is_empty() {
                        v["aggregatable"] = json!(false);
                        v["non_aggregatable_indices"] = json!(no);
                    }
                }
                // searchable in some indices and not others: say which
                if let (Some(Value::Array(no)), Some(Value::Array(all))) =
                    (unsearchable.clone(), idx.clone())
                {
                    if no.len() == all.len() {
                        v["searchable"] = json!(false);
                    } else if !no.is_empty() {
                        v["searchable"] = json!(false);
                        v["non_searchable_indices"] = json!(no);
                    }
                }
                // with `include_unmapped`, a field present everywhere still
                // needs no listing: there is nothing it is missing from
                let partly = type_count > 1;
                if partly && let Some(i) = idx {
                    v["indices"] = i;
                }
            }
        }
    }

    respond(&p, json!({"indices": kept, "fields": Value::Object(fields)}))
}
