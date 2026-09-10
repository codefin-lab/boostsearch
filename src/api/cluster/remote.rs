//! Cross-cluster search: the clusters this one may reach, and reaching them.
//!
//! A remote cluster is registered under a name -- `cluster.remote.<name>.seeds`
//! -- and then named in a search as `<name>:<index>`. The search is not run
//! here: it is sent to that cluster whole, and what comes back is merged with
//! what this cluster found. That is what OpenSearch's coordinating node does
//! too; the difference is that it speaks the transport protocol between
//! clusters and this speaks the same HTTP every client speaks.
//!
//! What a remote is asked is the caller's own request with the cluster's name
//! taken off the front of the index expression, so a remote answers about its
//! own indices under their own names and the names are put back on the way
//! out.

use serde_json::{Value, json};

use crate::store::Store;

/// A registered remote cluster.
#[derive(Clone, Debug)]
pub struct Remote {
    pub name: String,
    pub seeds: Vec<String>,
    pub mode: String,
    pub skip_unavailable: bool,
    pub proxy_address: Option<String>,
    pub node_connections: u32,
    pub proxy_socket_connections: u32,
}

/// Every remote this cluster knows, by name.
///
/// They come from the cluster settings a caller wrote and from what the node
/// was started with, the second being the defaults `GET _cluster/settings`
/// reports.
pub fn remotes(store: &Store) -> std::collections::BTreeMap<String, Remote> {
    let mut out: std::collections::BTreeMap<String, Remote> = Default::default();
    let mut note = |name: &str, key: &str, value: &Value| {
        let e = out.entry(name.to_string()).or_insert_with(|| Remote {
            name: name.to_string(),
            seeds: Vec::new(),
            mode: "sniff".into(),
            skip_unavailable: false,
            proxy_address: None,
            node_connections: default_connections(),
            proxy_socket_connections: 18,
        });
        let text = |v: &Value| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        match key {
            "seeds" => {
                e.seeds = match value {
                    Value::Array(a) => a.iter().map(text).collect(),
                    Value::Null => Vec::new(),
                    one => text(one).split(',').map(|s| s.trim().to_string()).collect(),
                };
                e.seeds.retain(|s| !s.is_empty());
            }
            "mode" => e.mode = text(value),
            "skip_unavailable" => {
                e.skip_unavailable = value.as_bool().unwrap_or_else(|| text(value) == "true")
            }
            "proxy_address" => {
                e.proxy_address = (!value.is_null()).then(|| text(value));
            }
            "node_connections" => {
                e.node_connections = text(value).parse().unwrap_or_else(|_| default_connections());
            }
            "proxy_socket_connections" => {
                e.proxy_socket_connections = text(value).parse().unwrap_or(18);
            }
            _ => {}
        }
    };
    // the node's own configuration first, then what the cluster was told
    for (key, value) in configured_defaults() {
        if let Some((name, leaf)) = split_key(&key) {
            note(&name, &leaf, &value);
        }
    }
    let settings = store.cluster_settings();
    for scope in ["persistent", "transient"] {
        let Some(o) = settings.get(scope).and_then(|v| v.as_object()) else { continue };
        for (key, value) in o {
            if let Some((name, leaf)) = split_key(key) {
                note(&name, &leaf, value);
            }
        }
    }
    out.retain(|_, r| !r.seeds.is_empty() || r.proxy_address.is_some());
    out
}

/// How many connections a sniffing remote keeps: three, as OpenSearch keeps,
/// unless the node was started with `cluster.remote.connections_per_cluster`
/// (or `BOOSTSEARCH_REMOTE_CONNECTIONS_PER_CLUSTER`). It used to be one --
/// the number OpenSearch's own cross-cluster suite configures its test
/// cluster with, which is not the number anyone else gets.
fn default_connections() -> u32 {
    std::env::var("BOOSTSEARCH_REMOTE_CONNECTIONS_PER_CLUSTER")
        .ok()
        .or_else(|| {
            crate::tls::node_settings()
                .get("cluster.remote.connections_per_cluster")
                .map(|v| v.as_str().map(String::from).unwrap_or_else(|| v.to_string()))
        })
        .and_then(|v| v.parse().ok())
        .unwrap_or(3)
}

/// `cluster.remote.<name>.<leaf>` split into the name and the leaf.
fn split_key(key: &str) -> Option<(String, String)> {
    let rest = key.strip_prefix("cluster.remote.")?;
    let (name, leaf) = rest.rsplit_once('.')?;
    (!name.contains('.')).then(|| (name.to_string(), leaf.to_string()))
}

/// What the node was started with: `BOOSTSEARCH_CLUSTER_REMOTE=<name>:<host:port>`,
/// several separated by commas, and the same keys in `boostsearch.yml`.
pub fn configured_defaults() -> Vec<(String, Value)> {
    let mut out = Vec::new();
    if let Ok(list) = std::env::var("BOOSTSEARCH_CLUSTER_REMOTE") {
        for one in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if let Some((name, address)) = one.split_once(':') {
                out.push((
                    format!("cluster.remote.{name}.seeds"),
                    json!([address.trim().to_string()]),
                ));
            }
        }
    }
    let settings = crate::tls::node_settings();
    if let Some(o) = settings.as_object() {
        for (k, v) in o {
            if k.starts_with("cluster.remote.") {
                out.push((k.clone(), v.clone()));
            }
        }
    }
    out
}

