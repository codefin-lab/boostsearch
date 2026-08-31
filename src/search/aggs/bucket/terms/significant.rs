//! The terms a set of documents has more of than the index does, and the
//! ones it has few of.

use super::*;

/// `rare_terms`: the terms few documents carry.
///
/// It is `terms` read from the other end -- keep the buckets at or below
/// `max_doc_count` instead of the largest ones -- so it is answered by
/// collecting the buckets and filtering, ordered by key.
pub(crate) fn run_rare_terms_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
    weighted: bool,
    agg_name: &str,
) -> std::result::Result<Value, Response> {
    let spec = def.get("rare_terms").cloned().unwrap_or(json!({}));
    let max_doc_count = spec.get("max_doc_count").and_then(|v| v.as_u64()).unwrap_or(1);
    let Some(field) = spec.get("field").and_then(|f| f.as_str()).map(|s| s.to_string()) else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "illegal_argument_exception",
            "Required one of fields [field, script], but none were specified.",
        ));
    };
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();

    let ty = targets
        .iter()
        .filter_map(|n| store.get(n))
        .next()
        .and_then(|st| st.read().mapping.type_of(&field).map(|t| t.to_string()));

    // a pattern only makes sense against text; a numeric, date or address
    // field has no spelling for a regular expression to match
    for pass in ["include", "exclude"] {
        let is_pattern = matches!(spec.get(pass), Some(Value::String(_)));
        if is_pattern && !matches!(ty.as_deref(), None | Some("keyword" | "text" | "wildcard")) {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!(
                    "Aggregation [{agg_name}] cannot support regular expression style \
                     include/exclude settings as they can only be applied to string fields. \
                     Use an array of values for include/exclude clauses"
                ),
            ));
        }
    }

    let mut terms = json!({"field": field, "size": 65_536});
    if let Some(v) = spec.get("missing") {
        terms["missing"] = v.clone();
    }
    // include and exclude are matched here rather than pushed down: they are
    // applied to the term dictionary, which a date or a number does not live
    // in, so pushing them down silently matches nothing
    let listed = |key: &str| -> Option<Vec<String>> {
        let v = spec.get(key)?;
        let items: Vec<String> = match v {
            Value::Array(a) => a.iter().filter_map(term_filter_text).collect(),
            other => term_filter_text(other).into_iter().collect(),
        };
        Some(items)
    };
    let include = listed("include");
    let exclude = listed("exclude");
    let mut node = json!({"terms": terms});
    if let Some(sa) = sub_aggs {
        node["aggs"] = sa;
    }
    let mut request = json!({"__rare": node});
    if weighted {
        inject_doc_count_helpers(&mut request);
    }
    let query = main_query.clone().unwrap_or_else(|| json!({"match_all": {}}));
    let (_, res) = filtered_count(store, targets, &query, &Some(request.clone()))?;
    let Some(mut res) = res else { return Ok(json!({"buckets": []})) };
    if weighted {
        apply_doc_counts(&mut res);
    }
    let mut buckets: Vec<Value> = res
        .pointer("/__rare/buckets")
        .and_then(|b| b.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|b| b.get("doc_count").and_then(|c| c.as_u64()).unwrap_or(0) <= max_doc_count)
        .filter(|b| {
            let key = b.get("key").cloned().unwrap_or(Value::Null);
            let shown = terms_key_view(key, ty.as_deref());
            let matches = |list: &Vec<String>| {
                list.iter().any(|want| term_filter_matches(want, &shown, ty.as_deref()))
            };
            include.as_ref().map(matches).unwrap_or(true)
                && !exclude.as_ref().map(matches).unwrap_or(false)
        })
        .map(|mut b| {
            if let Some(o) = b.as_object_mut() {
                if let Some(raw) = o.get("key").cloned() {
                    let (k, as_string) = terms_key_view(raw, ty.as_deref());
                    o.insert("key".into(), k);
                    match as_string {
                        Some(s) => {
                            o.insert("key_as_string".into(), Value::String(s));
                        }
                        None => {
                            o.remove("key_as_string");
                        }
                    }
                }
                o.remove(DC_SUM);
                o.remove(DC_CNT);
            }
            b
        })
        .collect();

    // rarest first, which is the whole point of the aggregation, and the key
    // settles the order among buckets that are equally rare
    buckets.sort_by(|a, b| {
        let c = |v: &Value| v.get("doc_count").and_then(|d| d.as_u64()).unwrap_or(0);
        let k = |v: &Value| v.get("key").cloned().unwrap_or(Value::Null);
        c(a).cmp(&c(b)).then_with(|| match (k(a), k(b)) {
            (Value::String(x), Value::String(y)) => x.cmp(&y),
            (Value::Number(x), Value::Number(y)) => x
                .as_f64()
                .unwrap_or(0.0)
                .partial_cmp(&y.as_f64().unwrap_or(0.0))
                .unwrap_or(Ordering::Equal),
            (x, y) => x.to_string().cmp(&y.to_string()),
        })
    });
    Ok(json!({"buckets": buckets}))
}

