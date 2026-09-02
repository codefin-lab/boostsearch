//! Painless, the scripting language OpenSearch runs.
//!
//! A script is compiled once -- read into a program -- and run against a
//! context: the variables it may read and the functions it may call. What it
//! cannot do it says so about the way OpenSearch does, with the position in
//! the source where it went wrong.

pub mod ast;
pub mod builtins;
pub mod contexts;
pub mod eval;
pub mod lexer;
pub mod parser;
pub mod value;

pub use eval::{Context, Flow};
pub use value::{Lambda, NativeObject, Value};

use std::rc::Rc;

/// A script read and ready to run.
#[derive(Clone, Debug)]
pub struct Script {
    pub source: String,
    program: Rc<ast::Program>,
}

/// Why a script could not be run: it could not be read, or it failed while
/// running. Both carry where in the source it happened.
#[derive(Debug, Clone)]
pub struct ScriptError {
    /// `compile error` or `runtime error`
    pub kind: &'static str,
    pub message: String,
    pub offset: usize,
    pub source: String,
    /// the exception type OpenSearch would have named as the cause
    pub cause: String,
}

impl ScriptError {
    /// The error as OpenSearch writes it: a `script_exception` whose cause
    /// names the underlying exception, with the position of the failure.
    pub fn to_json(&self) -> serde_json::Value {
        // the stack shows a window of 25 characters either side of the
        // fault, with an ellipsis where the script goes on
        let len = self.source.len();
        let offset = self.offset.min(len);
        let start = offset.saturating_sub(25);
        let end = (offset + 25).min(len);
        let mut snippet = String::new();
        if start > 0 {
            snippet.push_str("... ");
        }
        snippet.push_str(&self.source[start..end]);
        if end < len {
            snippet.push_str(" ...");
        }
        let lead = offset - start + if start > 0 { 4 } else { 0 };
        serde_json::json!({
            "type": "script_exception",
            "reason": self.kind,
            "script_stack": [snippet, format!("{}^---- HERE", " ".repeat(lead))],
            "script": self.source,
            "lang": "painless",
            "position": {"offset": offset, "start": start, "end": end},
            "caused_by": {"type": self.cause, "reason": self.message},
        })
    }
}

impl Script {
    pub fn compile(source: &str) -> Result<Script, ScriptError> {
        let toks = lexer::lex(source).map_err(|e| ScriptError {
            kind: "compile error",
            message: e.message,
            offset: e.at,
            source: source.to_string(),
            cause: "illegal_argument_exception".into(),
        })?;
        let program = parser::parse(toks).map_err(|e| ScriptError {
            kind: "compile error",
            message: e.message,
            offset: e.at,
            source: source.to_string(),
            cause: "illegal_argument_exception".into(),
        })?;
        Ok(Script { source: source.to_string(), program: Rc::new(program) })
    }

    pub fn run(&self, context: &mut dyn Context) -> Result<Value, ScriptError> {
        match eval::Interpreter::run(&self.program, context) {
            Ok(v) => Ok(v),
            Err(Flow::Return(v)) => Ok(v),
            Err(Flow::Throw(v)) => Err(ScriptError {
                kind: "runtime error",
                message: v.as_text(),
                offset: 0,
                source: self.source.clone(),
                cause: match &v {
                    Value::Error(_) => "illegal_argument_exception".into(),
                    _ => "runtime_exception".into(),
                },
            }),
            Err(Flow::Error(message, offset)) => Err(ScriptError {
                kind: "runtime error",
                cause: cause_of(&message),
                message,
                offset,
                source: self.source.clone(),
            }),
            Err(Flow::Break) | Err(Flow::Continue) => Ok(Value::Null),
        }
    }
}

/// The Java exception a runtime message would have come with.
fn cause_of(message: &str) -> String {
    if message.contains("null value") || message.contains("to null") {
        "null_pointer_exception".into()
    } else if message.contains("/ by zero") {
        "arithmetic_exception".into()
    } else if message.contains("out of bounds") || message.contains("out of range") {
        "index_out_of_bounds_exception".into()
    } else if message.contains("cannot be modified") || message.contains("unsupported") {
        "unsupported_operation_exception".into()
    } else if message.contains("Cannot cast") {
        "class_cast_exception".into()
    } else if message.starts_with("For input string") {
        "number_format_exception".into()
    } else if message.contains("maximum number of statements") {
        "painless_error".into()
    } else {
        "illegal_argument_exception".into()
    }
}

/// A context holding named values and nothing else, which is what the
/// simplest scripts -- and the tests -- need.
pub struct Bindings {
    pub names: Vec<(String, Value)>,
}

impl Bindings {
    pub fn new() -> Bindings {
        Bindings { names: Vec::new() }
    }
    pub fn with(mut self, name: &str, value: Value) -> Bindings {
        self.names.push((name.to_string(), value));
        self
    }
}

impl Default for Bindings {
    fn default() -> Self {
        Bindings::new()
    }
}

