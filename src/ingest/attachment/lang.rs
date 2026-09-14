//! What language a text is written in, as the reference's detector decides.
//!
//! OpenSearch asks Tika's Optimaize detector, which is Nakatani Shuyo's
//! language-detection library reworked: character n-grams of one to three
//! letters, a frequency profile per language, and a naive Bayes estimate. For
//! a text of thirty characters or fewer every n-gram is counted once through;
//! for a longer one, n-grams are drawn at random seven times over and the
//! estimates averaged. A language is only named when its probability is at
//! least 0.1; otherwise the answer is the empty string, which the processor
//! writes as it is.
//!
//! The profiles are the library's own, for the 55 languages it was published
//! with (Apache License 2.0, Cybozu Labs), packed into
//! `langprofiles.bin.gz` together with the kanji classes it folds
//! ideographs into. Optimaize adds some rarer languages -- Asturian, Breton,
//! Basque, Irish and a few more -- that are not here. The random draws use
//! Java's generator with a fixed seed, so an answer does not change between
//! runs; the reference seeds from the clock, and on text that is truly
//! ambiguous can answer differently from one run to the next.

use std::collections::HashMap;
use std::sync::OnceLock;

struct Model {
    /// the language codes, as the detector reports them
    langs: Vec<String>,
    /// for each n-gram, the probability each language that has it gives it
    grams: HashMap<String, Vec<(u8, f64)>>,
    /// an ideograph to the ideograph its class is folded into
    kanji: HashMap<char, char>,
}

fn model() -> &'static Model {
    static MODEL: OnceLock<Model> = OnceLock::new();
    MODEL.get_or_init(|| {
        load().unwrap_or(Model { langs: Vec::new(), grams: HashMap::new(), kanji: HashMap::new() })
    })
}

fn load() -> Option<Model> {
    use std::io::Read;
    const PACKED: &[u8] = include_bytes!("langprofiles.bin.gz");
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(PACKED).read_to_end(&mut raw).ok()?;
    if !raw.starts_with(b"LDP1") {
        return None;
    }
    let mut r = Cursor { raw: &raw, at: 4 };
    let nlangs = r.varint()?;
    let mut langs = Vec::with_capacity(nlangs);
    let mut n_words = Vec::with_capacity(nlangs);
    for _ in 0..nlangs {
        let name = r.string()?;
        // Optimaize names the two Chinese profiles by language and country,
        // and reports the language
        langs.push(name.split('-').next().unwrap_or(&name).to_string());
        n_words.push([r.varint()? as f64, r.varint()? as f64, r.varint()? as f64]);
    }
    let ngrams = r.varint()?;
    let mut grams = HashMap::with_capacity(ngrams);
    for _ in 0..ngrams {
        let gram = r.string()?;
        let n = gram.chars().count().clamp(1, 3);
        let count = r.varint()?;
        let mut probs = Vec::with_capacity(count);
        for _ in 0..count {
            let lang = r.varint()?;
            let c = r.varint()? as f64;
            let total = n_words.get(lang)?[n - 1];
            probs.push((lang as u8, if total > 0.0 { c / total } else { 0.0 }));
        }
        grams.insert(gram, probs);
    }
    let classes = r.varint()?;
    let mut kanji = HashMap::new();
    for _ in 0..classes {
        let class = r.string()?;
        let mut chars = class.chars();
        if let Some(first) = chars.next() {
            kanji.insert(first, first);
            for c in chars {
                kanji.insert(c, first);
            }
        }
    }
    Some(Model { langs, grams, kanji })
}

/// A reader over the packed profiles: numbers as varints, strings as a
/// length and their UTF-8 bytes.
struct Cursor<'a> {
    raw: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    fn varint(&mut self) -> Option<usize> {
        let mut v = 0usize;
        let mut shift = 0u32;
        loop {
            let b = *self.raw.get(self.at)?;
            self.at += 1;
            if shift > 56 {
                return None;
            }
            v |= ((b & 0x7f) as usize) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
            shift += 7;
        }
    }

    fn string(&mut self) -> Option<String> {
        let len = self.varint()?;
        let bytes = self.raw.get(self.at..self.at.checked_add(len)?)?;
        self.at += len;
        String::from_utf8(bytes.to_vec()).ok()
    }
}