/// `significant_terms`: the terms that stand out in what the query matched,
/// compared with how common they are in the index as a whole.
///
/// The measure is the one OpenSearch calls JLH: how much more of the
/// foreground a term takes up than of the background, multiplied by the ratio
/// between the two, so that a term has to be both commoner *and* markedly
/// commoner to score well.
pub(crate) fn run_significant_terms(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
    asked_as: &str,
) -> std::result::Result<Value, Response> {
    let kind = if def.get("significant_text").is_some() {
        "significant_text"
    } else {
        "significant_terms"
    };
    let name = def
        .as_object()
        .and_then(|o| o.keys().map(|k| k.to_string()).find(|k| k == kind))
        .unwrap_or_else(|| kind.to_string());
    let spec = def.get(&name).cloned().unwrap_or(json!({}));
    // the measures this aggregation can score by, and the options it takes
    const KNOWN: &[&str] = &[
        "field",
        "script",
        "size",
        "shard_size",
        "min_doc_count",
        "shard_min_doc_count",
        "include",
        "exclude",
        "execution_hint",
        "background_filter",
        "filter_duplicate_text",
        "source_fields",
        "jlh",
        "mutual_information",
        "chi_square",
        "gnd",
        "percentage",
        "script_heuristic",
    ];
    if let Some(stray) = spec
        .as_object()
        .and_then(|o| o.keys().map(|k| k.to_string()).find(|k| !KNOWN.contains(&k.as_str())))
    {
        let near = KNOWN.iter().find(|k| edit_distance(k, &stray) <= 2).copied().unwrap_or("jlh");
        return Err(err(
            StatusCode::BAD_REQUEST,
            "parsing_exception",
            format!("[{name}] unknown field [{stray}] did you mean [{near}]?"),
        ));
    }
    let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
    let size = spec.get("size").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
    // a term seen once or twice is noise, so the floor is higher here than it
    // is for a plain terms aggregation
    let min_doc_count = spec.get("min_doc_count").and_then(|v| v.as_u64()).unwrap_or(3);

    let string_field = targets
        .iter()
        .filter_map(|n| store.get(n))
        .find_map(|st| st.read().mapping.type_of(&field).map(|t| t.to_string()))
        .map(|t| matches!(t.as_str(), "text" | "keyword" | "match_only_text"))
        .unwrap_or(true);
    for key in ["include", "exclude"] {
        if spec.get(key).map(|v| v.is_string()).unwrap_or(false) && !string_field {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                format!(
                    "Aggregation [{asked_as}] cannot support regular expression style \
                     include/exclude settings as they can only be applied to string fields. Use \
                     an array of values for include/exclude clauses"
                ),
            ));
        }
    }
    let listed = |key: &str| -> Option<Vec<String>> {
        let a = spec.get(key)?.as_array()?;
        Some(a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
    };
    let include = listed("include");
    let exclude = listed("exclude");

    let query = main_query.clone().unwrap_or_else(|| json!({"match_all": {}}));
    // A significant *text* aggregation asks about the words in a field rather
    // than about its value. Words are not kept in a column -- only in the term
    // index -- so the text is read back and analysed again, which is what
    // OpenSearch does for this aggregation too.
    let analysed = kind == "significant_text"
        || targets
            .iter()
            .filter_map(|n| store.get(n))
            .find_map(|st| st.read().mapping.type_of(&field).map(|t| t.to_string()))
            .map(|t| matches!(t.as_str(), "text" | "match_only_text"))
            .unwrap_or(false);
    let counted = |q: &Value| -> std::result::Result<(u64, Vec<(Value, u64)>), Response> {
        if !analysed {
            let request =
                json!({"__s": {"terms": {"field": field, "size": 65_536, "min_doc_count": 1}}});
            let (total, res) = filtered_count(store, targets, q, &Some(request))?;
            let buckets = res
                .as_ref()
                .and_then(|r| r.pointer("/__s/buckets"))
                .and_then(|b| b.as_array())
                .map(|a| {
                    a.iter()
                        .map(|b| {
                            (
                                b.get("key").cloned().unwrap_or(Value::Null),
                                b.get("doc_count").and_then(|c| c.as_u64()).unwrap_or(0),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            return Ok((total, buckets));
        }
        let probe = json!({"query": q.clone(), "size": 10_000, "_source": [field.clone()]});
        let answer = run(store, &targets.join(","), &probe, &Params::new())?;
        let mut counts: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        // `filter_duplicate_text` is for text that repeats itself across
        // documents -- boilerplate, signatures, quoted replies. A run of words
        // already seen in an earlier document says nothing new about this one,
        // so the words in it are passed over.
        let dedup = spec.get("filter_duplicate_text").and_then(|v| v.as_bool()).unwrap_or(false);
        let mut seen_runs: std::collections::HashSet<String> = std::collections::HashSet::new();
        const RUN: usize = 3;
        let total = answer.hits.len() as u64;
        for hit in &answer.hits {
            let Some(text) = hit.pointer(&format!("/_source/{}", field.replace('.', "/"))) else {
                continue;
            };
            let text = match text {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            // a word counts once for the document it is in, however often it
            // is repeated there
            let mut tokens: Vec<String> = Vec::new();
            for st in targets.iter().filter_map(|n| store.get(n)).take(1) {
                let g = st.read();
                tokens = crate::query::analyze_text(&g.index, &text, None);
            }
            let mut skip = vec![false; tokens.len()];
            if dedup && tokens.len() >= RUN {
                let mut fresh = Vec::new();
                for i in 0..=tokens.len() - RUN {
                    let run = tokens[i..i + RUN].join(" ");
                    if seen_runs.contains(&run) {
                        for slot in skip.iter_mut().skip(i).take(RUN) {
                            *slot = true;
                        }
                    } else {
                        fresh.push(run);
                    }
                }
                for run in fresh {
                    seen_runs.insert(run);
                }
            }
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for (i, token) in tokens.into_iter().enumerate() {
                if skip.get(i).copied().unwrap_or(false) {
                    continue;
                }
                seen.insert(token);
            }
            for token in seen {
                *counts.entry(token).or_insert(0) += 1;
            }
        }
        let mut out: Vec<(Value, u64)> = counts.into_iter().map(|(k, c)| (json!(k), c)).collect();
        out.sort_by_key(|a| std::cmp::Reverse(a.1));
        Ok((total, out))
    };
    let (fg_total, fg) = counted(&query)?;
    let (bg_total, bg) = counted(&json!({"match_all": {}}))?;
    let read = |res: &Vec<(Value, u64)>| -> Vec<(Value, u64)> { res.clone() };
    let background: std::collections::HashMap<String, u64> =
        read(&bg).into_iter().map(|(k, c)| (k.to_string(), c)).collect();

    let ip_field = targets
        .iter()
        .filter_map(|n| store.get(n))
        .any(|st| st.read().mapping.type_of(&field) == Some("ip"));
    let mut buckets: Vec<Value> = Vec::new();
    for (key, count) in read(&fg) {
        if count < min_doc_count {
            continue;
        }
        let bg_count = background.get(&key.to_string()).copied().unwrap_or(count);
        let key = if ip_field {
            key.as_str().and_then(crate::store::ip_from_canonical).map(Value::String).unwrap_or(key)
        } else {
            key
        };
        let text = match &key {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        if let Some(only) = include.as_ref()
            && !only.contains(&text)
        {
            continue;
        }
        if let Some(never) = exclude.as_ref()
            && never.contains(&text)
        {
            continue;
        }
        let fg_pct = count as f64 / fg_total.max(1) as f64;
        let bg_pct = bg_count as f64 / bg_total.max(1) as f64;
        let score = if bg_pct > 0.0 { (fg_pct - bg_pct) * (fg_pct / bg_pct) } else { 0.0 };
        buckets.push(json!({
            "key": key,
            "doc_count": count,
            "score": score,
            "bg_count": bg_count,
        }));
    }
    buckets.sort_by(|a, b| {
        let s = |v: &Value| v.get("score").and_then(|x| x.as_f64()).unwrap_or(0.0);
        s(b).partial_cmp(&s(a)).unwrap_or(Ordering::Equal).then_with(|| {
            let k = |v: &Value| v.get("key").map(|x| x.to_string()).unwrap_or_default();
            k(a).cmp(&k(b))
        })
    });
    buckets.truncate(size);
    Ok(json!({"doc_count": fg_total, "bg_count": bg_total, "buckets": buckets}))
}
