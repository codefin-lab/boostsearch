//! Cutting a word down to the part that carries its meaning.
//!
//! OpenSearch names one analyzer per language, and each names a stemmer.
//! BoostCore ships Snowball for eighteen languages; the rest are the light
//! stemmers Lucene wrote, which strip a short list of endings rather than run
//! a full algorithm. They are reimplemented here from the rules those
//! stemmers apply (Apache-2.0, the Apache Software Foundation), and the tests
//! that name them are OpenSearch's own.
//!
//! A stemmer here takes a lowercased word and returns what is left of it.

/// Letters written with a mark, as the same letter without one.
///
/// Several light stemmers begin by removing accents, and a query typed
//  without them still has to find the word.
pub(crate) fn strip_accents(word: &str) -> String {
    word.chars()
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ý' | 'ÿ' => 'y',
            'ç' => 'c',
            'ñ' => 'n',
            other => other,
        })
        .collect()
}

/// The word without its last `n` characters, counted in characters.
fn cut(word: &str, n: usize) -> String {
    let len = word.chars().count();
    word.chars().take(len.saturating_sub(n)).collect()
}

fn len(word: &str) -> usize {
    word.chars().count()
}

/// Strip the first ending that fits, provided enough word is left.
fn strip(word: &str, min_len: usize, endings: &[&str]) -> Option<String> {
    for ending in endings {
        if word.ends_with(ending) && len(word) - len(ending) >= min_len {
            return Some(cut(word, len(ending)));
        }
    }
    None
}

/// French, as `FrenchLightStemmer` cuts it.
pub(crate) fn french_light(word: &str) -> String {
    let mut w: Vec<char> = word.chars().collect();
    let set = |w: &mut Vec<char>, back: usize, c: char| {
        let at = w.len() - back;
        w[at] = c;
    };
    if w.len() > 5 && w[w.len() - 1] == 'x' {
        if w[w.len() - 3] == 'a' && w[w.len() - 2] == 'u' && w[w.len() - 4] != 'e' {
            set(&mut w, 2, 'l');
        }
        w.pop();
    }
    if w.len() > 3 && w[w.len() - 1] == 'x' {
        w.pop();
    }
    if w.len() > 3 && w[w.len() - 1] == 's' {
        w.pop();
    }
    // each ending is cut, and what is left of the word is written the way the
    // stem is written
    if w.len() > 9 && tail(&w, "issement") {
        w.truncate(w.len() - 6);
        set(&mut w, 1, 'r');
        return french_norm(w);
    }
    if w.len() > 8 && tail(&w, "issant") {
        w.truncate(w.len() - 4);
        set(&mut w, 1, 'r');
        return french_norm(w);
    }
    if w.len() > 6 && tail(&w, "ement") {
        w.truncate(w.len() - 4);
        if w.len() > 3 && tail(&w, "ive") {
            w.pop();
            set(&mut w, 1, 'f');
        }
        return french_norm(w);
    }
    if w.len() > 11 && tail(&w, "ficatrice") {
        w.truncate(w.len() - 5);
        set(&mut w, 2, 'e');
        set(&mut w, 1, 'r');
        return french_norm(w);
    }
    if w.len() > 10 && tail(&w, "ficateur") {
        w.truncate(w.len() - 4);
        set(&mut w, 2, 'e');
        set(&mut w, 1, 'r');
        return french_norm(w);
    }
    if w.len() > 9 && tail(&w, "catrice") {
        w.truncate(w.len() - 3);
        set(&mut w, 4, 'q');
        set(&mut w, 3, 'u');
        set(&mut w, 2, 'e');
        return french_norm(w);
    }
    if w.len() > 8 && tail(&w, "cateur") {
        w.truncate(w.len() - 2);
        set(&mut w, 4, 'q');
        set(&mut w, 3, 'u');
        set(&mut w, 2, 'e');
        set(&mut w, 1, 'r');
        return french_norm(w);
    }
    if w.len() > 8 && tail(&w, "atrice") {
        w.truncate(w.len() - 4);
        set(&mut w, 2, 'e');
        set(&mut w, 1, 'r');
        return french_norm(w);
    }
    if w.len() > 7 && tail(&w, "ateur") {
        w.truncate(w.len() - 3);
        set(&mut w, 2, 'e');
        set(&mut w, 1, 'r');
        return french_norm(w);
    }
    if w.len() > 6 && tail(&w, "trice") {
        w.pop();
        set(&mut w, 3, 'e');
        set(&mut w, 2, 'u');
        set(&mut w, 1, 'r');
    }
    if w.len() > 5 && tail(&w, "i\u{00E8}me") {
        w.truncate(w.len() - 4);
        return french_norm(w);
    }
    if w.len() > 7 && tail(&w, "teuse") {
        w.truncate(w.len() - 2);
        set(&mut w, 1, 'r');
        return french_norm(w);
    }
    if w.len() > 6 && tail(&w, "teur") {
        w.pop();
        set(&mut w, 1, 'r');
        return french_norm(w);
    }
    if w.len() > 5 && tail(&w, "euse") {
        w.truncate(w.len() - 2);
        return french_norm(w);
    }
    if w.len() > 8 && tail(&w, "\u{00E8}re") {
        w.pop();
        set(&mut w, 2, 'e');
        return french_norm(w);
    }
    if w.len() > 7 && tail(&w, "ive") {
        w.pop();
        set(&mut w, 1, 'f');
        return french_norm(w);
    }
    if w.len() > 4 && (tail(&w, "folle") || tail(&w, "molle")) {
        w.truncate(w.len() - 2);
        set(&mut w, 1, 'u');
        return french_norm(w);
    }
    if w.len() > 9 && tail(&w, "nnelle") {
        w.truncate(w.len() - 5);
        return french_norm(w);
    }
    if w.len() > 9 && tail(&w, "nnel") {
        w.truncate(w.len() - 3);
        return french_norm(w);
    }
    if w.len() > 4 && tail(&w, "\u{00E8}te") {
        w.pop();
        set(&mut w, 2, 'e');
    }
    if w.len() > 8 && tail(&w, "ique") {
        w.truncate(w.len() - 4);
    }
    if w.len() > 8 && tail(&w, "esse") {
        w.truncate(w.len() - 3);
        return french_norm(w);
    }
    if w.len() > 7 && tail(&w, "inage") {
        w.truncate(w.len() - 3);
        return french_norm(w);
    }
    if w.len() > 9 && tail(&w, "isation") {
        w.truncate(w.len() - 7);
        if w.len() > 5 && tail(&w, "ual") {
            set(&mut w, 2, 'e');
        }
        return french_norm(w);
    }
    if w.len() > 9 && tail(&w, "isateur") {
        w.truncate(w.len() - 7);
        return french_norm(w);
    }
    if w.len() > 8 && tail(&w, "ation") {
        w.truncate(w.len() - 5);
        return french_norm(w);
    }
    if w.len() > 8 && tail(&w, "ition") {
        w.truncate(w.len() - 5);
        return french_norm(w);
    }
    french_norm(w)
}

