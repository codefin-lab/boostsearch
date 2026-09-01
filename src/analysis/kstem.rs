//! English, cut the way `KStemmer` cuts it.
//!
//! KStem is gentler than Porter: it asks a dictionary of English words at
//! every step, and stops as soon as what it has is a word. `dogs` is a word,
//! so it stays `dogs`; `spaced` is not, and becomes `space`. The dictionary
//! is Lucene's, and so is the algorithm (Apache-2.0, the Apache Software
//! Foundation); the plural, the past tense and the participle are what this
//! carries of it.

use std::collections::HashSet;
use std::sync::OnceLock;

/// The English words the stemmer knows.
fn dictionary() -> &'static HashSet<&'static str> {
    static WORDS: OnceLock<HashSet<&'static str>> = OnceLock::new();
    WORDS.get_or_init(|| include_str!("kstem_words.txt").lines().collect())
}

/// A word, as the stemmer works on it.
struct Word {
    letters: Vec<char>,
}

impl Word {
    fn text(&self) -> String {
        self.letters.iter().collect()
    }

    fn known(&self) -> bool {
        dictionary().contains(self.text().as_str())
    }

    fn ends_with(&self, ending: &str) -> bool {
        let ending: Vec<char> = ending.chars().collect();
        self.letters.len() > ending.len()
            && self.letters[self.letters.len() - ending.len()..] == ending[..]
    }

    /// Whether the letter at this place is a consonant. `y` is one unless the
    /// letter before it is.
    fn is_consonant(&self, at: usize) -> bool {
        match self.letters.get(at) {
            Some('a') | Some('e') | Some('i') | Some('o') | Some('u') => false,
            Some('y') if at > 0 => !self.is_consonant(at - 1),
            Some(_) => true,
            None => false,
        }
    }

    /// Whether what is left before the ending holds a vowel: without one it
    /// is an abbreviation rather than a word, and is left alone.
    fn vowel_in_stem(&self, ending: usize) -> bool {
        (0..self.letters.len().saturating_sub(ending)).any(|i| !self.is_consonant(i))
    }

    /// Whether the word ends in the same consonant twice.
    fn doubled_end(&self) -> bool {
        let len = self.letters.len();
        len > 1 && self.letters[len - 1] == self.letters[len - 2] && self.is_consonant(len - 1)
    }

    fn truncate(&mut self, by: usize) {
        let len = self.letters.len().saturating_sub(by);
        self.letters.truncate(len);
    }

    fn push(&mut self, c: char) {
        self.letters.push(c);
    }
}

/// One English word, as KStem leaves it.
pub fn stem(word: &str) -> String {
    if word.chars().count() <= 2 || !word.chars().all(|c| c.is_ascii_alphabetic()) {
        return word.to_string();
    }
    // a word the dictionary knows is already what it should be
    if dictionary().contains(word) {
        return word.to_string();
    }
    let mut w = Word { letters: word.chars().collect() };
    // a step counts only when what it left behind is a word the dictionary
    // knows; a word none of them can account for is left as it was written
    if plural(&mut w) || past_tense(&mut w) || participle(&mut w) {
        return w.text();
    }
    word.to_string()
}

/// `calories` is `calorie`, `flies` is `fly`, `dogs` is `dog`.
fn plural(w: &mut Word) -> bool {
    if w.letters.last() != Some(&'s') {
        return false;
    }
    if w.ends_with("ies") {
        w.truncate(1);
        if w.known() {
            return true;
        }
        w.truncate(2);
        w.push('y');
        return w.known();
    }
    if w.ends_with("es") {
        let len = w.letters.len();
        // `crosses` is not `crosse`, which is why a doubled s is left alone
        let try_e = len > 2 && !(w.letters[len - 3] == 's' && w.letters[len - 4] == 's');
        w.truncate(1);
        if try_e && w.known() {
            return true;
        }
        w.truncate(1);
        if w.known() {
            return true;
        }
        w.push('e');
        return w.known();
    }
    let len = w.letters.len();
    if len > 3 && w.letters[len - 2] != 's' && !w.ends_with("ous") {
        w.truncate(1);
        return w.known();
    }
    false
}

/// `applied` is `apply`, `spaced` is `space`, `stopped` is `stop`.
fn past_tense(w: &mut Word) -> bool {
    if w.letters.len() <= 4 {
        return false;
    }
    if w.ends_with("ied") {
        w.truncate(1);
        if w.known() {
            return true;
        }
        w.truncate(2);
        w.push('y');
        return w.known();
    }
    if !w.ends_with("ed") || !w.vowel_in_stem(2) {
        return false;
    }
    w.truncate(1);
    if w.known() {
        return true;
    }
    w.truncate(1);
    if w.known() {
        return true;
    }
    if w.doubled_end() {
        let doubled = w.letters[w.letters.len() - 1];
        w.truncate(1);
        if w.known() {
            return true;
        }
        w.push(doubled);
        return w.known();
    }
    // a word that begins with `un` keeps its ending: `unwed` is not `unwe`
    if w.letters.first() == Some(&'u') && w.letters.get(1) == Some(&'n') {
        w.push('e');
        w.push('d');
        return true;
    }
    w.push('e');
    w.known()
}

/// `spacing` is `space`, `stopping` is `stop`, `singing` is `sing`.
fn participle(w: &mut Word) -> bool {
    if w.letters.len() <= 5 || !w.ends_with("ing") || !w.vowel_in_stem(3) {
        return false;
    }
    w.truncate(3);
    w.push('e');
    if w.known() {
        return true;
    }
    w.truncate(1);
    if w.known() {
        return true;
    }
    if w.doubled_end() {
        let doubled = w.letters[w.letters.len() - 1];
        w.truncate(1);
        if w.known() {
            return true;
        }
        w.push(doubled);
        return w.known();
    }
    // two consonants before the ending mean the word ends there
    let len = w.letters.len();
    if len > 1 && w.is_consonant(len - 1) && w.is_consonant(len - 2) {
        return true;
    }
    w.push('e');
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_word_the_dictionary_knows_is_left_as_it_is() {
        assert_eq!(stem("dogs"), "dogs");
        assert_eq!(stem("spaced"), "space");
        assert_eq!(stem("foxes"), "fox");
        assert_eq!(stem("jumped"), "jump");
        assert_eq!(stem("bricks"), "brick");
    }
}
