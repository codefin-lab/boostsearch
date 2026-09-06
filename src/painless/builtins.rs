//! What the language does with each kind of value: the operators, the
//! methods, and the classes a script may name.
//!
//! This is the whitelist, written as the behaviour the suite exercises rather
//! than as a table of Java signatures: `Math`, `String`, `List`, `Map`, the
//! dates, `MovingFunctions`, and the doc values a document lends.

use std::cell::RefCell;
use std::rc::Rc;

use super::value::*;

pub type Callback<'a> =
    &'a mut dyn FnMut(&Rc<Lambda>, Vec<Value>) -> Result<Value, super::eval::Flow>;

fn no(m: String) -> Result<Value, String> {
    Err(m)
}

fn run_lambda(call: Callback<'_>, l: &Rc<Lambda>, args: Vec<Value>) -> Result<Value, String> {
    call(l, args).map_err(|f| match f {
        super::eval::Flow::Error(m, _) => m,
        super::eval::Flow::Throw(v) => v.as_text(),
        _ => "unexpected flow out of a lambda".to_string(),
    })
}

// ------------------------------------------------------------------ operators

pub fn unary(op: &str, v: &Value) -> Result<Value, String> {
    match (op, v) {
        ("!", Value::Bool(b)) => Ok(Value::Bool(!b)),
        ("-", Value::Int(i)) => Ok(Value::Int(-i)),
        ("-", Value::Long(i)) => Ok(Value::Long(-i)),
        ("-", Value::Float(f)) => Ok(Value::Float(-f)),
        ("-", Value::Double(f)) => Ok(Value::Double(-f)),
        ("+", x) if x.is_number() => Ok(x.clone()),
        ("~", Value::Int(i)) => Ok(Value::Int(!i)),
        ("~", Value::Long(i)) => Ok(Value::Long(!i)),
        ("!", Value::Null) | ("-", Value::Null) => no("cannot apply an operator to null".into()),
        _ => no(format!("cannot apply [{op}] to [{}]", v.type_name())),
    }
}

/// The type the two sides of an arithmetic operator meet at: Java promotes
/// to the wider of the two.
fn promoted(a: &Value, b: &Value) -> &'static str {
    let rank = |v: &Value| match v {
        Value::Double(_) => 4,
        Value::Float(_) => 3,
        Value::Long(_) => 2,
        Value::Int(_) | Value::Bool(_) => 1,
        _ => 0,
    };
    match rank(a).max(rank(b)) {
        4 => "double",
        3 => "float",
        2 => "long",
        _ => "int",
    }
}

pub fn binary(op: &str, a: &Value, b: &Value) -> Result<Value, String> {
    // `+` with a string on either side concatenates
    if op == "+" && (matches!(a, Value::Str(_)) || matches!(b, Value::Str(_))) {
        return Ok(Value::str(&format!("{}{}", a.as_text(), b.as_text())));
    }
    match op {
        "==" => return Ok(Value::Bool(a.equals(b))),
        "!=" => return Ok(Value::Bool(!a.equals(b))),
        "===" => return Ok(Value::Bool(a.equals(b) && a.type_name() == b.type_name())),
        "!==" => return Ok(Value::Bool(!(a.equals(b) && a.type_name() == b.type_name()))),
        "=~" | "==~" => {
            let Value::Regex(r) = b else { return no("the right side of =~ is a regex".into()) };
            let text = a.as_text();
            return Ok(Value::Bool(if op == "=~" {
                r.is_match(&text)
            } else {
                r.find(&text).map(|m| m.start() == 0 && m.end() == text.len()).unwrap_or(false)
            }));
        }
        "<" | ">" | "<=" | ">=" => {
            let Some(ord) = compare(a, b) else {
                return no(format!("cannot compare [{}] with [{}]", a.type_name(), b.type_name()));
            };
            use std::cmp::Ordering::*;
            return Ok(Value::Bool(match op {
                "<" => ord == Less,
                ">" => ord == Greater,
                "<=" => ord != Greater,
                _ => ord != Less,
            }));
        }
        _ => {}
    }
    if a.is_null() || b.is_null() {
        return no("cannot apply an arithmetic operator to null".into());
    }
    if let (Value::Bool(x), Value::Bool(y)) = (a, b) {
        return match op {
            "&" => Ok(Value::Bool(x & y)),
            "|" => Ok(Value::Bool(x | y)),
            "^" => Ok(Value::Bool(x ^ y)),
            _ => no(format!("cannot apply [{op}] to booleans")),
        };
    }
    if !a.is_number() || !b.is_number() {
        return no(format!("cannot apply [{op}] to [{}] and [{}]", a.type_name(), b.type_name()));
    }
    match promoted(a, b) {
        "int" | "long" => {
            let (x, y) = (a.as_i64().unwrap_or(0), b.as_i64().unwrap_or(0));
            let wide = promoted(a, b) == "long";
            let out = match op {
                "+" => x.wrapping_add(y),
                "-" => x.wrapping_sub(y),
                "*" => x.wrapping_mul(y),
                "/" => {
                    if y == 0 {
                        return no("/ by zero".into());
                    }
                    x.wrapping_div(y)
                }
                "%" => {
                    if y == 0 {
                        return no("/ by zero".into());
                    }
                    x.wrapping_rem(y)
                }
                "&" => x & y,
                "|" => x | y,
                "^" => x ^ y,
                "<<" => x.wrapping_shl(y as u32),
                ">>" => x.wrapping_shr(y as u32),
                ">>>" => ((x as u64).wrapping_shr(y as u32)) as i64,
                _ => return no(format!("unknown operator [{op}]")),
            };
            Ok(if wide { Value::Long(out) } else { Value::Int(out as i32 as i64) })
        }
        kind => {
            let (x, y) = (a.as_f64().unwrap_or(0.0), b.as_f64().unwrap_or(0.0));
            let out = match op {
                "+" => x + y,
                "-" => x - y,
                "*" => x * y,
                "/" => x / y,
                "%" => x % y,
                _ => return no(format!("cannot apply [{op}] to floating point numbers")),
            };
            Ok(if kind == "float" { Value::Float(out as f32 as f64) } else { Value::Double(out) })
        }
    }
}

pub fn cast(class: &str, v: Value) -> Result<Value, String> {
    Ok(match class {
        "int" | "Integer" | "short" | "byte" | "char" => match &v {
            x if x.is_number() => Value::Int(x.as_i64().unwrap_or(0) as i32 as i64),
            // a char is not a one-letter String: it is its own type, and
            // what a document may hold is decided by that difference
            Value::Str(s) if class == "char" => match s.chars().next() {
                Some(c) => Value::Char(c),
                None => return no("Cannot cast from [String] to [char].".to_string()),
            },
            _ => return no(format!("Cannot cast from [{}] to [{class}].", v.type_name())),
        },
        "long" | "Long" => match &v {
            x if x.is_number() => Value::Long(x.as_i64().unwrap_or(0)),
            _ => return no(format!("Cannot cast from [{}] to [long].", v.type_name())),
        },
        "float" | "Float" => match &v {
            x if x.is_number() => Value::Float(x.as_f64().unwrap_or(0.0) as f32 as f64),
            _ => return no(format!("Cannot cast from [{}] to [float].", v.type_name())),
        },
        "double" | "Double" | "Number" => match &v {
            x if x.is_number() => Value::Double(x.as_f64().unwrap_or(0.0)),
            _ => return no(format!("Cannot cast from [{}] to [double].", v.type_name())),
        },
        "boolean" | "Boolean" => match &v {
            Value::Bool(_) => v,
            _ => return no(format!("Cannot cast from [{}] to [boolean].", v.type_name())),
        },
        "String" | "CharSequence" => match &v {
            Value::Str(_) => v,
            Value::Null => Value::Null,
            other => Value::str(&other.as_text()),
        },
        _ => v,
    })
}

pub fn instance_of(v: &Value, class: &str) -> bool {
    match class {
        "String" | "CharSequence" => matches!(v, Value::Str(_)),
        "Integer" | "int" => matches!(v, Value::Int(_)),
        "Long" | "long" => matches!(v, Value::Long(_)),
        "Double" | "double" => matches!(v, Value::Double(_)),
        "Float" | "float" => matches!(v, Value::Float(_)),
        "Number" => v.is_number(),
        "Boolean" | "boolean" => matches!(v, Value::Bool(_)),
        "List" | "ArrayList" | "Collection" | "Iterable" => {
            matches!(v, Value::List(_) | Value::DocValues(_))
        }
        "Map" | "HashMap" => matches!(v, Value::Map(_)),
        "ZonedDateTime" => matches!(v, Value::Date { .. }),
        "Object" => !v.is_null(),
        _ => false,
    }
}

// ------------------------------------------------------------------ fields

pub fn get_field(t: &Value, name: &str) -> Result<Value, String> {
    match t {
        Value::Map(m) => Ok(map_get(m, &Value::str(name)).unwrap_or(Value::Null)),
        Value::Native(n) => n.get(name).ok_or_else(|| format!("unknown field [{name}]")),
        Value::DocValues(d) => match name {
            "value" => d.values.first().cloned().ok_or_else(|| {
                "A document doesn't have a value for a field! Use doc[<field>].size()==0 to check \
                 if a document is missing a field!"
                    .to_string()
            }),
            "values" => Ok(Value::list(d.values.clone())),
            "length" | "size" => Ok(Value::Int(d.values.len() as i64)),
            "empty" => Ok(Value::Bool(d.values.is_empty())),
            _ => no(format!("unknown field [{name}] on doc values")),
        },
        Value::List(l) => match name {
            "length" | "size" => Ok(Value::Int(l.borrow().len() as i64)),
            "empty" => Ok(Value::Bool(l.borrow().is_empty())),
            _ => no(format!("unknown field [{name}] on a list")),
        },
        Value::Str(s) => match name {
            "length" => Ok(Value::Int(s.chars().count() as i64)),
            "empty" => Ok(Value::Bool(s.is_empty())),
            _ => no(format!("unknown field [{name}] on a string")),
        },
        Value::Date { .. } => date_field(t, name),
        _ => no(format!("unknown field [{name}] on [{}]", t.type_name())),
    }
}

pub fn set_field(t: &Value, name: &str, value: Value) -> Result<(), String> {
    match t {
        Value::Map(m) => {
            map_put(m, Value::str(name), value);
            Ok(())
        }
        Value::Native(n) => match n.call("__set__", &[Value::str(name), value]) {
            Some(r) => r.map(|_| ()),
            None => Err(format!("cannot write [{name}] on [{}]", n.describe())),
        },
        _ => Err(format!("cannot write [{name}] on [{}]", t.type_name())),
    }
}

pub fn get_index(t: &Value, i: &Value) -> Result<Value, String> {
    match t {
        Value::List(l) => {
            let l = l.borrow();
            let Some(idx) = i.as_i64() else { return no("a list index is a number".into()) };
            let idx = if idx < 0 { l.len() as i64 + idx } else { idx };
            l.get(idx as usize)
                .cloned()
                .ok_or_else(|| format!("Index {idx} out of bounds for length {}", l.len()))
        }
        Value::DocValues(d) => {
            let Some(idx) = i.as_i64() else { return no("a list index is a number".into()) };
            d.values
                .get(idx as usize)
                .cloned()
                .ok_or_else(|| format!("Index {idx} out of bounds for length {}", d.values.len()))
        }
        Value::Map(m) => Ok(map_get(m, i).unwrap_or(Value::Null)),
        Value::Native(n) => match n.call("__index__", &[i.clone()]) {
            Some(r) => r,
            None => n.get(&i.as_text()).ok_or_else(|| format!("unknown key [{}]", i.as_text())),
        },
        Value::Str(s) => {
            let Some(idx) = i.as_i64() else { return no("a string index is a number".into()) };
            s.chars()
                .nth(idx as usize)
                .map(|c| Value::str(&c.to_string()))
                .ok_or_else(|| "index out of bounds".into())
        }
        Value::Null => no("cannot index a null value".into()),
        _ => no(format!("cannot index [{}]", t.type_name())),
    }
}

pub fn set_index(t: &Value, i: &Value, value: Value) -> Result<(), String> {
    match t {
        Value::List(l) => {
            let mut l = l.borrow_mut();
            let Some(idx) = i.as_i64() else { return Err("a list index is a number".into()) };
            let n = l.len();
            match l.get_mut(idx as usize) {
                Some(slot) => {
                    *slot = value;
                    Ok(())
                }
                None => Err(format!("Index {idx} out of bounds for length {n}")),
            }
        }
        Value::Map(m) => {
            map_put(m, i.clone(), value);
            Ok(())
        }
        Value::Native(n) => match n.call("__set__", &[i.clone(), value]) {
            Some(r) => r.map(|_| ()),
            None => Err("cannot write there".into()),
        },
        _ => Err(format!("cannot index [{}]", t.type_name())),
    }
}

// ------------------------------------------------------------------ methods

pub fn call_method(
    t: &Value,
    name: &str,
    args: &[Value],
    call: Callback<'_>,
) -> Result<Value, String> {
    match t {
        Value::Str(s) => string_method(s, name, args),
        Value::Builder(b) => builder_method(b, name, args),
        Value::List(l) => list_method(l, name, args, call),
        Value::DocValues(d) => doc_values_method(d, name, args, call),
        Value::Map(m) => map_method(m, name, args, call),
        Value::Date { .. } => date_method(t, name, args),
        Value::Regex(r) => match name {
            "matcher" => Ok(Value::map(vec![
                (Value::str("__pattern__"), Value::Regex(r.clone())),
                (Value::str("__text__"), args.first().cloned().unwrap_or(Value::Null)),
            ])),
            "pattern" => Ok(Value::str(r.as_str())),
            "split" => {
                let text = args.first().map(|a| a.as_text()).unwrap_or_default();
                Ok(Value::list(r.split(&text).map(Value::str).collect()))
            }
            _ => no(format!("unknown method [{name}] on Pattern")),
        },
        Value::Error(e) => match name {
            "getMessage" => Ok(Value::str(e)),
            _ => no(format!("unknown method [{name}] on an exception")),
        },
        Value::Native(n) => {
            n.call(name, args).unwrap_or_else(|| no(format!("unknown method [{name}]")))
        }
        x if x.is_number() => number_method(x, name, args),
        Value::Bool(b) => match name {
            "booleanValue" => Ok(Value::Bool(*b)),
            "toString" => Ok(Value::str(&b.to_string())),
            "equals" => Ok(Value::Bool(t.equals(args.first().unwrap_or(&Value::Null)))),
            "hashCode" => Ok(Value::Int(if *b { 1231 } else { 1237 })),
            "compareTo" => Ok(Value::Int(
                compare(t, args.first().unwrap_or(&Value::Null)).map(|o| o as i64).unwrap_or(0),
            )),
            _ => no(format!("unknown method [{name}] on boolean")),
        },
        Value::Null => no(format!("cannot invoke [{name}] on a null value")),
        _ => no(format!("unknown method [{name}] on [{}]", t.type_name())),
    }
}

