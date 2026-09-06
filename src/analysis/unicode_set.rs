//! `[^ßB]` -- which characters a filter is allowed to touch.
//!
//! ICU's normalizers take a set saying what they may change, so that a text
//! can be normalized without losing the one letter it is about: German ß
//! becomes `ss` under normalization, which is right for search and wrong for
//! a corpus of words about ß. `unicode_set_filter` is how that exception is
//! named, and it is a UnicodeSet -- a pattern language of its own, of which
//! this reads the part the setting is ever written in: a list of characters
//! and ranges, optionally negated.

/// The characters a filter may change.
#[derive(Clone, Debug)]
pub struct UnicodeSet {
    /// pairs of first and last, inclusive
    ranges: Vec<(char, char)>,
    negated: bool,
}

impl UnicodeSet {
    /// Read a pattern, or nothing if it is not one this understands -- in
    /// which case the filter touches everything, which is what it does with
    /// no set at all. Refusing to run would be worse: the pattern is an
    /// exception to a filter, and the filter is the point.
    pub fn parse(pattern: &str) -> Option<UnicodeSet> {
        let inner = pattern.trim().strip_prefix('[')?.strip_suffix(']')?;
        let (negated, inner) = match inner.strip_prefix('^') {
            Some(rest) => (true, rest),
            None => (false, inner),
        };
        let letters: Vec<char> = inner.chars().collect();
        let mut ranges = Vec::new();
        let mut at = 0;
        while at < letters.len() {
            let c = match letters[at] {
                // a backslash spells the character after it literally
                '\\' => {
                    at += 1;
                    *letters.get(at)?
                }
                // the parts of the language this does not read
                '[' | '&' | '{' | '$' | ':' => return None,
                other => other,
            };
            // `a-z` is every character between the two
            if letters.get(at + 1) == Some(&'-') && at + 2 < letters.len() {
                ranges.push((c, letters[at + 2]));
                at += 3;
                continue;
            }
            ranges.push((c, c));
            at += 1;
        }
        Some(UnicodeSet { ranges, negated })
    }

    /// Whether a filter may change this character.
    pub fn holds(&self, c: char) -> bool {
        let within = self.ranges.iter().any(|(first, last)| *first <= c && c <= *last);
        within != self.negated
    }
}

/// Run a filter over only the characters a set allows it to.
///
/// The filter is given each allowed character on its own rather than the
/// whole word, because a normalizer that saw the word would be free to change
/// a character the set protects -- and a run of allowed characters handed
/// over together would let it join one to a protected neighbour.
pub fn within(text: &str, set: &UnicodeSet, filter: impl Fn(&str) -> String) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match set.holds(c) {
            true => out.push_str(&filter(&c.to_string())),
            false => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_negated_set_holds_everything_it_does_not_name() {
        let set = UnicodeSet::parse("[^ßB]").expect("a set");
        assert!(set.holds('a'));
        assert!(!set.holds('ß'));
        assert!(!set.holds('B'));
    }

    #[test]
    fn a_plain_set_holds_only_what_it_names() {
        let set = UnicodeSet::parse("[abc]").expect("a set");
        assert!(set.holds('b'));
        assert!(!set.holds('d'));
    }

    #[test]
    fn a_range_holds_what_lies_between() {
        let set = UnicodeSet::parse("[a-f]").expect("a set");
        assert!(set.holds('c'));
        assert!(!set.holds('g'));
    }

    #[test]
    fn a_pattern_this_does_not_read_is_no_set_at_all() {
        assert!(UnicodeSet::parse("[:Latin:]").is_none());
        assert!(UnicodeSet::parse("[[a-z]&[^aeiou]]").is_none());
        assert!(UnicodeSet::parse("nonsense").is_none());
    }

    #[test]
    fn a_protected_letter_is_left_where_it_stands() {
        let set = UnicodeSet::parse("[^ß]").expect("a set");
        assert_eq!(within("Ruß", &set, |c| c.to_lowercase()), "ruß");
    }
}
