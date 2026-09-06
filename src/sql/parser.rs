//! Reading a SELECT.
//!
//! A recursive descent parser: each level of precedence is a function that
//! calls the one below it, which is what makes `a OR b AND c` mean
//! `a OR (b AND c)` without anything having to say so.

use super::ast::*;
use super::lexer::{Token, read};

pub struct Parser {
    tokens: Vec<Token>,
    at: usize,
}

type Answer<T> = Result<T, String>;

/// Read a whole statement.
pub fn parse(source: &str) -> Answer<Select> {
    let mut parser = Parser { tokens: read(source)?, at: 0 };
    let select = parser.select()?;
    // a query that ends before its text does is a query with a mistake in it
    if !matches!(parser.peek(), Token::End) && !parser.peek().is(";") {
        return Err(format!("unexpected [{}]", parser.peek().text()));
    }
    Ok(select)
}

impl Parser {
    fn peek(&self) -> Token {
        self.tokens.get(self.at).cloned().unwrap_or(Token::End)
    }

    fn peek_at(&self, ahead: usize) -> Token {
        self.tokens.get(self.at + ahead).cloned().unwrap_or(Token::End)
    }

    fn next(&mut self) -> Token {
        let t = self.peek();
        self.at += 1;
        t
    }

    /// Take a word if it is there, and say whether it was.
    fn took(&mut self, word: &str) -> bool {
        if self.peek().is(word) {
            self.at += 1;
            return true;
        }
        false
    }

    fn expect(&mut self, word: &str) -> Answer<()> {
        if self.took(word) {
            return Ok(());
        }
        Err(format!("expected [{word}] but found [{}]", self.peek().text()))
    }

    fn select(&mut self) -> Answer<Select> {
        self.expect("SELECT")?;
        let distinct = self.took("DISTINCT");
        let mut columns = Vec::new();
        loop {
            columns.push(self.column()?);
            if !self.took(",") {
                break;
            }
        }
        self.expect("FROM")?;
        let from = match self.next() {
            Token::Name(n) => n,
            Token::Quoted(n) => n,
            other => return Err(format!("expected an index but found [{}]", other.text())),
        };
        // `FROM index alias` and `FROM index AS alias`: the alias is noted and
        // ignored, since there is one table to be confused about
        if self.took("AS") {
            self.next();
        } else if matches!(self.peek(), Token::Name(_))
            && !self.starts_a_clause()
        {
            self.next();
        }
        let filter = self.took("WHERE").then(|| self.condition()).transpose()?;
        let mut group_by = Vec::new();
        if self.took("GROUP") {
            self.expect("BY")?;
            loop {
                group_by.push(self.expr()?);
                if !self.took(",") {
                    break;
                }
            }
        }
        let having = self.took("HAVING").then(|| self.condition()).transpose()?;
        let mut order_by = Vec::new();
        if self.took("ORDER") {
            self.expect("BY")?;
            loop {
                let expr = self.expr()?;
                let ascending = if self.took("DESC") {
                    false
                } else {
                    self.took("ASC");
                    true
                };
                order_by.push((expr, ascending));
                if !self.took(",") {
                    break;
                }
            }
        }
        let mut limit = None;
        let mut offset = 0usize;
        if self.took("LIMIT") {
            let first = self.number()? as usize;
            // `LIMIT a, b` counts from a and takes b, which is the other way
            // round from `LIMIT b OFFSET a`
            if self.took(",") {
                offset = first;
                limit = Some(self.number()? as usize);
            } else {
                limit = Some(first);
            }
        }
        if self.took("OFFSET") {
            offset = self.number()? as usize;
        }
        Ok(Select { columns, from, filter, group_by, having, order_by, limit, offset, distinct })
    }

    /// Whether what follows begins a clause rather than naming an alias.
    fn starts_a_clause(&self) -> bool {
        let t = self.peek();
        ["WHERE", "GROUP", "HAVING", "ORDER", "LIMIT", "OFFSET"].iter().any(|w| t.is(w))
    }

    fn number(&mut self) -> Answer<f64> {
        match self.next() {
            Token::Number(n) => Ok(n),
            other => Err(format!("expected a number but found [{}]", other.text())),
        }
    }

