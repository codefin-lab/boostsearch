//! The plain highlighter: Lucene's `Highlighter` over one value at a time,
//! with the fragments its fragmenter cuts and the scores its `QueryScorer`
//! gives them.

use super::text::Tok;
use super::{Encoder, Opts};

/// What the scorer knows of one term of the query.
#[derive(Clone, Debug)]
pub(super) struct Weighted {
    pub(super) weight: f32,
    /// a term of a phrase or a span query counts only where the query
    /// matched, not wherever the term stands
    pub(super) positional: bool,
    /// the positions each match of that query covered, first and last
    pub(super) spans: Vec<(usize, usize)>,
}

pub(super) type Scored = std::collections::HashMap<String, Weighted>;

/// One fragment of a value, and its score.
pub(super) struct Fragment {
    pub(super) text: String,
    pub(super) score: f32,
    num: usize,
}

struct Group {
    tokens: usize,
    /// where the group's tokens end, which decides whether the next token
    /// belongs to it
    end: usize,
    /// the stretch that is written out for the group
    start: usize,
    stop: usize,
    total: f32,
}

/// The best fragments of one value, best first.
pub(super) fn fragments(
    text: &[u16],
    toks: &[Tok],
    scored: &Scored,
    opts: &Opts,
    encoder: Encoder,
) -> Vec<Fragment> {
    let pre = opts.pre_tags.first().map(String::as_str).unwrap_or("<em>");
    let post = opts.post_tags.first().map(String::as_str).unwrap_or("</em>");
    let max_frags = if opts.fragments == 0 { 1 } else { opts.fragments as usize };
    let size = opts.fragment_size.max(0) as usize;
    let span_fragmenter = opts.fragmenter.as_deref() != Some("simple");
    let whole = opts.fragments == 0;
    let piece = |a: usize, b: usize| encoder.encode(&String::from_utf16_lossy(&text[a..b]));

    let mut out = String::new();
    // (start in `out`, end in `out`, score)
    let mut frags: Vec<(usize, usize, f32)> = Vec::new();
    let mut frag_start = 0usize;
    let mut found: std::collections::HashSet<&str> = Default::default();
    let mut frag_score = 0.0f32;
    let mut last_end = 0usize;
    let mut group = Group { tokens: 0, end: 0, start: 0, stop: 0, total: 0.0 };
    let mut current_frags = 1usize;
    let mut wait_for: Option<usize> = None;

    let flush = |group: &mut Group, out: &mut String, last_end: &mut usize| {
        if group.start > *last_end {
            out.push_str(&piece(*last_end, group.start));
        }
        let t = piece(group.start, group.stop);
        if group.total > 0.0 {
            out.push_str(pre);
            out.push_str(&t);
            out.push_str(post);
        } else {
            out.push_str(&t);
        }
        *last_end = (*last_end).max(group.stop);
        group.tokens = 0;
        group.total = 0.0;
    };

    // The span fragmenter keeps its own count of positions, and counts only
    // the tokens it is asked about -- never the first one, nor one that
    // overlaps the token before it -- so its count runs behind the scorer's.
    // Which phrase it waits to finish before cutting depends on that count,
    // so it is kept the same way here.
    let mut fragmenter_pos: i64 = -1;
    let mut prev_pos: i64 = -1;
    for tok in toks {
        if tok.to > text.len() || tok.from > text.len() {
            continue;
        }
        let increment = tok.pos as i64 - prev_pos;
        prev_pos = tok.pos as i64;
        if group.tokens > 0 && tok.from >= group.end {
            flush(&mut group, &mut out, &mut last_end);
            // does this token begin a new fragment?
            let new_frag = if whole {
                false
            } else if span_fragmenter {
                fragmenter_pos += increment;
                let held = match wait_for {
                    Some(w) if w as i64 <= fragmenter_pos => {
                        wait_for = None;
                        false
                    }
                    Some(_) => true,
                    None => false,
                };
                if held {
                    false
                } else {
                    if let Some(w) = scored.get(&tok.term)
                        && let Some((_, last)) =
                            w.spans.iter().find(|(first, _)| *first as i64 == fragmenter_pos)
                    {
                        wait_for = Some(last + 1);
                    }
                    tok.to >= size * current_frags && text.len().saturating_sub(tok.to) >= size >> 1
                }
            } else {
                tok.to >= size * current_frags
            };
            if new_frag {
                current_frags += 1;
                frags.push((frag_start, out.len(), frag_score));
                frag_start = out.len();
                found.clear();
                frag_score = 0.0;
            }
        }
        // the token's own score
        let score = match scored.get(&tok.term) {
            Some(w)
                if !w.positional || w.spans.iter().any(|(a, b)| *a <= tok.pos && tok.pos <= *b) =>
            {
                if found.insert(tok.term.as_str()) {
                    frag_score += w.weight;
                }
                w.weight
            }
            _ => 0.0,
        };
        // tokens that overlap make one group; what is marked of it is the
        // stretch its scoring tokens cover, not the whole group -- a shingle
        // that matched inside a run of shingles marks its own words
        if group.tokens == 0 {
            group.end = tok.to;
            group.start = tok.from;
            group.stop = tok.to;
            group.total = score;
            group.tokens = 1;
        } else if group.tokens < 50 {
            group.end = group.end.max(tok.to);
            if score > 0.0 {
                if group.total == 0.0 {
                    group.start = tok.from;
                    group.stop = tok.to;
                } else {
                    group.start = group.start.min(tok.from);
                    group.stop = group.stop.max(tok.to);
                }
                group.total += score;
            }
            group.tokens += 1;
        }
    }
    if group.tokens > 0 {
        flush(&mut group, &mut out, &mut last_end);
    }
    if last_end < text.len() {
        out.push_str(&piece(last_end, text.len()));
    }
    frags.push((frag_start, out.len(), frag_score));

    // the best `max_frags`, best first and the earlier of two equal ones first
    let mut numbered: Vec<Fragment> = frags
        .into_iter()
        .enumerate()
        .map(|(num, (a, b, score))| Fragment { text: out[a..b].to_string(), score, num })
        .collect();
    numbered.sort_by(|x, y| {
        y.score.partial_cmp(&x.score).unwrap_or(std::cmp::Ordering::Equal).then(x.num.cmp(&y.num))
    });
    numbered.truncate(max_frags);
    numbered
}

