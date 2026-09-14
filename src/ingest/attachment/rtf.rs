//! RTF, read as Tika's RTF text extractor reads it.
//!
//! An RTF file is groups in braces and control words behind backslashes;
//! the text is what is left once the control words are acted on and the
//! destinations that are not text -- the font table, the stylesheet, a
//! picture, anything marked `\*` -- are skipped. Characters come three ways:
//! as themselves, as `\'hh` in the document's code page (or the current
//! font's), and as `\uN` followed by a fallback the reader skips.
//!
//! Tika keeps a paragraph open from the start of the document: `\par` closes
//! it and opens the next, and the end of the document closes the last one.
//! That is why a file whose text ends in `\par` has two newlines after it.

use super::{Failure, Meta, Text};

#[derive(Clone)]
struct Group {
    /// a destination whose text is not document text
    skip: bool,
    /// characters to skip after a `\uN`
    uc: usize,
    /// the code page `\'hh` bytes are read in
    codepage: u16,
    /// which piece of document information this group holds, if any
    info: Option<&'static str>,
    in_fonttbl: bool,
    /// inside `\creatim`, whose `\yr` and the rest are the creation date
    creatim: bool,
}

pub fn parse(bytes: &[u8], text: &mut Text) -> Result<Meta, Failure> {
    let mut meta = Meta::default();
    let mut stack: Vec<Group> = Vec::new();
    let mut g =
        Group { skip: false, uc: 1, codepage: 1252, info: None, in_fonttbl: false, creatim: false };
    let mut document_codepage = 1252u16;
    // font number to code page, from the font table
    let mut fonts: Vec<(i32, u16)> = Vec::new();
    let mut font_being_defined: Option<i32> = None;
    let mut pending: Vec<u8> = Vec::new();
    let mut skip_after_unicode = 0usize;
    let mut info_text = String::new();
    // the parts of `\creatim`
    let mut created: [Option<i32>; 6] = [None; 6];
    let mut group_start = false;
    let mut i = 0usize;
    let mut out = String::new();

    macro_rules! flush {
        () => {
            if !pending.is_empty() {
                let decoded = decode(&pending, g.codepage);
                pending.clear();
                if g.info.is_some() {
                    info_text.push_str(&decoded);
                } else if !g.skip {
                    out.push_str(&decoded);
                }
            }
        };
    }
    macro_rules! emit {
        ($s:expr) => {
            flush!();
            if g.info.is_some() {
                info_text.push_str($s);
            } else if !g.skip {
                out.push_str($s);
            }
        };
    }

    while i < bytes.len() {
        if text.full() {
            break;
        }
        let b = bytes[i];
        match b {
            b'{' => {
                flush!();
                if stack.len() > 1024 {
                    return Err(Failure::tika("RTF groups nested too deeply"));
                }
                stack.push(g.clone());
                group_start = true;
                i += 1;
                continue;
            }
            b'}' => {
                flush!();
                let leaving = g.clone();
                if let Some(field) = leaving.info {
                    let value = info_text.trim().to_string();
                    info_text.clear();
                    if !value.is_empty() {
                        match field {
                            "title" => meta.title = Some(value),
                            "author" => meta.author = Some(value),
                            "keywords" => meta.keywords = Some(value),
                            _ => {}
                        }
                    }
                }
                g = stack.pop().unwrap_or(g);
                skip_after_unicode = 0;
                i += 1;
            }
            b'\\' => {
                let Some(&next) = bytes.get(i + 1) else { break };
                if next.is_ascii_alphabetic() {
                    let start = i + 1;
                    let mut j = start;
                    while j < bytes.len() && bytes[j].is_ascii_alphabetic() && j - start < 32 {
                        j += 1;
                    }
                    let word = std::str::from_utf8(&bytes[start..j]).unwrap_or("");
                    let num_start = j;
                    if j < bytes.len() && bytes[j] == b'-' {
                        j += 1;
                    }
                    while j < bytes.len() && bytes[j].is_ascii_digit() && j - num_start < 10 {
                        j += 1;
                    }
                    let param: Option<i32> =
                        std::str::from_utf8(&bytes[num_start..j]).ok().and_then(|s| s.parse().ok());
                    if j < bytes.len() && bytes[j] == b' ' {
                        j += 1;
                    }
                    i = j;
                    let at_group_start = group_start;
                    group_start = false;
                    if word != "u" && word != "bin" {
                        // the fallback after a `\u` is counted in characters,
                        // and a control word is one
                        if skip_after_unicode > 0 {
                            skip_after_unicode -= 1;
                            continue;
                        }
                    }
                    match word {
                        "fonttbl" => {
                            g.skip = true;
                            g.in_fonttbl = true;
                        }
                        "colortbl" | "stylesheet" | "listtable" | "listoverridetable"
                        | "revtbl" | "rsidtbl" | "generator" | "pict" | "themedata"
                        | "colorschememapping" | "datastore" | "latentstyles" | "xmlnstbl"
                        | "mmathPr" | "pgdsctbl" | "filetbl" | "fldinst" | "objdata"
                        | "private" | "operator" => g.skip = true,
                        "info" => g.skip = true,
                        "title" | "author" | "keywords" if at_group_start || g.skip => {
                            flush!();
                            g.info = Some(match word {
                                "title" => "title",
                                "author" => "author",
                                _ => "keywords",
                            });
                            info_text.clear();
                        }
                        "creatim" => {
                            g.skip = true;
                            g.creatim = true;
                            created = [None; 6];
                        }
                        "yr" | "mo" | "dy" | "hr" | "min" | "sec" if g.creatim => {
                            let slot = ["yr", "mo", "dy", "hr", "min", "sec"]
                                .iter()
                                .position(|w| *w == word)
                                .unwrap_or(0);
                            created[slot] = param;
                            if let (Some(y), Some(mo), Some(d)) =
                                (created[0], created[1], created[2])
                                && y > 0
                            {
                                meta.date = Some(format!(
                                    "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z",
                                    created[3].unwrap_or(0),
                                    created[4].unwrap_or(0),
                                    0
                                ));
                            }
                        }
                        "ansi" => document_codepage = 1252,
                        "mac" => document_codepage = 10000,
                        "pc" => document_codepage = 437,
                        "pca" => document_codepage = 850,
                        "ansicpg" => {
                            if let Some(p) = param.filter(|p| *p > 0 && *p < 65536) {
                                document_codepage = p as u16;
                                g.codepage = p as u16;
                            }
                        }
                        "f" => {
                            flush!();
                            if g.in_fonttbl {
                                font_being_defined = param;
                            } else if let Some(n) = param {
                                g.codepage = fonts
                                    .iter()
                                    .find(|(f, _)| *f == n)
                                    .map(|(_, cp)| *cp)
                                    .unwrap_or(document_codepage);
                            }
                        }
                        "fcharset" if g.in_fonttbl => {
                            if let (Some(f), Some(cs)) = (font_being_defined, param) {
                                let cp = match cs {
                                    128 => 932,
                                    129 => 949,
                                    134 => 936,
                                    136 => 950,
                                    161 => 1253,
                                    162 => 1254,
                                    163 => 1258,
                                    177 => 1255,
                                    178 => 1256,
                                    186 => 1257,
                                    204 => 1251,
                                    222 => 874,
                                    238 => 1250,
                                    _ => document_codepage,
                                };
                                fonts.push((f, cp));
                            }
                        }
                        "uc" => g.uc = param.unwrap_or(1).max(0) as usize,
                        "u" => {
                            if let Some(n) = param {
                                let unit = if n < 0 { (n + 65536) as u32 } else { n as u32 };
                                let c = char::from_u32(unit).unwrap_or('\u{FFFD}');
                                emit!(&c.to_string());
                                skip_after_unicode = g.uc;
                            }
                        }
                        "bin" => {
                            let n = param.unwrap_or(0).max(0) as usize;
                            i = i.saturating_add(n).min(bytes.len());
                        }
                        "par" | "sect" | "row" => {
                            flush!();
                            if !g.skip && g.info.is_none() {
                                text.chars(&out);
                                out.clear();
                                text.start("p");
                                text.end("p");
                            }
                        }
                        "line" => {
                            emit!("\n");
                        }
                        "tab" | "cell" => {
                            emit!("\t");
                        }
                        "emdash" => {
                            emit!("\u{2014}");
                        }
                        "endash" => {
                            emit!("\u{2013}");
                        }
                        "emspace" => {
                            emit!("\u{2003}");
                        }
                        "enspace" => {
                            emit!("\u{2002}");
                        }
                        "bullet" => {
                            emit!("\u{2022}");
                        }
                        "lquote" => {
                            emit!("\u{2018}");
                        }
                        "rquote" => {
                            emit!("\u{2019}");
                        }
                        "ldblquote" => {
                            emit!("\u{201C}");
                        }
                        "rdblquote" => {
                            emit!("\u{201D}");
                        }
                        _ => {}
                    }
                    continue;
                }
                group_start = false;
                i += 2;
                if skip_after_unicode > 0 && next != b'\'' {
                    skip_after_unicode -= 1;
                    continue;
                }
                match next {
                    b'\'' => {
                        let hex = bytes.get(i..i + 2).and_then(|h| std::str::from_utf8(h).ok());
                        if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                            i += 2;
                            if skip_after_unicode > 0 {
                                skip_after_unicode -= 1;
                            } else if g.info.is_some() || !g.skip {
                                pending.push(v);
                            }
                        }
                    }
                    b'*' => g.skip = true,
                    b'\\' | b'{' | b'}' => {
                        emit!(&(next as char).to_string());
                    }
                    b'~' => {
                        emit!("\u{a0}");
                    }
                    b'_' => {
                        emit!("\u{2011}");
                    }
                    b'\n' | b'\r' => {
                        flush!();
                        if !g.skip && g.info.is_none() {
                            text.chars(&out);
                            out.clear();
                            text.start("p");
                            text.end("p");
                        }
                    }
                    _ => {}
                }
            }
            b'\r' | b'\n' => i += 1,
            _ => {
                group_start = false;
                i += 1;
                if skip_after_unicode > 0 {
                    skip_after_unicode -= 1;
                    continue;
                }
                if b >= 0x80 {
                    if g.info.is_some() || !g.skip {
                        pending.push(b);
                    }
                    continue;
                }
                if g.in_fonttbl && b == b';' {
                    font_being_defined = None;
                }
                emit!(&(b as char).to_string());
            }
        }
        if out.len() > 4096 {
            text.chars(&out);
            out.clear();
        }
    }
    flush!();
    text.chars(&out);
    // the paragraph still open at the end of the document
    text.start("p");
    text.end("p");
    Ok(meta)
}