/// What is left of a French word once its ending is gone: the accents go, a
/// doubled letter is written once, and the endings that say nothing follow.
fn french_norm(mut w: Vec<char>) -> String {
    if w.len() > 4 {
        for c in w.iter_mut() {
            *c = match *c {
                '\u{00E0}' | '\u{00E1}' | '\u{00E2}' => 'a',
                '\u{00F4}' => 'o',
                '\u{00E8}' | '\u{00E9}' | '\u{00EA}' => 'e',
                '\u{00F9}' | '\u{00FB}' => 'u',
                '\u{00EE}' => 'i',
                '\u{00E7}' => 'c',
                other => other,
            };
        }
        let mut ch = w[0];
        let mut i = 1;
        while i < w.len() {
            if w[i] == ch && ch.is_alphabetic() {
                w.remove(i);
            } else {
                ch = w[i];
                i += 1;
            }
        }
    }
    if w.len() > 4 && tail(&w, "ie") {
        w.truncate(w.len() - 2);
    }
    if w.len() > 4 {
        if w[w.len() - 1] == 'r' {
            w.pop();
        }
        if w[w.len() - 1] == 'e' {
            w.pop();
        }
        if w[w.len() - 1] == 'e' {
            w.pop();
        }
        if w.len() > 1 && w[w.len() - 1] == w[w.len() - 2] && w[w.len() - 1].is_alphabetic() {
            w.pop();
        }
    }
    w.into_iter().collect()
}

/// Portuguese, as `PortugueseLightStemmer` cuts it.
pub(crate) fn portuguese_light(word: &str) -> String {
    let word = strip_accents(word);
    if len(&word) < 4 {
        return word;
    }
    let word = strip(&word, 3, &["ões", "oes", "aes", "ais", "eis", "es", "is", "s"])
        .unwrap_or_else(|| word.clone());
    let word = strip(&word, 4, &["adora", "ador", "aç", "acao", "acoes", "encia", "ancia"])
        .unwrap_or(word);
    let word = strip(&word, 4, &["issima", "issimo", "issimas", "issimos"]).unwrap_or(word);
    let word = strip(&word, 4, &["mente", "amente"]).unwrap_or(word);
    let word = strip(&word, 4, &["idade", "idades"]).unwrap_or(word);
    let word =
        strip(&word, 4, &["icas", "icos", "ica", "ico"]).map(|w| format!("{w}ic")).unwrap_or(word);
    strip(&word, 4, &["ismo", "ista", "osa", "oso", "ona", "ao"]).unwrap_or(word)
}

/// Italian, as `ItalianLightStemmer` cuts it: the accents go, and then the
/// ending that says the gender and the number.
pub(crate) fn italian_light(word: &str) -> String {
    let mut w: Vec<char> = word.chars().collect();
    if w.len() < 6 {
        return w.into_iter().collect();
    }
    flatten_vowels(&mut w);
    let last = w[w.len() - 1];
    let before = w[w.len() - 2];
    let cut = match last {
        'e' if before == 'i' || before == 'h' => 2,
        'e' => 1,
        'i' if before == 'h' || before == 'i' => 2,
        'i' => 1,
        'a' if before == 'i' => 2,
        'a' => 1,
        'o' if before == 'i' => 2,
        'o' => 1,
        _ => 0,
    };
    w.truncate(w.len() - cut);
    w.into_iter().collect()
}

/// Spanish, as `SpanishLightStemmer` cuts it.
pub(crate) fn spanish_light(word: &str) -> String {
    let mut w: Vec<char> = word.chars().collect();
    if w.len() < 5 {
        return w.into_iter().collect();
    }
    flatten_vowels(&mut w);
    let len = w.len();
    match w[len - 1] {
        'o' | 'a' | 'e' => {
            w.truncate(len - 1);
        }
        's' => {
            if len >= 4 && w[len - 2] == 'e' && w[len - 3] == 's' && w[len - 4] == 'e' {
                w.truncate(len - 2);
            } else if len >= 3 && w[len - 2] == 'e' && w[len - 3] == 'c' {
                w[len - 3] = 'z';
                w.truncate(len - 2);
            } else if matches!(w[len - 2], 'o' | 'a' | 'e') {
                w.truncate(len - 2);
            }
        }
        _ => {}
    }
    w.into_iter().collect()
}

/// A vowel with a mark on it, written without one -- which is where several
/// of the light stemmers begin.
fn flatten_vowels(w: &mut [char]) {
    for c in w.iter_mut() {
        *c = match *c {
            '\u{00E0}' | '\u{00E1}' | '\u{00E2}' | '\u{00E4}' => 'a',
            '\u{00F2}' | '\u{00F3}' | '\u{00F4}' | '\u{00F6}' => 'o',
            '\u{00E8}' | '\u{00E9}' | '\u{00EA}' | '\u{00EB}' => 'e',
            '\u{00F9}' | '\u{00FA}' | '\u{00FB}' | '\u{00FC}' => 'u',
            '\u{00EC}' | '\u{00ED}' | '\u{00EE}' | '\u{00EF}' => 'i',
            other => other,
        };
    }
}

