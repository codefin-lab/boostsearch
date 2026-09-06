//! Japanese kana written in the Latin alphabet.
//!
//! There is no single answer: `し` is `si` under the Kunrei-shiki system the
//! Japanese government publishes and `shi` under the Hepburn one everybody
//! else uses. A completion filter offers both, because somebody typing
//! `sushi` and somebody typing `susi` are both looking for 寿司.
//!
//! Only the two rows differ that ever differ -- the s, t, h and z rows, plus
//! `ん` before a labial -- so one table carries both spellings and the caller
//! says which it wants.

/// Which of the two systems a reading is written in.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum System {
    /// `si`, `ti`, `hu`, `zi`: the system taught in Japanese schools
    Kunrei,
    /// `shi`, `chi`, `fu`, `ji`: the one an English speaker would guess
    Hepburn,
}

/// (kana, kunrei, hepburn) -- longest first, so `きゃ` is read before `き`.
const TABLE: &[(&str, &str, &str)] = &[
    ("きゃ", "kya", "kya"),
    ("きゅ", "kyu", "kyu"),
    ("きょ", "kyo", "kyo"),
    ("しゃ", "sya", "sha"),
    ("しゅ", "syu", "shu"),
    ("しょ", "syo", "sho"),
    ("ちゃ", "tya", "cha"),
    ("ちゅ", "tyu", "chu"),
    ("ちょ", "tyo", "cho"),
    ("にゃ", "nya", "nya"),
    ("にゅ", "nyu", "nyu"),
    ("にょ", "nyo", "nyo"),
    ("ひゃ", "hya", "hya"),
    ("ひゅ", "hyu", "hyu"),
    ("ひょ", "hyo", "hyo"),
    ("みゃ", "mya", "mya"),
    ("みゅ", "myu", "myu"),
    ("みょ", "myo", "myo"),
    ("りゃ", "rya", "rya"),
    ("りゅ", "ryu", "ryu"),
    ("りょ", "ryo", "ryo"),
    ("ぎゃ", "gya", "gya"),
    ("ぎゅ", "gyu", "gyu"),
    ("ぎょ", "gyo", "gyo"),
    ("じゃ", "zya", "ja"),
    ("じゅ", "zyu", "ju"),
    ("じょ", "zyo", "jo"),
    ("ぢゃ", "zya", "ja"),
    ("ぢゅ", "zyu", "ju"),
    ("ぢょ", "zyo", "jo"),
    ("びゃ", "bya", "bya"),
    ("びゅ", "byu", "byu"),
    ("びょ", "byo", "byo"),
    ("ぴゃ", "pya", "pya"),
    ("ぴゅ", "pyu", "pyu"),
    ("ぴょ", "pyo", "pyo"),
    ("あ", "a", "a"),
    ("い", "i", "i"),
    ("う", "u", "u"),
    ("え", "e", "e"),
    ("お", "o", "o"),
    ("か", "ka", "ka"),
    ("き", "ki", "ki"),
    ("く", "ku", "ku"),
    ("け", "ke", "ke"),
    ("こ", "ko", "ko"),
    ("さ", "sa", "sa"),
    ("し", "si", "shi"),
    ("す", "su", "su"),
    ("せ", "se", "se"),
    ("そ", "so", "so"),
    ("た", "ta", "ta"),
    ("ち", "ti", "chi"),
    ("つ", "tu", "tsu"),
    ("て", "te", "te"),
    ("と", "to", "to"),
    ("な", "na", "na"),
    ("に", "ni", "ni"),
    ("ぬ", "nu", "nu"),
    ("ね", "ne", "ne"),
    ("の", "no", "no"),
    ("は", "ha", "ha"),
    ("ひ", "hi", "hi"),
    ("ふ", "hu", "fu"),
    ("へ", "he", "he"),
    ("ほ", "ho", "ho"),
    ("ま", "ma", "ma"),
    ("み", "mi", "mi"),
    ("む", "mu", "mu"),
    ("め", "me", "me"),
    ("も", "mo", "mo"),
    ("や", "ya", "ya"),
    ("ゆ", "yu", "yu"),
    ("よ", "yo", "yo"),
    ("ら", "ra", "ra"),
    ("り", "ri", "ri"),
    ("る", "ru", "ru"),
    ("れ", "re", "re"),
    ("ろ", "ro", "ro"),
    ("わ", "wa", "wa"),
    ("ゐ", "i", "i"),
    ("ゑ", "e", "e"),
    ("を", "wo", "o"),
    ("が", "ga", "ga"),
    ("ぎ", "gi", "gi"),
    ("ぐ", "gu", "gu"),
    ("げ", "ge", "ge"),
    ("ご", "go", "go"),
    ("ざ", "za", "za"),
    ("じ", "zi", "ji"),
    ("ず", "zu", "zu"),
    ("ぜ", "ze", "ze"),
    ("ぞ", "zo", "zo"),
    ("だ", "da", "da"),
    ("ぢ", "zi", "ji"),
    ("づ", "zu", "zu"),
    ("で", "de", "de"),
    ("ど", "do", "do"),
    ("ば", "ba", "ba"),
    ("び", "bi", "bi"),
    ("ぶ", "bu", "bu"),
    ("べ", "be", "be"),
    ("ぼ", "bo", "bo"),
    ("ぱ", "pa", "pa"),
    ("ぴ", "pi", "pi"),
    ("ぷ", "pu", "pu"),
    ("ぺ", "pe", "pe"),
    ("ぽ", "po", "po"),
    ("ぁ", "a", "a"),
    ("ぃ", "i", "i"),
    ("ぅ", "u", "u"),
    ("ぇ", "e", "e"),
    ("ぉ", "o", "o"),
    ("ゃ", "ya", "ya"),
    ("ゅ", "yu", "yu"),
    ("ょ", "yo", "yo"),
];

