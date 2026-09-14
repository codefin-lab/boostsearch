//! The fast vector highlighter: Lucene's `FastVectorHighlighter` over the
//! term vectors a field keeps, which here are the field's tokens made again.
//!
//! What sets it apart from the others is that it works in phrases rather
//! than terms. A phrase of the query found in the text is one match and is
//! marked once -- `<em>written notice</em>` -- and each term or phrase of the
//! query takes its own tag when several are given.

use super::breaks::{Bounds, sentence_bounds, word_bounds};
use super::query::{Hq, Leaf};
use super::text::Tok;
use super::{Encoder, Opts};

/// A term or a phrase of the query, flattened out of it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Flat {
    pub(super) field: String,
    pub(super) terms: Vec<String>,
    pub(super) slop: usize,
    pub(super) boost: f32,
}

/// The query as the highlighter reads it: its terms and phrases, each with
/// the number that picks its tag. Patterns become the terms of the text
/// they match; span and interval queries are not read at all, as upstream.
pub(super) fn flatten(queries: &[Hq], texts: &[(String, Vec<Tok>)]) -> Vec<(Flat, usize)> {
    let mut flat: Vec<Flat> = Vec::new();
    let push = |flat: &mut Vec<Flat>, f: Flat| {
        if !flat.contains(&f) {
            flat.push(f);
        }
    };
    let expand = |field: &str, leaf: &Leaf| -> Vec<String> {
        match leaf {
            Leaf::Term(t) => vec![t.clone()],
            other => {
                let mut found: Vec<String> = texts
                    .iter()
                    .flat_map(|(_, toks)| toks.iter())
                    .filter(|t| other.matches(&t.term))
                    .map(|t| t.term.clone())
                    .collect();
                let _ = field;
                found.sort();
                found.dedup();
                found
            }
        }
    };
    for q in queries {
        match q {
            Hq::Leaf { field, leaf, boost } => {
                for term in expand(field, leaf) {
                    push(
                        &mut flat,
                        Flat { field: field.clone(), terms: vec![term], slop: 0, boost: *boost },
                    );
                }
            }
            Hq::Near { clauses, slop, phrase: true, boost, .. } => {
                // each place of the phrase holds one term or several; every
                // way of picking one from each is a phrase of its own
                let mut combos: Vec<(String, Vec<String>)> = vec![(String::new(), Vec::new())];
                for clause in clauses {
                    let options: Vec<(String, String)> = match clause {
                        Hq::Leaf { field, leaf, .. } => {
                            expand(field, leaf).into_iter().map(|t| (field.clone(), t)).collect()
                        }
                        Hq::Or(leaves) => leaves
                            .iter()
                            .filter_map(|l| match l {
                                Hq::Leaf { field, leaf, .. } => Some(
                                    expand(field, leaf)
                                        .into_iter()
                                        .map(|t| (field.clone(), t))
                                        .collect::<Vec<_>>(),
                                ),
                                _ => None,
                            })
                            .flatten()
                            .collect(),
                        _ => Vec::new(),
                    };
                    let mut next = Vec::new();
                    for (_, terms) in &combos {
                        for (field, t) in &options {
                            let mut more = terms.clone();
                            more.push(t.clone());
                            next.push((field.clone(), more));
                        }
                    }
                    combos = next;
                    if combos.len() > 256 {
                        combos.truncate(256);
                    }
                }
                for (field, terms) in combos {
                    let f = if terms.len() == 1 {
                        Flat { field, terms, slop: 0, boost: *boost }
                    } else {
                        Flat { field, terms, slop: *slop, boost: *boost }
                    };
                    push(&mut flat, f);
                }
            }
            _ => {}
        }
    }
    flat.into_iter().enumerate().map(|(i, f)| (f, i)).collect()
}

#[derive(Clone, Debug)]
struct TermInfo {
    term: String,
    start: usize,
    end: usize,
    pos: usize,
}

#[derive(Clone, Debug)]
struct PhraseInfo {
    offsets: Vec<(usize, usize)>,
    boost: f32,
    seq: usize,
}