/// Bytes in a Windows code page.
fn decode(bytes: &[u8], codepage: u16) -> String {
    let label = match codepage {
        1252 => return bytes.iter().map(|b| super::office::cp1252(*b)).collect(),
        10000 => "macintosh",
        437 | 850 => return bytes.iter().map(|b| super::office::cp1252(*b)).collect(),
        874 => "windows-874",
        932 => "shift_jis",
        936 => "gbk",
        949 => "euc-kr",
        950 => "big5",
        1250..=1258 => {
            return super::charset::decode(bytes, &format!("windows-{codepage}"));
        }
        _ => return bytes.iter().map(|b| super::office::cp1252(*b)).collect(),
    };
    match encoding_rs::Encoding::for_label(label.as_bytes()) {
        Some(enc) => enc.decode_without_bom_handling(bytes).0.into_owned(),
        None => String::from_utf8_lossy(bytes).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(rtf: &[u8]) -> (String, Meta) {
        let mut text = Text::new(None);
        let meta = parse(rtf, &mut text).unwrap();
        (text.out, meta)
    }

    #[test]
    fn the_reference_example() {
        let (text, _) = read(b"{\\rtf1\\ansi\r\nLorem ipsum dolor sit amet\r\n\\par }");
        assert_eq!(text, "Lorem ipsum dolor sit amet\n\n");
        assert_eq!(text.chars().count(), 28);
    }

    #[test]
    fn unicode_escapes_code_pages_and_skipped_destinations() {
        let rtf = b"{\\rtf1\\ansi\\ansicpg1252\\uc1{\\fonttbl{\\f0\\fswiss Arial;}}{\\colortbl;\\red0;}\
                    {\\info{\\title Report}{\\author Priya Nair}{\\creatim\\yr2026\\mo3\\dy2\\hr9\\min5}}\
                    {\\*\\generator Writer}\\f0 caf\\'e9 \\u3616?\\u3634?\\u3625?\\u3634?\\u3652?\\u3607?\\u3618?\\par second\\tab line}";
        let (text, meta) = read(rtf);
        assert_eq!(
            text,
            "caf\u{e9} \u{e20}\u{e32}\u{e29}\u{e32}\u{e44}\u{e17}\u{e22}\nsecond\tline\n"
        );
        assert_eq!(meta.title.as_deref(), Some("Report"));
        assert_eq!(meta.author.as_deref(), Some("Priya Nair"));
        assert_eq!(meta.date.as_deref(), Some("2026-03-02T09:05:00Z"));
    }
}