/// `GET /_remote/info` -- what this cluster knows about the others.
pub async fn remote_info(
    axum::extract::State(store): axum::extract::State<Store>,
    axum::extract::Query(p): axum::extract::Query<crate::api::Params>,
) -> axum::response::Response {
    let mut out = serde_json::Map::new();
    for (name, r) in remotes(&store) {
        // a remote is "connected" when it answers; asking is cheap and the
        // answer is what the caller wants to know
        let connected = reachable(&r);
        let mut entry = json!({
            "connected": connected,
            "mode": r.mode,
            "initial_connect_timeout": "30s",
            "skip_unavailable": r.skip_unavailable,
        });
        if r.mode == "proxy" {
            entry["proxy_address"] = json!(r.proxy_address.clone().unwrap_or_default());
            entry["num_proxy_sockets_connected"] = json!(if connected { 1 } else { 0 });
            entry["max_proxy_socket_connections"] = json!(r.proxy_socket_connections);
        } else {
            entry["seeds"] = json!(r.seeds);
            entry["num_nodes_connected"] = json!(if connected { 1 } else { 0 });
            entry["max_connections_per_cluster"] = json!(r.node_connections);
        }
        out.insert(name, entry);
    }
    crate::api::respond(&p, Value::Object(out))
}

/// Whether a remote answers at all.
fn reachable(r: &Remote) -> bool {
    let Some(base) = base_url(r) else { return false };
    crate::snapshot::blobs::web().get(&format!("{base}/")).call().is_ok()
}

/// Where a remote's HTTP is, from its seeds or its proxy address.
pub fn base_url(r: &Remote) -> Option<String> {
    let address = match (&r.proxy_address, r.seeds.first()) {
        (Some(p), _) if r.mode == "proxy" => p.clone(),
        (_, Some(s)) => s.clone(),
        (Some(p), None) => p.clone(),
        _ => return None,
    };
    let address = address.trim();
    if address.is_empty() {
        return None;
    }
    Some(if address.starts_with("http") {
        address.trim_end_matches('/').to_string()
    } else {
        format!("http://{address}")
    })
}

// ---- searching across clusters ------------------------------------------------

/// An index expression split into what this cluster answers and what the
/// others do.
pub struct Split {
    /// the parts naming no cluster, joined as they were written
    pub local: String,
    /// (cluster name, the expression to ask it) pairs
    pub remote: Vec<(String, String)>,
}

/// Split `a,remote:b,*:c` into this cluster's part and each remote's.
///
/// A name before a colon is a cluster, and it may be a pattern: `*:foo` asks
/// every cluster registered here. A part with no colon is this cluster's.
pub fn split_expression(store: &Store, expr: &str) -> Split {
    let known = remotes(store);
    let mut local: Vec<String> = Vec::new();
    let mut remote: Vec<(String, String)> = Vec::new();
    for part in expr.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        // a date expression carries colons of its own inside angle brackets
        let colon = (!part.starts_with('<')).then(|| part.find(':')).flatten();
        match colon {
            None => local.push(part.to_string()),
            Some(at) => {
                let (cluster, index) = (&part[..at], &part[at + 1..]);
                let matching: Vec<String> = known
                    .keys()
                    .filter(|name| {
                        cluster == name.as_str()
                            || (cluster.contains('*')
                                && crate::store::wildcard_to_regex(cluster).is_match(name))
                    })
                    .cloned()
                    .collect();
                // a cluster pattern matches the *remote* clusters' names:
                // this cluster is named by leaving the prefix off, and `*:x`
                // used to search this cluster too, where `x` may not exist
                for name in matching {
                    remote.push((name, index.to_string()));
                }
            }
        }
    }
    let mut joined: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for (name, index) in remote {
        joined.entry(name).or_default().push(index);
    }
    Split {
        local: local.join(","),
        remote: joined.into_iter().map(|(n, parts)| (n, parts.join(","))).collect(),
    }
}

/// Ask one remote cluster the caller's own question.
pub fn ask(
    r: &Remote,
    method: &str,
    path: &str,
    query: &str,
    body: Option<&Value>,
) -> Result<(u16, Value), String> {
    let Some(base) = base_url(r) else { return Err(format!("[{}] has no address", r.name)) };
    let url =
        if query.is_empty() { format!("{base}{path}") } else { format!("{base}{path}?{query}") };
    // A refusal from a remote is an answer with a body, and the body says what
    // was wrong -- `no such index [x]` -- which is what the caller is owed.
    // An agent that turns a status into an error threw it away, and a
    // missing remote index came back as a 404 with nothing in it.
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    let agent = AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_connect(Some(std::time::Duration::from_secs(10)))
            .timeout_global(Some(std::time::Duration::from_secs(60)))
            .http_status_as_error(false)
            .build()
            .into()
    });
    let sent = match (method, body) {
        ("GET", None) => agent.get(&url).call(),
        ("DELETE", _) => agent.delete(&url).call(),
        (_, Some(b)) => agent.post(&url).header("content-type", "application/json").send_json(b),
        (_, None) => agent.post(&url).send_empty(),
    };
    match sent {
        Ok(mut answer) => {
            let status = answer.status().as_u16();
            let value: Value = answer.body_mut().read_json().unwrap_or(Value::Null);
            Ok((status, value))
        }
        Err(ureq::Error::StatusCode(code)) => Ok((code, Value::Null)),
        Err(e) => Err(format!("[{}]: {e}", r.name)),
    }
}

