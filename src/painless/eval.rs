//! Running a program.
//!
//! A tree walk over the statements with Java's rules for what the operators
//! do to each kind of value: integer arithmetic stays integral, `+` on a
//! string concatenates, `==` compares by value, a null on the left of `?.`
//! makes the whole thing null. Every step is counted, so a script that never
//! ends is stopped.

use std::rc::Rc;

use super::ast::*;
use super::builtins;
use super::value::*;

/// Why a script stopped: an error, or a statement that leaves the block.
pub enum Flow {
    Return(Value),
    Break,
    Continue,
    Throw(Value),
    /// an error with the offset of the expression it happened in
    Error(String, usize),
}

pub type Fallible = Result<Value, Flow>;

pub struct Scope {
    frames: Vec<Vec<(String, Value)>>,
}

impl Scope {
    fn new() -> Scope {
        Scope { frames: vec![Vec::new()] }
    }
    fn push(&mut self) {
        self.frames.push(Vec::new());
    }
    fn pop(&mut self) {
        self.frames.pop();
    }
    pub fn get(&self, name: &str) -> Option<Value> {
        for frame in self.frames.iter().rev() {
            if let Some((_, v)) = frame.iter().rev().find(|(n, _)| n == name) {
                return Some(v.clone());
            }
        }
        None
    }
    pub fn declare(&mut self, name: &str, value: Value) {
        if let Some(frame) = self.frames.last_mut() {
            frame.push((name.to_string(), value));
        }
    }
    fn set(&mut self, name: &str, value: Value) -> bool {
        for frame in self.frames.iter_mut().rev() {
            if let Some(slot) = frame.iter_mut().rev().find(|(n, _)| n == name) {
                slot.1 = value;
                return true;
            }
        }
        false
    }
    fn snapshot(&self) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        for frame in &self.frames {
            for (n, v) in frame {
                out.push((n.clone(), v.clone()));
            }
        }
        out
    }
}

/// What a context lends a script: the names it can read, and the functions
/// it may call by name.
pub trait Context {
    /// a name that is not a variable: `doc`, `params`, `ctx`, `_score`
    fn lookup(&self, name: &str) -> Option<Value>;
    /// a bare call the context answers: `emit(v)`, `saturation(a, b)`
    fn call(&mut self, name: &str, args: &[Value]) -> Option<Result<Value, String>>;
}

pub struct Interpreter<'a> {
    pub functions: &'a [Function],
    pub context: &'a mut dyn Context,
    pub steps: u64,
    pub max_steps: u64,
}

impl<'a> Interpreter<'a> {
    pub fn run(program: &'a Program, context: &'a mut dyn Context) -> Result<Value, Flow> {
        let mut it =
            Interpreter { functions: &program.functions, context, steps: 0, max_steps: 5_000_000 };
        let mut scope = Scope::new();
        match it.block(&program.body, &mut scope, false) {
            Ok(v) => Ok(v),
            Err(Flow::Return(v)) => Ok(v),
            Err(other) => Err(other),
        }
    }

    fn tick(&mut self, at: usize) -> Result<(), Flow> {
        self.steps += 1;
        if self.steps > self.max_steps {
            return Err(Flow::Error(
                "The maximum number of statements that can be executed in a loop has been reached."
                    .into(),
                at,
            ));
        }
        Ok(())
    }

    /// The last expression's value is what a block evaluates to, which is
    /// what a script without a `return` answers with.
    fn block(&mut self, stmts: &[Stmt], scope: &mut Scope, fresh: bool) -> Fallible {
        if fresh {
            scope.push();
        }
        let mut last = Value::Null;
        let out = (|| {
            for s in stmts {
                last = self.stmt(s, scope)?;
            }
            Ok(last)
        })();
        if fresh {
            scope.pop();
        }
        out
    }