/// Fold a stream's items the way a collector says: into a list, a count,
/// a joined string, a map, or groups that are themselves collected.
fn collect_with(items: Vec<Value>, collector: &Value, call: Callback<'_>) -> Result<Value, String> {
    let (kind, cargs): (String, Vec<Value>) = match collector {
        Value::Str(s) if s.as_ref() == "__counting__" => ("counting".into(), Vec::new()),
        Value::Str(_) | Value::Null => return Ok(Value::list(items)),
        Value::Map(m) => {
            let kind =
                map_get(m, &Value::str("__collector__")).map(|v| v.as_text()).unwrap_or_default();
            let cargs = match map_get(m, &Value::str("args")) {
                Some(Value::List(a)) => a.borrow().clone(),
                _ => Vec::new(),
            };
            if kind == "toMap" {
                let key = map_get(m, &Value::str("key")).unwrap_or(Value::Null);
                let value = map_get(m, &Value::str("value")).unwrap_or(Value::Null);
                ("toMap".into(), vec![key, value])
            } else {
                (kind, cargs)
            }
        }
        _ => return Ok(Value::list(items)),
    };
    fn apply_fn(call: Callback<'_>, f: &Value, v: &Value) -> Result<Value, String> {
        match f {
            Value::Lambda(lam) => run_lambda(call, lam, vec![v.clone()]),
            _ => Ok(v.clone()),
        }
    }
    let number_of = |v: &Value| v.as_f64().unwrap_or(0.0);
    Ok(match kind.as_str() {
        "counting" => Value::Long(items.len() as i64),
        "joining" => {
            let sep = cargs.first().map(|v| v.as_text()).unwrap_or_default();
            let prefix = cargs.get(1).map(|v| v.as_text()).unwrap_or_default();
            let suffix = cargs.get(2).map(|v| v.as_text()).unwrap_or_default();
            let joined: Vec<String> = items.iter().map(|v| v.as_text()).collect();
            Value::str(&format!("{prefix}{}{suffix}", joined.join(&sep)))
        }
        "toMap" => {
            let mut pairs: Vec<(Value, Value)> = Vec::new();
            for v in &items {
                let k = apply_fn(&mut *call, &cargs[0], v)?;
                let val = apply_fn(&mut *call, &cargs[1], v)?;
                if pairs.iter().any(|(pk, _)| pk.equals(&k)) {
                    return Err(format!("Duplicate key {}", k.as_text()));
                }
                pairs.push((k, val));
            }
            Value::map(pairs)
        }
        "groupingBy" | "partitioningBy" => {
            let classifier = cargs.first().cloned().unwrap_or(Value::Null);
            let downstream = cargs.get(1).cloned();
            let mut groups: Vec<(Value, Vec<Value>)> = if kind == "partitioningBy" {
                vec![(Value::Bool(false), Vec::new()), (Value::Bool(true), Vec::new())]
            } else {
                Vec::new()
            };
            for v in &items {
                let mut k = apply_fn(&mut *call, &classifier, v)?;
                if kind == "partitioningBy" {
                    k = Value::Bool(k.truthy().unwrap_or(false));
                }
                match groups.iter_mut().find(|(g, _)| g.equals(&k)) {
                    Some((_, list)) => list.push(v.clone()),
                    None => groups.push((k, vec![v.clone()])),
                }
            }
            let mut out = Vec::new();
            for (k, list) in groups {
                let folded = match &downstream {
                    Some(d) => collect_with(list, d, call)?,
                    None => Value::list(list),
                };
                out.push((k, folded));
            }
            Value::map(out)
        }
        "mapping" => {
            let mut mapped = Vec::new();
            for v in &items {
                mapped.push(apply_fn(&mut *call, &cargs[0], v)?);
            }
            match cargs.get(1) {
                Some(d) => collect_with(mapped, d, call)?,
                None => Value::list(mapped),
            }
        }
        "summingInt" | "summingLong" | "summingDouble" | "averagingInt" | "averagingLong"
        | "averagingDouble" => {
            let mut total = 0.0;
            for v in &items {
                total += number_of(&apply_fn(&mut *call, &cargs[0], v)?);
            }
            match kind.as_str() {
                "summingInt" => Value::Int(total as i64),
                "summingLong" => Value::Long(total as i64),
                "summingDouble" => Value::Double(total),
                _ => Value::Double(if items.is_empty() { 0.0 } else { total / items.len() as f64 }),
            }
        }
        "minBy" | "maxBy" => {
            let mut best: Option<Value> = None;
            for v in &items {
                best = Some(match best {
                    None => v.clone(),
                    Some(b) => {
                        let ord = match cargs.first() {
                            Some(Value::Lambda(lam)) => {
                                run_lambda(call, lam, vec![v.clone(), b.clone()])?
                                    .as_i64()
                                    .unwrap_or(0)
                            }
                            _ => compare(v, &b).map(|o| o as i64).unwrap_or(0),
                        };
                        if (kind == "minBy" && ord < 0) || (kind == "maxBy" && ord > 0) {
                            v.clone()
                        } else {
                            b
                        }
                    }
                });
            }
            optional_of(best.unwrap_or(Value::Null))
        }
        "reducing" => {
            // (identity, op) or (identity, mapper, op) or (op)
            let (seed, mapper, op) = match cargs.len() {
                1 => (None, None, cargs[0].clone()),
                2 => (Some(cargs[0].clone()), None, cargs[1].clone()),
                _ => (Some(cargs[0].clone()), Some(cargs[1].clone()), cargs[2].clone()),
            };
            let mut acc = seed.clone();
            for v in &items {
                let v = match &mapper {
                    Some(m) => apply_fn(&mut *call, m, v)?,
                    None => v.clone(),
                };
                acc = Some(match acc {
                    Some(a) => match &op {
                        Value::Lambda(lam) => run_lambda(call, lam, vec![a, v])?,
                        _ => v,
                    },
                    None => v,
                });
            }
            if seed.is_none() {
                optional_of(acc.unwrap_or(Value::Null))
            } else {
                acc.unwrap_or(Value::Null)
            }
        }
        "collectingAndThen" => {
            let inner = collect_with(items, &cargs[0], call)?;
            apply_fn(&mut *call, &cargs.get(1).cloned().unwrap_or(Value::Null), &inner)?
        }
        "summarizingInt" | "summarizingLong" | "summarizingDouble" => {
            let mut nums = Vec::new();
            for v in &items {
                nums.push(number_of(&apply_fn(&mut *call, &cargs[0], v)?));
            }
            let sum: f64 = nums.iter().sum();
            let n = nums.len() as f64;
            let whole = kind != "summarizingDouble";
            let num = |x: f64| if whole { Value::Long(x as i64) } else { Value::Double(x) };
            Value::map(vec![
                (Value::str("count"), Value::Long(nums.len() as i64)),
                (Value::str("sum"), num(sum)),
                (Value::str("min"), num(nums.iter().cloned().fold(f64::INFINITY, f64::min))),
                (Value::str("max"), num(nums.iter().cloned().fold(f64::NEG_INFINITY, f64::max))),
                (Value::str("average"), Value::Double(if n == 0.0 { 0.0 } else { sum / n })),
            ])
        }
        _ => Value::list(items),
    })
}

/// An optional is a list holding the value, or nothing.
fn optional_of(v: Value) -> Value {
    Value::list(if v.is_null() { Vec::new() } else { vec![v] })
}

/// An iterator is a list with a cursor kept at its end, under a marker.
fn iterator_of(items: Vec<Value>) -> Value {
    Value::map(vec![
        (Value::str("__iter__"), Value::list(items)),
        (Value::str("__at__"), Value::Int(0)),
    ])
}

fn iterator_step(
    l: &ListRef,
    name: &str,
    args: &[Value],
    call: Callback<'_>,
) -> Result<Value, String> {
    // a bare list asked to iterate is walked from its start each time
    let items = l.borrow().clone();
    Ok(match name {
        "hasNext" => Value::Bool(!items.is_empty()),
        "next" => match items.first() {
            Some(v) => {
                l.borrow_mut().remove(0);
                v.clone()
            }
            None => return no("No such element".into()),
        },
        _ => {
            if let Value::Lambda(lam) = arg(args, 0) {
                for v in items {
                    run_lambda(call, lam, vec![v])?;
                }
            }
            l.borrow_mut().clear();
            Value::Null
        }
    })
}

fn arg<'a>(args: &'a [Value], i: usize) -> &'a Value {
    args.get(i).unwrap_or(&Value::Null)
}

fn number_method(v: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    match name {
        "intValue" => Ok(Value::Int(v.as_i64().unwrap_or(0) as i32 as i64)),
        "longValue" => Ok(Value::Long(v.as_i64().unwrap_or(0))),
        "doubleValue" => Ok(Value::Double(v.as_f64().unwrap_or(0.0))),
        "floatValue" => Ok(Value::Float(v.as_f64().unwrap_or(0.0) as f32 as f64)),
        "shortValue" | "byteValue" => Ok(Value::Int(v.as_i64().unwrap_or(0))),
        "toString" => Ok(Value::str(&v.as_text())),
        "equals" => Ok(Value::Bool(v.equals(arg(args, 0)))),
        "compareTo" => Ok(Value::Int(compare(v, arg(args, 0)).map(|o| o as i64).unwrap_or(0))),
        "hashCode" => Ok(Value::Int(v.as_i64().unwrap_or(0))),
        "isNaN" => Ok(Value::Bool(v.as_f64().map(|f| f.is_nan()).unwrap_or(false))),
        "isInfinite" => Ok(Value::Bool(v.as_f64().map(|f| f.is_infinite()).unwrap_or(false))),
        _ => no(format!("unknown method [{name}] on [{}]", v.type_name())),
    }
}

fn string_method(s: &Rc<str>, name: &str, args: &[Value]) -> Result<Value, String> {
    let text: &str = s;
    let chars: Vec<char> = text.chars().collect();
    let at = |i: usize| arg(args, i).as_i64().unwrap_or(0).max(0) as usize;
    Ok(match name {
        "length" => Value::Int(chars.len() as i64),
        "codePointAt" | "codePointBefore" => {
            let i = if name == "codePointAt" { at(0) } else { at(0).saturating_sub(1) };
            match chars.get(i) {
                Some(c) => Value::Int(*c as i64),
                None => return no(format!("String index out of range: {i}")),
            }
        }
        "codePointCount" => Value::Int((at(1).min(chars.len())).saturating_sub(at(0)) as i64),
        "codePoints" | "chars" => {
            Value::list(chars.iter().map(|c| Value::Int(*c as i64)).collect())
        }
        "offsetByCodePoints" => Value::Int((at(0) as i64) + arg(args, 1).as_i64().unwrap_or(0)),
        "compareToIgnoreCase" => {
            Value::Int(text.to_lowercase().cmp(&arg(args, 0).as_text().to_lowercase()) as i64)
        }
        "contentEquals" => Value::Bool(arg(args, 0).as_text() == text),
        "subSequence" => {
            let (a, b) = (at(0), at(1).min(chars.len()));
            Value::str(&chars[a.min(b)..b].iter().collect::<String>())
        }
        "regionMatches" => {
            // (ignoreCase?, toffset, other, ooffset, len)
            let (ignore, shift) = match arg(args, 0) {
                Value::Bool(b) => (*b, 1),
                _ => (false, 0),
            };
            let toffset = arg(args, shift).as_i64().unwrap_or(0);
            let other: Vec<char> = arg(args, shift + 1).as_text().chars().collect();
            let ooffset = arg(args, shift + 2).as_i64().unwrap_or(0);
            let len = arg(args, shift + 3).as_i64().unwrap_or(0);
            if toffset < 0 || ooffset < 0 || len < 0 {
                Value::Bool(false)
            } else {
                let (t, o, n) = (toffset as usize, ooffset as usize, len as usize);
                if t + n > chars.len() || o + n > other.len() {
                    Value::Bool(false)
                } else {
                    let same = (0..n).all(|k| {
                        let (x, y) = (chars[t + k], other[o + k]);
                        if ignore { x.to_lowercase().eq(y.to_lowercase()) } else { x == y }
                    });
                    Value::Bool(same)
                }
            }
        }
        "getChars" => {
            // (srcBegin, srcEnd, dst, dstBegin)
            if let Value::List(dst) = arg(args, 2) {
                let (a, b, start) = (at(0), at(1).min(chars.len()), at(3));
                let mut d = dst.borrow_mut();
                for (k, c) in chars[a.min(b)..b].iter().enumerate() {
                    if let Some(slot) = d.get_mut(start + k) {
                        *slot = Value::str(&c.to_string());
                    }
                }
            }
            Value::Null
        }
        "isEmpty" => Value::Bool(text.is_empty()),
        "toLowerCase" => Value::str(&text.to_lowercase()),
        "toUpperCase" => Value::str(&text.to_uppercase()),
        "trim" | "strip" => Value::str(text.trim()),
        "contains" => Value::Bool(text.contains(arg(args, 0).as_text().as_str())),
        "startsWith" => Value::Bool(text.starts_with(arg(args, 0).as_text().as_str())),
        "endsWith" => Value::Bool(text.ends_with(arg(args, 0).as_text().as_str())),
        "equals" => Value::Bool(matches!(arg(args, 0), Value::Str(o) if o == s)),
        "equalsIgnoreCase" => Value::Bool(arg(args, 0).as_text().eq_ignore_ascii_case(text)),
        "compareTo" => Value::Int(text.cmp(arg(args, 0).as_text().as_str()) as i64),
        "indexOf" => {
            let want = arg(args, 0).as_text();
            let from = args.get(1).and_then(|a| a.as_i64()).unwrap_or(0).max(0) as usize;
            let chars: Vec<char> = text.chars().collect();
            let hay: String = chars.iter().skip(from).collect();
            Value::Int(
                hay.find(want.as_str())
                    .map(|b| (hay[..b].chars().count() + from) as i64)
                    .unwrap_or(-1),
            )
        }
        "lastIndexOf" => {
            let want = arg(args, 0).as_text();
            Value::Int(
                text.rfind(want.as_str()).map(|b| text[..b].chars().count() as i64).unwrap_or(-1),
            )
        }
        "charAt" => {
            let i = arg(args, 0).as_i64().unwrap_or(0);
            match text.chars().nth(i as usize) {
                Some(c) => Value::str(&c.to_string()),
                None => return no(format!("String index out of range: {i}")),
            }
        }
        "substring" => {
            let chars: Vec<char> = text.chars().collect();
            let from = arg(args, 0).as_i64().unwrap_or(0).max(0) as usize;
            let to =
                args.get(1).and_then(|a| a.as_i64()).map(|t| t as usize).unwrap_or(chars.len());
            if from > to || to > chars.len() {
                return no(format!("begin {from}, end {to}, length {}", chars.len()));
            }
            Value::str(&chars[from..to].iter().collect::<String>())
        }
        "replace" => Value::str(
            &text.replace(arg(args, 0).as_text().as_str(), arg(args, 1).as_text().as_str()),
        ),
        "replaceAll" => {
            let re = regex::Regex::new(&arg(args, 0).as_text()).map_err(|e| e.to_string())?;
            let rep = arg(args, 1).as_text().replace('$', "$$");
            let rep =
                regex::Regex::new(r"\$\$(\d)").unwrap().replace_all(&rep, "$${$1}").into_owned();
            Value::str(&re.replace_all(text, rep.as_str()))
        }
        "replaceFirst" => {
            let re = regex::Regex::new(&arg(args, 0).as_text()).map_err(|e| e.to_string())?;
            Value::str(&re.replace(text, arg(args, 1).as_text().as_str()))
        }
        "split" => {
            let sep = arg(args, 0).as_text();
            let re = regex::Regex::new(&sep).map_err(|e| e.to_string())?;
            let mut parts: Vec<&str> = re.split(text).collect();
            while parts.last().map(|p| p.is_empty()).unwrap_or(false) {
                parts.pop();
            }
            Value::list(parts.into_iter().map(Value::str).collect())
        }
        "splitOnToken" => {
            let sep = arg(args, 0).as_text();
            Value::list(text.split(sep.as_str()).map(Value::str).collect())
        }
        "concat" => Value::str(&format!("{text}{}", arg(args, 0).as_text())),
        "toString" | "intern" => Value::str(text),
        "hashCode" => Value::Int(java_string_hash(text)),
        "toCharArray" | "chars" => {
            Value::list(text.chars().map(|c| Value::str(&c.to_string())).collect())
        }
        "matches" => {
            let re = regex::Regex::new(&format!("^(?:{})$", arg(args, 0).as_text()))
                .map_err(|e| e.to_string())?;
            Value::Bool(re.is_match(text))
        }
        "sha1" => Value::str(&hex_digest(text, "sha1")),
        "sha256" => Value::str(&hex_digest(text, "sha256")),
        "encodeBase64" => Value::str(&base64_encode(text.as_bytes())),
        "decodeBase64" => Value::str(&String::from_utf8_lossy(&base64_decode(text))),
        "getBytes" => Value::list(text.bytes().map(|b| Value::Int(b as i64)).collect()),
        "utf8ToString" => Value::str(text),
        "repeat" => Value::str(&text.repeat(arg(args, 0).as_i64().unwrap_or(0).max(0) as usize)),
        "codePointAt" => Value::Int(
            text.chars()
                .nth(arg(args, 0).as_i64().unwrap_or(0) as usize)
                .map(|c| c as i64)
                .unwrap_or(0),
        ),
        "isBlank" => Value::Bool(text.trim().is_empty()),
        _ => return no(format!("unknown method [{name}] on String")),
    })
}

