//! The `attachment` processor: a document carried inside a document.
//!
//! A field holds a file -- base64 -- and what comes out is what the file
//! says: its text, how long that text is, what kind of file it was, and what
//! language it is written in. OpenSearch does this with Apache Tika, limited
//! to the parsers its plugin lists: HTML, PDF, plain text, RTF, the Office
//! formats old and new, OpenDocument, iWork, XML and EPUB. A file of any other
//! kind is still named -- an image is `image/png` -- but has no text, which is
//! what the reference answers for it too.
//!
//! Tika does not hand text straight back. Every parser writes XHTML, and the
//! text is what is left of that XHTML once the tags are gone, with a newline
//! after each block element and a tab before each cell. That is where the
//! newline at the end of a plain text file comes from, and why a table reads
//! the way it does. [`Text`] is that writer: the parsers here say which
//! elements they open and close, and the whitespace falls out the same way.

mod charset;
mod detect;
mod html;
mod lang;
mod office;
mod pdf;
mod rtf;
mod xml;

use serde_json::{Map, Value, json};

pub use lang::language_of;

/// Everything the processor can write, in the order OpenSearch names them.
pub const PROPERTIES: &[&str] = &[
    "content",
    "title",
    "author",
    "keywords",
    "date",
    "content_type",
    "content_length",
    "language",
];

/// What a file turned out to be.
#[derive(Debug, Default)]
pub struct Extracted {
    /// the text as the parser wrote it, cut at `indexed_chars` and not yet
    /// trimmed: the language is read from this, and its length reported
    pub text: String,
    /// how long that text is, counted the way Java counts a string: in
    /// UTF-16 code units, so a character outside the Basic Multilingual
    /// Plane counts twice
    pub length: usize,
    pub content_type: String,
    pub meta: Meta,
}

/// What a document says about itself.
#[derive(Debug, Default, Clone)]
pub struct Meta {
    pub title: Option<String>,
    pub author: Option<String>,
    pub keywords: Option<String>,
    pub date: Option<String>,
}

impl Meta {
    /// Fill what is not yet known from another source; what is already known
    /// stays, which is how Tika keeps a PDF's Info dictionary ahead of its XMP.
    fn fill(&mut self, other: Meta) {
        self.title = self.title.take().or(other.title);
        self.author = self.author.take().or(other.author);
        self.keywords = self.keywords.take().or(other.keywords);
        self.date = self.date.take().or(other.date);
    }
}

/// A file a parser could not read. The processor reports it as the
/// reference does -- `Error parsing document in field [...]` -- with this
/// underneath as the cause.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    /// the exception's type, as OpenSearch names it
    pub kind: &'static str,
    pub reason: String,
}

impl Failure {
    fn tika(reason: impl Into<String>) -> Failure {
        Failure { kind: "tika_exception", reason: reason.into() }
    }
}

/// Read a file into its text, stopping after `limit` characters when there
/// is a limit.
pub fn extract(bytes: &[u8], limit: Option<usize>) -> Result<Extracted, Failure> {
    let found = detect::detect(bytes);
    let mut text = Text::new(limit);
    let mut content_type = found.content_type.clone();
    // a file of no bytes is named but not parsed: Tika refuses it, and the
    // processor takes that refusal as an empty document
    if bytes.is_empty() {
        return Ok(Extracted { content_type, ..Extracted::default() });
    }
    let meta = match found.parser {
        detect::Parser::Text => {
            let (decoded, cs) = charset::decode_text(bytes, None)?;
            content_type = format!("{}; charset={cs}", found.content_type);
            text.start("p");
            text.chars(&decoded);
            text.end("p");
            Meta::default()
        }
        detect::Parser::Html => {
            let (decoded, cs) = charset::decode_text(bytes, None)?;
            content_type = format!("{}; charset={cs}", found.content_type);
            html::parse(&decoded, &mut text)
        }
        detect::Parser::Xml => xml::parse(bytes, &mut text)?,
        detect::Parser::Rtf => rtf::parse(bytes, &mut text)?,
        detect::Parser::Pdf => pdf::parse(bytes, &mut text)?,
        detect::Parser::Doc => office::doc(bytes, &mut text)?,
        detect::Parser::Docx => office::docx(bytes, &mut text)?,
        detect::Parser::Xlsx => office::xlsx(bytes, &mut text)?,
        detect::Parser::Pptx => office::pptx(bytes, &mut text)?,
        detect::Parser::Odf => office::odf(bytes, &mut text)?,
        detect::Parser::Epub => office::epub(bytes, &mut text)?,
        detect::Parser::None => Meta::default(),
    };
    let length = text.units;
    Ok(Extracted { text: text.out, length, content_type, meta })
}