    fn stmt(&mut self, s: &Stmt, scope: &mut Scope) -> Fallible {
        match s {
            Stmt::Declare { name, init, at } => {
                self.tick(*at)?;
                let v = match init {
                    Some(e) => self.expr(e, scope)?,
                    None => Value::Null,
                };
                scope.declare(name, v);
                Ok(Value::Null)
            }
            Stmt::Expr(e) => self.expr(e, scope),
            Stmt::If { cond, then, otherwise } => {
                let c = self.expr(cond, scope)?;
                if self.truth(&c, cond_at(cond))? {
                    self.block(then, scope, true)
                } else if let Some(o) = otherwise {
                    self.block(o, scope, true)
                } else {
                    Ok(Value::Null)
                }
            }
            Stmt::While { cond, body } => {
                loop {
                    self.tick(cond_at(cond))?;
                    let c = self.expr(cond, scope)?;
                    if !self.truth(&c, cond_at(cond))? {
                        break;
                    }
                    match self.block(body, scope, true) {
                        Err(Flow::Break) => break,
                        Err(Flow::Continue) => continue,
                        Err(e) => return Err(e),
                        Ok(_) => {}
                    }
                }
                Ok(Value::Null)
            }
            Stmt::DoWhile { body, cond } => {
                loop {
                    self.tick(cond_at(cond))?;
                    match self.block(body, scope, true) {
                        Err(Flow::Break) => break,
                        Err(Flow::Continue) => {}
                        Err(e) => return Err(e),
                        Ok(_) => {}
                    }
                    let c = self.expr(cond, scope)?;
                    if !self.truth(&c, cond_at(cond))? {
                        break;
                    }
                }
                Ok(Value::Null)
            }
            Stmt::For { init, cond, step, body } => {
                scope.push();
                let out = (|| {
                    if let Some(i) = init {
                        self.stmt(i, scope)?;
                    }
                    loop {
                        self.tick(0)?;
                        if let Some(c) = cond {
                            let v = self.expr(c, scope)?;
                            if !self.truth(&v, cond_at(c))? {
                                break;
                            }
                        }
                        match self.block(body, scope, true) {
                            Err(Flow::Break) => break,
                            Err(Flow::Continue) => {}
                            Err(e) => return Err(e),
                            Ok(_) => {}
                        }
                        if let Some(s) = step {
                            self.expr(s, scope)?;
                        }
                    }
                    Ok(Value::Null)
                })();
                scope.pop();
                out
            }
            Stmt::ForEach { name, over, body } => {
                let items = self.expr(over, scope)?;
                let list: Vec<Value> = match &items {
                    Value::List(l) => l.borrow().clone(),
                    Value::DocValues(d) => d.values.clone(),
                    Value::Map(m) => m.borrow().iter().map(|(k, _)| k.clone()).collect(),
                    Value::Str(s) => s.chars().map(|c| Value::str(&c.to_string())).collect(),
                    Value::Null => {
                        return Err(Flow::Error("Cannot iterate over null".into(), cond_at(over)));
                    }
                    other => {
                        return Err(Flow::Error(
                            format!("Cannot iterate over [{}]", other.type_name()),
                            cond_at(over),
                        ));
                    }
                };
                for item in list {
                    self.tick(0)?;
                    scope.push();
                    scope.declare(name, item);
                    let r = self.block(body, scope, false);
                    scope.pop();
                    match r {
                        Err(Flow::Break) => break,
                        Err(Flow::Continue) => continue,
                        Err(e) => return Err(e),
                        Ok(_) => {}
                    }
                }
                Ok(Value::Null)
            }
            Stmt::Return(e, _) => {
                let v = match e {
                    Some(e) => self.expr(e, scope)?,
                    None => Value::Null,
                };
                Err(Flow::Return(v))
            }
            Stmt::Throw(e, _) => {
                let v = self.expr(e, scope)?;
                Err(Flow::Throw(v))
            }
            Stmt::Break => Err(Flow::Break),
            Stmt::Continue => Err(Flow::Continue),
            Stmt::Block(b) => self.block(b, scope, true),
            Stmt::Try { body, catch_name, catch_body } => match self.block(body, scope, true) {
                Err(Flow::Throw(v)) if catch_name.is_some() => {
                    self.caught(v, catch_name, catch_body, scope)
                }
                Err(Flow::Error(m, _)) if catch_name.is_some() => {
                    let v = Value::Error(Rc::from(m.as_str()));
                    self.caught(v, catch_name, catch_body, scope)
                }
                other => other,
            },
        }
    }

