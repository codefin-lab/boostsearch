//! HTML, read the way Tika's jsoup parser and its HTML handler read it.
//!
//! Only the body is text. Of the elements in it, Tika keeps a short list it
//! calls safe -- paragraphs, headings, lists, tables, links -- and passes the
//! text of every other element through without the element, so a `<div>` or
//! a `<span>` adds nothing between its words and the next. `<script>` and
//! `<style>` are dropped with what is in them. The title comes from `<title>`,
//! and the author and keywords from `<meta>` tags named exactly `Author` and
//! `Keywords`: Tika keeps meta names as written, and the processor looks them
//! up as written.
//!
//! This is not a full HTML5 tree builder. It closes what the common implied
//! end tags close -- a paragraph before a block, a list item before the next,
//! a cell before the next -- which is what decides where the newlines go.

use super::{Meta, Text};

/// The elements Tika's default mapper keeps, by the name it keeps them as.
const SAFE: &[&str] = &[
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "p",
    "pre",
    "blockquote",
    "q",
    "ul",
    "ol",
    "li",
    "dl",
    "dt",
    "dd",
    "table",
    "thead",
    "tbody",
    "tr",
    "th",
    "td",
    "address",
    "a",
    "map",
    "area",
    "img",
    "frameset",
    "frame",
    "iframe",
    "object",
    "param",
    "ins",
    "del",
];

/// Elements with no end tag.
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr", "keygen", "basefont", "bgsound", "frame",
];

/// Elements whose start closes an open paragraph.
const CLOSES_P: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "center",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "main",
    "menu",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "ul",
    "li",
    "dd",
    "dt",
    "listing",
    "xmp",
    "plaintext",
];

/// Elements that belong in the head when they come before the body does.
const HEAD: &[&str] = &["title", "meta", "link", "base", "script", "style", "noscript", "template"];

/// Elements whose content is not markup.
const RAW: &[&str] = &["script", "style", "xmp", "iframe", "noembed", "noframes"];
const RCDATA: &[&str] = &["title", "textarea"];

#[derive(PartialEq)]
enum Mode {
    /// before the body has begun
    Head,
    Body,
    /// after `</body>` or `</html>`
    After,
}

struct Builder<'a> {
    text: &'a mut Text,
    meta: Meta,
    mode: Mode,
    stack: Vec<String>,
    discard: usize,
    title: Option<String>,
}

/// Read a page, writing its body into `text`.
pub fn parse(html: &str, text: &mut Text) -> Meta {
    let mut b = Builder {
        text,
        meta: Meta::default(),
        mode: Mode::Head,
        stack: Vec::new(),
        discard: 0,
        title: None,
    };
    // HTML reads a carriage return, with or without the newline after it, as
    // a newline
    let source = html.replace("\r\n", "\n").replace('\r', "\n");
    let s = source.as_str();
    let mut at = 0;
    while at < s.len() {
        if b.text.full() && b.mode != Mode::Head {
            break;
        }
        let rest = &s[at..];
        if !rest.starts_with('<') {
            let end = rest.find('<').unwrap_or(rest.len());
            b.characters(&decode_entities(&rest[..end], false));
            at += end;
            continue;
        }
        if let Some(body) = rest.strip_prefix("<!--") {
            at += 4 + body.find("-->").map(|e| e + 3).unwrap_or(body.len());
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            at += rest.find('>').map(|e| e + 1).unwrap_or(rest.len());
            continue;
        }
        let closing = rest.starts_with("</");
        let name_from = if closing { 2 } else { 1 };
        let name: String = rest[name_from..]
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '>' && *c != '/')
            .collect();
        if name.is_empty() || !name.starts_with(|c: char| c.is_ascii_alphabetic()) {
            // a `<` that does not open a tag is text
            b.characters("<");
            at += 1;
            continue;
        }
        let name = name.to_ascii_lowercase();
        let (attrs, tag_len) = attributes(&rest[name_from + name.len()..]);
        at += name_from + name.len() + tag_len;
        if closing {
            b.end_tag(&name);
            continue;
        }
        b.start_tag(&name, &attrs);
        if RAW.contains(&name.as_str()) || RCDATA.contains(&name.as_str()) {
            // everything up to the matching end tag is the element's text
            let lower = s[at..].to_ascii_lowercase();
            let close = format!("</{name}");
            let end = lower.find(&close).unwrap_or(lower.len());
            let inner = &s[at..at + end];
            if RCDATA.contains(&name.as_str()) {
                b.characters(&decode_entities(inner, false));
            } else {
                b.characters(inner);
            }
            at += end;
            if at < s.len() {
                let after = s[at..].find('>').map(|e| e + 1).unwrap_or(s.len() - at);
                at += after;
            }
            b.end_tag(&name);
        }
    }
    while let Some(open) = b.stack.pop() {
        b.closed(&open);
    }
    b.meta
}

