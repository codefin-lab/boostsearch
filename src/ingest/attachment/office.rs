//! The office formats: Word's binary `.doc`, the Open XML documents Office
//! writes now, OpenDocument, and EPUB -- which is a zip of XHTML.
//!
//! Each is read into the XHTML Tika's parser for it writes: a Word paragraph
//! is a `<p>`, a table cell a `<td>`, a spreadsheet a heading with the sheet's
//! name and a table under it, a slide a `<div>` of paragraphs.

use super::xml::{Ev, attr, walk};
use super::{Failure, Meta, Text};

/// The most a part of an archive may inflate to: past it the archive is a
/// bomb, and the document is unreadable rather than the process gone.
const MAX_INFLATED: usize = 64 << 20;

/// A part a parser needs and the file does not have.
fn missing(part: &str) -> Failure {
    Failure::tika(format!("the package has no part [{part}]"))
}

fn part(bytes: &[u8], name: &str) -> Result<String, Failure> {
    let raw = zip_entry(bytes, name).ok_or_else(|| missing(name))?;
    Ok(super::xml::decode(&raw))
}

fn xml_error(e: String) -> Failure {
    Failure::tika(format!("XML parse error: {e}"))
}

/// What an Open XML package says about itself, in `docProps/core.xml`, in
/// the Dublin Core vocabulary.
fn core_properties(bytes: &[u8]) -> Meta {
    let mut meta = Meta::default();
    let Ok(xml) = part(bytes, "docProps/core.xml") else { return meta };
    let mut field: Option<String> = None;
    let mut value = String::new();
    let _ = walk(&xml, |ev| {
        match ev {
            Ev::Start { name, .. } => {
                field = Some(name.to_string());
                value.clear();
            }
            Ev::Text(t) => value.push_str(t),
            Ev::End { name } => {
                if field.as_deref() == Some(name) {
                    let v = value.trim().to_string();
                    if !v.is_empty() {
                        match name {
                            "title" => meta.title = Some(v),
                            "creator" => meta.author = Some(v),
                            "keywords" => meta.keywords = Some(v),
                            // the date a document carries is the date it was
                            // made, not the date it was last touched
                            "created" => meta.date = Some(iso_seconds(&v)),
                            _ => {}
                        }
                    }
                }
                field = None;
            }
        }
        true
    });
    meta
}

/// A W3C date as Tika writes one back: to the second, in UTC.
fn iso_seconds(v: &str) -> String {
    match crate::store::parse_date_lenient(v) {
        Some(at) => {
            crate::store::format_millis(at.unix_timestamp() * 1000, "yyyy-MM-dd'T'HH:mm:ss'Z'")
                .unwrap_or_else(|| v.to_string())
        }
        None => v.to_string(),
    }
}

/// Where a relationship in a package points, by id, resolved against the
/// part the relationships belong to.
fn relationships(bytes: &[u8], owner: &str) -> Vec<(String, String, String)> {
    let (dir, file) = match owner.rsplit_once('/') {
        Some((d, f)) => (d.to_string(), f.to_string()),
        None => (String::new(), owner.to_string()),
    };
    let rels_name = if dir.is_empty() {
        format!("_rels/{file}.rels")
    } else {
        format!("{dir}/_rels/{file}.rels")
    };
    let Ok(xml) = part(bytes, &rels_name) else { return Vec::new() };
    let mut out = Vec::new();
    let _ = walk(&xml, |ev| {
        if let Ev::Start { name: "Relationship", attrs, .. } = ev {
            let id = attr(attrs, "Id").unwrap_or("").to_string();
            let kind =
                attr(attrs, "Type").unwrap_or("").rsplit('/').next().unwrap_or("").to_string();
            let target = attr(attrs, "Target").unwrap_or("");
            out.push((id, kind, resolve(&dir, target)));
        }
        true
    });
    out
}

/// A relative part name, resolved.
fn resolve(dir: &str, target: &str) -> String {
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    }
    let mut parts: Vec<&str> = if dir.is_empty() { Vec::new() } else { dir.split('/').collect() };
    for piece in target.split('/') {
        match piece {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            p => parts.push(p),
        }
    }
    parts.join("/")
}

/// The part a package's root relationship names as its main document.
fn main_part(bytes: &[u8], default: &str) -> String {
    relationships(bytes, "")
        .into_iter()
        .find(|(_, kind, _)| kind == "officeDocument")
        .map(|(_, _, target)| target)
        .unwrap_or_else(|| default.to_string())
}

