//! `intervals`: rules over where words stand, matched and scored as Lucene's
//! interval iterators do it.
//!
//! The rules used to be built as the queries nearest them -- a `match` for a
//! word, a bool for `all_of` -- and the candidates were then read back from
//! the source and analysed again to see whether the words really stood where
//! the rule said. That found the right documents only as often as the source
//! and the index agreed (a sub-field such as `body.english` is not in the
//! source at all), could not stand anywhere but at the top of a query, and
//! scored every hit a flat half.
//!
//! What is here builds the rule into the tree of sources Lucene builds --
//! with the same rewrites, since a filter over an `any_of` is pushed below it
//! and a repeated word becomes one source standing twice -- and walks each
//! document's positions with the same iterators. A document scores
//! `boost * freq / (freq + 1)`, where each interval found adds
//! `1 / (width - minimum width + 1)`: a match as tight as the rule allows
//! counts one, a looser one less.

use super::positions::{LuceneHeap, NO_MORE, SegmentPositions, docs_of, intersect, union};
use super::*;

/// A script that decides which intervals stay, compiled once.
#[derive(Clone)]
pub(crate) struct IntervalScript(Arc<crate::painless::contexts::Compiled>);

impl std::fmt::Debug for IntervalScript {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "IntervalScript")
    }
}

impl PartialEq for IntervalScript {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// Lucene's `IntervalsSource`, one variant per class it builds.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Source {
    Term(Term),
    /// a prefix, wildcard, regexp or fuzzy term, as the words it expands to
    Multi(Vec<Term>),
    Nothing,
    Or(Vec<Source>, bool),
    Ordered(Vec<Source>),
    Unordered(Vec<Source>),
    Block(Vec<Source>),
    Repeating(Box<Source>, usize),
    MaxGaps(Box<Source>, i32),
    Extend(Box<Source>, i32, i32),
    Containing(Box<Source>, Box<Source>),
    ContainedBy(Box<Source>, Box<Source>),
    Overlapping(Box<Source>, Box<Source>),
    NotContaining(Box<Source>, Box<Source>),
    NotContainedBy(Box<Source>, Box<Source>),
    NonOverlapping(Box<Source>, Box<Source>),
    Offset(Box<Source>, bool),
    Script(Box<Source>, IntervalScript),
}

impl Source {
    /// `Intervals.or`: nested disjunctions are flattened and repeats dropped.
    pub(crate) fn or(sources: Vec<Source>) -> Source {
        let mut simplified: Vec<Source> = Vec::new();
        for source in sources {
            let pulled = match &source {
                Source::Or(..) => source.pull_up(),
                _ => vec![source],
            };
            for one in pulled {
                if !simplified.contains(&one) {
                    simplified.push(one);
                }
            }
        }
        if simplified.len() == 1 {
            return simplified.remove(0);
        }
        Source::Or(simplified, true)
    }

    /// `Intervals.ordered`: a run of the same source is that source repeated.
    pub(crate) fn ordered(mut sources: Vec<Source>) -> Source {
        if sources.len() == 1 {
            return sources.remove(0);
        }
        let mut runs: Vec<(Source, usize)> = Vec::new();
        for source in sources {
            match runs.last_mut() {
                Some((last, count)) if *last == source => *count += 1,
                _ => runs.push((source, 1)),
            }
        }
        let mut rewritten: Vec<Source> =
            runs.into_iter().map(|(s, n)| Source::repeating(s, n)).collect();
        if rewritten.len() == 1 {
            return rewritten.remove(0);
        }
        Source::Ordered(rewritten)
    }

    /// `Intervals.unordered`: the same source anywhere in the list is that
    /// source repeated.
    pub(crate) fn unordered(mut sources: Vec<Source>) -> Source {
        if sources.len() == 1 {
            return sources.remove(0);
        }
        let mut counts: Vec<(Source, usize)> = Vec::new();
        for source in sources {
            match counts.iter_mut().find(|(s, _)| *s == source) {
                Some((_, count)) => *count += 1,
                None => counts.push((source, 1)),
            }
        }
        let mut rewritten: Vec<Source> =
            counts.into_iter().map(|(s, n)| Source::repeating(s, n)).collect();
        if rewritten.len() == 1 {
            return rewritten.remove(0);
        }
        Source::Unordered(rewritten)
    }

    fn repeating(source: Source, count: usize) -> Source {
        match count {
            1 => source,
            n => Source::Repeating(Box::new(source), n),
        }
    }

    /// `Intervals.phrase`: disjunctions inside are pulled up into an or of
    /// phrases, and a phrase inside a phrase is one phrase.
    pub(crate) fn phrase(mut sources: Vec<Source>) -> Source {
        if sources.len() == 1 {
            return sources.remove(0);
        }
        Source::or(pull_up_list(sources, &|list| {
            let mut flat = Vec::new();
            for s in list {
                match s {
                    Source::Block(inner) => flat.extend(inner),
                    other => flat.push(other),
                }
            }
            Source::Block(flat)
        }))
    }

    pub(crate) fn max_gaps(source: Source, gaps: i32) -> Source {
        Source::or(
            source.pull_up().into_iter().map(|s| Source::MaxGaps(Box::new(s), gaps)).collect(),
        )
    }

    pub(crate) fn containing(big: Source, small: Source) -> Source {
        Source::or(pull_up_one(big, &|s| Source::Containing(Box::new(s), Box::new(small.clone()))))
    }

    pub(crate) fn contained_by(small: Source, big: Source) -> Source {
        Source::or(pull_up_one(big, &|s| Source::ContainedBy(Box::new(small.clone()), Box::new(s))))
    }

    pub(crate) fn not_containing(minuend: Source, subtrahend: Source) -> Source {
        Source::or(pull_up_one(minuend, &|s| {
            Source::NotContaining(Box::new(s), Box::new(subtrahend.clone()))
        }))
    }

    pub(crate) fn not_contained_by(small: Source, big: Source) -> Source {
        Source::or(pull_up_one(big, &|s| {
            Source::NotContainedBy(Box::new(small.clone()), Box::new(s))
        }))
    }

    pub(crate) fn before(source: Source, reference: Source) -> Source {
        let preceding =
            Source::Extend(Box::new(Source::Offset(Box::new(reference), true)), i32::MAX, 0);
        Source::contained_by(source, preceding)
    }

    pub(crate) fn after(source: Source, reference: Source) -> Source {
        let following =
            Source::Extend(Box::new(Source::Offset(Box::new(reference), false)), 0, i32::MAX);
        Source::contained_by(source, following)
    }

