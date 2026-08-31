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

pub(crate) fn analyze(ctx: &Ctx, view: View, text: &str) -> Vec<String> {
    analyze_with(ctx, view, text, None)
}

pub(crate) fn analyze_with(
    ctx: &Ctx,
    view: View,
    text: &str,
    analyzer: Option<&str>,
) -> Vec<String> {
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