impl PhraseInfo {
    fn new(terms: &[TermInfo], boost: f32, seq: usize) -> PhraseInfo {
        let mut offsets = vec![(terms[0].start, terms[0].end)];
        let mut pos = terms[0].pos;
        for t in &terms[1..] {
            if t.pos == pos + 1 {
                if let Some(last) = offsets.last_mut() {
                    last.1 = t.end;
                }
            } else {
                offsets.push((t.start, t.end));
            }
            pos = t.pos;
        }
        PhraseInfo { offsets, boost, seq }
    }
    fn start(&self) -> usize {
        self.offsets[0].0
    }
    fn end(&self) -> usize {
        self.offsets[self.offsets.len() - 1].1
    }
    fn overlaps(&self, o: &PhraseInfo) -> bool {
        let (so, eo, oso, oeo) = (self.start(), self.end(), o.start(), o.end());
        (so <= oso && oso < eo)
            || (so < oeo && oeo <= eo)
            || (oso <= so && so < oeo)
            || (oso < eo && eo <= oeo)
    }
}

/// The phrases of the query found in one field's tokens.
fn phrase_list(
    field: &str,
    toks: &[Tok],
    flats: &[(Flat, usize)],
    field_match: bool,
    limit: usize,
) -> Vec<PhraseInfo> {
    let wanted = |f: &Flat| {
        !field_match
            || f.field == field
            || f.field == "*"
            || crate::store::glob_match(&f.field, field)
    };
    let term_set: std::collections::HashSet<&str> = flats
        .iter()
        .filter(|(f, _)| wanted(f))
        .flat_map(|(f, _)| f.terms.iter().map(String::as_str))
        .collect();
    // the term vector holds the terms in order and each term's positions in
    // order; the stack is that, sorted by position
    let mut infos: Vec<TermInfo> = toks
        .iter()
        .filter(|t| term_set.contains(t.term.as_str()))
        .map(|t| TermInfo { term: t.term.clone(), start: t.from, end: t.to, pos: t.pos })
        .collect();
    infos.sort_by(|a, b| a.term.cmp(&b.term).then(a.pos.cmp(&b.pos)));
    infos.sort_by_key(|t| t.pos);
    // the terms at one position are read together
    let mut groups: Vec<Vec<TermInfo>> = Vec::new();
    for t in infos {
        match groups.last_mut() {
            Some(g) if g[0].pos == t.pos => g.push(t),
            _ => groups.push(vec![t]),
        }
    }
    let lookup = |seq: &[String]| -> Option<(bool, f32, usize, usize)> {
        // (terminal, boost, number, slop) of the query phrase map node
        let mut node: Option<(bool, f32, usize, usize)> = None;
        let mut prefix_exists = false;
        for (f, n) in flats.iter().filter(|(f, _)| wanted(f)) {
            if f.terms.len() >= seq.len() && f.terms[..seq.len()] == *seq {
                prefix_exists = true;
                if f.terms.len() == seq.len() {
                    node = Some((true, f.boost, *n, f.slop));
                }
            }
        }
        match node {
            Some(n) => Some(n),
            None if prefix_exists => Some((false, 0.0, 0, 0)),
            None => None,
        }
    };
    let mut stack: std::collections::VecDeque<Vec<TermInfo>> = groups.into();
    let mut out: Vec<PhraseInfo> = Vec::new();
    let add = |out: &mut Vec<PhraseInfo>, p: PhraseInfo| {
        if !out.iter().any(|e| e.overlaps(&p)) {
            out.push(p);
        }
    };
    while let Some(first) = stack.pop_front() {
        if out.len() >= limit {
            break;
        }
        let Some(ti) =
            first.iter().find(|t| lookup(std::slice::from_ref(&t.term)).is_some()).cloned()
        else {
            continue;
        };
        let mut candidate = vec![ti];
        loop {
            let next = stack.pop_front();
            let seq: Vec<String> = candidate.iter().map(|t| t.term.clone()).collect();
            let found = next.as_ref().and_then(|g| {
                g.iter()
                    .find(|t| {
                        let mut s = seq.clone();
                        s.push(t.term.clone());
                        lookup(&s).is_some()
                    })
                    .cloned()
            });
            match found {
                Some(t) => {
                    candidate.push(t);
                }
                None => {
                    if let Some(g) = next {
                        stack.push_front(g);
                    }
                    let seq: Vec<String> = candidate.iter().map(|t| t.term.clone()).collect();
                    let valid = |c: &[TermInfo], slop: usize| {
                        c.windows(2).all(|w| {
                            (w[1].pos as i64 - w[0].pos as i64 - 1).unsigned_abs() as usize <= slop
                        })
                    };
                    match lookup(&seq) {
                        Some((true, boost, n, slop))
                            if candidate.len() == 1 || valid(&candidate, slop) =>
                        {
                            add(&mut out, PhraseInfo::new(&candidate, boost, n));
                        }
                        _ => {
                            while candidate.len() > 1 {
                                let back = candidate.pop().expect("more than one");
                                stack.push_front(vec![back]);
                                let seq: Vec<String> =
                                    candidate.iter().map(|t| t.term.clone()).collect();
                                if let Some((true, boost, n, slop)) = lookup(&seq)
                                    && (candidate.len() == 1 || valid(&candidate, slop))
                                {
                                    add(&mut out, PhraseInfo::new(&candidate, boost, n));
                                    break;
                                }
                            }
                        }
                    }
                    break;
                }
            }
        }
    }
    out
}

