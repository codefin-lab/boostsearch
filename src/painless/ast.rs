//! What a script says, once read.

/// A whole script: the functions it declares and the statements it runs.
#[derive(Debug, Clone)]
pub struct Program {
    pub functions: Vec<Function>,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub struct Function {
    pub name: String,
    pub params: Vec<String>,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    /// `def x = …`, `int x`, `List y = …`
    Declare {
        name: String,
        init: Option<Expr>,
        at: usize,
    },
    Expr(Expr),
    If {
        cond: Expr,
        then: Vec<Stmt>,
        otherwise: Option<Vec<Stmt>>,
    },
    While {
        cond: Expr,
        body: Vec<Stmt>,
    },
    DoWhile {
        body: Vec<Stmt>,
        cond: Expr,
    },
    For {
        init: Option<Box<Stmt>>,
        cond: Option<Expr>,
        step: Option<Expr>,
        body: Vec<Stmt>,
    },
    ForEach {
        name: String,
        over: Expr,
        body: Vec<Stmt>,
    },
    Return(Option<Expr>, usize),
    Throw(Expr, usize),
    Break,
    Continue,
    Block(Vec<Stmt>),
    Try {
        body: Vec<Stmt>,
        catch_name: Option<String>,
        catch_body: Vec<Stmt>,
    },
}

#[derive(Debug, Clone)]
pub enum Expr {
    Null,
    Bool(bool),
    Int(i64),
    Long(i64),
    Float(f64),
    Double(f64),
    Str(String),
    Regex(String, String),
    Ident(String, usize),
    /// `[a, b]`
    List(Vec<Expr>),
    /// `[k: v]` or `[:]`
    Map(Vec<(Expr, Expr)>),
    /// `a.b` -- also `a?.b`
    Field {
        target: Box<Expr>,
        name: String,
        safe: bool,
        at: usize,
    },
    /// `a[i]`
    Index {
        target: Box<Expr>,
        index: Box<Expr>,
        at: usize,
    },
    /// `a.b(c)` -- also `a?.b(c)`
    Call {
        target: Box<Expr>,
        name: String,
        args: Vec<Expr>,
        safe: bool,
        at: usize,
    },
    /// `f(c)` -- a function of the script's own, or one the context lends
    Invoke {
        name: String,
        args: Vec<Expr>,
        at: usize,
    },
    /// `new T(args)`
    New {
        class: String,
        args: Vec<Expr>,
        at: usize,
    },
    /// `T.method(args)` and `T.FIELD`
    Static {
        class: String,
        name: String,
        args: Option<Vec<Expr>>,
        at: usize,
    },
    Unary {
        op: &'static str,
        expr: Box<Expr>,
        at: usize,
    },
    Binary {
        op: &'static str,
        left: Box<Expr>,
        right: Box<Expr>,
        at: usize,
    },
    /// `a ? b : c`
    Conditional {
        cond: Box<Expr>,
        then: Box<Expr>,
        otherwise: Box<Expr>,
    },
    /// `a ?: b`
    Elvis {
        value: Box<Expr>,
        fallback: Box<Expr>,
    },
    /// `x = v`, `x += v`, and the rest
    Assign {
        target: Box<Expr>,
        op: &'static str,
        value: Box<Expr>,
        at: usize,
    },
    /// `x++`, `--x`
    Step {
        target: Box<Expr>,
        delta: i64,
        prefix: bool,
        at: usize,
    },
    /// `(T) x`
    Cast {
        class: String,
        expr: Box<Expr>,
    },
    /// `x instanceof T`
    InstanceOf {
        expr: Box<Expr>,
        class: String,
    },
    /// `(a, b) -> a + b` and `x -> { … }`
    Lambda {
        params: Vec<String>,
        body: Vec<Stmt>,
    },
    /// `T::method` and `this::f`
    MethodRef {
        class: String,
        name: String,
    },
}
