//! XML: a walk over the events of a document, shared by every format that is
//! XML inside, and Tika's parser for XML files themselves.
//!
//! An XML file's text is every run of character data in it, one after the
//! other, with a space where each element begins -- Tika's text handler is
//! told to put one there, so that `<a>x</a><b>y</b>` does not read `xy`. What
//! the file says about itself is whatever Dublin Core elements it carries.

use super::{Failure, Meta, Text};
use quick_xml::events::Event;

/// One thing an XML document says, with names reduced to their local part.
pub enum Ev<'a> {
    Start { name: &'a str, prefix: &'a str, attrs: &'a [(String, String)] },
    End { name: &'a str },
    Text(&'a str),
}

/// Walk a document, calling `f` for each event until it returns `false` or
/// the document ends. A document that is not well formed is an error at the
/// point it stops being so; what came before has been walked.
pub fn walk(xml: &str, mut f: impl FnMut(Ev) -> bool) -> Result<(), String> {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut depth: usize = 0;
    loop {
        let event = reader.read_event_into(&mut buf).map_err(|e| e.to_string())?;
        let go = match event {
            Event::Start(e) | Event::Empty(e) if depth > 4096 => {
                let _ = e;
                return Err("document nested too deeply".into());
            }
            Event::Start(e) => {
                depth += 1;
                let (prefix, name, attrs) = parts(&e);
                f(Ev::Start { name: &name, prefix: &prefix, attrs: &attrs })
            }
            Event::Empty(e) => {
                let (prefix, name, attrs) = parts(&e);
                f(Ev::Start { name: &name, prefix: &prefix, attrs: &attrs })
                    && f(Ev::End { name: &name })
            }
            Event::End(e) => {
                depth = depth.saturating_sub(1);
                let raw = e.name();
                let name = local(raw.as_ref());
                f(Ev::End { name })
            }
            Event::Text(t) => f(Ev::Text(&t.xml10_content())),
            Event::CData(c) => {
                let raw = c.into_inner();
                f(Ev::Text(&raw))
            }
            Event::GeneralRef(r) => {
                let resolved = match r.resolve_char_ref() {
                    Ok(Some(c)) => c.to_string(),
                    Ok(None) => match r.xml10_content().as_ref() {
                        "lt" => "<".into(),
                        "gt" => ">".into(),
                        "amp" => "&".into(),
                        "apos" => "'".into(),
                        "quot" => "\"".into(),
                        // an entity only a DTD could define is left as written
                        other => format!("&{other};"),
                    },
                    Err(e) => return Err(e.to_string()),
                };
                f(Ev::Text(&resolved))
            }
            Event::Eof => return Ok(()),
            _ => true,
        };
        if !go {
            return Ok(());
        }
        buf.clear();
    }
}

fn local(name: &str) -> &str {
    name.rsplit_once(':').map(|(_, l)| l).unwrap_or(name)
}

fn parts(e: &quick_xml::events::BytesStart) -> (String, String, Vec<(String, String)>) {
    let raw = e.name();
    let full: &str = raw.as_ref();
    let (prefix, name) = match full.split_once(':') {
        Some((p, l)) => (p.to_string(), l.to_string()),
        None => (String::new(), full.to_string()),
    };
    let attrs = e
        .attributes()
        .with_checks(false)
        .filter_map(|a| a.ok())
        .map(|a| {
            let key: &str = a.key.as_ref();
            let value = a
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map(|v| v.into_owned())
                .unwrap_or_default();
            (key.to_string(), value)
        })
        .collect();
    (prefix, name, attrs)
}

/// The value of an attribute, by its full or local name.
pub fn attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs.iter().find(|(k, _)| k == name || local(k) == name).map(|(_, v)| v.as_str())
}

/// XML bytes as text, in the encoding the declaration names.
pub fn decode(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFE, 0xFF]) || bytes.starts_with(&[0xFF, 0xFE]) {
        let name = if bytes[0] == 0xFE { "UTF-16BE" } else { "UTF-16LE" };
        return super::charset::decode(bytes, name).trim_start_matches('\u{FEFF}').to_string();
    }
    let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let head = String::from_utf8_lossy(&body[..body.len().min(200)]);
    if let Some(decl_end) = head.find("?>")
        && let Some(at) = head[..decl_end].find("encoding=")
    {
        let rest = &head[at + 9..decl_end];
        let name: String = rest
            .trim_start_matches(['"', '\''])
            .chars()
            .take_while(|c| !matches!(c, '"' | '\''))
            .collect();
        if let Some(cs) = super::charset::canonical(&name)
            && cs != "UTF-8"
        {
            return super::charset::decode(body, &cs);
        }
    }
    String::from_utf8_lossy(body).into_owned()
}

const DUBLIN_CORE: &str = "http://purl.org/dc/elements/1.1/";

/// Tika's XML parser: the text of every element, and the Dublin Core
/// elements as what the document says about itself.
pub fn parse(bytes: &[u8], text: &mut Text) -> Result<Meta, Failure> {
    let xml = decode(bytes);
    let mut meta = Meta::default();
    let mut dc_prefixes: Vec<String> = Vec::new();
    let mut open: Vec<(bool, String)> = Vec::new();
    let mut collecting: Option<(String, String)> = None;
    text.start("p");
    let walked = walk(&xml, |ev| {
        match ev {
            Ev::Start { name, prefix, attrs } => {
                for (k, v) in attrs {
                    if v == DUBLIN_CORE {
                        dc_prefixes.push(k.strip_prefix("xmlns:").unwrap_or("").to_string());
                    }
                }
                text.chars(" ");
                let dc = dc_prefixes.iter().any(|p| p == prefix);
                if dc
                    && collecting.is_none()
                    && matches!(name, "title" | "creator" | "subject" | "date")
                {
                    collecting = Some((name.to_string(), String::new()));
                }
                open.push((dc, name.to_string()));
            }
            Ev::End { name } => {
                if let Some((dc, open_name)) = open.pop()
                    && dc
                    && collecting
                        .as_ref()
                        .is_some_and(|(n, _)| *n == open_name && open_name == name)
                {
                    let (field, value) = collecting.take().unwrap_or_default();
                    let value = value.trim().to_string();
                    if !value.is_empty() {
                        let slot = match field.as_str() {
                            "title" => &mut meta.title,
                            "creator" => &mut meta.author,
                            "subject" => &mut meta.keywords,
                            _ => &mut meta.date,
                        };
                        slot.get_or_insert(value);
                    }
                }
            }
            Ev::Text(t) => {
                if let Some((_, value)) = collecting.as_mut() {
                    value.push_str(t);
                }
                text.chars(t);
            }
        }
        !text.full()
    });
    if walked.is_err() && !text.full() {
        return Err(Failure { kind: "tika_exception", reason: "XML parse error".into() });
    }
    text.end("p");
    Ok(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_with_a_space_per_element_and_dublin_core() {
        let doc = br#"<?xml version="1.0"?><doc xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>The &amp; Title</dc:title><body>Hello<b>world</b></body></doc>"#;
        let mut text = Text::new(None);
        let meta = parse(doc, &mut text).unwrap();
        assert_eq!(text.out, "  The & Title Hello world\n");
        assert_eq!(meta.title.as_deref(), Some("The & Title"));
    }

    #[test]
    fn broken_xml_is_an_error() {
        let mut text = Text::new(None);
        assert!(parse(b"<?xml version=\"1.0\"?><a><b></a>", &mut text).is_err());
    }
}
