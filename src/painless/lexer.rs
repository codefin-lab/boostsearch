//! Cutting a script into tokens.
//!
//! Painless reads like Java: the same numbers, strings, operators and
//! punctuation, plus a regex literal between slashes and `?.`, `?:` and `=~`.
//! Every token remembers where it started, which is what an error names.

/// One token, and where it began in the source.
#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub kind: Tok,
    pub at: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Int(i64),
    Long(i64),
    Float(f64),
    Double(f64),
    Str(String),
    Regex(String, String),
    Ident(String),
    /// an operator or a punctuation mark, as written
    Op(&'static str),
    End,
}

/// The operators and marks, longest first so that `>>>=` is read before `>`.
const OPS: &[&str] = &[
    ">>>=", "<<=", ">>=", ">>>", "===", "!==", "?.", "?:", "->", "::", "++", "--", "&&", "||",
    "==", "!=", "<=", ">=", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "<<", ">>", "=~",
    "==~", "{", "}", "(", ")", "[", "]", ";", ",", ".", "?", ":", "+", "-", "*", "/", "%", "<",
    ">", "=", "!", "~", "&", "|", "^",
];

/// What went wrong reading the script, and where.
#[derive(Debug, Clone)]
pub struct LexError {
    pub message: String,
    pub at: usize,
}

pub fn lex(src: &str) -> Result<Vec<Token>, LexError> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // comments, both kinds
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            let mut j = i + 2;
            while j + 1 < bytes.len() && !(bytes[j] == b'*' && bytes[j + 1] == b'/') {
                j += 1;
            }
            i = j + 2;
            continue;
        }
        let start = i;
        // a regex literal stands where a value may stand: after an operator
        // or an opening mark, not after a value
        let value_before = matches!(
            out.last().map(|t: &Token| &t.kind),
            Some(
                Tok::Int(_)
                    | Tok::Long(_)
                    | Tok::Float(_)
                    | Tok::Double(_)
                    | Tok::Str(_)
                    | Tok::Ident(_)
                    | Tok::Regex(..)
            ) | Some(Tok::Op(")"))
                | Some(Tok::Op("]"))
        );
        if c == b'/' && !value_before {
            let mut j = i + 1;
            let mut pattern = String::new();
            while j < bytes.len() && bytes[j] != b'/' {
                if bytes[j] == b'\\' && j + 1 < bytes.len() {
                    pattern.push(bytes[j] as char);
                    j += 1;
                }
                pattern.push(bytes[j] as char);
                j += 1;
            }
            j += 1;
            let mut flags = String::new();
            while j < bytes.len() && bytes[j].is_ascii_alphabetic() {
                flags.push(bytes[j] as char);
                j += 1;
            }
            out.push(Token { kind: Tok::Regex(pattern, flags), at: start });
            i = j;
            continue;
        }
        if c.is_ascii_digit() || (c == b'.' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit())
        {
            let mut j = i;
            let mut float = false;
            // hex and octal keep Java's spellings
            if c == b'0' && j + 1 < bytes.len() && (bytes[j + 1] == b'x' || bytes[j + 1] == b'X') {
                j += 2;
                while j < bytes.len() && bytes[j].is_ascii_hexdigit() {
                    j += 1;
                }
                let n = i64::from_str_radix(&src[i + 2..j], 16)
                    .map_err(|_| LexError { message: "bad number".into(), at: start })?;
                let long = j < bytes.len() && (bytes[j] == b'l' || bytes[j] == b'L');
                if long {
                    j += 1;
                }
                out.push(Token { kind: if long { Tok::Long(n) } else { Tok::Int(n) }, at: start });
                i = j;
                continue;
            }
            while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b'_') {
                j += 1;
            }
            if j < bytes.len()
                && bytes[j] == b'.'
                && j + 1 < bytes.len()
                && bytes[j + 1].is_ascii_digit()
            {
                float = true;
                j += 1;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
            }
            if j < bytes.len() && (bytes[j] == b'e' || bytes[j] == b'E') {
                let mut k = j + 1;
                if k < bytes.len() && (bytes[k] == b'+' || bytes[k] == b'-') {
                    k += 1;
                }
                if k < bytes.len() && bytes[k].is_ascii_digit() {
                    float = true;
                    j = k;
                    while j < bytes.len() && bytes[j].is_ascii_digit() {
                        j += 1;
                    }
                }
            }
            let text: String = src[i..j].chars().filter(|c| *c != '_').collect();
            let suffix = if j < bytes.len() { bytes[j].to_ascii_lowercase() } else { 0 };
            let kind = match suffix {
                b'l' => {
                    j += 1;
                    Tok::Long(
                        text.parse()
                            .map_err(|_| LexError { message: "bad number".into(), at: start })?,
                    )
                }
                b'f' => {
                    j += 1;
                    Tok::Float(
                        text.parse()
                            .map_err(|_| LexError { message: "bad number".into(), at: start })?,
                    )
                }
                b'd' => {
                    j += 1;
                    Tok::Double(
                        text.parse()
                            .map_err(|_| LexError { message: "bad number".into(), at: start })?,
                    )
                }
                _ if float => Tok::Double(
                    text.parse()
                        .map_err(|_| LexError { message: "bad number".into(), at: start })?,
                ),
                _ => match text.parse::<i64>() {
                    Ok(n) if n <= i32::MAX as i64 => Tok::Int(n),
                    Ok(n) => Tok::Long(n),
                    Err(_) => return Err(LexError { message: "bad number".into(), at: start }),
                },
            };
            out.push(Token { kind, at: start });
            i = j;
            continue;
        }
        if c == b'"' || c == b'\'' {
            let quote = c;
            let mut j = i + 1;
            let mut s = String::new();
            while j < bytes.len() && bytes[j] != quote {
                if bytes[j] == b'\\' && j + 1 < bytes.len() {
                    j += 1;
                    s.push(match bytes[j] {
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        b'\\' => '\\',
                        b'\'' => '\'',
                        b'"' => '"',
                        other => other as char,
                    });
                    j += 1;
                    continue;
                }
                let ch = src[j..].chars().next().unwrap();
                s.push(ch);
                j += ch.len_utf8();
            }
            if j >= bytes.len() {
                return Err(LexError { message: "unclosed string".into(), at: start });
            }
            out.push(Token { kind: Tok::Str(s), at: start });
            i = j + 1;
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' || c == b'$' {
            let mut j = i;
            while j < bytes.len()
                && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'$')
            {
                j += 1;
            }
            out.push(Token { kind: Tok::Ident(src[i..j].to_string()), at: start });
            i = j;
            continue;
        }
        let mut matched = None;
        for op in OPS {
            if src[i..].starts_with(op) {
                matched = Some(*op);
                break;
            }
        }
        match matched {
            Some(op) => {
                out.push(Token { kind: Tok::Op(op), at: start });
                i += op.len();
            }
            None => {
                return Err(LexError {
                    message: format!("unexpected character [{}]", c as char),
                    at: start,
                });
            }
        }
    }
    out.push(Token { kind: Tok::End, at: src.len() });
    Ok(out)
}
