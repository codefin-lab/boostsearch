//! The unified highlighter: the field's values joined into one text, the
//! query's matches found in it, and the best passages around them.
//!
//! This follows OpenSearch's `CustomFieldHighlighter` and Lucene's
//! `PassageScorer` step for step, because what comes back -- which passages,
//! cut where, in what order -- is decided by those details: a passage is a
//! sentence no longer than `fragment_size`, cut at words where the sentence
//! is longer; the passages are scored with BM25 over the passage; the best
//! `number_of_fragments` are kept and returned in the order of the text.

use super::breaks::{Bounds, Passages, sentence_bounds, word_bounds};
use super::{Encoder, Opts};

/// One match in the text: where it stands, and the term it counts as.
#[derive(Clone, Debug)]
pub(super) struct Match {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) key: String,
    /// how often that term matched in the whole text
    pub(super) freq: usize,
}

#[derive(Clone, Debug, Default)]
struct Passage {
    start: Option<usize>,
    end: usize,
    matches: Vec<Match>,
    score: f32,
}

const K1: f32 = 1.2;
const B: f32 = 0.75;
const PIVOT: f32 = 87.0;

fn tf(freq: usize, passage_len: usize) -> f32 {
    let norm = K1 * ((1.0 - B) + B * (passage_len as f32 / PIVOT));
    freq as f32 / (freq as f32 + norm)
}

fn weight(content_len: usize, freq_in_doc: usize) -> f32 {
    let docs = 1.0 + content_len as f32 / PIVOT;
    (K1 + 1.0) * (1.0 + (docs as f64 + 0.5) / (freq_in_doc as f64 + 0.5)).ln() as f32
}

fn norm(start: usize) -> f32 {
    1.0 + 1.0 / ((PIVOT + start as f32) as f64).ln() as f32
}

fn score(p: &Passage, content_len: usize) -> f32 {
    // terms in the order they first matched in the passage
    let mut terms: Vec<(&str, usize, usize)> = Vec::new();
    for m in &p.matches {
        match terms.iter_mut().find(|(k, _, _)| *k == m.key) {
            Some(t) => t.1 += 1,
            None => terms.push((&m.key, 1, m.freq)),
        }
    }
    let len = p.end - p.start.unwrap_or(0);
    let mut s = 0.0f32;
    for (_, in_passage, in_doc) in terms {
        s += tf(in_passage, len) * weight(content_len, in_doc);
    }
    s * norm(p.start.unwrap_or(0))
}

/// Whether `a` sorts below `b` in the queue of best passages: the lower
/// score, and at the same score the earlier passage.
fn below(a: &Passage, b: &Passage) -> bool {
    if a.score != b.score {
        return a.score < b.score;
    }
    a.start < b.start
}

fn offer(queue: &mut Vec<Passage>, mut passage: Passage, max: usize, content_len: usize) {
    if passage.start.is_none() {
        return;
    }
    passage.score = score(&passage, content_len);
    let least = queue.iter().enumerate().fold(None, |best: Option<usize>, (i, p)| match best {
        Some(b) if !below(p, &queue[b]) => Some(b),
        _ => Some(i),
    });
    if queue.len() == max
        && let Some(l) = least
        && passage.score < queue[l].score
    {
        return;
    }
    queue.push(passage);
    if queue.len() > max {
        let l =
            queue.iter().enumerate().fold(0, |b, (i, p)| if below(p, &queue[b]) { i } else { b });
        queue.remove(l);
    }
}

/// The passages of each value found apart, as Lucene's
/// `SplittingBreakIterator` finds them.
///
/// The values of a field are joined with a separator, and the break iterator
/// is run over one value at a time. Run over the joined text, a sentence ran
/// on into the next value and two values came back as one snippet with the
/// separator inside it.
struct Sliced<'a, F: Fn(&[u16]) -> Passages> {
    content: &'a [u16],
    make: F,
    current: Option<(usize, usize, Passages)>,
}

impl<'a, F: Fn(&[u16]) -> Passages> Sliced<'a, F> {
    fn new(content: &'a [u16], make: F) -> Self {
        Sliced { content, make, current: None }
    }

    /// The value holding the unit at `at`, made current.
    fn slice_of(&mut self, at: usize) -> &mut (usize, usize, Passages) {
        let inside = self.current.as_ref().is_some_and(|(s, e, _)| *s <= at && at < *e);
        if !inside {
            let start = self.content[..at.min(self.content.len())]
                .iter()
                .rposition(|u| *u == 0)
                .map(|i| i + 1)
                .unwrap_or(0);
            let end = self.content[start..]
                .iter()
                .position(|u| *u == 0)
                .map(|i| start + i)
                .unwrap_or(self.content.len());
            let passages = (self.make)(&self.content[start..end]);
            self.current = Some((start, end, passages));
        }
        self.current.as_mut().expect("just made")
    }

    fn preceding(&mut self, offset: usize) -> usize {
        let (start, _, p) = self.slice_of(offset.saturating_sub(1));
        let start = *start;
        start + p.preceding(offset - start)
    }

    fn following(&mut self, offset: usize) -> usize {
        let (start, end, p) = self.slice_of(offset);
        let (start, end) = (*start, *end);
        start + p.following(offset - start, end - start)
    }
}

