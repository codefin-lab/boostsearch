//! Cutting text into the tokens a query is matched by.

use super::*;

/// Put a query term through the same normalizer the field was indexed with,
/// so `ABCD` finds what `lowercase` stored as `abcd`.
pub(crate) fn normalized(ctx: &Ctx, field: &str, text: &str) -> String {
    match ctx.mapping.normalizer_of(field) {
        Some(n) => crate::store::normalize(&Value::String(text.to_string()), &n)
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| text.to_string()),
        None => text.to_string(),
    }
}

/// Map OpenSearch analyzer names onto the tokenizers BoostCore ships.
pub(crate) fn tokenizer_name(analyzer: Option<&str>) -> &str {
    match analyzer.unwrap_or("standard") {
        "whitespace" => "whitespace",
        "keyword" | "raw" => "raw",
        "english" | "en_stem" => "en_stem",
        _ => "default",
    }
}

/// Tokenise text with a named analyzer, for the `_analyze` endpoint.
pub fn analyze_text(index: &Index, text: &str, analyzer: Option<&str>) -> Vec<String> {
    let name = tokenizer_name(analyzer);
    let mut out = Vec::new();
    if let Some(mut tk) = index.tokenizers().get(name) {
        let mut stream = tk.token_stream(text);
        while stream.advance() {
            out.push(stream.token().text.clone());
        }
    }
    out
}

/// The same analysis, keeping where each token came from.
pub fn analyze_spans(
    index: &Index,
    text: &str,
    analyzer: Option<&str>,
) -> Vec<(String, usize, usize, usize)> {
    let name = tokenizer_name(analyzer);
    let mut out = Vec::new();
    if let Some(mut tk) = index.tokenizers().get(name) {
        let mut stream = tk.token_stream(text);
        while stream.advance() {
            let t = stream.token();
            out.push((t.text.clone(), t.position, t.offset_from, t.offset_to));
        }
    }
    out
}

pub(crate) fn analyze(ctx: &Ctx, view: View, field: &str, text: &str) -> Vec<String> {
    analyze_with(ctx, view, field, text, None)
}

/// The analyzer a field is queried with: the one the query named, else the
/// one the mapping named for searching, else the one it was written with.
fn named_for(ctx: &Ctx, field: &str, asked: Option<&str>) -> Option<String> {
    if let Some(name) = asked {
        return Some(name.to_string());
    }
    for key in ["search_analyzer", "analyzer"] {
        if let Some(name) =
            ctx.mapping.field_option(field, key).and_then(|v| v.as_str().map(|s| s.to_string()))
        {
            return Some(name);
        }
    }
    // an index may name the analyzer every search uses, and the one every
    // document is written with, without naming either on a field
    for name in ["default_search", "default"] {
        if ctx.analysis.knows_named(name) {
            return Some(name.to_string());
        }
    }
    None
}

pub(crate) fn analyze_with(
    ctx: &Ctx,
    view: View,
    field: &str,
    text: &str,
    analyzer: Option<&str>,
) -> Vec<String> {
    // an analyzer the index defined, or one of the built-ins under the name
    // OpenSearch gives it, cuts the query exactly as it cut the document
    if let Some(name) = named_for(ctx, field, analyzer)
        && let Some(chain) = ctx.analysis.get(&name)
    {
        let tokens = chain.terms(text);
        if !tokens.is_empty() || text.is_empty() {
            return tokens;
        }
    }
    if view == View::Raw && analyzer.is_none() {
        return vec![text.to_string()];
    }
    let name = tokenizer_name(analyzer);
    let mut out = Vec::new();
    if let Ok(mut tk) =
        ctx.index.tokenizers().get(name).ok_or(TantivyError::InvalidArgument("no tokenizer".into()))
    {
        let mut stream = tk.token_stream(text);
        while stream.advance() {
            out.push(stream.token().text.clone());
        }
    }
    if out.is_empty() && !text.is_empty() {
        out.push(text.to_lowercase());
    }
    out
}