/// A Word document written in Open XML.
///
/// A paragraph is a `<p>`, a table a table of cells each holding its
/// paragraphs, a tab a tab and a break a newline. The headers come first and
/// the footers last, as Tika writes them; deleted text and field codes are
/// not text.
pub fn docx(bytes: &[u8], text: &mut Text) -> Result<Meta, Failure> {
    let meta = core_properties(bytes);
    let main = main_part(bytes, "word/document.xml");
    let xml = part(bytes, &main)?;
    let rels = relationships(bytes, &main);
    // the headers and footers of the document's last section, first page's,
    // even pages' and the default, in that order
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut footers: Vec<(String, String)> = Vec::new();
    let _ = walk(&xml, |ev| {
        if let Ev::Start { name, attrs, .. } = ev
            && (name == "headerReference" || name == "footerReference")
        {
            let kind = attr(attrs, "w:type").unwrap_or("default").to_string();
            let id = attr(attrs, "r:id").unwrap_or("").to_string();
            let list = if name == "headerReference" { &mut headers } else { &mut footers };
            list.retain(|(k, _)| *k != kind);
            list.push((kind, id));
        }
        true
    });
    let ordered = |list: &[(String, String)]| -> Vec<String> {
        ["first", "even", "default"]
            .iter()
            .filter_map(|k| list.iter().find(|(kind, _)| kind == k))
            .filter_map(|(_, id)| {
                rels.iter().find(|(rid, _, _)| rid == id).map(|(_, _, t)| t.clone())
            })
            .collect()
    };
    for header in ordered(&headers) {
        if let Ok(x) = part(bytes, &header) {
            word_body(&x, text).map_err(xml_error)?;
        }
    }
    word_body(&xml, text).map_err(xml_error)?;
    for footer in ordered(&footers) {
        if let Ok(x) = part(bytes, &footer) {
            word_body(&x, text).map_err(xml_error)?;
        }
    }
    Ok(meta)
}

/// The text of one WordprocessingML part.
fn word_body(xml: &str, text: &mut Text) -> Result<(), String> {
    let mut in_text = false;
    // text inside an alternative's fallback repeats what its choice says
    let mut fallback = 0usize;
    let mut run = String::new();
    // a tab or a break is text inside a run; in a paragraph's properties a
    // `w:tab` is a tab stop
    let mut in_run = 0usize;
    walk(xml, |ev| {
        match ev {
            Ev::Start { name, .. } => match name {
                "Fallback" => fallback += 1,
                _ if fallback > 0 => {}
                "t" => in_text = true,
                "r" => in_run += 1,
                "tab" if in_run > 0 => run.push('\t'),
                "br" | "cr" if in_run > 0 => run.push('\n'),
                "p" | "tbl" | "tr" | "tc" => {
                    text.chars(&run);
                    run.clear();
                    let element = match name {
                        "tbl" => "table",
                        "tc" => "td",
                        other => other,
                    };
                    text.start(element);
                }
                _ => {}
            },
            Ev::End { name } => match name {
                "Fallback" => fallback = fallback.saturating_sub(1),
                _ if fallback > 0 => {}
                "t" => in_text = false,
                "r" => in_run = in_run.saturating_sub(1),
                "p" | "tbl" | "tr" | "tc" => {
                    text.chars(&run);
                    run.clear();
                    let element = match name {
                        "tbl" => "table",
                        "tc" => "td",
                        other => other,
                    };
                    text.end(element);
                }
                _ => {}
            },
            Ev::Text(t) => {
                if in_text && fallback == 0 {
                    run.push_str(t);
                }
            }
        }
        !text.full()
    })?;
    text.chars(&run);
    Ok(())
}