    /// Lucene's `pullUpDisjunctions`: the alternatives this source stands for
    /// when a disjunction inside it is lifted out.
    fn pull_up(&self) -> Vec<Source> {
        match self {
            Source::Nothing => Vec::new(),
            Source::Or(subs, true) => subs.clone(),
            Source::Ordered(subs) => pull_up_list(subs.clone(), &Source::Ordered),
            Source::Unordered(subs) => pull_up_list(subs.clone(), &Source::Unordered),
            Source::Containing(big, small) => {
                pull_up_one((**big).clone(), &|s| Source::Containing(Box::new(s), small.clone()))
            }
            Source::ContainedBy(small, big) => {
                pull_up_one((**big).clone(), &|s| Source::ContainedBy(small.clone(), Box::new(s)))
            }
            Source::Overlapping(source, reference) => {
                pull_up_list(vec![(**source).clone(), (**reference).clone()], &|mut l| {
                    let reference = l.remove(1);
                    Source::Overlapping(Box::new(l.remove(0)), Box::new(reference))
                })
            }
            Source::Extend(inner, before, after) => {
                let pulled = inner.pull_up();
                if pulled.is_empty() {
                    return vec![self.clone()];
                }
                pulled.into_iter().map(|s| Source::Extend(Box::new(s), *before, *after)).collect()
            }
            other => vec![other.clone()],
        }
    }

    /// The fewest positions an interval of this source can cover.
    pub(crate) fn min_extent(&self) -> i32 {
        match self {
            Source::Term(_) | Source::Multi(_) | Source::Offset(..) => 1,
            Source::Nothing => 0,
            Source::Or(subs, _) => subs.iter().map(|s| s.min_extent()).min().unwrap_or(i32::MAX),
            Source::Ordered(subs) | Source::Unordered(subs) | Source::Block(subs) => {
                subs.iter().fold(0i32, |acc, s| acc.wrapping_add(s.min_extent()))
            }
            Source::Repeating(inner, _) | Source::MaxGaps(inner, _) | Source::Script(inner, _) => {
                inner.min_extent()
            }
            Source::Extend(inner, before, after) => {
                let extent = before.wrapping_add(inner.min_extent()).wrapping_add(*after);
                if extent < 0 { i32::MAX } else { extent }
            }
            Source::Containing(first, _)
            | Source::ContainedBy(first, _)
            | Source::Overlapping(first, _)
            | Source::NotContaining(first, _)
            | Source::NotContainedBy(first, _)
            | Source::NonOverlapping(first, _) => first.min_extent(),
        }
    }

    fn terms(&self, out: &mut Vec<Term>) {
        let mut add = |t: &Term| {
            if !out.contains(t) {
                out.push(t.clone())
            }
        };
        match self {
            Source::Term(t) => add(t),
            Source::Multi(ts) => ts.iter().for_each(add),
            Source::Nothing => {}
            Source::Or(subs, _)
            | Source::Ordered(subs)
            | Source::Unordered(subs)
            | Source::Block(subs) => subs.iter().for_each(|s| s.terms(out)),
            Source::Repeating(inner, _)
            | Source::MaxGaps(inner, _)
            | Source::Extend(inner, _, _)
            | Source::Offset(inner, _)
            | Source::Script(inner, _) => inner.terms(out),
            Source::Containing(a, b)
            | Source::ContainedBy(a, b)
            | Source::Overlapping(a, b)
            | Source::NotContaining(a, b)
            | Source::NotContainedBy(a, b)
            | Source::NonOverlapping(a, b) => {
                a.terms(out);
                b.terms(out);
            }
        }
    }

    /// Whether this source's iterator stands on a document at all: every
    /// part of a conjunction does, any part of a disjunction, and the first
    /// part of a difference.
    fn on_doc(&self, present: &dyn Fn(&Term) -> bool) -> bool {
        match self {
            Source::Term(t) => present(t),
            Source::Multi(ts) => ts.iter().any(present),
            Source::Nothing => false,
            Source::Or(subs, _) => subs.iter().any(|s| s.on_doc(present)),
            Source::Ordered(subs) | Source::Unordered(subs) | Source::Block(subs) => {
                subs.iter().all(|s| s.on_doc(present))
            }
            Source::Repeating(inner, _)
            | Source::MaxGaps(inner, _)
            | Source::Extend(inner, _, _)
            | Source::Offset(inner, _)
            | Source::Script(inner, _) => inner.on_doc(present),
            Source::Containing(a, b) | Source::ContainedBy(a, b) | Source::Overlapping(a, b) => {
                a.on_doc(present) && b.on_doc(present)
            }
            Source::NotContaining(a, _)
            | Source::NotContainedBy(a, _)
            | Source::NonOverlapping(a, _) => a.on_doc(present),
        }
    }

    /// The documents of a segment this source's iterator stands on.
    fn candidates(&self, docs: &dyn Fn(&Term) -> Vec<velocore::DocId>) -> Vec<velocore::DocId> {
        let any = |subs: &[Source]| {
            subs.iter()
                .fold(Vec::new(), |acc: Vec<velocore::DocId>, s| union(&acc, &s.candidates(docs)))
        };
        let all = |subs: &[&Source]| {
            let mut acc: Option<Vec<velocore::DocId>> = None;
            for s in subs {
                let here = s.candidates(docs);
                acc = Some(match acc {
                    None => here,
                    Some(before) => intersect(&before, &here),
                });
            }
            acc.unwrap_or_default()
        };
        match self {
            Source::Term(t) => docs(t),
            Source::Multi(ts) => {
                ts.iter().fold(Vec::new(), |acc: Vec<velocore::DocId>, t| union(&acc, &docs(t)))
            }
            Source::Nothing => Vec::new(),
            Source::Or(subs, _) => any(subs),
            Source::Ordered(subs) | Source::Unordered(subs) | Source::Block(subs) => {
                all(&subs.iter().collect::<Vec<_>>())
            }
            Source::Repeating(inner, _)
            | Source::MaxGaps(inner, _)
            | Source::Extend(inner, _, _)
            | Source::Offset(inner, _)
            | Source::Script(inner, _) => inner.candidates(docs),
            Source::Containing(a, b) | Source::ContainedBy(a, b) | Source::Overlapping(a, b) => {
                all(&[a, b])
            }
            Source::NotContaining(a, _)
            | Source::NotContainedBy(a, _)
            | Source::NonOverlapping(a, _) => a.candidates(docs),
        }
    }