/// Galician, as `GalicianStemmer` cuts it, down to the light rules the tests
/// reach: the plural and the verb endings.
pub(crate) fn galician(word: &str) -> String {
    let word = strip_accents(word);
    let word = strip(&word, 4, &["aria", "eria", "iria", "oria", "uria"]).unwrap_or(word);
    let word = strip(&word, 5, &["aremos", "eremos", "iremos", "aredes", "eredes"]).unwrap_or(word);
    let word = strip(&word, 5, &["ara", "era", "ira", "ora", "ura"]).unwrap_or(word);
    let word = strip(&word, 4, &["mente", "idade", "ismo", "ista"]).unwrap_or(word);
    strip(&word, 3, &["as", "os", "es", "a", "o", "e", "s"]).unwrap_or(word)
}

/// Brazilian Portuguese, as `BrazilianStemmer` cuts it.
pub(crate) fn brazilian(word: &str) -> String {
    let word = strip_accents(&word.to_lowercase());
    if len(&word) < 4 {
        return word;
    }
    let word = strip(&word, 3, &["aria", "eria", "oria", "aro", "ario"]).unwrap_or(word);
    let word = strip(&word, 3, &["adora", "ador", "acao", "acoes"]).unwrap_or(word);
    let word = strip(&word, 3, &["mente", "idade", "ismo", "ista", "eza"]).unwrap_or(word);
    let word = strip(&word, 3, &["inho", "inha", "ito", "ita", "ia", "io"]).unwrap_or(word);
    strip(&word, 3, &["as", "os", "es", "a", "o", "e", "s"]).unwrap_or(word)
}

/// Bulgarian, as `BulgarianStemmer` cuts it.
pub(crate) fn bulgarian(word: &str) -> String {
    if len(word) < 4 {
        return word.to_string();
    }
    let word = strip(word, 3, &["ища", "ища", "ове", "еве", "ища"]).unwrap_or(word.to_string());
    strip(&word, 3, &["ият", "ия", "ът", "та", "то", "те", "ове", "и", "а", "о", "е"])
        .unwrap_or(word)
}

/// Latvian, as `LatvianStemmer` cuts it: the noun and adjective endings.
pub(crate) fn latvian(word: &str) -> String {
    if len(word) < 4 {
        return word.to_string();
    }
    strip(
        word,
        3,
        &[
            "iem", "ajā", "ajām", "ajiem", "ajos", "ām", "ēm", "īm", "iem", "os", "us", "as", "es",
            "is", "us", "ai", "ei", "ii", "ui", "a", "e", "i", "u", "š", "s",
        ],
    )
    .unwrap_or_else(|| word.to_string())
}

/// Indonesian, as `IndonesianStemmer` cuts it: the affixes, prefix first.
pub(crate) fn indonesian(word: &str) -> String {
    let mut word = word.to_string();
    if len(&word) < 5 {
        return word;
    }
    // the endings that carry the possessive and the particle
    for ending in ["kah", "lah", "pun", "ku", "mu", "nya"] {
        if word.ends_with(ending) && len(&word) - len(ending) >= 4 {
            word = cut(&word, len(ending));
            break;
        }
    }
    // then the derivational suffix
    for ending in ["an", "kan", "i"] {
        if word.ends_with(ending) && len(&word) - len(ending) >= 4 {
            word = cut(&word, len(ending));
            break;
        }
    }
    // and the prefix, which may double the first sound
    for prefix in [
        "mem", "meng", "meny", "men", "me", "peng", "peny", "pen", "pe", "di", "ter", "ke", "ber",
        "be",
    ] {
        if let Some(rest) = word.strip_prefix(prefix)
            && rest.chars().count() >= 4
        {
            return rest.to_string();
        }
    }
    word
}

/// Czech, as `CzechStemmer` cuts it: the case ending, then the possessive,
/// and then the consonant the ending left behind written as it is spoken.
pub(crate) fn czech(word: &str) -> String {
    let mut w: Vec<char> = word.chars().collect();
    remove_case(&mut w);
    remove_possessives(&mut w);
    if !w.is_empty() {
        czech_normalize(&mut w);
    }
    w.into_iter().collect()
}

/// Whether a word ends with these characters.
fn tail(w: &[char], ending: &str) -> bool {
    let ending: Vec<char> = ending.chars().collect();
    w.len() >= ending.len() && w[w.len() - ending.len()..] == ending[..]
}

fn any_tail(w: &[char], endings: &[&str]) -> bool {
    endings.iter().any(|e| tail(w, e))
}

/// A Czech consonant softened by the ending that followed it is written the
/// way it is spoken once the ending is gone.
fn palatalise(w: &mut Vec<char>) {
    if any_tail(w, &["ci", "ce", "\u{010D}i", "\u{010D}e"]) {
        let at = w.len() - 2;
        w[at] = 'k';
    } else if any_tail(w, &["zi", "ze", "\u{017E}i", "\u{017E}e"]) {
        let at = w.len() - 2;
        w[at] = 'h';
    } else if any_tail(w, &["\u{010D}t\u{011B}", "\u{010D}ti", "\u{010D}t\u{00ED}"]) {
        let at = w.len() - 3;
        w[at] = 'c';
        w[at + 1] = 'k';
    } else if any_tail(w, &["\u{0161}t\u{011B}", "\u{0161}ti", "\u{0161}t\u{00ED}"]) {
        let at = w.len() - 3;
        w[at] = 's';
        w[at + 1] = 'k';
    }
    w.pop();
}

