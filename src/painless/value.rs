//! The values a script handles.
//!
//! Painless is dynamically typed at the `def` level, and everything the suite
//! writes reads well as a dynamic value: Java's numbers, strings, lists and
//! maps, a date, a regex, a lambda, and the objects a context lends -- doc
//! values, `ctx`, `params` -- which are maps with a method or two of their own.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use serde_json::Value as Json;

use super::ast::Stmt;

/// A map with insertion order, which is how Java's LinkedHashMap and JSON
/// objects both behave.
pub type MapRef = Rc<RefCell<Vec<(Value, Value)>>>;
pub type ListRef = Rc<RefCell<Vec<Value>>>;

#[derive(Clone)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Long(i64),
    Float(f64),
    Double(f64),
    Str(Rc<str>),
    List(ListRef),
    Map(MapRef),
    /// milliseconds since the epoch, in a zone named by an offset in seconds
    Date {
        millis: i64,
        offset_secs: i32,
    },
    Regex(Rc<regex::Regex>),
    Lambda(Rc<Lambda>),
    /// the values one document holds for a field: `doc['x']`
    DocValues(Rc<DocValues>),
    /// what a context lends by name and answers itself: `doc`, `params._source`
    Native(Rc<dyn NativeObject>),
    /// a string builder or another mutable holder of text
    Builder(Rc<RefCell<String>>),
    /// an exception a script threw
    Error(Rc<str>),
}

pub struct Lambda {
    pub params: Vec<String>,
    pub body: Vec<Stmt>,
    /// the variables visible where it was written
    pub captured: Vec<(String, Value)>,
    /// a reference to a method of the script, `this::f`, or of a class
    pub method: Option<(String, String)>,
}

/// The doc values of one field, as Painless reads them.
pub struct DocValues {
    pub field: String,
    pub values: Vec<Value>,
}

/// An object a context lends, answering field reads and calls itself.
pub trait NativeObject {
    fn get(&self, name: &str) -> Option<Value>;
    fn call(&self, name: &str, args: &[Value]) -> Option<Result<Value, String>>;
    fn describe(&self) -> String;
}

impl Value {
    pub fn str(s: &str) -> Value {
        Value::Str(Rc::from(s))
    }

    pub fn list(items: Vec<Value>) -> Value {
        Value::List(Rc::new(RefCell::new(items)))
    }

    pub fn map(pairs: Vec<(Value, Value)>) -> Value {
        Value::Map(Rc::new(RefCell::new(pairs)))
    }

    pub fn from_json(v: &Json) -> Value {
        match v {
            Json::Null => Value::Null,
            Json::Bool(b) => Value::Bool(*b),
            Json::Number(n) => match n.as_i64() {
                Some(i) if i >= i32::MIN as i64 && i <= i32::MAX as i64 => Value::Int(i),
                Some(i) => Value::Long(i),
                None => Value::Double(n.as_f64().unwrap_or(0.0)),
            },
            Json::String(s) => Value::str(s),
            Json::Array(a) => Value::list(a.iter().map(Value::from_json).collect()),
            Json::Object(o) => {
                Value::map(o.iter().map(|(k, v)| (Value::str(k), Value::from_json(v))).collect())
            }
        }
    }