/// Where the beginning of a value given back for no match ends: at the last
/// token that ends by `size`.
pub(super) fn no_match_end(toks: &[Tok], size: usize, len: usize) -> Option<usize> {
    if len <= size && toks.is_empty() {
        return None;
    }
    let mut end: Option<usize> = None;
    for t in toks {
        if t.to >= size {
            if t.to == size {
                end = Some(size);
            }
            return end;
        }
        end = Some(t.to);
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(text: &str) -> Vec<Tok> {
        let mut out = Vec::new();
        let mut start = None;
        for (i, c) in text.char_indices().chain(std::iter::once((text.len(), ' '))) {
            match (c.is_alphanumeric(), start) {
                (true, None) => start = Some(i),
                (false, Some(s)) => {
                    let pos = out.len();
                    out.push(Tok { term: text[s..i].to_lowercase(), pos, len: 1, from: s, to: i });
                    start = None;
                }
                _ => {}
            }
        }
        out
    }

    /// The case the contract example reported: two fragments of forty
    /// characters asked for, and the whole clause given back.
    #[test]
    fn fragments_are_cut_where_the_span_fragmenter_cuts_them() {
        let text = "Either party may terminate this agreement immediately upon written notice if the \
                    other party commits a material breach and fails to cure that breach within \
                    thirty days.";
        let toks = tokens(text);
        let mut scored = Scored::new();
        for w in ["written", "notice"] {
            scored.insert(
                w.to_string(),
                Weighted { weight: 1.0, positional: false, spans: Vec::new() },
            );
        }
        let opts = Opts::of(
            &serde_json::json!({}),
            &serde_json::json!({"type": "plain", "fragment_size": 40, "number_of_fragments": 2}),
        );
        let units: Vec<u16> = text.encode_utf16().collect();
        let out: Vec<String> = fragments(&units, &toks, &scored, &opts, Encoder::Default)
            .into_iter()
            .filter(|f| f.score > 0.0)
            .map(|f| f.text)
            .collect();
        assert_eq!(out, vec![" agreement immediately upon <em>written</em> <em>notice</em> if"]);
    }
}