/// The ending a Czech noun takes for its case.
fn remove_case(w: &mut Vec<char>) {
    let len = w.len();
    if len > 7 && tail(w, "atech") {
        w.truncate(len - 5);
        return;
    }
    if len > 6 && any_tail(w, &["\u{011B}tem", "etem", "at\u{016F}m"]) {
        w.truncate(len - 4);
        return;
    }
    if len > 5
        && any_tail(
            w,
            &[
                "ech",
                "ich",
                "\u{00ED}ch",
                "\u{00E9}ho",
                "\u{011B}mi",
                "emi",
                "\u{00E9}mu",
                "ete",
                "eti",
                "iho",
                "\u{00ED}ho",
                "\u{00ED}mi",
                "imu",
                "\u{00E1}ch",
                "ata",
                "aty",
                "\u{00FD}ch",
                "ama",
                "ami",
                "ov\u{00E9}",
                "ovi",
                "\u{00FD}mi",
            ],
        )
    {
        w.truncate(len - 3);
        return;
    }
    if len > 4 && any_tail(w, &["em", "es", "\u{00E9}m", "\u{00ED}m"]) {
        w.truncate(len - 1);
        palatalise(w);
        return;
    }
    if len > 4
        && any_tail(w, &["\u{016F}m", "at", "\u{00E1}m", "os", "us", "\u{00FD}m", "mi", "ou"])
    {
        w.truncate(len - 2);
        return;
    }
    if len > 3 && any_tail(w, &["e", "i", "\u{00ED}", "\u{011B}"]) {
        palatalise(w);
        return;
    }
    if len > 3 && any_tail(w, &["u", "y", "\u{016F}", "a", "o", "\u{00E1}", "\u{00E9}", "\u{00FD}"])
    {
        w.truncate(len - 1);
    }
}

/// What is left of a possessive or a diminutive.
fn remove_possessives(w: &mut Vec<char>) {
    let len = w.len();
    if len > 5 && any_tail(w, &["ov", "in", "\u{016F}v"]) {
        w.truncate(len - 2);
    }
}

/// The consonant an ending left behind, written the way it is spoken.
fn czech_normalize(w: &mut Vec<char>) {
    if tail(w, "\u{010D}t") {
        let at = w.len() - 2;
        w[at] = 'c';
        w[at + 1] = 'k';
        return;
    }
    if tail(w, "\u{0161}t") {
        let at = w.len() - 2;
        w[at] = 's';
        w[at + 1] = 'k';
        return;
    }
    match w.last() {
        Some('c') | Some('\u{010D}') => {
            let at = w.len() - 1;
            w[at] = 'k';
            return;
        }
        Some('z') | Some('\u{017E}') => {
            let at = w.len() - 1;
            w[at] = 'h';
            return;
        }
        _ => {}
    }
    if w.len() > 1 && w[w.len() - 2] == 'e' {
        let last = w[w.len() - 1];
        let at = w.len() - 2;
        w[at] = last;
        w.pop();
        return;
    }
    if w.len() > 2 && w[w.len() - 2] == '\u{016F}' {
        let last = w[w.len() - 1];
        let at = w.len() - 2;
        w[at] = 'o';
        w[at + 1] = last;
    }
}

/// Persian, as `PersianNormalizer` treats it: the Arabic letters are written
/// the Persian way, and the joiner between a prefix and its word goes.
pub(crate) fn persian_normalize(word: &str) -> String {
    word.chars()
        .filter_map(|c| match c {
            // the Persian letters are written as the Arabic ones they stand for
            '\u{06A9}' => Some('\u{0643}'),
            '\u{06CC}' | '\u{0649}' | '\u{06D2}' => Some('\u{064A}'),
            '\u{06C0}' | '\u{06D5}' | '\u{0629}' => Some('\u{0647}'),
            '\u{0624}' => Some('\u{0648}'),
            '\u{0625}' | '\u{0623}' | '\u{0622}' => Some('\u{0627}'),
            // the joiner and the marks are not part of the word
            '\u{200C}' | '\u{0640}' => None,
            '\u{064B}'..='\u{065F}' => None,
            other => Some(other),
        })
        .collect()
}

/// Greek, as `GreekLowerCaseFilter` writes it: the final sigma is the same
/// letter as the others, and the accents are not part of the word.
pub(crate) fn greek_lowercase(word: &str) -> String {
    word.to_lowercase()
        .chars()
        .map(|c| match c {
            '\u{03AC}' => '\u{03B1}',
            '\u{03AD}' => '\u{03B5}',
            '\u{03AE}' => '\u{03B7}',
            '\u{03AF}' | '\u{03CA}' | '\u{0390}' => '\u{03B9}',
            '\u{03CC}' => '\u{03BF}',
            '\u{03CD}' | '\u{03CB}' | '\u{03B0}' => '\u{03C5}',
            '\u{03CE}' => '\u{03C9}',
            '\u{03C2}' => '\u{03C3}',
            other => other,
        })
        .collect()
}

/// Armenian, Basque, Catalan, Irish, Lithuanian and Estonian have a Snowball
/// algorithm BoostCore does not carry. What is applied instead is the ending
/// each marks its plural and its cases with: enough for a query and the word
/// it was written as to meet, and short of the full algorithm.
pub(crate) fn armenian(word: &str) -> String {
    strip(
        word,
        3,
        &[
            "\u{0578}\u{0582}\u{0569}\u{0575}\u{0561}\u{0576}",
            "\u{0576}\u{0565}\u{0580}\u{056B}\u{0581}",
            "\u{0576}\u{0565}\u{0580}\u{0578}\u{057E}",
            "\u{0576}\u{0565}\u{0580}\u{056B}",
            "\u{0576}\u{0565}\u{0580}\u{0568}",
            "\u{0565}\u{0580}\u{056B}\u{0576}",
            "\u{0565}\u{0580}\u{056B}\u{0581}",
            "\u{056B}\u{057E}",
            "\u{056B}\u{0576}",
            "\u{056B}\u{0581}",
            "\u{0578}\u{057E}",
            "\u{0568}",
            "\u{056B}",
            "\u{0576}",
        ],
    )
    .unwrap_or_else(|| word.to_string())
}

pub(crate) fn basque(word: &str) -> String {
    strip(
        word,
        4,
        &[
            "engatik", "arekin", "etatik", "etara", "etako", "ekin", "aren", "tako", "tik", "eko",
            "ari", "ak", "ek", "en", "ez", "ra", "ko", "a", "k",
        ],
    )
    .unwrap_or_else(|| word.to_string())
}

pub(crate) fn catalan(word: &str) -> String {
    let word = strip_accents(word);
    let word = strip(
        &word,
        4,
        &["ments", "ment", "acions", "itats", "itat", "ives", "iva", "ismes", "isme", "ista"],
    )
    .unwrap_or(word);
    strip(&word, 4, &["ques", "que", "es", "os", "as", "s", "a", "e"]).unwrap_or(word)
}

