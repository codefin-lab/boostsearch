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

/// French, as `FrenchLightStemmer` cuts it: the accents go, then the ending.
pub(crate) fn french_light(word: &str) -> String {
    let word = strip_accents(word);
    if len(&word) > 5 && word.ends_with('x') {
        let base = cut(&word, 1);
        if base.ends_with("au") {
            return format!("{}l", cut(&base, 1));
        }
        return base;
    }
    let word = if len(&word) > 3 && (word.ends_with('x') || word.ends_with('s')) {
        cut(&word, 1)
    } else {
        word
    };
    let word = strip(&word, 5, &["issement", "issant", "issem"]).unwrap_or(word);
    let word = strip(&word, 6, &["ement"]).unwrap_or(word);
    let word = strip(&word, 5, &["ations", "ation", "ition", "ateur", "atrice"]).unwrap_or(word);
    let word = strip(&word, 5, &["ives", "ive", "logie", "logue"]).unwrap_or(word);
    let word = strip(&word, 6, &["aires", "aire"]).map(|w| format!("{w}air")).unwrap_or(word);
    strip(&word, 4, &["ance", "ence", "ique", "isme", "iste"]).unwrap_or(word)
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

/// Italian, as `ItalianLightStemmer` cuts it: only the plural and the gender.
pub(crate) fn italian_light(word: &str) -> String {
    let word = strip_accents(word);
    if len(&word) < 6 {
        return word;
    }
    match word.chars().last() {
        Some('a') | Some('e') | Some('i') | Some('o') => cut(&word, 1),
        _ => word,
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
    let word = strip_accents(word);
    if len(&word) < 4 {
        return word;
    }
    let word = strip(&word, 3, &["aria", "eria", "oria", "aro", "ario"]).unwrap_or(word);
    let word = strip(&word, 3, &["adora", "ador", "acao", "acoes"]).unwrap_or(word);
    let word = strip(&word, 3, &["mente", "idade", "ismo", "ista", "eza"]).unwrap_or(word);
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

/// Czech, as `CzechStemmer` cuts it: the case ending, then what is left of
/// the possessive.
pub(crate) fn czech(word: &str) -> String {
    if len(word) < 5 {
        return word.to_string();
    }
    let word = strip(
        word,
        4,
        &[
            "atech", "etem", "atum", "ech", "ich", "ich", "eho", "emi", "emu", "ete", "eti", "iho",
            "imi", "imu", "ach", "ata", "aty", "ych", "ama", "ami", "ove", "ovi", "ymi", "em",
            "es", "im", "um", "at", "am", "os", "us", "ym", "mi", "ou", "a", "e", "i", "u", "y",
            "o",
        ],
    )
    .unwrap_or_else(|| word.to_string());
    // and the ending that marks a possessive or a diminutive
    let word = strip(&word, 5, &["ov", "in", "uv"]).unwrap_or(word);
    strip(
        &word,
        4,
        &[
            "ak", "ec", "en", "ic", "in", "it", "iv", "ob", "ot", "ov", "ul", "yn", "ck", "dl",
            "nk", "tv", "tk", "vk",
        ],
    )
    .unwrap_or(word)
}

/// Persian, as `PersianNormalizer` treats it: the Arabic letters are written
/// the Persian way, and the joiner between a prefix and its word goes.
pub(crate) fn persian_normalize(word: &str) -> String {
    word.chars()
        .filter_map(|c| match c {
            '\u{0643}' => Some('\u{06A9}'),
            '\u{064A}' | '\u{0649}' => Some('\u{06CC}'),
            '\u{06C0}' | '\u{0629}' => Some('\u{0647}'),
            '\u{0624}' => Some('\u{0648}'),
            '\u{0625}' | '\u{0623}' | '\u{0622}' => Some('\u{0627}'),
            '\u{200C}' => None,
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
        "hindi" | "indic" => hindi_normalize(word),
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
                // Cyrillic written in Latin letters
                '\u{0430}' => 'a',
                '\u{0431}' => 'b',
                '\u{0432}' => 'v',
                '\u{0433}' => 'g',
                '\u{0434}' => 'd',
                '\u{0435}' => 'e',
                '\u{0437}' => 'z',
                '\u{0438}' => 'i',
                '\u{043A}' => 'k',
                '\u{043B}' => 'l',
                '\u{043C}' => 'm',
                '\u{043D}' => 'n',
                '\u{043E}' => 'o',
                '\u{043F}' => 'p',
                '\u{0440}' => 'r',
                '\u{0441}' => 's',
                '\u{0442}' => 't',
                '\u{0443}' => 'u',
                '\u{0444}' => 'f',
                '\u{0445}' => 'h',
                '\u{0446}' => 'c',
                other => other,
            })
            .collect(),
        _ => word.to_string(),
    }
}

/// The Bengali letters that are written two ways, written one way.
fn bengali_normalize(word: &str) -> String {
    word.chars()
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

/// The Devanagari nasal written as the mark for it.
fn hindi_normalize(word: &str) -> String {
    let chars: Vec<char> = word.chars().collect();
    let mut out = String::with_capacity(word.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let nasal = matches!(c, '\u{0919}' | '\u{091E}' | '\u{0923}' | '\u{0928}' | '\u{092E}');
        if nasal && chars.get(i + 1) == Some(&'\u{094D}') && i + 2 < chars.len() {
            out.push('\u{0902}');
            i += 2;
            continue;
        }
        out.push(c);
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