fn builder_method(b: &Rc<RefCell<String>>, name: &str, args: &[Value]) -> Result<Value, String> {
    let at = |i: usize| arg(args, i).as_i64().unwrap_or(0).max(0) as usize;
    match name {
        "delete" | "deleteCharAt" | "setCharAt" | "appendCodePoint" => {
            let mut text = b.borrow_mut();
            let mut chars: Vec<char> = text.chars().collect();
            match name {
                "delete" => {
                    let (a, e) = (at(0).min(chars.len()), at(1).min(chars.len()));
                    if a < e {
                        chars.drain(a..e);
                    }
                }
                "deleteCharAt" => {
                    let i = at(0);
                    if i < chars.len() {
                        chars.remove(i);
                    }
                }
                "setCharAt" => {
                    let i = at(0);
                    if let (true, Some(c)) =
                        (i < chars.len(), arg(args, 1).as_text().chars().next())
                    {
                        chars[i] = c;
                    }
                }
                _ => {
                    if let Some(c) = char::from_u32(at(0) as u32) {
                        chars.push(c);
                    }
                }
            }
            *text = chars.into_iter().collect();
            drop(text);
            return Ok(Value::Builder(b.clone()));
        }
        "capacity" => return Ok(Value::Int((b.borrow().chars().count() + 16) as i64)),
        "subSequence" | "codePointAt" | "codePointBefore" | "codePointCount" | "codePoints"
        | "offsetByCodePoints" | "getChars" => {
            let snapshot: Rc<str> = Rc::from(b.borrow().as_str());
            return string_method(&snapshot, name, args);
        }
        _ => {}
    }
    match name {
        "append" => {
            b.borrow_mut().push_str(&arg(args, 0).as_text());
            Ok(Value::Builder(b.clone()))
        }
        "toString" => Ok(Value::str(&b.borrow())),
        "length" => Ok(Value::Int(b.borrow().chars().count() as i64)),
        "insert" => {
            let at = arg(args, 0).as_i64().unwrap_or(0) as usize;
            let mut s = b.borrow_mut();
            let byte = s.char_indices().nth(at).map(|(i, _)| i).unwrap_or(s.len());
            s.insert_str(byte, &arg(args, 1).as_text());
            Ok(Value::Builder(b.clone()))
        }
        "reverse" => {
            let r: String = b.borrow().chars().rev().collect();
            *b.borrow_mut() = r;
            Ok(Value::Builder(b.clone()))
        }
        "setLength" => {
            let n = arg(args, 0).as_i64().unwrap_or(0) as usize;
            let mut s = b.borrow_mut();
            let byte = s.char_indices().nth(n).map(|(i, _)| i).unwrap_or(s.len());
            s.truncate(byte);
            Ok(Value::Null)
        }
        _ => string_method(&Rc::from(b.borrow().as_str()), name, args),
    }
}

fn list_method(
    l: &ListRef,
    name: &str,
    args: &[Value],
    call: Callback<'_>,
) -> Result<Value, String> {
    Ok(match name {
        "size" | "length" => Value::Int(l.borrow().len() as i64),
        "isEmpty" => Value::Bool(l.borrow().is_empty()),
        "get" => {
            let i = arg(args, 0).as_i64().unwrap_or(0);
            let list = l.borrow();
            match list.get(i as usize) {
                Some(v) => v.clone(),
                None => return no(format!("Index {i} out of bounds for length {}", list.len())),
            }
        }
        "add" => {
            if args.len() == 2 {
                let i = arg(args, 0).as_i64().unwrap_or(0) as usize;
                l.borrow_mut().insert(i, arg(args, 1).clone());
            } else {
                l.borrow_mut().push(arg(args, 0).clone());
            }
            Value::Bool(true)
        }
        "addAll" => {
            let more = match arg(args, 0) {
                Value::List(o) => o.borrow().clone(),
                Value::DocValues(d) => d.values.clone(),
                _ => Vec::new(),
            };
            l.borrow_mut().extend(more);
            Value::Bool(true)
        }
        "set" => {
            let i = arg(args, 0).as_i64().unwrap_or(0) as usize;
            let mut list = l.borrow_mut();
            let n = list.len();
            match list.get_mut(i) {
                Some(slot) => std::mem::replace(slot, arg(args, 1).clone()),
                None => return no(format!("Index {i} out of bounds for length {n}")),
            }
        }
        "remove" => {
            let mut list = l.borrow_mut();
            match arg(args, 0) {
                Value::Int(i) => {
                    let i = *i as usize;
                    if i >= list.len() {
                        return no(format!("Index {i} out of bounds for length {}", list.len()));
                    }
                    list.remove(i)
                }
                other => match list.iter().position(|v| v.equals(other)) {
                    Some(i) => {
                        list.remove(i);
                        Value::Bool(true)
                    }
                    None => Value::Bool(false),
                },
            }
        }
        "clear" => {
            l.borrow_mut().clear();
            Value::Null
        }
        "contains" => Value::Bool(l.borrow().iter().any(|v| v.equals(arg(args, 0)))),
        "indexOf" => Value::Int(
            l.borrow().iter().position(|v| v.equals(arg(args, 0))).map(|i| i as i64).unwrap_or(-1),
        ),
        "lastIndexOf" => Value::Int(
            l.borrow().iter().rposition(|v| v.equals(arg(args, 0))).map(|i| i as i64).unwrap_or(-1),
        ),
        "toString" => Value::str(&Value::List(l.clone()).as_text()),
        "equals" => Value::Bool(Value::List(l.clone()).equals(arg(args, 0))),
        "hashCode" => Value::Int(l.borrow().len() as i64),
        "iterator" | "stream" | "toArray" | "subList" | "asList" => {
            if name == "subList" {
                let from = arg(args, 0).as_i64().unwrap_or(0) as usize;
                let to = arg(args, 1).as_i64().unwrap_or(0) as usize;
                let list = l.borrow();
                Value::list(list.get(from..to.min(list.len())).unwrap_or(&[]).to_vec())
            } else {
                Value::List(l.clone())
            }
        }
        "sort" => {
            let mut list = l.borrow().clone();
            if let Value::Lambda(cmp) = arg(args, 0) {
                let mut err = None;
                list.sort_by(|a, b| {
                    if err.is_some() {
                        return std::cmp::Ordering::Equal;
                    }
                    match run_lambda(call, cmp, vec![a.clone(), b.clone()]) {
                        Ok(v) => v.as_i64().unwrap_or(0).cmp(&0),
                        Err(m) => {
                            err = Some(m);
                            std::cmp::Ordering::Equal
                        }
                    }
                });
                if let Some(m) = err {
                    return no(m);
                }
            } else {
                list.sort_by(|a, b| compare(a, b).unwrap_or(std::cmp::Ordering::Equal));
            }
            *l.borrow_mut() = list;
            Value::Null
        }
        "forEach" => {
            let Value::Lambda(f) = arg(args, 0) else { return no("forEach takes a lambda".into()) };
            for v in l.borrow().clone() {
                run_lambda(call, f, vec![v])?;
            }
            Value::Null
        }
        "map" => {
            let Value::Lambda(f) = arg(args, 0) else { return no("map takes a lambda".into()) };
            let mut out = Vec::new();
            for v in l.borrow().clone() {
                out.push(run_lambda(call, f, vec![v])?);
            }
            Value::list(out)
        }
        "filter" => {
            let Value::Lambda(f) = arg(args, 0) else { return no("filter takes a lambda".into()) };
            let mut out = Vec::new();
            for v in l.borrow().clone() {
                if matches!(run_lambda(call, f, vec![v.clone()])?, Value::Bool(true)) {
                    out.push(v);
                }
            }
            Value::list(out)
        }
        "removeIf" => {
            let Value::Lambda(f) = arg(args, 0) else {
                return no("removeIf takes a lambda".into());
            };
            let mut kept = Vec::new();
            let mut removed = false;
            for v in l.borrow().clone() {
                if matches!(run_lambda(call, f, vec![v.clone()])?, Value::Bool(true)) {
                    removed = true;
                } else {
                    kept.push(v);
                }
            }
            *l.borrow_mut() = kept;
            Value::Bool(removed)
        }
        "toList" => Value::List(l.clone()),
        "collect" => {
            let items = l.borrow().clone();
            collect_with(items, arg(args, 0), call)?
        }
        "count" => Value::Long(l.borrow().len() as i64),
        "sum" => {
            let list = l.borrow();
            if list.iter().all(|v| v.is_integral()) {
                Value::Long(list.iter().map(|v| v.as_i64().unwrap_or(0)).sum())
            } else {
                Value::Double(list.iter().map(|v| v.as_f64().unwrap_or(0.0)).sum())
            }
        }
        "max" | "min" => {
            let list = l.borrow();
            let mut best: Option<Value> = None;
            for v in list.iter() {
                best = match best {
                    None => Some(v.clone()),
                    Some(b) => {
                        let ord = compare(v, &b).unwrap_or(std::cmp::Ordering::Equal);
                        if (name == "max" && ord == std::cmp::Ordering::Greater)
                            || (name == "min" && ord == std::cmp::Ordering::Less)
                        {
                            Some(v.clone())
                        } else {
                            Some(b)
                        }
                    }
                };
            }
            best.unwrap_or(Value::Null)
        }
        "mapToDouble" | "mapToInt" | "mapToLong" => {
            let Value::Lambda(f) = arg(args, 0) else { return no("map takes a lambda".into()) };
            let mut out = Vec::new();
            for v in l.borrow().clone() {
                out.push(run_lambda(call, f, vec![v])?);
            }
            Value::list(out)
        }
        "average" => {
            let list = l.borrow();
            if list.is_empty() {
                Value::Null
            } else {
                Value::Double(
                    list.iter().map(|v| v.as_f64().unwrap_or(0.0)).sum::<f64>() / list.len() as f64,
                )
            }
        }
        "join" | "joining" => {
            let sep = arg(args, 0).as_text();
            Value::str(&l.borrow().iter().map(|v| v.as_text()).collect::<Vec<_>>().join(&sep))
        }
        "containsAll" => {
            let list = l.borrow();
            let other = match arg(args, 0) {
                Value::List(o) => o.borrow().clone(),
                _ => Vec::new(),
            };
            Value::Bool(other.iter().all(|x| list.iter().any(|y| y.equals(x))))
        }
        "removeAll" | "retainAll" => {
            let other = match arg(args, 0) {
                Value::List(o) => o.borrow().clone(),
                _ => Vec::new(),
            };
            let mut list = l.borrow_mut();
            let before = list.len();
            let keep_found = name == "retainAll";
            list.retain(|x| other.iter().any(|y| y.equals(x)) == keep_found);
            Value::Bool(list.len() != before)
        }
        "any" | "every" | "each" | "eachWithIndex" | "findAll" | "findResult" | "findResults"
        | "groupBy" | "allMatch" | "anyMatch" | "noneMatch" | "peek" | "forEachOrdered"
        | "flatMap" | "flatMapToInt" | "flatMapToLong" | "flatMapToDouble" | "reduce"
        | "findFirst" | "findAny" | "distinct" | "limit" | "skip" | "sorted" | "listIterator"
        | "spliterator" | "asCollection" | "sequential" | "unordered" | "close" | "isParallel"
        | "getByPath" => {
            let items = l.borrow().clone();
            let mut f = |v: &Value| -> Result<Value, String> {
                match arg(args, 0) {
                    Value::Lambda(lam) => run_lambda(call, lam, vec![v.clone()]),
                    _ => Ok(Value::Null),
                }
            };
            let mut truthy =
                |v: &Value| -> Result<bool, String> { Ok(f(v)?.truthy().unwrap_or(false)) };
            match name {
                "any" | "anyMatch" => {
                    for v in &items {
                        if truthy(v)? {
                            return Ok(Value::Bool(true));
                        }
                    }
                    Value::Bool(false)
                }
                "every" | "allMatch" => {
                    for v in &items {
                        if !truthy(v)? {
                            return Ok(Value::Bool(false));
                        }
                    }
                    Value::Bool(true)
                }
                "noneMatch" => {
                    for v in &items {
                        if truthy(v)? {
                            return Ok(Value::Bool(false));
                        }
                    }
                    Value::Bool(true)
                }
                "each" | "peek" | "forEachOrdered" => {
                    for v in &items {
                        f(v)?;
                    }
                    if name == "each" { Value::List(l.clone()) } else { Value::list(items) }
                }
                "eachWithIndex" => {
                    if let Value::Lambda(lam) = arg(args, 0) {
                        for (i, v) in items.iter().enumerate() {
                            run_lambda(call, lam, vec![v.clone(), Value::Int(i as i64)])?;
                        }
                    }
                    Value::List(l.clone())
                }
                "findAll" => {
                    let mut out = Vec::new();
                    for v in &items {
                        if truthy(v)? {
                            out.push(v.clone());
                        }
                    }
                    Value::list(out)
                }
                "findResult" => {
                    // the first non-null answer, else the default given first
                    let (fallback, lam) = match (arg(args, 0), args.get(1)) {
                        (d, Some(Value::Lambda(lam))) => (d.clone(), Some(lam.clone())),
                        (Value::Lambda(lam), _) => (Value::Null, Some(lam.clone())),
                        _ => (Value::Null, None),
                    };
                    let Some(lam) = lam else { return Ok(fallback) };
                    for v in &items {
                        let made = run_lambda(call, &lam, vec![v.clone()])?;
                        if !made.is_null() {
                            return Ok(made);
                        }
                    }
                    fallback
                }
                "findResults" => {
                    let mut out = Vec::new();
                    for v in &items {
                        let made = f(v)?;
                        if !made.is_null() {
                            out.push(made);
                        }
                    }
                    Value::list(out)
                }
                "groupBy" => {
                    let mut groups: Vec<(Value, Value)> = Vec::new();
                    for v in &items {
                        let key = f(v)?;
                        match groups.iter().find(|(k, _)| k.equals(&key)) {
                            Some((_, Value::List(g))) => g.borrow_mut().push(v.clone()),
                            _ => groups.push((key, Value::list(vec![v.clone()]))),
                        }
                    }
                    Value::map(groups)
                }
                "flatMap" | "flatMapToInt" | "flatMapToLong" | "flatMapToDouble" => {
                    let mut out = Vec::new();
                    for v in &items {
                        match f(v)? {
                            Value::List(inner) => out.extend(inner.borrow().iter().cloned()),
                            other => out.push(other),
                        }
                    }
                    Value::list(out)
                }
                "reduce" => {
                    // (identity, op) or (op) with an optional answer
                    let (mut acc, lam, bare) = match (arg(args, 0), args.get(1)) {
                        (seed, Some(Value::Lambda(lam))) => {
                            (Some(seed.clone()), Some(lam.clone()), false)
                        }
                        (Value::Lambda(lam), _) => (None, Some(lam.clone()), true),
                        _ => (None, None, true),
                    };
                    if let Some(lam) = lam {
                        for v in &items {
                            acc = Some(match acc {
                                Some(a) => run_lambda(call, &lam, vec![a, v.clone()])?,
                                None => v.clone(),
                            });
                        }
                    }
                    if bare {
                        optional_of(acc.unwrap_or(Value::Null))
                    } else {
                        acc.unwrap_or(Value::Null)
                    }
                }
                "findFirst" | "findAny" => {
                    optional_of(items.first().cloned().unwrap_or(Value::Null))
                }
                "distinct" => {
                    let mut out: Vec<Value> = Vec::new();
                    for v in items {
                        if !out.iter().any(|x| x.equals(&v)) {
                            out.push(v);
                        }
                    }
                    Value::list(out)
                }
                "limit" => {
                    let n = arg(args, 0).as_i64().unwrap_or(0).max(0) as usize;
                    Value::list(items.into_iter().take(n).collect())
                }
                "skip" => {
                    let n = arg(args, 0).as_i64().unwrap_or(0).max(0) as usize;
                    Value::list(items.into_iter().skip(n).collect())
                }
                "sorted" => {
                    let mut out = items;
                    match arg(args, 0) {
                        Value::Lambda(lam) => {
                            let mut failed = None;
                            out.sort_by(|a, b| {
                                match run_lambda(call, lam, vec![a.clone(), b.clone()]) {
                                    Ok(v) => match v.as_i64().unwrap_or(0) {
                                        n if n < 0 => std::cmp::Ordering::Less,
                                        0 => std::cmp::Ordering::Equal,
                                        _ => std::cmp::Ordering::Greater,
                                    },
                                    Err(e) => {
                                        failed = Some(e);
                                        std::cmp::Ordering::Equal
                                    }
                                }
                            });
                            if let Some(e) = failed {
                                return Err(e);
                            }
                        }
                        _ => out.sort_by(|a, b| compare(a, b).unwrap_or(std::cmp::Ordering::Equal)),
                    }
                    Value::list(out)
                }
                "listIterator" | "spliterator" => iterator_of(items),
                "getByPath" => {
                    let path = arg(args, 0).as_text();
                    let mut cur = Value::List(l.clone());
                    for part in path.split('.') {
                        cur = match &cur {
                            Value::Map(m) => map_get(m, &Value::str(part)).unwrap_or(Value::Null),
                            Value::List(inner) => match part.parse::<usize>() {
                                Ok(i) => inner.borrow().get(i).cloned().unwrap_or(Value::Null),
                                Err(_) => Value::Null,
                            },
                            _ => Value::Null,
                        };
                    }
                    cur
                }
                "isParallel" => Value::Bool(false),
                "close" => Value::Null,
                _ => Value::List(l.clone()),
            }
        }
        "hasNext" | "next" | "forEachRemaining" => {
            // a list stands as its own iterator, walked by a cursor kept
            // beside it
            return iterator_step(l, name, args, call);
        }
        "isPresent" | "isEmpty_" | "orElse" | "orElseGet" | "orElseThrow" | "ifPresent"
        | "ofNullable_" => {
            // an optional is a list of at most one value
            let held = l.borrow().first().cloned().filter(|v| !v.is_null());
            match name {
                "isPresent" => Value::Bool(held.is_some()),
                "orElse" => held.unwrap_or_else(|| arg(args, 0).clone()),
                "orElseGet" => match held {
                    Some(v) => v,
                    None => match arg(args, 0) {
                        Value::Lambda(lam) => run_lambda(call, lam, vec![])?,
                        other => other.clone(),
                    },
                },
                "orElseThrow" => match held {
                    Some(v) => v,
                    None => return no("No value present".into()),
                },
                "ifPresent" => {
                    if let (Some(v), Value::Lambda(lam)) = (held, arg(args, 0)) {
                        run_lambda(call, lam, vec![v])?;
                    }
                    Value::Null
                }
                _ => Value::Null,
            }
        }
        "reverse" => {
            l.borrow_mut().reverse();
            Value::Null
        }
        "getLength" => Value::Int(l.borrow().len() as i64),
        _ => return no(format!("unknown method [{name}] on a list")),
    })
}

