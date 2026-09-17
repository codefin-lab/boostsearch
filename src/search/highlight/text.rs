//! The text of a field as a highlighter reads it: the values the document
//! holds, and the tokens the field's analyzer cuts them into.

use serde_json::Value;

/// One token of a value, where it stands and where it came from.
///
/// The offsets are UTF-16 units into the text the highlighter works on,
/// because every length the reference reports and every size a request gives
/// is counted in them.
#[derive(Clone, Debug)]
pub(crate) struct Tok {
    pub(crate) term: String,
    pub(crate) pos: usize,
    pub(crate) len: usize,
    pub(crate) from: usize,
    pub(crate) to: usize,
}

/// What the highlighters need to know about the index.
pub(crate) struct Env<'a> {
    pub(crate) mapping: &'a crate::store::Mapping,
    pub(crate) index: &'a velocore::Index,
    pub(crate) analysis: &'a crate::analysis::Registry,
}

/// Types whose value is one token, however long.
fn is_keyword_type(t: &str) -> bool {
    matches!(t, "keyword" | "constant_keyword" | "wildcard" | "flat_object")
}

impl Env<'_> {
    pub(crate) fn type_of(&self, field: &str) -> Option<String> {
        if let Some(t) = self.mapping.type_of(field) {
            return Some(t.to_string());
        }
        if let Some(t) =
            self.mapping.field_option(field, "type").and_then(|v| v.as_str().map(str::to_string))
        {
            return Some(t);
        }
        // the sub-fields a `search_as_you_type` field makes are text
        let (parent, _) = field.rsplit_once('.')?;
        (self.mapping.type_of(parent) == Some("search_as_you_type")).then(|| "text".to_string())
    }

    /// Whether the field is made by a script rather than read from the
    /// source, or sits inside an object that is.
    pub(crate) fn is_derived(&self, field: &str) -> bool {
        self.mapping.derived_fields().iter().any(|(name, _)| {
            field == name || field.strip_prefix(name.as_str()).is_some_and(|r| r.starts_with('.'))
        })
    }

    /// Whether the field's value is one token: a keyword, or anything else
    /// the analyzers do not cut.
    pub(crate) fn is_keyword(&self, field: &str) -> bool {
        self.type_of(field).map(|t| is_keyword_type(&t)).unwrap_or(false)
    }

    /// The fields of the mapping a pattern names that hold words.
    ///
    /// A query over every field asks each of them with its own analyzer, so
    /// the pattern is spelt out here rather than read as a field of its own:
    /// read as one, it was analysed with the default analyzer, and a stemmed
    /// sub-field that holds `notic` was never found to hold `notice`.
    pub(crate) fn fields_matching(&self, pattern: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .mapping
            .types
            .iter()
            .filter(|(k, t)| {
                (pattern == "*" || crate::store::glob_match(pattern, k))
                    && matches!(
                        t.as_str(),
                        "text"
                            | "match_only_text"
                            | "keyword"
                            | "search_as_you_type"
                            | "wildcard"
                            | "constant_keyword"
                    )
            })
            .map(|(k, _)| k.clone())
            .collect();
        out.sort();
        out
    }

    /// How many words a token of this field holds, where the field is one of
    /// the shingle sub-fields a `search_as_you_type` mapping makes.
    pub(crate) fn shingle_width(&self, field: &str) -> Option<usize> {
        let (_, leaf) = field.rsplit_once('.')?;
        let n = leaf.strip_prefix('_')?.strip_suffix("gram")?;
        n.parse::<usize>().ok().filter(|w| *w > 1)
    }

    /// Whether the last word of a bool prefix query is answered from a field
    /// of word beginnings rather than from this one.
    pub(crate) fn answers_prefixes_elsewhere(&self, field: &str) -> bool {
        self.type_of(field).as_deref() == Some("search_as_you_type")
            || self.shingle_width(field).is_some()
    }

    /// Whether a prefix query on this field is rewritten into a term on the
    /// field's own `_index_prefix`: a `search_as_you_type` field always, a
    /// text field with `index_prefixes` when the prefix is one it keeps.
    pub(crate) fn prefix_answered_elsewhere(&self, field: &str, prefix: &str) -> bool {
        if self.type_of(field).as_deref() == Some("search_as_you_type") {
            return true;
        }
        let Some(spec) = self.mapping.field_option(field, "index_prefixes") else { return false };
        let min = spec.get("min_chars").and_then(|v| v.as_u64()).unwrap_or(2) as usize;
        let max = spec.get("max_chars").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
        let n = prefix.chars().count();
        n >= min && n <= max
    }

    /// The analyzer the field's values were written with.
    fn index_analyzer(&self, field: &str) -> String {
        let base = match self.shingle_width(field) {
            Some(_) => field.rsplit_once('.').map(|(p, _)| p).unwrap_or(field),
            None => field,
        };
        if let Some(name) =
            self.mapping.field_option(base, "analyzer").and_then(|v| v.as_str().map(str::to_string))
        {
            return name;
        }
        if self.analysis.knows_named("default") {
            return "default".to_string();
        }
        "standard".to_string()
    }

    /// The analyzer the field is searched with.
    fn search_analyzer(&self, field: &str, asked: Option<&str>) -> String {
        if let Some(name) = asked {
            return name.to_string();
        }
        let base = match self.shingle_width(field) {
            Some(_) => field.rsplit_once('.').map(|(p, _)| p).unwrap_or(field),
            None => field,
        };
        for key in ["search_analyzer", "analyzer"] {
            if let Some(name) =
                self.mapping.field_option(base, key).and_then(|v| v.as_str().map(str::to_string))
            {
                return name;
            }
        }
        for name in ["default_search", "default"] {
            if self.analysis.knows_named(name) {
                return name.to_string();
            }
        }
        "standard".to_string()
    }

    /// Tokens in bytes, as the chains make them.
    fn cut(&self, analyzer: &str, text: &str) -> Vec<crate::analysis::Token> {
        match self.analysis.get(analyzer) {
            Some(chain) => chain.tokens(text),
            None => crate::query::analyze_spans(self.index, text, Some(analyzer)),
        }
    }

    /// A keyword's value as the index keeps it: through the normalizer the
    /// mapping names, when it names one.
    pub(crate) fn normalized(&self, field: &str, value: &str) -> String {
        let named = self
            .mapping
            .field_option(field, "normalizer")
            .and_then(|v| v.as_str().map(str::to_string));
        match named.and_then(|n| self.analysis.get(&n)) {
            Some(chain) => {
                chain.terms(value).into_iter().next().unwrap_or_else(|| value.to_string())
            }
            None => value.to_string(),
        }
    }

    /// The tokens of one value of the field, as it was indexed.
    pub(crate) fn index_tokens(&self, field: &str, text: &str) -> Vec<Tok> {
        if self.is_keyword(field) {
            let units = text.encode_utf16().count();
            return vec![Tok {
                term: self.normalized(field, text),
                pos: 0,
                len: 1,
                from: 0,
                to: units,
            }];
        }
        let mut raw = self.cut(&self.index_analyzer(field), text);
        crate::analysis::reported_offsets(text, &mut raw);
        let toks: Vec<Tok> = raw
            .into_iter()
            .map(|(term, pos, from, to, len)| Tok { term, pos, len: len.max(1), from, to })
            .collect();
        match self.shingle_width(field) {
            Some(width) => shingles(&toks, width),
            None => toks,
        }
    }

    /// The terms a query's text becomes on this field, with the place each
    /// stands in and how many places it spans.
    pub(crate) fn query_tokens(
        &self,
        field: &str,
        text: &str,
        analyzer: Option<&str>,
    ) -> Vec<(String, usize, usize)> {
        if analyzer.is_none() && self.is_keyword(field) {
            return vec![(self.normalized(field, text), 0, 1)];
        }
        let raw = self.cut(&self.search_analyzer(field, analyzer), text);
        let toks: Vec<Tok> = raw
            .into_iter()
            .map(|(term, pos, from, to, len)| Tok { term, pos, len: len.max(1), from, to })
            .collect();
        let toks = match self.shingle_width(field) {
            Some(width) => shingles(&toks, width),
            None => toks,
        };
        toks.into_iter().map(|t| (t.term, t.pos, t.len)).collect()
    }
}

