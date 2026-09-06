//! Korean numbers written in words, as digits.
//!
//! `십만이천오백` is a hundred and two thousand five hundred, and somebody
//! searching for `102500` should find it. The Sino-Korean numerals are
//! positional in the same way the Chinese ones they come from are: a digit
//! followed by a place multiplies it, and the places stack -- 만 (ten
//! thousand) and 억 (a hundred million) start a new group, while 십, 백 and
//! 천 build one up.
//!
//! Arabic digits mixed in are read as themselves, so `３.２천` is 3200: the
//! full-width digits are digits, and 천 multiplies what came before.

/// A digit, or nothing if the character is not one.
fn digit(c: char) -> Option<u64> {
    match c {
        '영' | '령' | '공' => Some(0),
        '일' => Some(1),
        '이' => Some(2),
        '삼' => Some(3),
        '사' => Some(4),
        '오' => Some(5),
        '육' | '륙' => Some(6),
        '칠' => Some(7),
        '팔' => Some(8),
        '구' => Some(9),
        // the digits as digits, half-width and full
        '0'..='9' => Some(c as u64 - '0' as u64),
        '０'..='９' => Some(c as u64 - '０' as u64),
        _ => None,
    }
}

/// What a place multiplies by, and whether it starts a new group.
fn place(c: char) -> Option<(u64, bool)> {
    match c {
        '십' => Some((10, false)),
        '백' => Some((100, false)),
        '천' => Some((1_000, false)),
        '만' => Some((10_000, true)),
        '억' => Some((100_000_000, true)),
        '조' => Some((1_000_000_000_000, true)),
        _ => None,
    }
}

/// Whether a token is made only of the characters a number is written with.
///
/// A number can arrive as several tokens -- the dictionary reads `십만이천오백`
/// as `십`, `만이천`, `오`, `백` -- and it is one number, so the run has to be
/// put back together before it can be read. This is what says where such a
/// run continues and where it ends.
pub fn is_numeral(word: &str) -> bool {
    !word.is_empty() && word.chars().all(|c| digit(c).is_some() || place(c).is_some())
}

/// Whether a token is the point in the middle of a decimal number.
pub fn is_point(word: &str) -> bool {
    word == "." || word == "．"
}

/// A word as a number, or nothing if it is not one.
///
/// Nothing is the answer whenever a single character is not a numeral, which
/// is what keeps a word merely containing a numeral from being rewritten:
/// `사람` begins with the numeral for four and is the word for a person.
pub fn of(word: &str) -> Option<String> {
    if word.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    // what has been read: the total, the group being built, and the digits
    // seen since the last place
    let (mut total, mut group, mut digits) = (0u64, 0u64, None::<u64>);
    // a fraction: `３.２천` is two tenths of a thousand added to three thousand
    let mut fraction: Option<(u64, u32)> = None;
    let mut any = false;
    let mut after_point = false;
    for c in word.chars() {
        if c == '.' || c == '．' {
            if digits.is_none() {
                return None;
            }
            after_point = true;
            continue;
        }
        if let Some(d) = digit(c) {
            any = true;
            match after_point {
                true => {
                    let (held, places) = fraction.unwrap_or((0, 0));
                    fraction = Some((held * 10 + d, places + 1));
                }
                // several digits in a row spell one number: `12` is twelve
                false => digits = Some(digits.unwrap_or(0) * 10 + d),
            }
            continue;
        }
        let Some((by, starts_group)) = place(c) else {
            // anything that is not a numeral means the word is not a number
            return None;
        };
        any = true;
        // a fraction of a place: two tenths of a thousand is two hundred
        let part = match fraction.take() {
            Some((held, places)) => held * by / 10u64.pow(places),
            None => 0,
        };
        after_point = false;
        match starts_group {
            // 만 closes the group below it and multiplies all of it at once:
            // `십만이천오백` is ten times ten thousand, then two thousand five
            // hundred beside it
            true => {
                total += (group + digits.take().unwrap_or(0)) * by;
                group = 0;
            }
            // 십, 백 and 천 multiply the digits before them, or stand for one
            // of themselves where there are none: `십` alone is ten
            false => group += digits.take().unwrap_or(1) * by + part,
        }
    }
    if !any {
        return None;
    }
    let tail = match (digits, fraction) {
        (Some(d), _) => d,
        (None, Some((held, places))) => held / 10u64.pow(places),
        (None, None) => 0,
    };
    Some((total + group + tail).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_places_stack() {
        assert_eq!(of("십").as_deref(), Some("10"));
        assert_eq!(of("오백").as_deref(), Some("500"));
        assert_eq!(of("이천오백").as_deref(), Some("2500"));
    }

    #[test]
    fn a_group_multiplies_everything_under_it() {
        assert_eq!(of("십만이천오백").as_deref(), Some("102500"));
        assert_eq!(of("일억").as_deref(), Some("100000000"));
    }

    #[test]
    fn digits_and_places_read_together() {
        assert_eq!(of("３.２천").as_deref(), Some("3200"));
        assert_eq!(of("12천").as_deref(), Some("12000"));
    }

    #[test]
    fn a_word_that_is_not_a_number_is_left_alone() {
        for word in ["사람", "나무", "뿌리", "과"] {
            assert_eq!(of(word), None, "{word}");
        }
    }

    #[test]
    fn digits_alone_are_already_digits() {
        assert_eq!(of("123"), None, "nothing to rewrite");
    }
}