fn doc_values_method(
    d: &Rc<DocValues>,
    name: &str,
    args: &[Value],
    call: Callback<'_>,
) -> Result<Value, String> {
    match name {
        "getValue" => Ok(d.values.first().cloned().unwrap_or(Value::Null)),
        "getValues" => Ok(Value::list(d.values.clone())),
        "size" | "length" => Ok(Value::Int(d.values.len() as i64)),
        "isEmpty" => Ok(Value::Bool(d.values.is_empty())),
        "get" => {
            let i = arg(args, 0).as_i64().unwrap_or(0);
            d.values
                .get(i as usize)
                .cloned()
                .ok_or_else(|| format!("Index {i} out of bounds for length {}", d.values.len()))
        }
        _ => list_method(&Rc::new(RefCell::new(d.values.clone())), name, args, call),
    }
}

fn map_method(m: &MapRef, name: &str, args: &[Value], call: Callback<'_>) -> Result<Value, String> {
    Ok(match name {
        "get" => map_get(m, arg(args, 0)).unwrap_or(Value::Null),
        "getOrDefault" => map_get(m, arg(args, 0)).unwrap_or_else(|| arg(args, 1).clone()),
        "put" => map_put(m, arg(args, 0).clone(), arg(args, 1).clone()).unwrap_or(Value::Null),
        "putIfAbsent" => match map_get(m, arg(args, 0)) {
            Some(v) => v,
            None => {
                map_put(m, arg(args, 0).clone(), arg(args, 1).clone());
                Value::Null
            }
        },
        "putAll" => {
            if let Value::Map(o) = arg(args, 0) {
                for (k, v) in o.borrow().clone() {
                    map_put(m, k, v);
                }
            }
            Value::Null
        }
        "remove" => map_remove(m, arg(args, 0)).unwrap_or(Value::Null),
        "containsKey" => Value::Bool(map_get(m, arg(args, 0)).is_some()),
        "containsValue" => Value::Bool(m.borrow().iter().any(|(_, v)| v.equals(arg(args, 0)))),
        "size" => Value::Int(m.borrow().len() as i64),
        "isEmpty" => Value::Bool(m.borrow().is_empty()),
        "clear" => {
            m.borrow_mut().clear();
            Value::Null
        }
        "keySet" => Value::list(m.borrow().iter().map(|(k, _)| k.clone()).collect()),
        "values" => Value::list(m.borrow().iter().map(|(_, v)| v.clone()).collect()),
        "entrySet" => Value::list(
            m.borrow()
                .iter()
                .map(|(k, v)| {
                    Value::map(vec![
                        (Value::str("key"), k.clone()),
                        (Value::str("value"), v.clone()),
                    ])
                })
                .collect(),
        ),
        "getKey" => map_get(m, &Value::str("key")).unwrap_or(Value::Null),
        "getValue" => map_get(m, &Value::str("value")).unwrap_or(Value::Null),
        "toString" => Value::str(&Value::Map(m.clone()).as_text()),
        "equals" => Value::Bool(Value::Map(m.clone()).equals(arg(args, 0))),
        "hashCode" => Value::Int(m.borrow().len() as i64),
        "forEach" => {
            let Value::Lambda(f) = arg(args, 0) else { return no("forEach takes a lambda".into()) };
            for (k, v) in m.borrow().clone() {
                run_lambda(call, f, vec![k, v])?;
            }
            Value::Null
        }
        "compute" | "merge" | "computeIfAbsent" => {
            let key = arg(args, 0).clone();
            let current = map_get(m, &key);
            let f = args
                .iter()
                .rev()
                .find_map(|a| if let Value::Lambda(l) = a { Some(l.clone()) } else { None });
            let Some(f) = f else { return no(format!("{name} takes a lambda")) };
            let out = match name {
                "computeIfAbsent" => match current {
                    Some(v) => v,
                    None => run_lambda(call, &f, vec![key.clone()])?,
                },
                "merge" => match current {
                    Some(v) => run_lambda(call, &f, vec![v, arg(args, 1).clone()])?,
                    None => arg(args, 1).clone(),
                },
                _ => run_lambda(call, &f, vec![key.clone(), current.unwrap_or(Value::Null)])?,
            };
            map_put(m, key, out.clone());
            out
        }
        // a matcher made from a regex: `matcher.matches()` / `.find()` / `.group()`
        "hasNext" | "next" | "forEachRemaining"
            if map_get(m, &Value::str("__iter__")).is_some() =>
        {
            let Some(Value::List(items)) = map_get(m, &Value::str("__iter__")) else {
                return no("not an iterator".into());
            };
            let at =
                map_get(m, &Value::str("__at__")).and_then(|v| v.as_i64()).unwrap_or(0) as usize;
            let list = items.borrow().clone();
            match name {
                "hasNext" => Value::Bool(at < list.len()),
                "next" => match list.get(at) {
                    Some(v) => {
                        map_put(m, Value::str("__at__"), Value::Int(at as i64 + 1));
                        v.clone()
                    }
                    None => return no("No such element".into()),
                },
                _ => {
                    if let Value::Lambda(lam) = arg(args, 0) {
                        for v in list.iter().skip(at) {
                            run_lambda(call, lam, vec![v.clone()])?;
                        }
                    }
                    map_put(m, Value::str("__at__"), Value::Int(list.len() as i64));
                    Value::Null
                }
            }
        }
        "groupCount"
        | "lookingAt"
        | "reset"
        | "hitEnd"
        | "requireEnd"
        | "regionStart"
        | "regionEnd"
        | "region"
        | "usePattern"
        | "useAnchoringBounds"
        | "useTransparentBounds"
        | "hasAnchoringBounds"
        | "hasTransparentBounds"
        | "namedGroup"
        | "pattern"
            if map_get(m, &Value::str("__pattern__")).is_some() =>
        {
            let Some(Value::Regex(r)) = map_get(m, &Value::str("__pattern__")) else {
                return no("not a matcher".into());
            };
            let text = map_get(m, &Value::str("__text__")).map(|v| v.as_text()).unwrap_or_default();
            match name {
                "groupCount" => Value::Int(r.captures_len().saturating_sub(1) as i64),
                "lookingAt" => Value::Bool(r.find(&text).map(|f| f.start() == 0).unwrap_or(false)),
                "reset" => {
                    if let Some(t) = args.first() {
                        map_put(m, Value::str("__text__"), t.clone());
                    }
                    map_put(m, Value::str("__pos__"), Value::Int(0));
                    Value::Map(m.clone())
                }
                "hitEnd" | "requireEnd" => Value::Bool(false),
                "regionStart" => Value::Int(0),
                "regionEnd" => Value::Int(text.chars().count() as i64),
                "region" | "usePattern" | "useAnchoringBounds" | "useTransparentBounds" => {
                    if name == "usePattern"
                        && let Value::Regex(p) = arg(args, 0)
                    {
                        map_put(m, Value::str("__pattern__"), Value::Regex(p.clone()));
                    }
                    Value::Map(m.clone())
                }
                "hasAnchoringBounds" => Value::Bool(true),
                "hasTransparentBounds" => Value::Bool(false),
                "namedGroup" => {
                    let wanted = arg(args, 0).as_text();
                    match r.capture_names().position(|n| n == Some(wanted.as_str())) {
                        Some(i) => Value::Int(i as i64),
                        None => return no(format!("No group with name <{wanted}>")),
                    }
                }
                _ => Value::Regex(r.clone()),
            }
        }
        "computeIfPresent" => {
            let key = arg(args, 0).clone();
            if let (Some(held), Value::Lambda(lam)) = (map_get(m, &key), arg(args, 1))
                && !held.is_null()
            {
                let made = run_lambda(call, lam, vec![key.clone(), held])?;
                if made.is_null() {
                    m.borrow_mut().retain(|(k, _)| !k.equals(&key));
                } else {
                    map_put(m, key, made.clone());
                }
                made
            } else {
                Value::Null
            }
        }
        "each" | "every" | "any" | "findAll" | "findResult" | "findResults" | "groupBy"
        | "count"
            if map_get(m, &Value::str("__pattern__")).is_none() =>
        {
            let pairs = m.borrow().clone();
            let Value::Lambda(lam) = arg(args, 0) else { return Ok(Value::Null) };
            let mut ask = |k: &Value, v: &Value| run_lambda(call, lam, vec![k.clone(), v.clone()]);
            match name {
                "each" => {
                    for (k, v) in &pairs {
                        ask(k, v)?;
                    }
                    Value::Map(m.clone())
                }
                "every" => {
                    for (k, v) in &pairs {
                        if !ask(k, v)?.truthy().unwrap_or(false) {
                            return Ok(Value::Bool(false));
                        }
                    }
                    Value::Bool(true)
                }
                "any" => {
                    for (k, v) in &pairs {
                        if ask(k, v)?.truthy().unwrap_or(false) {
                            return Ok(Value::Bool(true));
                        }
                    }
                    Value::Bool(false)
                }
                "count" => {
                    let mut n = 0;
                    for (k, v) in &pairs {
                        if ask(k, v)?.truthy().unwrap_or(false) {
                            n += 1;
                        }
                    }
                    Value::Int(n)
                }
                "findAll" => {
                    let mut out = Vec::new();
                    for (k, v) in &pairs {
                        if ask(k, v)?.truthy().unwrap_or(false) {
                            out.push((k.clone(), v.clone()));
                        }
                    }
                    Value::map(out)
                }
                "findResult" => {
                    for (k, v) in &pairs {
                        let made = ask(k, v)?;
                        if !made.is_null() {
                            return Ok(made);
                        }
                    }
                    Value::Null
                }
                "findResults" => {
                    let mut out = Vec::new();
                    for (k, v) in &pairs {
                        let made = ask(k, v)?;
                        if !made.is_null() {
                            out.push(made);
                        }
                    }
                    Value::list(out)
                }
                _ => {
                    let mut groups: Vec<(Value, Value)> = Vec::new();
                    for (k, v) in &pairs {
                        let key = ask(k, v)?;
                        match groups.iter().find(|(g, _)| g.equals(&key)) {
                            Some((_, Value::Map(g))) => {
                                let _ = map_put(g, k.clone(), v.clone());
                            }
                            _ => groups.push((key, Value::map(vec![(k.clone(), v.clone())]))),
                        }
                    }
                    Value::map(groups)
                }
            }
        }
        "getByPath" => {
            let path = arg(args, 0).as_text();
            let mut cur = Value::Map(m.clone());
            for part in path.split('.') {
                cur = match &cur {
                    Value::Map(inner) => map_get(inner, &Value::str(part)).unwrap_or(Value::Null),
                    Value::List(inner) => match part.parse::<usize>() {
                        Ok(i) => inner.borrow().get(i).cloned().unwrap_or(Value::Null),
                        Err(_) => Value::Null,
                    },
                    _ => Value::Null,
                };
            }
            cur
        }
        "matches" | "find" | "group" | "start" | "end" => {
            let Some(Value::Regex(r)) = map_get(m, &Value::str("__pattern__")) else {
                return no(format!("unknown method [{name}] on a map"));
            };
            let text = map_get(m, &Value::str("__text__")).map(|v| v.as_text()).unwrap_or_default();
            match name {
                "matches" => Value::Bool(
                    r.find(&text).map(|f| f.start() == 0 && f.end() == text.len()).unwrap_or(false),
                ),
                "find" => {
                    let from = map_get(m, &Value::str("__at__"))
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0) as usize;
                    match r.find_at(&text, from.min(text.len())) {
                        Some(f) => {
                            map_put(
                                m,
                                Value::str("__at__"),
                                Value::Int(f.end().max(f.start() + 1) as i64),
                            );
                            map_put(m, Value::str("__last__"), Value::Int(f.start() as i64));
                            Value::Bool(true)
                        }
                        None => Value::Bool(false),
                    }
                }
                "group" => {
                    let g = arg(args, 0).as_i64().unwrap_or(0) as usize;
                    let from = map_get(m, &Value::str("__last__"))
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0) as usize;
                    match r.captures_at(&text, from) {
                        Some(c) => c.get(g).map(|g| Value::str(g.as_str())).unwrap_or(Value::Null),
                        None => Value::Null,
                    }
                }
                _ => Value::Int(
                    map_get(m, &Value::str("__last__")).and_then(|v| v.as_i64()).unwrap_or(-1),
                ),
            }
        }
        _ => return no(format!("unknown method [{name}] on a map")),
    })
}

// ------------------------------------------------------------------ dates

fn date_parts(millis: i64, offset: i32) -> (i64, i64, i64, i64, i64, i64, i64, i64) {
    let secs = millis.div_euclid(1000) + offset as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    // 1970-01-01 was a Thursday; Java counts Monday as 1
    let dow = (days + 3).rem_euclid(7) + 1;
    (y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60, millis.rem_euclid(1000), dow)
}

fn date_field(t: &Value, name: &str) -> Result<Value, String> {
    let Value::Date { millis, offset_secs } = t else { return no("not a date".into()) };
    let (y, mo, d, h, mi, s, ms, dow) = date_parts(*millis, *offset_secs);
    let day_of_year = days_from_civil(y, mo, d) - days_from_civil(y, 1, 1) + 1;
    Ok(match name {
        "year" => Value::Int(y),
        "monthValue" => Value::Int(mo),
        "month" => Value::str(MONTHS[(mo - 1) as usize]),
        "dayOfMonth" => Value::Int(d),
        "dayOfYear" => Value::Int(day_of_year),
        "dayOfWeek" => Value::str(DAYS[(dow - 1) as usize]),
        "dayOfWeekEnum" => Value::map(vec![
            (Value::str("value"), Value::Int(dow)),
            (Value::str("name"), Value::str(DAYS[(dow - 1) as usize])),
        ]),
        "hour" => Value::Int(h),
        "minute" => Value::Int(mi),
        "second" => Value::Int(s),
        "nano" => Value::Int(ms * 1_000_000),
        "millis" => Value::Long(*millis),
        "epochSecond" => Value::Long(millis.div_euclid(1000)),
        _ => return no(format!("unknown field [{name}] on ZonedDateTime")),
    })
}

const MONTHS: &[&str] = &[
    "JANUARY",
    "FEBRUARY",
    "MARCH",
    "APRIL",
    "MAY",
    "JUNE",
    "JULY",
    "AUGUST",
    "SEPTEMBER",
    "OCTOBER",
    "NOVEMBER",
    "DECEMBER",
];
const DAYS: &[&str] =
    &["MONDAY", "TUESDAY", "WEDNESDAY", "THURSDAY", "FRIDAY", "SATURDAY", "SUNDAY"];