/// A character as the profiles were built from it: punctuation and digits
/// are spaces, and each script whose letters say nothing apart from the
/// script -- kana, hangul, most ideographs -- is folded into one letter.
fn normalize(c: char, kanji: &HashMap<char, char>) -> char {
    let cp = c as u32;
    match cp {
        0x00..=0x7F => {
            if c.is_ascii_alphabetic() {
                c
            } else {
                ' '
            }
        }
        0x80..=0xFF => {
            if matches!(c, '\u{A0}' | '\u{AB}' | '\u{B0}' | '\u{BB}') {
                ' '
            } else {
                c
            }
        }
        0x0180..=0x024F => match c {
            '\u{0219}' => '\u{015F}',
            '\u{021B}' => '\u{0163}',
            _ => c,
        },
        0x2000..=0x206F => ' ',
        0x0600..=0x06FF => {
            if c == '\u{06CC}' {
                '\u{064A}'
            } else {
                c
            }
        }
        0x1E00..=0x1EFF => {
            if cp >= 0x1EA0 {
                '\u{1EC3}'
            } else {
                c
            }
        }
        0x3040..=0x309F => '\u{3042}',
        0x30A0..=0x30FF => '\u{30A2}',
        0x3100..=0x312F | 0x31A0..=0x31BF => '\u{3105}',
        0x4E00..=0x9FFF => kanji.get(&c).copied().unwrap_or(c),
        0xAC00..=0xD7AF => '\u{AC00}',
        _ => c,
    }
}

/// The detector reads at most this much of a text: Tika feeds it in blocks
/// of 4096 characters until it has twenty thousand.
const MAX_CHARS: usize = 5 * 4096;
const SHORT_TEXT: usize = 30;
const ALPHA: f64 = 0.5;
const ALPHA_WIDTH: f64 = 0.05;
const BASE_FREQ: f64 = 10000.0;
const N_TRIAL: usize = 7;
const ITERATION_LIMIT: usize = 1000;
const CONV_THRESHOLD: f64 = 0.99999;
const PROB_THRESHOLD: f64 = 0.1;
const SEED: u64 = 0x5DEE_CE66;

/// The language a text is written in, or the empty string where no
/// language is likely enough to name.
pub fn language_of(text: &str) -> String {
    let m = model();
    if m.langs.is_empty() {
        return String::new();
    }
    let mut prepared = String::with_capacity(text.len().min(MAX_CHARS * 3) + 2);
    let mut units = 0usize;
    let mut last = ' ';
    // padded with a space at either end, so a word's first and last letters
    // are n-grams with the space beside them
    prepared.push(' ');
    for c in text.chars() {
        units += c.len_utf16();
        if units > MAX_CHARS {
            break;
        }
        let n = normalize(c, &m.kanji);
        if n == ' ' && last == ' ' {
            continue;
        }
        prepared.push(n);
        last = n;
    }
    if last != ' ' {
        prepared.push(' ');
    }
    let chars: Vec<char> = prepared.chars().collect();
    let mut grams: Vec<String> = Vec::new();
    for len in 1..=3 {
        if chars.len() < len {
            continue;
        }
        for pos in 0..=chars.len() - len {
            let g = &chars[pos..pos + len];
            let keep = match len {
                1 => g[0] != ' ',
                2 => !(g[0] == ' ' && g[1] == ' '),
                _ => g[1] != ' ',
            };
            if keep {
                grams.push(g.iter().collect());
            }
        }
    }
    let n = m.langs.len();
    let text_len: usize = text.chars().map(char::len_utf16).sum::<usize>().min(MAX_CHARS);
    let probabilities = if text_len <= SHORT_TEXT {
        short_text(m, &grams, chars.len())
    } else {
        long_text(m, &grams)
    };
    let Some(prob) = probabilities else { return String::new() };
    let mut best: Option<(usize, f64)> = None;
    for (i, p) in prob.iter().enumerate().take(n) {
        if *p >= PROB_THRESHOLD && best.is_none_or(|(_, b)| *p > b) {
            best = Some((i, *p));
        }
    }
    best.map(|(i, _)| m.langs[i].clone()).unwrap_or_default()
}