/// Phrases found through several fields, the ones that overlap made one.
fn merge(lists: Vec<Vec<PhraseInfo>>) -> Vec<PhraseInfo> {
    let mut all: Vec<PhraseInfo> = lists.into_iter().flatten().collect();
    all.sort_by(|a, b| {
        (a.start(), a.end())
            .cmp(&(b.start(), b.end()))
            .then(a.boost.partial_cmp(&b.boost).unwrap_or(std::cmp::Ordering::Equal))
    });
    let mut out = Vec::new();
    let mut work: Vec<PhraseInfo> = Vec::new();
    let mut work_end = 0;
    let fold = |work: &mut Vec<PhraseInfo>| -> PhraseInfo {
        if work.len() == 1 {
            return work.remove(0);
        }
        let seq = work[0].seq;
        let boost = work.iter().map(|w| w.boost).sum();
        let mut offs: Vec<(usize, usize)> = work.iter().flat_map(|w| w.offsets.clone()).collect();
        offs.sort();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (s, e) in offs {
            match merged.last_mut() {
                Some(last) if s <= last.1 => last.1 = last.1.max(e),
                _ => merged.push((s, e)),
            }
        }
        work.clear();
        PhraseInfo { offsets: merged, boost, seq }
    };
    for p in all {
        if !work.is_empty() && p.start() <= work_end {
            work_end = work_end.max(p.end());
            work.push(p);
        } else {
            if !work.is_empty() {
                out.push(fold(&mut work));
            }
            work_end = p.end();
            work.push(p);
        }
    }
    if !work.is_empty() {
        out.push(fold(&mut work));
    }
    out
}

/// A phrase inside a fragment: the stretches it marks, the number that picks
/// its tag, and its boost.
type SubInfo = (Vec<(usize, usize)>, usize, f32);

#[derive(Clone, Debug)]
struct FragInfo {
    start: usize,
    end: usize,
    subs: Vec<SubInfo>,
    boost: f32,
}

fn frag_list(phrases: &[PhraseInfo], size: usize, margin: usize, whole: bool) -> Vec<FragInfo> {
    let info = |start: usize, end: usize, ps: &[PhraseInfo]| FragInfo {
        start,
        end,
        subs: ps.iter().map(|p| (p.offsets.clone(), p.seq, p.boost)).collect(),
        boost: ps.iter().map(|p| p.boost).sum(),
    };
    if whole {
        return if phrases.is_empty() { Vec::new() } else { vec![info(0, usize::MAX, phrases)] };
    }
    let mut out = Vec::new();
    let mut start_offset = 0usize;
    let mut i = 0;
    while i < phrases.len() {
        let p = &phrases[i];
        if p.start() < start_offset {
            i += 1;
            continue;
        }
        let current_start = p.start();
        let mut current_end = p.end();
        let span_start = current_start.saturating_sub(margin).max(start_offset);
        let span_end = current_end.max(span_start + size);
        let mut taken = vec![p.clone()];
        i += 1;
        while i < phrases.len() && phrases[i].end() <= span_end {
            current_end = phrases[i].end();
            taken.push(phrases[i].clone());
            i += 1;
        }
        let match_len = current_end - current_start;
        let new_margin = size.saturating_sub(match_len) / 2;
        let span_start = current_start.saturating_sub(new_margin).max(start_offset);
        let span_end = span_start + match_len.max(size);
        start_offset = span_end;
        out.push(info(span_start, span_end, &taken));
    }
    out
}