impl Context for Bindings {
    fn lookup(&self, name: &str) -> Option<Value> {
        self.names.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone())
    }
    fn call(&mut self, _name: &str, _args: &[Value]) -> Option<Result<Value, String>> {
        None
    }
}

/// Params as a script sees them: a map it may read but not write.
pub struct Params(pub Value);

impl NativeObject for Params {
    fn get(&self, name: &str) -> Option<Value> {
        match &self.0 {
            Value::Map(m) => Some(value::map_get(m, &Value::str(name)).unwrap_or(Value::Null)),
            _ => None,
        }
    }
    fn call(&self, name: &str, args: &[Value]) -> Option<Result<Value, String>> {
        match name {
            "__set__" | "put" | "remove" | "clear" => {
                Some(Err("Unsupported operation: the params map cannot be modified".to_string()))
            }
            "__all__" => Some(Ok(self.0.clone())),
            "__index__" => Some(Ok(match &self.0 {
                Value::Map(m) => {
                    value::map_get(m, args.first().unwrap_or(&Value::Null)).unwrap_or(Value::Null)
                }
                _ => Value::Null,
            })),
            _ => match &self.0 {
                Value::Map(m) => {
                    let mut noop = |_: &Rc<Lambda>, _: Vec<Value>| Ok(Value::Null);
                    Some(builtins::call_method(&Value::Map(m.clone()), name, args, &mut noop))
                }
                _ => None,
            },
        }
    }
    fn describe(&self) -> String {
        self.0.as_text()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &str) -> Value {
        let script = Script::compile(src).unwrap_or_else(|e| panic!("{}: {}", e.kind, e.message));
        let params = Value::Native(Rc::new(Params(Value::from_json(
            &serde_json::json!({"x": 3, "name": "ann", "f": 2.5}),
        ))));
        let mut ctx = Bindings::new().with("params", params);
        script.run(&mut ctx).unwrap_or_else(|e| panic!("{}: {}", e.kind, e.message))
    }

    #[test]
    fn arithmetic_keeps_java_types() {
        assert_eq!(run("1 + 2").as_text(), "3");
        assert_eq!(run("7 / 2").as_text(), "3");
        assert_eq!(run("7 / 2.0").as_text(), "3.5");
        assert_eq!(run("params.x * 2").as_text(), "6");
        assert_eq!(run("params.f + 1").as_text(), "3.5");
        assert_eq!(run("'a' + 1 + 2").as_text(), "a12");
        assert_eq!(run("100.0 / 1000.0").as_text(), "0.1");
    }

    #[test]
    fn statements_and_functions() {
        assert_eq!(
            run("int s = 0; for (int i = 0; i < 4; i++) { s += i; } return s").as_text(),
            "6"
        );
        assert_eq!(
            run("def l = [3, 1, 2]; l.sort((a, b) -> a - b); return l").as_text(),
            "[1, 2, 3]"
        );
        assert_eq!(run("def m = [:]; m.a = 1; m['b'] = 2; return m.size()").as_text(), "2");
        assert_eq!(run("int twice(int n) { return n * 2 } return twice(21)").as_text(), "42");
        assert_eq!(run("def s = null; return s?.length()").as_text(), "null");
        assert_eq!(run("def s = null; return s?.length() ?: 7").as_text(), "7");
        assert_eq!(run("params.name.toUpperCase()").as_text(), "ANN");
        assert_eq!(
            run("if (params.x > 2) { return 'big' } else { return 'small' }").as_text(),
            "big"
        );
        assert_eq!(run("def out = []; for (def v : [1, 2, 3]) { if (v == 2) continue; out.add(v) } return out").as_text(), "[1, 3]");
        assert_eq!(run("Math.max(1, 2.5)").as_text(), "2.5");
        assert_eq!(run("'a,b'.split(',').length").as_text(), "2");
        assert_eq!(run("def x = [:] ; def y = [:] ; x.a = 1 ; return x.a").as_text(), "1");
    }

    #[test]
    fn errors_say_where() {
        let e = Script::compile("_score * foo bar + 1").unwrap_err();
        assert_eq!(e.kind, "compile error");
        assert_eq!(e.offset, 13);
        let script = Script::compile("params.that = 3").unwrap();
        let params = Value::Native(Rc::new(Params(Value::from_json(&serde_json::json!({})))));
        let mut ctx = Bindings::new().with("params", params);
        let e = script.run(&mut ctx).unwrap_err();
        assert_eq!(e.cause, "unsupported_operation_exception");
        let mut ctx = Bindings::new();
        let e = Script::compile("while (true) {}").unwrap().run(&mut ctx).unwrap_err();
        assert!(e.message.contains("maximum number of statements"));
    }

    #[test]
    fn dates() {
        assert_eq!(
            run("ZonedDateTime.parse('2021-03-04T05:06:07Z').toInstant().toEpochMilli()").as_text(),
            "1614834367000"
        );
        assert_eq!(
            run("ZonedDateTime.parse('2021-03-04T05:06:07Z').dayOfWeekEnum.value").as_text(),
            "4"
        );
    }
}