    /// The iterator of this source over one document it stands on, reset for
    /// that document the way advancing to it resets Lucene's.
    fn iterator<'a>(&self, doc: &DocPlaces<'a>) -> Box<dyn Intervals + 'a> {
        let present = |t: &Term| doc.of(t).is_some_and(|p| !p.is_empty());
        let mut it: Box<dyn Intervals + 'a> = match self {
            Source::Term(t) => {
                Box::new(TermIt { places: doc.of(t).unwrap_or(&[]), next: 0, upto: 0, pos: -1 })
            }
            Source::Multi(ts) => Box::new(Disjunction::new(
                ts.iter()
                    .filter(|t| present(t))
                    .map(|t| Source::Term(t.clone()).iterator(doc))
                    .collect(),
            )),
            Source::Nothing => Box::new(TermIt { places: &[], next: 0, upto: 0, pos: NO_MORE }),
            Source::Or(subs, _) => Box::new(Disjunction::new(
                subs.iter().filter(|s| s.on_doc(&present)).map(|s| s.iterator(doc)).collect(),
            )),
            Source::Ordered(subs) => Box::new(OrderedIt {
                subs: subs.iter().map(|s| s.iterator(doc)).collect(),
                start: -1,
                end: -1,
                slop: 0,
                i: 1,
            }),
            Source::Unordered(subs) => Box::new(UnorderedIt {
                subs: subs.iter().map(|s| s.iterator(doc)).collect(),
                queue: LuceneHeap::new(),
                start: -1,
                end: -1,
                slop: 0,
                queue_end: -1,
            }),
            Source::Block(subs) => Box::new(BlockIt {
                subs: subs.iter().map(|s| s.iterator(doc)).collect(),
                start: -1,
                end: -1,
            }),
            Source::Repeating(inner, count) => Box::new(RepeatIt {
                inner: inner.iterator(doc),
                cache: vec![-1; count * 2],
                length: *count,
                base: 0,
                started: false,
                exhausted: false,
            }),
            Source::MaxGaps(inner, gaps) => {
                Box::new(FilterIt { inner: inner.iterator(doc), accept: Accept::MaxGaps(*gaps) })
            }
            Source::Script(inner, script) => Box::new(FilterIt {
                inner: inner.iterator(doc),
                accept: Accept::Script(script.clone()),
            }),
            Source::Extend(inner, before, after) => Box::new(ExtendIt {
                inner: inner.iterator(doc),
                before: *before,
                after: *after,
                positioned: false,
            }),
            Source::Offset(inner, before) => {
                Box::new(OffsetIt { inner: inner.iterator(doc), before: *before })
            }
            Source::Containing(a, b) | Source::ContainedBy(a, b) | Source::Overlapping(a, b) => {
                let kind = match self {
                    Source::Containing(..) => Relation::Containing,
                    Source::ContainedBy(..) => Relation::ContainedBy,
                    _ => Relation::Overlapping,
                };
                Box::new(FilteringIt { a: a.iterator(doc), b: b.iterator(doc), bpos: false, kind })
            }
            Source::NotContaining(a, b)
            | Source::NotContainedBy(a, b)
            | Source::NonOverlapping(a, b) => {
                let kind = match self {
                    Source::NotContaining(..) => Difference::NotContaining,
                    Source::NotContainedBy(..) => Difference::NotContainedBy,
                    _ => Difference::NonOverlapping,
                };
                let b_here = b.on_doc(&present);
                Box::new(RelativeIt {
                    a: a.iterator(doc),
                    b: if b_here { Some(b.iterator(doc)) } else { None },
                    bpos: false,
                    kind,
                })
            }
        };
        it.reset();
        it
    }
}

/// Lucene's `splitDisjunctions`: the one-word alternatives stay together as
/// one disjunction, the longer ones stand apart.
fn split(source: Source) -> Vec<Source> {
    let (mut singles, mut others) = (Vec::new(), Vec::new());
    for one in source.pull_up() {
        if one.min_extent() == 1 { singles.push(one) } else { others.push(one) }
    }
    let mut out = Vec::new();
    if !singles.is_empty() {
        out.push(Source::or(singles));
    }
    out.extend(others);
    out
}

fn pull_up_list(sources: Vec<Source>, build: &dyn Fn(Vec<Source>) -> Source) -> Vec<Source> {
    let mut rewritten: Vec<Vec<Source>> = vec![Vec::new()];
    for source in sources {
        let disjuncts = split(source);
        if disjuncts.len() == 1 {
            rewritten.iter_mut().for_each(|l| l.push(disjuncts[0].clone()));
        } else {
            let mut added = Vec::new();
            for disjunct in &disjuncts {
                for prefix in &rewritten {
                    let mut l = prefix.clone();
                    l.push(disjunct.clone());
                    added.push(l);
                }
            }
            rewritten = added;
        }
    }
    rewritten.into_iter().map(build).collect()
}

fn pull_up_one(source: Source, build: &dyn Fn(Source) -> Source) -> Vec<Source> {
    split(source).into_iter().map(build).collect()
}

/// Where each word of the query stands in the document being walked.
struct DocPlaces<'a> {
    terms: &'a [Term],
    places: &'a [Vec<i32>],
}

impl<'a> DocPlaces<'a> {
    fn of(&self, term: &Term) -> Option<&'a [i32]> {
        self.terms.iter().position(|t| t == term).map(|i| self.places[i].as_slice())
    }
}

/// Lucene's `IntervalIterator`, over one document.
trait Intervals {
    fn reset(&mut self) {}
    fn next_interval(&mut self) -> i32;
    fn start(&self) -> i32;
    fn end(&self) -> i32;
    fn gaps(&self) -> i32;
    fn width(&self) -> i32 {
        self.end().wrapping_sub(self.start()).wrapping_add(1)
    }
}

struct TermIt<'a> {
    places: &'a [i32],
    next: usize,
    upto: i64,
    pos: i32,
}

impl Intervals for TermIt<'_> {
    fn reset(&mut self) {
        if self.pos != NO_MORE {
            self.upto = self.places.len() as i64;
            self.pos = -1;
        }
    }
    fn next_interval(&mut self) -> i32 {
        if self.upto <= 0 {
            self.pos = NO_MORE;
            return NO_MORE;
        }
        self.upto -= 1;
        self.pos = self.places[self.next];
        self.next += 1;
        self.pos
    }
    fn start(&self) -> i32 {
        self.pos
    }
    fn end(&self) -> i32 {
        self.pos
    }
    fn gaps(&self) -> i32 {
        0
    }
}

type Subs<'a> = Vec<Box<dyn Intervals + 'a>>;

#[derive(Clone, Copy, PartialEq)]
enum Current {
    Empty,
    Exhausted,
    At(usize),
}

struct Disjunction<'a> {
    subs: Subs<'a>,
    queue: LuceneHeap,
    current: Current,
}

