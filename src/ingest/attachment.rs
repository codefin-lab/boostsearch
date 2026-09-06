//! The `attachment` processor: a document carried inside a document.
//!
//! A field holds a file -- base64 -- and what comes out is what the file
//! says: its text, how long that text is, what kind of file it was, and what
//! language it is written in. OpenSearch does this with Apache Tika; this
//! reads the formats it can and says so plainly for the ones it cannot.

use serde_json::{Map, Value, json};

/// What a file turned out to be.
pub struct Extracted {
    pub content: String,
    /// how much text there was before it was trimmed, which is what
    /// `content_length` reports
    pub length: usize,
    pub content_type: String,
    /// what the file says about itself
    pub title: Option<String>,
    pub author: Option<String>,
    pub keywords: Option<String>,
    pub date: Option<String>,
}

/// Read a file into its text.
pub fn extract(bytes: &[u8], limit: usize) -> Extracted {
    let mut meta = Meta::default();
    let (mut text, content_type) = match kind_of(bytes) {
        Kind::Docx => {
            meta = docx_meta(bytes);
            (docx_text(bytes).unwrap_or_default(), DOCX.to_string())
        }
        Kind::Doc => {
            meta = doc_meta(bytes);
            (doc_text(bytes).unwrap_or_default(), DOC.to_string())
        }
        Kind::Text => {
            // a plain file ends in a newline whether or not one was written,
            // which is what makes a fifty-three character line fifty-four
            let read = String::from_utf8_lossy(bytes).to_string();
            let charset = if bytes.is_ascii() { "ISO-8859-1" } else { "UTF-8" };
            (format!("{read}\n"), format!("text/plain; charset={charset}"))
        }
        Kind::Unknown => (String::new(), "application/octet-stream".to_string()),
    };
    // `indexed_chars` is a ceiling on how much of a file is read, and it is
    // the length after that ceiling that is reported
    if text.chars().count() > limit {
        text = text.chars().take(limit).collect();
    }
    Extracted {
        content: text.trim().to_string(),
        length: text.chars().count(),
        content_type,
        title: meta.title,
        author: meta.author,
        keywords: meta.keywords,
        date: meta.date,
    }
}

/// What a document says about itself.
#[derive(Default)]
pub struct Meta {
    pub title: Option<String>,
    pub author: Option<String>,
    pub keywords: Option<String>,
    pub date: Option<String>,
}

/// An Open XML document keeps what it says about itself in `docProps/core.xml`,
/// in the Dublin Core vocabulary.
fn docx_meta(bytes: &[u8]) -> Meta {
    let mut meta = Meta::default();
    let Some(xml) = zip_entry(bytes, "docProps/core.xml") else { return meta };
    let xml = String::from_utf8_lossy(&xml);
    let mut reader = quick_xml::Reader::from_str(&xml);
    let mut buf = Vec::new();
    let mut field: Option<String> = None;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(e)) => {
                field = Some(local_name(e.name().as_ref()).to_string());
            }
            Ok(quick_xml::events::Event::End(_)) => field = None,
            Ok(quick_xml::events::Event::Text(t)) => {
                let text = t.xml_content(quick_xml::XmlVersion::Implicit1_0).to_string();
                match field.as_deref() {
                    Some("title") => meta.title = Some(text),
                    Some("creator") => meta.author = Some(text),
                    Some("keywords") => meta.keywords = Some(text),
                    // the date a document carries is the date it was made,
                    // not the date it was last touched
                    Some("created") => meta.date = Some(text),
                    Some("modified") if meta.date.is_none() => meta.date = Some(text),
                    _ => {}
                }
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        buf.clear();
    }
    meta
}

/// What language the text is written in, as the two letters OpenSearch
/// reports.
///
/// The detector answers in ISO 639-3 -- `eng` -- and OpenSearch answers in
/// 639-1 -- `en` -- so the sixty-nine languages it can name are mapped across.
/// A language with no two-letter code keeps its three-letter one, which is
/// what the standard says to do.
pub fn language_of(text: &str) -> Option<String> {
    let found = whatlang::detect(text)?;
    // A dozen words is enough to know a language and three is not: "Test
    // opensearch" comes back as Dutch at a confidence of 0.18, which is
    // noise wearing an answer's clothes. Where the detector says it is not
    // sure, the script is all that is really known, and Latin script is
    // reported as English -- which is both the commonest answer and the one
    // Tika gives, so a document read by both engines is read the same way.
    if !found.is_reliable() {
        return match found.script() {
            whatlang::Script::Latin => Some("en".to_string()),
            _ => {
                let three = found.lang().code();
                Some(two_letter(three).unwrap_or(three).to_string())
            }
        };
    }
    let three = found.lang().code();
    Some(two_letter(three).unwrap_or(three).to_string())
}

