//! What kind of file a run of bytes is, read from the bytes themselves.
//!
//! OpenSearch hands Tika the bytes and nothing else -- no file name -- so
//! what Tika has to go on is magic: a signature at the start of the file,
//! then a look inside a container (which part of a zip is the document, which
//! stream of an OLE2 file), and last a guess at whether the bytes are text at
//! all. The names are Tika's, which are not always the names a browser uses.

use super::charset::Stats;

/// Which parser reads a file. `None` is a kind of file the reference names
/// but has no parser for, and reads no text from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Parser {
    Text,
    Html,
    Xml,
    Rtf,
    Pdf,
    Doc,
    Docx,
    Xlsx,
    Pptx,
    Odf,
    Epub,
    None,
}

#[derive(Debug)]
pub struct Detected {
    /// without a charset: the text parsers add the one they read with
    pub content_type: String,
    pub parser: Parser,
}

fn found(content_type: &str, parser: Parser) -> Detected {
    Detected { content_type: content_type.to_string(), parser }
}

pub fn detect(bytes: &[u8]) -> Detected {
    let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    if body.starts_with(b"%PDF-") {
        return found("application/pdf", Parser::Pdf);
    }
    if body.starts_with(b"{\\rtf") {
        return found("application/rtf", Parser::Rtf);
    }
    if bytes.starts_with(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]) {
        return ole2(bytes);
    }
    if bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") {
        return zip(bytes);
    }
    if let Some(kind) = binary_magic(bytes) {
        return found(kind, Parser::None);
    }
    let head = &body[..body.len().min(8192)];
    let lower: Vec<u8> = head.iter().map(|b| b.to_ascii_lowercase()).collect();
    // XHTML is recognised by its namespace anywhere near the start, ahead of
    // both the XML declaration and the HTML signatures it also carries
    if find(&lower, b"<html xmlns=\"http://www.w3.org/1999/xhtml\"").is_some() {
        return found("application/xhtml+xml", Parser::Html);
    }
    if body.starts_with(b"<?xml") {
        return xml_root(head);
    }
    if html_magic(&lower) {
        return found("text/html", Parser::Html);
    }
    if let Some(kind) = mail_magic(head) {
        return found(kind, Parser::None);
    }
    let stats = Stats::of(&bytes[..bytes.len().min(64 * 1024)]);
    if stats.mostly_ascii() || stats.looks_like_utf8() {
        return found("text/plain", Parser::Text);
    }
    found("application/octet-stream", Parser::None)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The signatures of the formats that carry no text the processor reads.