/// A fragment cut to the values it stands in, one piece for each.
fn discrete(frags: Vec<FragInfo>, values: &[Vec<u16>]) -> Vec<FragInfo> {
    let mut result = Vec::new();
    'frags: for mut frag in frags {
        let mut field_end = 0usize;
        for value in values {
            if value.is_empty() {
                field_end += 1;
                continue;
            }
            let field_start = field_end;
            field_end += value.len() + 1;
            if frag.start >= field_start
                && frag.end >= field_start
                && frag.start <= field_end
                && frag.end <= field_end
            {
                result.push(frag);
                continue 'frags;
            }
            if frag.subs.is_empty() {
                continue 'frags;
            }
            let first = frag.subs[0].0[0];
            if frag.start >= field_end || first.0 >= field_end {
                continue;
            }
            let frag_start = if frag.start > field_start && frag.start < field_end {
                frag.start
            } else {
                field_start
            };
            let frag_end =
                if frag.end > field_start && frag.end < field_end { frag.end } else { field_end };
            let mut subs = Vec::new();
            let mut boost = 0.0;
            for (offsets, seq, b) in frag.subs.iter_mut() {
                let mut kept = Vec::new();
                let mut remaining = Vec::new();
                let mut past = false;
                for &(s, e) in offsets.iter() {
                    if past || s >= field_end {
                        past = true;
                        remaining.push((s, e));
                        continue;
                    }
                    let after = s >= field_start;
                    let before = e < field_end;
                    if after && before {
                        kept.push((s, e));
                    } else if after {
                        kept.push((s, field_end - 1));
                        remaining.push((s, e));
                    } else if before {
                        kept.push((field_start, e));
                    } else {
                        kept.push((field_start, field_end - 1));
                        remaining.push((s, e));
                    }
                }
                *offsets = remaining;
                if !kept.is_empty() {
                    subs.push((kept, *seq, *b));
                    boost += *b;
                }
            }
            frag.subs.retain(|(o, _, _)| !o.is_empty());
            result.push(FragInfo { start: frag_start, end: frag_end, subs, boost });
        }
    }
    result.sort_by_key(|f| f.start);
    result
}

fn find_start(buffer: &[u16], start: usize, opts: &Opts) -> usize {
    match opts.boundary_scanner.as_deref() {
        Some("sentence") | Some("word") => {
            if start > buffer.len() || start < 1 {
                return start;
            }
            let b = if opts.boundary_scanner.as_deref() == Some("word") {
                word_bounds(buffer)
            } else {
                sentence_bounds(buffer)
            };
            Bounds(b).preceding(start)
        }
        _ => {
            if start > buffer.len() || start < 1 {
                return start;
            }
            let mut offset = start;
            let mut count = opts.boundary_max_scan;
            while offset > 0 && count > 0 {
                if opts.boundary_chars.contains(&buffer[offset - 1]) {
                    return offset;
                }
                offset -= 1;
                count -= 1;
            }
            if offset == 0 { 0 } else { start }
        }
    }
}

fn find_end(buffer: &[u16], start: usize, opts: &Opts) -> usize {
    match opts.boundary_scanner.as_deref() {
        Some("sentence") | Some("word") => {
            if start > buffer.len() {
                return start;
            }
            let b = if opts.boundary_scanner.as_deref() == Some("word") {
                word_bounds(buffer)
            } else {
                sentence_bounds(buffer)
            };
            Bounds(b).following(start).unwrap_or(buffer.len())
        }
        _ => {
            if start > buffer.len() {
                return start;
            }
            let mut offset = start;
            let mut count = opts.boundary_max_scan;
            while offset < buffer.len() && count > 0 {
                if opts.boundary_chars.contains(&buffer[offset]) {
                    return offset;
                }
                offset += 1;
                count -= 1;
            }
            start
        }
    }
}

