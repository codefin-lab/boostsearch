//! What character set a text file is written in, decided the way Tika
//! decides it, and the text read in it.
//!
//! Tika asks three detectors in turn: a byte-order mark, a `<meta charset>`
//! in the first eight kilobytes, and juniversalchardet over the first
//! sixteen. The last never names ASCII -- there is nothing in ASCII for it to
//! recognise -- and Tika's wrapper answers ISO-8859-1 for text that is mostly
//! ASCII, which is why an English text file is reported as Latin-1.

use super::Failure;

/// How often each byte value occurs: Tika's `TextStatistics`.
pub struct Stats {
    counts: [usize; 256],
    total: usize,
}

impl Stats {
    pub fn of(bytes: &[u8]) -> Stats {
        let mut counts = [0usize; 256];
        for b in bytes {
            counts[*b as usize] += 1;
        }
        Stats { counts, total: bytes.len() }
    }

    fn count(&self, from: usize, to: usize) -> usize {
        self.counts[from..to].iter().sum()
    }

    /// Tab, newline, form feed, carriage return and escape: the control
    /// characters text has.
    fn safe_control(&self) -> usize {
        [b'\t', b'\n', 0x0c, b'\r', 0x1b].iter().map(|b| self.counts[*b as usize]).sum()
    }

    pub fn mostly_ascii(&self) -> bool {
        let control = self.count(0, 0x20);
        let ascii = self.count(0x20, 128);
        let safe = self.safe_control();
        self.total > 0
            && (control - safe) * 100 < self.total * 2
            && (ascii + safe) * 100 > self.total * 90
    }

    pub fn looks_like_utf8(&self) -> bool {
        let control = self.count(0, 0x20);
        let mut utf8 = self.count(0x20, 0x80);
        let safe = self.safe_control();
        let mut expected = 0usize;
        for (i, (from, to)) in [(0xc0, 0xe0), (0xe0, 0xf0), (0xf0, 0xf8)].iter().enumerate() {
            let leading = self.count(*from, *to);
            utf8 += leading;
            expected += (i + 1) * leading;
        }
        let continuation = self.count(0x80, 0xc0);
        utf8 > 0
            && continuation <= expected
            && continuation + 3 >= expected
            && self.count(0xf8, 0x100) == 0
            && control.saturating_sub(safe) * 100 < utf8 * 2
    }
}

/// Read text in the character set Tika would pick for it, and name that
/// character set as Java does.
pub fn decode_text(bytes: &[u8], hint: Option<&str>) -> Result<(String, String), Failure> {
    let name = detect(bytes, hint)
        .ok_or_else(|| Failure::tika("Failed to detect the character encoding of a document"))?;
    Ok((decode(bytes, &name), name))
}

fn detect(bytes: &[u8], hint: Option<&str>) -> Option<String> {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Some("UTF-8".into());
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return Some("UTF-16BE".into());
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return Some("UTF-16LE".into());
    }
    if let Some(named) = meta_charset(&bytes[..bytes.len().min(8192)]) {
        return Some(named);
    }
    if let Some(h) = hint.and_then(canonical) {
        return Some(h);
    }
    universal(&bytes[..bytes.len().min(16 * 1024)])
}

/// juniversalchardet as Tika wraps it. The detector itself is a set of
/// statistical probers; what is kept here is what decides the common cases:
/// valid UTF-8 is UTF-8, other high bytes are the Windows Latin-1 family, and
/// nothing but ASCII is Tika's ISO-8859-1.
fn universal(bytes: &[u8]) -> Option<String> {
    let stats = Stats::of(bytes);
    if bytes.iter().any(|b| *b >= 0x80) {
        if valid_utf8_prefix(bytes) {
            return Some("UTF-8".into());
        }
        // the detector says windows-1252, and Tika second-guesses it: a file
        // with no carriage returns was more likely written on a system whose
        // Latin-1 is the ISO one, and one with a currency sign in it more
        // likely in the ISO variant that has the euro there
        if stats.counts[b'\r' as usize] > 0 {
            return Some("windows-1252".into());
        }
        if stats.counts[0xa4] > 0 {
            return Some("ISO-8859-15".into());
        }
        return Some("ISO-8859-1".into());
    }
    if stats.mostly_ascii() { Some("ISO-8859-1".into()) } else { None }
}

/// UTF-8, allowing the last character to be cut short where the detector
/// stopped reading.
fn valid_utf8_prefix(bytes: &[u8]) -> bool {
    match std::str::from_utf8(bytes) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none() && bytes.len() - e.valid_up_to() < 4,
    }
}