/// A spreadsheet written in Open XML.
///
/// Each sheet is a `<div>` holding a heading with the sheet's name and a
/// table of its rows; a cell left empty between two with values is written
/// as an empty cell, so the columns stay where they were.
pub fn xlsx(bytes: &[u8], text: &mut Text) -> Result<Meta, Failure> {
    let meta = core_properties(bytes);
    let main = main_part(bytes, "xl/workbook.xml");
    let workbook = part(bytes, &main)?;
    let rels = relationships(bytes, &main);
    let mut shared: Vec<String> = Vec::new();
    if let Some((_, _, target)) = rels.iter().find(|(_, kind, _)| kind == "sharedStrings")
        && let Ok(xml) = part(bytes, target)
    {
        let mut current: Option<String> = None;
        let mut depth_phonetic = 0usize;
        let mut in_t = false;
        walk(&xml, |ev| {
            match ev {
                Ev::Start { name, .. } => match name {
                    "si" => current = Some(String::new()),
                    "rPh" => depth_phonetic += 1,
                    "t" => in_t = true,
                    _ => {}
                },
                Ev::End { name } => match name {
                    "si" => shared.push(current.take().unwrap_or_default()),
                    "rPh" => depth_phonetic = depth_phonetic.saturating_sub(1),
                    "t" => in_t = false,
                    _ => {}
                },
                Ev::Text(t) => {
                    if in_t
                        && depth_phonetic == 0
                        && let Some(c) = current.as_mut()
                    {
                        c.push_str(t);
                    }
                }
            }
            true
        })
        .map_err(xml_error)?;
    }
    let mut sheets: Vec<(String, String)> = Vec::new();
    walk(&workbook, |ev| {
        if let Ev::Start { name: "sheet", attrs, .. } = ev {
            let title = attr(attrs, "name").unwrap_or("").to_string();
            let id = attr(attrs, "r:id").unwrap_or("");
            if let Some((_, _, target)) = rels.iter().find(|(rid, _, _)| rid == id) {
                sheets.push((title, target.clone()));
            }
        }
        true
    })
    .map_err(xml_error)?;
    for (title, target) in sheets {
        if text.full() {
            break;
        }
        let Ok(xml) = part(bytes, &target) else { continue };
        text.start("div");
        text.element("h1", &title);
        text.start("table");
        text.start("tbody");
        let mut last_col: i64 = -1;
        let mut cell: Option<(i64, String)> = None;
        let mut kind = String::new();
        let mut value = String::new();
        let mut in_value = false;
        walk(&xml, |ev| {
            match ev {
                Ev::Start { name, attrs, .. } => match name {
                    "row" => {
                        text.start("tr");
                        last_col = -1;
                    }
                    "c" => {
                        let col = attr(attrs, "r").map(column_of).unwrap_or(last_col + 1);
                        kind = attr(attrs, "t").unwrap_or("n").to_string();
                        cell = Some((col, String::new()));
                        value.clear();
                    }
                    "v" | "t" => in_value = true,
                    _ => {}
                },
                Ev::Text(t) => {
                    if in_value {
                        value.push_str(t);
                    }
                }
                Ev::End { name } => match name {
                    "v" | "t" => in_value = false,
                    "c" => {
                        if let Some((col, _)) = cell.take()
                            && !value.is_empty()
                        {
                            let shown = match kind.as_str() {
                                "s" => value
                                    .trim()
                                    .parse::<usize>()
                                    .ok()
                                    .and_then(|i| shared.get(i).cloned())
                                    .unwrap_or_default(),
                                "b" => {
                                    if value.trim() == "1" {
                                        "TRUE".into()
                                    } else {
                                        "FALSE".into()
                                    }
                                }
                                "n" => general_number(&value),
                                _ => value.clone(),
                            };
                            for _ in (last_col + 1)..col {
                                text.start("td");
                                text.end("td");
                            }
                            last_col = col;
                            text.element("td", &shown);
                        }
                    }
                    "row" => text.end("tr"),
                    _ => {}
                },
            }
            !text.full()
        })
        .map_err(xml_error)?;
        text.end("tbody");
        text.end("table");
        text.end("div");
    }
    Ok(meta)
}

/// The column a cell reference names: `C7` is the third, counted from zero.
fn column_of(reference: &str) -> i64 {
    let mut col: i64 = 0;
    for c in reference.chars().take_while(|c| c.is_ascii_alphabetic()).take(4) {
        col = col * 26 + (c.to_ascii_uppercase() as i64 - 'A' as i64 + 1);
    }
    col - 1
}