impl<'a> Disjunction<'a> {
    fn new(subs: Subs<'a>) -> Disjunction<'a> {
        Disjunction { subs, queue: LuceneHeap::new(), current: Current::Empty }
    }

    fn less(subs: &Subs<'_>, a: usize, b: usize) -> bool {
        let (x, y) = (&subs[a], &subs[b]);
        x.end() < y.end() || (x.end() == y.end() && x.start() >= y.start())
    }
}

impl Intervals for Disjunction<'_> {
    fn reset(&mut self) {
        self.queue.clear();
        for i in 0..self.subs.len() {
            self.subs[i].next_interval();
            let subs = &self.subs;
            self.queue.add(i, &|a, b| Disjunction::less(subs, a, b));
        }
        self.current = Current::Empty;
    }
    fn next_interval(&mut self) -> i32 {
        if matches!(self.current, Current::Empty | Current::Exhausted) {
            if let Some(top) = self.queue.top() {
                self.current = Current::At(top);
            }
            return self.start();
        }
        let (start, end) = (self.start(), self.end());
        while let Some(top) = self.queue.top() {
            let t = &self.subs[top];
            let contains =
                start >= t.start() && start <= t.end() && end >= t.start() && end <= t.end();
            if !contains {
                break;
            }
            let subs = &self.subs;
            let popped = self.queue.pop(&|a, b| Disjunction::less(subs, a, b)).unwrap_or(top);
            if self.subs[popped].next_interval() != NO_MORE {
                let subs = &self.subs;
                self.queue.add(popped, &|a, b| Disjunction::less(subs, a, b));
            }
        }
        match self.queue.top() {
            None => {
                self.current = Current::Exhausted;
                NO_MORE
            }
            Some(top) => {
                self.current = Current::At(top);
                self.start()
            }
        }
    }
    fn start(&self) -> i32 {
        match self.current {
            Current::Empty => -1,
            Current::Exhausted => NO_MORE,
            Current::At(i) => self.subs[i].start(),
        }
    }
    fn end(&self) -> i32 {
        match self.current {
            Current::Empty => -1,
            Current::Exhausted => NO_MORE,
            Current::At(i) => self.subs[i].end(),
        }
    }
    fn gaps(&self) -> i32 {
        match self.current {
            Current::At(i) => self.subs[i].gaps(),
            _ => 0,
        }
    }
}

/// Every part in order, each after the one before, as short as it can be.
struct OrderedIt<'a> {
    subs: Subs<'a>,
    start: i32,
    end: i32,
    slop: i32,
    i: usize,
}

impl Intervals for OrderedIt<'_> {
    fn reset(&mut self) {
        self.subs[0].next_interval();
        self.i = 1;
        self.start = -1;
        self.end = -1;
        self.slop = -1;
    }
    fn next_interval(&mut self) -> i32 {
        self.start = NO_MORE;
        self.end = NO_MORE;
        self.slop = NO_MORE;
        let mut last_start = NO_MORE;
        let mut minimizing = false;
        let size = self.subs.len();
        let mut current = self.i;
        loop {
            let mut prev_end = self.subs[current - 1].end();
            loop {
                if prev_end >= last_start {
                    self.i = current;
                    return self.start;
                }
                if current == size {
                    break;
                }
                if minimizing && self.subs[current].start() > prev_end {
                    break;
                }
                loop {
                    if self.subs[current].end() >= last_start {
                        self.i = current;
                        return self.start;
                    }
                    let next = self.subs[current].next_interval();
                    if next == NO_MORE {
                        self.i = current;
                        return self.start;
                    }
                    if next > prev_end {
                        break;
                    }
                }
                prev_end = self.subs[current].end();
                current += 1;
            }
            let start = self.subs[0].start();
            self.start = start;
            if start == NO_MORE {
                self.i = current;
                self.end = NO_MORE;
                return NO_MORE;
            }
            let end = self.subs[size - 1].end();
            self.end = end;
            let mut slop = end.wrapping_sub(start).wrapping_add(1);
            for sub in &self.subs {
                slop = slop.wrapping_sub(sub.width());
            }
            self.slop = slop;
            current = 1;
            if self.subs[0].next_interval() == NO_MORE {
                self.i = current;
                return start;
            }
            last_start = self.subs[size - 1].start();
            minimizing = true;
        }
    }
    fn start(&self) -> i32 {
        self.start
    }
    fn end(&self) -> i32 {
        self.end
    }
    fn gaps(&self) -> i32 {
        self.slop
    }
}

/// Every part in any order, the shortest stretch holding one of each.
struct UnorderedIt<'a> {
    subs: Subs<'a>,
    queue: LuceneHeap,
    start: i32,
    end: i32,
    slop: i32,
    queue_end: i32,
}

impl UnorderedIt<'_> {
    fn less(subs: &Subs<'_>, a: usize, b: usize) -> bool {
        let (x, y) = (&subs[a], &subs[b]);
        x.start() < y.start() || (x.start() == y.start() && x.end() >= y.end())
    }

    fn update_right(&mut self, i: usize) {
        self.queue_end = self.queue_end.max(self.subs[i].end());
    }

    fn pop(&mut self) -> Option<usize> {
        let subs = &self.subs;
        self.queue.pop(&|a, b| UnorderedIt::less(subs, a, b))
    }

    fn add(&mut self, i: usize) {
        let subs = &self.subs;
        self.queue.add(i, &|a, b| UnorderedIt::less(subs, a, b));
    }

    fn top(&self) -> Option<usize> {
        self.queue.top()
    }
}

impl Intervals for UnorderedIt<'_> {
    fn reset(&mut self) {
        self.queue_end = -1;
        self.start = -1;
        self.end = -1;
        self.queue.clear();
        for i in 0..self.subs.len() {
            if self.subs[i].next_interval() == NO_MORE {
                break;
            }
            self.add(i);
            self.update_right(i);
        }
    }
    fn next_interval(&mut self) -> i32 {
        let n = self.subs.len();
        while self.queue.len() == n && self.top().map(|t| self.subs[t].start()) == Some(self.start)
        {
            if let Some(it) = self.pop()
                && self.subs[it].next_interval() != NO_MORE
            {
                self.add(it);
                self.update_right(it);
            }
        }
        if self.queue.len() < n {
            self.start = NO_MORE;
            self.end = NO_MORE;
            return NO_MORE;
        }
        loop {
            let top = self.top().unwrap_or(0);
            self.start = self.subs[top].start();
            self.end = self.queue_end;
            let mut slop = self.width();
            for sub in &self.subs {
                slop = slop.wrapping_sub(sub.width());
            }
            self.slop = slop;
            if self.subs[top].end() == self.end {
                return self.start;
            }
            if let Some(it) = self.pop()
                && self.subs[it].next_interval() != NO_MORE
            {
                self.add(it);
                self.update_right(it);
            }
            if !(self.queue.len() == n && self.end == self.queue_end) {
                break;
            }
        }
        self.start
    }
    fn start(&self) -> i32 {
        self.start
    }
    fn end(&self) -> i32 {
        self.end
    }
    fn gaps(&self) -> i32 {
        self.slop
    }
}

/// Every part immediately after the one before: a phrase.
struct BlockIt<'a> {
    subs: Subs<'a>,
    start: i32,
    end: i32,
}