fn date_method(t: &Value, name: &str, args: &[Value]) -> Result<Value, String> {
    let Value::Date { millis, offset_secs } = t else { return no("not a date".into()) };
    let getter = name.strip_prefix("get").map(|rest| {
        let mut c = rest.chars();
        match c.next() {
            Some(f) => f.to_lowercase().collect::<String>() + c.as_str(),
            None => String::new(),
        }
    });
    if let Some(field) = getter
        && let Ok(v) = date_field(t, &field)
    {
        return Ok(v);
    }
    let day = 86_400_000i64;
    Ok(match name {
        "toInstant" => Value::Date { millis: *millis, offset_secs: 0 },
        "toEpochMilli" => Value::Long(*millis),
        "toEpochSecond" => Value::Long(millis.div_euclid(1000)),
        "getMillis" => Value::Long(*millis),
        "toString" => Value::str(&t.as_text()),
        "equals" => Value::Bool(t.equals(arg(args, 0))),
        "isAfter" => {
            Value::Bool(matches!(arg(args, 0), Value::Date { millis: o, .. } if millis > o))
        }
        "isBefore" => {
            Value::Bool(matches!(arg(args, 0), Value::Date { millis: o, .. } if millis < o))
        }
        "isEqual" => {
            Value::Bool(matches!(arg(args, 0), Value::Date { millis: o, .. } if millis == o))
        }
        "compareTo" => Value::Int(compare(t, arg(args, 0)).map(|o| o as i64).unwrap_or(0)),
        "plusDays" => Value::Date {
            millis: millis + arg(args, 0).as_i64().unwrap_or(0) * day,
            offset_secs: *offset_secs,
        },
        "minusDays" => Value::Date {
            millis: millis - arg(args, 0).as_i64().unwrap_or(0) * day,
            offset_secs: *offset_secs,
        },
        "plusHours" => Value::Date {
            millis: millis + arg(args, 0).as_i64().unwrap_or(0) * 3_600_000,
            offset_secs: *offset_secs,
        },
        "minusHours" => Value::Date {
            millis: millis - arg(args, 0).as_i64().unwrap_or(0) * 3_600_000,
            offset_secs: *offset_secs,
        },
        "plusMinutes" => Value::Date {
            millis: millis + arg(args, 0).as_i64().unwrap_or(0) * 60_000,
            offset_secs: *offset_secs,
        },
        "plusSeconds" => Value::Date {
            millis: millis + arg(args, 0).as_i64().unwrap_or(0) * 1000,
            offset_secs: *offset_secs,
        },
        "plusMillis" => Value::Date {
            millis: millis + arg(args, 0).as_i64().unwrap_or(0),
            offset_secs: *offset_secs,
        },
        "plusYears" | "minusYears" | "plusMonths" | "minusMonths" => {
            let n =
                arg(args, 0).as_i64().unwrap_or(0) * if name.starts_with("minus") { -1 } else { 1 };
            let (y, mo, d, h, mi, s, ms, _) = date_parts(*millis, *offset_secs);
            let (y, mo) = if name.ends_with("Years") {
                (y + n, mo)
            } else {
                let total = (y * 12 + (mo - 1)) + n;
                (total.div_euclid(12), total.rem_euclid(12) + 1)
            };
            let last = days_in_month(y, mo);
            let days = days_from_civil(y, mo, d.min(last));
            let secs = days * 86_400 + h * 3600 + mi * 60 + s - *offset_secs as i64;
            Value::Date { millis: secs * 1000 + ms, offset_secs: *offset_secs }
        }
        "withZoneSameInstant" => {
            Value::Date { millis: *millis, offset_secs: zone_offset(&arg(args, 0).as_text()) }
        }
        "toLocalDate" | "toLocalDateTime" | "truncatedTo" => t.clone(),
        "format" => Value::str(&t.as_text()),
        _ => return no(format!("unknown method [{name}] on ZonedDateTime")),
    })
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
    }
}

fn zone_offset(zone: &str) -> i32 {
    let z = zone.trim_start_matches("UTC").trim_start_matches("GMT");
    if z.is_empty() || z == "Z" {
        return 0;
    }
    let sign = if z.starts_with('-') { -1 } else { 1 };
    let z = z.trim_start_matches(['+', '-']);
    let (h, m) = z.split_once(':').unwrap_or((z, "0"));
    sign * (h.parse::<i32>().unwrap_or(0) * 3600 + m.parse::<i32>().unwrap_or(0) * 60)
}

/// A date written the way ISO writes it, into milliseconds and a zone.
pub fn parse_date(text: &str) -> Option<Value> {
    let t = text.trim();
    let (date, rest) = match t.find('T') {
        Some(i) => (&t[..i], &t[i + 1..]),
        None => (t, ""),
    };
    let mut parts = date.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next().unwrap_or("1").parse().ok()?;
    let d: i64 = parts.next().unwrap_or("1").parse().ok()?;
    let (time, zone) = match rest.find(['Z', '+']).or_else(|| rest.rfind('-').filter(|i| *i > 0)) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let mut hms = time.split(':');
    let h: i64 = hms.next().unwrap_or("0").parse().unwrap_or(0);
    let mi: i64 = hms.next().unwrap_or("0").parse().unwrap_or(0);
    let sec_text = hms.next().unwrap_or("0");
    let (s, ms) = match sec_text.split_once('.') {
        Some((s, frac)) => (
            s.parse().unwrap_or(0),
            format!("{:0<3}", &frac[..frac.len().min(3)]).parse().unwrap_or(0),
        ),
        None => (sec_text.parse().unwrap_or(0), 0i64),
    };
    let offset = zone_offset(zone);
    let secs = days_from_civil(y, m, d) * 86_400 + h * 3600 + mi * 60 + s - offset as i64;
    Some(Value::Date { millis: secs * 1000 + ms, offset_secs: offset })
}

// ------------------------------------------------------------------ statics

pub fn static_field(class: &str, name: &str) -> Result<Value, String> {
    Ok(match (class, name) {
        ("Math", "PI") => Value::Double(std::f64::consts::PI),
        ("Math", "E") => Value::Double(std::f64::consts::E),
        ("Integer", "MAX_VALUE") => Value::Int(i32::MAX as i64),
        ("Integer", "MIN_VALUE") => Value::Int(i32::MIN as i64),
        ("Long", "MAX_VALUE") => Value::Long(i64::MAX),
        ("Long", "MIN_VALUE") => Value::Long(i64::MIN),
        ("Double", "MAX_VALUE") => Value::Double(f64::MAX),
        ("Double", "MIN_VALUE") => Value::Double(f64::MIN_POSITIVE),
        ("Double", "POSITIVE_INFINITY") => Value::Double(f64::INFINITY),
        ("Double", "NEGATIVE_INFINITY") => Value::Double(f64::NEG_INFINITY),
        ("Double", "NaN") => Value::Double(f64::NAN),
        ("Float", "MAX_VALUE") => Value::Float(f32::MAX as f64),
        ("ZoneOffset", "UTC") | ("ZoneId", "UTC") => Value::str("UTC"),
        ("ChronoUnit", unit) | ("ChronoField", unit) => Value::str(unit),
        ("DayOfWeek", day) => Value::map(vec![
            (
                Value::str("value"),
                Value::Int(DAYS.iter().position(|d| *d == day).map(|i| i as i64 + 1).unwrap_or(0)),
            ),
            (Value::str("name"), Value::str(day)),
        ]),
        ("Boolean", "TRUE") => Value::Bool(true),
        ("Boolean", "FALSE") => Value::Bool(false),
        ("Integer", "BYTES") => Value::Int(4),
        ("Integer", "SIZE") => Value::Int(32),
        ("Long", "BYTES") => Value::Int(8),
        ("Long", "SIZE") => Value::Int(64),
        ("Double", "BYTES") => Value::Int(8),
        ("Double", "SIZE") => Value::Int(64),
        ("Double", "MAX_EXPONENT") => Value::Int(1023),
        ("Double", "MIN_EXPONENT") => Value::Int(-1022),
        ("Double", "MIN_NORMAL") => Value::Double(f64::MIN_POSITIVE),
        ("Float", "MIN_VALUE") => Value::Float(f32::MIN_POSITIVE as f64),
        ("Float", "POSITIVE_INFINITY") => Value::Float(f64::INFINITY),
        ("Float", "NEGATIVE_INFINITY") => Value::Float(f64::NEG_INFINITY),
        ("Float", "NaN") => Value::Float(f64::NAN),
        ("Character", "BYTES") => Value::Int(2),
        ("Character", "SIZE") => Value::Int(16),
        ("Character", "MAX_VALUE") => Value::Int(0xFFFF),
        ("Character", "MIN_VALUE") => Value::Int(0),
        ("Character", "MAX_RADIX") => Value::Int(36),
        ("Character", "MIN_RADIX") => Value::Int(2),
        ("Character", "MAX_CODE_POINT") => Value::Int(0x10FFFF),
        ("Character", "MIN_CODE_POINT") => Value::Int(0),
        ("Character", "MIN_SUPPLEMENTARY_CODE_POINT") => Value::Int(0x10000),
        ("Character", "MAX_HIGH_SURROGATE") => Value::Int(0xDBFF),
        ("Character", "MIN_HIGH_SURROGATE") => Value::Int(0xD800),
        ("Character", "MAX_LOW_SURROGATE") => Value::Int(0xDFFF),
        ("Character", "MIN_LOW_SURROGATE") => Value::Int(0xDC00),
        ("Character", "MAX_SURROGATE") => Value::Int(0xDFFF),
        ("Character", "MIN_SURROGATE") => Value::Int(0xD800),
        // the character categories, numbered as Java numbers them
        ("Character", category) if character_category(category).is_some() => {
            Value::Int(character_category(category).unwrap_or(0))
        }
        ("Collections", "EMPTY_LIST") => Value::list(Vec::new()),
        ("Collections", "EMPTY_MAP") => Value::map(Vec::new()),
        ("Collections", "EMPTY_SET") => Value::list(Vec::new()),
        ("Instant", "EPOCH") => Value::Date { millis: 0, offset_secs: 0 },
        ("Instant", "MIN") | ("LocalDate", "MIN") | ("LocalDateTime", "MIN") => {
            Value::Date { millis: i64::MIN / 2, offset_secs: 0 }
        }
        ("Instant", "MAX") | ("LocalDate", "MAX") | ("LocalDateTime", "MAX") => {
            Value::Date { millis: i64::MAX / 2, offset_secs: 0 }
        }
        ("Duration", "ZERO") => Value::Long(0),
        ("DateTimeFormatter", pattern) => Value::str(&formatter_pattern(pattern)),
        ("Month", month) => Value::map(vec![
            (
                Value::str("value"),
                Value::Int(
                    MONTHS.iter().position(|m| *m == month).map(|i| i as i64 + 1).unwrap_or(0),
                ),
            ),
            (Value::str("name"), Value::str(month)),
        ]),
        _ => return no(format!("unknown static field [{class}.{name}]")),
    })
}

/// Java's number for a Unicode general category, or a directionality.
fn character_category(name: &str) -> Option<i64> {
    const CATEGORIES: &[(&str, i64)] = &[
        ("UNASSIGNED", 0),
        ("UPPERCASE_LETTER", 1),
        ("LOWERCASE_LETTER", 2),
        ("TITLECASE_LETTER", 3),
        ("MODIFIER_LETTER", 4),
        ("OTHER_LETTER", 5),
        ("NON_SPACING_MARK", 6),
        ("ENCLOSING_MARK", 7),
        ("COMBINING_SPACING_MARK", 8),
        ("DECIMAL_DIGIT_NUMBER", 9),
        ("LETTER_NUMBER", 10),
        ("OTHER_NUMBER", 11),
        ("SPACE_SEPARATOR", 12),
        ("LINE_SEPARATOR", 13),
        ("PARAGRAPH_SEPARATOR", 14),
        ("CONTROL", 15),
        ("FORMAT", 16),
        ("PRIVATE_USE", 18),
        ("SURROGATE", 19),
        ("DASH_PUNCTUATION", 20),
        ("START_PUNCTUATION", 21),
        ("END_PUNCTUATION", 22),
        ("CONNECTOR_PUNCTUATION", 23),
        ("OTHER_PUNCTUATION", 24),
        ("MATH_SYMBOL", 25),
        ("CURRENCY_SYMBOL", 26),
        ("MODIFIER_SYMBOL", 27),
        ("OTHER_SYMBOL", 28),
        ("INITIAL_QUOTE_PUNCTUATION", 29),
        ("FINAL_QUOTE_PUNCTUATION", 30),
        ("DIRECTIONALITY_UNDEFINED", -1),
        ("DIRECTIONALITY_LEFT_TO_RIGHT", 0),
        ("DIRECTIONALITY_RIGHT_TO_LEFT", 1),
        ("DIRECTIONALITY_RIGHT_TO_LEFT_ARABIC", 2),
        ("DIRECTIONALITY_EUROPEAN_NUMBER", 3),
        ("DIRECTIONALITY_EUROPEAN_NUMBER_SEPARATOR", 4),
        ("DIRECTIONALITY_EUROPEAN_NUMBER_TERMINATOR", 5),
        ("DIRECTIONALITY_ARABIC_NUMBER", 6),
        ("DIRECTIONALITY_COMMON_NUMBER_SEPARATOR", 7),
        ("DIRECTIONALITY_NONSPACING_MARK", 8),
        ("DIRECTIONALITY_BOUNDARY_NEUTRAL", 9),
        ("DIRECTIONALITY_PARAGRAPH_SEPARATOR", 10),
        ("DIRECTIONALITY_SEGMENT_SEPARATOR", 11),
        ("DIRECTIONALITY_WHITESPACE", 12),
        ("DIRECTIONALITY_OTHER_NEUTRALS", 13),
        ("DIRECTIONALITY_LEFT_TO_RIGHT_EMBEDDING", 14),
        ("DIRECTIONALITY_LEFT_TO_RIGHT_OVERRIDE", 15),
        ("DIRECTIONALITY_RIGHT_TO_LEFT_EMBEDDING", 16),
        ("DIRECTIONALITY_RIGHT_TO_LEFT_OVERRIDE", 17),
        ("DIRECTIONALITY_POP_DIRECTIONAL_FORMAT", 18),
        ("DIRECTIONALITY_LEFT_TO_RIGHT_ISOLATE", 19),
        ("DIRECTIONALITY_RIGHT_TO_LEFT_ISOLATE", 20),
        ("DIRECTIONALITY_FIRST_STRONG_ISOLATE", 21),
        ("DIRECTIONALITY_POP_DIRECTIONAL_ISOLATE", 22),
    ];
    CATEGORIES.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
}

/// The pattern a named formatter stands for.
fn formatter_pattern(name: &str) -> String {
    match name {
        "BASIC_ISO_DATE" => "yyyyMMdd",
        "ISO_LOCAL_DATE" | "ISO_DATE" => "yyyy-MM-dd",
        "ISO_LOCAL_TIME" | "ISO_TIME" => "HH:mm:ss",
        "ISO_LOCAL_DATE_TIME" => "yyyy-MM-dd'T'HH:mm:ss",
        "ISO_OFFSET_DATE" => "yyyy-MM-ddXXX",
        "ISO_OFFSET_TIME" => "HH:mm:ssXXX",
        "ISO_OFFSET_DATE_TIME" | "ISO_DATE_TIME" | "ISO_ZONED_DATE_TIME" => {
            "yyyy-MM-dd'T'HH:mm:ssXXX"
        }
        "ISO_INSTANT" => "yyyy-MM-dd'T'HH:mm:ss'Z'",
        "ISO_ORDINAL_DATE" => "yyyy-DDD",
        "ISO_WEEK_DATE" => "YYYY-'W'ww-e",
        "RFC_1123_DATE_TIME" => "EEE, d MMM yyyy HH:mm:ss 'GMT'",
        other => other,
    }
    .to_string()
}

/// The next representable double after `x` towards `toward`.
fn next_after(x: f64, toward: f64) -> f64 {
    if x.is_nan() || toward.is_nan() {
        return f64::NAN;
    }
    if x == toward {
        return toward;
    }
    if x == 0.0 {
        let tiny = f64::from_bits(1);
        return if toward > 0.0 { tiny } else { -tiny };
    }
    let bits = x.to_bits();
    let up = (toward > x) == (x > 0.0);
    f64::from_bits(if up { bits + 1 } else { bits - 1 })
}

/// Days since 1970-01-01 of a calendar date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// A double the way Java's `toHexString` writes it.
fn java_hex_double(v: f64) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0x0.0p0".into() } else { "0x0.0p0".into() };
    }
    let bits = v.to_bits();
    let sign = if bits >> 63 == 1 { "-" } else { "" };
    let exp = ((bits >> 52) & 0x7ff) as i64;
    let mantissa = bits & 0xf_ffff_ffff_ffff;
    let (lead, e) = if exp == 0 { ("0", -1022) } else { ("1", exp - 1023) };
    let mut frac = format!("{mantissa:013x}");
    while frac.len() > 1 && frac.ends_with('0') {
        frac.pop();
    }
    format!("{sign}0x{lead}.{frac}p{e}")
}