/// A `charset` named in a `<meta>` tag, as Tika's HTML encoding detector
/// finds it: anywhere in the attributes of any meta element.
fn meta_charset(head: &[u8]) -> Option<String> {
    let text: String = head.iter().map(|b| *b as char).collect();
    let lower = text.to_ascii_lowercase();
    let mut from = 0;
    while let Some(at) = lower[from..].find("<meta") {
        let start = from + at + 5;
        let end = lower[start..].find(['<', '>']).map(|e| start + e).unwrap_or(lower.len());
        let attrs = &lower[start..end];
        from = start;
        let Some(cs) = attrs.find("charset") else { continue };
        let rest = attrs[cs + 7..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else { continue };
        let rest = rest.trim_start().trim_start_matches(['"', '\'']).trim_start();
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
            .collect();
        if name.is_empty() {
            continue;
        }
        // HTML says a page cannot really be UTF-16 if its meta tag could be
        // read as ASCII, and a user-defined charset is Windows Latin-1
        if name.starts_with("utf-16") || name.starts_with("utf-32") {
            return Some("UTF-8".into());
        }
        if name == "x-user-defined" {
            return Some("windows-1252".into());
        }
        if let Some(c) = canonical(&name) {
            return Some(c);
        }
    }
    None
}

/// A character set's name as Java spells it, for the sets it can decode.
pub fn canonical(label: &str) -> Option<String> {
    let l = label.trim().to_ascii_lowercase();
    let named = match l.as_str() {
        "utf-8" | "utf8" | "unicode-1-1-utf-8" => "UTF-8",
        "iso-8859-1" | "iso8859-1" | "iso_8859-1" | "latin1" | "l1" | "8859_1" => "ISO-8859-1",
        "us-ascii" | "ascii" | "iso646-us" => "US-ASCII",
        "utf-16" => "UTF-16",
        "utf-16le" => "UTF-16LE",
        "utf-16be" => "UTF-16BE",
        "windows-874" | "cp874" => "x-windows-874",
        "tis-620" | "tis620" => "TIS-620",
        "shift_jis" | "shift-jis" | "sjis" => "Shift_JIS",
        "windows-31j" | "ms932" => "windows-31j",
        "euc-jp" => "EUC-JP",
        "iso-2022-jp" => "ISO-2022-JP",
        "euc-kr" => "EUC-KR",
        "gb2312" => "GB2312",
        "gbk" => "GBK",
        "gb18030" => "GB18030",
        "big5" => "Big5",
        "koi8-r" => "KOI8-R",
        "koi8-u" => "KOI8-U",
        _ => {
            if let Some(n) = l.strip_prefix("windows-").or_else(|| l.strip_prefix("cp"))
                && ["1250", "1251", "1252", "1253", "1254", "1255", "1256", "1257", "1258"]
                    .contains(&n)
            {
                return Some(format!("windows-{n}"));
            }
            if let Some(n) = l.strip_prefix("iso-8859-").or_else(|| l.strip_prefix("iso8859-"))
                && n.parse::<u8>().is_ok_and(|n| (2..=16).contains(&n) && n != 12)
            {
                return Some(format!("ISO-8859-{n}"));
            }
            return None;
        }
    };
    Some(named.to_string())
}

/// Bytes read in a named character set, a byte-order mark dropped.
pub fn decode(bytes: &[u8], name: &str) -> String {
    match name {
        "ISO-8859-1" => bytes.iter().map(|b| *b as char).collect(),
        "US-ASCII" => {
            bytes.iter().map(|b| if *b < 0x80 { *b as char } else { '\u{FFFD}' }).collect()
        }
        "UTF-8" => {
            let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
            String::from_utf8_lossy(body).into_owned()
        }
        _ => {
            let label = match name {
                "x-windows-874" | "TIS-620" => "windows-874",
                "windows-31j" => "shift_jis",
                "UTF-16" => "utf-16be",
                other => other,
            };
            match encoding_rs::Encoding::for_label(label.as_bytes()) {
                Some(enc) => enc.decode(bytes).0.into_owned(),
                None => String::from_utf8_lossy(bytes).into_owned(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_latin1_and_utf8_is_utf8() {
        assert_eq!(decode_text(b"plain words", None).unwrap().1, "ISO-8859-1");
        assert_eq!(decode_text("ภาษาไทย".as_bytes(), None).unwrap().1, "UTF-8");
        assert_eq!(decode_text(b"caf\xe9 au lait", None).unwrap().1, "ISO-8859-1");
        assert_eq!(decode_text(b"caf\xe9\r\n", None).unwrap().1, "windows-1252");
    }

    #[test]
    fn a_meta_tag_names_the_charset() {
        let page = b"<html><head><meta charset=\"utf-8\"></head><body>hi</body></html>";
        assert_eq!(decode_text(page, None).unwrap().1, "UTF-8");
        let page =
            b"<meta http-equiv=\"Content-Type\" content=\"text/html; charset=windows-1252\">";
        assert_eq!(decode_text(page, None).unwrap().1, "windows-1252");
    }
}