    /// What a `catch` does with what was thrown.
    fn caught(
        &mut self,
        thrown: Value,
        name: &Option<String>,
        body: &[Stmt],
        scope: &mut Scope,
    ) -> Fallible {
        scope.push();
        scope.declare(name.as_deref().unwrap_or("e"), thrown);
        let r = self.block(body, scope, false);
        scope.pop();
        r
    }

    fn truth(&self, v: &Value, at: usize) -> Result<bool, Flow> {
        match v {
            Value::Bool(b) => Ok(*b),
            Value::Null => Err(Flow::Error("cannot cast null to boolean".into(), at)),
            other => Err(Flow::Error(
                format!("Cannot cast from [{}] to [boolean].", other.type_name()),
                at,
            )),
        }
    }

    pub fn expr(&mut self, e: &Expr, scope: &mut Scope) -> Fallible {
        match e {
            Expr::Null => Ok(Value::Null),
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            Expr::Int(i) => Ok(Value::Int(*i)),
            Expr::Long(i) => Ok(Value::Long(*i)),
            Expr::Float(f) => Ok(Value::Float(*f)),
            Expr::Double(f) => Ok(Value::Double(*f)),
            Expr::Str(s) => Ok(Value::str(s)),
            Expr::Regex(p, flags) => {
                let mut pattern = String::new();
                if flags.contains('i') {
                    pattern.push_str("(?i)");
                }
                if flags.contains('m') {
                    pattern.push_str("(?m)");
                }
                if flags.contains('s') {
                    pattern.push_str("(?s)");
                }
                pattern.push_str(p);
                match regex::Regex::new(&pattern) {
                    Ok(r) => Ok(Value::Regex(Rc::new(r))),
                    Err(e) => Err(Flow::Error(format!("invalid regex: {e}"), 0)),
                }
            }
            Expr::Ident(name, at) => {
                if let Some(v) = scope.get(name) {
                    return Ok(v);
                }
                if let Some(v) = self.context.lookup(name) {
                    return Ok(v);
                }
                Err(Flow::Error(format!("cannot resolve symbol [{name}]"), *at))
            }
            Expr::List(items) => {
                let mut out = Vec::with_capacity(items.len());
                for i in items {
                    out.push(self.expr(i, scope)?);
                }
                Ok(Value::list(out))
            }
            Expr::Map(pairs) => {
                let mut out = Vec::with_capacity(pairs.len());
                for (k, v) in pairs {
                    let k = self.expr(k, scope)?;
                    let v = self.expr(v, scope)?;
                    out.push((k, v));
                }
                Ok(Value::map(out))
            }
            Expr::Field { target, name, safe, at } => {
                let t = self.expr(target, scope)?;
                if t.is_null() {
                    if *safe {
                        return Ok(Value::Null);
                    }
                    return Err(Flow::Error(
                        format!("cannot access [{name}] on a null value"),
                        *at,
                    ));
                }
                builtins::get_field(&t, name).map_err(|m| Flow::Error(m, *at))
            }
            Expr::Index { target, index, at } => {
                let t = self.expr(target, scope)?;
                let i = self.expr(index, scope)?;
                builtins::get_index(&t, &i).map_err(|m| Flow::Error(m, *at))
            }
            Expr::Call { target, name, args, safe, at } => {
                let t = self.expr(target, scope)?;
                if t.is_null() {
                    if *safe {
                        return Ok(Value::Null);
                    }
                    return Err(Flow::Error(
                        format!("cannot invoke [{name}] on a null value"),
                        *at,
                    ));
                }
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(self.expr(a, scope)?);
                }
                self.call_method(&t, name, vals, *at)
            }
            Expr::Invoke { name, args, at } => {
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(self.expr(a, scope)?);
                }
                // a lambda held in a variable may be called by its name
                if let Some(Value::Lambda(l)) = scope.get(name) {
                    return self.call_lambda(&l, vals, *at);
                }
                if let Some(f) = self.functions.iter().find(|f| f.name == *name) {
                    let f = f.clone();
                    return self.call_function(&f, vals, *at);
                }
                match self.context.call(name, &vals) {
                    Some(Ok(v)) => Ok(v),
                    Some(Err(m)) => Err(Flow::Error(m, *at)),
                    None => builtins::call_free(name, &vals).map_err(|m| Flow::Error(m, *at)),
                }
            }
            Expr::New { class, args, at } => {
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(self.expr(a, scope)?);
                }
                builtins::construct(class, &vals).map_err(|m| Flow::Error(m, *at))
            }
            Expr::Static { class, name, args, at } => match args {
                Some(args) => {
                    let mut vals = Vec::with_capacity(args.len());
                    for a in args {
                        vals.push(self.expr(a, scope)?);
                    }
                    // the callback lets a static method that takes a lambda
                    // -- `Collections.sort(list, cmp)` -- call back into here
                    let mut call = |l: &Rc<Lambda>, a: Vec<Value>| self.call_lambda(l, a, *at);
                    builtins::call_static(class, name, &vals, &mut call)
                        .map_err(|m| Flow::Error(m, *at))
                }
                None => builtins::static_field(class, name).map_err(|m| Flow::Error(m, *at)),
            },
            Expr::Unary { op, expr, at } => {
                let v = self.expr(expr, scope)?;
                builtins::unary(op, &v).map_err(|m| Flow::Error(m, *at))
            }
            Expr::Binary { op, left, right, at } => {
                let l = self.expr(left, scope)?;
                // the short circuits
                match *op {
                    "&&" => {
                        if !self.truth(&l, *at)? {
                            return Ok(Value::Bool(false));
                        }
                        let r = self.expr(right, scope)?;
                        return Ok(Value::Bool(self.truth(&r, *at)?));
                    }
                    "||" => {
                        if self.truth(&l, *at)? {
                            return Ok(Value::Bool(true));
                        }
                        let r = self.expr(right, scope)?;
                        return Ok(Value::Bool(self.truth(&r, *at)?));
                    }
                    _ => {}
                }
                let r = self.expr(right, scope)?;
                builtins::binary(op, &l, &r).map_err(|m| Flow::Error(m, *at))
            }
            Expr::Conditional { cond, then, otherwise } => {
                let c = self.expr(cond, scope)?;
                if self.truth(&c, cond_at(cond))? {
                    self.expr(then, scope)
                } else {
                    self.expr(otherwise, scope)
                }
            }
            Expr::Elvis { value, fallback } => {
                let v = self.expr(value, scope)?;
                if v.is_null() { self.expr(fallback, scope) } else { Ok(v) }
            }
            Expr::Assign { target, op, value, at } => {
                let rhs = self.expr(value, scope)?;
                let new = if *op == "=" {
                    rhs
                } else {
                    let current = self.expr(target, scope)?;
                    let base = &op[..op.len() - 1];
                    builtins::binary(base, &current, &rhs).map_err(|m| Flow::Error(m, *at))?
                };
                self.assign(target, new.clone(), scope, *at)?;
                Ok(new)
            }
            Expr::Step { target, delta, prefix, at } => {
                let current = self.expr(target, scope)?;
                let new = builtins::binary("+", &current, &Value::Int(*delta))
                    .map_err(|m| Flow::Error(m, *at))?;
                self.assign(target, new.clone(), scope, *at)?;
                Ok(if *prefix { new } else { current })
            }
            Expr::Cast { class, expr } => {
                let v = self.expr(expr, scope)?;
                builtins::cast(class, v).map_err(|m| Flow::Error(m, 0))
            }
            Expr::InstanceOf { expr, class } => {
                let v = self.expr(expr, scope)?;
                Ok(Value::Bool(builtins::instance_of(&v, class)))
            }
            Expr::Lambda { params, body } => Ok(Value::Lambda(Rc::new(Lambda {
                params: params.clone(),
                body: body.clone(),
                captured: scope.snapshot(),
                method: None,
            }))),
            Expr::MethodRef { class, name } => Ok(Value::Lambda(Rc::new(Lambda {
                params: Vec::new(),
                body: Vec::new(),
                captured: Vec::new(),
                method: Some((class.clone(), name.clone())),
            }))),
        }
    }

    fn assign(
        &mut self,
        target: &Expr,
        value: Value,
        scope: &mut Scope,
        at: usize,
    ) -> Result<(), Flow> {
        match target {
            Expr::Ident(name, _) => {
                if scope.set(name, value.clone()) {
                    return Ok(());
                }
                // an assignment to a name the context lends is refused: the
                // context's objects are written through their fields
                if self.context.lookup(name).is_some() {
                    return Err(Flow::Error(format!("cannot assign to [{name}]"), at));
                }
                scope.declare(name, value);
                Ok(())
            }
            Expr::Field { target, name, .. } => {
                let t = self.expr(target, scope)?;
                builtins::set_field(&t, name, value).map_err(|m| Flow::Error(m, at))
            }
            Expr::Index { target, index, .. } => {
                let t = self.expr(target, scope)?;
                let i = self.expr(index, scope)?;
                builtins::set_index(&t, &i, value).map_err(|m| Flow::Error(m, at))
            }
            _ => Err(Flow::Error("invalid assignment target".into(), at)),
        }
    }

    fn call_function(&mut self, f: &Function, args: Vec<Value>, at: usize) -> Fallible {
        if args.len() != f.params.len() {
            return Err(Flow::Error(
                format!("[{}] takes {} arguments", f.name, f.params.len()),
                at,
            ));
        }
        let mut scope = Scope::new();
        for (p, a) in f.params.iter().zip(args) {
            scope.declare(p, a);
        }
        match self.block(&f.body, &mut scope, false) {
            Ok(v) => Ok(v),
            Err(Flow::Return(v)) => Ok(v),
            Err(e) => Err(e),
        }
    }

    pub fn call_lambda(&mut self, l: &Rc<Lambda>, args: Vec<Value>, at: usize) -> Fallible {
        if let Some((class, name)) = &l.method {
            // `this::f` names a function of the script; `String::valueOf` a
            // static method; `Integer::compare` likewise
            if class == "this" {
                if let Some(f) = self.functions.iter().find(|f| f.name == *name) {
                    let f = f.clone();
                    return self.call_function(&f, args, at);
                }
                return Err(Flow::Error(format!("unknown function [{name}]"), at));
            }
            let mut call = |l: &Rc<Lambda>, a: Vec<Value>| self.call_lambda(l, a, at);
            return builtins::call_static(class, name, &args, &mut call)
                .map_err(|m| Flow::Error(m, at));
        }
        let mut scope = Scope::new();
        for (n, v) in &l.captured {
            scope.declare(n, v.clone());
        }
        scope.push();
        for (i, p) in l.params.iter().enumerate() {
            scope.declare(p, args.get(i).cloned().unwrap_or(Value::Null));
        }
        match self.block(&l.body, &mut scope, false) {
            Ok(v) => Ok(v),
            Err(Flow::Return(v)) => Ok(v),
            Err(e) => Err(e),
        }
    }

    fn call_method(&mut self, target: &Value, name: &str, args: Vec<Value>, at: usize) -> Fallible {
        // a lambda-valued object answers the functional interface's method
        if let Value::Lambda(l) = target {
            return match name {
                "get" | "apply" | "accept" | "test" | "run" | "call" | "compare" | "applyAsInt"
                | "applyAsLong" | "applyAsDouble" => self.call_lambda(l, args, at),
                _ => Err(Flow::Error(format!("unknown method [{name}] on lambda"), at)),
            };
        }
        if let Value::Native(n) = target
            && let Some(r) = n.call(name, &args)
        {
            return r.map_err(|m| Flow::Error(m, at));
        }
        let mut call = |l: &Rc<Lambda>, a: Vec<Value>| self.call_lambda(l, a, at);
        builtins::call_method(target, name, &args, &mut call).map_err(|m| Flow::Error(m, at))
    }
}

fn cond_at(e: &Expr) -> usize {
    match e {
        Expr::Ident(_, at)
        | Expr::Field { at, .. }
        | Expr::Index { at, .. }
        | Expr::Call { at, .. }
        | Expr::Invoke { at, .. }
        | Expr::New { at, .. }
        | Expr::Static { at, .. }
        | Expr::Unary { at, .. }
        | Expr::Binary { at, .. }
        | Expr::Assign { at, .. }
        | Expr::Step { at, .. } => *at,
        _ => 0,
    }
}