/// The text a parser writes, as Tika's XHTML handler would leave it.
///
/// `indexed_chars` is enforced here, where Tika enforces it: once the limit
/// is reached nothing more is written, and a parser that asks can stop early.
pub struct Text {
    out: String,
    /// UTF-16 code units written so far
    units: usize,
    limit: Option<usize>,
}

/// The elements Tika ends with a newline.
const ENDLINE: &[&str] = &[
    "p",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "div",
    "ul",
    "ol",
    "dl",
    "pre",
    "hr",
    "blockquote",
    "address",
    "fieldset",
    "table",
    "form",
    "noscript",
    "li",
    "dt",
    "dd",
    "noframes",
    "br",
    "tr",
    "select",
    "option",
    "link",
    "script",
];

/// The elements Tika begins with a tab.
const INDENT: &[&str] = &["li", "dd", "dt", "td", "th", "frame"];

impl Text {
    pub fn new(limit: Option<usize>) -> Text {
        Text { out: String::new(), units: 0, limit }
    }

    /// Whether the limit has been reached, past which nothing is kept.
    pub fn full(&self) -> bool {
        self.limit.is_some_and(|l| self.units >= l)
    }

    pub fn start(&mut self, element: &str) {
        if INDENT.contains(&element) {
            self.chars("\t");
        }
    }

    pub fn end(&mut self, element: &str) {
        if ENDLINE.contains(&element) {
            self.chars("\n");
        }
    }

    /// An element with nothing but text in it.
    pub fn element(&mut self, element: &str, text: &str) {
        self.start(element);
        self.chars(text);
        self.end(element);
    }

    pub fn chars(&mut self, s: &str) {
        for c in s.chars() {
            // XML has no place for most control characters, and Tika's
            // handler puts the replacement character where one was
            let c = match c {
                '\t' | '\n' | '\r' => c,
                c if (c as u32) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => '\u{FFFD}',
                c => c,
            };
            let width = c.len_utf16();
            if let Some(limit) = self.limit
                && self.units + width > limit
            {
                // a character that would straddle the limit is not
                // written, but the limit is what is counted -- Java cuts
                // between the two halves of a surrogate pair
                self.units = limit;
                return;
            }
            self.out.push(c);
            self.units += width;
        }
    }
}

/// What the processor writes, given what the file turned out to be.
pub fn fields(found: &Extracted, wanted: Option<&[String]>) -> Value {
    let take: Vec<String> = match wanted {
        Some(w) => w.iter().map(|p| p.to_lowercase()).collect(),
        None => PROPERTIES.iter().map(|s| s.to_string()).collect(),
    };
    let wants = |p: &str| take.iter().any(|t| t == p);
    // Only what the file actually said is written: a document with no author
    // has no `author` field rather than an empty one. The order is the order
    // the reference's hash map hands its keys back in, which is neither the
    // order asked for nor the order written.
    const WRITTEN: &[&str] = &[
        "date",
        "keywords",
        "content_type",
        "author",
        "language",
        "title",
        "content",
        "content_length",
    ];
    let mut out = Map::new();
    let has_text = !found.text.is_empty();
    for p in WRITTEN {
        if !wants(p) {
            continue;
        }
        match *p {
            // text that is all whitespace is still text: it is written as an
            // empty string, and its language is the detector's empty answer
            "content" if has_text => {
                out.insert("content".into(), json!(found.text.trim()));
            }
            "language" if has_text => {
                out.insert("language".into(), json!(language_of(&found.text)));
            }
            "content_length" => {
                out.insert("content_length".into(), json!(found.length));
            }
            "content_type" if !found.content_type.is_empty() => {
                out.insert("content_type".into(), json!(found.content_type));
            }
            "title" | "author" | "keywords" | "date" => {
                let value = match *p {
                    "title" => &found.meta.title,
                    "author" => &found.meta.author,
                    "keywords" => &found.meta.keywords,
                    _ => &found.meta.date,
                };
                if let Some(v) = value.as_ref().filter(|v| !v.is_empty()) {
                    out.insert((*p).into(), json!(v));
                }
            }
            _ => {}
        }
    }
    Value::Object(out)
}