fn two_letter(code: &str) -> Option<&'static str> {
    const MAP: &[(&str, &str)] = &[
        ("afr", "af"),
        ("aka", "ak"),
        ("amh", "am"),
        ("ara", "ar"),
        ("aze", "az"),
        ("bel", "be"),
        ("ben", "bn"),
        ("bul", "bg"),
        ("cat", "ca"),
        ("ces", "cs"),
        ("cmn", "zh"),
        ("dan", "da"),
        ("deu", "de"),
        ("ell", "el"),
        ("eng", "en"),
        ("epo", "eo"),
        ("est", "et"),
        ("fin", "fi"),
        ("fra", "fr"),
        ("guj", "gu"),
        ("heb", "he"),
        ("hin", "hi"),
        ("hrv", "hr"),
        ("hun", "hu"),
        ("hye", "hy"),
        ("ind", "id"),
        ("ita", "it"),
        ("jav", "jv"),
        ("jpn", "ja"),
        ("kan", "kn"),
        ("kat", "ka"),
        ("khm", "km"),
        ("kor", "ko"),
        ("lat", "la"),
        ("lav", "lv"),
        ("lit", "lt"),
        ("mal", "ml"),
        ("mar", "mr"),
        ("mkd", "mk"),
        ("mya", "my"),
        ("nep", "ne"),
        ("nld", "nl"),
        ("nob", "nb"),
        ("ori", "or"),
        ("pan", "pa"),
        ("pes", "fa"),
        ("pol", "pl"),
        ("por", "pt"),
        ("ron", "ro"),
        ("rus", "ru"),
        ("sin", "si"),
        ("slk", "sk"),
        ("slv", "sl"),
        ("sna", "sn"),
        ("spa", "es"),
        ("srp", "sr"),
        ("swe", "sv"),
        ("tam", "ta"),
        ("tel", "te"),
        ("tgl", "tl"),
        ("tha", "th"),
        ("tuk", "tk"),
        ("tur", "tr"),
        ("ukr", "uk"),
        ("urd", "ur"),
        ("uzb", "uz"),
        ("vie", "vi"),
        ("yid", "yi"),
        ("zul", "zu"),
    ];
    MAP.iter().find(|(three, _)| *three == code).map(|(_, two)| *two)
}

const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const DOC: &str = "application/msword";

enum Kind {
    Text,
    Doc,
    Docx,
    Unknown,
}

/// What a file is, read from the first bytes of it rather than from its name.
fn kind_of(bytes: &[u8]) -> Kind {
    // a zip, which is what every Open XML document is inside
    if bytes.starts_with(b"PK\x03\x04") {
        return Kind::Docx;
    }
    // the OLE2 compound file the older Office formats are written in
    if bytes.starts_with(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]) {
        return Kind::Doc;
    }
    // text is what has no byte in it that text does not have
    let printable = bytes.iter().all(|b| *b >= 0x20 || matches!(*b, b'\n' | b'\r' | b'\t' | 0x0c));
    if printable && std::str::from_utf8(bytes).is_ok() {
        return Kind::Text;
    }
    Kind::Unknown
}

/// The text of a Word document written in Open XML.
///
/// An Open XML file is a zip holding `word/document.xml`, and the text of
/// that document is what its `w:t` elements hold, one paragraph per `w:p`.
fn docx_text(bytes: &[u8]) -> Option<String> {
    let xml = zip_entry(bytes, "word/document.xml")?;
    let xml = String::from_utf8_lossy(&xml);
    let mut reader = quick_xml::Reader::from_str(&xml);
    let mut out = String::new();
    let mut in_text = false;
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(e)) => match local_name(e.name().as_ref()) {
                "t" => in_text = true,
                _ => {}
            },
            Ok(quick_xml::events::Event::End(e)) => match local_name(e.name().as_ref()) {
                "t" => in_text = false,
                // a paragraph is a line, and a line break is one too
                "p" => out.push('\n'),
                _ => {}
            },
            Ok(quick_xml::events::Event::Empty(e)) => {
                if local_name(e.name().as_ref()) == "br" {
                    out.push('\n');
                }
            }
            Ok(quick_xml::events::Event::Text(t)) if in_text => {
                out.push_str(&t.xml_content(quick_xml::XmlVersion::Implicit1_0));
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        buf.clear();
    }
    Some(out)
}

