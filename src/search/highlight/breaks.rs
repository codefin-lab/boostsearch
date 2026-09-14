//! Where a text may be cut into passages: the sentence and word boundaries
//! Java's `BreakIterator` finds, and the iterators the highlighters build out
//! of them.
//!
//! Everything here counts UTF-16 units, because that is what the reference
//! counts: `fragment_size`, `no_match_size` and every offset a highlighter
//! reads are Java string indices.

/// The character standing at a unit of the text, with a surrogate read as the
/// letter it is half of.
fn char_at(text: &[u16], at: usize) -> char {
    // half of a character outside the basic plane is nearly always a letter
    // or a symbol inside a word, and never a boundary of its own
    char::from_u32(text[at] as u32).unwrap_or('a')
}

fn is_space(c: char) -> bool {
    matches!(c, '\t' | '\r' | '\u{c}' | '\n' | '\u{2028}') || (c.is_whitespace() && c != '\u{2029}')
}

/// Punctuation that closes: a bracket or a closing quote.
fn is_end(c: char) -> bool {
    matches!(c, ')' | ']' | '}' | '"' | '\'' | '\u{bb}' | '\u{2019}' | '\u{201d}')
}

fn is_term(c: char) -> bool {
    matches!(c, '!' | '?' | '\u{3002}' | '\u{ff01}' | '\u{ff1f}')
}

fn is_period(c: char) -> bool {
    matches!(c, '.' | '\u{ff0e}')
}

/// Every place a sentence ends, the start and the end of the text included.
///
/// The rules are the JDK's, as the reference showed them: `!` and `?` always
/// end a sentence, with the closing punctuation and the spaces after them; a
/// period ends one only when a space follows it and what comes after the
/// space is not a lowercase letter or a digit -- `e.g. the` is one sentence,
/// `one. Two` is two. A line break is a space like any other, and the
/// separator between the values of a field ends a sentence wherever it is.
pub(super) fn sentence_bounds(text: &[u16]) -> Vec<usize> {
    let n = text.len();
    let mut out = vec![0];
    let mut i = 0;
    while i < n {
        let c = char_at(text, i);
        if c == '\u{2029}' || c == '\0' {
            i += 1;
            out.push(i);
            continue;
        }
        if is_term(c) {
            i += 1;
            while i < n && {
                let d = char_at(text, i);
                is_term(d) || is_period(d) || is_end(d)
            } {
                i += 1;
            }
            while i < n && is_space(char_at(text, i)) {
                i += 1;
            }
            out.push(i);
            continue;
        }
        if is_period(c) {
            i += 1;
            while i < n && {
                let d = char_at(text, i);
                is_period(d) || is_end(d)
            } {
                i += 1;
            }
            let spaces_from = i;
            while i < n && is_space(char_at(text, i)) {
                i += 1;
            }
            let spaces = i - spaces_from;
            if spaces == 0 {
                continue;
            }
            let lower_next = i < n && {
                let d = char_at(text, i);
                d.is_lowercase() || d.is_numeric()
            };
            if !lower_next {
                out.push(i);
            } else if spaces >= 2 {
                // what follows the first space is a space, which is not a
                // lowercase letter
                out.push(spaces_from + 1);
            }
            continue;
        }
        i += 1;
    }
    if out.last() != Some(&n) {
        out.push(n);
    }
    out.dedup();
    out
}