/// Runs of `width` words, each one token spanning the words it holds.
fn shingles(toks: &[Tok], width: usize) -> Vec<Tok> {
    toks.windows(width)
        .enumerate()
        .map(|(i, run)| Tok {
            term: run.iter().map(|t| t.term.as_str()).collect::<Vec<_>>().join(" "),
            pos: i,
            len: 1,
            from: run[0].from,
            to: run[width - 1].to,
        })
        .collect()
}

/// A scalar of the source as the text a highlighter reads.
fn as_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn collect(node: &Value, segs: &[&str], out: &mut Vec<String>) {
    if segs.is_empty() {
        match node {
            Value::Array(items) => items.iter().for_each(|i| collect(i, segs, out)),
            other => out.extend(as_text(other)),
        }
        return;
    }
    match node {
        // an array of objects holds the field once in each of them
        Value::Array(items) => items.iter().for_each(|i| collect(i, segs, out)),
        Value::Object(o) => {
            // a key may be written with dots in it, `{"a.b": ...}`
            for k in 1..=segs.len() {
                if let Some(child) = o.get(&segs[..k].join(".")) {
                    collect(child, &segs[k..], out);
                }
            }
        }
        _ => {}
    }
}

/// The values a document holds for a field, in the order it holds them.
///
/// Arrays are read through at every level: `items.body` of a document whose
/// `items` is a list of objects is the `body` of each of them. The node read
/// the path with a JSON pointer, which stops at an array, so a field inside a
/// list of objects was never highlighted.
pub(crate) fn values_of(source: &Value, field: &str) -> Vec<String> {
    let segs: Vec<&str> = field.split('.').collect();
    let mut out = Vec::new();
    collect(source, &segs, &mut out);
    out
}