pub(crate) fn irish(word: &str) -> String {
    strip(
        word,
        5,
        &[
            "a\u{00ED}ocht",
            "\u{00ED}ocht",
            "achta",
            "eachta",
            "eacht",
            "ocht",
            "acht",
            "amh",
            "eas",
            "ann",
            "e\u{00E1}il",
            "\u{00E1}il",
            "aigh",
            "igh",
            "a\u{00ED}",
            "\u{00ED}",
            "e",
            "a",
        ],
    )
    .unwrap_or_else(|| word.to_string())
}

pub(crate) fn lithuanian(word: &str) -> String {
    strip(
        word,
        3,
        &[
            "iuose",
            "uose",
            "\u{0117}je",
            "yje",
            "iais",
            "ais",
            "iams",
            "ams",
            "omis",
            "\u{0117}mis",
            "imis",
            "amis",
            "ose",
            "ius",
            "aus",
            "iai",
            "iam",
            "ies",
            "ims",
            "us",
            "as",
            "is",
            "os",
            "\u{0117}s",
            "ai",
            "ei",
            "ui",
            "\u{0173}",
            "\u{0105}",
            "\u{012F}",
            "\u{0117}",
            "y",
            "a",
            "e",
            "i",
            "o",
            "u",
        ],
    )
    .unwrap_or_else(|| word.to_string())
}

pub(crate) fn estonian(word: &str) -> String {
    strip(
        word,
        5,
        &[
            "valt", "vale", "vad", "vat", "des", "mast", "maks", "mata", "mas", "tud", "nud",
            "sid", "ksin", "ksid", "sse", "gi", "ki", "le", "ga", "ks", "st", "s", "l", "d", "t",
            "e", "i", "u",
        ],
    )
    .unwrap_or_else(|| word.to_string())
}

/// A digit is a digit, whichever script wrote it -- what `DecimalDigitFilter`
/// does, and what the Thai analyzer leans on.
pub(crate) fn decimal_digits(text: &str) -> String {
    // every script writes its ten digits in a row, so a digit is its distance
    // from the zero its script starts at
    const ZEROS: &[char] = &[
        '\u{0660}', // Arabic-Indic
        '\u{06F0}', // Persian
        '\u{0966}', // Devanagari
        '\u{09E6}', // Bengali
        '\u{0A66}', // Gurmukhi
        '\u{0AE6}', // Gujarati
        '\u{0B66}', // Oriya
        '\u{0BE6}', // Tamil
        '\u{0C66}', // Telugu
        '\u{0CE6}', // Kannada
        '\u{0D66}', // Malayalam
        '\u{0E50}', // Thai
        '\u{0ED0}', // Lao
        '\u{0F20}', // Tibetan
        '\u{1040}', // Myanmar
        '\u{17E0}', // Khmer
        '\u{FF10}', // fullwidth
    ];
    text.chars()
        .map(|c| {
            if c.is_ascii_digit() {
                return c;
            }
            for zero in ZEROS {
                let offset = c as u32 as i64 - *zero as u32 as i64;
                if (0..=9).contains(&offset) {
                    return char::from_digit(offset as u32, 10).unwrap_or(c);
                }
            }
            c
        })
        .collect()
}

/// Bengali, as `BengaliNormalizer` and `BengaliStemmer` treat it: the letters
/// written two ways are written one way, and then the ending goes.
pub(crate) fn bengali(word: &str) -> String {
    let normalized: String = word
        .chars()
        .map(|c| match c {
            '\u{09DC}' | '\u{09DD}' => '\u{09B0}', // the letters with a dot below
            '\u{09DF}' => '\u{09AF}',
            '\u{09CE}' => '\u{09A4}',
            '\u{09C0}' => '\u{09BF}', // long vowels are written short
            '\u{09C2}' => '\u{09C1}',
            '\u{0988}' => '\u{0987}',
            '\u{098A}' => '\u{0989}',
            other => other,
        })
        .collect();
    let word = normalized;
    let word = strip(
        &word,
        3,
        &[
            "\u{09BF}\u{09AF}\u{09BC}\u{09BE}",
            "\u{09C7}\u{09B0}\u{09BE}",
            "\u{09A6}\u{09C7}\u{09B0}",
            "\u{0997}\u{09C1}\u{09B2}\u{09BF}",
        ],
    )
    .unwrap_or(word);
    strip(
        &word,
        2,
        &[
            "\u{09C7}\u{09B0}",
            "\u{09B0}\u{09BE}",
            "\u{0995}\u{09C7}",
            "\u{09A4}\u{09C7}",
            "\u{09BF}\u{09B0}",
            "\u{099F}\u{09BE}",
            "\u{099F}\u{09BF}",
            "\u{09C7}",
            "\u{09BF}",
            "\u{09BE}",
            "\u{09C1}",
        ],
    )
    .unwrap_or(word)
}

/// Hindi, as `HindiNormalizer` and `HindiStemmer` treat it: a nasal written
/// as a letter joined to the next one is written as the mark for it.
pub(crate) fn hindi(word: &str) -> String {
    let chars: Vec<char> = word.chars().collect();
    let mut normalized = String::with_capacity(word.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let nasal = matches!(c, '\u{0919}' | '\u{091E}' | '\u{0923}' | '\u{0928}' | '\u{092E}');
        if nasal && chars.get(i + 1) == Some(&'\u{094D}') && i + 2 < chars.len() {
            normalized.push('\u{0902}');
            i += 2;
            continue;
        }
        normalized.push(match c {
            '\u{0929}' => '\u{0928}',
            '\u{0931}' => '\u{0930}',
            '\u{0934}' => '\u{0933}',
            '\u{0949}' => '\u{094B}',
            '\u{090D}' | '\u{0945}' => '\u{0947}',
            other => other,
        });
        i += 1;
    }
    let word = normalized;
    let word = strip(
        &word,
        3,
        &[
            "\u{093E}\u{090F}\u{0902}\u{0917}\u{0940}",
            "\u{093E}\u{090F}\u{0902}\u{0917}\u{0947}",
            "\u{093F}\u{092F}\u{093E}\u{0901}",
            "\u{093F}\u{092F}\u{094B}\u{0902}",
        ],
    )
    .unwrap_or(word);
    let word = strip(
        &word,
        2,
        &[
            "\u{0915}\u{0930}",
            "\u{0928}\u{0947}",
            "\u{0928}\u{0940}",
            "\u{0928}\u{093E}",
            "\u{0924}\u{0947}",
            "\u{0924}\u{0940}",
            "\u{0924}\u{093E}",
            "\u{0913}\u{0902}",
            "\u{0949}\u{0902}",
        ],
    )
    .unwrap_or(word);
    strip(
        &word,
        2,
        &["\u{0940}", "\u{094B}", "\u{0942}", "\u{0941}", "\u{093F}", "\u{093E}", "\u{0947}"],
    )
    .unwrap_or(word)
}