/// `w:t` is a `t` in the `w` namespace; what matters here is the `t`.
fn local_name(name: &str) -> &str {
    match name.rsplit_once(':') {
        Some((_, local)) => local,
        None => name,
    }
}

/// One file out of a zip, by name, read through its central directory.
///
/// Written here rather than taken from a crate: an Open XML document is the
/// only zip this engine opens, and it opens one file out of it.
fn zip_entry(bytes: &[u8], want: &str) -> Option<Vec<u8>> {
    let u16_at = |at: usize| -> Option<usize> {
        Some(u16::from_le_bytes([*bytes.get(at)?, *bytes.get(at + 1)?]) as usize)
    };
    let u32_at = |at: usize| -> Option<usize> {
        Some(u32::from_le_bytes([
            *bytes.get(at)?,
            *bytes.get(at + 1)?,
            *bytes.get(at + 2)?,
            *bytes.get(at + 3)?,
        ]) as usize)
    };
    // the end of the directory is at the end of the file, behind a comment
    // whose length nobody records anywhere else
    let eocd = (0..bytes.len().saturating_sub(21))
        .rev()
        .find(|at| bytes[*at..].starts_with(b"PK\x05\x06"))?;
    let count = u16_at(eocd + 10)?;
    let mut at = u32_at(eocd + 16)?;
    for _ in 0..count {
        if !bytes.get(at..)?.starts_with(b"PK\x01\x02") {
            return None;
        }
        let method = u16_at(at + 10)?;
        let compressed = u32_at(at + 20)?;
        let name_len = u16_at(at + 28)?;
        let extra_len = u16_at(at + 30)?;
        let comment_len = u16_at(at + 32)?;
        let local = u32_at(at + 42)?;
        let name = bytes.get(at + 46..at + 46 + name_len)?;
        if name == want.as_bytes() {
            // the local header repeats the name and the extra field, and may
            // disagree with the directory about how long they are
            let local_name_len = u16_at(local + 26)?;
            let local_extra_len = u16_at(local + 28)?;
            let start = local + 30 + local_name_len + local_extra_len;
            let data = bytes.get(start..start + compressed)?;
            return match method {
                0 => Some(data.to_vec()),
                8 => {
                    use std::io::Read;
                    let mut out = Vec::new();
                    flate2::read::DeflateDecoder::new(data).read_to_end(&mut out).ok()?;
                    Some(out)
                }
                _ => None,
            };
        }
        at += 46 + name_len + extra_len + comment_len;
    }
    None
}

/// What an older Office document says about itself, out of the property set
/// every one of them carries.
///
/// `\x05SummaryInformation` is a property set: a header saying where the
/// section starts, then pairs of property id and offset, then the values.
/// Only four of them are of interest here.
fn doc_meta(bytes: &[u8]) -> Meta {
    let mut meta = Meta::default();
    let Ok(mut file) = cfb::CompoundFile::open(std::io::Cursor::new(bytes.to_vec())) else {
        return meta;
    };
    let Some(stream) = read_stream(&mut file, "/\u{5}SummaryInformation") else { return meta };
    let u32_at = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes([
            *stream.get(at)?,
            *stream.get(at + 1)?,
            *stream.get(at + 2)?,
            *stream.get(at + 3)?,
        ]))
    };
    // the first section begins where the header says it does
    let Some(section) = u32_at(44).map(|v| v as usize) else { return meta };
    let Some(count) = u32_at(section + 4).map(|v| v as usize) else { return meta };
    for i in 0..count {
        let Some(id) = u32_at(section + 8 + i * 8) else { continue };
        let Some(offset) = u32_at(section + 12 + i * 8).map(|v| v as usize) else { continue };
        let at = section + offset;
        let Some(kind) = u32_at(at) else { continue };
        match (id, kind) {
            // a string property: its length, then its bytes
            (2 | 4 | 5, 0x1E) => {
                let Some(len) = u32_at(at + 4).map(|v| v as usize) else { continue };
                let Some(raw) = stream.get(at + 8..at + 8 + len) else { continue };
                // the length counts the terminator, and the value is padded
                // to a four-byte boundary after it: the string is what stands
                // before the first of those zeroes
                let raw = match raw.iter().position(|b| *b == 0) {
                    Some(end) => &raw[..end],
                    None => raw,
                };
                let text: String = raw.iter().map(|b| cp1252(*b)).collect();
                // a property a document does not set is padding, not an
                // empty answer
                if text.trim().is_empty() {
                    continue;
                }
                match id {
                    2 => meta.title = Some(text),
                    4 => meta.author = Some(text),
                    5 => meta.keywords = Some(text),
                    _ => {}
                }
            }
            // when the document was made, as a Windows filetime
            (12, 0x40) => {
                let (Some(low), Some(high)) = (u32_at(at + 4), u32_at(at + 8)) else { continue };
                let filetime = ((high as u64) << 32) | low as u64;
                meta.date = filetime_to_iso(filetime);
            }
            _ => {}
        }
    }
    meta
}