    pub fn to_json(&self) -> Json {
        match self {
            Value::Null => Json::Null,
            Value::Bool(b) => Json::Bool(*b),
            Value::Int(i) | Value::Long(i) => Json::from(*i),
            Value::Float(f) | Value::Double(f) => match serde_json::Number::from_f64(*f) {
                Some(n) => Json::Number(n),
                None => Json::Null,
            },
            Value::Str(s) => Json::String(s.to_string()),
            Value::List(l) => Json::Array(l.borrow().iter().map(|v| v.to_json()).collect()),
            Value::Map(m) => {
                let mut o = serde_json::Map::new();
                for (k, v) in m.borrow().iter() {
                    o.insert(k.as_text(), v.to_json());
                }
                Json::Object(o)
            }
            Value::Date { .. } => Json::String(self.as_text()),
            Value::DocValues(d) => Json::Array(d.values.iter().map(|v| v.to_json()).collect()),
            Value::Builder(b) => Json::String(b.borrow().clone()),
            Value::Error(e) => Json::String(e.to_string()),
            Value::Regex(_) | Value::Lambda(_) | Value::Native(_) => Json::Null,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) | Value::Long(i) => Some(*i as f64),
            Value::Float(f) | Value::Double(f) => Some(*f),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Value::Str(s) => s.parse().ok(),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) | Value::Long(i) => Some(*i),
            Value::Float(f) | Value::Double(f) => Some(*f as i64),
            Value::Str(s) => s.parse().ok(),
            _ => None,
        }
    }

    pub fn is_number(&self) -> bool {
        matches!(self, Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_))
    }

    pub fn is_integral(&self) -> bool {
        matches!(self, Value::Int(_) | Value::Long(_))
    }

    pub fn truthy(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// How Java would print it.
    pub fn as_text(&self) -> String {
        match self {
            Value::Null => "null".into(),
            Value::Bool(b) => b.to_string(),
            Value::Int(i) | Value::Long(i) => i.to_string(),
            Value::Float(f) | Value::Double(f) => java_double(*f),
            Value::Str(s) => s.to_string(),
            Value::List(l) => {
                let items: Vec<String> = l.borrow().iter().map(|v| v.as_text()).collect();
                format!("[{}]", items.join(", "))
            }
            Value::Map(m) => {
                let items: Vec<String> = m
                    .borrow()
                    .iter()
                    .map(|(k, v)| format!("{}={}", k.as_text(), v.as_text()))
                    .collect();
                format!("{{{}}}", items.join(", "))
            }
            Value::Date { millis, offset_secs } => format_date(*millis, *offset_secs),
            Value::Regex(r) => r.as_str().to_string(),
            Value::Lambda(_) => "lambda".into(),
            Value::DocValues(d) => {
                let items: Vec<String> = d.values.iter().map(|v| v.as_text()).collect();
                format!("[{}]", items.join(", "))
            }
            Value::Native(n) => n.describe(),
            Value::Builder(b) => b.borrow().clone(),
            Value::Error(e) => e.to_string(),
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Int(_) => "int",
            Value::Long(_) => "long",
            Value::Float(_) => "float",
            Value::Double(_) => "double",
            Value::Str(_) => "String",
            Value::List(_) => "ArrayList",
            Value::Map(_) => "HashMap",
            Value::Date { .. } => "ZonedDateTime",
            Value::Regex(_) => "Pattern",
            Value::Lambda(_) => "lambda",
            Value::DocValues(_) => "ScriptDocValues",
            Value::Native(_) => "Object",
            Value::Builder(_) => "StringBuilder",
            Value::Error(_) => "Exception",
        }
    }

    /// Java's `equals`: numbers by value, text by text, the rest by content.
    pub fn equals(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Null, _) | (_, Value::Null) => false,
            (a, b) if a.is_number() && b.is_number() => {
                if a.is_integral() && b.is_integral() {
                    a.as_i64() == b.as_i64()
                } else {
                    a.as_f64() == b.as_f64()
                }
            }
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::List(a), Value::List(b)) => {
                let (a, b) = (a.borrow(), b.borrow());
                a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x.equals(y))
            }
            (Value::Map(a), Value::Map(b)) => {
                let (a, b) = (a.borrow(), b.borrow());
                a.len() == b.len()
                    && a.iter().all(|(k, v)| b.iter().any(|(k2, v2)| k.equals(k2) && v.equals(v2)))
            }
            (Value::Date { millis: a, .. }, Value::Date { millis: b, .. }) => a == b,
            (Value::Builder(a), Value::Builder(b)) => *a.borrow() == *b.borrow(),
            (Value::Str(a), Value::Builder(b)) | (Value::Builder(b), Value::Str(a)) => {
                a.as_ref() == b.borrow().as_str()
            }
            _ => false,
        }
    }
}

/// Java prints a double with a `.0` where it is whole, and in scientific
/// notation past 1e7 or below 1e-3.
pub fn java_double(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if f == f.trunc() && f.abs() < 1e7 {
        return format!("{f:.1}");
    }
    let a = f.abs();
    if (a >= 1e7 || a < 1e-3) && a != 0.0 {
        let s = format!("{f:E}");
        // Java writes 1.0E7, Rust 1E7
        let (mantissa, exp) = s.split_once('E').unwrap_or((&s, "0"));
        let mantissa =
            if mantissa.contains('.') { mantissa.to_string() } else { format!("{mantissa}.0") };
        return format!("{mantissa}E{exp}");
    }
    let s = format!("{f}");
    s
}

pub fn format_date(millis: i64, offset_secs: i32) -> String {
    let secs = millis.div_euclid(1000) + offset_secs as i64;
    let ms = millis.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let zone = if offset_secs == 0 {
        "Z".to_string()
    } else {
        let sign = if offset_secs < 0 { '-' } else { '+' };
        let o = offset_secs.abs();
        format!("{sign}{:02}:{:02}", o / 3600, (o % 3600) / 60)
    };
    if ms == 0 {
        format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}{zone}")
    } else {
        format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{ms:03}{zone}")
    }
}

/// Days since 1970-01-01 to a calendar date.
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The key a map is asked with, as text where the map is JSON-shaped.
pub fn map_get(map: &MapRef, key: &Value) -> Option<Value> {
    map.borrow().iter().find(|(k, _)| k.equals(key)).map(|(_, v)| v.clone())
}

pub fn map_put(map: &MapRef, key: Value, value: Value) -> Option<Value> {
    let mut m = map.borrow_mut();
    if let Some(slot) = m.iter_mut().find(|(k, _)| k.equals(&key)) {
        return Some(std::mem::replace(&mut slot.1, value));
    }
    m.push((key, value));
    None
}

pub fn map_remove(map: &MapRef, key: &Value) -> Option<Value> {
    let mut m = map.borrow_mut();
    let at = m.iter().position(|(k, _)| k.equals(key))?;
    Some(m.remove(at).1)
}

/// The order Java's `compareTo` gives two values.
pub fn compare(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (x, y) if x.is_number() && y.is_number() => {
            if x.is_integral() && y.is_integral() {
                Some(x.as_i64()?.cmp(&y.as_i64()?))
            } else {
                x.as_f64()?.partial_cmp(&y.as_f64()?)
            }
        }
        (Value::Str(x), Value::Str(y)) => Some(x.cmp(y)),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        (Value::Date { millis: x, .. }, Value::Date { millis: y, .. }) => Some(x.cmp(y)),
        _ => None,
    }
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_text())
    }
}

/// A convenience for building the map-shaped objects a context lends.
pub fn json_map(map: &BTreeMap<String, Json>) -> Value {
    Value::map(map.iter().map(|(k, v)| (Value::str(k), Value::from_json(v))).collect())
}
