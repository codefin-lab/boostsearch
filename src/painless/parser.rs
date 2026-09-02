//! Reading tokens into a program.
//!
//! A hand-written recursive descent parser with precedence climbing for the
//! expressions. It knows the Java-shaped grammar Painless has: declarations
//! with a type in front, the usual statements, lambdas, casts, and the few
//! operators Painless adds.

use super::ast::*;
use super::lexer::{Tok, Token};

#[derive(Debug, Clone)]
pub struct ParseError {
    pub message: String,
    pub at: usize,
}

struct Parser {
    toks: Vec<Token>,
    pos: usize,
}

/// Names that begin a declaration rather than an expression.
const TYPE_WORDS: &[&str] = &[
    "def",
    "int",
    "long",
    "float",
    "double",
    "boolean",
    "byte",
    "short",
    "char",
    "void",
    "String",
    "List",
    "Map",
    "Set",
    "ArrayList",
    "HashMap",
    "HashSet",
    "Object",
    "Integer",
    "Long",
    "Double",
    "Float",
    "Boolean",
    "Character",
    "Number",
    "BigDecimal",
    "BigInteger",
    "ZonedDateTime",
    "Instant",
    "LocalDate",
    "LocalDateTime",
    "Supplier",
    "Function",
    "BiFunction",
    "Consumer",
    "Predicate",
    "Comparator",
    "Runnable",
    "Iterator",
    "StringBuilder",
    "Pattern",
    "Matcher",
    "Exception",
    "Collection",
    "Iterable",
    "Random",
    "Date",
    "CharSequence",
    "Deque",
    "Queue",
];