/// A Windows filetime is the hundreds of nanoseconds since 1601; a date is
/// written the way every other date in an answer is written.
fn filetime_to_iso(filetime: u64) -> Option<String> {
    // 1601-01-01 to 1970-01-01, in seconds
    const EPOCH_DIFFERENCE: i64 = 11_644_473_600;
    let seconds = (filetime / 10_000_000) as i64 - EPOCH_DIFFERENCE;
    crate::store::format_millis(seconds * 1000, "yyyy-MM-dd'T'HH:mm:ss'Z'")
}

/// The text of a Word document written in the older binary format.
///
/// A `.doc` is an OLE2 compound file. The `WordDocument` stream begins with
/// the FIB, which says where the text starts, how the file is laid out, and
/// which of the two table streams holds the piece table. The piece table says
/// where each run of text really is and whether it was written as one byte a
/// character or two -- Word does not keep the text in one place, and reading
/// it from `fcMin` to `fcMac` is only right by accident.
fn doc_text(bytes: &[u8]) -> Option<String> {
    let mut file = cfb::CompoundFile::open(std::io::Cursor::new(bytes.to_vec())).ok()?;
    let word = read_stream(&mut file, "WordDocument")?;
    // the FIB: `fWhichTblStm` (bit 9 of the flags at 0x000A) says which of the
    // two table streams this document's piece table is in
    let flags = u16::from_le_bytes([*word.get(0x0A)?, *word.get(0x0B)?]);
    let table_name = if flags & 0x0200 != 0 { "1Table" } else { "0Table" };
    let table = read_stream(&mut file, table_name)?;
    // where the piece table sits inside the table stream. The FIB grew over
    // the years; `fcClx`/`lcbClx` are the 33rd pair of the fibRgFcLcb97 array,
    // which begins at 0x01A2
    let fc_clx = u32::from_le_bytes([
        *table_at(&word, 0x01A2)?,
        *table_at(&word, 0x01A3)?,
        *table_at(&word, 0x01A4)?,
        *table_at(&word, 0x01A5)?,
    ]) as usize;
    let lcb_clx = u32::from_le_bytes([
        *table_at(&word, 0x01A6)?,
        *table_at(&word, 0x01A7)?,
        *table_at(&word, 0x01A8)?,
        *table_at(&word, 0x01A9)?,
    ]) as usize;
    let clx = table.get(fc_clx..fc_clx + lcb_clx)?;
    let pieces = piece_table(clx)?;
    let mut out = String::new();
    for (start, end, compressed) in pieces {
        if compressed {
            // one byte a character, in the Windows Latin-1 code page
            let run = word.get(start..end)?;
            out.extend(run.iter().map(|b| cp1252(*b)));
        } else {
            let run = word.get(start..end)?;
            for pair in run.chunks_exact(2) {
                if let Some(c) = char::from_u32(u16::from_le_bytes([pair[0], pair[1]]) as u32) {
                    out.push(c);
                }
            }
        }
    }
    // Word writes a paragraph end as a carriage return, and a table cell end
    // as a bell; neither is text
    Some(
        out.chars()
            .map(|c| match c {
                '\r' | '\u{7}' | '\u{b}' => '\n',
                other => other,
            })
            .collect(),
    )
}

fn table_at(word: &[u8], at: usize) -> Option<&u8> {
    word.get(at)
}

fn read_stream(
    file: &mut cfb::CompoundFile<std::io::Cursor<Vec<u8>>>,
    name: &str,
) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut stream = file.open_stream(name).ok()?;
    let mut out = Vec::new();
    stream.read_to_end(&mut out).ok()?;
    Some(out)
}

