//! What each place a script runs lends it.
//!
//! A score script sees `_score`, `doc` and `params`; an update script sees
//! `ctx` with the document under `_source`; a field script emits values; a
//! metric aggregation keeps `state`. The `Runner` here builds those objects
//! from what the engine has -- a document's source and its mapping -- and
//! answers the calls a context adds, `emit` and `randomScore` among them.

use std::cell::RefCell;
use std::rc::Rc;

use serde_json::Value as Json;

use super::eval::Context;
use super::value::*;
use super::{Params, Script, ScriptError};
use crate::store::Mapping;

/// A script and the parameters it was given, as a request names them.
pub struct Compiled {
    pub script: Script,
    pub params: Json,
}

impl Compiled {
    /// Read `{"source": …, "params": …}`, `{"id": …}` or a bare string, taking
    /// a stored script from the store where one is named.
    pub fn of(spec: &Json, stored: &dyn Fn(&str) -> Option<Json>) -> Result<Compiled, ScriptError> {
        let (source, params) = match spec {
            Json::String(s) => (s.clone(), Json::Object(Default::default())),
            Json::Object(o) => {
                let params =
                    o.get("params").cloned().unwrap_or_else(|| Json::Object(Default::default()));
                if let Some(id) = o.get("id").and_then(|v| v.as_str()) {
                    let Some(found) = stored(id) else {
                        return Err(ScriptError {
                            kind: "compile error",
                            message: format!("unable to find script [{id}] in cluster state"),
                            offset: 0,
                            source: String::new(),
                            cause: "resource_not_found_exception".into(),
                        });
                    };
                    let text = found
                        .get("source")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    (text, params)
                } else {
                    let text = o
                        .get("source")
                        .or_else(|| o.get("inline"))
                        .map(|v| match v {
                            Json::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .unwrap_or_default();
                    (text, params)
                }
            }
            other => (other.to_string(), Json::Object(Default::default())),
        };
        Ok(Compiled { script: Script::compile(&source)?, params })
    }
}

/// Run a script spec over one document, the way a search does: `doc`,
/// `params`, `_source` and `_score` are what it sees.
pub fn run_on_doc(
    spec: &Json,
    source: &Json,
    mapping: &Mapping,
    score: f64,
) -> Result<Value, ScriptError> {
    run_on_doc_with(spec, source, mapping, score, None)
}

/// As `run_on_doc`, with the term statistics a score script may ask for.
pub fn run_on_doc_with(
    spec: &Json,
    source: &Json,
    mapping: &Mapping,
    score: f64,
    term_stats: Option<Box<dyn Fn(&str, &str, &str) -> f64>>,
) -> Result<Value, ScriptError> {
    let compiled = Compiled::of(spec, &|_| None)?;
    let mut runner = Runner::new(&compiled.params).with_doc(source, mapping).with_score(score);
    runner.term_stats = term_stats;
    runner.run(&compiled.script)
}

/// Everything a script may reach in one run.
pub struct Runner {
    pub params: Value,
    pub doc: Option<Value>,
    pub ctx: Option<Value>,
    pub score: Option<f64>,
    pub state: Option<Value>,
    pub states: Option<Value>,
    pub values: Option<Value>,
    /// `_value`: one value of the field an aggregation reads, for the script
    /// that maps it
    pub value: Option<Value>,
    /// `interval`: the stretch of text an intervals filter script judges
    pub interval: Option<Value>,
    /// `token`: the token an analysis script judges
    pub token: Option<Value>,
    pub source: Option<Value>,
    /// what `emit(…)` was given, in order
    pub emitted: Rc<RefCell<Vec<Value>>>,
    /// term statistics a score script may ask for, by field and term
    pub term_stats: Option<Box<dyn Fn(&str, &str, &str) -> f64>>,
    /// the id and sequence number a random score is seeded with
    pub seed: (String, u64),
}

impl Runner {
    pub fn new(params: &Json) -> Runner {
        Runner {
            params: Value::Native(Rc::new(Params(Value::from_json(params)))),
            doc: None,
            ctx: None,
            score: None,
            state: None,
            states: None,
            values: None,
            value: None,
            interval: None,
            token: None,
            source: None,
            emitted: Rc::new(RefCell::new(Vec::new())),
            term_stats: None,
            seed: (String::new(), 0),
        }
    }

    /// The parameters, with the document's source under `_source`, which is
    /// how a derived field's script reads the document.
    pub fn with_source_param(mut self, source: &Json) -> Runner {
        let mut base = match &self.params {
            Value::Native(_) => Value::Null,
            other => other.clone(),
        };
        let _ = &mut base;
        let mut pairs: Vec<(Value, Value)> = match Value::from_json(&Json::Null) {
            _ => Vec::new(),
        };
        if let Value::Native(n) = &self.params {
            // read the pairs back out of the params object
            if let Some(Value::Map(m)) = n.call("__all__", &[]).and_then(|r| r.ok()) {
                pairs = m.borrow().clone();
            }
        }
        pairs.push((Value::str("_source"), Value::from_json(source)));
        self.params = Value::Native(Rc::new(Params(Value::map(pairs))));
        self
    }

    pub fn with_doc(mut self, source: &Json, mapping: &Mapping) -> Runner {
        self.doc =
            Some(Value::Native(Rc::new(Doc { source: source.clone(), mapping: mapping.clone() })));
        self.source = Some(Value::from_json(source));
        self
    }

    pub fn with_score(mut self, score: f64) -> Runner {
        self.score = Some(score);
        self
    }

    /// The `ctx` an update script writes into.
    pub fn with_ctx(mut self, ctx: Value) -> Runner {
        self.ctx = Some(ctx);
        self
    }

    pub fn with_state(mut self, state: Value) -> Runner {
        self.state = Some(state);
        self
    }

    pub fn with_states(mut self, states: Value) -> Runner {
        self.states = Some(states);
        self
    }

    pub fn with_value(mut self, value: Value) -> Runner {
        self.value = Some(value);
        self
    }

    pub fn with_values(mut self, values: Vec<f64>) -> Runner {
        self.values = Some(Value::list(values.into_iter().map(Value::Double).collect()));
        self
    }

    pub fn run(&mut self, script: &Script) -> Result<Value, ScriptError> {
        script.run(self)
    }
}

impl Context for Runner {
    fn lookup(&self, name: &str) -> Option<Value> {
        match name {
            "params" => Some(self.params.clone()),
            "doc" => self.doc.clone(),
            "ctx" => self.ctx.clone(),
            "_score" => self.score.map(Value::Double),
            "state" => self.state.clone(),
            "states" => self.states.clone(),
            "values" => self.values.clone(),
            "_source" => self.source.clone(),
            "interval" => self.interval.clone(),
            "token" => self.token.clone(),
            "_value" => self.value.clone().or_else(|| {
                self.values.as_ref().and_then(|v| match v {
                    Value::List(l) => l.borrow().first().cloned(),
                    _ => None,
                })
            }),
            _ => None,
        }
    }

    fn call(&mut self, name: &str, args: &[Value]) -> Option<Result<Value, String>> {
        match name {
            "emit" => {
                let mut out = self.emitted.borrow_mut();
                // a point is emitted as two numbers, which is one value
                if args.len() == 2 && args.iter().all(|a| a.is_number()) {
                    out.push(Value::map(vec![
                        (Value::str("lat"), args[0].clone()),
                        (Value::str("lon"), args[1].clone()),
                    ]));
                } else {
                    for a in args {
                        out.push(a.clone());
                    }
                }
                Some(Ok(Value::Null))
            }
            "randomScore" => {
                // a random score is repeatable: the seed, the field's value
                // for this document and the document's id decide it
                let seed = args.first().and_then(|v| v.as_i64()).unwrap_or(0) as u64;
                let field = args.get(1).map(|v| v.as_text()).unwrap_or_default();
                let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ seed;
                let material = match field.as_str() {
                    "_seq_no" => self.seed.1.to_string(),
                    "" => self.seed.0.clone(),
                    other => self
                        .source
                        .as_ref()
                        .and_then(|s| get_path(s, other))
                        .map(|v| v.as_text())
                        .unwrap_or_else(|| self.seed.0.clone()),
                };
                for b in material.as_bytes() {
                    h ^= *b as u64;
                    h = h.wrapping_mul(0x0000_0100_0000_01b3);
                }
                Some(Ok(Value::Double((h >> 11) as f64 / (1u64 << 53) as f64)))
            }
            "termFreq" | "totalTermFreq" | "sumTotalTermFreq" | "docFreq" | "sumDocFreq" => {
                let field = args.first().map(|v| v.as_text()).unwrap_or_default();
                let term = args.get(1).map(|v| v.as_text()).unwrap_or_default();
                let stats = self.term_stats.as_ref().map(|f| f(name, &field, &term)).unwrap_or(0.0);
                Some(Ok(if name == "termFreq" {
                    Value::Int(stats as i64)
                } else {
                    Value::Long(stats as i64)
                }))
            }
            _ => None,
        }
    }
}

/// `doc`: the document's fields, each answering `.value`, `.size()` and the
/// rest the way doc values do.
pub struct Doc {
    pub source: Json,
    pub mapping: Mapping,
}

impl NativeObject for Doc {
    fn get(&self, name: &str) -> Option<Value> {
        Some(self.field(name))
    }
    fn call(&self, name: &str, args: &[Value]) -> Option<Result<Value, String>> {
        match name {
            "__index__" | "get" => {
                Some(Ok(self.field(&args.first().map(|v| v.as_text()).unwrap_or_default())))
            }
            "containsKey" => {
                let field = args.first().map(|v| v.as_text()).unwrap_or_default();
                Some(Ok(Value::Bool(get_path(&Value::from_json(&self.source), &field).is_some())))
            }
            "__set__" => Some(Err("Unsupported operation: doc values cannot be modified".into())),
            _ => None,
        }
    }
    fn describe(&self) -> String {
        "doc".into()
    }
}

impl Doc {
    fn field(&self, name: &str) -> Value {
        // a keyword sub-field of a text field holds the text itself
        let held = get_path(&Value::from_json(&self.source), name).or_else(|| {
            name.strip_suffix(".keyword")
                .and_then(|base| get_path(&Value::from_json(&self.source), base))
        });
        // a derived field has no doc values: a script reads it as absent
        if self.mapping.is_derived(name) {
            return Value::DocValues(Rc::new(DocValues {
                field: name.to_string(),
                values: Vec::new(),
            }));
        }
        let kind = self.mapping.type_of(name).unwrap_or("").to_string();
        let mut values: Vec<Value> = match held {
            Some(Value::List(l)) => l.borrow().clone(),
            Some(Value::Null) | None => Vec::new(),
            Some(v) => vec![v],
        };
        // a text field has no doc values to read; only its keyword does
        if kind == "text" || kind == "match_only_text" {
            values.clear();
        }
        values = values.into_iter().map(|v| typed(v, &kind)).collect();
        // Doc values come back sorted, because that is the order a column
        // holds them in. A vector is the exception: its order is its meaning,
        // and [1, 0] sorted is [0, 1], which points somewhere else entirely.
        let ordered = kind == "knn_vector";
        if !ordered
            && (matches!(kind.as_str(), "keyword" | "ip" | "boolean" | "date")
                || kind.is_empty()
                || values.iter().all(|v| v.is_number()))
        {
            values.sort_by(|a, b| compare(a, b).unwrap_or(std::cmp::Ordering::Equal));
        }
        Value::DocValues(Rc::new(DocValues { field: name.to_string(), values }))
    }
}

/// A value as its mapped type reads it: a date as a date, a number at the
/// width the field keeps, a point as a point.
fn typed(v: Value, kind: &str) -> Value {
    match kind {
        "date" | "date_nanos" => match &v {
            Value::Str(s) => super::builtins::parse_date(s).unwrap_or(v),
            other if other.is_number() => {
                Value::Date { millis: other.as_i64().unwrap_or(0), offset_secs: 0 }
            }
            _ => v,
        },
        "long" | "integer" | "short" | "byte" | "token_count" => match v.as_i64() {
            Some(n) if kind == "long" => Value::Long(n),
            Some(n) => Value::Int(n),
            None => v,
        },
        "double" | "scaled_float" => v.as_f64().map(Value::Double).unwrap_or(v),
        "float" | "half_float" => v.as_f64().map(|f| Value::Float(f as f32 as f64)).unwrap_or(v),
        "boolean" => match &v {
            Value::Str(s) => Value::Bool(s.as_ref() == "true"),
            _ => v,
        },
        "geo_point" => match &v {
            Value::Str(s) => match crate::search::read_point(&Json::String(s.to_string())) {
                Some((lat, lon)) => geo_point(lat, lon),
                None => v,
            },
            Value::Map(m) => {
                let lat = map_get(m, &Value::str("lat")).and_then(|x| x.as_f64());
                let lon = map_get(m, &Value::str("lon")).and_then(|x| x.as_f64());
                match (lat, lon) {
                    (Some(lat), Some(lon)) => geo_point(lat, lon),
                    _ => v,
                }
            }
            Value::List(l) => {
                let l = l.borrow();
                match (l.get(1).and_then(|x| x.as_f64()), l.first().and_then(|x| x.as_f64())) {
                    (Some(lat), Some(lon)) => geo_point(lat, lon),
                    _ => Value::Null,
                }
            }
            _ => v,
        },
        // a binary value is kept as base64; a script reads the bytes as text
        "binary" => match &v {
            Value::Str(s) => {
                use base64::Engine;
                match base64::engine::general_purpose::STANDARD.decode(s.as_bytes()) {
                    Ok(bytes) => Value::str(&String::from_utf8_lossy(&bytes)),
                    Err(_) => Value::str(s),
                }
            }
            _ => v,
        },
        // an address is kept as the hex of its sixteen bytes; a script reads
        // it spelt the usual way
        "ip" => match &v {
            Value::Str(s) if s.len() == 32 && s.chars().all(|c| c.is_ascii_hexdigit()) => {
                match u128::from_str_radix(s, 16) {
                    Ok(bits) => {
                        let v6 = std::net::Ipv6Addr::from(bits);
                        Value::str(&match v6.to_ipv4_mapped() {
                            Some(v4) => v4.to_string(),
                            None => v6.to_string(),
                        })
                    }
                    Err(_) => v,
                }
            }
            _ => v,
        },
        _ => v,
    }
}

/// A point as doc values hold it: each coordinate is kept as a 32-bit
/// integer, so what a script reads back is the coordinate at that grain.
fn geo_point(lat: f64, lon: f64) -> Value {
    const LAT_STEP: f64 = 180.0 / 4_294_967_296.0;
    const LON_STEP: f64 = 360.0 / 4_294_967_296.0;
    let lat = (lat / LAT_STEP).floor() * LAT_STEP;
    let lon = (lon / LON_STEP).floor() * LON_STEP;
    Value::map(vec![
        (Value::str("lat"), Value::Double(lat)),
        (Value::str("lon"), Value::Double(lon)),
    ])
}

/// A dotted path into a JSON-shaped value.
pub fn get_path(v: &Value, path: &str) -> Option<Value> {
    let mut cur = v.clone();
    for part in path.split('.') {
        cur = match &cur {
            Value::Map(m) => map_get(m, &Value::str(part))?,
            _ => return None,
        };
    }
    Some(cur)
}

/// The `ctx` of an update script: a map the script writes into, whose
/// `_source` is the document.
pub fn update_ctx(
    index: &str,
    id: &str,
    version: u64,
    source: &Json,
    now_ms: i64,
    op: &str,
) -> Value {
    Value::map(vec![
        (Value::str("_index"), Value::str(index)),
        (Value::str("_id"), Value::str(id)),
        (Value::str("_version"), Value::Long(version as i64)),
        (Value::str("_routing"), Value::Null),
        (Value::str("_now"), Value::Long(now_ms)),
        (Value::str("op"), Value::str(op)),
        (Value::str("_source"), Value::from_json(source)),
    ])
}

/// What an update script left in `ctx`.
/// The keys a script added to `ctx` that no update context has.
pub fn ctx_extra_keys(ctx: &Value) -> Vec<String> {
    const KNOWN: &[&str] = &[
        "_index",
        "_id",
        "_version",
        "_routing",
        "_source",
        "_now",
        "op",
        "_type",
        "_seq_no",
        "_primary_term",
    ];
    let Value::Map(m) = ctx else { return Vec::new() };
    m.borrow().iter().map(|(k, _)| k.as_text()).filter(|k| !KNOWN.contains(&k.as_str())).collect()
}

/// What the script left in `ctx`: the operation, the source, and the id
/// and routing if it set them. `Err` where the source has come to hold
/// itself, which cannot be written.
pub fn read_ctx(
    ctx: &Value,
) -> Result<(String, Json, Option<String>, Option<String>), &'static str> {
    let Value::Map(m) = ctx else { return Ok(("index".into(), Json::Null, None, None)) };
    let op = map_get(m, &Value::str("op")).map(|v| v.as_text()).unwrap_or_else(|| "index".into());
    let source = match map_get(m, &Value::str("_source")) {
        Some(v) => v.try_json().map_err(|_| "Iterable object is self-referencing itself")?,
        None => Json::Null,
    };
    let id = map_get(m, &Value::str("_id")).filter(|v| !v.is_null()).map(|v| v.as_text());
    let routing = map_get(m, &Value::str("_routing")).filter(|v| !v.is_null()).map(|v| v.as_text());
    Ok((op, source, id, routing))
}