impl Intervals for BlockIt<'_> {
    fn reset(&mut self) {
        self.start = -1;
        self.end = -1;
    }
    fn next_interval(&mut self) -> i32 {
        let exhausted = |me: &mut BlockIt<'_>| {
            me.start = NO_MORE;
            me.end = NO_MORE;
            NO_MORE
        };
        if self.subs[0].next_interval() == NO_MORE {
            return exhausted(self);
        }
        let mut i = 1;
        while i < self.subs.len() {
            while self.subs[i].start() <= self.subs[i - 1].end() {
                if self.subs[i].next_interval() == NO_MORE {
                    return exhausted(self);
                }
            }
            if self.subs[i].start() == self.subs[i - 1].end().wrapping_add(1) {
                i += 1;
            } else {
                if self.subs[0].next_interval() == NO_MORE {
                    return exhausted(self);
                }
                i = 1;
            }
        }
        self.start = self.subs[0].start();
        self.end = self.subs[self.subs.len() - 1].end();
        self.start
    }
    fn start(&self) -> i32 {
        self.start
    }
    fn end(&self) -> i32 {
        self.end
    }
    fn gaps(&self) -> i32 {
        0
    }
}

/// One source standing several times in a row: each window of that many of
/// its intervals.
struct RepeatIt<'a> {
    inner: Box<dyn Intervals + 'a>,
    cache: Vec<i32>,
    length: usize,
    base: usize,
    started: bool,
    exhausted: bool,
}

impl RepeatIt<'_> {
    fn cache_next(&mut self, slot: usize) -> i32 {
        if self.inner.next_interval() == NO_MORE {
            self.exhausted = true;
            return NO_MORE;
        }
        self.cache[slot * 2] = self.inner.start();
        self.cache[slot * 2 + 1] = self.inner.end();
        self.start()
    }
}

impl Intervals for RepeatIt<'_> {
    fn reset(&mut self) {
        self.started = false;
        self.exhausted = false;
        self.cache.iter_mut().for_each(|c| *c = -1);
    }
    fn next_interval(&mut self) -> i32 {
        if self.exhausted {
            return NO_MORE;
        }
        if !self.started {
            for slot in 0..self.length {
                if self.cache_next(slot) == NO_MORE {
                    return NO_MORE;
                }
            }
            self.base = 0;
            self.started = true;
            return self.start();
        }
        let insert = (self.base + self.length) % self.length;
        self.base = (self.base + 1) % self.length;
        self.cache_next(insert)
    }
    fn start(&self) -> i32 {
        if self.exhausted { NO_MORE } else { self.cache[(self.base % self.length) * 2] }
    }
    fn end(&self) -> i32 {
        if self.exhausted {
            NO_MORE
        } else {
            self.cache[((self.base + self.length - 1) % self.length) * 2 + 1]
        }
    }
    // Lucene adds `start - end + 1` for each cached interval here, the other
    // way round from a width; for words, which stand one place each, it is
    // one either way, and it is kept as Lucene has it
    fn width(&self) -> i32 {
        let mut width = 0i32;
        for i in 0..self.length {
            let at = (self.base + i) % self.length;
            width = width.wrapping_add(
                self.cache[at * 2].wrapping_sub(self.cache[at * 2 + 1]).wrapping_add(1),
            );
        }
        width
    }
    fn gaps(&self) -> i32 {
        self.end().wrapping_sub(self.start()).wrapping_add(1).wrapping_sub(self.width())
    }
}

enum Accept {
    MaxGaps(i32),
    Script(IntervalScript),
}

/// The intervals of a source that pass a test.
struct FilterIt<'a> {
    inner: Box<dyn Intervals + 'a>,
    accept: Accept,
}

impl FilterIt<'_> {
    fn accepts(&self) -> bool {
        match &self.accept {
            Accept::MaxGaps(most) => self.inner.gaps() <= *most,
            Accept::Script(script) => {
                use crate::painless::Value as P;
                let interval = P::map(vec![
                    (P::str("start"), P::Int(self.inner.start() as i64)),
                    (P::str("end"), P::Int(self.inner.end() as i64)),
                    (P::str("gaps"), P::Int(self.inner.gaps() as i64)),
                ]);
                let mut runner = crate::painless::contexts::Runner::new(&script.0.params);
                runner.interval = Some(interval);
                runner.run(&script.0.script).ok().and_then(|v| v.truthy()).unwrap_or(false)
            }
        }
    }
}

impl Intervals for FilterIt<'_> {
    fn next_interval(&mut self) -> i32 {
        loop {
            let next = self.inner.next_interval();
            if next == NO_MORE || self.accepts() {
                return next;
            }
        }
    }
    fn start(&self) -> i32 {
        self.inner.start()
    }
    fn end(&self) -> i32 {
        self.inner.end()
    }
    fn gaps(&self) -> i32 {
        self.inner.gaps()
    }
}

/// A source's intervals, widened by so many positions either side.
struct ExtendIt<'a> {
    inner: Box<dyn Intervals + 'a>,
    before: i32,
    after: i32,
    positioned: bool,
}

impl Intervals for ExtendIt<'_> {
    fn reset(&mut self) {
        self.positioned = false;
    }
    fn next_interval(&mut self) -> i32 {
        self.positioned = true;
        self.inner.next_interval();
        self.start()
    }
    fn start(&self) -> i32 {
        if !self.positioned {
            return -1;
        }
        match self.inner.start() {
            NO_MORE => NO_MORE,
            start => start.wrapping_sub(self.before).max(0),
        }
    }
    fn end(&self) -> i32 {
        if !self.positioned {
            return -1;
        }
        match self.inner.end() {
            NO_MORE => NO_MORE,
            end => {
                let end = end.wrapping_add(self.after);
                if end < 0 || end == NO_MORE { NO_MORE - 1 } else { end }
            }
        }
    }
    fn gaps(&self) -> i32 {
        self.inner.gaps()
    }
}

/// The one position just before a source's interval, or just after it.
struct OffsetIt<'a> {
    inner: Box<dyn Intervals + 'a>,
    before: bool,
}

impl Intervals for OffsetIt<'_> {
    fn next_interval(&mut self) -> i32 {
        self.inner.next_interval();
        self.start()
    }
    fn start(&self) -> i32 {
        if self.before {
            match self.inner.start() {
                -1 => -1,
                NO_MORE => NO_MORE,
                pos => (pos - 1).max(0),
            }
        } else {
            let pos = self.inner.end().wrapping_add(1);
            if pos == 0 {
                -1
            } else if pos < 0 {
                NO_MORE
            } else if pos == i32::MAX {
                i32::MAX - 1
            } else {
                pos
            }
        }
    }
    fn end(&self) -> i32 {
        self.start()
    }
    fn gaps(&self) -> i32 {
        0
    }
}

#[derive(Clone, Copy)]
enum Relation {
    Containing,
    ContainedBy,
    Overlapping,
}

/// `containing`, `contained_by` and `overlapping`: the intervals of `a` that
/// stand in that relation to one of `b`.
struct FilteringIt<'a> {
    a: Box<dyn Intervals + 'a>,
    b: Box<dyn Intervals + 'a>,
    bpos: bool,
    kind: Relation,
}