/// The runs of text a document is really made of, as byte ranges into the
/// `WordDocument` stream, each saying whether it was written one byte a
/// character or two.
fn piece_table(clx: &[u8]) -> Option<Vec<(usize, usize, bool)>> {
    // the Clx is a run of Prc structures followed by one Pcdt, which is what
    // is wanted; a Prc says how long it is, so they can be stepped over
    let mut at = 0usize;
    while *clx.get(at)? == 0x01 {
        let len = u16::from_le_bytes([*clx.get(at + 1)?, *clx.get(at + 2)?]) as usize;
        at += 3 + len;
    }
    if *clx.get(at)? != 0x02 {
        return None;
    }
    let lcb = u32::from_le_bytes([
        *clx.get(at + 1)?,
        *clx.get(at + 2)?,
        *clx.get(at + 3)?,
        *clx.get(at + 4)?,
    ]) as usize;
    let plc = clx.get(at + 5..at + 5 + lcb)?;
    // a PLC is n+1 character positions followed by n pieces of eight bytes
    let n = (lcb - 4) / 12;
    let mut out = Vec::new();
    for i in 0..n {
        let cp = |k: usize| -> Option<usize> {
            Some(u32::from_le_bytes([
                *plc.get(k * 4)?,
                *plc.get(k * 4 + 1)?,
                *plc.get(k * 4 + 2)?,
                *plc.get(k * 4 + 3)?,
            ]) as usize)
        };
        let (from, to) = (cp(i)?, cp(i + 1)?);
        let pcd = (n + 1) * 4 + i * 8;
        let fc = u32::from_le_bytes([
            *plc.get(pcd + 2)?,
            *plc.get(pcd + 3)?,
            *plc.get(pcd + 4)?,
            *plc.get(pcd + 5)?,
        ]);
        // the top bit says the run is one byte a character, and the address
        // is then twice what it appears to be
        let compressed = fc & 0x4000_0000 != 0;
        let start = if compressed { (fc & 0x3FFF_FFFF) as usize / 2 } else { fc as usize };
        let length = to - from;
        let end = start + if compressed { length } else { length * 2 };
        out.push((start, end, compressed));
    }
    Some(out)
}

/// The characters Windows Latin-1 has where Latin-1 itself has none.
fn cp1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '\u{20AC}', '\u{81}', '\u{201A}', '\u{192}', '\u{201E}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{2C6}', '\u{2030}', '\u{160}', '\u{2039}', '\u{152}', '\u{8D}', '\u{17D}',
        '\u{8F}', '\u{90}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}',
        '\u{2014}', '\u{2DC}', '\u{2122}', '\u{161}', '\u{203A}', '\u{153}', '\u{9D}', '\u{17E}',
        '\u{178}',
    ];
    match b {
        0x80..=0x9F => HIGH[(b - 0x80) as usize],
        other => other as char,
    }
}

/// What the processor writes, given what the file turned out to be.
pub fn fields(found: &Extracted, text: &str, wanted: Option<&[String]>) -> Value {
    // everything the processor can write, in the order OpenSearch names them.
    // Only what the file actually said is written: a document with no author
    // has no `author` field rather than an empty one
    let all = [
        "content",
        "title",
        "author",
        "keywords",
        "date",
        "content_type",
        "content_length",
        "language",
    ];
    let take: Vec<String> = match wanted {
        Some(w) => w.to_vec(),
        None => all.iter().map(|s| s.to_string()).collect(),
    };
    let mut out = Map::new();
    for p in &take {
        match p.as_str() {
            "content" if !found.content.is_empty() => {
                out.insert("content".into(), json!(found.content));
            }
            "language" => {
                if let Some(lang) = language_of(text) {
                    out.insert("language".into(), json!(lang));
                }
            }
            "content_length" => {
                out.insert("content_length".into(), json!(found.length));
            }
            "content_type" => {
                out.insert("content_type".into(), json!(found.content_type));
            }
            "title" => {
                if let Some(v) = &found.title {
                    out.insert("title".into(), json!(v));
                }
            }
            "author" => {
                if let Some(v) = &found.author {
                    out.insert("author".into(), json!(v));
                }
            }
            "keywords" => {
                if let Some(v) = &found.keywords {
                    out.insert("keywords".into(), json!(v));
                }
            }
            "date" => {
                if let Some(v) = &found.date {
                    out.insert("date".into(), json!(v));
                }
            }
            _ => {}
        }
    }
    Value::Object(out)
}