pub fn call_static(
    class: &str,
    name: &str,
    args: &[Value],
    call: Callback<'_>,
) -> Result<Value, String> {
    let f = |i: usize| arg(args, i).as_f64().unwrap_or(0.0);
    let i = |i: usize| arg(args, i).as_i64().unwrap_or(0);
    Ok(match (class, name) {
        ("Math", "abs") => match arg(args, 0) {
            Value::Int(v) => Value::Int(v.abs()),
            Value::Long(v) => Value::Long(v.abs()),
            Value::Float(v) => Value::Float(v.abs()),
            other => Value::Double(other.as_f64().unwrap_or(0.0).abs()),
        },
        ("Math", "max") | ("Math", "min") => {
            let (a, b) = (arg(args, 0), arg(args, 1));
            let pick = |x: &Value, y: &Value| {
                let ord = compare(x, y).unwrap_or(std::cmp::Ordering::Equal);
                if (name == "max") == (ord == std::cmp::Ordering::Greater)
                    || ord == std::cmp::Ordering::Equal
                {
                    x.clone()
                } else {
                    y.clone()
                }
            };
            match promoted(a, b) {
                "int" => Value::Int(pick(a, b).as_i64().unwrap_or(0)),
                "long" => Value::Long(pick(a, b).as_i64().unwrap_or(0)),
                "float" => Value::Float(pick(a, b).as_f64().unwrap_or(0.0)),
                _ => Value::Double(pick(a, b).as_f64().unwrap_or(0.0)),
            }
        }
        ("Math", "pow") => Value::Double(f(0).powf(f(1))),
        ("Math", "sqrt") => Value::Double(f(0).sqrt()),
        ("Math", "cbrt") => Value::Double(f(0).cbrt()),
        ("Math", "log") => Value::Double(f(0).ln()),
        ("Math", "log10") => Value::Double(f(0).log10()),
        ("Math", "log1p") => Value::Double(f(0).ln_1p()),
        ("Math", "exp") => Value::Double(f(0).exp()),
        ("Math", "expm1") => Value::Double(f(0).exp_m1()),
        ("Math", "floor") => Value::Double(f(0).floor()),
        ("Math", "ceil") => Value::Double(f(0).ceil()),
        ("Math", "round") => Value::Long((f(0) + 0.5).floor() as i64),
        ("Math", "rint") => Value::Double({
            let x = f(0);
            let r = x.round();
            if (x - x.trunc()).abs() == 0.5 { 2.0 * (x / 2.0).round() } else { r }
        }),
        ("Math", "sin") => Value::Double(f(0).sin()),
        ("Math", "cos") => Value::Double(f(0).cos()),
        ("Math", "tan") => Value::Double(f(0).tan()),
        ("Math", "asin") => Value::Double(f(0).asin()),
        ("Math", "acos") => Value::Double(f(0).acos()),
        ("Math", "atan") => Value::Double(f(0).atan()),
        ("Math", "atan2") => Value::Double(f(0).atan2(f(1))),
        ("Math", "sinh") => Value::Double(f(0).sinh()),
        ("Math", "cosh") => Value::Double(f(0).cosh()),
        ("Math", "tanh") => Value::Double(f(0).tanh()),
        ("Math", "hypot") => Value::Double(f(0).hypot(f(1))),
        ("Math", "signum") => Value::Double(f(0).signum()),
        ("Math", "toRadians") => Value::Double(f(0).to_radians()),
        ("Math", "toDegrees") => Value::Double(f(0).to_degrees()),
        ("Math", "random") => Value::Double(0.5),
        ("Math", "floorDiv") => Value::Long(i(0).div_euclid(i(1).max(1))),
        ("Math", "floorMod") => Value::Long(i(0).rem_euclid(i(1).max(1))),
        ("Math", "addExact") => Value::Long(i(0) + i(1)),
        ("Math", "subtractExact") => Value::Long(i(0) - i(1)),
        ("Math", "multiplyExact") => Value::Long(i(0) * i(1)),
        ("Math", "toIntExact") => Value::Int(i(0)),
        ("Math", "IEEEremainder") => Value::Double({
            let (x, y) = (f(0), f(1));
            x - (x / y).round_ties_even() * y
        }),
        ("Math", "copySign") => Value::Double(f(0).copysign(f(1))),
        ("Math", "nextUp") => Value::Double(next_after(f(0), f64::INFINITY)),
        ("Math", "nextDown") => Value::Double(next_after(f(0), f64::NEG_INFINITY)),
        ("Math", "nextAfter") => Value::Double(next_after(f(0), f(1))),
        ("Math", "scalb") => Value::Double(f(0) * 2f64.powi(i(1) as i32)),
        ("Math", "ulp") => Value::Double({
            let x = f(0).abs();
            if x.is_finite() { next_after(x, f64::INFINITY) - x } else { x }
        }),
        ("Math", "negateExact") => Value::Long(-i(0)),
        ("Math", "incrementExact") => Value::Long(i(0) + 1),
        ("Math", "decrementExact") => Value::Long(i(0) - 1),
        ("Integer", "bitCount") => Value::Int((i(0) as i32).count_ones() as i64),
        ("Long", "bitCount") => Value::Int(i(0).count_ones() as i64),
        ("Integer", "highestOneBit") => Value::Int({
            let v = i(0) as i32;
            if v == 0 { 0 } else { (1i32 << (31 - v.leading_zeros())) as i64 }
        }),
        ("Long", "highestOneBit") => Value::Long({
            let v = i(0);
            if v == 0 { 0 } else { 1i64 << (63 - v.leading_zeros()) }
        }),
        ("Integer", "lowestOneBit") => {
            Value::Int(((i(0) as i32) & (i(0) as i32).wrapping_neg()) as i64)
        }
        ("Long", "lowestOneBit") => Value::Long(i(0) & i(0).wrapping_neg()),
        ("Integer", "numberOfLeadingZeros") => Value::Int((i(0) as i32).leading_zeros() as i64),
        ("Long", "numberOfLeadingZeros") => Value::Int(i(0).leading_zeros() as i64),
        ("Integer", "numberOfTrailingZeros") => Value::Int((i(0) as i32).trailing_zeros() as i64),
        ("Long", "numberOfTrailingZeros") => Value::Int(i(0).trailing_zeros() as i64),
        ("Integer", "reverse") => Value::Int((i(0) as i32).reverse_bits() as i64),
        ("Long", "reverse") => Value::Long(i(0).reverse_bits()),
        ("Integer", "reverseBytes") => Value::Int((i(0) as i32).swap_bytes() as i64),
        ("Long", "reverseBytes") => Value::Long(i(0).swap_bytes()),
        ("Integer", "rotateLeft") => Value::Int((i(0) as i32).rotate_left(i(1) as u32 & 31) as i64),
        ("Integer", "rotateRight") => {
            Value::Int((i(0) as i32).rotate_right(i(1) as u32 & 31) as i64)
        }
        ("Long", "rotateLeft") => Value::Long(i(0).rotate_left(i(1) as u32 & 63)),
        ("Long", "rotateRight") => Value::Long(i(0).rotate_right(i(1) as u32 & 63)),
        ("Integer", "signum") => Value::Int((i(0) as i32).signum() as i64),
        ("Long", "signum") => Value::Int(i(0).signum()),
        ("Integer", "compareUnsigned") => Value::Int((i(0) as u32).cmp(&(i(1) as u32)) as i64),
        ("Long", "compareUnsigned") => Value::Int((i(0) as u64).cmp(&(i(1) as u64)) as i64),
        ("Integer", "divideUnsigned") => Value::Int(((i(0) as u32) / (i(1) as u32).max(1)) as i64),
        ("Long", "divideUnsigned") => Value::Long(((i(0) as u64) / (i(1) as u64).max(1)) as i64),
        ("Integer", "remainderUnsigned") => {
            Value::Int(((i(0) as u32) % (i(1) as u32).max(1)) as i64)
        }
        ("Long", "remainderUnsigned") => Value::Long(((i(0) as u64) % (i(1) as u64).max(1)) as i64),
        ("Integer", "toUnsignedLong") => Value::Long(i(0) as u32 as i64),
        ("Integer", "toUnsignedString") => Value::str(&(i(0) as u32).to_string()),
        ("Long", "toUnsignedString") => Value::str(&(i(0) as u64).to_string()),
        ("Integer", "toBinaryString") => Value::str(&format!("{:b}", i(0) as u32)),
        ("Long", "toBinaryString") => Value::str(&format!("{:b}", i(0) as u64)),
        ("Integer", "toHexString") => Value::str(&format!("{:x}", i(0) as u32)),
        ("Long", "toHexString") => Value::str(&format!("{:x}", i(0) as u64)),
        ("Integer", "toOctalString") => Value::str(&format!("{:o}", i(0) as u32)),
        ("Long", "toOctalString") => Value::str(&format!("{:o}", i(0) as u64)),
        ("Integer", "parseUnsignedInt")
        | ("Long", "parseUnsignedLong")
        | ("Integer", "decode")
        | ("Long", "decode") => {
            let text = arg(args, 0).as_text();
            let radix = args.get(1).and_then(|v| v.as_i64()).unwrap_or(10) as u32;
            let (digits, radix) = if name == "decode" {
                let t = text.trim_start_matches('+');
                let neg = t.starts_with('-');
                let t = t.trim_start_matches('-');
                let (d, r) = if let Some(h) = t
                    .strip_prefix("0x")
                    .or_else(|| t.strip_prefix("0X"))
                    .or_else(|| t.strip_prefix('#'))
                {
                    (h.to_string(), 16)
                } else if t.len() > 1 && t.starts_with('0') {
                    (t[1..].to_string(), 8)
                } else {
                    (t.to_string(), 10)
                };
                (if neg { format!("-{d}") } else { d }, r)
            } else {
                (text.clone(), radix)
            };
            match i64::from_str_radix(&digits, radix) {
                Ok(n) if class == "Integer" => Value::Int(n as i32 as i64),
                Ok(n) => Value::Long(n),
                Err(_) => return no(format!("For input string: \"{text}\"")),
            }
        }
        ("Double", "isFinite") => Value::Bool(f(0).is_finite()),
        ("Double", "doubleToLongBits") | ("Double", "doubleToRawLongBits") => {
            Value::Long(f(0).to_bits() as i64)
        }
        ("Double", "longBitsToDouble") => Value::Double(f64::from_bits(i(0) as u64)),
        ("Double", "toHexString") => Value::str(&java_hex_double(f(0))),
        ("Double", "hashCode") => Value::Int({
            let b = f(0).to_bits();
            ((b ^ (b >> 32)) as u32) as i32 as i64
        }),
        ("Double", "sum") => Value::Double(f(0) + f(1)),
        ("Double", "max") => Value::Double(f(0).max(f(1))),
        ("Double", "min") => Value::Double(f(0).min(f(1))),
        ("Integer", "sum") | ("Long", "sum") => Value::Long(i(0) + i(1)),
        ("Integer", "max") | ("Long", "max") => Value::Long(i(0).max(i(1))),
        ("Integer", "min") | ("Long", "min") => Value::Long(i(0).min(i(1))),
        ("Integer", "hashCode") => Value::Int(i(0) as i32 as i64),
        ("Long", "hashCode") => {
            Value::Int(((i(0) as u64 ^ ((i(0) as u64) >> 32)) as u32) as i32 as i64)
        }
        ("Boolean", "logicalAnd") => Value::Bool(
            arg(args, 0).truthy().unwrap_or(false) && arg(args, 1).truthy().unwrap_or(false),
        ),
        ("Boolean", "logicalOr") => Value::Bool(
            arg(args, 0).truthy().unwrap_or(false) || arg(args, 1).truthy().unwrap_or(false),
        ),
        ("Boolean", "logicalXor") => Value::Bool(
            arg(args, 0).truthy().unwrap_or(false) != arg(args, 1).truthy().unwrap_or(false),
        ),
        ("Boolean", "hashCode") => {
            Value::Int(if arg(args, 0).truthy().unwrap_or(false) { 1231 } else { 1237 })
        }
        ("Boolean", "toString") => Value::str(&arg(args, 0).as_text()),
        ("Objects", "requireNonNull") => match arg(args, 0) {
            Value::Null => return no(args.get(1).map(|m| m.as_text()).unwrap_or_default()),
            v => v.clone(),
        },
        ("Objects", "deepEquals") | ("Arrays", "deepEquals") | ("Arrays", "equals") => {
            Value::Bool(arg(args, 0).equals(arg(args, 1)))
        }
        ("Objects", "compare") => match args.get(2) {
            Some(Value::Lambda(lam)) => {
                run_lambda(call, lam, vec![arg(args, 0).clone(), arg(args, 1).clone()])?
            }
            _ => Value::Int(compare(arg(args, 0), arg(args, 1)).map(|o| o as i64).unwrap_or(0)),
        },
        ("Objects", "requireNonNullElse") => match arg(args, 0) {
            Value::Null => arg(args, 1).clone(),
            v => v.clone(),
        },
        ("Arrays", "deepToString") => Value::str(&arg(args, 0).as_text()),
        ("Arrays", "deepHashCode") | ("Arrays", "hashCode") => {
            Value::Int(arg(args, 0).as_text().len() as i64)
        }
        ("Arrays", "fill") => {
            if let Value::List(l) = arg(args, 0) {
                let fill = args.last().cloned().unwrap_or(Value::Null);
                for slot in l.borrow_mut().iter_mut() {
                    *slot = fill.clone();
                }
            }
            Value::Null
        }
        ("Collections", "emptySet")
        | ("Collections", "emptyIterator")
        | ("Collections", "emptyListIterator")
        | ("Collections", "emptyEnumeration")
        | ("Collections", "emptySortedSet")
        | ("Collections", "emptyNavigableSet") => Value::list(Vec::new()),
        ("Collections", "emptySortedMap") | ("Collections", "emptyNavigableMap") => {
            Value::map(Vec::new())
        }
        ("Collections", "singleton") => Value::list(vec![arg(args, 0).clone()]),
        ("Collections", "singletonMap") => {
            Value::map(vec![(arg(args, 0).clone(), arg(args, 1).clone())])
        }
        ("Collections", "nCopies") => Value::list(vec![arg(args, 1).clone(); i(0).max(0) as usize]),
        ("Collections", "frequency") => match arg(args, 0) {
            Value::List(l) => {
                Value::Int(l.borrow().iter().filter(|v| v.equals(arg(args, 1))).count() as i64)
            }
            _ => Value::Int(0),
        },
        ("Collections", "disjoint") => match (arg(args, 0), arg(args, 1)) {
            (Value::List(a), Value::List(b)) => {
                let (a, b) = (a.borrow(), b.borrow());
                Value::Bool(!a.iter().any(|x| b.iter().any(|y| y.equals(x))))
            }
            _ => Value::Bool(true),
        },
        ("Collections", "swap") => {
            if let Value::List(l) = arg(args, 0) {
                let (a, b) = (i(1) as usize, i(2) as usize);
                let mut list = l.borrow_mut();
                if a < list.len() && b < list.len() {
                    list.swap(a, b);
                }
            }
            Value::Null
        }
        ("Collections", "rotate") => {
            if let Value::List(l) = arg(args, 0) {
                let mut list = l.borrow_mut();
                let n = list.len();
                if n > 0 {
                    let by = i(1).rem_euclid(n as i64) as usize;
                    list.rotate_right(by);
                }
            }
            Value::Null
        }
        ("Collections", "fill") => {
            if let Value::List(l) = arg(args, 0) {
                for slot in l.borrow_mut().iter_mut() {
                    *slot = arg(args, 1).clone();
                }
            }
            Value::Null
        }
        ("Collections", "shuffle") => Value::Null,
        ("Collections", "copy") => {
            if let (Value::List(dst), Value::List(src)) = (arg(args, 0), arg(args, 1)) {
                let from = src.borrow().clone();
                let mut into = dst.borrow_mut();
                if from.len() > into.len() {
                    return no("Source does not fit in dest".into());
                }
                for (i, v) in from.into_iter().enumerate() {
                    into[i] = v;
                }
            }
            Value::Null
        }
        ("Collections", "reverseOrder") => Value::str("__reverse_order__"),
        ("Collections", "binarySearch") => match arg(args, 0) {
            Value::List(l) => {
                let list = l.borrow();
                let key = arg(args, 1);
                match list.iter().position(|v| v.equals(key)) {
                    Some(i) => Value::Int(i as i64),
                    None => {
                        let insert = list
                            .iter()
                            .take_while(|v| compare(v, key) == Some(std::cmp::Ordering::Less))
                            .count();
                        Value::Int(-(insert as i64) - 1)
                    }
                }
            }
            _ => Value::Int(-1),
        },
        ("Collections", "indexOfSubList") | ("Collections", "lastIndexOfSubList") => {
            match (arg(args, 0), arg(args, 1)) {
                (Value::List(a), Value::List(b)) => {
                    let (a, b) = (a.borrow(), b.borrow());
                    let hits: Vec<usize> = (0..=a.len().saturating_sub(b.len()))
                        .filter(|&i| {
                            !b.is_empty()
                                && b.iter().enumerate().all(|(k, v)| {
                                    a.get(i + k).map(|x| x.equals(v)).unwrap_or(false)
                                })
                        })
                        .collect();
                    let pick = if name == "indexOfSubList" { hits.first() } else { hits.last() };
                    Value::Int(pick.map(|i| *i as i64).unwrap_or(-1))
                }
                _ => Value::Int(-1),
            }
        }
        ("Collections", "list")
        | ("Collections", "enumeration")
        | ("Collections", "asLifoQueue")
        | ("Collections", "unmodifiableCollection")
        | ("Collections", "unmodifiableSet")
        | ("Collections", "unmodifiableSortedSet")
        | ("Collections", "unmodifiableNavigableSet")
        | ("Collections", "synchronizedList")
        | ("Collections", "synchronizedSet")
        | ("Collections", "synchronizedCollection")
        | ("Collections", "newSetFromMap") => match arg(args, 0) {
            Value::List(l) => Value::List(l.clone()),
            Value::Map(m) => Value::list(m.borrow().iter().map(|(k, _)| k.clone()).collect()),
            _ => Value::list(Vec::new()),
        },
        ("Collections", "unmodifiableSortedMap")
        | ("Collections", "unmodifiableNavigableMap")
        | ("Collections", "synchronizedMap") => arg(args, 0).clone(),
        ("Collections", "addAll") => {
            if let Value::List(l) = arg(args, 0) {
                l.borrow_mut().extend(args[1..].iter().cloned());
            }
            Value::Bool(args.len() > 1)
        }
        ("Processors", "bytes") => {
            let text = arg(args, 0).as_text();
            match crate::ingest::bytes_of_text(&text) {
                Ok(n) => Value::Long(n),
                Err(e) => return no(e),
            }
        }
        ("Processors", "lowercase") => Value::str(&arg(args, 0).as_text().to_lowercase()),
        ("Processors", "uppercase") => Value::str(&arg(args, 0).as_text().to_uppercase()),
        ("Processors", "urlDecode") => Value::str(
            &percent_encoding::percent_decode_str(&arg(args, 0).as_text().replace('+', " "))
                .decode_utf8_lossy(),
        ),
        ("Processors", "json") => {
            // json(text) parses; json(map, field) parses the field into the map
            match (arg(args, 0), args.get(1)) {
                (Value::Map(m), Some(field)) => {
                    let text = map_get(m, field).map(|v| v.as_text()).unwrap_or_default();
                    match serde_json::from_str::<serde_json::Value>(&text) {
                        Ok(serde_json::Value::Object(o)) => {
                            for (k, v) in o {
                                let _ = map_put(m, Value::str(&k), Value::from_json(&v));
                            }
                            Value::Null
                        }
                        Ok(other) => Value::from_json(&other),
                        Err(e) => return no(e.to_string()),
                    }
                }
                (v, _) => match serde_json::from_str::<serde_json::Value>(&v.as_text()) {
                    Ok(parsed) => Value::from_json(&parsed),
                    Err(e) => return no(e.to_string()),
                },
            }
        }
        ("Pattern", "quote") => Value::str(&format!("\\Q{}\\E", arg(args, 0).as_text())),
        ("Optional", "of") | ("Optional", "ofNullable") => optional_of(arg(args, 0).clone()),
        ("Optional", "empty") => Value::list(Vec::new()),
        ("Function", "identity") | ("UnaryOperator", "identity") => Value::str("__identity__"),
        ("Collectors", "toSet")
        | ("Collectors", "toCollection")
        | ("Collectors", "toUnmodifiableList")
        | ("Collectors", "toUnmodifiableSet") => Value::str("__to_list__"),
        ("Collectors", "counting") => Value::str("__counting__"),
        ("Collectors", "toList") => Value::str("__to_list__"),
        ("Collectors", "joining") => Value::map(vec![
            (Value::str("__collector__"), Value::str("joining")),
            (Value::str("args"), Value::list(args.to_vec())),
        ]),
        ("Collectors", "toMap") | ("Collectors", "toUnmodifiableMap") => Value::map(vec![
            (Value::str("__collector__"), Value::str("toMap")),
            (Value::str("key"), arg(args, 0).clone()),
            (Value::str("value"), arg(args, 1).clone()),
        ]),
        ("Collectors", "groupingBy")
        | ("Collectors", "partitioningBy")
        | ("Collectors", "mapping")
        | ("Collectors", "summingInt")
        | ("Collectors", "summingLong")
        | ("Collectors", "summingDouble")
        | ("Collectors", "averagingInt")
        | ("Collectors", "averagingLong")
        | ("Collectors", "averagingDouble")
        | ("Collectors", "minBy")
        | ("Collectors", "maxBy")
        | ("Collectors", "reducing")
        | ("Collectors", "collectingAndThen")
        | ("Collectors", "summarizingInt")
        | ("Collectors", "summarizingLong")
        | ("Collectors", "summarizingDouble") => Value::map(vec![
            (Value::str("__collector__"), Value::str(name)),
            (Value::str("args"), Value::list(args.to_vec())),
        ]),
        ("Instant", "from")
        | ("ZonedDateTime", "from")
        | ("LocalDateTime", "from")
        | ("LocalDate", "from") => arg(args, 0).clone(),
        ("ZonedDateTime", "ofLocal") | ("ZonedDateTime", "ofStrict") => arg(args, 0).clone(),
        ("LocalDate", "ofEpochDay") => Value::Date { millis: i(0) * 86_400_000, offset_secs: 0 },
        ("LocalDate", "ofYearDay") => {
            let base = days_from_civil(i(0), 1, 1);
            Value::Date { millis: (base + i(1) - 1) * 86_400_000, offset_secs: 0 }
        }
        ("LocalDate", "of") | ("LocalDateTime", "of") => {
            let days = days_from_civil(i(0), i(1).clamp(1, 12), i(2).clamp(1, 31));
            let secs = i(3) * 3600 + i(4) * 60 + i(5);
            let nanos = args.get(6).and_then(|v| v.as_i64()).unwrap_or(0);
            Value::Date {
                millis: days * 86_400_000 + secs * 1000 + nanos / 1_000_000,
                offset_secs: 0,
            }
        }
        ("Duration", "ofDays") => Value::Long(i(0) * 86_400_000),
        ("Duration", "ofHours") => Value::Long(i(0) * 3_600_000),
        ("Duration", "ofMinutes") => Value::Long(i(0) * 60_000),
        ("Duration", "ofSeconds") => {
            Value::Long(i(0) * 1000 + args.get(1).and_then(|v| v.as_i64()).unwrap_or(0) / 1_000_000)
        }
        ("Duration", "ofMillis") => Value::Long(i(0)),
        ("Duration", "ofNanos") => Value::Long(i(0) / 1_000_000),
        ("Duration", "of") => {
            let unit = arg(args, 1).as_text().to_uppercase();
            Value::Long(
                i(0) * match unit.as_str() {
                    "DAYS" => 86_400_000,
                    "HOURS" => 3_600_000,
                    "MINUTES" => 60_000,
                    "SECONDS" => 1000,
                    "MILLIS" => 1,
                    _ => 0,
                },
            )
        }
        ("Duration", "between") => match (arg(args, 0), arg(args, 1)) {
            (Value::Date { millis: a, .. }, Value::Date { millis: b, .. }) => Value::Long(b - a),
            _ => Value::Long(0),
        },
        ("Duration", "parse") => {
            // PnDTnHnMn.nS
            let text = arg(args, 0).as_text().to_uppercase();
            let mut total: f64 = 0.0;
            let mut num = String::new();
            let mut in_time = false;
            let sign = if text.starts_with('-') { -1.0 } else { 1.0 };
            for c in text.trim_start_matches('-').trim_start_matches('P').chars() {
                match c {
                    'T' => in_time = true,
                    'D' | 'H' | 'M' | 'S' => {
                        let n: f64 = num.parse().unwrap_or(0.0);
                        num.clear();
                        total += n * match (c, in_time) {
                            ('D', _) => 86_400_000.0,
                            ('H', _) => 3_600_000.0,
                            ('M', true) => 60_000.0,
                            ('S', _) => 1000.0,
                            _ => 0.0,
                        };
                    }
                    other => num.push(other),
                }
            }
            Value::Long((sign * total) as i64)
        }
        ("DateTimeFormatter", "ofPattern") => Value::str(&arg(args, 0).as_text()),
        ("DateTimeFormatter", "ofLocalizedDate")
        | ("DateTimeFormatter", "ofLocalizedTime")
        | ("DateTimeFormatter", "ofLocalizedDateTime") => Value::str("yyyy-MM-dd"),
        ("Character", "getNumericValue") => Value::Int(
            arg(args, 0)
                .as_text()
                .chars()
                .next()
                .and_then(|c| c.to_digit(36))
                .map(|d| d as i64)
                .unwrap_or(-1),
        ),
        ("Character", "getType") => Value::Int({
            let c = arg(args, 0).as_text().chars().next().unwrap_or('\0');
            if c.is_uppercase() {
                1
            } else if c.is_lowercase() {
                2
            } else if c.is_ascii_digit() {
                9
            } else if c.is_whitespace() {
                12
            } else if c.is_control() {
                15
            } else if c.is_alphabetic() {
                5
            } else if c.is_ascii_punctuation() {
                24
            } else {
                0
            }
        }),
        ("Character", "isAlphabetic")
        | ("Character", "isLetterOrDigit")
        | ("Character", "isUpperCase")
        | ("Character", "isLowerCase")
        | ("Character", "isSpaceChar")
        | ("Character", "isISOControl")
        | ("Character", "isDefined")
        | ("Character", "isJavaIdentifierStart")
        | ("Character", "isJavaIdentifierPart")
        | ("Character", "isUnicodeIdentifierStart")
        | ("Character", "isUnicodeIdentifierPart")
        | ("Character", "isIdentifierIgnorable")
        | ("Character", "isMirrored")
        | ("Character", "isTitleCase")
        | ("Character", "isSurrogate")
        | ("Character", "isHighSurrogate")
        | ("Character", "isLowSurrogate")
        | ("Character", "isSupplementaryCodePoint")
        | ("Character", "isBmpCodePoint")
        | ("Character", "isValidCodePoint")
        | ("Character", "isIdeographic") => {
            let c = match arg(args, 0) {
                Value::Str(s) => s.chars().next().unwrap_or('\0'),
                other => char::from_u32(other.as_i64().unwrap_or(0) as u32).unwrap_or('\0'),
            };
            let code = c as u32;
            Value::Bool(match name {
                "isAlphabetic" => c.is_alphabetic(),
                "isLetterOrDigit" => c.is_alphanumeric(),
                "isUpperCase" => c.is_uppercase(),
                "isLowerCase" => c.is_lowercase(),
                "isSpaceChar" => c.is_whitespace(),
                "isISOControl" => c.is_control(),
                "isDefined" => code != 0,
                "isJavaIdentifierStart" | "isUnicodeIdentifierStart" => {
                    c.is_alphabetic() || c == '_' || c == '$'
                }
                "isJavaIdentifierPart" | "isUnicodeIdentifierPart" => {
                    c.is_alphanumeric() || c == '_' || c == '$'
                }
                "isIdentifierIgnorable" => c.is_control() && !c.is_whitespace(),
                "isMirrored" => matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>'),
                "isTitleCase" => false,
                "isSurrogate" => (0xD800..=0xDFFF).contains(&code),
                "isHighSurrogate" => (0xD800..=0xDBFF).contains(&code),
                "isLowSurrogate" => (0xDC00..=0xDFFF).contains(&code),
                "isSupplementaryCodePoint" => code >= 0x10000,
                "isBmpCodePoint" => code < 0x10000,
                "isValidCodePoint" => code <= 0x10FFFF,
                _ => ('\u{4E00}'..='\u{9FFF}').contains(&c),
            })
        }
        ("Character", "toUpperCase")
        | ("Character", "toLowerCase")
        | ("Character", "toTitleCase")
        | ("Character", "toChars")
        | ("Character", "reverseBytes")
        | ("Character", "highSurrogate")
        | ("Character", "lowSurrogate")
        | ("Character", "charCount")
        | ("Character", "digit")
        | ("Character", "forDigit")
        | ("Character", "getName")
        | ("Character", "hashCode")
        | ("Character", "codePointAt")
        | ("Character", "toCodePoint")
        | ("Character", "toString")
        | ("Character", "valueOf")
        | ("Character", "getDirectionality") => {
            let c = match arg(args, 0) {
                Value::Str(s) => s.chars().next().unwrap_or('\0'),
                other => char::from_u32(other.as_i64().unwrap_or(0) as u32).unwrap_or('\0'),
            };
            match name {
                "toUpperCase" => Value::str(&c.to_uppercase().to_string()),
                "toLowerCase" => Value::str(&c.to_lowercase().to_string()),
                "toTitleCase" => Value::str(&c.to_uppercase().to_string()),
                "toChars" => Value::list(vec![Value::str(&c.to_string())]),
                "reverseBytes" => Value::Int((c as u16).swap_bytes() as i64),
                "highSurrogate" | "lowSurrogate" => Value::Int(0),
                "charCount" => Value::Int(if (c as u32) >= 0x10000 { 2 } else { 1 }),
                "digit" => {
                    Value::Int(c.to_digit(i(1).clamp(2, 36) as u32).map(|d| d as i64).unwrap_or(-1))
                }
                "forDigit" => Value::str(
                    &char::from_digit(i(0) as u32, i(1).clamp(2, 36) as u32)
                        .map(|c| c.to_string())
                        .unwrap_or_default(),
                ),
                "getName" => Value::str(&format!("U+{:04X}", c as u32)),
                "hashCode" | "codePointAt" | "toCodePoint" => Value::Int(c as i64),
                "getDirectionality" => Value::Int(0),
                _ => Value::str(&c.to_string()),
            }
        }
        ("Integer", "parseInt")
        | ("Integer", "valueOf")
        | ("Short", "parseShort")
        | ("Byte", "parseByte") => match arg(args, 0) {
            Value::Str(s) => Value::Int(
                s.trim().parse::<i64>().map_err(|_| format!("For input string: \"{s}\""))?,
            ),
            other => Value::Int(other.as_i64().unwrap_or(0)),
        },
        ("Long", "parseLong") | ("Long", "valueOf") => match arg(args, 0) {
            Value::Str(s) => Value::Long(
                s.trim().parse::<i64>().map_err(|_| format!("For input string: \"{s}\""))?,
            ),
            other => Value::Long(other.as_i64().unwrap_or(0)),
        },
        ("Double", "parseDouble") | ("Double", "valueOf") => match arg(args, 0) {
            Value::Str(s) => Value::Double(
                s.trim().parse::<f64>().map_err(|_| format!("For input string: \"{s}\""))?,
            ),
            other => Value::Double(other.as_f64().unwrap_or(0.0)),
        },
        ("Float", "parseFloat") | ("Float", "valueOf") => Value::Float(f(0) as f32 as f64),
        ("Boolean", "parseBoolean") | ("Boolean", "valueOf") => {
            Value::Bool(arg(args, 0).as_text() == "true")
        }
        ("Integer", "toString")
        | ("Long", "toString")
        | ("Double", "toString")
        | ("String", "valueOf")
        | ("Objects", "toString") => Value::str(&arg(args, 0).as_text()),
        ("Integer", "compare")
        | ("Long", "compare")
        | ("Double", "compare")
        | ("Character", "compare") => {
            Value::Int(compare(arg(args, 0), arg(args, 1)).map(|o| o as i64).unwrap_or(0))
        }
        ("Double", "isNaN") => Value::Bool(f(0).is_nan()),
        ("Double", "isInfinite") => Value::Bool(f(0).is_infinite()),
        ("Character", "isDigit") => Value::Bool(
            arg(args, 0).as_text().chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false),
        ),
        ("Character", "isLetter") => Value::Bool(
            arg(args, 0).as_text().chars().next().map(|c| c.is_alphabetic()).unwrap_or(false),
        ),
        ("Character", "isWhitespace") => Value::Bool(
            arg(args, 0).as_text().chars().next().map(|c| c.is_whitespace()).unwrap_or(false),
        ),
        ("Character", "toLowerCase") => Value::str(&arg(args, 0).as_text().to_lowercase()),
        ("Character", "toUpperCase") => Value::str(&arg(args, 0).as_text().to_uppercase()),
        ("String", "join") => {
            let sep = arg(args, 0).as_text();
            let items: Vec<String> = match arg(args, 1) {
                Value::List(l) => l.borrow().iter().map(|v| v.as_text()).collect(),
                _ => args[1..].iter().map(|v| v.as_text()).collect(),
            };
            Value::str(&items.join(&sep))
        }
        ("String", "format") => Value::str(&java_format(&arg(args, 0).as_text(), &args[1..])),
        ("Objects", "equals") => Value::Bool(arg(args, 0).equals(arg(args, 1))),
        ("Objects", "isNull") => Value::Bool(arg(args, 0).is_null()),
        ("Objects", "nonNull") => Value::Bool(!arg(args, 0).is_null()),
        ("Objects", "hash") | ("Objects", "hashCode") => Value::Int(
            args.iter()
                .map(|v| java_string_hash(&v.as_text()))
                .fold(1, |h, x| h.wrapping_mul(31).wrapping_add(x)),
        ),
        ("Collections", "sort") => {
            if let Value::List(l) = arg(args, 0) {
                list_method(l, "sort", &args[1..], call)?;
            }
            Value::Null
        }
        ("Collections", "reverse") => {
            if let Value::List(l) = arg(args, 0) {
                l.borrow_mut().reverse();
            }
            Value::Null
        }
        ("Collections", "max") | ("Collections", "min") => match arg(args, 0) {
            Value::List(l) => list_method(l, name, &[], call)?,
            _ => Value::Null,
        },
        ("Collections", "emptyList") | ("List", "of") | ("Arrays", "asList") => {
            if name == "emptyList" {
                Value::list(Vec::new())
            } else if args.len() == 1 && matches!(args[0], Value::List(_)) {
                args[0].clone()
            } else {
                Value::list(args.to_vec())
            }
        }
        ("Collections", "emptyMap") | ("Map", "of") => {
            let mut pairs = Vec::new();
            let mut k = 0;
            while k + 1 < args.len() {
                pairs.push((args[k].clone(), args[k + 1].clone()));
                k += 2;
            }
            Value::map(pairs)
        }
        ("Collections", "unmodifiableList")
        | ("Collections", "unmodifiableMap")
        | ("Collections", "singletonList") => {
            if name == "singletonList" {
                Value::list(vec![arg(args, 0).clone()])
            } else {
                arg(args, 0).clone()
            }
        }
        ("Arrays", "toString") | ("Arrays", "stream") => match name {
            "toString" => Value::str(&arg(args, 0).as_text()),
            _ => arg(args, 0).clone(),
        },
        ("System", "currentTimeMillis") => Value::Long(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
        ),
        ("System", "nanoTime") => Value::Long(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0),
        ),
        ("ZonedDateTime", "parse")
        | ("Instant", "parse")
        | ("LocalDate", "parse")
        | ("LocalDateTime", "parse")
        | ("OffsetDateTime", "parse") => parse_date(&arg(args, 0).as_text())
            .ok_or_else(|| format!("Text '{}' could not be parsed", arg(args, 0).as_text()))?,
        ("ZonedDateTime", "of") => {
            let (y, mo, d) = (i(0), i(1), i(2));
            let (h, mi, s) = (i(3), i(4), i(5));
            let ms = i(6) / 1_000_000;
            let offset = zone_offset(&arg(args, 7).as_text());
            let secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + s - offset as i64;
            Value::Date { millis: secs * 1000 + ms, offset_secs: offset }
        }
        ("ZonedDateTime", "ofInstant") => match arg(args, 0) {
            Value::Date { millis, .. } => {
                Value::Date { millis: *millis, offset_secs: zone_offset(&arg(args, 1).as_text()) }
            }
            other => Value::Date {
                millis: other.as_i64().unwrap_or(0),
                offset_secs: zone_offset(&arg(args, 1).as_text()),
            },
        },
        ("Instant", "ofEpochMilli") => Value::Date { millis: i(0), offset_secs: 0 },
        ("Instant", "ofEpochSecond") => Value::Date { millis: i(0) * 1000, offset_secs: 0 },
        ("ZoneId", "of") | ("ZoneOffset", "of") => Value::str(&arg(args, 0).as_text()),
        ("ChronoUnit", _) => Value::str(name),
        ("MovingFunctions", _) => moving_function(name, args)?,
        ("Pattern", "compile") => Value::Regex(Rc::new(
            regex::Regex::new(&arg(args, 0).as_text()).map_err(|e| e.to_string())?,
        )),
        ("Pattern", "matches") => {
            let re = regex::Regex::new(&format!("^(?:{})$", arg(args, 0).as_text()))
                .map_err(|e| e.to_string())?;
            Value::Bool(re.is_match(&arg(args, 1).as_text()))
        }
        ("Base64", "getEncoder") | ("Base64", "getDecoder") => Value::str(name),
        _ => return no(format!("unknown static method [{class}.{name}]")),
    })
}

