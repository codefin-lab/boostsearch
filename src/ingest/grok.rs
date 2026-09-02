//! Grok: named patterns built out of a bank of named patterns.
//!
//! `%{NUMBER:status:int}` names a pattern from the bank, the field its
//! match goes to, and the type it is read as. The bank is the one
//! OpenSearch ships, expanded here into one regex per pattern.

use std::collections::HashMap;

use serde_json::Value;

use super::IngestError;

/// The patterns OpenSearch ships, by name.
pub fn bank() -> &'static HashMap<String, String> {
    static BANK: std::sync::OnceLock<HashMap<String, String>> = std::sync::OnceLock::new();
    BANK.get_or_init(|| {
        let raw: Value =
            serde_json::from_str(include_str!("grok_patterns.json")).unwrap_or(Value::Null);
        raw.get("patterns")
            .and_then(|p| p.as_object())
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// A compiled grok pattern: the regex, and which capture is which field.
pub struct Grok {
    regex: fancy_regex::Regex,
    /// (group name in the regex, field name, type)
    captures: Vec<(String, String, String)>,
}

impl Grok {
    pub fn compile(
        pattern: &str,
        definitions: &HashMap<String, String>,
    ) -> Result<Grok, IngestError> {
        let mut captures = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        let expanded = expand(pattern, definitions, &mut captures, &mut seen, 0)?;
        let regex = fancy_regex::Regex::new(&expanded).map_err(|e| {
            IngestError::illegal(format!("Invalid regex pattern found in: [{pattern}]. {e}"))
        })?;
        Ok(Grok { regex, captures })
    }

    /// The fields a text yields, or nothing where it does not match.
    pub fn captures(&self, text: &str) -> Result<Option<Vec<(String, Value)>>, IngestError> {
        let found = self.regex.captures(text).map_err(|e| {
            IngestError::illegal(format!("grok pattern matching was interrupted: {e}"))
        })?;
        let Some(caps) = found else { return Ok(None) };
        let mut out = Vec::new();
        for (group, field, kind) in &self.captures {
            let Some(m) = caps.name(group) else { continue };
            let text = m.as_str();
            let value = match kind.as_str() {
                "int" | "long" => match text.parse::<i64>() {
                    Ok(n) => Value::from(n),
                    Err(_) => match text.parse::<f64>() {
                        Ok(f) => Value::from(f as i64),
                        Err(_) => {
                            return Err(IngestError::illegal(format!(
                                "For input string: \"{text}\""
                            )));
                        }
                    },
                },
                "float" | "double" => match text.parse::<f64>() {
                    Ok(f) => {
                        serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)
                    }
                    Err(_) => {
                        return Err(IngestError::illegal(format!("For input string: \"{text}\"")));
                    }
                },
                "boolean" => Value::Bool(text.eq_ignore_ascii_case("true")),
                _ => Value::String(text.to_string()),
            };
            out.push((field.clone(), value));
        }
        Ok(Some(out))
    }
}

/// Turn `%{NAME:field:type}` references into the patterns they name,
/// with a capture group wherever a field is named.
fn expand(
    pattern: &str,
    definitions: &HashMap<String, String>,
    captures: &mut Vec<(String, String, String)>,
    seen: &mut Vec<String>,
    depth: usize,
) -> Result<String, IngestError> {
    if depth > 200 {
        return Err(IngestError::illegal("circular reference in pattern".to_string()));
    }
    let mut out = String::new();
    let mut rest = pattern;
    while let Some(at) = rest.find("%{") {
        out.push_str(&convert_syntax(&rest[..at]));
        let after = &rest[at + 2..];
        let Some(end) = after.find('}') else {
            return Err(IngestError::illegal(format!("Invalid grok pattern: [{pattern}]")));
        };
        let reference = &after[..end];
        rest = &after[end + 1..];
        let mut parts = reference.splitn(3, ':');
        let name = parts.next().unwrap_or("");
        let field = parts.next().filter(|f| !f.is_empty());
        let kind = parts.next().unwrap_or("").to_string();
        let Some(def) = definitions.get(name).or_else(|| bank().get(name)) else {
            return Err(IngestError::illegal(format!(
                "Unable to find pattern [{name}] in Grok's pattern dictionary"
            )));
        };
        if seen.iter().filter(|s| s.as_str() == name).count() > 20 {
            return Err(IngestError::illegal(format!("circular reference in pattern [{name}]")));
        }
        seen.push(name.to_string());
        let inner = expand(def, definitions, captures, seen, depth + 1)?;
        seen.pop();
        match field {
            Some(field) => {
                let group = format!("g{}", captures.len());
                captures.push((group.clone(), field.to_string(), kind));
                out.push_str(&format!("(?P<{group}>{inner})"));
            }
            None => {
                out.push_str(&format!("(?:{inner})"));
            }
        }
    }
    out.push_str(&convert_syntax(rest));
    Ok(out)
}

/// Java's regex spellings this engine reads differently: `(?<name>` is a
/// named group with a `P`, and a bare named group in the bank is kept as a
/// field of its own.
fn convert_syntax(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // `(?<name>` is a named group; `(?<=` and `(?<!` are lookbehinds
        if c == '('
            && chars.get(i + 1) == Some(&'?')
            && chars.get(i + 2) == Some(&'<')
            && chars.get(i + 3).map(|n| n.is_alphabetic() || *n == '_').unwrap_or(false)
        {
            out.push_str("(?P<");
            i += 3;
            continue;
        }
        // an atomic group is read as a plain group
        if c == '(' && chars.get(i + 1) == Some(&'?') && chars.get(i + 2) == Some(&'>') {
            out.push_str("(?:");
            i += 3;
            continue;
        }
        // a possessive quantifier is read as the greedy one
        if c == '+'
            && i > 0
            && matches!(chars[i - 1], '+' | '*' | '?' | '}')
            && !(i > 1 && chars[i - 2] == '\\')
        {
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// The whole bank, expanded: what `_ingest/processor/grok` answers.
pub fn bank_json() -> Value {
    let raw: Value =
        serde_json::from_str(include_str!("grok_patterns.json")).unwrap_or(Value::Null);
    raw
}
