//! Stemmers written as rules rather than as code.
//!
//! Portuguese and Galician are stemmed by RSLP -- a list of steps, each a
//! list of endings with what to put in their place, the shortest stem that
//! may be left, and the words the rule does not speak for. Lucene keeps those
//! rules in a file and reads them at startup; the file is the same one, read
//! here.
//!
//! The rule files are the Apache Software Foundation's, under Apache-2.0.

use std::sync::OnceLock;

/// One ending, and what stands in its place.
struct Rule {
    suffix: String,
    /// the shortest stem the rule may leave behind
    min: usize,
    replacement: String,
    /// the words this rule does not speak for
    exceptions: Vec<String>,
}

/// One step of the walk: the endings it looks for, and the rules it applies.
struct Step {
    /// a word that ends in none of these is left to the next step
    suffixes: Vec<String>,
    min: usize,
    rules: Vec<Rule>,
}

impl Step {
    /// The word as this step leaves it, and whether a rule spoke for it.
    fn apply(&self, word: &str) -> (String, bool) {
        if word.chars().count() < self.min {
            return (word.to_string(), false);
        }
        if !self.suffixes.is_empty() && !self.suffixes.iter().any(|s| word.ends_with(s.as_str())) {
            return (word.to_string(), false);
        }
        for rule in &self.rules {
            let stem_len = word.chars().count() as isize - rule.suffix.chars().count() as isize;
            if stem_len < rule.min as isize || !word.ends_with(&rule.suffix) {
                continue;
            }
            if rule.exceptions.iter().any(|e| e == word) {
                continue;
            }
            let keep: String =
                word.chars().take(stem_len as usize).chain(rule.replacement.chars()).collect();
            return (keep, true);
        }
        (word.to_string(), false)
    }
}

/// The steps of one language's rules, in the order the file writes them.
///
/// A step is written as `{ "Name", min, flag, {suffixes}, {rule}, {rule} }`,
/// and a rule as `{ "ending", min, "replacement", {exceptions} }`. Reading it
/// is a matter of keeping each brace's pieces apart from its parent's, which
/// is what the stack is for.
fn steps(text: &'static str) -> Vec<(String, Step)> {
    let mut out: Vec<(String, Step)> = Vec::new();
    let mut stack: Vec<Vec<String>> = Vec::new();
    let mut pieces: Vec<String> = Vec::new();
    let mut groups: Vec<Vec<Vec<String>>> = Vec::new();
    let mut token = String::new();
    let mut in_string = false;
    let mut comment = false;
    for c in text.chars() {
        if comment {
            if c == '\n' {
                comment = false;
            }
            continue;
        }
        if in_string {
            if c == '"' {
                in_string = false;
                pieces.push(std::mem::take(&mut token));
            } else {
                token.push(c);
            }
            continue;
        }
        let mut end_token = |token: &mut String, pieces: &mut Vec<String>| {
            if !token.trim().is_empty() {
                pieces.push(token.trim().to_string());
            }
            token.clear();
        };
        match c {
            '#' => comment = true,
            '"' => {
                token.clear();
                in_string = true;
            }
            '{' => {
                end_token(&mut token, &mut pieces);
                stack.push(std::mem::take(&mut pieces));
                groups.push(Vec::new());
            }
            '}' => {
                end_token(&mut token, &mut pieces);
                let held = std::mem::take(&mut pieces);
                let inner = groups.pop().unwrap_or_default();
                pieces = stack.pop().unwrap_or_default();
                let depth = stack.len();
                if depth == 0 {
                    // a whole step: its head, its suffixes and its rules
                    let name = held.first().cloned().unwrap_or_default();
                    let min: usize = held.get(1).and_then(|n| n.parse().ok()).unwrap_or(0);
                    let mut suffixes = Vec::new();
                    let mut rules = Vec::new();
                    for group in inner {
                        let is_rule = group.len() >= 2 && group[1].parse::<usize>().is_ok();
                        if is_rule {
                            rules.push(Rule {
                                suffix: group[0].clone(),
                                min: group[1].parse().unwrap_or(0),
                                replacement: group.get(2).cloned().unwrap_or_default(),
                                exceptions: group.iter().skip(3).cloned().collect(),
                            });
                        } else if rules.is_empty() {
                            suffixes.extend(group);
                        }
                    }
                    let min = if min == 0 {
                        rules.iter().map(|r| r.min + r.suffix.chars().count()).min().unwrap_or(0)
                    } else {
                        min
                    };
                    if !name.is_empty() {
                        out.push((name, Step { suffixes, min, rules }));
                    }
                } else {
                    // a rule, or a list of words inside one: the pieces it
                    // held, and whatever its own braces held after them
                    let mut group = held;
                    for nested in inner {
                        group.extend(nested);
                    }
                    if let Some(parent) = groups.last_mut() {
                        parent.push(group);
                    }
                }
            }
            ',' | '\n' | '\t' | ' ' | '\r' => end_token(&mut token, &mut pieces),
            other => token.push(other),
        }
    }
    out
}

fn galician_steps() -> &'static Vec<(String, Step)> {
    static STEPS: OnceLock<Vec<(String, Step)>> = OnceLock::new();
    STEPS.get_or_init(|| steps(include_str!("galician.rslp")))
}

/// Galician, as `GalicianStemmer` cuts it: the plural, the adverb, the
/// augmentative, the noun or the verb, the vowel, and then the accents.
pub fn galician(word: &str) -> String {
    let all = galician_steps();
    let step = |name: &str| all.iter().find(|(n, _)| n == name).map(|(_, s)| s);
    let mut w = word.to_string();
    for name in ["Plural", "Unification", "Adverb"] {
        if let Some(s) = step(name) {
            w = s.apply(&w).0;
        }
    }
    if let Some(s) = step("Augmentative") {
        loop {
            let (next, changed) = s.apply(&w);
            w = next;
            if !changed {
                break;
            }
        }
    }
    let mut spoken_for = false;
    if let Some(s) = step("Noun") {
        let (next, changed) = s.apply(&w);
        w = next;
        spoken_for = changed;
    }
    if !spoken_for && let Some(s) = step("Verb") {
        w = s.apply(&w).0;
    }
    if let Some(s) = step("Vowel") {
        w = s.apply(&w).0;
    }
    w.chars()
        .map(|c| match c {
            '\u{00E1}' => 'a',
            '\u{00E9}' | '\u{00EA}' => 'e',
            '\u{00ED}' => 'i',
            '\u{00F3}' => 'o',
            '\u{00FA}' => 'u',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rules_are_read_as_they_are_written() {
        let all = galician_steps();
        let names: Vec<&str> = all.iter().map(|(n, _)| n.as_str()).collect();
        println!("steps: {names:?}");
        for (name, step) in all {
            println!(
                "{name}: min={} suffixes={:?} rules={}",
                step.min,
                step.suffixes,
                step.rules.len()
            );
        }
        assert!(names.contains(&"Plural"));
    }
}