/// Katakana as the hiragana of the same sound, which is what the table holds.
fn as_hiragana(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            // the two kana are the same syllabary written twice, one block apart
            'ァ'..='ヶ' => char::from_u32(c as u32 - 0x60).unwrap_or(c),
            other => other,
        })
        .collect()
}

/// A reading, written in the Latin alphabet.
///
/// Anything the table does not know is left as it stands: a reading that came
/// back from the dictionary in kanji is not a reading this can write, and
/// dropping it would be worse than passing it through.
pub fn of(reading: &str, system: System) -> String {
    let kana = as_hiragana(reading);
    let mut out = String::new();
    let mut rest = kana.as_str();
    while !rest.is_empty() {
        // `っ` doubles the consonant of the sound after it: `けっか` is `kekka`
        if let Some(after) = rest.strip_prefix('っ') {
            let next = of(&after.chars().next().map(|c| c.to_string()).unwrap_or_default(), system);
            if let Some(consonant) = next.chars().next().filter(|c| !"aiueo".contains(*c)) {
                out.push(consonant);
            }
            rest = after;
            continue;
        }
        // `ー` holds the sound before it, which the Latin alphabet writes by
        // repeating the vowel
        if let Some(after) = rest.strip_prefix('ー') {
            if let Some(vowel) = out.chars().next_back().filter(|c| "aiueo".contains(*c)) {
                out.push(vowel);
            }
            rest = after;
            continue;
        }
        // `ん` is `n`, and `m` before a sound made with the lips under Hepburn
        if let Some(after) = rest.strip_prefix('ん') {
            let labial = after.starts_with([
                'ば', 'び', 'ぶ', 'べ', 'ぼ', 'ぱ', 'ぴ', 'ぷ', 'ぺ', 'ぽ', 'ま', 'み', 'む', 'め',
                'も',
            ]);
            out.push(if labial && system == System::Hepburn { 'm' } else { 'n' });
            rest = after;
            continue;
        }
        match TABLE.iter().find(|(kana, _, _)| rest.starts_with(kana)) {
            Some((kana, kunrei, hepburn)) => {
                out.push_str(if system == System::Kunrei { kunrei } else { hepburn });
                rest = &rest[kana.len()..];
            }
            None => {
                let c = rest.chars().next().unwrap_or_default();
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_systems_differ_where_they_are_meant_to() {
        assert_eq!(of("スシ", System::Kunrei), "susi");
        assert_eq!(of("スシ", System::Hepburn), "sushi");
        assert_eq!(of("オイシイ", System::Kunrei), "oisii");
        assert_eq!(of("オイシイ", System::Hepburn), "oishii");
    }

    #[test]
    fn and_agree_where_they_agree() {
        for word in ["ガ", "ネ", "カンサイ", "ナマエ"] {
            assert_eq!(of(word, System::Kunrei), of(word, System::Hepburn), "{word}");
        }
    }

    #[test]
    fn a_small_tsu_doubles_the_consonant_after_it() {
        assert_eq!(of("ケッカ", System::Hepburn), "kekka");
    }

    #[test]
    fn a_long_mark_repeats_the_vowel_before_it() {
        assert_eq!(of("サーバー", System::Hepburn), "saabaa");
    }

    #[test]
    fn n_is_m_before_the_lips_only_in_hepburn() {
        assert_eq!(of("シンブン", System::Hepburn), "shimbun");
        assert_eq!(of("シンブン", System::Kunrei), "sinbun");
    }
}