fn is_letter(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_digit(c: char) -> bool {
    c.is_numeric()
}

fn is_mid_word(c: char) -> bool {
    matches!(c, '-' | '\u{ad}' | '\u{2027}' | '"' | '\'' | '.' | '\u{2010}'..='\u{2015}')
}

fn is_mid_num(c: char) -> bool {
    matches!(c, '"' | '\'' | ',' | '\u{66b}' | '.')
}

fn is_post_num(c: char) -> bool {
    matches!(c, '%' | '&' | '\u{a2}' | '\u{66a}' | '\u{2030}' | '\u{2031}')
}

/// Every place a word begins or ends, the start and the end of the text
/// included.
///
/// A word is a run of letters and digits, and it holds the punctuation that
/// stands between two of its letters -- `foo-bar`, `don't` and `U.S.A` are one
/// word each, as `3.14` and `1,000` are one number. Any other character is a
/// piece of its own, and a run of spaces is one piece.
pub(super) fn word_bounds(text: &[u16]) -> Vec<usize> {
    let n = text.len();
    let mut out = vec![0];
    let mut i = 0;
    let run = |mut i: usize, member: fn(char) -> bool, mid: fn(char) -> bool| -> usize {
        while i < n && member(char_at(text, i)) {
            i += 1;
        }
        while i + 1 < n && mid(char_at(text, i)) && member(char_at(text, i + 1)) {
            i += 1;
            while i < n && member(char_at(text, i)) {
                i += 1;
            }
        }
        i
    };
    while i < n {
        let c = char_at(text, i);
        let start = i;
        if is_letter(c) || is_digit(c) || (c == '.' && i + 1 < n && is_digit(char_at(text, i + 1)))
        {
            if !is_letter(c) && !is_digit(c) {
                i += 1;
            }
            loop {
                if i < n && is_letter(char_at(text, i)) {
                    i = run(i, is_letter, is_mid_word);
                } else if i < n && is_digit(char_at(text, i)) {
                    i = run(i, is_digit, is_mid_num);
                    if i < n && is_post_num(char_at(text, i)) {
                        i += 1;
                        break;
                    }
                } else {
                    break;
                }
            }
        } else if is_space(c) && c != '\n' && c != '\r' && c != '\u{2028}' && c != '\u{c}' {
            while i < n && {
                let d = char_at(text, i);
                is_space(d) && d != '\n' && d != '\r' && d != '\u{2028}' && d != '\u{c}'
            } {
                i += 1;
            }
            if i < n && char_at(text, i) == '\r' {
                i += 1;
            }
            if i < n && matches!(char_at(text, i), '\n' | '\u{c}' | '\u{2028}' | '\u{2029}') {
                i += 1;
            }
        } else if c == '\r' && i + 1 < n && char_at(text, i + 1) == '\n' {
            i += 2;
        } else {
            i += 1;
        }
        if i == start {
            i += 1;
        }
        out.push(i);
    }
    if out.last() != Some(&n) {
        out.push(n);
    }
    out
}

/// The boundaries a highlighter asks about, however they were found.
pub(super) struct Bounds(pub(super) Vec<usize>);

impl Bounds {
    /// The last boundary before `offset`, or the start of the text.
    pub(super) fn preceding(&self, offset: usize) -> usize {
        match self.0.partition_point(|b| *b < offset) {
            0 => 0,
            at => self.0[at - 1],
        }
    }

    /// The first boundary after `offset`, or `None` past the last one.
    pub(super) fn following(&self, offset: usize) -> Option<usize> {
        let at = self.0.partition_point(|b| *b <= offset);
        self.0.get(at).copied()
    }
}

/// The break iterator the unified highlighter cuts passages with.
pub(super) enum Passages {
    /// sentences, words, or the values of the field: a passage runs from one
    /// boundary to the next
    Plain(Bounds),
    /// sentences no longer than `max_len`, cut at words where a sentence is
    /// longer -- `BoundedBreakIteratorScanner`
    Bounded {
        main: Bounds,
        inner: Bounds,
        max_len: usize,
        window_start: Option<usize>,
        window_end: usize,
        inner_start: usize,
        inner_end: usize,
    },
}

impl Passages {
    pub(super) fn bounded(text: &[u16], max_len: usize) -> Passages {
        Passages::Bounded {
            main: Bounds(sentence_bounds(text)),
            inner: Bounds(word_bounds(text)),
            max_len,
            window_start: None,
            window_end: 0,
            inner_start: 0,
            inner_end: 0,
        }
    }

    /// Where the passage holding the match that starts at `offset - 1`
    /// begins. The unified highlighter always asks this first, and then
    /// `following` for the same match.
    pub(super) fn preceding(&mut self, offset: usize) -> usize {
        match self {
            Passages::Plain(b) => b.preceding(offset),
            Passages::Bounded {
                main,
                inner,
                max_len,
                window_start,
                window_end,
                inner_start,
                inner_end,
            } => {
                let max_len = *max_len;
                let inside = window_start.is_some_and(|s| offset > s) && offset < *window_end;
                if inside {
                    *inner_start = *inner_end;
                    *inner_end = *window_end;
                } else {
                    let start = main.preceding(offset);
                    let end = main.following(offset.saturating_sub(1)).unwrap_or(start);
                    *window_start = Some(start);
                    *inner_start = start;
                    *window_end = end;
                    *inner_end = end;
                    // take in the sentences after it while they still fit
                    while *inner_end - *inner_start < max_len {
                        match main.following(*inner_end) {
                            Some(next) if next - *inner_start <= max_len => {
                                *window_end = next;
                                *inner_end = next;
                            }
                            _ => break,
                        }
                    }
                }
                if *inner_end - *inner_start > max_len {
                    // the sentence is too long: cut it at the words, first to
                    // the left of the match and then to the right with what
                    // is left of the length
                    if offset > max_len && offset - max_len > *inner_start {
                        *inner_start = (*inner_start).max(inner.preceding(offset - max_len));
                    }
                    let remaining = max_len.saturating_sub(offset - *inner_start);
                    if offset + remaining < *window_end {
                        let cut = inner.following(offset + remaining).unwrap_or(*window_end);
                        *inner_end = (*window_end).min(cut);
                    }
                }
                *inner_start
            }
        }
    }

    /// Where that passage ends.
    pub(super) fn following(&mut self, offset: usize, len: usize) -> usize {
        match self {
            Passages::Plain(b) => b.following(offset).unwrap_or(len),
            Passages::Bounded { inner_end, .. } => *inner_end,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn sentences(s: &str) -> Vec<String> {
        let t = units(s);
        let b = sentence_bounds(&t);
        b.windows(2).map(|w| String::from_utf16_lossy(&t[w[0]..w[1]])).collect()
    }

    /// The cases the reference was asked about one by one.
    #[test]
    fn a_sentence_ends_where_the_jdk_ends_it() {
        assert_eq!(sentences("One. Two"), vec!["One. ", "Two"]);
        assert_eq!(sentences("One. two"), vec!["One. two"]);
        assert_eq!(sentences("One.  two"), vec!["One. ", " two"]);
        assert_eq!(sentences("One.Two"), vec!["One.Two"]);
        assert_eq!(sentences("One? two"), vec!["One? ", "two"]);
        assert_eq!(sentences("One!two"), vec!["One!", "two"]);
        assert_eq!(sentences("One. 42"), vec!["One. 42"]);
        assert_eq!(sentences("One. (two"), vec!["One. ", "(two"]);
        assert_eq!(sentences("One.) two"), vec!["One.) two"]);
        assert_eq!(sentences("One\nTwo"), vec!["One\nTwo"]);
        assert_eq!(sentences("One.\nTwo"), vec!["One.\n", "Two"]);
        assert_eq!(sentences("a\0b"), vec!["a\0", "b"]);
    }

    fn words(s: &str) -> Vec<String> {
        let t = units(s);
        let b = word_bounds(&t);
        b.windows(2).map(|w| String::from_utf16_lossy(&t[w[0]..w[1]])).collect()
    }

    #[test]
    fn a_word_holds_the_punctuation_between_its_letters() {
        assert_eq!(
            words("Hello, world's 3.14 foo-bar e.g. a:b"),
            vec![
                "Hello", ",", " ", "world's", " ", "3.14", " ", "foo-bar", " ", "e.g", ".", " ",
                "a", ":", "b"
            ]
        );
    }
}