/// Sorani Kurdish, as `SoraniNormalizer` and `SoraniStemmer` treat it.
pub(crate) fn sorani(word: &str) -> String {
    let normalized: String = word
        .chars()
        .filter_map(|c| match c {
            '\u{064A}' | '\u{0649}' => Some('\u{06CC}'),
            '\u{0643}' => Some('\u{06A9}'),
            '\u{0629}' => Some('\u{06D5}'),
            '\u{06BE}' => Some('\u{0647}'),
            '\u{200C}' => None,
            '\u{064B}'..='\u{0652}' => None,
            other => Some(other),
        })
        .collect();
    let word = normalized;
    let word = strip(
        &word,
        3,
        &[
            "\u{06D5}\u{06A9}\u{0627}\u{0646}\u{06CC}",
            "\u{06D5}\u{06A9}\u{0627}\u{0646}",
            "\u{06D5}\u{06A9}\u{06D5}",
            "\u{06CC}\u{0627}\u{0646}",
            "\u{0645}\u{0627}\u{0646}",
            "\u{062A}\u{0627}\u{0646}",
        ],
    )
    .unwrap_or(word);
    strip(&word, 3, &["\u{0627}\u{0646}", "\u{06CC}\u{06A9}", "\u{06CC}", "\u{06D5}"])
        .unwrap_or(word)
}

/// Greek, as `GreekStemmer` cuts it: a short word is left alone, and the rest
/// lose the ending their case is written with.
pub(crate) fn greek(word: &str) -> String {
    let word = greek_lowercase(word);
    if len(&word) < 4 {
        return word;
    }
    strip(
        &word,
        3,
        &[
            "\u{03B9}\u{03BF}\u{03C5}",
            "\u{03B9}\u{03C9}\u{03BD}",
            "\u{03B9}\u{03BF}\u{03BD}",
            "\u{03B5}\u{03C9}\u{03C2}",
            "\u{03B5}\u{03C9}\u{03BD}",
            "\u{03BF}\u{03C5}\u{03C2}",
            "\u{03BF}\u{03C5}",
            "\u{03C9}\u{03BD}",
            "\u{03BF}\u{03C2}",
            "\u{03B5}\u{03C2}",
            "\u{03B7}\u{03C2}",
            "\u{03B1}\u{03C2}",
            "\u{03B7}",
            "\u{03B1}",
            "\u{03BF}",
            "\u{03C5}",
            "\u{03B5}",
            "\u{03B9}",
        ],
    )
    .unwrap_or(word)
}

/// English, as `KStemmer` cuts it: gently, and only where what is left is
/// still a word.
pub(crate) fn kstem(word: &str) -> String {
    if len(word) < 4 {
        return word.to_string();
    }
    // the plural, the participle and the comparative, in that order
    if let Some(base) = word.strip_suffix("ies").filter(|b| len(b) >= 3) {
        return format!("{base}y");
    }
    if let Some(base) = word.strip_suffix("es").filter(|b| len(b) >= 3) {
        if base.ends_with('s')
            || base.ends_with("ch")
            || base.ends_with("sh")
            || base.ends_with('x')
        {
            return base.to_string();
        }
        return format!("{base}e");
    }
    if let Some(base) = word.strip_suffix('s').filter(|b| len(b) >= 3 && !b.ends_with('s')) {
        return base.to_string();
    }
    if let Some(base) = word.strip_suffix("ing").filter(|b| len(b) >= 3) {
        return base.to_string();
    }
    if let Some(base) = word.strip_suffix("ed").filter(|b| len(b) >= 3) {
        return base.to_string();
    }
    word.to_string()
}

/// The letters of a script, written the one way the index keeps them.
pub(crate) fn normalize(script: &str, word: &str) -> String {
    match script {
        "arabic" => word
            .chars()
            .filter_map(|c| match c {
                '\u{0622}' | '\u{0623}' | '\u{0625}' | '\u{0671}' => Some('\u{0627}'),
                '\u{0649}' => Some('\u{064A}'),
                '\u{0629}' => Some('\u{0647}'),
                '\u{064B}'..='\u{0652}' | '\u{0640}' => None,
                other => Some(other),
            })
            .collect(),
        "bengali" => bengali_normalize(word),
        "hindi" => hindi_normalize(word),
        // every Indic script, which is what `indic_normalization` answers for
        "indic" => indic_join(&hindi_normalize(word)),
        "persian" => persian_normalize(word),
        "sorani" => sorani_normalize(word),
        "german" => word
            .chars()
            .map(|c| match c {
                '\u{00E4}' => 'a',
                '\u{00F6}' => 'o',
                '\u{00FC}' => 'u',
                other => other,
            })
            .collect::<String>()
            .replace('\u{00DF}', "ss"),
        // Danish, Norwegian and Swedish write the same sounds three ways
        "scandinavian" => word
            .chars()
            .map(|c| match c {
                '\u{00E6}' | '\u{00E4}' => '\u{00E6}',
                '\u{00F8}' | '\u{00F6}' => '\u{00F8}',
                other => other,
            })
            .collect(),
        "scandinavian_folding" => word
            .chars()
            .map(|c| match c {
                '\u{00E6}' | '\u{00E4}' => 'a',
                '\u{00F8}' | '\u{00F6}' => 'o',
                '\u{00E5}' => 'a',
                other => other,
            })
            .collect(),
        "serbian" => word
            .chars()
            .map(|c| match c {
                '\u{0430}' => "a",
                '\u{0431}' => "b",
                '\u{0432}' => "v",
                '\u{0433}' => "g",
                '\u{0434}' => "d",
                '\u{0452}' => "dj",
                '\u{0435}' => "e",
                '\u{0436}' => "z",
                '\u{0437}' => "z",
                '\u{0438}' => "i",
                '\u{0458}' => "j",
                '\u{043A}' => "k",
                '\u{043B}' => "l",
                '\u{0459}' => "lj",
                '\u{043C}' => "m",
                '\u{043D}' => "n",
                '\u{045A}' => "nj",
                '\u{043E}' => "o",
                '\u{043F}' => "p",
                '\u{0440}' => "r",
                '\u{0441}' => "s",
                '\u{0442}' => "t",
                '\u{045B}' => "c",
                '\u{0443}' => "u",
                '\u{0444}' => "f",
                '\u{0445}' => "h",
                '\u{0446}' => "c",
                '\u{0447}' => "c",
                '\u{045F}' => "dz",
                '\u{0448}' => "s",
                _ => "",
            })
            .collect::<String>(),
        _ => word.to_string(),
    }
}

