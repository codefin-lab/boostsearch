//! Turning a query into the pieces a parser can read.

/// One piece of a query.
#[derive(Clone, Debug, PartialEq)]
pub enum Token {
    /// a bare name: a table, a column, a function
    Name(String),
    /// a name in backticks or double quotes, which may hold anything
    Quoted(String),
    Number(f64),
    Text(String),
    /// `=`, `<>`, `+`, `(`, `,` and the rest
    Symbol(String),
    End,
}

impl Token {
    /// Whether this is a particular word, whatever case it was written in.
    pub fn is(&self, word: &str) -> bool {
        match self {
            Token::Name(n) => n.eq_ignore_ascii_case(word),
            Token::Symbol(s) => s == word,
            _ => false,
        }
    }

    pub fn text(&self) -> String {
        match self {
            Token::Name(n) | Token::Quoted(n) | Token::Text(n) | Token::Symbol(n) => n.clone(),
            Token::Number(n) => n.to_string(),
            Token::End => String::new(),
        }
    }
}

/// Read a query into its pieces.
///
/// The only surprise here is that a name may hold dots, stars and hyphens:
/// an index is `logs-2026.01`, a field is `user.name`, and both arrive as one
/// name rather than as arithmetic.
pub fn read(source: &str) -> Result<Vec<Token>, String> {
    let mut out = Vec::new();
    let chars: Vec<char> = source.chars().collect();
    let mut at = 0usize;
    while at < chars.len() {
        let c = chars[at];
        if c.is_whitespace() {
            at += 1;
            continue;
        }
        // a comment runs to the end of the line
        if c == '-' && chars.get(at + 1) == Some(&'-') {
            while at < chars.len() && chars[at] != '\n' {
                at += 1;
            }
            continue;
        }
        if c.is_ascii_digit() || (c == '.' && chars.get(at + 1).is_some_and(|d| d.is_ascii_digit()))
        {
            let start = at;
            while at < chars.len()
                && (chars[at].is_ascii_digit()
                    || chars[at] == '.'
                    || chars[at] == 'e'
                    || chars[at] == 'E'
                    || ((chars[at] == '-' || chars[at] == '+')
                        && matches!(chars[at - 1], 'e' | 'E')))
            {
                at += 1;
            }
            let text: String = chars[start..at].iter().collect();
            let number = text.parse::<f64>().map_err(|_| format!("bad number [{text}]"))?;
            out.push(Token::Number(number));
            continue;
        }
        if c.is_alphabetic() || c == '_' || c == '@' {
            let start = at;
            // a name may hold what an index name holds
            while at < chars.len()
                && (chars[at].is_alphanumeric()
                    || matches!(chars[at], '_' | '.' | '@' | '*' | '-' | ':'))
            {
                at += 1;
            }
            out.push(Token::Name(chars[start..at].iter().collect()));
            continue;
        }
        if c == '`' || c == '"' {
            let close = c;
            at += 1;
            let start = at;
            while at < chars.len() && chars[at] != close {
                at += 1;
            }
            let text: String = chars[start..at].iter().collect();
            at += 1;
            out.push(Token::Quoted(text));
            continue;
        }
        if c == '\'' {
            at += 1;
            let mut text = String::new();
            while at < chars.len() {
                if chars[at] == '\'' {
                    // two quotes in a row are one quote in the string
                    if chars.get(at + 1) == Some(&'\'') {
                        text.push('\'');
                        at += 2;
                        continue;
                    }
                    break;
                }
                text.push(chars[at]);
                at += 1;
            }
            at += 1;
            out.push(Token::Text(text));
            continue;
        }
        // the two-character operators, before the one-character ones they
        // begin with
        let two: String = chars[at..(at + 2).min(chars.len())].iter().collect();
        if matches!(two.as_str(), "<>" | "!=" | ">=" | "<=" | "||" | "&&") {
            out.push(Token::Symbol(two));
            at += 2;
            continue;
        }
        out.push(Token::Symbol(c.to_string()));
        at += 1;
    }
    out.push(Token::End);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_index_name_is_one_name() {
        let read = read("SELECT * FROM logs-2026.01.02 WHERE user.name = 'ann'").unwrap();
        assert!(read.contains(&Token::Name("logs-2026.01.02".into())));
        assert!(read.contains(&Token::Name("user.name".into())));
        assert!(read.contains(&Token::Text("ann".into())));
    }

    #[test]
    fn a_quote_inside_a_string_is_a_quote() {
        let read = read("WHERE name = 'O''Hara'").unwrap();
        assert!(read.contains(&Token::Text("O'Hara".into())));
    }

    #[test]
    fn the_two_character_operators_stay_together() {
        let read = read("a >= 1 AND b <> 2").unwrap();
        assert!(read.contains(&Token::Symbol(">=".into())));
        assert!(read.contains(&Token::Symbol("<>".into())));
    }

    #[test]
    fn a_comment_is_not_part_of_the_query() {
        let read = read("SELECT a -- everything after this is a note\nFROM b").unwrap();
        assert!(read.iter().all(|t| !t.text().contains("note")));
        assert!(read.contains(&Token::Name("b".into())));
    }
}