pub fn parse(toks: Vec<Token>) -> Result<Program, ParseError> {
    let mut p = Parser { toks, pos: 0 };
    let mut functions = Vec::new();
    let mut body = Vec::new();
    while !p.at_end() {
        if p.looks_like_function() {
            functions.push(p.function()?);
        } else {
            body.push(p.statement()?);
        }
    }
    Ok(Program { functions, body })
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].kind
    }
    fn peek_at(&self, n: usize) -> &Tok {
        &self.toks[(self.pos + n).min(self.toks.len() - 1)].kind
    }
    fn here(&self) -> usize {
        self.toks[self.pos].at
    }
    fn at_end(&self) -> bool {
        matches!(self.peek(), Tok::End)
    }
    fn next(&mut self) -> Token {
        let t = self.toks[self.pos].clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }
    fn is_op(&self, op: &str) -> bool {
        matches!(self.peek(), Tok::Op(o) if *o == op)
    }
    fn eat_op(&mut self, op: &str) -> bool {
        if self.is_op(op) {
            self.next();
            true
        } else {
            false
        }
    }
    fn expect_op(&mut self, op: &str) -> Result<(), ParseError> {
        if self.eat_op(op) { Ok(()) } else { Err(self.error(format!("expected [{op}]"))) }
    }
    fn is_ident(&self, word: &str) -> bool {
        matches!(self.peek(), Tok::Ident(w) if w == word)
    }
    fn error(&self, message: String) -> ParseError {
        ParseError { message, at: self.here() }
    }
    fn ident(&mut self) -> Result<String, ParseError> {
        match self.next().kind {
            Tok::Ident(w) => Ok(w),
            other => Err(self.error(format!("expected a name, found {other:?}"))),
        }
    }

    /// `type name(type a, type b) { … }`
    fn looks_like_function(&self) -> bool {
        let is_type = matches!(self.peek(), Tok::Ident(w) if is_type_word(w));
        is_type
            && matches!(self.peek_at(1), Tok::Ident(_))
            && matches!(self.peek_at(2), Tok::Op("("))
            && self.paren_closes_before_brace()
    }

    fn paren_closes_before_brace(&self) -> bool {
        let mut depth = 0;
        let mut i = self.pos + 2;
        while i < self.toks.len() {
            match &self.toks[i].kind {
                Tok::Op("(") => depth += 1,
                Tok::Op(")") => {
                    depth -= 1;
                    if depth == 0 {
                        return matches!(self.toks.get(i + 1).map(|t| &t.kind), Some(Tok::Op("{")));
                    }
                }
                Tok::End => return false,
                _ => {}
            }
            i += 1;
        }
        false
    }

    fn function(&mut self) -> Result<Function, ParseError> {
        self.ident()?; // return type
        let name = self.ident()?;
        self.expect_op("(")?;
        let mut params = Vec::new();
        while !self.is_op(")") {
            self.type_name()?;
            params.push(self.ident()?);
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op(")")?;
        let body = self.block()?;
        Ok(Function { name, params, body })
    }

    /// A type, possibly generic or an array: `Map<String,Object>`, `int[]`
    fn type_name(&mut self) -> Result<String, ParseError> {
        let mut name = self.ident()?;
        while self.eat_op(".") {
            name.push('.');
            name.push_str(&self.ident()?);
        }
        if self.is_op("<") {
            let mut depth = 0;
            loop {
                match self.next().kind {
                    Tok::Op("<") => depth += 1,
                    Tok::Op(">") => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    Tok::Op(">>") => {
                        depth -= 2;
                        if depth <= 0 {
                            break;
                        }
                    }
                    Tok::End => return Err(self.error("unclosed type".into())),
                    _ => {}
                }
            }
        }
        while self.is_op("[") && matches!(self.peek_at(1), Tok::Op("]")) {
            self.next();
            self.next();
            name.push_str("[]");
        }
        Ok(name)
    }

    fn block(&mut self) -> Result<Vec<Stmt>, ParseError> {
        self.expect_op("{")?;
        let mut out = Vec::new();
        while !self.is_op("}") {
            if self.at_end() {
                return Err(self.error("unexpected end of script".into()));
            }
            out.push(self.statement()?);
        }
        self.expect_op("}")?;
        Ok(out)
    }

    /// A statement, or a block where one may stand.
    fn body(&mut self) -> Result<Vec<Stmt>, ParseError> {
        if self.is_op("{") { self.block() } else { Ok(vec![self.statement()?]) }
    }

    fn end_statement(&mut self) -> Result<(), ParseError> {
        if self.eat_op(";") || self.is_op("}") || self.at_end() {
            Ok(())
        } else {
            Err(self.error("expected [;]".into()))
        }
    }

    fn statement(&mut self) -> Result<Stmt, ParseError> {
        let at = self.here();
        if self.is_op("{") {
            return Ok(Stmt::Block(self.block()?));
        }
        if self.eat_op(";") {
            return Ok(Stmt::Block(Vec::new()));
        }
        if let Tok::Ident(word) = self.peek().clone() {
            match word.as_str() {
                "if" => {
                    self.next();
                    self.expect_op("(")?;
                    let cond = self.expr()?;
                    self.expect_op(")")?;
                    let then = self.body()?;
                    let otherwise = if self.is_ident("else") {
                        self.next();
                        Some(self.body()?)
                    } else {
                        None
                    };
                    return Ok(Stmt::If { cond, then, otherwise });
                }
                "while" => {
                    self.next();
                    self.expect_op("(")?;
                    let cond = self.expr()?;
                    self.expect_op(")")?;
                    let body = self.body()?;
                    return Ok(Stmt::While { cond, body });
                }
                "do" => {
                    self.next();
                    let body = self.body()?;
                    if !self.is_ident("while") {
                        return Err(self.error("expected [while]".into()));
                    }
                    self.next();
                    self.expect_op("(")?;
                    let cond = self.expr()?;
                    self.expect_op(")")?;
                    self.end_statement()?;
                    return Ok(Stmt::DoWhile { body, cond });
                }
                "for" => {
                    self.next();
                    self.expect_op("(")?;
                    // `for (T x : xs)` walks a collection
                    let foreach = self.is_declaration_start()
                        && matches!(self.peek_at(1), Tok::Ident(_))
                        && matches!(self.peek_at(2), Tok::Op(":"));
                    if foreach {
                        self.type_name()?;
                        let name = self.ident()?;
                        self.expect_op(":")?;
                        let over = self.expr()?;
                        self.expect_op(")")?;
                        let body = self.body()?;
                        return Ok(Stmt::ForEach { name, over, body });
                    }
                    let init = if self.is_op(";") {
                        None
                    } else if self.is_declaration_start() {
                        Some(Box::new(self.declaration()?))
                    } else {
                        Some(Box::new(Stmt::Expr(self.expr()?)))
                    };
                    self.expect_op(";")?;
                    let cond = if self.is_op(";") { None } else { Some(self.expr()?) };
                    self.expect_op(";")?;
                    let step = if self.is_op(")") { None } else { Some(self.expr()?) };
                    self.expect_op(")")?;
                    let body = self.body()?;
                    return Ok(Stmt::For { init, cond, step, body });
                }
                "return" => {
                    self.next();
                    let value = if self.is_op(";") || self.is_op("}") || self.at_end() {
                        None
                    } else {
                        Some(self.expr()?)
                    };
                    self.end_statement()?;
                    return Ok(Stmt::Return(value, at));
                }
                "throw" => {
                    self.next();
                    let value = self.expr()?;
                    self.end_statement()?;
                    return Ok(Stmt::Throw(value, at));
                }
                "break" => {
                    self.next();
                    self.end_statement()?;
                    return Ok(Stmt::Break);
                }
                "continue" => {
                    self.next();
                    self.end_statement()?;
                    return Ok(Stmt::Continue);
                }
                "try" => {
                    self.next();
                    let body = self.block()?;
                    let mut catch_name = None;
                    let mut catch_body = Vec::new();
                    if self.is_ident("catch") {
                        self.next();
                        self.expect_op("(")?;
                        self.type_name()?;
                        catch_name = Some(self.ident()?);
                        self.expect_op(")")?;
                        catch_body = self.block()?;
                    }
                    return Ok(Stmt::Try { body, catch_name, catch_body });
                }
                _ => {}
            }
        }
        if self.is_declaration_start() {
            let d = self.declaration()?;
            self.end_statement()?;
            return Ok(d);
        }
        let e = self.expr()?;
        self.end_statement()?;
        Ok(Stmt::Expr(e))
    }

    /// `T name` -- a type word (or any name) followed by a name
    fn is_declaration_start(&self) -> bool {
        let Tok::Ident(word) = self.peek() else { return false };
        if !is_type_word(word)
            && !word.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false)
        {
            return false;
        }
        // skip a generic part or array brackets when looking ahead
        let mut i = self.pos + 1;
        let mut depth = 0;
        loop {
            match &self.toks[i].kind {
                Tok::Op("<") => depth += 1,
                Tok::Op(">") => depth -= 1,
                Tok::Op(">>") => depth -= 2,
                Tok::Op("[") if depth == 0 && matches!(self.toks[i + 1].kind, Tok::Op("]")) => {
                    i += 1;
                }
                Tok::Op(".") if depth == 0 => {}
                Tok::Ident(_) if depth == 0 => {
                    // `T name` where the previous was the type
                    return !matches!(
                        self.toks[i - 1].kind,
                        Tok::Op(".") | Tok::Op("<") | Tok::Op(",")
                    );
                }
                Tok::Ident(_) => {}
                Tok::Op(",") if depth > 0 => {}
                _ => return false,
            }
            i += 1;
            if i >= self.toks.len() {
                return false;
            }
        }
    }

    fn declaration(&mut self) -> Result<Stmt, ParseError> {
        let at = self.here();
        self.type_name()?;
        let name = self.ident()?;
        let init = if self.eat_op("=") { Some(self.expr()?) } else { None };
        // `int a = 1, b = 2` declares two; only the first is kept as one
        // statement here, the rest follow as their own
        if self.is_op(",") {
            return Err(self.error("one declaration per statement".into()));
        }
        Ok(Stmt::Declare { name, init, at })
    }

    pub fn expr(&mut self) -> Result<Expr, ParseError> {
        self.assignment()
    }

    fn assignment(&mut self) -> Result<Expr, ParseError> {
        let at = self.here();
        // a lambda: `x -> …` or `(a, b) -> …`
        if let Some(lambda) = self.try_lambda()? {
            return Ok(lambda);
        }
        let left = self.conditional()?;
        for op in ["=", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "<<=", ">>=", ">>>="] {
            if self.is_op(op) {
                self.next();
                let value = self.assignment()?;
                return Ok(Expr::Assign { target: Box::new(left), op, value: Box::new(value), at });
            }
        }
        Ok(left)
    }

    fn try_lambda(&mut self) -> Result<Option<Expr>, ParseError> {
        // `name -> body`
        if let (Tok::Ident(name), Tok::Op("->")) = (self.peek().clone(), self.peek_at(1).clone()) {
            self.next();
            self.next();
            let body = self.lambda_body()?;
            return Ok(Some(Expr::Lambda { params: vec![name], body }));
        }
        // `(a, b) -> body` and `(T a, T b) -> body`
        if self.is_op("(") {
            let mut i = self.pos + 1;
            let mut depth = 1;
            while i < self.toks.len() {
                match &self.toks[i].kind {
                    Tok::Op("(") => depth += 1,
                    Tok::Op(")") => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    Tok::End => return Ok(None),
                    _ => {}
                }
                i += 1;
            }
            if !matches!(self.toks.get(i + 1).map(|t| &t.kind), Some(Tok::Op("->"))) {
                return Ok(None);
            }
            self.next();
            let mut params = Vec::new();
            while !self.is_op(")") {
                // a typed parameter is two names; the last one is the name
                let first = self.ident()?;
                let name = if let Tok::Ident(_) = self.peek() { self.ident()? } else { first };
                params.push(name);
                if !self.eat_op(",") {
                    break;
                }
            }
            self.expect_op(")")?;
            self.expect_op("->")?;
            let body = self.lambda_body()?;
            return Ok(Some(Expr::Lambda { params, body }));
        }
        Ok(None)
    }

    fn lambda_body(&mut self) -> Result<Vec<Stmt>, ParseError> {
        if self.is_op("{") {
            self.block()
        } else {
            let at = self.here();
            let e = self.expr()?;
            Ok(vec![Stmt::Return(Some(e), at)])
        }
    }

    fn conditional(&mut self) -> Result<Expr, ParseError> {
        let cond = self.elvis()?;
        if self.eat_op("?") {
            let then = self.assignment()?;
            self.expect_op(":")?;
            let otherwise = self.assignment()?;
            return Ok(Expr::Conditional {
                cond: Box::new(cond),
                then: Box::new(then),
                otherwise: Box::new(otherwise),
            });
        }
        Ok(cond)
    }

    fn elvis(&mut self) -> Result<Expr, ParseError> {
        let value = self.binary(0)?;
        if self.eat_op("?:") {
            let fallback = self.elvis()?;
            return Ok(Expr::Elvis { value: Box::new(value), fallback: Box::new(fallback) });
        }
        Ok(value)
    }

    /// Precedence climbing over the binary operators, lowest first.
    fn binary(&mut self, level: usize) -> Result<Expr, ParseError> {
        const LEVELS: &[&[&str]] = &[
            &["||"],
            &["&&"],
            &["|"],
            &["^"],
            &["&"],
            &["==", "!=", "===", "!==", "=~", "==~"],
            &["<", ">", "<=", ">=", "instanceof"],
            &["<<", ">>", ">>>"],
            &["+", "-"],
            &["*", "/", "%"],
        ];
        if level >= LEVELS.len() {
            return self.unary();
        }
        let mut left = self.binary(level + 1)?;
        loop {
            let at = self.here();
            let found = LEVELS[level].iter().find(|op| {
                if **op == "instanceof" { self.is_ident("instanceof") } else { self.is_op(op) }
            });
            let Some(op) = found else { break };
            self.next();
            if *op == "instanceof" {
                let class = self.type_name()?;
                left = Expr::InstanceOf { expr: Box::new(left), class };
                continue;
            }
            let right = self.binary(level + 1)?;
            left = Expr::Binary { op, left: Box::new(left), right: Box::new(right), at };
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr, ParseError> {
        let at = self.here();
        for op in ["!", "-", "+", "~"] {
            if self.is_op(op) {
                self.next();
                let expr = self.unary()?;
                return Ok(Expr::Unary { op, expr: Box::new(expr), at });
            }
        }
        for (op, delta) in [("++", 1), ("--", -1)] {
            if self.is_op(op) {
                self.next();
                let target = self.unary()?;
                return Ok(Expr::Step { target: Box::new(target), delta, prefix: true, at });
            }
        }
        // a cast: `(int) x`, `(String) y`
        if self.is_op("(")
            && let Tok::Ident(word) = self.peek_at(1).clone()
            && (is_type_word(&word)
                || word.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false))
            && matches!(self.peek_at(2), Tok::Op(")"))
            && !matches!(
                self.peek_at(3),
                Tok::Op(".") | Tok::Op("->") | Tok::End | Tok::Op(")") | Tok::Op(";")
            )
        {
            self.next();
            let class = self.ident()?;
            self.expect_op(")")?;
            let expr = self.unary()?;
            return Ok(Expr::Cast { class, expr: Box::new(expr) });
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Expr, ParseError> {
        let mut e = self.primary()?;
        loop {
            let at = self.here();
            if self.is_op(".") || self.is_op("?.") {
                let safe = self.is_op("?.");
                self.next();
                let name = match self.next().kind {
                    Tok::Ident(w) => w,
                    other => {
                        return Err(
                            self.error(format!("expected a name after [.], found {other:?}"))
                        );
                    }
                };
                if self.is_op("(") {
                    let args = self.arguments()?;
                    e = Expr::Call { target: Box::new(e), name, args, safe, at };
                } else {
                    e = Expr::Field { target: Box::new(e), name, safe, at };
                }
                continue;
            }
            if self.is_op("::") {
                self.next();
                let name = self.ident()?;
                let class = match e {
                    Expr::Ident(c, _) => c,
                    _ => return Err(self.error("a method reference names a class".into())),
                };
                e = Expr::MethodRef { class, name };
                continue;
            }
            if self.is_op("[") {
                self.next();
                let index = self.expr()?;
                self.expect_op("]")?;
                e = Expr::Index { target: Box::new(e), index: Box::new(index), at };
                continue;
            }
            for (op, delta) in [("++", 1), ("--", -1)] {
                if self.is_op(op) {
                    self.next();
                    e = Expr::Step { target: Box::new(e), delta, prefix: false, at };
                }
            }
            break;
        }
        Ok(e)
    }

    fn arguments(&mut self) -> Result<Vec<Expr>, ParseError> {
        self.expect_op("(")?;
        let mut args = Vec::new();
        while !self.is_op(")") {
            args.push(self.expr()?);
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(args)
    }

    fn primary(&mut self) -> Result<Expr, ParseError> {
        let at = self.here();
        let t = self.next();
        match t.kind {
            Tok::Int(n) => Ok(Expr::Int(n)),
            Tok::Long(n) => Ok(Expr::Long(n)),
            Tok::Float(f) => Ok(Expr::Float(f)),
            Tok::Double(f) => Ok(Expr::Double(f)),
            Tok::Str(s) => Ok(Expr::Str(s)),
            Tok::Regex(p, f) => Ok(Expr::Regex(p, f)),
            Tok::Op("(") => {
                let e = self.expr()?;
                self.expect_op(")")?;
                Ok(e)
            }
            Tok::Op("[") => {
                // `[:]` is an empty map, `[a: b]` a map, `[a, b]` a list
                if self.eat_op(":") {
                    self.expect_op("]")?;
                    return Ok(Expr::Map(Vec::new()));
                }
                if self.eat_op("]") {
                    return Ok(Expr::List(Vec::new()));
                }
                let first = self.expr()?;
                if self.eat_op(":") {
                    let mut pairs = vec![(first, self.expr()?)];
                    while self.eat_op(",") {
                        if self.is_op("]") {
                            break;
                        }
                        let k = self.expr()?;
                        self.expect_op(":")?;
                        pairs.push((k, self.expr()?));
                    }
                    self.expect_op("]")?;
                    return Ok(Expr::Map(pairs));
                }
                let mut items = vec![first];
                while self.eat_op(",") {
                    if self.is_op("]") {
                        break;
                    }
                    items.push(self.expr()?);
                }
                self.expect_op("]")?;
                Ok(Expr::List(items))
            }
            Tok::Ident(word) => match word.as_str() {
                "true" => Ok(Expr::Bool(true)),
                "false" => Ok(Expr::Bool(false)),
                "null" => Ok(Expr::Null),
                "new" => {
                    let class = self.type_name()?;
                    // `new int[] {…}` and `new ArrayList()`
                    if self.is_op("[") {
                        self.next();
                        if self.eat_op("]") {
                            let items = if self.is_op("{") {
                                self.next();
                                let mut items = Vec::new();
                                while !self.is_op("}") {
                                    items.push(self.expr()?);
                                    if !self.eat_op(",") {
                                        break;
                                    }
                                }
                                self.expect_op("}")?;
                                items
                            } else {
                                Vec::new()
                            };
                            return Ok(Expr::List(items));
                        }
                        let size = self.expr()?;
                        self.expect_op("]")?;
                        return Ok(Expr::New { class: format!("{class}[]"), args: vec![size], at });
                    }
                    let args = self.arguments()?;
                    Ok(Expr::New { class, args, at })
                }
                _ => {
                    // a call of the script's own function or a context one
                    if self.is_op("(") {
                        let args = self.arguments()?;
                        return Ok(Expr::Invoke { name: word, args, at });
                    }
                    // `Math.max(…)`, `Integer.MAX_VALUE`: a class and its member
                    if is_class_name(&word)
                        && self.is_op(".")
                        && matches!(self.peek_at(1), Tok::Ident(_))
                    {
                        self.next();
                        let name = self.ident()?;
                        let args = if self.is_op("(") { Some(self.arguments()?) } else { None };
                        return Ok(Expr::Static { class: word, name, args, at });
                    }
                    Ok(Expr::Ident(word, at))
                }
            },
            other => Err(ParseError { message: format!("unexpected token {other:?}"), at }),
        }
    }
}

fn is_type_word(word: &str) -> bool {
    TYPE_WORDS.contains(&word)
}

/// A capitalised name that is not a variable is read as a class.
pub fn is_class_name(word: &str) -> bool {
    word.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false)
}