/// The answers of several clusters, as one answer.
///
/// Each cluster has already reduced its own shards; what is left is to put
/// the hits in one order, add the counts up, and merge the aggregations. This
/// is the coordinating half of a cross-cluster search, and it is where the
/// answer stops being any one cluster's.
pub fn merge_answers(
    mut answers: Vec<(String, Value)>,
    size: usize,
    from: usize,
    plan: &Plan,
) -> Value {
    let (index_aggs, avg_aggs, pipelines) = (&plan.index_aggs, &plan.avg_aggs, &plan.pipelines);
    // the local cluster's answer is the shape everything else is folded into
    let (_, mut out) = answers.remove(0);
    if !out.is_object() {
        out = json!({});
    }
    let mut hits: Vec<Value> =
        out.pointer("/hits/hits").and_then(|h| h.as_array()).cloned().unwrap_or_default();
    // a total is `{value, relation}`, or a bare number where the caller
    // asked for `rest_total_hits_as_int` -- and a remote was asked the same way
    let total_of = |v: &Value| -> u64 {
        v.pointer("/hits/total/value")
            .and_then(|x| x.as_u64())
            .or_else(|| v.pointer("/hits/total").and_then(|x| x.as_u64()))
            .unwrap_or(0)
    };
    let mut total = total_of(&out);
    let relation =
        out.pointer("/hits/total/relation").and_then(|v| v.as_str()).unwrap_or("eq").to_string();
    let mut shards = out.get("_shards").cloned().unwrap_or_else(|| json!({}));
    let mut max_score = out.pointer("/hits/max_score").and_then(|v| v.as_f64());
    let mut aggs = out.get("aggregations").cloned();
    let mut took = out.get("took").and_then(|v| v.as_u64()).unwrap_or(0);

    for (cluster, answer) in answers {
        let theirs: Vec<Value> =
            answer.pointer("/hits/hits").and_then(|h| h.as_array()).cloned().unwrap_or_default();
        for mut hit in theirs {
            // a hit from another cluster carries that cluster's name in front
            // of its index, which is how a caller tells them apart
            if let Some(index) = hit.get("_index").and_then(|v| v.as_str()) {
                hit["_index"] = json!(format!("{cluster}:{index}"));
            }
            hits.push(hit);
        }
        total += total_of(&answer);
        took = took.max(answer.get("took").and_then(|v| v.as_u64()).unwrap_or(0));
        if let Some(theirs) = answer.pointer("/hits/max_score").and_then(|v| v.as_f64()) {
            max_score = Some(max_score.map(|m| m.max(theirs)).unwrap_or(theirs));
        }
        for key in ["total", "successful", "skipped", "failed"] {
            let mine = shards.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
            let theirs = answer.pointer(&format!("/_shards/{key}")).and_then(|v| v.as_u64());
            shards[key] = json!(mine + theirs.unwrap_or(0));
        }
        // what a remote says about its own indices is said again with its
        // name in front: a `top_hits` hit and a bucket keyed by `_index`
        // both name an index, and `x` on a remote is `cluster:x` here
        let mut theirs_aggs = answer.get("aggregations").cloned();
        if let Some(a) = theirs_aggs.as_mut() {
            prefix_remote_names(a, &cluster, index_aggs);
        }
        match (&mut aggs, theirs_aggs) {
            (Some(mine), Some(theirs)) => merge_aggregations(mine, &theirs, plan),
            (None, Some(theirs)) => aggs = Some(theirs),
            _ => {}
        }
    }
    // the hits in one order: by their sort values where the request sorted,
    // by score where it did not
    let sorted = hits.first().map(|h| h.get("sort").is_some()).unwrap_or(false);
    if sorted {
        hits.sort_by(|a, b| {
            let (x, y) = (a.get("sort"), b.get("sort"));
            compare_sort_values(x, y, &plan.descending)
        });
    } else {
        hits.sort_by(|a, b| {
            let s = |h: &Value| h.get("_score").and_then(|v| v.as_f64()).unwrap_or(f64::MIN);
            s(b).partial_cmp(&s(a)).unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    let page: Vec<Value> = hits.into_iter().skip(from).take(size).collect();
    out["hits"] = json!({
        "total": {"value": total, "relation": relation},
        "max_score": max_score.map(|m| json!(m)).unwrap_or(Value::Null),
        "hits": page,
    });
    out["_shards"] = shards;
    out["took"] = json!(took);
    if let Some(mut a) = aggs {
        // the terms asked for deeper than the caller wanted are cut back to
        // what was asked, and a cardinality asked for as its terms is a count
        trim_terms(&mut a, &plan.terms_sizes);
        terms_back_to_cardinality(&mut a, &plan.cardinalities);
        // the averages asked for as sums and counts are averages again
        stats_back_to_averages(&mut a, avg_aggs);
        // A sibling pipeline -- `avg_bucket` and its family -- reads other
        // buckets, and each cluster worked it out from its own buckets only:
        // an average of two clusters' averages is not the average of their
        // buckets. It is worked out again here, from the merged ones.
        recompute_bucket_pipelines(&mut a, pipelines);
        out["aggregations"] = a;
    }
    out
}

/// A sibling bucket pipeline a request asked for: its name, its kind, and the
/// path to the values it reads.
pub type Pipeline = (String, String, String);

/// Every `*_bucket` sibling pipeline at the top of a request's aggregations.
pub fn bucket_pipelines(body: &Value) -> Vec<Pipeline> {
    let mut out = Vec::new();
    for key in ["aggs", "aggregations"] {
        let Some(o) = body.get(key).and_then(|a| a.as_object()) else { continue };
        for (name, def) in o {
            for kind in ["avg_bucket", "sum_bucket", "min_bucket", "max_bucket"] {
                if let Some(path) =
                    def.pointer(&format!("/{kind}/buckets_path")).and_then(|v| v.as_str())
                {
                    out.push((name.clone(), kind.to_string(), path.to_string()));
                }
            }
        }
    }
    out
}

/// Each sibling pipeline, from the merged buckets it reads.
fn recompute_bucket_pipelines(aggs: &mut Value, pipelines: &[Pipeline]) {
    for (name, kind, path) in pipelines {
        // `cluster>s` or `cluster.s`: the buckets of `cluster`, and the metric
        // `s` in each -- the older spelling with a dot is still the one the
        // suite writes
        let Some((bucketed, metric)) = path.split_once('>').or_else(|| path.split_once('.')) else {
            continue;
        };
        let values: Vec<f64> = aggs
            .get(bucketed)
            .and_then(|b| b.get("buckets"))
            .and_then(|b| b.as_array())
            .map(|buckets| {
                buckets
                    .iter()
                    .filter_map(|b| {
                        if metric == "_count" {
                            b.get("doc_count").and_then(|v| v.as_f64())
                        } else {
                            // `m.avg` is the `avg` of a `stats` -- or, once
                            // the stats are an average again, its value
                            let (agg, key) = metric.split_once('.').unwrap_or((metric, "value"));
                            let m = b.get(agg)?;
                            m.get(key).or_else(|| m.get("value")).and_then(|v| v.as_f64())
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        if values.is_empty() {
            continue;
        }
        let value = match kind.as_str() {
            "avg_bucket" => values.iter().sum::<f64>() / values.len() as f64,
            "sum_bucket" => values.iter().sum(),
            "min_bucket" => values.iter().cloned().fold(f64::INFINITY, f64::min),
            _ => values.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        };
        if let Some(slot) = aggs.get_mut(name) {
            slot["value"] = json!(value);
        }
    }
}

/// Every `avg` aggregation in a request, asked for as `stats` instead so its
/// sum and count survive the trip; the names are returned so the answer can
/// be turned back.
pub fn averages_as_stats(body: &mut Value) -> Vec<String> {
    fn walk(node: &mut Value, out: &mut Vec<String>) {
        let Some(o) = node.as_object_mut() else { return };
        for (name, def) in o.iter_mut() {
            if let Some(inner) = def.get("avg").cloned() {
                if let Some(d) = def.as_object_mut() {
                    d.remove("avg");
                    d.insert("stats".into(), inner);
                }
                out.push(name.clone());
            }
            for key in ["aggs", "aggregations"] {
                if let Some(sub) = def.get_mut(key) {
                    walk(sub, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    for key in ["aggs", "aggregations"] {
        if let Some(a) = body.get_mut(key) {
            walk(a, &mut out);
        }
    }
    // A pipeline that read one of them reads a single number, and `stats`
    // is several: its path is pointed at the `avg` inside, or the request
    // stops parsing -- which it did, for every search with an `avg_bucket`
    // over an `avg`.
    fn repoint(node: &mut Value, renamed: &[String]) {
        match node {
            Value::Object(o) => {
                for (k, v) in o.iter_mut() {
                    if k == "buckets_path"
                        && let Value::String(path) = v
                    {
                        let last = path.rsplit(['>', '.']).next().unwrap_or("").to_string();
                        if renamed.contains(&last) {
                            *path = format!("{path}.avg");
                        }
                    } else {
                        repoint(v, renamed);
                    }
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|x| repoint(x, renamed)),
            _ => {}
        }
    }
    if !out.is_empty() {
        for key in ["aggs", "aggregations"] {
            if let Some(a) = body.get_mut(key) {
                repoint(a, &out);
            }
        }
    }
    out
}

/// The names of the `terms` aggregations over `_index`, wherever they sit.
pub fn aggs_on_index(body: &Value) -> Vec<String> {
    fn walk(node: &Value, out: &mut Vec<String>) {
        let Some(o) = node.as_object() else { return };
        for (name, def) in o {
            if def.pointer("/terms/field").and_then(|f| f.as_str()) == Some("_index") {
                out.push(name.clone());
            }
            for key in ["aggs", "aggregations"] {
                if let Some(sub) = def.get(key) {
                    walk(sub, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    for key in ["aggs", "aggregations"] {
        if let Some(a) = body.get(key) {
            walk(a, &mut out);
        }
    }
    out
}

/// A remote's index names, inside its aggregations, written as this cluster
/// sees them.
fn prefix_remote_names(aggs: &mut Value, cluster: &str, index_aggs: &[String]) {
    let Some(o) = aggs.as_object_mut() else { return };
    for (name, agg) in o.iter_mut() {
        if let Some(hits) = agg.pointer_mut("/hits/hits").and_then(|h| h.as_array_mut()) {
            for hit in hits {
                if let Some(index) = hit.get("_index").and_then(|v| v.as_str()).map(String::from) {
                    hit["_index"] = json!(format!("{cluster}:{index}"));
                }
            }
        }
        if let Some(buckets) = agg.get_mut("buckets").and_then(|b| b.as_array_mut()) {
            for bucket in buckets.iter_mut() {
                if index_aggs.contains(name)
                    && let Some(key) = bucket.get("key").and_then(|k| k.as_str()).map(String::from)
                {
                    bucket["key"] = json!(format!("{cluster}:{key}"));
                }
                prefix_remote_names(bucket, cluster, index_aggs);
            }
        }
    }
}

/// `stats` results that were asked for in place of `avg`, as `avg` results.
fn stats_back_to_averages(aggs: &mut Value, avg_aggs: &[String]) {
    let Some(o) = aggs.as_object_mut() else { return };
    for (name, agg) in o.iter_mut() {
        if avg_aggs.contains(name) && agg.get("count").is_some() {
            let count = agg.get("count").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let sum = agg.get("sum").and_then(|v| v.as_f64()).unwrap_or(0.0);
            *agg = if count > 0.0 { json!({"value": sum / count}) } else { json!({"value": null}) };
            continue;
        }
        if let Some(buckets) = agg.get_mut("buckets").and_then(|b| b.as_array_mut()) {
            for bucket in buckets.iter_mut() {
                stats_back_to_averages(bucket, avg_aggs);
            }
        }
    }
}

/// Two hits' `sort` arrays, compared the way a search sorted them: on the
/// first value that differs, in the direction that key was asked for. A
/// cluster answers in its own order and the merge has to put them in one.
///
/// It compared every key ascending, so a search sorted `desc` across two
/// clusters came back in ascending order.
fn compare_sort_values(
    a: Option<&Value>,
    b: Option<&Value>,
    descending: &[bool],
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (Some(Value::Array(a)), Some(Value::Array(b))) = (a, b) else { return Ordering::Equal };
    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        // a hit missing the value sorts last, whichever way the key runs
        match (x.is_null(), y.is_null()) {
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            _ => {}
        }
        let ord = match (x, y) {
            (Value::Number(p), Value::Number(q)) => p
                .as_f64()
                .unwrap_or(f64::MIN)
                .partial_cmp(&q.as_f64().unwrap_or(f64::MIN))
                .unwrap_or(Ordering::Equal),
            (Value::String(p), Value::String(q)) => p.cmp(q),
            _ => Ordering::Equal,
        };
        let ord = if descending.get(i).copied().unwrap_or(false) { ord.reverse() } else { ord };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// One cluster's aggregations folded into another's.
///
/// Buckets with the same key are one bucket, and their counts add up; a
/// metric is combined the way that metric combines. What cannot be combined
/// -- a cardinality sketch, a percentile -- is left as the first cluster's,
/// which is wrong in the small and is said here rather than hidden.
pub fn merge_aggregations(into: &mut Value, from: &Value, plan: &Plan) {
    let kinds = &plan.kinds;
    let (Some(mine), Some(theirs)) = (into.as_object_mut(), from.as_object()) else { return };
    for (name, theirs) in theirs {
        let Some(slot) = mine.get_mut(name) else {
            mine.insert(name.clone(), theirs.clone());
            continue;
        };
        // a bucketed aggregation: the buckets are joined by key
        if let (Some(mine_buckets), Some(their_buckets)) = (
            slot.get("buckets").and_then(|b| b.as_array()).cloned(),
            theirs.get("buckets").and_then(|b| b.as_array()),
        ) {
            let mut joined: Vec<Value> = mine_buckets;
            for bucket in their_buckets {
                let key = bucket.get("key");
                match joined.iter_mut().find(|b| b.get("key") == key) {
                    Some(mine) => {
                        let a = mine.get("doc_count").and_then(|v| v.as_u64()).unwrap_or(0);
                        let b = bucket.get("doc_count").and_then(|v| v.as_u64()).unwrap_or(0);
                        mine["doc_count"] = json!(a + b);
                        merge_aggregations(mine, bucket, plan);
                    }
                    None => joined.push(bucket.clone()),
                }
            }
            // the fullest first, and buckets holding the same number by their
            // key -- the order one cluster answers in, which the merge has to
            // reproduce rather than leave in the order the clusters replied
            joined.sort_by(|a, b| {
                let c = |v: &Value| v.get("doc_count").and_then(|x| x.as_u64()).unwrap_or(0);
                let k = |v: &Value| match v.get("key") {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => String::new(),
                };
                c(b).cmp(&c(a)).then_with(|| k(a).cmp(&k(b)))
            });
            slot["buckets"] = json!(joined);
            for key in ["sum_other_doc_count", "doc_count_error_upper_bound"] {
                if let (Some(a), Some(b)) = (
                    slot.get(key).and_then(|v| v.as_u64()),
                    theirs.get(key).and_then(|v| v.as_u64()),
                ) {
                    slot[key] = json!(a + b);
                }
            }
            continue;
        }
        // `top_hits` holds hits, not buckets or a value: both clusters' hits
        // are the candidates, in score order, as many as either returned
        if let (Some(mine_hits), Some(their_hits)) = (
            slot.pointer("/hits/hits").and_then(|h| h.as_array()).cloned(),
            theirs.pointer("/hits/hits").and_then(|h| h.as_array()),
        ) {
            // in the order the aggregation asked for, and as many as it asked
            // for: both clusters' hits were kept, one list after the other,
            // and the second cluster's best never reached the top
            let (size, descending) = plan
                .top_hits
                .iter()
                .find(|(n, _, _)| n == name)
                .map(|(_, size, d)| (*size, d.clone()))
                .unwrap_or((3, Vec::new()));
            let mut all = mine_hits;
            all.extend(their_hits.iter().cloned());
            if all.first().map(|h| h.get("sort").is_some()).unwrap_or(false) {
                all.sort_by(|a, b| compare_sort_values(a.get("sort"), b.get("sort"), &descending));
            } else {
                all.sort_by(|a, b| {
                    let s =
                        |h: &Value| h.get("_score").and_then(|v| v.as_f64()).unwrap_or(f64::MIN);
                    s(b).partial_cmp(&s(a)).unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            all.truncate(size);
            let add = |p: &str| -> u64 {
                slot.pointer(p).and_then(|v| v.as_u64()).unwrap_or(0)
                    + theirs.pointer(p).and_then(|v| v.as_u64()).unwrap_or(0)
            };
            let total = add("/hits/total/value").max(add("/hits/total"));
            slot["hits"]["hits"] = json!(all);
            if slot.pointer("/hits/total/value").is_some() {
                slot["hits"]["total"]["value"] = json!(total);
            } else if slot.pointer("/hits/total").is_some() {
                slot["hits"]["total"] = json!(total);
            }
            continue;
        }
        // a metric: combined the way its own arithmetic combines
        let both = |key: &str| -> Option<(f64, f64)> {
            Some((
                slot.get(key).and_then(|v| v.as_f64())?,
                theirs.get(key).and_then(|v| v.as_f64())?,
            ))
        };
        // a `stats` result: counts and sums add, the extremes are extremes.
        // The values are read out before anything is written back, so the
        // reading and the writing do not overlap.
        if slot.get("count").is_some() && slot.get("sum").is_some() {
            let num = |v: &Value, k: &str| v.get(k).and_then(|x| x.as_f64());
            let (mine, other) = (slot.clone(), theirs.clone());
            let add = |k: &str| num(&mine, k).unwrap_or(0.0) + num(&other, k).unwrap_or(0.0);
            let count = add("count");
            let sum = add("sum");
            let min = match (num(&mine, "min"), num(&other, "min")) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            let max = match (num(&mine, "max"), num(&other, "max")) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
            // a count is a whole number, and was answered as `11.0`
            slot["count"] = json!(count as u64);
            slot["sum"] = json!(sum);
            slot["min"] = min.map(|m| json!(m)).unwrap_or(Value::Null);
            slot["max"] = max.map(|m| json!(m)).unwrap_or(Value::Null);
            slot["avg"] = if count > 0.0 { json!(sum / count) } else { Value::Null };
            continue;
        }
        // By what the aggregation is, which the request says -- not by what
        // it is called. The name was read for `min` and `max`, so a `max`
        // called `mx` was added up and answered 3 where the answer was 2.
        if let Some((a, b)) = both("value") {
            let kind = kinds.get(name).map(String::as_str).unwrap_or("");
            let integral = slot.get("value").map(|v| v.is_u64()).unwrap_or(false)
                && theirs.get("value").map(|v| v.is_u64()).unwrap_or(false);
            slot["value"] = match kind {
                "min" => json!(a.min(b)),
                "max" => json!(a.max(b)),
                // a sibling pipeline is worked out again from the merged
                // buckets; what either cluster said of its own is not added
                k if k.ends_with("_bucket") => json!(a),
                _ if integral => json!(
                    slot["value"].as_u64().unwrap_or(0) + theirs["value"].as_u64().unwrap_or(0)
                ),
                // a sum, a count, a script's reduction: they add
                _ => json!(a + b),
            };
        }
        if slot.get("doc_count").is_some() {
            let a = slot.get("doc_count").and_then(|v| v.as_u64()).unwrap_or(0);
            let b = theirs.get("doc_count").and_then(|v| v.as_u64()).unwrap_or(0);
            slot["doc_count"] = json!(a + b);
            merge_aggregations(slot, theirs, plan);
        }
    }
}

/// What a cross-cluster search had to change about the request it sends, so
/// the answers can be put back together, and what each aggregation is.
#[derive(Default)]
pub struct Plan {
    pub index_aggs: Vec<String>,
    pub avg_aggs: Vec<String>,
    pub pipelines: Vec<Pipeline>,
    /// every aggregation's name, and its kind
    pub kinds: std::collections::HashMap<String, String>,
    /// `cardinality` aggregations, asked for as the `terms` they count
    pub cardinalities: Vec<String>,
    /// `terms` aggregations asked for deeper, and the size the caller wanted
    pub terms_sizes: Vec<(String, usize)>,
    /// for each sort key, whether it runs descending
    pub descending: Vec<bool>,
    /// `top_hits` aggregations: how many hits, and which way each key sorts
    pub top_hits: Vec<(String, usize, Vec<bool>)>,
}

/// The request each cluster is sent, and the plan for merging what they say.
pub fn plan_across_clusters(body: &mut Value) -> Plan {
    let mut plan = Plan { kinds: aggregation_kinds(body), ..Default::default() };
    rewrite_for_merge(body, &mut plan);
    plan.avg_aggs = averages_as_stats(body);
    plan.index_aggs = aggs_on_index(body);
    plan.pipelines = bucket_pipelines(body);
    plan.descending = sort_directions(body);
    plan.top_hits = top_hits_of(body);
    plan
}

/// Each aggregation's name and the kind of aggregation it is.
fn aggregation_kinds(body: &Value) -> std::collections::HashMap<String, String> {
    fn walk(node: &Value, out: &mut std::collections::HashMap<String, String>) {
        let Some(o) = node.as_object() else { return };
        for (name, def) in o {
            let Some(d) = def.as_object() else { continue };
            if let Some(kind) =
                d.keys().find(|k| !matches!(k.as_str(), "aggs" | "aggregations" | "meta"))
            {
                out.insert(name.clone(), kind.clone());
            }
            for key in ["aggs", "aggregations"] {
                if let Some(sub) = d.get(key) {
                    walk(sub, out);
                }
            }
        }
    }
    let mut out = Default::default();
    for key in ["aggs", "aggregations"] {
        if let Some(a) = body.get(key) {
            walk(a, &mut out);
        }
    }
    out
}

/// A `cardinality` cannot be merged from two counts -- the values both
/// clusters saw were counted twice, 6 where there were 4 -- so each cluster
/// is asked for the values and they are counted once they are together. A
/// `terms` is asked for deeper than the caller wanted, the way OpenSearch
/// asks its shards (`size * 1.5 + 10`): a bucket that is second on one
/// cluster and third on another is first once they are added, and asking
/// each for its top two left it out and undercounted what it did return.
fn rewrite_for_merge(body: &mut Value, plan: &mut Plan) {
    fn walk(node: &mut Value, plan: &mut Plan) {
        let Some(o) = node.as_object_mut() else { return };
        for (name, def) in o.iter_mut() {
            if let Some(card) = def.get("cardinality").cloned()
                && let Some(field) = card.get("field").cloned()
                && card.get("script").is_none()
                && let Some(d) = def.as_object_mut()
            {
                d.remove("cardinality");
                // as many values as the search may make buckets of by default;
                // past that the count is of the first ten thousand, which is
                // still closer than two sketches added together
                let mut terms = json!({"field": field, "size": 10_000});
                if let Some(missing) = card.get("missing") {
                    terms["missing"] = missing.clone();
                }
                d.insert("terms".into(), terms);
                plan.cardinalities.push(name.clone());
            } else if let Some(terms) = def.get_mut("terms").and_then(|t| t.as_object_mut())
                && terms.get("order").is_none()
            {
                let size = terms.get("size").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
                let shard = terms
                    .get("shard_size")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize)
                    .unwrap_or(size + size / 2 + 10);
                terms.insert("size".into(), json!(shard.max(size)));
                plan.terms_sizes.push((name.clone(), size));
            }
            for key in ["aggs", "aggregations"] {
                if let Some(sub) = def.get_mut(key) {
                    walk(sub, plan);
                }
            }
        }
    }
    for key in ["aggs", "aggregations"] {
        if let Some(a) = body.get_mut(key) {
            walk(a, plan);
        }
    }
}

/// The `terms` asked for deeper, cut back to the size the caller asked for;
/// what is cut is counted in `sum_other_doc_count`, as it would have been.
fn trim_terms(aggs: &mut Value, sizes: &[(String, usize)]) {
    let Some(o) = aggs.as_object_mut() else { return };
    for (name, agg) in o.iter_mut() {
        let wanted = sizes.iter().find(|(n, _)| n == name).map(|(_, s)| *s);
        if let Some(buckets) = agg.get_mut("buckets").and_then(|b| b.as_array_mut()) {
            let mut cut = 0u64;
            if let Some(size) = wanted
                && buckets.len() > size
            {
                cut = buckets[size..]
                    .iter()
                    .map(|b| b.get("doc_count").and_then(|v| v.as_u64()).unwrap_or(0))
                    .sum();
                buckets.truncate(size);
            }
            for bucket in buckets.iter_mut() {
                trim_terms(bucket, sizes);
            }
            if cut > 0 {
                let other = agg.get("sum_other_doc_count").and_then(|v| v.as_u64()).unwrap_or(0);
                agg["sum_other_doc_count"] = json!(other + cut);
            }
        }
    }
}

/// A `cardinality` asked for as `terms`, answered as a count of its keys.
fn terms_back_to_cardinality(aggs: &mut Value, names: &[String]) {
    let Some(o) = aggs.as_object_mut() else { return };
    for (name, agg) in o.iter_mut() {
        if names.contains(name)
            && let Some(buckets) = agg.get("buckets").and_then(|b| b.as_array())
        {
            *agg = json!({"value": buckets.len()});
            continue;
        }
        if let Some(buckets) = agg.get_mut("buckets").and_then(|b| b.as_array_mut()) {
            for bucket in buckets.iter_mut() {
                terms_back_to_cardinality(bucket, names);
            }
        }
    }
}

/// Every `top_hits`, with its size and the direction of each of its sort keys.
fn top_hits_of(body: &Value) -> Vec<(String, usize, Vec<bool>)> {
    fn walk(node: &Value, out: &mut Vec<(String, usize, Vec<bool>)>) {
        let Some(o) = node.as_object() else { return };
        for (name, def) in o {
            if let Some(t) = def.get("top_hits") {
                let size = t.get("size").and_then(|v| v.as_u64()).unwrap_or(3) as usize;
                let from = t.get("from").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                out.push((name.clone(), size + from, sort_directions(t)));
            }
            for key in ["aggs", "aggregations"] {
                if let Some(sub) = def.get(key) {
                    walk(sub, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    for key in ["aggs", "aggregations"] {
        if let Some(a) = body.get(key) {
            walk(a, &mut out);
        }
    }
    out
}

/// For each key a request sorts on, whether it runs descending: a score
/// runs descending unless it is told otherwise, and everything else ascends.
fn sort_directions(body: &Value) -> Vec<bool> {
    let keys: Vec<Value> = match body.get("sort") {
        Some(Value::Array(a)) => a.clone(),
        Some(one) => vec![one.clone()],
        None => Vec::new(),
    };
    keys.iter()
        .map(|k| match k {
            Value::String(s) => s == "_score",
            Value::Object(o) => o.iter().next().is_some_and(|(field, spec)| {
                let order = spec.as_str().or_else(|| spec.get("order").and_then(|v| v.as_str()));
                match order {
                    Some(o) => o == "desc",
                    None => field == "_score",
                }
            }),
            _ => false,
        })
        .collect()
}

/// A query's `_index` clauses, as a remote cluster must read them.
///
/// `remote:x` becomes `x`, and a bare `x` -- an index of the asking cluster
/// -- becomes a clause that matches nothing on the remote.
pub fn localize_index_clauses(query: &mut Value, cluster: &str) {
    let prefix = format!("{cluster}:");
    let nothing = json!({"bool": {"must_not": [{"match_all": {}}]}});
    let Some(o) = query.as_object_mut() else {
        if let Some(a) = query.as_array_mut() {
            a.iter_mut().for_each(|q| localize_index_clauses(q, cluster));
        }
        return;
    };
    for kind in ["term", "prefix", "wildcard"] {
        let Some(slot) = o.get_mut(kind).and_then(|t| t.get_mut("_index")) else { continue };
        let text = match slot {
            Value::String(s) => Some(s.clone()),
            Value::Object(inner) => inner.get("value").and_then(|v| v.as_str()).map(String::from),
            _ => None,
        };
        let Some(text) = text else { continue };
        match text.strip_prefix(&prefix) {
            Some(local) => match slot {
                Value::String(s) => *s = local.to_string(),
                Value::Object(inner) => {
                    inner.insert("value".into(), json!(local));
                }
                _ => {}
            },
            None => {
                *query = nothing;
                return;
            }
        }
    }
    if let Some(Value::Array(list)) = o.get_mut("terms").and_then(|t| t.get_mut("_index")) {
        let kept: Vec<Value> = list
            .iter()
            .filter_map(|v| v.as_str())
            .filter_map(|v| v.strip_prefix(&prefix).map(|l| json!(l)))
            .collect();
        if kept.is_empty() {
            *query = nothing;
            return;
        }
        *list = kept;
    }
    for (_, v) in o.iter_mut() {
        localize_index_clauses(v, cluster);
    }
}

#[cfg(test)]
mod merge_tests {
    use super::*;

    fn answer(hits: Value, aggs: Value) -> Value {
        json!({"took": 1, "_shards": {"total": 1, "successful": 1, "skipped": 0, "failed": 0},
               "hits": {"total": {"value": 1, "relation": "eq"}, "max_score": null, "hits": hits},
               "aggregations": aggs})
    }

    /// Measured against OpenSearch 3.1.0: every one of these was a different
    /// number there, and every one was a merge that read the wrong thing.
    #[test]
    fn a_merge_reads_the_request_not_the_names() {
        let mut body = json!({"size": 0, "aggs": {
            "mx": {"max": {"field": "n"}}, "c": {"cardinality": {"field": "k"}},
            "t": {"terms": {"field": "k", "size": 1}}}});
        let plan = plan_across_clusters(&mut body);
        assert_eq!(body.pointer("/aggs/t/terms/size"), Some(&json!(11)));
        assert!(body.pointer("/aggs/c/terms").is_some());
        let local = answer(
            json!([]),
            json!({"mx": {"value": 2.0},
            "c": {"buckets": [{"key": "a", "doc_count": 1}, {"key": "b", "doc_count": 1}]},
            "t": {"buckets": [{"key": "a", "doc_count": 2}, {"key": "b", "doc_count": 1}],
                  "sum_other_doc_count": 0, "doc_count_error_upper_bound": 0}}),
        );
        let remote = answer(
            json!([]),
            json!({"mx": {"value": 1.0},
            "c": {"buckets": [{"key": "b", "doc_count": 1}, {"key": "c", "doc_count": 1}]},
            "t": {"buckets": [{"key": "b", "doc_count": 3}],
                  "sum_other_doc_count": 0, "doc_count_error_upper_bound": 0}}),
        );
        let out = merge_answers(vec![(String::new(), local), ("r".into(), remote)], 0, 0, &plan);
        assert_eq!(out.pointer("/aggregations/mx/value"), Some(&json!(2.0)));
        assert_eq!(out.pointer("/aggregations/c/value"), Some(&json!(3)));
        assert_eq!(out.pointer("/aggregations/t/buckets/0/key"), Some(&json!("b")));
        assert_eq!(out.pointer("/aggregations/t/buckets/0/doc_count"), Some(&json!(4)));
        assert_eq!(out.pointer("/aggregations/t/sum_other_doc_count"), Some(&json!(2)));
    }

    #[test]
    fn a_descending_sort_stays_descending_across_clusters() {
        let mut body = json!({"sort": [{"n": "desc"}]});
        let plan = plan_across_clusters(&mut body);
        let local = answer(
            json!([{"_index": "a", "sort": [1]}, {"_index": "a", "sort": [0]}]),
            json!(null),
        );
        let remote = answer(json!([{"_index": "b", "sort": [2]}]), json!(null));
        let out = merge_answers(vec![(String::new(), local), ("r".into(), remote)], 10, 0, &plan);
        let order: Vec<i64> = out["hits"]["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["sort"][0].as_i64().unwrap())
            .collect();
        assert_eq!(order, vec![2, 1, 0]);
    }
}
