//! What a query says, once it has been read.

/// A whole statement.
#[derive(Clone, Debug)]
pub struct Select {
    pub columns: Vec<Column>,
    pub from: String,
    pub filter: Option<Condition>,
    pub group_by: Vec<Expr>,
    pub having: Option<Condition>,
    pub order_by: Vec<(Expr, bool)>,
    pub limit: Option<usize>,
    pub offset: usize,
    pub distinct: bool,
}

/// One thing asked for, and what to call it.
#[derive(Clone, Debug)]
pub struct Column {
    pub expr: Expr,
    pub alias: Option<String>,
}

impl Column {
    /// What this column is called in the answer.
    pub fn name(&self) -> String {
        self.alias.clone().unwrap_or_else(|| self.expr.name())
    }
}

#[derive(Clone, Debug)]
pub enum Expr {
    /// every column there is
    Star,
    Field(String),
    Number(f64),
    Text(String),
    Boolean(bool),
    Null,
    /// `count(*)`, `avg(price)`, `date_format(ts, 'yyyy')`
    Call {
        name: String,
        args: Vec<Expr>,
    },
    /// `a + b`, `a || b`
    Binary {
        op: String,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    /// `-a`
    Negate(Box<Expr>),
    /// `CASE WHEN … THEN … ELSE … END`
    Case {
        whens: Vec<(Condition, Expr)>,
        otherwise: Option<Box<Expr>>,
    },
}

impl Expr {
    /// What this is called when nothing else names it -- which for an
    /// aggregate is the whole call, as SQL reports it.
    pub fn name(&self) -> String {
        match self {
            Expr::Star => "*".to_string(),
            Expr::Field(f) => f.clone(),
            Expr::Number(n) => n.to_string(),
            Expr::Text(t) => t.clone(),
            Expr::Boolean(b) => b.to_string(),
            Expr::Null => "NULL".to_string(),
            Expr::Call { name, args } => {
                let inside: Vec<String> = args.iter().map(|a| a.name()).collect();
                format!("{}({})", name.to_lowercase(), inside.join(", "))
            }
            Expr::Binary { op, left, right } => format!("{} {op} {}", left.name(), right.name()),
            Expr::Negate(inner) => format!("-{}", inner.name()),
            Expr::Case { .. } => "case".to_string(),
        }
    }

    /// The field this stands for, where it stands for exactly one.
    pub fn field(&self) -> Option<&str> {
        match self {
            Expr::Field(f) => Some(f),
            _ => None,
        }
    }

    /// Whether this is an aggregate, or holds one.
    pub fn is_aggregate(&self) -> bool {
        match self {
            Expr::Call { name, args } => {
                is_aggregate_name(name) || args.iter().any(|a| a.is_aggregate())
            }
            Expr::Binary { left, right, .. } => left.is_aggregate() || right.is_aggregate(),
            Expr::Negate(inner) => inner.is_aggregate(),
            _ => false,
        }
    }
}

/// Whether a function name is an aggregate, which is what decides whether a
/// query asks for documents or for groups. There is one list of these and
/// this is it: a second copy drifts, and a name missing from one of them is a
/// query silently answered as though it had asked something else.
pub fn is_aggregate_name(name: &str) -> bool {
    matches!(
        name.to_lowercase().as_str(),
        "count"
            | "count_distinct"
            | "sum"
            | "avg"
            | "min"
            | "max"
            | "var_pop"
            | "var_samp"
            | "stddev_pop"
            | "stddev_samp"
            | "percentile"
            | "percentile_approx"
    )
}

/// A thing that is true or false of a document.
#[derive(Clone, Debug)]
pub enum Condition {
    And(Box<Condition>, Box<Condition>),
    Or(Box<Condition>, Box<Condition>),
    Not(Box<Condition>),
    /// `a = 1`, `a > 1`, `a <> 1`
    Compare {
        left: Expr,
        op: String,
        right: Expr,
    },
    /// `a BETWEEN 1 AND 2`
    Between {
        value: Expr,
        low: Expr,
        high: Expr,
        negated: bool,
    },
    /// `a IN (1, 2, 3)`
    In {
        value: Expr,
        options: Vec<Expr>,
        negated: bool,
    },
    /// `a LIKE 'ann%'`
    Like {
        value: Expr,
        pattern: String,
        negated: bool,
    },
    /// `a IS NULL`
    IsNull {
        value: Expr,
        negated: bool,
    },
    /// `MATCH(field, 'words')` and its relatives, which have no SQL meaning
    /// and every meaning here
    Search {
        name: String,
        args: Vec<Expr>,
    },
    Always(bool),
}