impl Intervals for FilteringIt<'_> {
    fn reset(&mut self) {
        self.bpos = self.b.next_interval() != NO_MORE;
    }
    fn next_interval(&mut self) -> i32 {
        if !self.bpos {
            return NO_MORE;
        }
        while self.a.next_interval() != NO_MORE {
            match self.kind {
                Relation::Containing => {
                    while self.b.start() < self.a.start() && self.b.end() < self.a.end() {
                        if self.b.next_interval() == NO_MORE {
                            self.bpos = false;
                            return NO_MORE;
                        }
                    }
                    if self.a.start() <= self.b.start() && self.a.end() >= self.b.end() {
                        return self.a.start();
                    }
                }
                Relation::ContainedBy => {
                    while self.b.end() < self.a.end() {
                        if self.b.next_interval() == NO_MORE {
                            self.bpos = false;
                            return NO_MORE;
                        }
                    }
                    if self.b.start() <= self.a.start() {
                        return self.a.start();
                    }
                }
                Relation::Overlapping => {
                    while self.b.end() < self.a.start() {
                        if self.b.next_interval() == NO_MORE {
                            self.bpos = false;
                            return NO_MORE;
                        }
                    }
                    if self.b.start() <= self.a.end() {
                        return self.a.start();
                    }
                }
            }
        }
        if !matches!(self.kind, Relation::Containing) {
            self.bpos = false;
        }
        NO_MORE
    }
    fn start(&self) -> i32 {
        if self.bpos { self.a.start() } else { NO_MORE }
    }
    fn end(&self) -> i32 {
        if self.bpos { self.a.end() } else { NO_MORE }
    }
    fn gaps(&self) -> i32 {
        self.a.gaps()
    }
}

#[derive(Clone, Copy)]
enum Difference {
    NotContaining,
    NotContainedBy,
    NonOverlapping,
}

/// `not_containing`, `not_contained_by` and `not_overlapping`: the intervals
/// of `a` that no interval of `b` spoils.
struct RelativeIt<'a> {
    a: Box<dyn Intervals + 'a>,
    b: Option<Box<dyn Intervals + 'a>>,
    bpos: bool,
    kind: Difference,
}

impl Intervals for RelativeIt<'_> {
    fn reset(&mut self) {
        self.bpos = self.b.is_some();
    }
    fn next_interval(&mut self) -> i32 {
        let Some(b) = self.b.as_mut().filter(|_| self.bpos) else {
            return self.a.next_interval();
        };
        let a = &mut self.a;
        while a.next_interval() != NO_MORE {
            match self.kind {
                Difference::NotContaining => {
                    while b.start() < a.start() && b.end() < a.end() {
                        if b.next_interval() == NO_MORE {
                            self.bpos = false;
                            return a.start();
                        }
                    }
                    if b.start() > a.end() {
                        return a.start();
                    }
                }
                Difference::NotContainedBy => {
                    while b.end() < a.end() {
                        if b.next_interval() == NO_MORE {
                            return a.start();
                        }
                    }
                    if a.start() < b.start() {
                        return a.start();
                    }
                }
                Difference::NonOverlapping => {
                    while b.end() < a.start() {
                        if b.next_interval() == NO_MORE {
                            self.bpos = false;
                            return a.start();
                        }
                    }
                    if b.start() > a.end() {
                        return a.start();
                    }
                }
            }
        }
        NO_MORE
    }
    fn start(&self) -> i32 {
        self.a.start()
    }
    fn end(&self) -> i32 {
        self.a.end()
    }
    fn gaps(&self) -> i32 {
        self.a.gaps()
    }
}

/// An `intervals` query: its rule built into a source, scored as Lucene's
/// `IntervalQuery` with a saturation pivot of one.
#[derive(Clone, Debug)]
pub(crate) struct IntervalQuery {
    source: Source,
}

impl Query for IntervalQuery {
    fn weight(&self, _scoring: EnableScoring<'_>) -> velocore::Result<Box<dyn Weight>> {
        let mut terms = Vec::new();
        self.source.terms(&mut terms);
        Ok(Box::new(IntervalWeight {
            min_extent: self.source.min_extent(),
            source: self.source.clone(),
            terms,
        }))
    }
}

struct IntervalWeight {
    source: Source,
    terms: Vec<Term>,
    min_extent: i32,
}

impl IntervalWeight {
    fn frequencies(
        &self,
        reader: &velocore::SegmentReader,
        only: Option<velocore::DocId>,
    ) -> velocore::Result<Vec<(velocore::DocId, f32)>> {
        let mut lists = Vec::with_capacity(self.terms.len());
        for term in &self.terms {
            lists.push(docs_of(reader, term)?);
        }
        let lookup = |t: &Term| {
            self.terms.iter().position(|x| x == t).map(|i| lists[i].clone()).unwrap_or_default()
        };
        let mut candidates = self.source.candidates(&lookup);
        if let Some(doc) = only {
            candidates.retain(|d| *d == doc);
        }
        let mut positions = SegmentPositions::open(reader, &self.terms)?;
        let mut places = Vec::new();
        let mut out = Vec::new();
        for doc in candidates {
            positions.read(doc, &mut places);
            let here = DocPlaces { terms: &self.terms, places: &places };
            let mut it = self.source.iterator(&here);
            if it.next_interval() == NO_MORE {
                continue;
            }
            let mut freq = 0f32;
            loop {
                let length = it.end().wrapping_sub(it.start()).wrapping_add(1);
                let counted = length.wrapping_sub(self.min_extent).wrapping_add(1).max(1);
                freq = (freq as f64 + 1.0 / counted as f64) as f32;
                if it.next_interval() == NO_MORE {
                    break;
                }
            }
            out.push((doc, freq));
        }
        Ok(out)
    }
}

/// `w * (1 - pivot / (pivot + freq))` with a pivot of one.
fn saturation(boost: f32, freq: f32) -> f32 {
    boost * (1.0 - 1.0 / (1.0 + freq))
}

impl Weight for IntervalWeight {
    fn scorer(
        &self,
        reader: &velocore::SegmentReader,
        boost: velocore::Score,
    ) -> velocore::Result<Box<dyn velocore::query::Scorer>> {
        let scored = self
            .frequencies(reader, None)?
            .into_iter()
            .map(|(doc, freq)| (doc, saturation(boost, freq)))
            .collect();
        Ok(Box::new(ScoredDocs::new(scored)))
    }

    fn explain(
        &self,
        reader: &velocore::SegmentReader,
        doc: velocore::DocId,
    ) -> velocore::Result<velocore::query::Explanation> {
        let Some((_, freq)) = self.frequencies(reader, Some(doc))?.into_iter().next() else {
            return Err(velocore::TantivyError::InvalidArgument(
                "document does not match the intervals".to_string(),
            ));
        };
        let mut explanation = velocore::query::Explanation::new(
            "Saturation function on interval frequency, computed as w * S / (S + k) from:",
            saturation(1.0, freq),
        );
        explanation.add_const("w, weight of this function", 1.0);
        explanation.add_const(
            "k, pivot feature value that would give a score contribution equal to w/2",
            1.0,
        );
        explanation.add_const("S, feature value", freq);
        Ok(explanation)
    }
}