/// The attributes of a start tag, and how many bytes the rest of the tag
/// took.
fn attributes(s: &str) -> (Vec<(String, String)>, usize) {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        if i >= bytes.len() {
            return (out, i);
        }
        if bytes[i] == b'>' {
            return (out, i + 1);
        }
        let start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && !matches!(bytes[i], b'=' | b'>' | b'/')
        {
            i += 1;
        }
        let name = s[start..i].to_ascii_lowercase();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let quote = bytes[i];
                let from = i + 1;
                let end = bytes[from..]
                    .iter()
                    .position(|b| *b == quote)
                    .map(|e| from + e)
                    .unwrap_or(bytes.len());
                value = decode_entities(&s[from..end], true);
                i = (end + 1).min(bytes.len());
            } else {
                let from = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
                    i += 1;
                }
                value = decode_entities(&s[from..i], true);
            }
        }
        if !name.is_empty() {
            out.push((name, value));
        }
    }
}

impl Builder<'_> {
    fn enter_body(&mut self) {
        if self.mode == Mode::Head {
            self.mode = Mode::Body;
        }
    }

    fn characters(&mut self, s: &str) {
        if let Some(title) = self.title.as_mut() {
            title.push_str(s);
            return;
        }
        match self.mode {
            Mode::Head | Mode::After if s.chars().all(|c| c.is_ascii_whitespace()) => {
                return;
            }
            Mode::Head if self.discard > 0 => return,
            _ => self.enter_body(),
        }
        if self.mode == Mode::After {
            self.mode = Mode::Body;
        }
        if self.discard == 0 {
            self.text.chars(s);
        }
    }

    fn start_tag(&mut self, name: &str, attrs: &[(String, String)]) {
        let attr = |key: &str| attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        match name {
            "html" | "head" => return,
            "body" | "frameset" => {
                self.enter_body();
                if name == "body" {
                    return;
                }
            }
            _ => {}
        }
        if self.mode == Mode::Head && HEAD.contains(&name) {
            match name {
                "title" => self.title = Some(String::new()),
                "meta" => {
                    if let (Some(key), Some(content)) = (attr("name"), attr("content")) {
                        // the processor asks for these two names, spelled so
                        match key.as_str() {
                            "Author" if self.meta.author.is_none() => {
                                self.meta.author = Some(content)
                            }
                            "Keywords" if self.meta.keywords.is_none() => {
                                self.meta.keywords = Some(content)
                            }
                            _ => {}
                        }
                    }
                }
                "script" | "style" | "noscript" | "template" => self.discarding(name),
                _ => {}
            }
            return;
        }
        self.enter_body();
        if self.mode == Mode::After {
            self.mode = Mode::Body;
        }
        self.implied_ends(name);
        if name == "script" || name == "style" {
            self.discarding(name);
            return;
        }
        if self.discard == 0 && SAFE.contains(&name) {
            self.text.start(name);
        }
        if VOID.contains(&name) {
            if self.discard == 0 && SAFE.contains(&name) {
                self.text.end(name);
            }
        } else {
            self.stack.push(name.to_string());
        }
    }

    fn end_tag(&mut self, name: &str) {
        if name == "title" && self.title.is_some() {
            let title = self.title.take().unwrap_or_default();
            if self.meta.title.is_none() {
                self.meta.title = Some(title.trim().to_string());
            }
            return;
        }
        match name {
            "body" | "html" => {
                if self.mode == Mode::Body {
                    self.mode = Mode::After;
                }
                return;
            }
            "head" => return,
            _ => {}
        }
        let dropped = format!("#{name}");
        if let Some(open) = self.stack.iter().rev().find(|open| **open == name || **open == dropped)
        {
            let open = open.clone();
            self.pop_until(&open);
        } else if name == "p" && self.mode != Mode::Head {
            // an end tag with no start makes an empty paragraph
            self.text.start("p");
            self.text.end("p");
        }
    }

    fn pop_until(&mut self, name: &str) {
        while let Some(open) = self.stack.pop() {
            let done = open == name;
            self.closed(&open);
            if done {
                break;
            }
        }
    }

    /// Open an element whose content is dropped. It is kept on the stack
    /// under a marked name, so that closing it knows to stop dropping.
    fn discarding(&mut self, name: &str) {
        self.stack.push(format!("#{name}"));
        self.discard += 1;
    }

    fn closed(&mut self, open: &str) {
        if open.starts_with('#') {
            self.discard = self.discard.saturating_sub(1);
            return;
        }
        if self.discard == 0 && SAFE.contains(&open) {
            self.text.end(open);
        }
    }

    /// Close what the start of `name` closes.
    fn implied_ends(&mut self, name: &str) {
        let in_scope = |stack: &[String], target: &[&str], boundary: &[&str]| -> Option<String> {
            for open in stack.iter().rev() {
                if target.contains(&open.as_str()) {
                    return Some(open.clone());
                }
                if boundary.contains(&open.as_str()) {
                    return None;
                }
            }
            None
        };
        const BUTTON_SCOPE: &[&str] =
            &["table", "td", "th", "caption", "html", "button", "marquee", "object", "applet"];
        if CLOSES_P.contains(&name)
            && let Some(p) = in_scope(&self.stack, &["p"], BUTTON_SCOPE)
        {
            self.pop_until(&p);
        }
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                if self
                    .stack
                    .last()
                    .is_some_and(|l| matches!(l.as_str(), "h1" | "h2" | "h3" | "h4" | "h5" | "h6"))
                {
                    let open = self.stack.last().cloned().unwrap_or_default();
                    self.pop_until(&open);
                }
            }
            "li" => {
                if let Some(li) =
                    in_scope(&self.stack, &["li"], &["ul", "ol", "table", "td", "th", "html"])
                {
                    self.pop_until(&li);
                }
            }
            "dd" | "dt" => {
                if let Some(d) =
                    in_scope(&self.stack, &["dd", "dt"], &["dl", "table", "td", "th", "html"])
                {
                    self.pop_until(&d);
                }
            }
            "tr" => {
                if let Some(r) = in_scope(&self.stack, &["tr"], &["table", "html"]) {
                    self.pop_until(&r);
                }
            }
            "td" | "th" => {
                if let Some(c) = in_scope(&self.stack, &["td", "th"], &["tr", "table", "html"]) {
                    self.pop_until(&c);
                }
            }
            "option" if self.stack.last().is_some_and(|l| l == "option") => {
                self.pop_until("option");
            }
            _ => {}
        }
    }
}