fn update(m: &Model, prob: &mut [f64], gram: &str, count: usize, alpha: f64) -> bool {
    let Some(known) = m.grams.get(gram) else { return false };
    let weight = alpha / BASE_FREQ;
    let mut per_lang = vec![0.0f64; prob.len()];
    for (lang, p) in known {
        if let Some(slot) = per_lang.get_mut(*lang as usize) {
            *slot = *p;
        }
    }
    for (i, p) in prob.iter_mut().enumerate() {
        for _ in 0..count {
            *p *= weight + per_lang[i];
        }
    }
    true
}

fn normalize_prob(prob: &mut [f64]) -> f64 {
    let sum: f64 = prob.iter().sum();
    let mut max = 0.0;
    for p in prob.iter_mut() {
        *p /= sum;
        if max < *p {
            max = *p;
        }
    }
    max
}

/// Every n-gram of a short text counted once through, in the order Java's
/// `HashMap` would hand them back: the order decides where the estimate
/// stops once it is sure.
fn short_text(m: &Model, grams: &[String], padded_len: usize) -> Option<Vec<f64>> {
    let mut counted: Vec<(String, usize)> = Vec::new();
    for g in grams {
        match counted.iter_mut().find(|(k, _)| k == g) {
            Some((_, c)) => *c += 1,
            None => counted.push((g.clone(), 1)),
        }
    }
    if counted.is_empty() {
        return None;
    }
    let initial: usize = (1..=3).map(|l| padded_len.saturating_sub(l - 1)).sum();
    let keys: Vec<&str> = counted.iter().map(|(g, _)| g.as_str()).collect();
    let mut prob = vec![1.0 / m.langs.len() as f64; m.langs.len()];
    for i in java_hash_order(&keys, initial) {
        let (gram, count) = &counted[i];
        update(m, &mut prob, gram, *count, ALPHA);
        if normalize_prob(&mut prob) > CONV_THRESHOLD {
            break;
        }
    }
    normalize_prob(&mut prob);
    Some(prob)
}

/// The order a `java.util.HashMap` created with `initial` capacity iterates
/// keys inserted in this order: by bucket, and within a bucket by insertion.
fn java_hash_order(keys: &[&str], initial: usize) -> Vec<usize> {
    let mut capacity = initial.max(1).next_power_of_two();
    while keys.len() as f64 > capacity as f64 * 0.75 {
        capacity *= 2;
    }
    let bucket = |s: &str| -> usize {
        let mut h: i32 = 0;
        for unit in s.encode_utf16() {
            h = h.wrapping_mul(31).wrapping_add(unit as i32);
        }
        let spread = h ^ ((h as u32) >> 16) as i32;
        (spread as u32 as usize) & (capacity - 1)
    };
    let mut ordered: Vec<(usize, usize)> =
        keys.iter().enumerate().map(|(i, k)| (bucket(k), i)).collect();
    ordered.sort();
    ordered.into_iter().map(|(_, i)| i).collect()
}

fn long_text(m: &Model, grams: &[String]) -> Option<Vec<f64>> {
    if grams.is_empty() {
        return None;
    }
    let n = m.langs.len();
    let mut total = vec![0.0f64; n];
    let mut rand = JavaRandom::new(SEED);
    for _ in 0..N_TRIAL {
        let mut prob = vec![1.0 / n as f64; n];
        let alpha = ALPHA + rand.next_gaussian() * ALPHA_WIDTH;
        let mut i = 0usize;
        loop {
            let r = rand.next_int(grams.len() as i32) as usize;
            update(m, &mut prob, &grams[r], 1, alpha);
            if i.is_multiple_of(5)
                && (normalize_prob(&mut prob) > CONV_THRESHOLD || i >= ITERATION_LIMIT)
            {
                break;
            }
            i += 1;
        }
        for (t, p) in total.iter_mut().zip(prob.iter()) {
            *t += p / N_TRIAL as f64;
        }
    }
    Some(total)
}