/// The text of one fragment, marked.
///
/// Upstream reads the values into its buffer only as far as the fragment
/// needs, so what the boundary scanner can see past the fragment's end is
/// what has been read so far; `built` is how far that is.
fn make(
    all: &[u16],
    ends: &[usize],
    built: &mut usize,
    frag: &FragInfo,
    opts: &Opts,
    encoder: Encoder,
) -> String {
    for &end in ends {
        if *built >= frag.end {
            break;
        }
        *built = (*built).max(end);
    }
    let buffer = &all[..(*built).min(all.len())];
    // the space after the last value read is left out
    let buffer_len = buffer.len().saturating_sub(1);
    let eo = if buffer_len < frag.end { buffer_len } else { find_end(buffer, frag.end, opts) };
    let so = find_start(buffer, frag.start, opts);
    let eo = eo.max(so).min(buffer.len());
    let src = &buffer[so..eo];
    let piece = |a: usize, b: usize| {
        encoder.encode(&String::from_utf16_lossy(
            &src[a.min(src.len())..b.min(src.len()).max(a.min(src.len()))],
        ))
    };
    let mut out = String::new();
    let mut at = 0usize;
    for (offsets, seq, _) in &frag.subs {
        for &(s, e) in offsets {
            let (s, e) = (s.saturating_sub(so), e.saturating_sub(so));
            out.push_str(&piece(at, s));
            out.push_str(&opts.pre_tags[seq % opts.pre_tags.len()]);
            out.push_str(&piece(s, e));
            out.push_str(&opts.post_tags[seq % opts.post_tags.len()]);
            at = e;
        }
    }
    out.push_str(&piece(at, src.len()));
    out
}

/// The fragments of a field, from the phrases found in its own tokens and
/// in those of the fields it is told to match through.
pub(super) fn highlight(
    values: &[String],
    texts: &[(String, Vec<Tok>)],
    flats: &[(Flat, usize)],
    opts: &Opts,
    encoder: Encoder,
) -> Vec<String> {
    let field_match = opts.require_field_match;
    let whole = opts.fragments == 0;
    let max = if whole { usize::MAX } else { opts.fragments as usize };
    let size = if whole { usize::MAX / 4 } else { opts.fragment_size.max(0) as usize };
    let margin = opts.fragment_offset.unwrap_or(6).max(0) as usize;
    let lists: Vec<Vec<PhraseInfo>> = texts
        .iter()
        .map(|(field, toks)| phrase_list(field, toks, flats, field_match, opts.phrase_limit))
        .collect();
    let phrases =
        if lists.len() == 1 { lists.into_iter().next().unwrap_or_default() } else { merge(lists) };
    let units: Vec<Vec<u16>> = values.iter().map(|v| v.encode_utf16().collect()).collect();
    let mut buffer: Vec<u16> = Vec::new();
    let mut ends: Vec<usize> = Vec::new();
    for v in &units {
        buffer.extend(v);
        buffer.push(b' ' as u16);
        ends.push(buffer.len());
    }
    let mut built = 0usize;
    let mut frags = frag_list(&phrases, size, margin, whole);
    if frags.is_empty() {
        if opts.no_match_size > 0 && !values.is_empty() {
            let frag = FragInfo {
                start: 0,
                end: opts.no_match_size as usize,
                subs: Vec::new(),
                boost: 0.0,
            };
            let frags = if units.len() > 1 { discrete(vec![frag], &units) } else { vec![frag] };
            return frags
                .iter()
                .take(1)
                .map(|f| make(&buffer, &ends, &mut built, f, opts, encoder))
                .collect();
        }
        return Vec::new();
    }
    if units.len() > 1 {
        frags = discrete(frags, &units);
    }
    if opts.score_order && !whole {
        frags.sort_by(|a, b| {
            b.boost
                .partial_cmp(&a.boost)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.start.cmp(&b.start))
        });
    }
    frags.iter().take(max).map(|f| make(&buffer, &ends, &mut built, f, opts, encoder)).collect()
}
