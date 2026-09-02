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
            Value::Str(s) if class == "char" => {
                Value::str(&s.chars().next().map(|c| c.to_string()).unwrap_or_default())
            }
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
    Ok(match name {
        "length" => Value::Int(text.chars().count() as i64),
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
        "collect" | "toList" => Value::List(l.clone()),
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
        _ => return no(format!("unknown static field [{class}.{name}]")),
    })
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