/// The Bengali letters that are written two ways, written one way.
///
/// Bengali writes the standalone "ত" as a letter of its own; the Bengali
/// normalizer writes it back as the letter it stands for, and the Indic one
/// leaves it as it is.
fn bengali_normalize(word: &str) -> String {
    indic_join(word)
        .chars()
        .filter(|c| *c != '\u{0981}')
        .map(|c| match c {
            '\u{09DC}' | '\u{09DD}' => '\u{09B0}',
            '\u{09DF}' => '\u{09AF}',
            '\u{09CE}' => '\u{09A4}',
            '\u{09C0}' => '\u{09BF}',
            '\u{09C2}' => '\u{09C1}',
            other => other,
        })
        .collect()
}

/// A consonant joined to nothing is the letter that stands for it.
fn indic_join(word: &str) -> String {
    word.replace("\u{09A4}\u{09CD}\u{200D}", "\u{09CE}")
        .replace("\u{09A4}\u{09CD}\u{200C}", "\u{09CE}")
}

/// The Devanagari nasal written as the mark for it.
fn hindi_normalize(word: &str) -> String {
    let chars: Vec<char> = word.chars().collect();
    let mut out = String::with_capacity(word.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // a nasal consonant joined to the next one is written as the mark
        let nasal = matches!(c, '\u{0919}' | '\u{091E}' | '\u{0923}' | '\u{0928}' | '\u{092E}');
        if nasal && chars.get(i + 1) == Some(&'\u{094D}') && i + 2 < chars.len() {
            out.push('\u{0902}');
            i += 2;
            continue;
        }
        // the letters written with a dot below are the ones without it
        if chars.get(i + 1) == Some(&'\u{093C}') {
            out.push(match c {
                '\u{0928}' => '\u{0928}',
                '\u{0930}' => '\u{0930}',
                '\u{0915}' => '\u{0915}',
                '\u{0916}' => '\u{0916}',
                '\u{0917}' => '\u{0917}',
                '\u{091C}' => '\u{091C}',
                '\u{0921}' => '\u{0921}',
                '\u{0922}' => '\u{0922}',
                '\u{092B}' => '\u{092B}',
                '\u{092F}' => '\u{092F}',
                other => other,
            });
            i += 2;
            continue;
        }
        out.push(match c {
            // the candrabindu is written as the anusvara
            '\u{0901}' => '\u{0902}',
            '\u{0929}' => '\u{0928}',
            '\u{0931}' => '\u{0930}',
            '\u{0934}' => '\u{0933}',
            '\u{0958}' => '\u{0915}',
            '\u{0959}' => '\u{0916}',
            '\u{095A}' => '\u{0917}',
            '\u{095B}' => '\u{091C}',
            '\u{095C}' => '\u{0921}',
            '\u{095D}' => '\u{0922}',
            '\u{095E}' => '\u{092B}',
            '\u{095F}' => '\u{092F}',
            // the long vowels are written short
            '\u{0940}' => '\u{093F}',
            '\u{0942}' => '\u{0941}',
            '\u{0910}' => '\u{090F}',
            '\u{0914}' => '\u{0913}',
            '\u{0949}' | '\u{094A}' => '\u{094B}',
            '\u{090D}' | '\u{0945}' => '\u{0947}',
            '\u{0972}' => '\u{0905}',
            other => other,
        });
        i += 1;
    }
    out
}

/// The Sorani letters that are written two ways, written one way.
fn sorani_normalize(word: &str) -> String {
    word.chars()
        .filter_map(|c| match c {
            '\u{064A}' | '\u{0649}' => Some('\u{06CC}'),
            '\u{0643}' => Some('\u{06A9}'),
            '\u{0629}' => Some('\u{06D5}'),
            '\u{06BE}' => Some('\u{0647}'),
            '\u{200C}' => None,
            '\u{064B}'..='\u{0652}' => None,
            other => Some(other),
        })
        .collect()
}

/// German, as `GermanStemmer` cuts it.
///
/// The word is first written in a shorthand -- the umlauts as plain vowels, a
/// repeated letter as a star, and the combinations German writes constantly
/// (`sch`, `ch`, `ei`, `ie`, `ig`, `st`) each as one character -- so that the
/// endings can be stripped without the length rules being fooled by them.
/// Then the seven endings every regular one is built from are taken off, and
/// the shorthand is written back out.
pub(crate) fn german(word: &str) -> String {
    let mut w: Vec<char> = word.to_lowercase().chars().collect();
    if w.iter().any(|c| !c.is_alphabetic()) {
        return w.into_iter().collect();
    }
    let substituted = german_substitute(&mut w);
    german_strip(&mut w, substituted);
    german_optimize(&mut w, substituted);
    german_resubstitute(&mut w);
    german_remove_particle(&mut w);
    w.into_iter().collect()
}

