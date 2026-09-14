//! Where in a request body a field stands, as `line:column`.
//!
//! The reference reports a request it could not parse with the place its
//! parser had got to -- `[1:91] [terms] failed to parse field [exclude]` --
//! and this reported the same words with no place. A caller reading the
//! message to find the mistake in a long body had nothing to go on, and a
//! client comparing refusals saw two different ones. Nothing here keeps
//! positions while a body is parsed, so the body is walked again, once, only
//! for a refusal that names a field.

/// The position of `field`'s value inside an object that is the value of
/// `parent`, 1-based, the way the reference's parser reports it: for an array
/// or an object the closing bracket, where the parser stands once it has read
/// the whole value; for anything else the value's first character.
pub fn locate(raw: &str, parent: &str, field: &str) -> Option<(usize, usize)> {
    let mut w = Walker { b: raw.as_bytes(), i: 0, found: None, parent, field };
    w.ws();
    w.value(None);
    w.found.map(|at| line_col(raw, at))
}

fn line_col(raw: &str, at: usize) -> (usize, usize) {
    let before = &raw.as_bytes()[..at.min(raw.len())];
    let line = before.iter().filter(|c| **c == b'\n').count() + 1;
    let col = match before.iter().rposition(|c| *c == b'\n') {
        Some(nl) => at - nl,
        None => at + 1,
    };
    (line, col)
}

struct Walker<'a> {
    b: &'a [u8],
    i: usize,
    found: Option<usize>,
    parent: &'a str,
    field: &'a str,
}

impl Walker<'_> {
    fn ws(&mut self) {
        while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    /// Read one string, returning its contents; the cursor ends after it.
    fn string(&mut self) -> String {
        let mut out = Vec::new();
        self.i += 1;
        while self.i < self.b.len() {
            match self.b[self.i] {
                b'\\' => {
                    if let Some(c) = self.b.get(self.i + 1) {
                        out.push(*c);
                    }
                    self.i += 2;
                }
                b'"' => {
                    self.i += 1;
                    break;
                }
                c => {
                    out.push(c);
                    self.i += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Read one value. `owner` is the key this value belongs to. Returns the
    /// position the reference's parser would report for it.
    fn value(&mut self, owner: Option<&str>) -> usize {
        self.ws();
        let start = self.i;
        match self.b.get(self.i) {
            Some(b'{') => {
                self.i += 1;
                loop {
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b'}') => {
                            let end = self.i;
                            self.i += 1;
                            return end;
                        }
                        Some(b',') => self.i += 1,
                        Some(b'"') => {
                            let key = self.string();
                            self.ws();
                            if self.b.get(self.i) == Some(&b':') {
                                self.i += 1;
                            }
                            let at = self.value(Some(&key));
                            if self.found.is_none()
                                && key == self.field
                                && owner == Some(self.parent)
                            {
                                self.found = Some(at);
                            }
                        }
                        _ => return start,
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                loop {
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b']') => {
                            let end = self.i;
                            self.i += 1;
                            return end;
                        }
                        Some(b',') => self.i += 1,
                        Some(_) => {
                            self.value(None);
                        }
                        None => return start,
                    }
                }
            }
            Some(b'"') => {
                self.string();
                start
            }
            Some(_) => {
                while self.i < self.b.len()
                    && !matches!(self.b[self.i], b',' | b'}' | b']')
                    && !self.b[self.i].is_ascii_whitespace()
                {
                    self.i += 1;
                }
                start
            }
            None => start,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::locate;

    #[test]
    fn an_array_is_reported_at_its_closing_bracket() {
        let body = r#"{"size": 0, "aggs": {"t": {"terms": {"field": "sub", "include": "s[0-2]", "exclude": ["s1"]}}}}"#;
        assert_eq!(locate(body, "terms", "exclude"), Some((1, 91)));
    }

    #[test]
    fn a_string_is_reported_where_it_starts_and_lines_are_counted() {
        let body = "{\n  \"aggs\": {\"t\": {\"terms\": {\n    \"exclude\": \"s1\"}}}}";
        assert_eq!(locate(body, "terms", "exclude"), Some((3, 16)));
    }

    #[test]
    fn a_field_under_another_parent_is_not_taken() {
        let body = r#"{"query": {"exclude": 1}, "aggs": {"t": {"terms": {"exclude": [1]}}}}"#;
        let (_, col) = locate(body, "terms", "exclude").expect("found");
        assert_eq!(&body[col - 1..col], "]");
    }
}