/// Decode base64 the way `java.util.Base64.getDecoder()` does, refusing what
/// it refuses with the words it uses: whitespace is not skipped, padding may
/// be left off, and a stray character is named in hexadecimal.
pub fn java_base64(input: &str) -> Result<Vec<u8>, String> {
    // Java reads the string as ISO-8859-1 bytes, and a character outside it
    // becomes a question mark
    let src: Vec<u8> =
        input.chars().map(|c| if (c as u32) < 256 { c as u32 as u8 } else { b'?' }).collect();
    if src.len() == 1 {
        return Err("Input byte[] should at least have 2 bytes for base64 bytes".into());
    }
    let value = |b: u8| -> i32 {
        match b {
            b'A'..=b'Z' => (b - b'A') as i32,
            b'a'..=b'z' => (b - b'a') as i32 + 26,
            b'0'..=b'9' => (b - b'0') as i32 + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => -2,
            _ => -1,
        }
    };
    let mut out = Vec::with_capacity(src.len() / 4 * 3);
    let (mut bits, mut shift, mut sp) = (0u32, 18i32, 0usize);
    while sp < src.len() {
        let raw = src[sp];
        sp += 1;
        let b = value(raw);
        if b < 0 {
            if b == -2 {
                if (shift == 6
                    && (sp == src.len() || {
                        let next = src[sp];
                        sp += 1;
                        next != b'='
                    }))
                    || shift == 18
                {
                    return Err("Input byte array has wrong 4-byte ending unit".into());
                }
                break;
            }
            return Err(format!("Illegal base64 character {}", java_hex(raw as i8)));
        }
        bits |= (b as u32) << shift;
        shift -= 6;
        if shift < 0 {
            out.extend_from_slice(&[(bits >> 16) as u8, (bits >> 8) as u8, bits as u8]);
            shift = 18;
            bits = 0;
        }
    }
    match shift {
        6 => out.push((bits >> 16) as u8),
        0 => out.extend_from_slice(&[(bits >> 16) as u8, (bits >> 8) as u8]),
        12 => return Err("Last unit does not have enough valid bits".into()),
        _ => {}
    }
    if sp < src.len() {
        return Err(format!("Input byte array has incorrect ending byte at {sp}"));
    }
    Ok(out)
}

/// `Integer.toString(b, 16)` on a signed byte: `-3d`, not `c3`.
fn java_hex(b: i8) -> String {
    if b < 0 { format!("-{:x}", -(b as i32)) } else { format!("{:x}", b) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(bytes: &[u8]) -> Value {
        fields(&extract(bytes, Some(100_000)).unwrap(), None)
    }

    #[test]
    fn plain_text_gets_the_newline_tika_writes() {
        let out = read(b"This is an english text to test if the pipeline works");
        assert_eq!(out["content"], "This is an english text to test if the pipeline works");
        assert_eq!(out["content_length"], 54);
        assert_eq!(out["content_type"], "text/plain; charset=ISO-8859-1");
        assert_eq!(out["language"], "en");
    }

    #[test]
    fn indexed_chars_cuts_the_text_and_the_length() {
        let found =
            extract(b"\"God Save the Queen\" (alternatively \"God Save the King\"\n", Some(10))
                .unwrap();
        let out = fields(&found, None);
        assert_eq!(out["content"], "\"God Save");
        assert_eq!(out["content_length"], 10);
        assert_ne!(out["language"], "en");
    }

    #[test]
    fn an_empty_file_has_no_content_and_no_language() {
        let out = read(b"");
        assert!(out.get("content").is_none());
        assert!(out.get("language").is_none());
        assert_eq!(out["content_length"], 0);
    }

    #[test]
    fn whitespace_is_content_all_the_same() {
        let out = read(b"   ");
        assert_eq!(out["content"], "");
        assert_eq!(out["language"], "");
    }

    #[test]
    fn properties_choose_what_is_written() {
        let found = extract(b"hello world, this is english text", None).unwrap();
        let out = fields(&found, Some(&["language".to_string()]));
        assert_eq!(out.as_object().unwrap().len(), 1);
    }

    #[test]
    fn base64_is_refused_as_java_refuses_it() {
        assert_eq!(java_base64("aGVsbG8K").unwrap(), b"hello\n");
        assert_eq!(java_base64("aGVsbG8").unwrap(), b"hello");
        assert_eq!(java_base64("aGVs bG8K").unwrap_err(), "Illegal base64 character 20");
        assert_eq!(java_base64("aGVsbG8K\n").unwrap_err(), "Illegal base64 character a");
        assert_eq!(java_base64("aGVsb-8K").unwrap_err(), "Illegal base64 character 2d");
        assert_eq!(
            java_base64("a").unwrap_err(),
            "Input byte[] should at least have 2 bytes for base64 bytes"
        );
        assert_eq!(java_base64("aGVsb").unwrap_err(), "Last unit does not have enough valid bits");
        assert_eq!(
            java_base64("aGVsbA=").unwrap_err(),
            "Input byte array has wrong 4-byte ending unit"
        );
        assert_eq!(java_base64("").unwrap(), b"");
    }

    #[test]
    fn a_limit_counts_utf16_units() {
        let mut t = Text::new(Some(3));
        t.chars("a\u{1F600}b");
        assert_eq!(t.out, "a\u{1F600}");
        assert_eq!(t.units, 3);
        let mut t = Text::new(Some(2));
        t.chars("a\u{1F600}");
        assert_eq!(t.out, "a");
        assert_eq!(t.units, 2);
    }
}