fn binary_magic(b: &[u8]) -> Option<&'static str> {
    let at = |offset: usize, sig: &[u8]| b.get(offset..offset + sig.len()) == Some(sig);
    Some(if at(0, b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if at(0, &[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if at(0, b"GIF87a") || at(0, b"GIF89a") {
        "image/gif"
    } else if at(0, b"II*\0") || at(0, b"MM\0*") {
        "image/tiff"
    } else if at(0, b"RIFF") && at(8, b"WEBP") {
        "image/webp"
    } else if at(0, b"RIFF") && at(8, b"WAVE") {
        "audio/vnd.wave"
    } else if at(0, &[0x1F, 0x8B]) {
        "application/gzip"
    } else if at(0, b"7z\xBC\xAF\x27\x1C") {
        "application/x-7z-compressed"
    } else if at(0, &[0xFD, b'7', b'z', b'X', b'Z', 0x00]) {
        "application/x-xz"
    } else if at(0, b"%!PS-Adobe") {
        "application/postscript"
    } else if at(0, b"ID3") {
        "audio/mpeg"
    } else if at(0, b"fLaC") {
        "audio/x-flac"
    } else if at(0, b"SQLite format 3\0") {
        "application/x-sqlite3"
    } else if at(4, b"ftypqt  ") {
        "video/quicktime"
    } else if at(4, b"ftypM4A ") {
        "audio/mp4"
    } else if at(4, b"ftyp") {
        "video/mp4"
    } else {
        return None;
    })
}

/// Tika's HTML signatures: a tag HTML documents begin with, near the start.
fn html_magic(lower: &[u8]) -> bool {
    let start = lower.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(lower.len());
    let near = &lower[..lower.len().min(64)];
    const NEAR: &[&[u8]] = &[b"<!doctype html", b"<html", b"<head", b"<title"];
    if NEAR.iter().any(|sig| find(near, sig).is_some()) {
        return true;
    }
    const AT_START: &[&[u8]] = &[
        b"<body", b"<!--", b"<h1", b"<h2", b"<h3", b"<div", b"<script", b"<style", b"<p>", b"<p ",
    ];
    let rest = &lower[start..];
    AT_START.iter().any(|sig| rest.starts_with(sig))
}

/// An email, or a mailbox of them. A header name that only mail has is
/// enough on its own; one that a note could begin with as well -- `Date:`,
/// `Subject:` -- needs a second header after it.
fn mail_magic(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(b"From ") && head.iter().take(200).any(|b| *b == b'\n') {
        return Some("application/mbox");
    }
    let lower: Vec<u8> = head.iter().map(|b| b.to_ascii_lowercase()).collect();
    const STRONG: &[&[u8]] = &[
        b"relay-version:",
        b"#! rnews",
        b"n#! rnews",
        b"forward to ",
        b"pipe to ",
        b"return-path:",
        b"received:",
        b"message-id:",
        b"x-mailer:",
        b"delivered-to:",
        b"dkim-signature:",
    ];
    if STRONG.iter().any(|s| lower.starts_with(s)) {
        return Some("message/rfc822");
    }
    const WEAK: &[&[u8]] = &[b"from:", b"date:", b"subject:", b"to:", b"mime-version:"];
    const SECOND: &[&[u8]] = &[
        b"\nfrom:",
        b"\nto:",
        b"\nsubject:",
        b"\ndate:",
        b"\nmessage-id:",
        b"\nmime-version:",
        b"\ncontent-type:",
    ];
    if WEAK.iter().any(|s| lower.starts_with(s)) && SECOND.iter().any(|s| find(&lower, s).is_some())
    {
        return Some("message/rfc822");
    }
    None
}

/// An XML document is named by its root element where Tika knows that
/// element, and is plain XML otherwise.
fn xml_root(head: &[u8]) -> Detected {
    let text = String::from_utf8_lossy(head);
    let mut rest = text.as_ref();
    // step over the declaration, comments, processing instructions and the
    // doctype to the first element
    while let Some(at) = rest.find('<') {
        rest = &rest[at..];
        if rest.starts_with("<?") || rest.starts_with("<!") {
            let end = if rest.starts_with("<!--") {
                rest.find("-->").map(|e| e + 3)
            } else {
                rest.find('>').map(|e| e + 1)
            };
            match end {
                Some(e) => rest = &rest[e..],
                None => break,
            }
            continue;
        }
        let name: String = rest[1..]
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '>' && *c != '/')
            .collect();
        let local = name.rsplit(':').next().unwrap_or("");
        return match local {
            "svg" => found("image/svg+xml", Parser::Xml),
            "html" if rest.contains("http://www.w3.org/1999/xhtml") => {
                found("application/xhtml+xml", Parser::Html)
            }
            "rss" => found("application/rss+xml", Parser::Xml),
            "feed" if rest.contains("http://www.w3.org/2005/Atom") => {
                found("application/atom+xml", Parser::Xml)
            }
            _ => found("application/xml", Parser::Xml),
        };
    }
    found("application/xml", Parser::Xml)
}

/// An OLE2 compound file is named by the streams it holds.
fn ole2(bytes: &[u8]) -> Detected {
    let Ok(file) = cfb::CompoundFile::open(std::io::Cursor::new(bytes)) else {
        return found("application/x-tika-msoffice", Parser::None);
    };
    let names: Vec<String> = file.read_root_storage().map(|e| e.name().to_string()).collect();
    let has = |n: &str| names.iter().any(|e| e == n);
    if has("WordDocument") {
        found("application/msword", Parser::Doc)
    } else if has("Workbook") || has("Book") {
        found("application/vnd.ms-excel", Parser::None)
    } else if has("PowerPoint Document") {
        found("application/vnd.ms-powerpoint", Parser::None)
    } else if has("VisioDocument") {
        found("application/vnd.visio", Parser::None)
    } else if has("EncryptedPackage") {
        found("application/x-tika-ooxml-protected", Parser::None)
    } else if names.iter().any(|e| e.starts_with("__substg1.0_")) {
        found("application/vnd.ms-outlook", Parser::None)
    } else if has("Quill") {
        found("application/x-mspublisher", Parser::None)
    } else {
        found("application/x-tika-msoffice", Parser::None)
    }
}

/// A zip is named by what it says it holds: an OpenDocument or EPUB file by
/// its `mimetype` entry, an Open XML file by the content type of its main
/// part.
fn zip(bytes: &[u8]) -> Detected {
    use super::office::zip_entry;
    if let Some(mime) = zip_entry(bytes, "mimetype") {
        let mime = String::from_utf8_lossy(&mime).trim().to_string();
        if mime.starts_with("application/vnd.oasis.opendocument.") {
            return Detected { content_type: mime, parser: Parser::Odf };
        }
        if mime == "application/epub+zip" {
            return found("application/epub+zip", Parser::Epub);
        }
        if !mime.is_empty() && mime.len() < 100 && mime.contains('/') {
            return Detected { content_type: mime, parser: Parser::None };
        }
    }
    if let Some(types) = zip_entry(bytes, "[Content_Types].xml") {
        let types = String::from_utf8_lossy(&types);
        const MAIN: &[(&str, &str, Parser)] = &[
            (
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                Parser::Docx,
            ),
            (
                "application/vnd.ms-word.document.macroEnabled.main+xml",
                "application/vnd.ms-word.document.macroenabled.12",
                Parser::Docx,
            ),
            (
                "application/vnd.openxmlformats-officedocument.wordprocessingml.template.main+xml",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.template",
                Parser::Docx,
            ),
            (
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                Parser::Xlsx,
            ),
            (
                "application/vnd.ms-excel.sheet.macroEnabled.main+xml",
                "application/vnd.ms-excel.sheet.macroenabled.12",
                Parser::Xlsx,
            ),
            (
                "application/vnd.openxmlformats-officedocument.spreadsheetml.template.main+xml",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.template",
                Parser::Xlsx,
            ),
            (
                "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                Parser::Pptx,
            ),
            (
                "application/vnd.ms-powerpoint.presentation.macroEnabled.main+xml",
                "application/vnd.ms-powerpoint.presentation.macroenabled.12",
                Parser::Pptx,
            ),
            (
                "application/vnd.openxmlformats-officedocument.presentationml.slideshow.main+xml",
                "application/vnd.openxmlformats-officedocument.presentationml.slideshow",
                Parser::Pptx,
            ),
            (
                "application/vnd.openxmlformats-officedocument.presentationml.template.main+xml",
                "application/vnd.openxmlformats-officedocument.presentationml.template",
                Parser::Pptx,
            ),
            // the plugin leaves Visio out on purpose: named, never read
            (
                "application/vnd.ms-visio.drawing.main+xml",
                "application/vnd.ms-visio.drawing",
                Parser::None,
            ),
            (
                "application/vnd.ms-excel.sheet.binary.macroEnabled.main",
                "application/vnd.ms-excel.sheet.binary.macroenabled.12",
                Parser::None,
            ),
        ];
        for (part, name, parser) in MAIN {
            if types.contains(&format!("ContentType=\"{part}\"")) {
                return found(name, *parser);
            }
        }
        return found("application/x-tika-ooxml", Parser::None);
    }
    if zip_entry(bytes, "META-INF/MANIFEST.MF").is_some() {
        return found("application/java-archive", Parser::None);
    }
    found("application/zip", Parser::None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_come_before_content() {
        assert_eq!(detect(b"%PDF-1.4\n").content_type, "application/pdf");
        assert_eq!(detect(b"{\\rtf1\\ansi hello}").content_type, "application/rtf");
        assert_eq!(detect(b"<!DOCTYPE html><p>x").content_type, "text/html");
        assert_eq!(detect(b"<?xml version=\"1.0\"?><a>b</a>").content_type, "application/xml");
        assert_eq!(detect(b"\x89PNG\r\n\x1a\n\0\0").content_type, "image/png");
        assert_eq!(detect(b"just words").content_type, "text/plain");
        assert_eq!(detect(&[0u8, 1, 2, 3, 200, 201]).content_type, "application/octet-stream");
        assert_eq!(detect(b"").content_type, "application/octet-stream");
    }

    #[test]
    fn xhtml_is_named_by_its_namespace() {
        let page =
            b"<!-- a comment -->\n<html xmlns=\"http://www.w3.org/1999/xhtml\"><body/></html>";
        assert_eq!(detect(page).content_type, "application/xhtml+xml");
    }

    #[test]
    fn mail_needs_more_than_a_date() {
        assert_eq!(detect(b"Date: tomorrow, bring cake\n").content_type, "text/plain");
        assert_eq!(
            detect(b"From: a@example.com\nTo: b@example.com\nSubject: hi\n\nbody").content_type,
            "message/rfc822"
        );
    }
}
