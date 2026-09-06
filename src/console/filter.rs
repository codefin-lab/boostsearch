//! The `filter` a find may carry, in the query language the front end speaks.
//!
//! The front end's language is DQL, and a filter names a type's attribute and
//! what it must be: `dashboard.attributes.title:foo`. Of the whole language
//! this reads the part a find is ever given -- a field, a colon, a value, and
//! `and`/`or`/`not` between such pairs -- and refuses the rest in the words
//! the front end's own parser uses, because the front end shows those words.

use serde_json::{Value, json};

/// A filter as a search, or why it is not one.
pub fn parse(text: &str, types: &[String]) -> Result<Value, String> {
    let mut reader = Reader { text, at: 0 };
    let query = reader.or_group(types)?;
    reader.skip_space();
    if reader.at < text.len() {
        return Err(reader.syntax_error("AND, OR, end of input, whitespace"));
    }
    Ok(query)
}

struct Reader<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Reader<'a> {
    fn skip_space(&mut self) {
        while self.text[self.at..].starts_with(char::is_whitespace) {
            self.at += 1;
        }
    }

    fn take_word(&mut self, word: &str) -> bool {
        self.skip_space();
        let rest = &self.text[self.at..];
        let ends = rest
            .get(word.len()..)
            .map(|r| r.is_empty() || r.starts_with(char::is_whitespace))
            .unwrap_or(false);
        if rest.len() >= word.len() && rest[..word.len()].eq_ignore_ascii_case(word) && ends {
            self.at += word.len();
            true
        } else {
            false
        }
    }

    fn or_group(&mut self, types: &[String]) -> Result<Value, String> {
        let mut parts = vec![self.and_group(types)?];
        while self.take_word("or") {
            parts.push(self.and_group(types)?);
        }
        Ok(match parts.len() {
            1 => parts.remove(0),
            _ => json!({"bool": {"should": parts, "minimum_should_match": 1}}),
        })
    }

    fn and_group(&mut self, types: &[String]) -> Result<Value, String> {
        let mut parts = vec![self.term(types)?];
        while self.take_word("and") {
            parts.push(self.term(types)?);
        }
        Ok(match parts.len() {
            1 => parts.remove(0),
            _ => json!({"bool": {"filter": parts}}),
        })
    }

    /// `type.attributes.field:value`, or `not` one, or a group in parentheses.
    fn term(&mut self, types: &[String]) -> Result<Value, String> {
        if self.take_word("not") {
            let inner = self.term(types)?;
            return Ok(json!({"bool": {"must_not": [inner]}}));
        }
        self.skip_space();
        if self.text[self.at..].starts_with('(') {
            self.at += 1;
            let inner = self.or_group(types)?;
            self.skip_space();
            if !self.text[self.at..].starts_with(')') {
                return Err(self.syntax_error(")"));
            }
            self.at += 1;
            return Ok(inner);
        }
        let field = self.take_until(|c| c == ':' || c.is_whitespace());
        if field.is_empty() {
            return Err(self.syntax_error("a field name"));
        }
        self.skip_space();
        if !self.text[self.at..].starts_with(':') {
            return Err(self.syntax_error(":"));
        }
        self.at += 1;
        self.skip_space();
        let value = self.value()?;
        // `dashboard.attributes.title` is the title of a dashboard, and a
        // find that asked about visualizations has no business with it
        let mut pieces = field.splitn(3, '.');
        let (kind, middle, name) = (pieces.next().unwrap_or(""), pieces.next(), pieces.next());
        if !types.iter().any(|t| t == kind) {
            return Err(format!("This type {kind} is not allowed: Bad Request"));
        }
        let path = match (middle, name) {
            (Some("attributes"), Some(name)) => format!("{kind}.{name}"),
            (Some(other), None) => format!("{kind}.{other}"),
            _ => field.to_string(),
        };
        Ok(match value.as_str() {
            "*" => json!({"exists": {"field": path}}),
            _ => json!({"match": {path: value}}),
        })
    }

    fn value(&mut self) -> Result<String, String> {
        let rest = &self.text[self.at..];
        if let Some(inner) = rest.strip_prefix('"') {
            let Some(end) = inner.find('"') else { return Err(self.syntax_error("\"")) };
            self.at += end + 2;
            return Ok(inner[..end].to_string());
        }
        let value = self.take_until(|c| c.is_whitespace() || c == ')' || c == '<' || c == '>');
        if value.is_empty() {
            return Err(self.syntax_error("a value"));
        }
        Ok(value)
    }

    fn take_until(&mut self, stop: impl Fn(char) -> bool) -> String {
        let start = self.at;
        while let Some(c) = self.text[self.at..].chars().next() {
            if stop(c) {
                break;
            }
            self.at += c.len_utf8();
        }
        self.text[start..self.at].to_string()
    }

    /// The error the front end's own parser writes: what was expected, what
    /// was found, and the text with a caret under the place.
    fn syntax_error(&self, expected: &str) -> String {
        let found = self.text[self.at..]
            .chars()
            .next()
            .map(|c| format!("\"{c}\""))
            .unwrap_or_else(|| "end of input".into());
        format!(
            "DQLSyntaxError: Expected {expected} but {found} found.\n{}\n{}^: Bad Request",
            self.text,
            "-".repeat(self.at)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_filter_names_a_type_it_was_asked_about() {
        let types = vec!["dashboard".to_string()];
        let found = parse("dashboard.attributes.title:foo", &types).expect("parses");
        assert_eq!(found, json!({"match": {"dashboard.title": "foo"}}));
        let refused =
            parse("dashboard.attributes.title:foo", &["visualization".to_string()]).unwrap_err();
        assert_eq!(refused, "This type dashboard is not allowed: Bad Request");
    }

    #[test]
    fn a_broken_filter_is_refused_in_the_front_ends_own_words() {
        let types = vec!["dashboard".to_string()];
        let refused = parse("dashboard.attributes.title:foo<invalid", &types).unwrap_err();
        assert_eq!(
            refused,
            "DQLSyntaxError: Expected AND, OR, end of input, whitespace but \"<\" found.\n\
             dashboard.attributes.title:foo<invalid\n------------------------------^: Bad Request"
        );
    }

    #[test]
    fn and_or_and_not_join_terms() {
        let types = vec!["dashboard".to_string()];
        let found =
            parse("dashboard.attributes.title:a and not dashboard.attributes.title:b", &types)
                .expect("parses");
        assert!(found["bool"]["filter"].as_array().is_some_and(|f| f.len() == 2));
    }
}