pub fn construct(class: &str, args: &[Value]) -> Result<Value, String> {
    let base = class.trim_end_matches("[]");
    Ok(match (class.ends_with("[]"), base) {
        (true, _) => {
            let n = arg(args, 0).as_i64().unwrap_or(0).max(0) as usize;
            let fill = match base {
                "int" | "long" | "short" | "byte" => Value::Int(0),
                "double" | "float" => Value::Double(0.0),
                "boolean" => Value::Bool(false),
                _ => Value::Null,
            };
            Value::list(vec![fill; n])
        }
        (
            _,
            "ArrayList" | "LinkedList" | "HashSet" | "TreeSet" | "LinkedHashSet" | "ArrayDeque",
        ) => match arg(args, 0) {
            Value::List(l) => Value::list(l.borrow().clone()),
            _ => Value::list(Vec::new()),
        },
        (_, "HashMap" | "TreeMap" | "LinkedHashMap") => match arg(args, 0) {
            Value::Map(m) => Value::map(m.borrow().clone()),
            _ => Value::map(Vec::new()),
        },
        (_, "StringBuilder" | "StringBuffer") => {
            Value::Builder(Rc::new(RefCell::new(match arg(args, 0) {
                Value::Str(s) => s.to_string(),
                _ => String::new(),
            })))
        }
        (_, "String") => Value::str(&arg(args, 0).as_text()),
        (_, "Integer" | "Long" | "Double" | "Float" | "Boolean") => {
            cast(base, arg(args, 0).clone())?
        }
        (_, "Date") => Value::Date { millis: arg(args, 0).as_i64().unwrap_or(0), offset_secs: 0 },
        (_, "Random") => Value::map(vec![(Value::str("__seed__"), arg(args, 0).clone())]),
        (_, c) if c.ends_with("Exception") || c.ends_with("Error") => {
            Value::Error(Rc::from(arg(args, 0).as_text().as_str()))
        }
        _ => return no(format!("unknown class [{class}]")),
    })
}