/// How the parts of a rule are put together.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Ordered,
    Unordered,
    UnorderedNoOverlap,
}

fn mode_of(spec: &Value) -> Result<Mode> {
    if let Some(ordered) = spec.get("ordered").and_then(|v| v.as_bool()) {
        return Ok(if ordered { Mode::Ordered } else { Mode::Unordered });
    }
    match spec.get("mode").and_then(|v| v.as_str()).map(|m| m.to_ascii_lowercase()) {
        None => Ok(Mode::Unordered),
        Some(m) if m == "ordered" => Ok(Mode::Ordered),
        Some(m) if m == "unordered" => Ok(Mode::Unordered),
        Some(m) if m == "unordered_no_overlap" => Ok(Mode::UnorderedNoOverlap),
        Some(m) => Err(anyhow!("no mode can be parsed from ordinal {m}")),
    }
}

/// OpenSearch's `IntervalBuilder.combineSources`.
fn combine(mut sources: Vec<Source>, max_gaps: i32, mode: Mode) -> Source {
    match sources.len() {
        0 => return Source::Nothing,
        1 => return sources.remove(0),
        _ => {}
    }
    if max_gaps == 0 && mode == Mode::Ordered {
        return Source::phrase(sources);
    }
    let inner = match mode {
        Mode::Ordered => Source::ordered(sources),
        Mode::Unordered => Source::unordered(sources),
        Mode::UnorderedNoOverlap => {
            let no_overlap = |a: Source, b: Source| {
                Source::or(vec![
                    Source::ordered(vec![a.clone(), b.clone()]),
                    Source::ordered(vec![b, a]),
                ])
            };
            let rest = sources.split_off(2);
            let second = sources.remove(1);
            let mut inner = no_overlap(sources.remove(0), second);
            for next in rest {
                let held = if max_gaps == -1 { inner } else { Source::max_gaps(inner, max_gaps) };
                inner = no_overlap(held, next);
            }
            inner
        }
    };
    if max_gaps == -1 { inner } else { Source::max_gaps(inner, max_gaps) }
}

fn term_of(ctx: &Ctx, field: &str, word: &str) -> Term {
    let (f, path, _) = ctx.resolve(field, true);
    let mut term = Term::from_field_json_path(f, &path, true);
    term.append_type_and_str(word);
    term
}

/// A text as a source: its words by the field's analyzer, a gap left where
/// a word was dropped, and words standing in one place as alternatives.
fn analyzed(
    ctx: &Ctx,
    field: &str,
    text: &str,
    max_gaps: i32,
    mode: Mode,
    analyzer: Option<&str>,
) -> Source {
    let (_, _, view) = ctx.resolve(field, true);
    let mut edges = analyze_graph(ctx, view, field, text, analyzer);
    edges.sort_by_key(|e| e.from);
    match edges.len() {
        0 => return Source::Nothing,
        1 => return Source::Term(term_of(ctx, field, &edges[0].text)),
        _ => {}
    }
    let increments: Vec<i64> = edges
        .iter()
        .enumerate()
        .map(|(i, e)| match i {
            0 => e.from as i64 + 1,
            _ => e.from as i64 - edges[i - 1].from as i64,
        })
        .collect();
    let extend = |source: Source, spaces: i64| match spaces {
        0 => source,
        n => Source::Extend(Box::new(source), n as i32, 0),
    };
    if !increments.contains(&0) {
        let terms = edges
            .iter()
            .zip(&increments)
            .map(|(e, inc)| extend(Source::Term(term_of(ctx, field, &e.text)), inc - 1))
            .collect();
        return combine(terms, max_gaps, mode);
    }
    let mut terms = Vec::new();
    let mut synonyms: Vec<Source> = Vec::new();
    let mut spaces = 0i64;
    for (e, inc) in edges.iter().zip(&increments) {
        if *inc > 0 {
            match synonyms.len() {
                0 => {}
                1 => terms.push(extend(synonyms.remove(0), spaces)),
                _ => terms.push(extend(Source::or(std::mem::take(&mut synonyms)), spaces)),
            }
            synonyms.clear();
            spaces = inc - 1;
        }
        synonyms.push(Source::Term(term_of(ctx, field, &e.text)));
    }
    match synonyms.len() {
        1 => terms.push(extend(synonyms.remove(0), spaces)),
        _ => terms.push(extend(Source::or(synonyms), spaces)),
    }
    combine(terms, max_gaps, mode)
}

/// The words of a field's dictionary a multi-term rule stands for, refused
/// past `max_expansions` as Lucene refuses them.
fn expanded(
    ctx: &Ctx,
    field: &str,
    prefix: &str,
    accept: &dyn Fn(&str) -> bool,
    most: usize,
    pattern: &str,
) -> Result<Source> {
    let (f, path, _) = ctx.resolve(field, true);
    let words = crate::query::dictionary_words(ctx, f, &path, prefix, accept, most)
        .map_err(|_| anyhow!("Automaton [{pattern}] expanded to too many terms (limit {most})"))?;
    Ok(Source::Multi(words))
}