/// A number the way a cell in the General format shows it: no trailing
/// zeros, no decimal point for a whole number.
fn general_number(raw: &str) -> String {
    let Ok(v) = raw.trim().parse::<f64>() else { return raw.to_string() };
    if v.fract() == 0.0 && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    let s = format!("{v:.10}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    s.to_string()
}

/// A presentation written in Open XML: a `<div>` per slide holding a
/// paragraph per paragraph of text, then the slide's notes in a `<div>` of
/// their own.
pub fn pptx(bytes: &[u8], text: &mut Text) -> Result<Meta, Failure> {
    let meta = core_properties(bytes);
    let main = main_part(bytes, "ppt/presentation.xml");
    let presentation = part(bytes, &main)?;
    let rels = relationships(bytes, &main);
    let mut slides: Vec<String> = Vec::new();
    walk(&presentation, |ev| {
        if let Ev::Start { name: "sldId", attrs, .. } = ev
            && let Some(id) = attr(attrs, "r:id")
            && let Some((_, _, target)) = rels.iter().find(|(rid, _, _)| rid == id)
        {
            slides.push(target.clone());
        }
        true
    })
    .map_err(xml_error)?;
    for slide in slides {
        if text.full() {
            break;
        }
        let Ok(xml) = part(bytes, &slide) else { continue };
        text.start("div");
        drawing_text(&xml, text).map_err(xml_error)?;
        text.end("div");
        if let Some((_, _, notes)) =
            relationships(bytes, &slide).iter().find(|(_, kind, _)| kind == "notesSlide")
            && let Ok(xml) = part(bytes, notes)
        {
            text.start("div");
            drawing_text(&xml, text).map_err(xml_error)?;
            text.end("div");
        }
    }
    Ok(meta)
}

/// The paragraphs and tables of a DrawingML part. Placeholders for the
/// slide number, date, footer and the slide image on a notes page carry
/// nothing a reader wrote, and are left out.
fn drawing_text(xml: &str, text: &mut Text) -> Result<(), String> {
    let mut in_t = false;
    let mut skip_shape = 0usize;
    let mut shape_depth = 0usize;
    let mut run = String::new();
    walk(xml, |ev| {
        match ev {
            Ev::Start { name, attrs, .. } => match name {
                "sp" => {
                    shape_depth += 1;
                }
                "ph" => {
                    let kind = attr(attrs, "type").unwrap_or("");
                    if matches!(kind, "sldNum" | "dt" | "ftr" | "sldImg" | "hdr") && skip_shape == 0
                    {
                        skip_shape = shape_depth;
                    }
                }
                "t" => in_t = true,
                "br" if skip_shape == 0 => run.push('\n'),
                "p" | "tbl" | "tr" | "tc" if skip_shape == 0 => {
                    let element = match name {
                        "tbl" => "table",
                        "tc" => "td",
                        other => other,
                    };
                    text.start(element);
                }
                _ => {}
            },
            Ev::Text(t) => {
                if in_t && skip_shape == 0 {
                    run.push_str(t);
                }
            }
            Ev::End { name } => match name {
                "t" => in_t = false,
                "sp" => {
                    if skip_shape == shape_depth {
                        skip_shape = 0;
                    }
                    shape_depth = shape_depth.saturating_sub(1);
                }
                "p" | "tbl" | "tr" | "tc" if skip_shape == 0 => {
                    text.chars(&run);
                    run.clear();
                    let element = match name {
                        "tbl" => "table",
                        "tc" => "td",
                        other => other,
                    };
                    text.end(element);
                }
                _ => {}
            },
        }
        !text.full()
    })
}

/// An OpenDocument file: text, spreadsheet or presentation, all of which
/// keep their text in `content.xml` and what they say about themselves in
/// `meta.xml`.
pub fn odf(bytes: &[u8], text: &mut Text) -> Result<Meta, Failure> {
    let mut meta = Meta::default();
    if let Ok(xml) = part(bytes, "meta.xml") {
        let mut field: Option<String> = None;
        let mut value = String::new();
        let mut initial_creator: Option<String> = None;
        let _ = walk(&xml, |ev| {
            match ev {
                Ev::Start { name, prefix, .. } => {
                    field = Some(format!("{prefix}:{name}"));
                    value.clear();
                }
                Ev::Text(t) => value.push_str(t),
                Ev::End { .. } => {
                    let v = value.trim().to_string();
                    if !v.is_empty() {
                        match field.as_deref() {
                            Some("dc:title") => meta.title = meta.title.take().or(Some(v)),
                            Some("dc:creator") => meta.author = meta.author.take().or(Some(v)),
                            Some("meta:initial-creator") => initial_creator = Some(v),
                            Some("meta:keyword") => {
                                meta.keywords = meta.keywords.take().or(Some(v))
                            }
                            Some("meta:creation-date") => meta.date = Some(v),
                            _ => {}
                        }
                    }
                    field = None;
                    value.clear();
                }
            }
            true
        });
        meta.author = meta.author.or(initial_creator);
    }
    let xml = part(bytes, "content.xml")?;
    let mut in_body = false;
    // annotations, tracked deletions and the like are not the document's text
    let mut skipping = 0usize;
    walk(&xml, |ev| {
        match ev {
            Ev::Start { name, prefix, attrs } => {
                if name == "body" && prefix == "office" {
                    in_body = true;
                }
                if !in_body {
                    return true;
                }
                if skipping > 0
                    || matches!(
                        (prefix, name),
                        ("office", "annotation")
                            | ("text", "tracked-changes")
                            | ("text", "note-citation")
                            | ("text", "sequence-decls")
                            | ("office", "forms")
                            | ("svg", "title")
                            | ("svg", "desc")
                    )
                {
                    skipping += 1;
                    return true;
                }
                match (prefix, name) {
                    ("text", "p") => text.start("p"),
                    ("text", "h") => text.start("h1"),
                    ("text", "list") => text.start("ul"),
                    ("text", "list-item") => text.start("li"),
                    ("table", "table") => text.start("table"),
                    ("table", "table-row") => text.start("tr"),
                    ("table", "table-cell") => text.start("td"),
                    ("text", "tab") => text.chars("\t"),
                    ("text", "line-break") => text.chars("\n"),
                    ("text", "s") => {
                        let n = attr(attrs, "text:c")
                            .and_then(|c| c.parse::<usize>().ok())
                            .unwrap_or(1)
                            .min(1000);
                        text.chars(&" ".repeat(n));
                    }
                    _ => {}
                }
            }
            Ev::End { name } => {
                if !in_body {
                    return true;
                }
                if skipping > 0 {
                    skipping -= 1;
                    return true;
                }
                match name {
                    "p" => text.end("p"),
                    "h" => text.end("h1"),
                    "list" => text.end("ul"),
                    "list-item" => text.end("li"),
                    "table" => text.end("table"),
                    "table-row" => text.end("tr"),
                    "table-cell" => text.end("td"),
                    "body" => in_body = false,
                    _ => {}
                }
            }
            Ev::Text(t) => {
                if in_body && skipping == 0 {
                    text.chars(t);
                }
            }
        }
        !text.full()
    })
    .map_err(xml_error)?;
    Ok(meta)
}

/// An EPUB: the package document names the book's parts in reading order,
/// and each part's body is read as HTML into the one text.
pub fn epub(bytes: &[u8], text: &mut Text) -> Result<Meta, Failure> {
    let mut meta = Meta::default();
    let container = part(bytes, "META-INF/container.xml")?;
    let mut opf_path = None;
    let _ = walk(&container, |ev| {
        if let Ev::Start { name: "rootfile", attrs, .. } = ev {
            opf_path = attr(attrs, "full-path").map(|p| p.to_string());
            return false;
        }
        true
    });
    let opf_path = opf_path.ok_or_else(|| missing("rootfile"))?;
    let opf = part(bytes, &opf_path)?;
    let dir = opf_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let mut manifest: Vec<(String, String, String)> = Vec::new();
    let mut spine: Vec<String> = Vec::new();
    let mut field: Option<String> = None;
    let mut value = String::new();
    walk(&opf, |ev| {
        match ev {
            Ev::Start { name, prefix, attrs } => {
                match name {
                    "item" => manifest.push((
                        attr(attrs, "id").unwrap_or("").to_string(),
                        resolve(dir, attr(attrs, "href").unwrap_or("")),
                        attr(attrs, "media-type").unwrap_or("").to_string(),
                    )),
                    "itemref" => spine.push(attr(attrs, "idref").unwrap_or("").to_string()),
                    _ => {}
                }
                field = (prefix == "dc").then(|| name.to_string());
                value.clear();
            }
            Ev::Text(t) => value.push_str(t),
            Ev::End { name } => {
                if field.as_deref() == Some(name) {
                    let v = value.trim().to_string();
                    if !v.is_empty() {
                        match name {
                            "title" => meta.title = meta.title.take().or(Some(v)),
                            "creator" => meta.author = meta.author.take().or(Some(v)),
                            "subject" => meta.keywords = meta.keywords.take().or(Some(v)),
                            "date" => meta.date = meta.date.take().or(Some(v)),
                            _ => {}
                        }
                    }
                }
                field = None;
            }
        }
        true
    })
    .map_err(xml_error)?;
    let readable =
        |media: &str| media.contains("html") || media.contains("xml") && !media.contains("ncx");
    let mut order: Vec<String> = spine
        .iter()
        .filter_map(|id| manifest.iter().find(|(mid, _, media)| mid == id && readable(media)))
        .map(|(_, href, _)| href.clone())
        .collect();
    if order.is_empty() {
        order = manifest
            .iter()
            .filter(|(_, _, media)| media.contains("html"))
            .map(|(_, h, _)| h.clone())
            .collect();
    }
    for href in order {
        if text.full() {
            break;
        }
        if let Some(raw) = zip_entry(bytes, &href) {
            let page = super::xml::decode(&raw);
            super::html::parse(&page, text);
        }
    }
    Ok(meta)
}

/// A Word document written in the older binary format.
pub fn doc(bytes: &[u8], text: &mut Text) -> Result<Meta, Failure> {
    let meta = doc_meta(bytes);
    let body = doc_text(bytes).ok_or_else(|| Failure::tika("Unable to read the Word document"))?;
    text.chars(&without_fields(&body));
    Ok(meta)
}

/// Word keeps a field as its instructions and its result between marks:
/// `\x13` begins the field, `\x14` ends the instructions and `\x15` the
/// field. Only the result is text. The other low characters Word writes
/// stand for pictures, drawings and note references, and are not text either.
fn without_fields(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    // for each field open, whether its instructions are still being read
    let mut fields: Vec<bool> = Vec::new();
    for c in body.chars() {
        match c {
            '\u{13}' => fields.push(true),
            '\u{14}' => {
                if let Some(top) = fields.last_mut() {
                    *top = false;
                }
            }
            '\u{15}' => {
                fields.pop();
            }
            // pictures, note references, page and section breaks
            c if (c as u32) < 0x20 && c != '\t' && c != '\n' => {}
            _ if fields.iter().any(|instructions| *instructions) => {}
            c => out.push(c),
        }
    }
    out
}

/// How many entries a zip's central directory has, and where it starts.
fn central_directory(bytes: &[u8]) -> Option<(usize, usize)> {
    // the end of the directory is at the end of the file, behind a comment
    // of at most 64KiB whose length nobody records anywhere else
    let floor = bytes.len().saturating_sub(22 + 65535);
    let eocd = (floor..bytes.len().saturating_sub(21))
        .rev()
        .find(|at| bytes[*at..].starts_with(b"PK\x05\x06"))?;
    let count = u16::from_le_bytes([*bytes.get(eocd + 10)?, *bytes.get(eocd + 11)?]) as usize;
    let at = u32::from_le_bytes([
        *bytes.get(eocd + 16)?,
        *bytes.get(eocd + 17)?,
        *bytes.get(eocd + 18)?,
        *bytes.get(eocd + 19)?,
    ]) as usize;
    Some((count, at))
}

/// One file out of a zip, by name, read through its central directory.
///
/// Written here rather than taken from a crate: the office formats are the
/// only zips this engine opens, and it opens a few named files out of each.
pub fn zip_entry(bytes: &[u8], want: &str) -> Option<Vec<u8>> {
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
    let (count, mut at) = central_directory(bytes)?;
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
            let start = local.checked_add(30 + local_name_len + local_extra_len)?;
            let data = bytes.get(start..start.checked_add(compressed)?)?;
            return match method {
                0 => Some(data.to_vec()),
                8 => {
                    use std::io::Read;
                    let mut out = Vec::new();
                    flate2::read::DeflateDecoder::new(data)
                        .take(MAX_INFLATED as u64 + 1)
                        .read_to_end(&mut out)
                        .ok()?;
                    if out.len() > MAX_INFLATED {
                        return None;
                    }
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
            for pair in run.as_chunks::<2>().0 {
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
pub fn cp1252(b: u8) -> char {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A zip of the given entries, stored without compression.
    fn zip(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, body) in entries {
            let offset = out.len() as u32;
            let crc = {
                let mut h = flate2::Crc::new();
                h.update(body.as_bytes());
                h.sum()
            };
            let header = |sig: &[u8], central: bool| {
                let mut h = sig.to_vec();
                if central {
                    h.extend_from_slice(&20u16.to_le_bytes());
                }
                h.extend_from_slice(&20u16.to_le_bytes());
                h.extend_from_slice(&0u16.to_le_bytes());
                h.extend_from_slice(&0u16.to_le_bytes());
                h.extend_from_slice(&[0, 0, 0, 0]);
                h.extend_from_slice(&crc.to_le_bytes());
                h.extend_from_slice(&(body.len() as u32).to_le_bytes());
                h.extend_from_slice(&(body.len() as u32).to_le_bytes());
                h.extend_from_slice(&(name.len() as u16).to_le_bytes());
                h.extend_from_slice(&0u16.to_le_bytes());
                if central {
                    // comment length, disk, internal and external attributes
                    h.extend_from_slice(&[0; 10]);
                    h.extend_from_slice(&offset.to_le_bytes());
                }
                h.extend_from_slice(name.as_bytes());
                h
            };
            out.extend_from_slice(&header(b"PK\x03\x04", false));
            out.extend_from_slice(body.as_bytes());
            central.extend_from_slice(&header(b"PK\x01\x02", true));
        }
        let at = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(b"PK\x05\x06\0\0\0\0");
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&at.to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out
    }

    const RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;
    const CORE: &str = r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/"><dc:title>Handbook</dc:title><dc:creator>Priya Nair</dc:creator><cp:keywords>security</cp:keywords><dcterms:created>2026-01-15T09:30:00.000Z</dcterms:created></cp:coreProperties>"#;

    fn extract(bytes: &[u8]) -> (String, String, Meta) {
        let found = super::super::extract(bytes, None).unwrap();
        (found.content_type, found.text, found.meta)
    }

    #[test]
    fn a_word_document_with_a_table_and_a_field() {
        let document = r#"<w:document xmlns:w="w"><w:body><w:p><w:r><w:t>T&amp;C</w:t><w:tab/><w:t>apply</w:t></w:r></w:p><w:tbl><w:tr><w:tc><w:p><w:r><w:t>a</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>b</w:t></w:r></w:p></w:tc></w:tr></w:tbl><w:p><w:r><w:instrText>PAGE</w:instrText><w:delText>gone</w:delText><w:t>end</w:t></w:r></w:p></w:body></w:document>"#;
        let file = zip(&[
            (
                "[Content_Types].xml",
                r#"<Types><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#,
            ),
            ("_rels/.rels", RELS),
            ("word/document.xml", document),
            ("docProps/core.xml", CORE),
        ]);
        let (kind, text, meta) = extract(&file);
        assert_eq!(kind, "application/vnd.openxmlformats-officedocument.wordprocessingml.document");
        assert_eq!(text, "T&C\tapply\n\ta\n\tb\n\n\nend\n");
        assert_eq!(meta.title.as_deref(), Some("Handbook"));
        assert_eq!(meta.author.as_deref(), Some("Priya Nair"));
        assert_eq!(meta.date.as_deref(), Some("2026-01-15T09:30:00Z"));
    }

    #[test]
    fn a_spreadsheet_keeps_its_columns() {
        let file = zip(&[
            (
                "[Content_Types].xml",
                r#"<Types><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/></Types>"#,
            ),
            (
                "_rels/.rels",
                r#"<Relationships><Relationship Id="rId1" Type="http://x/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
            ),
            (
                "xl/workbook.xml",
                r#"<workbook xmlns:r="r"><sheets><sheet name="Budget" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Type="http://x/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://x/sharedStrings" Target="sharedStrings.xml"/></Relationships>"#,
            ),
            (
                "xl/sharedStrings.xml",
                r#"<sst><si><t>Item</t></si><si><r><t>Lap</t></r><r><t>tops</t></r></si></sst>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c><c r="C1" t="b"><v>1</v></c></row><row r="2"><c r="A2" t="s"><v>1</v></c><c r="B2"><v>1200.50</v></c></row></sheetData></worksheet>"#,
            ),
        ]);
        let (kind, text, _) = extract(&file);
        assert_eq!(kind, "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet");
        assert_eq!(text, "Budget\n\tItem\t\tTRUE\n\tLaptops\t1200.5\n\n\n");
    }

    #[test]
    fn a_presentation_and_its_notes() {
        let file = zip(&[
            (
                "[Content_Types].xml",
                r#"<Types><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/></Types>"#,
            ),
            (
                "_rels/.rels",
                r#"<Relationships><Relationship Id="rId1" Type="http://x/officeDocument" Target="ppt/presentation.xml"/></Relationships>"#,
            ),
            (
                "ppt/presentation.xml",
                r#"<p:presentation xmlns:p="p" xmlns:r="r"><p:sldIdLst><p:sldId id="256" r:id="rId2"/></p:sldIdLst></p:presentation>"#,
            ),
            (
                "ppt/_rels/presentation.xml.rels",
                r#"<Relationships><Relationship Id="rId2" Type="http://x/slide" Target="slides/slide1.xml"/></Relationships>"#,
            ),
            (
                "ppt/slides/slide1.xml",
                r#"<p:sld xmlns:p="p" xmlns:a="a"><p:sp><p:txBody><a:p><a:r><a:t>Results</a:t></a:r></a:p></p:txBody></p:sp><p:sp><p:nvSpPr><p:nvPr><p:ph type="sldNum"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>7</a:t></a:r></a:p></p:txBody></p:sp></p:sld>"#,
            ),
            (
                "ppt/slides/_rels/slide1.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Type="http://x/notesSlide" Target="../notesSlides/notesSlide1.xml"/></Relationships>"#,
            ),
            (
                "ppt/notesSlides/notesSlide1.xml",
                r#"<p:notes xmlns:p="p" xmlns:a="a"><p:sp><p:txBody><a:p><a:r><a:t>Say hello</a:t></a:r></a:p></p:txBody></p:sp></p:notes>"#,
            ),
        ]);
        let (_, text, _) = extract(&file);
        assert_eq!(text, "Results\n\nSay hello\n\n");
    }

    #[test]
    fn opendocument_and_epub() {
        let odt = zip(&[
            ("mimetype", "application/vnd.oasis.opendocument.text"),
            (
                "content.xml",
                r#"<office:document-content xmlns:office="o" xmlns:text="t"><office:body><office:text><text:h>Title</text:h><text:p>one<text:s text:c="2"/>two<text:tab/>three</text:p><office:annotation><text:p>note</text:p></office:annotation></office:text></office:body></office:document-content>"#,
            ),
            (
                "meta.xml",
                r#"<office:document-meta xmlns:office="o" xmlns:meta="m" xmlns:dc="d"><office:meta><dc:title>Doc</dc:title><meta:initial-creator>Ada</meta:initial-creator><meta:keyword>k1</meta:keyword><meta:creation-date>2026-01-15T09:30:00</meta:creation-date></office:meta></office:document-meta>"#,
            ),
        ]);
        let (kind, text, meta) = extract(&odt);
        assert_eq!(kind, "application/vnd.oasis.opendocument.text");
        assert_eq!(text, "Title\none  two\tthree\n");
        assert_eq!(
            (meta.title.as_deref(), meta.author.as_deref(), meta.keywords.as_deref()),
            (Some("Doc"), Some("Ada"), Some("k1"))
        );

        let epub = zip(&[
            ("mimetype", "application/epub+zip"),
            (
                "META-INF/container.xml",
                r#"<container><rootfiles><rootfile full-path="OPS/book.opf"/></rootfiles></container>"#,
            ),
            (
                "OPS/book.opf",
                r#"<package xmlns:dc="http://purl.org/dc/elements/1.1/"><metadata><dc:title>Book</dc:title><dc:creator>Author</dc:creator><dc:subject>fiction</dc:subject><dc:date>2009-10-07</dc:date></metadata><manifest><item id="c2" href="c2.xhtml" media-type="application/xhtml+xml"/><item id="c1" href="c1.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="c1"/><itemref idref="c2"/></spine></package>"#,
            ),
            (
                "OPS/c1.xhtml",
                "<html><head><title>x</title></head><body><p>Chapter one</p></body></html>",
            ),
            ("OPS/c2.xhtml", "<html><body><p>Chapter two</p></body></html>"),
        ]);
        let (kind, text, meta) = extract(&epub);
        assert_eq!(kind, "application/epub+zip");
        assert_eq!(text, "Chapter one\nChapter two\n");
        assert_eq!(meta.date.as_deref(), Some("2009-10-07"));
        assert_eq!(meta.keywords.as_deref(), Some("fiction"));
    }

    #[test]
    fn word_fields_leave_their_results() {
        assert_eq!(without_fields("a\u{13} PAGE \u{14}3\u{15}b\u{1}c"), "a3bc");
        assert_eq!(without_fields("\u{13}TOC \u{13}inner\u{14}x\u{15}\u{14}shown\u{15}"), "shown");
    }

    #[test]
    fn damaged_zips_end_quickly() {
        let file = zip(&[
            ("mimetype", "application/epub+zip"),
            ("META-INF/container.xml", "<container/>"),
        ]);
        for cut in 0..file.len() {
            let _ = super::super::extract(&file[..cut], None);
        }
    }
}