    fn column(&mut self) -> Answer<Column> {
        let expr = self.expr()?;
        let alias = if self.took("AS") {
            Some(self.next().text())
        } else if matches!(self.peek(), Token::Name(_) | Token::Quoted(_))
            && !self.peek().is("FROM")
        {
            Some(self.next().text())
        } else {
            None
        };
        Ok(Column { expr, alias })
    }

    // conditions, loosest binding first

    fn condition(&mut self) -> Answer<Condition> {
        let mut left = self.condition_and()?;
        while self.took("OR") || self.took("||") {
            let right = self.condition_and()?;
            left = Condition::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn condition_and(&mut self) -> Answer<Condition> {
        let mut left = self.condition_not()?;
        while self.took("AND") || self.took("&&") {
            let right = self.condition_not()?;
            left = Condition::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn condition_not(&mut self) -> Answer<Condition> {
        if self.took("NOT") {
            return Ok(Condition::Not(Box::new(self.condition_not()?)));
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Answer<Condition> {
        if self.peek().is("(") {
            // a bracket may hold a condition or a value; which it is only
            // becomes clear from what follows
            let save = self.at;
            self.at += 1;
            if let Ok(inner) = self.condition()
                && self.took(")")
                && !self.peek().is("=")
                && !self.peek().is("<")
                && !self.peek().is(">")
            {
                return Ok(inner);
            }
            self.at = save;
        }
        // the search functions, which are conditions rather than values
        if let Token::Name(name) = self.peek()
            && matches!(
                name.to_lowercase().as_str(),
                "match" | "match_phrase" | "matchquery" | "match_query" | "multi_match"
                    | "query_string" | "simple_query_string" | "wildcard_query" | "regexp_query"
            )
            && self.peek_at(1).is("(")
        {
            self.at += 2;
            let mut args = Vec::new();
            if !self.peek().is(")") {
                loop {
                    args.push(self.expr()?);
                    if !self.took(",") {
                        break;
                    }
                }
            }
            self.expect(")")?;
            return Ok(Condition::Search { name: name.to_lowercase(), args });
        }
        let left = self.expr()?;
        if self.took("IS") {
            let negated = self.took("NOT");
            self.expect("NULL")?;
            return Ok(Condition::IsNull { value: left, negated });
        }
        let negated = self.took("NOT");
        if self.took("BETWEEN") {
            let low = self.expr()?;
            self.expect("AND")?;
            let high = self.expr()?;
            return Ok(Condition::Between { value: left, low, high, negated });
        }
        if self.took("IN") {
            self.expect("(")?;
            let mut options = Vec::new();
            loop {
                options.push(self.expr()?);
                if !self.took(",") {
                    break;
                }
            }
            self.expect(")")?;
            return Ok(Condition::In { value: left, options, negated });
        }
        if self.took("LIKE") {
            let pattern = match self.next() {
                Token::Text(t) => t,
                other => other.text(),
            };
            return Ok(Condition::Like { value: left, pattern, negated });
        }
        if negated {
            return Err("expected BETWEEN, IN or LIKE after NOT".to_string());
        }
        let op = match self.peek() {
            Token::Symbol(s) if matches!(s.as_str(), "=" | "<" | ">" | "<>" | "!=" | ">=" | "<=") => {
                self.at += 1;
                s
            }
            other => return Err(format!("expected a comparison but found [{}]", other.text())),
        };
        let right = self.expr()?;
        Ok(Condition::Compare { left, op, right })
    }

    // values, loosest binding first

    fn expr(&mut self) -> Answer<Expr> {
        let mut left = self.term()?;
        loop {
            let op = match self.peek() {
                Token::Symbol(s) if matches!(s.as_str(), "+" | "-" | "||") => s,
                _ => break,
            };
            self.at += 1;
            let right = self.term()?;
            left = Expr::Binary { op, left: Box::new(left), right: Box::new(right) };
        }
        Ok(left)
    }

    fn term(&mut self) -> Answer<Expr> {
        let mut left = self.factor()?;
        loop {
            let op = match self.peek() {
                Token::Symbol(s) if matches!(s.as_str(), "*" | "/" | "%") => s,
                _ => break,
            };
            self.at += 1;
            let right = self.factor()?;
            left = Expr::Binary { op, left: Box::new(left), right: Box::new(right) };
        }
        Ok(left)
    }

    fn factor(&mut self) -> Answer<Expr> {
        if self.took("-") {
            return Ok(Expr::Negate(Box::new(self.factor()?)));
        }
        if self.took("+") {
            return self.factor();
        }
        if self.took("(") {
            let inner = self.expr()?;
            self.expect(")")?;
            return Ok(inner);
        }
        if self.peek().is("CASE") {
            return self.case();
        }
        match self.next() {
            Token::Number(n) => Ok(Expr::Number(n)),
            Token::Text(t) => Ok(Expr::Text(t)),
            Token::Quoted(q) => Ok(Expr::Field(q)),
            Token::Symbol(s) if s == "*" => Ok(Expr::Star),
            Token::Name(name) => {
                let lowered = name.to_lowercase();
                if lowered == "true" || lowered == "false" {
                    return Ok(Expr::Boolean(lowered == "true"));
                }
                if lowered == "null" {
                    return Ok(Expr::Null);
                }
                if self.took("(") {
                    let mut args = Vec::new();
                    // `count(distinct x)` counts what is different about x
                    let distinct = self.took("DISTINCT");
                    if !self.peek().is(")") {
                        loop {
                            args.push(self.expr()?);
                            if !self.took(",") {
                                break;
                            }
                        }
                    }
                    self.expect(")")?;
                    let name = if distinct && lowered == "count" {
                        "count_distinct".to_string()
                    } else {
                        lowered
                    };
                    return Ok(Expr::Call { name, args });
                }
                Ok(Expr::Field(name))
            }
            other => Err(format!("expected a value but found [{}]", other.text())),
        }
    }

    fn case(&mut self) -> Answer<Expr> {
        self.expect("CASE")?;
        let mut whens = Vec::new();
        while self.took("WHEN") {
            let when = self.condition()?;
            self.expect("THEN")?;
            whens.push((when, self.expr()?));
        }
        let otherwise =
            self.took("ELSE").then(|| self.expr()).transpose()?.map(Box::new);
        self.expect("END")?;
        Ok(Expr::Case { whens, otherwise })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_select() {
        let q = parse("SELECT a, b FROM logs WHERE a > 1 ORDER BY b DESC LIMIT 5").unwrap();
        assert_eq!(q.from, "logs");
        assert_eq!(q.columns.len(), 2);
        assert_eq!(q.limit, Some(5));
        assert_eq!(q.order_by.len(), 1);
        assert!(!q.order_by[0].1, "DESC means not ascending");
    }

    #[test]
    fn and_binds_tighter_than_or() {
        let q = parse("SELECT * FROM t WHERE a = 1 OR b = 2 AND c = 3").unwrap();
        // the top of the tree should be the OR
        match q.filter.unwrap() {
            Condition::Or(_, right) => match *right {
                Condition::And(_, _) => {}
                other => panic!("the right of the OR should be an AND, was {other:?}"),
            },
            other => panic!("the top should be an OR, was {other:?}"),
        }
    }

    #[test]
    fn an_aggregate_is_recognised_as_one() {
        let q = parse("SELECT region, count(*) FROM t GROUP BY region").unwrap();
        assert!(!q.columns[0].expr.is_aggregate());
        assert!(q.columns[1].expr.is_aggregate());
        assert_eq!(q.group_by.len(), 1);
    }

    #[test]
    fn a_column_may_be_named() {
        let q = parse("SELECT count(*) AS total, avg(price) mean FROM t").unwrap();
        assert_eq!(q.columns[0].name(), "total");
        assert_eq!(q.columns[1].name(), "mean");
    }

    #[test]
    fn limit_may_be_written_either_way() {
        assert_eq!(parse("SELECT * FROM t LIMIT 10 OFFSET 5").unwrap().offset, 5);
        let comma = parse("SELECT * FROM t LIMIT 5, 10").unwrap();
        assert_eq!((comma.offset, comma.limit), (5, Some(10)));
    }

    #[test]
    fn a_mistake_is_reported_rather_than_ignored() {
        assert!(parse("SELECT FROM").is_err());
        assert!(parse("SELECT a FROM t WHERE").is_err());
        assert!(parse("SELECT a FROM t nonsense nonsense").is_err());
    }
}