/// What the highlighter returns for a field: the snippets, each with the
/// score it was chosen by.
pub(super) fn highlight(
    content: &[u16],
    matches: &mut [Match],
    opts: &Opts,
    tokenized: bool,
    encoder: Encoder,
) -> Vec<(String, f32)> {
    let len = content.len();
    let whole = opts.fragments == 0 || !tokenized;
    let max = if opts.fragments == 0 { usize::MAX - 1 } else { opts.fragments as usize };
    let mut breaks = Sliced::new(content, |slice| {
        if whole {
            // a passage is a value of the field, and nothing shorter
            Passages::Plain(Bounds(vec![0, slice.len()]))
        } else {
            match opts.boundary_scanner.as_deref() {
                Some("word") => Passages::Plain(Bounds(word_bounds(slice))),
                _ if opts.fragment_size > 0 => {
                    Passages::bounded(slice, opts.fragment_size as usize)
                }
                _ => Passages::Plain(Bounds(sentence_bounds(slice))),
            }
        }
    });
    matches.sort_by(|a, b| (a.start, a.end, &a.key).cmp(&(b.start, b.end, &b.key)));
    let mut queue: Vec<Passage> = Vec::new();
    let mut passage = Passage::default();
    for m in matches.iter() {
        if m.start < len && m.end > len {
            continue;
        }
        if passage.start.is_none() || m.start >= passage.end {
            offer(&mut queue, std::mem::take(&mut passage), max, len);
            if m.start >= len {
                break;
            }
            let start = breaks.preceding(m.start + 1);
            let end = breaks.following(m.start).min(len);
            passage = Passage { start: Some(start), end, matches: Vec::new(), score: 0.0 };
        }
        passage.matches.push(m.clone());
    }
    offer(&mut queue, passage, max, len);
    queue.sort_by_key(|p| p.start);
    if queue.is_empty() && opts.no_match_size > 0 {
        return no_match(content, opts.no_match_size as usize, encoder);
    }
    let pre = opts.pre_tags.first().map(String::as_str).unwrap_or("<em>");
    let post = opts.post_tags.first().map(String::as_str).unwrap_or("</em>");
    queue.iter().map(|p| (format(content, p, pre, post, encoder), p.score)).collect()
}

/// The beginning of the first value, cut at the first word boundary past
/// `no_match_size`.
fn no_match(content: &[u16], size: usize, encoder: Encoder) -> Vec<(String, f32)> {
    let mut pos = 0;
    while pos < content.len() && content[pos] == 0 {
        pos += 1;
    }
    if pos >= content.len() {
        return Vec::new();
    }
    let mut end =
        content[pos..].iter().position(|u| *u == 0).map(|i| pos + i).unwrap_or(content.len());
    if size + pos < end {
        end = Bounds(word_bounds(content)).following(size + pos).unwrap_or(content.len());
    }
    let text = encoder.encode(&String::from_utf16_lossy(&content[pos..end]));
    vec![(java_trim(&text).to_string(), f32::NAN)]
}

/// `String.trim()`: every character up to and including the space, taken
/// off both ends.
pub(super) fn java_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c <= ' ')
}

fn format(content: &[u16], p: &Passage, pre: &str, post: &str, encoder: Encoder) -> String {
    let piece = |a: usize, b: usize| encoder.encode(&String::from_utf16_lossy(&content[a..b]));
    let start = p.start.unwrap_or(0);
    let mut out = String::new();
    let mut pos = start;
    let mut i = 0;
    while i < p.matches.len() {
        let m = &p.matches[i];
        let begin = m.start.max(pos);
        out.push_str(&piece(pos, begin.max(pos)));
        let mut end = m.end;
        // matches that overlap are marked as one
        while i + 1 < p.matches.len() && p.matches[i + 1].start < end {
            i += 1;
            end = p.matches[i].end;
        }
        let end = end.min(p.end).max(begin);
        out.push_str(pre);
        out.push_str(&piece(begin, end));
        out.push_str(post);
        pos = end;
        i += 1;
    }
    out.push_str(&piece(pos, p.end.max(pos)));
    if out.ends_with('\u{2029}') || out.ends_with('\0') {
        out.pop();
    }
    java_trim(&out).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches_of(text: &str, words: &[&str]) -> Vec<Match> {
        let lower = text.to_lowercase();
        let mut out = Vec::new();
        for w in words {
            let mut from = 0;
            while let Some(at) = lower[from..].find(w) {
                let start = from + at;
                out.push(Match { start, end: start + w.len(), key: w.to_string(), freq: 1 });
                from = start + w.len();
            }
        }
        out
    }

    /// The whole clause came back where the reference returns the sentence
    /// cut to about a hundred characters around the match.
    #[test]
    fn a_passage_is_cut_to_the_fragment_size() {
        let text = "Either party may terminate this agreement immediately upon written notice if the \
                    other party commits a material breach and fails to cure that breach within \
                    thirty days.";
        let content: Vec<u16> = text.encode_utf16().collect();
        let mut m = matches_of(text, &["written", "notice"]);
        let opts = Opts::of(&serde_json::json!({}), &serde_json::json!({}));
        let out = highlight(&content, &mut m, &opts, true, Encoder::Default);
        assert_eq!(
            out.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(),
            vec![
                "Either party may terminate this agreement immediately upon <em>written</em> \
                 <em>notice</em> if the other party commits"
            ]
        );
    }

    /// Two values of a field are two passages, never one with the separator
    /// inside it.
    #[test]
    fn the_values_of_a_field_are_passages_apart() {
        let content: Vec<u16> =
            "no match here\0written notice given\0another written one".encode_utf16().collect();
        let mut m = vec![
            Match { start: 14, end: 21, key: "written".into(), freq: 2 },
            Match { start: 43, end: 50, key: "written".into(), freq: 2 },
        ];
        let opts = Opts::of(&serde_json::json!({}), &serde_json::json!({}));
        let out = highlight(&content, &mut m, &opts, true, Encoder::Default);
        assert_eq!(
            out.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(),
            vec!["<em>written</em> notice given", "another <em>written</em> one"]
        );
    }
}