/// A bare call the language itself knows, once the context has declined it.
pub fn call_free(name: &str, args: &[Value]) -> Result<Value, String> {
    match name {
        "saturation" => {
            let (v, p) =
                (arg(args, 0).as_f64().unwrap_or(0.0), arg(args, 1).as_f64().unwrap_or(1.0));
            Ok(Value::Double(v / (v + p)))
        }
        "sigmoid" => {
            let (v, p, e) = (
                arg(args, 0).as_f64().unwrap_or(0.0),
                arg(args, 1).as_f64().unwrap_or(1.0),
                arg(args, 2).as_f64().unwrap_or(1.0),
            );
            Ok(Value::Double(v.powf(e) / (v.powf(e) + p.powf(e))))
        }
        // The vector functions the k-NN plugin adds to the language, for a
        // script that wants to score by distance itself rather than let a
        // `knn` query do it. Both sides are arrays of numbers; what differs
        // is what "apart" means.
        "cosineSimilarity" | "l2Squared" | "l1Norm" | "hammingDistance" | "innerProduct" => {
            // a vector reaches a script as a list, and a field read through
            // `doc[...]` reaches it as whatever that context hands back; both
            // are arrays of numbers once they are written out
            let read = |v: &Value| -> Option<Vec<f32>> {
                match v.to_json() {
                    serde_json::Value::Array(items) => {
                        items.iter().map(|i| i.as_f64().map(|n| n as f32)).collect()
                    }
                    serde_json::Value::Number(one) => Some(vec![one.as_f64()? as f32]),
                    _ => None,
                }
            };
            let (a, b) = (read(&arg(args, 0)), read(&arg(args, 1)));
            let (Some(a), Some(b)) = (a, b) else {
                return no(format!("[{name}] takes two arrays of numbers"));
            };
            if a.len() != b.len() {
                return no(format!(
                    "[{name}]: query vector has {} dimensions and the document has {}",
                    a.len(),
                    b.len()
                ));
            }
            let space = match name {
                "cosineSimilarity" => crate::knn::Space::Cosine,
                "l1Norm" => crate::knn::Space::L1,
                "hammingDistance" => crate::knn::Space::Hamming,
                "innerProduct" => crate::knn::Space::InnerProduct,
                _ => crate::knn::Space::L2,
            };
            let distance = space.distance(&a, &b);
            Ok(Value::Double(match name {
                // a similarity runs the other way from a distance: one is
                // pointing the same way, minus one is pointing away
                "cosineSimilarity" => (1.0 - distance) as f64,
                // an inner product is what it is, not what it costs
                "innerProduct" => (-distance) as f64,
                _ => distance as f64,
            }))
        }
        n if n.starts_with("decayNumeric")
            || n.starts_with("decayDate")
            || n.starts_with("decayGeo") =>
        {
            decay(n, args)
        }
        _ => no(format!("Unknown call [{name}]")),
    }
}

fn decay(name: &str, args: &[Value]) -> Result<Value, String> {
    let scale_of = |v: &Value| -> f64 {
        match v {
            // a time is written with a unit and read back in nanoseconds; a
            // distance with a unit and read back in metres
            Value::Str(s) => crate::search::parse_time_amount(s)
                .map(|ns| ns / 1e6)
                .or_else(|| crate::search::parse_distance(s))
                .unwrap_or_else(|| s.parse().unwrap_or(1.0)),
            other => other.as_f64().unwrap_or(1.0),
        }
    };
    let (origin, scale, offset, decay, value) =
        (arg(args, 0), arg(args, 1), arg(args, 2), arg(args, 3), arg(args, 4));
    let decay = decay.as_f64().unwrap_or(0.5);
    let distance = if name.starts_with("decayGeo") {
        let o = origin.as_text();
        let v = match value {
            Value::Map(m) => format!(
                "{},{}",
                map_get(m, &Value::str("lat")).map(|v| v.as_text()).unwrap_or_default(),
                map_get(m, &Value::str("lon")).map(|v| v.as_text()).unwrap_or_default()
            ),
            other => other.as_text(),
        };
        crate::search::geo_distance_metres(
            &serde_json::Value::String(o),
            &serde_json::Value::String(v),
        )
        .unwrap_or(0.0)
    } else if name.starts_with("decayDate") {
        let o = match origin {
            Value::Date { millis, .. } => *millis as f64,
            other => parse_date(&other.as_text())
                .and_then(|d| {
                    if let Value::Date { millis, .. } = d { Some(millis as f64) } else { None }
                })
                .unwrap_or(0.0),
        };
        let v = match value {
            Value::Date { millis, .. } => *millis as f64,
            other => other.as_f64().unwrap_or(0.0),
        };
        (v - o).abs()
    } else {
        (value.as_f64().unwrap_or(0.0) - origin.as_f64().unwrap_or(0.0)).abs()
    };
    let scale = scale_of(scale);
    let offset = scale_of(offset);
    let d = (distance - offset).max(0.0);
    let out = if name.ends_with("Gauss") {
        let sigma2 = -(scale * scale) / (2.0 * decay.ln());
        (-(d * d) / (2.0 * sigma2)).exp()
    } else if name.ends_with("Exp") {
        let lambda = decay.ln() / scale;
        (lambda * d).exp()
    } else {
        let s = scale / (1.0 - decay);
        ((s - d) / s).max(0.0)
    };
    Ok(Value::Double(out))
}

fn moving_function(name: &str, args: &[Value]) -> Result<Value, String> {
    let values: Vec<f64> = match arg(args, 0) {
        Value::List(l) => l.borrow().iter().filter_map(|v| v.as_f64()).collect(),
        _ => Vec::new(),
    };
    let f = |i: usize| arg(args, i).as_f64().unwrap_or(0.0);
    if values.is_empty() {
        // a sum of nothing is nought; the rest have no answer
        return Ok(Value::Double(if name == "sum" { 0.0 } else { f64::NAN }));
    }
    let n = values.len() as f64;
    Ok(Value::Double(match name {
        "max" | "windowMax" => values.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        "min" | "windowMin" => values.iter().cloned().fold(f64::INFINITY, f64::min),
        "sum" => values.iter().sum(),
        "unweightedAvg" => values.iter().sum::<f64>() / n,
        "stdDev" => {
            let avg = f(1);
            (values.iter().map(|v| (v - avg).powi(2)).sum::<f64>() / n).sqrt()
        }
        "linearWeightedAvg" => {
            let (mut total, mut weight) = (0.0, 0.0);
            for (i, v) in values.iter().enumerate() {
                total += v * (i + 1) as f64;
                weight += (i + 1) as f64;
            }
            total / weight
        }
        "ewma" => {
            let alpha = f(1);
            let mut avg = values[0];
            for v in &values[1..] {
                avg = alpha * v + (1.0 - alpha) * avg;
            }
            avg
        }
        "holt" => {
            let (alpha, beta) = (f(1), f(2));
            if values.len() < 2 {
                values[0]
            } else {
                let mut s = values[0];
                let mut b = values[1] - values[0];
                for v in &values[1..] {
                    let last_s = s;
                    s = alpha * v + (1.0 - alpha) * (s + b);
                    b = beta * (s - last_s) + (1.0 - beta) * b;
                }
                s + b
            }
        }
        "holtWinters" => {
            // the additive form, one step ahead
            let (alpha, beta, gamma, period) =
                (f(1), f(2), f(3), arg(args, 4).as_i64().unwrap_or(1).max(1) as usize);
            if values.len() < 2 * period {
                return Ok(Value::Double(f64::NAN));
            }
            let mut s = values[..period].iter().sum::<f64>() / period as f64;
            let mut b = (values[period..2 * period].iter().sum::<f64>()
                - values[..period].iter().sum::<f64>())
                / (period * period) as f64;
            let mut seasonal: Vec<f64> = values[..period].iter().map(|v| v - s).collect();
            for (i, v) in values.iter().enumerate().skip(period) {
                let last_s = s;
                let sea = seasonal[i % period];
                s = alpha * (v - sea) + (1.0 - alpha) * (s + b);
                b = beta * (s - last_s) + (1.0 - beta) * b;
                seasonal[i % period] = gamma * (v - s) + (1.0 - gamma) * sea;
            }
            s + b + seasonal[values.len() % period]
        }
        _ => return no(format!("unknown MovingFunctions.{name}")),
    }))
}

// ------------------------------------------------------------------ helpers

pub fn java_string_hash(s: &str) -> i64 {
    let mut h: i32 = 0;
    for c in s.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(c as i32);
    }
    h as i64
}

/// The little of `String.format` the suite uses: `%s`, `%d`, `%.2f`.
fn java_format(pattern: &str, args: &[Value]) -> String {
    let mut out = String::new();
    let mut chars = pattern.chars().peekable();
    let mut next = 0;
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let mut spec = String::new();
        while let Some(&d) = chars.peek() {
            chars.next();
            spec.push(d);
            if d.is_ascii_alphabetic() || d == '%' {
                break;
            }
        }
        let value = args.get(next).cloned().unwrap_or(Value::Null);
        match spec.chars().last() {
            Some('%') => out.push('%'),
            Some('d') => {
                out.push_str(&value.as_i64().unwrap_or(0).to_string());
                next += 1;
            }
            Some('f') => {
                let precision = spec
                    .trim_start_matches('.')
                    .trim_end_matches('f')
                    .parse::<usize>()
                    .unwrap_or(6);
                out.push_str(&format!("{:.*}", precision, value.as_f64().unwrap_or(0.0)));
                next += 1;
            }
            _ => {
                out.push_str(&value.as_text());
                next += 1;
            }
        }
    }
    out
}

fn hex_digest(text: &str, algorithm: &str) -> String {
    use sha1::Digest;
    match algorithm {
        "sha1" => format!("{:x}", sha1::Sha1::digest(text.as_bytes())),
        _ => format!("{:x}", sha2::Sha256::digest(text.as_bytes())),
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.len();
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let v = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(v >> 18) as usize & 63] as char);
        out.push(T[(v >> 12) as usize & 63] as char);
        out.push(if n > 1 { T[(v >> 6) as usize & 63] as char } else { '=' });
        out.push(if n > 2 { T[v as usize & 63] as char } else { '=' });
    }
    out
}

fn base64_decode(text: &str) -> Vec<u8> {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0;
    for c in text.bytes() {
        if c == b'=' {
            break;
        }
        let Some(v) = T.iter().position(|t| *t == c) else { continue };
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    out
}