/// One rule of an `intervals` query, as the source Lucene builds for it.
pub(crate) fn rule_source(ctx: &Ctx, field: &str, rule: &Value) -> Result<Source> {
    let Some((kind, spec)) = rule.as_object().and_then(|o| o.iter().next()) else {
        return Err(anyhow!("Missing intervals from interval query definition"));
    };
    let use_field = spec.get("use_field").and_then(|v| v.as_str());
    let field = use_field.unwrap_or(field);
    let analyzer = spec.get("analyzer").and_then(|v| v.as_str());
    let max_gaps = spec.get("max_gaps").and_then(|v| v.as_i64()).unwrap_or(-1) as i32;
    let max_expansions =
        spec.get("max_expansions").and_then(|v| v.as_i64()).filter(|n| *n > 0).unwrap_or(128)
            as usize;
    // a prefix or pattern is normalised the way the field's analyzer
    // normalises a word, which for the analyzers here is lower case
    let normal = |s: &str| s.to_lowercase();
    let source = match kind.as_str() {
        "match" => {
            let text = spec
                .get("query")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("[match] failed to parse field [query]"))?;
            analyzed(ctx, field, text, max_gaps, mode_of(spec)?, analyzer)
        }
        "any_of" | "all_of" => {
            let mut sources = Vec::new();
            for sub in spec.get("intervals").and_then(|v| v.as_array()).into_iter().flatten() {
                sources.push(rule_source(ctx, field, sub)?);
            }
            match kind.as_str() {
                "any_of" => Source::or(sources),
                _ => combine(sources, max_gaps, mode_of(spec)?),
            }
        }
        "prefix" => {
            let prefix = normal(spec.get("prefix").and_then(|v| v.as_str()).unwrap_or_default());
            expanded(ctx, field, &prefix, &|_| true, 128, &format!("{prefix}*"))?
        }
        "wildcard" => {
            let pattern = normal(spec.get("pattern").and_then(|v| v.as_str()).unwrap_or_default());
            let re = regex::Regex::new(&format!("^{}$", wildcard_to_regex(&pattern)))
                .map_err(|e| anyhow!("bad wildcard `{pattern}`: {e}"))?;
            expanded(ctx, field, "", &|w| re.is_match(w), max_expansions, &pattern)?
        }
        "regexp" => {
            let pattern = spec.get("pattern").and_then(|v| v.as_str()).unwrap_or_default();
            let head = if is_true(spec.get("case_insensitive")) { "(?i)" } else { "" };
            let re = regex::Regex::new(&format!("{head}^(?:{pattern})$"))
                .map_err(|e| anyhow!("bad regex `{pattern}`: {e}"))?;
            expanded(ctx, field, "", &|w| re.is_match(w), max_expansions, pattern)?
        }
        "fuzzy" => {
            let raw = spec.get("term").and_then(|v| v.as_str()).unwrap_or_default();
            let word = normal(raw);
            let auto = Value::String("AUTO".into());
            let edits =
                fuzzy_edits(Some(spec.get("fuzziness").unwrap_or(&auto)), raw).unwrap_or(0).min(2);
            let transpositions =
                spec.get("transpositions").and_then(|v| v.as_bool()).unwrap_or(true);
            let prefix = spec.get("prefix_length").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let searcher = ctx.index.reader()?.searcher();
            let words = crate::query::ScoredFuzzy::new(
                term_of(ctx, field, &word),
                &word,
                edits,
                transpositions,
            )
            .prefix_length(prefix)
            .max_expansions(129)
            .words(&searcher)?;
            if words.len() > 128 {
                return Err(anyhow!("Automaton [{raw}] expanded to too many terms (limit 128)"));
            }
            let mut words = words;
            words.sort_by(|a, b| a.serialized_value_bytes().cmp(b.serialized_value_bytes()));
            Source::Multi(words)
        }
        other => {
            return Err(anyhow!(
                "Unknown interval type [{other}], expecting one of [match, any_of, all_of, prefix, wildcard, regexp]"
            ));
        }
    };
    match (kind.as_str(), spec.get("filter")) {
        ("match" | "any_of" | "all_of", Some(filter)) => apply_filter(ctx, field, source, filter),
        _ => Ok(source),
    }
}

fn apply_filter(ctx: &Ctx, field: &str, source: Source, filter: &Value) -> Result<Source> {
    let Some((kind, inner)) = filter.as_object().and_then(|o| o.iter().next()) else {
        return Err(anyhow!("Expected [FIELD_NAME] but got [END_OBJECT]"));
    };
    if kind == "script" {
        let compiled = crate::painless::contexts::Compiled::of(inner, &|_| None)
            .map_err(|e| anyhow!("{e:?}"))?;
        return Ok(Source::Script(Box::new(source), IntervalScript(Arc::new(compiled))));
    }
    let reference = rule_source(ctx, field, inner)?;
    Ok(match kind.to_ascii_lowercase().as_str() {
        "containing" => Source::containing(source, reference),
        "contained_by" => Source::contained_by(source, reference),
        "not_containing" => Source::not_containing(source, reference),
        "not_contained_by" => Source::not_contained_by(source, reference),
        "overlapping" => Source::Overlapping(Box::new(source), Box::new(reference)),
        "not_overlapping" => Source::NonOverlapping(Box::new(source), Box::new(reference)),
        "before" => Source::before(source, reference),
        "after" => Source::after(source, reference),
        other => return Err(anyhow!("Unknown filter type [{other}]")),
    })
}

/// The `intervals` query.
pub(crate) fn build_intervals(ctx: &Ctx, body: &Value) -> Result<Box<dyn Query>> {
    let Some((field, spec)) = body.as_object().and_then(|o| o.iter().next()) else {
        return Err(anyhow!("[intervals] requires a field"));
    };
    let rules: Vec<(&String, &Value)> = spec
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| *k != "boost" && *k != "_name")
        .collect();
    if rules.len() > 1 {
        return Err(anyhow!(
            "Only one interval rule can be specified, found [{}] and [{}]",
            rules[0].0,
            rules[1].0
        ));
    }
    let Some((kind, rule)) = rules.first() else {
        return Err(anyhow!("Missing intervals from interval query definition"));
    };
    // where the words stand is not kept for a field that keeps only that
    // they are there
    if ctx.mapping.type_of(field) == Some("match_only_text") {
        return Err(anyhow!(
            "Cannot create intervals over field [{field}] with no positions indexed"
        ));
    }
    let rule = serde_json::json!({ (*kind).clone(): (*rule).clone() });
    Ok(Box::new(IntervalQuery { source: rule_source(ctx, field, &rule)? }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(word: &str) -> Term {
        Term::from_field_text(Field::from_field_id(0), word)
    }

    /// Every interval of a source in one document.
    fn intervals(source: &Source, doc: &[(&str, &[i32])]) -> Vec<(i32, i32)> {
        let terms: Vec<Term> = doc.iter().map(|(w, _)| term(w)).collect();
        let places: Vec<Vec<i32>> = doc.iter().map(|(_, p)| p.to_vec()).collect();
        let here = DocPlaces { terms: &terms, places: &places };
        let mut it = source.iterator(&here);
        let mut out = Vec::new();
        while it.next_interval() != NO_MORE {
            out.push((it.start(), it.end()));
        }
        out
    }

    #[test]
    fn a_word_twice_in_order_is_one_word_repeated() {
        let the = Source::Term(term("the"));
        let ordered = Source::ordered(vec![the.clone(), the]);
        assert!(matches!(ordered, Source::Repeating(_, 2)));
        // "the cat and the dog and the cat"
        assert_eq!(intervals(&ordered, &[("the", &[0, 3, 6])]), vec![(0, 3), (3, 6)]);
        assert_eq!(ordered.min_extent(), 1);
    }

    #[test]
    fn ordered_intervals_are_the_shortest() {
        let rule = Source::ordered(vec![Source::Term(term("a")), Source::Term(term("b"))]);
        // "a a b a b"
        assert_eq!(intervals(&rule, &[("a", &[0, 1, 3]), ("b", &[2, 4])]), vec![(1, 2), (3, 4)]);
    }

    #[test]
    fn max_gaps_is_pushed_below_a_disjunction() {
        let either = Source::or(vec![
            Source::ordered(vec![Source::Term(term("a")), Source::Term(term("b"))]),
            Source::ordered(vec![Source::Term(term("b")), Source::Term(term("a"))]),
        ]);
        match Source::max_gaps(either, 0) {
            Source::Or(subs, true) => {
                assert!(subs.iter().all(|s| matches!(s, Source::MaxGaps(..))))
            }
            other => panic!("expected an or of filtered sources, got {other:?}"),
        }
    }
}