/// `java.util.Random`, which the detector draws from.
struct JavaRandom {
    seed: u64,
    next_gaussian: Option<f64>,
}

impl JavaRandom {
    const MULTIPLIER: u64 = 0x5_DEEC_E66D;
    const MASK: u64 = (1 << 48) - 1;

    fn new(seed: u64) -> JavaRandom {
        JavaRandom { seed: (seed ^ Self::MULTIPLIER) & Self::MASK, next_gaussian: None }
    }

    fn next(&mut self, bits: u32) -> i32 {
        self.seed = (self.seed.wrapping_mul(Self::MULTIPLIER).wrapping_add(0xB)) & Self::MASK;
        (self.seed >> (48 - bits)) as i64 as i32
    }

    fn next_int(&mut self, bound: i32) -> i32 {
        if bound <= 0 {
            return 0;
        }
        if (bound as u32).is_power_of_two() {
            return ((bound as i64 * self.next(31) as i64) >> 31) as i32;
        }
        loop {
            let bits = self.next(31);
            let val = bits % bound;
            if bits.wrapping_sub(val).wrapping_add(bound - 1) >= 0 {
                return val;
            }
        }
    }

    fn next_double(&mut self) -> f64 {
        let high = (self.next(26) as i64) << 27;
        ((high + self.next(27) as i64) as f64) * (1.0 / (1u64 << 53) as f64)
    }

    fn next_gaussian(&mut self) -> f64 {
        if let Some(g) = self.next_gaussian.take() {
            return g;
        }
        loop {
            let v1 = 2.0 * self.next_double() - 1.0;
            let v2 = 2.0 * self.next_double() - 1.0;
            let s = v1 * v1 + v2 * v2;
            if s < 1.0 && s != 0.0 {
                let multiplier = (-2.0 * s.ln() / s).sqrt();
                self.next_gaussian = Some(v2 * multiplier);
                return v1 * multiplier;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_languages() {
        assert_eq!(language_of("This is an english text to test if the pipeline works\n"), "en");
        assert_eq!(
            language_of(
                "Allons enfants de la Patrie Le jour de gloire est arrivé. Contre nous de la tyrannie\n"
            ),
            "fr"
        );
        assert_eq!(
            language_of(
                "Der schnelle braune Fuchs springt über den faulen Hund, und das ist gut so.\n"
            ),
            "de"
        );
        assert_eq!(
            language_of("El rápido zorro marrón salta sobre el perro perezoso en la mañana.\n"),
            "es"
        );
        assert_eq!(language_of("ภาษาไทยเป็นภาษาราชการของประเทศไทย และใช้กันอย่างแพร่หลาย\n"), "th");
        assert_eq!(
            language_of("日本語は日本で話されている言語です。ひらがなとカタカナを使います。\n"),
            "ja"
        );
        assert_eq!(language_of("   \n"), "");
    }

    #[test]
    fn short_text_follows_the_reference() {
        assert_eq!(language_of("This is an english"), "en");
        assert_ne!(language_of("\"God Save "), "en");
    }

    #[test]
    fn java_random_matches_java() {
        // what `new Random(42)` gives in Java
        let mut r = JavaRandom::new(42);
        assert_eq!((r.next_int(100), r.next_int(100), r.next_int(7919)), (30, 63, 2685));
        let mut g = JavaRandom::new(42);
        // Java takes the logarithm with StrictMath, which may differ in the last place
        assert!((g.next_gaussian() - 1.1419053154730547).abs() < 1e-12);
        assert!((g.next_gaussian() - 0.9194079489827879).abs() < 1e-12);
        assert!((g.next_gaussian() + 0.9498666368908959).abs() < 1e-12);
    }

    #[test]
    fn hash_map_order_matches_java() {
        let keys = ["T", "h", "i", "s", " T", "Th", "hi", "is", "s ", " Th", "Thi", "his", "is "];
        let order: Vec<&str> = java_hash_order(&keys, 20).into_iter().map(|i| keys[i]).collect();
        assert_eq!(
            order,
            ["hi", "h", "i", "is", "s ", "s", "his", "T", " T", "Th", " Th", "Thi", "is "]
        );
    }
}