/// Character references replaced by the characters they stand for.
pub fn decode_entities(s: &str, in_attribute: bool) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        if let Some((c, used)) = reference(rest, in_attribute) {
            out.push_str(&c);
            rest = &rest[used..];
        } else {
            out.push('&');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

fn reference(s: &str, in_attribute: bool) -> Option<(String, usize)> {
    let body = &s[1..];
    if let Some(num) = body.strip_prefix('#') {
        let (hex, digits) = match num.strip_prefix(['x', 'X']) {
            Some(h) => (true, h),
            None => (false, num),
        };
        let len = digits
            .chars()
            .take_while(|c| if hex { c.is_ascii_hexdigit() } else { c.is_ascii_digit() })
            .count();
        if len == 0 {
            return None;
        }
        let value =
            u32::from_str_radix(&digits[..len.min(8)], if hex { 16 } else { 10 }).unwrap_or(0xFFFD);
        let mut used = 1 + 1 + usize::from(hex) + len;
        if digits[len..].starts_with(';') {
            used += 1;
        }
        let c = match value {
            0 => '\u{FFFD}',
            0x80..=0x9F => super::office::cp1252(value as u8),
            v => char::from_u32(v).unwrap_or('\u{FFFD}'),
        };
        return Some((c.to_string(), used));
    }
    let len = body.chars().take_while(|c| c.is_ascii_alphanumeric()).count();
    if len == 0 {
        return None;
    }
    let word = &body[..len];
    if body[len..].starts_with(';') {
        if let Some((_, c)) = ENTITIES.iter().find(|(n, _)| *n == word) {
            return Some((c.to_string(), len + 2));
        }
        return None;
    }
    // the older entities may be written without their semicolon, but not in
    // an attribute where a letter or `=` follows
    for take in (2..=len).rev() {
        let prefix = &body[..take];
        if let Some((_, c)) = ENTITIES.iter().find(|(n, ch)| *n == prefix && (*ch as u32) < 256) {
            let next = body[take..].chars().next();
            if in_attribute && next.is_some_and(|n| n.is_ascii_alphanumeric() || n == '=') {
                return None;
            }
            return Some((c.to_string(), take + 1));
        }
    }
    None
}

const ENTITIES: &[(&str, char)] = &[
    ("AElig", '\u{C6}'),
    ("Aacute", '\u{C1}'),
    ("Acirc", '\u{C2}'),
    ("Agrave", '\u{C0}'),
    ("Alpha", '\u{391}'),
    ("Aring", '\u{C5}'),
    ("Atilde", '\u{C3}'),
    ("Auml", '\u{C4}'),
    ("Beta", '\u{392}'),
    ("Ccedil", '\u{C7}'),
    ("Chi", '\u{3A7}'),
    ("Dagger", '\u{2021}'),
    ("Delta", '\u{394}'),
    ("ETH", '\u{D0}'),
    ("Eacute", '\u{C9}'),
    ("Ecirc", '\u{CA}'),
    ("Egrave", '\u{C8}'),
    ("Epsilon", '\u{395}'),
    ("Eta", '\u{397}'),
    ("Euml", '\u{CB}'),
    ("Gamma", '\u{393}'),
    ("Iacute", '\u{CD}'),
    ("Icirc", '\u{CE}'),
    ("Igrave", '\u{CC}'),
    ("Iota", '\u{399}'),
    ("Iuml", '\u{CF}'),
    ("Kappa", '\u{39A}'),
    ("Lambda", '\u{39B}'),
    ("Mu", '\u{39C}'),
    ("Ntilde", '\u{D1}'),
    ("Nu", '\u{39D}'),
    ("OElig", '\u{152}'),
    ("Oacute", '\u{D3}'),
    ("Ocirc", '\u{D4}'),
    ("Ograve", '\u{D2}'),
    ("Omega", '\u{3A9}'),
    ("Omicron", '\u{39F}'),
    ("Oslash", '\u{D8}'),
    ("Otilde", '\u{D5}'),
    ("Ouml", '\u{D6}'),
    ("Phi", '\u{3A6}'),
    ("Pi", '\u{3A0}'),
    ("Prime", '\u{2033}'),
    ("Psi", '\u{3A8}'),
    ("Rho", '\u{3A1}'),
    ("Scaron", '\u{160}'),
    ("Sigma", '\u{3A3}'),
    ("THORN", '\u{DE}'),
    ("Tau", '\u{3A4}'),
    ("Theta", '\u{398}'),
    ("Uacute", '\u{DA}'),
    ("Ucirc", '\u{DB}'),
    ("Ugrave", '\u{D9}'),
    ("Upsilon", '\u{3A5}'),
    ("Uuml", '\u{DC}'),
    ("Xi", '\u{39E}'),
    ("Yacute", '\u{DD}'),
    ("Yuml", '\u{178}'),
    ("Zeta", '\u{396}'),
    ("aacute", '\u{E1}'),
    ("acirc", '\u{E2}'),
    ("acute", '\u{B4}'),
    ("aelig", '\u{E6}'),
    ("agrave", '\u{E0}'),
    ("alefsym", '\u{2135}'),
    ("alpha", '\u{3B1}'),
    ("amp", '\u{26}'),
    ("and", '\u{2227}'),
    ("ang", '\u{2220}'),
    ("apos", '\u{27}'),
    ("aring", '\u{E5}'),
    ("asymp", '\u{2248}'),
    ("atilde", '\u{E3}'),
    ("auml", '\u{E4}'),
    ("bdquo", '\u{201E}'),
    ("beta", '\u{3B2}'),
    ("brvbar", '\u{A6}'),
    ("bull", '\u{2022}'),
    ("cap", '\u{2229}'),
    ("ccedil", '\u{E7}'),
    ("cedil", '\u{B8}'),
    ("cent", '\u{A2}'),
    ("chi", '\u{3C7}'),
    ("circ", '\u{2C6}'),
    ("clubs", '\u{2663}'),
    ("cong", '\u{2245}'),
    ("copy", '\u{A9}'),
    ("crarr", '\u{21B5}'),
    ("cup", '\u{222A}'),
    ("curren", '\u{A4}'),
    ("dArr", '\u{21D3}'),
    ("dagger", '\u{2020}'),
    ("darr", '\u{2193}'),
    ("deg", '\u{B0}'),
    ("delta", '\u{3B4}'),
    ("diams", '\u{2666}'),
    ("divide", '\u{F7}'),
    ("eacute", '\u{E9}'),
    ("ecirc", '\u{EA}'),
    ("egrave", '\u{E8}'),
    ("empty", '\u{2205}'),
    ("emsp", '\u{2003}'),
    ("ensp", '\u{2002}'),
    ("epsilon", '\u{3B5}'),
    ("equiv", '\u{2261}'),
    ("eta", '\u{3B7}'),
    ("eth", '\u{F0}'),
    ("euml", '\u{EB}'),
    ("euro", '\u{20AC}'),
    ("exist", '\u{2203}'),
    ("fnof", '\u{192}'),
    ("forall", '\u{2200}'),
    ("frac12", '\u{BD}'),
    ("frac14", '\u{BC}'),
    ("frac34", '\u{BE}'),
    ("frasl", '\u{2044}'),
    ("gamma", '\u{3B3}'),
    ("ge", '\u{2265}'),
    ("gt", '\u{3E}'),
    ("hArr", '\u{21D4}'),
    ("harr", '\u{2194}'),
    ("hearts", '\u{2665}'),
    ("hellip", '\u{2026}'),
    ("iacute", '\u{ED}'),
    ("icirc", '\u{EE}'),
    ("iexcl", '\u{A1}'),
    ("igrave", '\u{EC}'),
    ("image", '\u{2111}'),
    ("infin", '\u{221E}'),
    ("int", '\u{222B}'),
    ("iota", '\u{3B9}'),
    ("iquest", '\u{BF}'),
    ("isin", '\u{2208}'),
    ("iuml", '\u{EF}'),
    ("kappa", '\u{3BA}'),
    ("lArr", '\u{21D0}'),
    ("lambda", '\u{3BB}'),
    ("lang", '\u{2329}'),
    ("laquo", '\u{AB}'),
    ("larr", '\u{2190}'),
    ("lceil", '\u{2308}'),
    ("ldquo", '\u{201C}'),
    ("le", '\u{2264}'),
    ("lfloor", '\u{230A}'),
    ("lowast", '\u{2217}'),
    ("loz", '\u{25CA}'),
    ("lrm", '\u{200E}'),
    ("lsaquo", '\u{2039}'),
    ("lsquo", '\u{2018}'),
    ("lt", '\u{3C}'),
    ("macr", '\u{AF}'),
    ("mdash", '\u{2014}'),
    ("micro", '\u{B5}'),
    ("middot", '\u{B7}'),
    ("minus", '\u{2212}'),
    ("mu", '\u{3BC}'),
    ("nabla", '\u{2207}'),
    ("nbsp", '\u{A0}'),
    ("ndash", '\u{2013}'),
    ("ne", '\u{2260}'),
    ("ni", '\u{220B}'),
    ("not", '\u{AC}'),
    ("notin", '\u{2209}'),
    ("nsub", '\u{2284}'),
    ("ntilde", '\u{F1}'),
    ("nu", '\u{3BD}'),
    ("oacute", '\u{F3}'),
    ("ocirc", '\u{F4}'),
    ("oelig", '\u{153}'),
    ("ograve", '\u{F2}'),
    ("oline", '\u{203E}'),
    ("omega", '\u{3C9}'),
    ("omicron", '\u{3BF}'),
    ("oplus", '\u{2295}'),
    ("or", '\u{2228}'),
    ("ordf", '\u{AA}'),
    ("ordm", '\u{BA}'),
    ("oslash", '\u{F8}'),
    ("otilde", '\u{F5}'),
    ("otimes", '\u{2297}'),
    ("ouml", '\u{F6}'),
    ("para", '\u{B6}'),
    ("part", '\u{2202}'),
    ("permil", '\u{2030}'),
    ("perp", '\u{22A5}'),
    ("phi", '\u{3C6}'),
    ("pi", '\u{3C0}'),
    ("piv", '\u{3D6}'),
    ("plusmn", '\u{B1}'),
    ("pound", '\u{A3}'),
    ("prime", '\u{2032}'),
    ("prod", '\u{220F}'),
    ("prop", '\u{221D}'),
    ("psi", '\u{3C8}'),
    ("quot", '\u{22}'),
    ("rArr", '\u{21D2}'),
    ("radic", '\u{221A}'),
    ("rang", '\u{232A}'),
    ("raquo", '\u{BB}'),
    ("rarr", '\u{2192}'),
    ("rceil", '\u{2309}'),
    ("rdquo", '\u{201D}'),
    ("real", '\u{211C}'),
    ("reg", '\u{AE}'),
    ("rfloor", '\u{230B}'),
    ("rho", '\u{3C1}'),
    ("rlm", '\u{200F}'),
    ("rsaquo", '\u{203A}'),
    ("rsquo", '\u{2019}'),
    ("sbquo", '\u{201A}'),
    ("scaron", '\u{161}'),
    ("sdot", '\u{22C5}'),
    ("sect", '\u{A7}'),
    ("shy", '\u{AD}'),
    ("sigma", '\u{3C3}'),
    ("sigmaf", '\u{3C2}'),
    ("sim", '\u{223C}'),
    ("spades", '\u{2660}'),
    ("sub", '\u{2282}'),
    ("sube", '\u{2286}'),
    ("sum", '\u{2211}'),
    ("sup", '\u{2283}'),
    ("sup1", '\u{B9}'),
    ("sup2", '\u{B2}'),
    ("sup3", '\u{B3}'),
    ("supe", '\u{2287}'),
    ("szlig", '\u{DF}'),
    ("tau", '\u{3C4}'),
    ("there4", '\u{2234}'),
    ("theta", '\u{3B8}'),
    ("thetasym", '\u{3D1}'),
    ("thinsp", '\u{2009}'),
    ("thorn", '\u{FE}'),
    ("tilde", '\u{2DC}'),
    ("times", '\u{D7}'),
    ("trade", '\u{2122}'),
    ("uArr", '\u{21D1}'),
    ("uacute", '\u{FA}'),
    ("uarr", '\u{2191}'),
    ("ucirc", '\u{FB}'),
    ("ugrave", '\u{F9}'),
    ("uml", '\u{A8}'),
    ("upsih", '\u{3D2}'),
    ("upsilon", '\u{3C5}'),
    ("uuml", '\u{FC}'),
    ("weierp", '\u{2118}'),
    ("xi", '\u{3BE}'),
    ("yacute", '\u{FD}'),
    ("yen", '\u{A5}'),
    ("yuml", '\u{FF}'),
    ("zeta", '\u{3B6}'),
    ("zwj", '\u{200D}'),
    ("zwnj", '\u{200C}'),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn read(page: &str) -> (String, Meta) {
        let mut text = Text::new(None);
        let meta = parse(page, &mut text);
        (text.out, meta)
    }

    #[test]
    fn body_text_with_head_metadata() {
        let page = "<!DOCTYPE HTML>\n<html lang=\"fr\">\n<head>\n    <title>Hello</title>\n    \
                    <meta name=\"date\" content=\"\">\n    <meta name=\"Author\" content=\"foobar\">\n    \
                    <meta name=\"Keywords\" content=\"opensearch,cool,bonsai\">\n</head>\n\
                    <body>Hello again. This is a test sentence.</body>\n</html>\n";
        let (text, meta) = read(page);
        assert_eq!(text, "Hello again. This is a test sentence.");
        assert_eq!(meta.title.as_deref(), Some("Hello"));
        assert_eq!(meta.author.as_deref(), Some("foobar"));
        assert_eq!(meta.keywords.as_deref(), Some("opensearch,cool,bonsai"));
    }

    #[test]
    fn scripts_and_styles_are_dropped_and_blocks_end_lines() {
        let page = "<html><head><style>p{color:red}</style><script>var x = '<p>';</script></head>\
                    <body><h1>T&amp;C</h1><p>one<p>two</p><div>a</div><div>b</div>\
                    <ul><li>x<li>y</ul><table><tr><td>1<td>2</table>\
                    <script>alert(1)</script></body></html>";
        let (text, _) = read(page);
        assert_eq!(text, "T&C\none\ntwo\nab\t x\n\ty\n\n\t1\t2\n\n".replace("\t x", "\tx"));
    }

    #[test]
    fn entities_and_utf8() {
        let (text, _) =
            read("<p>caf&eacute; &#x0E44;&#3607;&#xE22; &copy 2026 &nbsp;&notanentity;</p>");
        assert_eq!(text, "caf\u{e9} \u{e44}\u{e17}\u{e22} \u{a9} 2026 \u{a0}&notanentity;\n");
    }
}