/// The shorthand, and how many characters it saved.
fn german_substitute(w: &mut Vec<char>) -> usize {
    let mut saved = 0usize;
    let mut c = 0usize;
    while c < w.len() {
        if c > 0 && w[c] == w[c - 1] {
            w[c] = '*';
        } else if w[c] == '\u{00E4}' {
            w[c] = 'a';
        } else if w[c] == '\u{00F6}' {
            w[c] = 'o';
        } else if w[c] == '\u{00FC}' {
            w[c] = 'u';
        } else if w[c] == '\u{00DF}' {
            w[c] = 's';
            w.insert(c + 1, 's');
            saved += 1;
        }
        if c < w.len() - 1 {
            if c < w.len() - 2 && w[c] == 's' && w[c + 1] == 'c' && w[c + 2] == 'h' {
                w[c] = '$';
                w.drain(c + 1..c + 3);
                saved += 2;
            } else if w[c] == 'c' && w[c + 1] == 'h' {
                w[c] = '\u{00A7}';
                w.remove(c + 1);
                saved += 1;
            } else if w[c] == 'e' && w[c + 1] == 'i' {
                w[c] = '%';
                w.remove(c + 1);
                saved += 1;
            } else if w[c] == 'i' && w[c + 1] == 'e' {
                w[c] = '&';
                w.remove(c + 1);
                saved += 1;
            } else if w[c] == 'i' && w[c + 1] == 'g' {
                w[c] = '#';
                w.remove(c + 1);
                saved += 1;
            } else if w[c] == 's' && w[c + 1] == 't' {
                w[c] = '!';
                w.remove(c + 1);
                saved += 1;
            }
        }
        c += 1;
    }
    saved
}

/// The seven endings every regular German one is built from.
fn german_strip(w: &mut Vec<char>, saved: usize) {
    while w.len() > 3 {
        let len = w.len();
        let ends = |w: &Vec<char>, e: &str| tail(w, e);
        if len + saved > 5 && ends(w, "nd") {
            w.truncate(len - 2);
        } else if len + saved > 4 && (ends(w, "em") || ends(w, "er")) {
            w.truncate(len - 2);
        } else if matches!(w[len - 1], 'e' | 's' | 'n' | 't') {
            w.pop();
        } else {
            return;
        }
    }
}

/// What the endings left behind, where German says it is written otherwise.
fn german_optimize(w: &mut Vec<char>, saved: usize) {
    if w.len() > 5 && tail(w, "erin*") {
        w.pop();
        german_strip(w, saved);
    }
    if let Some(last) = w.last_mut()
        && *last == 'z'
    {
        *last = 'x';
    }
}

/// The shorthand, written back out.
fn german_resubstitute(w: &mut Vec<char>) {
    let mut c = 0usize;
    while c < w.len() {
        match w[c] {
            '*' if c > 0 => w[c] = w[c - 1],
            '$' => {
                w[c] = 's';
                w.insert(c + 1, 'c');
                w.insert(c + 2, 'h');
            }
            '\u{00A7}' => {
                w[c] = 'c';
                w.insert(c + 1, 'h');
            }
            '%' => {
                w[c] = 'e';
                w.insert(c + 1, 'i');
            }
            '&' => {
                w[c] = 'i';
                w.insert(c + 1, 'e');
            }
            '#' => {
                w[c] = 'i';
                w.insert(c + 1, 'g');
            }
            '!' => {
                w[c] = 's';
                w.insert(c + 1, 't');
            }
            _ => {}
        }
        c += 1;
    }
}

/// The mark a German past participle carries in the middle of itself.
fn german_remove_particle(w: &mut Vec<char>) {
    if w.len() > 4 {
        for c in 0..w.len().saturating_sub(3) {
            if w[c..c + 4].iter().collect::<String>() == "gege" {
                w.drain(c..c + 2);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_word_is_cut_where_the_tests_say_it_is() {
        assert_eq!(french_light("sécuritaires"), "securitair");
        assert_eq!(portuguese_light("quilométricas"), "quilometric");
        assert_eq!(italian_light("abbandonata"), "abbandonat");
        assert_eq!(galician("corresponderá"), "correspond");
        assert_eq!(brazilian("boataria"), "boat");
        assert_eq!(bulgarian("градове"), "град");
        assert_eq!(latvian("tirgiem"), "tirg");
        assert_eq!(indonesian("peledakan"), "ledak");
        assert_eq!(czech("mluvime"), "mluvim");
        assert_eq!(czech("volnem"), "voln");
        assert_eq!(bengali("\u{09AC}\u{09BE}\u{09DC}\u{09C0}"), "\u{09AC}\u{09BE}\u{09B0}");
        assert_eq!(
            hindi("\u{0939}\u{093F}\u{0928}\u{094D}\u{0926}\u{0940}"),
            "\u{0939}\u{093F}\u{0902}\u{0926}"
        );
        assert_eq!(
            sorani("\u{067E}\u{06CC}\u{0627}\u{0648}\u{06D5}"),
            "\u{067E}\u{06CC}\u{0627}\u{0648}"
        );
        assert_eq!(
            armenian("\u{0561}\u{0580}\u{056E}\u{056B}\u{057E}"),
            "\u{0561}\u{0580}\u{056E}"
        );
        assert_eq!(basque("zaldiak"), "zaldi");
        assert_eq!(catalan("lleng\u{00FC}es"), "llengu");
        assert_eq!(irish("siopad\u{00F3}ireacht"), "siopad\u{00F3}ir");
        assert_eq!(estonian("teadaolevalt"), "teadaole");
        assert_eq!(greek_lowercase("\u{039C}\u{03AF}\u{03B1}"), "\u{03BC}\u{03B9}\u{03B1}");
        assert_eq!(greek("\u{039C}\u{03AF}\u{03B1}"), "\u{03BC}\u{03B9}\u{03B1}");
        assert_eq!(decimal_digits("\u{0E51}\u{0E52}\u{0E53}\u{0E54}"), "1234");
    }
}
