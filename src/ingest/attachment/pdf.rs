//! PDF, read the way Tika reads it through PDFBox's text stripper.
//!
//! A PDF has no text in the sense a text file has. It has pages, and each
//! page a content stream of drawing operators, some of which show glyphs of
//! a font at a position. The text is rebuilt from those positions: a glyph
//! that does not overlap the height of the line before starts a new line, a
//! gap wider than a fraction of a space starts a new word, and a line that
//! drops much further than a line would, or is indented, starts a new
//! paragraph. Tika writes each page as a `<div>` and each paragraph as a
//! `<p>`, with a newline at the end of every line -- so a paragraph break
//! reads as a blank line, and a page ends in two newlines.
//!
//! What a glyph's character is comes from the font: its ToUnicode map when
//! it has one, and otherwise its encoding and the glyph names in it.
//!
//! The file structure is read leniently, as PDFBox reads it: the
//! cross-reference table is followed when it is sound, and when it is not the
//! file is scanned for its objects. An encrypted file is opened with the
//! empty password when it has one; a file that needs a real password is
//! refused as the reference refuses it.

mod tables;

use super::{Failure, Meta, Text};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// The most any one stream may decode to.
const MAX_DECODED: usize = 64 << 20;
/// How deeply objects may nest, and form XObjects may call one another.
const MAX_DEPTH: usize = 64;

// ---- objects ---------------------------------------------------------------

#[derive(Clone, Debug)]
enum Obj {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    Str(Vec<u8>),
    Name(Vec<u8>),
    Array(Vec<Obj>),
    Dict(Dict),
    Stream(Rc<Stream>),
    /// a reference to an object, by number: the generation is not needed to
    /// find one
    Ref(u32),
    /// a content stream operator
    Op(Vec<u8>),
}

type Dict = Vec<(Vec<u8>, Obj)>;

#[derive(Debug)]
struct Stream {
    dict: Dict,
    /// where the encoded data sits in the file
    start: usize,
    end: usize,
    /// the object it is, for decryption
    id: (u32, u16),
}

fn get<'a>(d: &'a Dict, key: &str) -> Option<&'a Obj> {
    d.iter().find(|(k, _)| k == key.as_bytes()).map(|(_, v)| v)
}

impl Obj {
    fn num(&self) -> Option<f64> {
        match self {
            Obj::Int(i) => Some(*i as f64),
            Obj::Real(r) => Some(*r),
            _ => None,
        }
    }

    fn name(&self) -> Option<&[u8]> {
        match self {
            Obj::Name(n) => Some(n),
            _ => None,
        }
    }
}

// ---- lexing ----------------------------------------------------------------

fn is_ws(b: u8) -> bool {
    matches!(b, 0 | b'\t' | b'\n' | 0x0c | b'\r' | b' ')
}

fn is_delim(b: u8) -> bool {
    matches!(b, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%')
}

struct Lexer<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn new(b: &'a [u8], pos: usize) -> Lexer<'a> {
        Lexer { b, pos }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.b.len() {
            let c = self.b[self.pos];
            if is_ws(c) {
                self.pos += 1;
            } else if c == b'%' {
                while self.pos < self.b.len() && !matches!(self.b[self.pos], b'\r' | b'\n') {
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    fn regular(&mut self) -> &'a [u8] {
        let start = self.pos;
        while self.pos < self.b.len() && !is_ws(self.b[self.pos]) && !is_delim(self.b[self.pos]) {
            self.pos += 1;
        }
        &self.b[start..self.pos]
    }

    /// One object, or an operator where one stands in a content stream.
    fn object(&mut self, depth: usize) -> Option<Obj> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.skip_ws();
        let c = self.peek()?;
        match c {
            b'/' => {
                self.pos += 1;
                let raw = self.regular();
                let mut name = Vec::with_capacity(raw.len());
                let mut i = 0;
                while i < raw.len() {
                    if raw[i] == b'#'
                        && let Some(h) = raw.get(i + 1..i + 3)
                        && let Ok(v) =
                            u8::from_str_radix(std::str::from_utf8(h).unwrap_or("zz"), 16)
                    {
                        name.push(v);
                        i += 3;
                        continue;
                    }
                    name.push(raw[i]);
                    i += 1;
                }
                Some(Obj::Name(name))
            }
            b'(' => Some(Obj::Str(self.literal())),
            b'<' => {
                if self.b.get(self.pos + 1) == Some(&b'<') {
                    self.pos += 2;
                    let mut dict = Dict::new();
                    loop {
                        self.skip_ws();
                        match self.peek() {
                            None => break,
                            Some(b'>') => {
                                self.pos +=
                                    if self.b.get(self.pos + 1) == Some(&b'>') { 2 } else { 1 };
                                break;
                            }
                            Some(b'/') => {
                                let Some(Obj::Name(key)) = self.object(depth + 1) else { break };
                                self.skip_ws();
                                if self.peek() == Some(b'>') {
                                    dict.push((key, Obj::Null));
                                    continue;
                                }
                                let value = self.object(depth + 1).unwrap_or(Obj::Null);
                                dict.push((key, value));
                            }
                            Some(_) => {
                                // a stray token where a key should be
                                let before = self.pos;
                                let _ = self.object(depth + 1);
                                if self.pos == before {
                                    self.pos += 1;
                                }
                            }
                        }
                    }
                    Some(Obj::Dict(dict))
                } else {
                    self.pos += 1;
                    let mut out = Vec::new();
                    let mut high: Option<u8> = None;
                    while let Some(c) = self.peek() {
                        self.pos += 1;
                        if c == b'>' {
                            break;
                        }
                        let v = match c {
                            b'0'..=b'9' => c - b'0',
                            b'a'..=b'f' => c - b'a' + 10,
                            b'A'..=b'F' => c - b'A' + 10,
                            _ => continue,
                        };
                        match high.take() {
                            Some(h) => out.push(h << 4 | v),
                            None => high = Some(v),
                        }
                    }
                    if let Some(h) = high {
                        out.push(h << 4);
                    }
                    Some(Obj::Str(out))
                }
            }
            b'[' => {
                self.pos += 1;
                let mut items = Vec::new();
                loop {
                    self.skip_ws();
                    match self.peek() {
                        None => break,
                        Some(b']') => {
                            self.pos += 1;
                            break;
                        }
                        Some(_) => {
                            let before = self.pos;
                            match self.object(depth + 1) {
                                Some(o) => items.push(o),
                                None => {
                                    if self.pos == before {
                                        self.pos += 1;
                                    }
                                }
                            }
                        }
                    }
                }
                Some(Obj::Array(items))
            }
            b'0'..=b'9' | b'+' | b'-' | b'.' => {
                let raw = self.regular();
                let text = std::str::from_utf8(raw).ok()?;
                if let Ok(i) = text.parse::<i64>() {
                    // an integer may be the first of `n g R`
                    if i >= 0 {
                        let save = self.pos;
                        self.skip_ws();
                        let generation = self.regular();
                        if !generation.is_empty() && generation.iter().all(u8::is_ascii_digit) {
                            self.skip_ws();
                            if self.peek() == Some(b'R')
                                && self
                                    .b
                                    .get(self.pos + 1)
                                    .is_none_or(|n| is_ws(*n) || is_delim(*n))
                            {
                                self.pos += 1;
                                return Some(Obj::Ref(i.min(u32::MAX as i64) as u32));
                            }
                        }
                        self.pos = save;
                    }
                    return Some(Obj::Int(i));
                }
                // PDFBox reads `--5` and `5.3.2` as best it can; so does this
                let cleaned: String =
                    text.chars().filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-').collect();
                let negative = cleaned.starts_with('-');
                let digits = cleaned.trim_start_matches('-');
                let mut parts = digits.splitn(2, '.');
                let whole = parts.next().unwrap_or("");
                let frac: String =
                    parts.next().unwrap_or("").chars().filter(|c| *c != '.').collect();
                let v = format!(
                    "{}{}.{}",
                    if negative { "-" } else { "" },
                    if whole.is_empty() { "0" } else { whole },
                    frac
                );
                Some(Obj::Real(v.parse::<f64>().unwrap_or(0.0)))
            }
            b')' | b'>' | b']' | b'}' | b'{' => {
                self.pos += 1;
                Some(Obj::Op(vec![c]))
            }
            _ => {
                let raw = self.regular();
                if raw.is_empty() {
                    self.pos += 1;
                    return None;
                }
                Some(match raw {
                    b"true" => Obj::Bool(true),
                    b"false" => Obj::Bool(false),
                    b"null" => Obj::Null,
                    other => Obj::Op(other.to_vec()),
                })
            }
        }
    }

    fn literal(&mut self) -> Vec<u8> {
        self.pos += 1;
        let mut out = Vec::new();
        let mut depth = 1;
        while let Some(c) = self.peek() {
            self.pos += 1;
            match c {
                b'(' => {
                    depth += 1;
                    out.push(c);
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    out.push(c);
                }
                b'\\' => {
                    let Some(e) = self.peek() else { break };
                    self.pos += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'\r' => {
                            if self.peek() == Some(b'\n') {
                                self.pos += 1;
                            }
                        }
                        b'\n' => {}
                        b'0'..=b'7' => {
                            let mut v = (e - b'0') as u32;
                            for _ in 0..2 {
                                match self.peek() {
                                    Some(d @ b'0'..=b'7') => {
                                        v = v * 8 + (d - b'0') as u32;
                                        self.pos += 1;
                                    }
                                    _ => break,
                                }
                            }
                            out.push(v as u8);
                        }
                        other => out.push(other),
                    }
                }
                b'\r' => {
                    // an end of line in a string is a newline, however written
                    if self.peek() == Some(b'\n') {
                        self.pos += 1;
                    }
                    out.push(b'\n');
                }
                _ => out.push(c),
            }
        }
        out
    }
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= hay.len() || needle.is_empty() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w == needle).map(|p| p + from)
}

fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).rposition(|w| w == needle)
}

// ---- the document ----------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Loc {
    At(usize),
    InStream(u32, u32),
}

struct Pdf<'a> {
    bytes: &'a [u8],
    locs: HashMap<u32, Loc>,
    /// the object locations found by scanning, built when the table fails
    scanned: RefCell<Option<HashMap<u32, Loc>>>,
    cache: RefCell<HashMap<u32, Obj>>,
    trailer: Dict,
    crypt: Option<Crypt>,
    resolving: RefCell<usize>,
}

fn encrypted() -> Failure {
    Failure {
        kind: "encrypted_document_exception",
        reason: "Unable to process: document is encrypted".into(),
    }
}

pub fn parse(bytes: &[u8], text: &mut Text) -> Result<Meta, Failure> {
    let mut pdf = Pdf::open(bytes)?;
    if let Some(Obj::Dict(enc)) = get(&pdf.trailer, "Encrypt").map(|o| pdf.resolve(o)) {
        let id0 = match get(&pdf.trailer, "ID").map(|o| pdf.resolve(o)) {
            Some(Obj::Array(ids)) => match ids.first().map(|o| pdf.resolve(o)) {
                Some(Obj::Str(s)) => s,
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        let encrypt_ref = match get(&pdf.trailer, "Encrypt") {
            Some(Obj::Ref(n)) => Some(*n),
            _ => None,
        };
        pdf.crypt = Some(Crypt::open(&pdf, &enc, &id0, encrypt_ref)?);
        // what was read before the key was known was read encrypted
        pdf.cache.borrow_mut().clear();
    }
    let root = match get(&pdf.trailer, "Root").map(|o| pdf.resolve(o)) {
        Some(Obj::Dict(d)) => d,
        _ => return Err(Failure::tika("Unable to extract PDF content: no document catalog")),
    };
    let mut meta = pdf.info();
    if let Some(xmp) = pdf.xmp(&root) {
        meta.fill(xmp);
    }
    let pages = pdf.pages(&root);
    for page in pages {
        if text.full() {
            break;
        }
        text.start("div");
        let mut layout = Layout::new();
        let resources = match page.resources {
            Some(Obj::Dict(d)) => d,
            _ => Dict::new(),
        };
        let mut fonts = FontCache::default();
        let mut seen = HashSet::new();
        let content = pdf.page_content(&page.dict);
        let mut state = Interpreter::new();
        state.run(&pdf, &content, &resources, &mut fonts, &mut layout, 0, &mut seen);
        layout.finish(text);
        pdf.annotations(&page.dict, text);
        text.end("div");
    }
    if !text.full() {
        pdf.outline(&root, text);
    }
    Ok(meta)
}

struct Page {
    dict: Dict,
    resources: Option<Obj>,
}

impl<'a> Pdf<'a> {
    fn open(bytes: &'a [u8]) -> Result<Pdf<'a>, Failure> {
        let mut pdf = Pdf {
            bytes,
            locs: HashMap::new(),
            scanned: RefCell::new(None),
            cache: RefCell::new(HashMap::new()),
            trailer: Dict::new(),
            crypt: None,
            resolving: RefCell::new(0),
        };
        let from_table = pdf.read_xref();
        let sound = from_table
            && matches!(get(&pdf.trailer, "Root").map(|o| pdf.resolve(o)), Some(Obj::Dict(ref d)) if get(d, "Pages").is_some());
        if !sound {
            let (locs, trailer) = scan(bytes);
            pdf.cache.borrow_mut().clear();
            pdf.locs = locs.clone();
            *pdf.scanned.borrow_mut() = Some(locs);
            for (k, v) in trailer {
                if get(&pdf.trailer, std::str::from_utf8(&k).unwrap_or("")).is_none() {
                    pdf.trailer.push((k, v));
                }
            }
            if !matches!(get(&pdf.trailer, "Root").map(|o| pdf.resolve(o)), Some(Obj::Dict(_))) {
                // no trailer says where the catalog is: find it by its type
                let mut ids: Vec<u32> = pdf.locs.keys().copied().collect();
                ids.sort();
                for id in ids {
                    if let Some(Obj::Dict(d)) = pdf.object(id)
                        && get(&d, "Type").and_then(Obj::name) == Some(b"Catalog")
                    {
                        pdf.trailer.retain(|(k, _)| k != b"Root");
                        pdf.trailer.push((b"Root".to_vec(), Obj::Ref(id)));
                        break;
                    }
                }
            }
        }
        if pdf.locs.is_empty() {
            return Err(Failure::tika("Unable to extract PDF content: no objects"));
        }
        Ok(pdf)
    }

    /// Follow the cross-reference chain from `startxref`. Newer sections
    /// come first, so an object already located keeps its newer place.
    fn read_xref(&mut self) -> bool {
        let tail_from = self.bytes.len().saturating_sub(2048);
        let Some(at) = rfind(&self.bytes[tail_from..], b"startxref").map(|p| p + tail_from) else {
            return false;
        };
        let mut lex = Lexer::new(self.bytes, at + 9);
        let Some(Obj::Int(start)) = lex.object(0) else { return false };
        // newest first: a table, then the stream of objects a hybrid file's
        // table leaves out, then the section before
        let mut queue = std::collections::VecDeque::from([start]);
        let mut visited = HashSet::new();
        let mut first = true;
        while let Some(offset) = queue.pop_front() {
            if offset < 0
                || offset as usize >= self.bytes.len()
                || !visited.insert(offset)
                || visited.len() > 256
            {
                continue;
            }
            let Some(trailer) = self.xref_section(offset as usize) else { return false };
            if first {
                self.trailer = trailer.clone();
                first = false;
            } else {
                for (k, v) in &trailer {
                    if !self.trailer.iter().any(|(tk, _)| tk == k) {
                        self.trailer.push((k.clone(), v.clone()));
                    }
                }
            }
            if let Some(Obj::Int(prev)) = get(&trailer, "Prev") {
                queue.push_back(*prev);
            }
            if let Some(Obj::Int(stm)) = get(&trailer, "XRefStm") {
                queue.push_front(*stm);
            }
        }
        self.locs.retain(|_, loc| !matches!(loc, Loc::At(0)));
        !self.locs.is_empty()
    }

    /// One cross-reference section, table or stream: its entries are added
    /// where no newer section placed the object, and its trailer returned.
    fn xref_section(&mut self, offset: usize) -> Option<Dict> {
        let mut lex = Lexer::new(self.bytes, offset);
        lex.skip_ws();
        if self.bytes[lex.pos..].starts_with(b"xref") {
            lex.pos += 4;
            loop {
                let save = lex.pos;
                let (Some(Obj::Int(start)), Some(Obj::Int(count))) = (lex.object(0), lex.object(0))
                else {
                    lex.pos = save;
                    break;
                };
                for i in 0..count.clamp(0, 10_000_000) {
                    let (Some(Obj::Int(off)), Some(Obj::Int(_)), Some(Obj::Op(kind))) =
                        (lex.object(0), lex.object(0), lex.object(0))
                    else {
                        break;
                    };
                    let id = (start + i).clamp(0, u32::MAX as i64) as u32;
                    let loc =
                        if kind == b"n" && off > 0 { Loc::At(off as usize) } else { Loc::At(0) };
                    self.locs.entry(id).or_insert(loc);
                }
            }
            lex.skip_ws();
            if !self.bytes[lex.pos..].starts_with(b"trailer") {
                return None;
            }
            lex.pos += 7;
            return match lex.object(0) {
                Some(Obj::Dict(d)) => Some(d),
                _ => None,
            };
        }
        let (_, Obj::Stream(stream)) = self.parse_indirect(offset)? else { return None };
        let data = self.decode(&stream, false)?;
        let widths: Vec<usize> = match get(&stream.dict, "W") {
            Some(Obj::Array(w)) => {
                w.iter().map(|x| x.num().unwrap_or(0.0).max(0.0) as usize).collect()
            }
            _ => return None,
        };
        if widths.len() < 3 || widths.iter().sum::<usize>() == 0 || widths.iter().any(|w| *w > 8) {
            return None;
        }
        let size = get(&stream.dict, "Size").and_then(Obj::num).unwrap_or(0.0) as i64;
        let index: Vec<i64> = match get(&stream.dict, "Index") {
            Some(Obj::Array(ix)) => ix.iter().map(|x| x.num().unwrap_or(0.0) as i64).collect(),
            _ => vec![0, size],
        };
        let row = widths.iter().sum::<usize>();
        let field = |at: usize, w: usize| -> u64 {
            data.get(at..at + w)
                .map(|b| b.iter().fold(0u64, |acc, x| acc << 8 | *x as u64))
                .unwrap_or(0)
        };
        let mut at = 0usize;
        for pair in index.chunks(2) {
            let (start, count) = (pair[0], pair.get(1).copied().unwrap_or(0));
            for i in 0..count.max(0) {
                if at + row > data.len() {
                    break;
                }
                let kind = if widths[0] == 0 { 1 } else { field(at, widths[0]) };
                let a = field(at + widths[0], widths[1]);
                let b = field(at + widths[0] + widths[1], widths[2]);
                at += row;
                let id = (start + i).clamp(0, u32::MAX as i64) as u32;
                let loc = match kind {
                    1 => Loc::At(a as usize),
                    2 => Loc::InStream(a as u32, b as u32),
                    _ => Loc::At(0),
                };
                self.locs.entry(id).or_insert(loc);
            }
        }
        Some(stream.dict.clone())
    }

    /// The object that starts at `offset`: `n g obj`, then the object, and
    /// the stream's data when a dictionary is followed by one.
    fn parse_indirect(&self, offset: usize) -> Option<(u32, Obj)> {
        let mut lex = Lexer::new(self.bytes, offset);
        let Some(Obj::Int(num)) = lex.object(0) else { return None };
        let Some(Obj::Int(generation)) = lex.object(0) else { return None };
        let Some(Obj::Op(kw)) = lex.object(0) else { return None };
        if kw != b"obj" {
            return None;
        }
        let obj = lex.object(0)?;
        let Obj::Dict(dict) = obj else { return Some((num as u32, obj)) };
        let mut after = Lexer::new(self.bytes, lex.pos);
        after.skip_ws();
        if !self.bytes[after.pos..].starts_with(b"stream") {
            return Some((num as u32, Obj::Dict(dict)));
        }
        let mut start = after.pos + 6;
        if self.bytes.get(start) == Some(&b'\r') {
            start += 1;
        }
        if self.bytes.get(start) == Some(&b'\n') {
            start += 1;
        }
        let declared = match get(&dict, "Length") {
            Some(Obj::Int(n)) => Some(*n),
            Some(r @ Obj::Ref(_)) => {
                let depth = *self.resolving.borrow();
                if depth > 4 {
                    None
                } else {
                    match self.resolve(r) {
                        Obj::Int(n) => Some(n),
                        _ => None,
                    }
                }
            }
            _ => None,
        };
        let end = declared
            .filter(|n| *n >= 0)
            .map(|n| start.saturating_add(n as usize))
            .filter(|end| {
                *end <= self.bytes.len() && {
                    let mut l = Lexer::new(self.bytes, *end);
                    l.skip_ws();
                    self.bytes[l.pos..].starts_with(b"endstream")
                }
            })
            .or_else(|| {
                // a length that does not land on `endstream` is wrong, and
                // the data ends where `endstream` is
                find(self.bytes, b"endstream", start).map(|mut e| {
                    if e > start && self.bytes[e - 1] == b'\n' {
                        e -= 1;
                    }
                    if e > start && self.bytes[e - 1] == b'\r' {
                        e -= 1;
                    }
                    e
                })
            })
            .unwrap_or(self.bytes.len());
        Some((
            num as u32,
            Obj::Stream(Rc::new(Stream {
                dict,
                start,
                end: end.max(start),
                id: (num as u32, generation as u16),
            })),
        ))
    }

    fn object(&self, id: u32) -> Option<Obj> {
        if let Some(o) = self.cache.borrow().get(&id) {
            return Some(o.clone());
        }
        {
            let mut depth = self.resolving.borrow_mut();
            if *depth > MAX_DEPTH {
                return None;
            }
            *depth += 1;
        }
        let found = self.load(id);
        *self.resolving.borrow_mut() -= 1;
        let obj = found?;
        self.cache.borrow_mut().insert(id, obj.clone());
        Some(obj)
    }

    fn load(&self, id: u32) -> Option<Obj> {
        let from = |locs: &HashMap<u32, Loc>| -> Option<Obj> {
            match *locs.get(&id)? {
                Loc::At(offset) => {
                    let (num, obj) = self.parse_indirect(offset)?;
                    if num != id {
                        return None;
                    }
                    Some(self.decrypt_strings(obj, id))
                }
                Loc::InStream(stm, index) => self.in_object_stream(stm, index, id),
            }
        };
        if let Some(o) = from(&self.locs) {
            return Some(o);
        }
        // the table pointed somewhere wrong: look the object up by scanning
        if self.scanned.borrow().is_none() {
            let (locs, _) = scan(self.bytes);
            *self.scanned.borrow_mut() = Some(locs);
        }
        let scanned = self.scanned.borrow();
        from(scanned.as_ref()?)
    }

    fn in_object_stream(&self, stm: u32, index: u32, want: u32) -> Option<Obj> {
        let Obj::Stream(stream) = self.object(stm)? else { return None };
        let data = self.decode(&stream, true)?;
        let n = get(&stream.dict, "N").and_then(Obj::num)? as usize;
        let first = get(&stream.dict, "First").and_then(Obj::num)? as usize;
        let mut lex = Lexer::new(&data, 0);
        let mut entries = Vec::new();
        for _ in 0..n.min(1_000_000) {
            let (Some(Obj::Int(num)), Some(Obj::Int(off))) = (lex.object(0), lex.object(0)) else {
                break;
            };
            entries.push((num as u32, off as usize));
        }
        let pick = entries
            .get(index as usize)
            .filter(|(num, _)| *num == want)
            .or_else(|| entries.iter().find(|(num, _)| *num == want))?;
        let mut lex = Lexer::new(&data, first.checked_add(pick.1)?);
        lex.object(0)
    }

    fn resolve(&self, o: &Obj) -> Obj {
        let mut current = o.clone();
        for _ in 0..32 {
            match current {
                Obj::Ref(n) => match self.object(n) {
                    Some(next) => current = next,
                    None => return Obj::Null,
                },
                other => return other,
            }
        }
        Obj::Null
    }

    fn dict(&self, o: Option<&Obj>) -> Option<Dict> {
        match self.resolve(o?) {
            Obj::Dict(d) => Some(d),
            Obj::Stream(s) => Some(s.dict.clone()),
            _ => None,
        }
    }

    fn decrypt_strings(&self, obj: Obj, id: u32) -> Obj {
        let Some(crypt) = &self.crypt else { return obj };
        if Some(id) == crypt.encrypt_ref {
            return obj;
        }
        fn walk(o: Obj, crypt: &Crypt, id: u32, depth: usize) -> Obj {
            if depth > MAX_DEPTH {
                return o;
            }
            match o {
                Obj::Str(s) => Obj::Str(crypt.decrypt(&s, id, 0, false)),
                Obj::Array(a) => {
                    Obj::Array(a.into_iter().map(|x| walk(x, crypt, id, depth + 1)).collect())
                }
                Obj::Dict(d) => Obj::Dict(
                    d.into_iter().map(|(k, v)| (k, walk(v, crypt, id, depth + 1))).collect(),
                ),
                Obj::Stream(s) => {
                    let dict = s
                        .dict
                        .clone()
                        .into_iter()
                        .map(|(k, v)| (k, walk(v, crypt, id, depth + 1)))
                        .collect();
                    Obj::Stream(Rc::new(Stream { dict, start: s.start, end: s.end, id: s.id }))
                }
                other => other,
            }
        }
        walk(obj, crypt, id, 0)
    }

    /// A stream's data with its filters undone.
    fn decode(&self, stream: &Stream, decrypt: bool) -> Option<Vec<u8>> {
        let raw = self.bytes.get(stream.start..stream.end)?;
        let mut data = raw.to_vec();
        let kind = get(&stream.dict, "Type").and_then(Obj::name);
        if decrypt
            && let Some(crypt) = &self.crypt
            && kind != Some(b"XRef")
            && !(kind == Some(b"Metadata") && !crypt.encrypt_metadata)
        {
            data = crypt.decrypt(&data, stream.id.0, stream.id.1, true);
        }
        let filters: Vec<Vec<u8>> = match get(&stream.dict, "Filter").map(|f| self.resolve(f)) {
            Some(Obj::Name(n)) => vec![n],
            Some(Obj::Array(a)) => {
                a.iter().filter_map(|f| self.resolve(f).name().map(|n| n.to_vec())).collect()
            }
            _ => Vec::new(),
        };
        let parms: Vec<Option<Dict>> = match get(&stream.dict, "DecodeParms")
            .or_else(|| get(&stream.dict, "DP"))
            .map(|p| self.resolve(p))
        {
            Some(Obj::Dict(d)) => vec![Some(d)],
            Some(Obj::Array(a)) => a
                .iter()
                .map(|p| match self.resolve(p) {
                    Obj::Dict(d) => Some(d),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        for (i, f) in filters.iter().enumerate() {
            let parm = parms.get(i).cloned().flatten();
            data = match f.as_slice() {
                b"FlateDecode" | b"Fl" => predict(inflate(&data)?, parm.as_ref())?,
                b"LZWDecode" | b"LZW" => {
                    let early = parm
                        .as_ref()
                        .and_then(|p| get(p, "EarlyChange"))
                        .and_then(Obj::num)
                        .unwrap_or(1.0)
                        != 0.0;
                    predict(lzw(&data, early), parm.as_ref())?
                }
                b"ASCIIHexDecode" | b"AHx" => ascii_hex(&data),
                b"ASCII85Decode" | b"A85" => ascii85(&data),
                b"RunLengthDecode" | b"RL" => run_length(&data),
                b"Crypt" => data,
                // image codecs: there is no text past them
                _ => return Some(data),
            };
            if data.len() > MAX_DECODED {
                return None;
            }
        }
        Some(data)
    }

    fn pages(&self, root: &Dict) -> Vec<Page> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let Some(tree) = get(root, "Pages") else { return out };
        self.page_tree(tree, None, &mut out, &mut seen, 0);
        out
    }

    fn page_tree(
        &self,
        node: &Obj,
        resources: Option<Obj>,
        out: &mut Vec<Page>,
        seen: &mut HashSet<u32>,
        depth: usize,
    ) {
        if depth > MAX_DEPTH || out.len() > 100_000 {
            return;
        }
        if let Obj::Ref(n) = node
            && !seen.insert(*n)
        {
            return;
        }
        let Some(dict) = self.dict(Some(node)) else { return };
        let resources = get(&dict, "Resources").map(|r| self.resolve(r)).or(resources);
        let kind = get(&dict, "Type").and_then(Obj::name).map(|n| n.to_vec());
        match (kind.as_deref(), get(&dict, "Kids").map(|k| self.resolve(k))) {
            (Some(b"Page"), _) | (_, None) => out.push(Page { dict, resources }),
            (_, Some(Obj::Array(kids))) => {
                for kid in kids {
                    self.page_tree(&kid, resources.clone(), out, seen, depth + 1);
                }
            }
            _ => {}
        }
    }

    fn page_content(&self, page: &Dict) -> Vec<u8> {
        let mut out = Vec::new();
        let streams = match get(page, "Contents").map(|c| self.resolve(c)) {
            Some(Obj::Array(a)) => a.iter().map(|s| self.resolve(s)).collect(),
            Some(s @ Obj::Stream(_)) => vec![s],
            _ => Vec::new(),
        };
        for s in streams {
            if let Obj::Stream(stream) = s
                && let Some(data) = self.decode(&stream, true)
            {
                out.extend_from_slice(&data);
                out.push(b'\n');
                if out.len() > MAX_DECODED {
                    break;
                }
            }
        }
        out
    }

    /// The document information dictionary.
    fn info(&self) -> Meta {
        let mut meta = Meta::default();
        let Some(info) = self.dict(get(&self.trailer, "Info")) else { return meta };
        let text_of = |key: &str| -> Option<String> {
            match get(&info, key).map(|o| self.resolve(o)) {
                Some(Obj::Str(s)) => Some(text_string(&s)).filter(|t| !t.trim().is_empty()),
                _ => None,
            }
        };
        meta.title = text_of("Title");
        meta.author = text_of("Author");
        meta.keywords = text_of("Keywords");
        meta.date = text_of("CreationDate").and_then(|d| pdf_date(&d));
        meta
    }

    /// The XMP packet a catalog may carry.
    fn xmp(&self, root: &Dict) -> Option<Meta> {
        let Obj::Stream(stream) = self.resolve(get(root, "Metadata")?) else { return None };
        let data = self.decode(&stream, true)?;
        let xml = super::xml::decode(&data);
        let mut meta = Meta::default();
        let mut path: Vec<String> = Vec::new();
        let mut value = String::new();
        let _ = super::xml::walk(&xml, |ev| {
            match ev {
                super::xml::Ev::Start { name, prefix, attrs } => {
                    path.push(format!("{prefix}:{name}"));
                    value.clear();
                    if name == "Description" {
                        for (k, v) in attrs {
                            if k == "xmp:CreateDate" && meta.date.is_none() {
                                meta.date = Some(v.clone());
                            }
                        }
                    }
                }
                super::xml::Ev::Text(t) => value.push_str(t),
                super::xml::Ev::End { .. } => {
                    let v = value.trim().to_string();
                    let inside = |p: &str| path.iter().any(|e| e == p);
                    if !v.is_empty() {
                        if inside("dc:title") && meta.title.is_none() {
                            meta.title = Some(v);
                        } else if inside("dc:creator") && meta.author.is_none() {
                            meta.author = Some(v);
                        } else if inside("dc:subject") && meta.keywords.is_none() {
                            meta.keywords = Some(v);
                        } else if path.last().is_some_and(|l| l == "xmp:CreateDate")
                            && meta.date.is_none()
                        {
                            meta.date = Some(v);
                        }
                    }
                    value.clear();
                    path.pop();
                }
            }
            true
        });
        meta.date = meta.date.map(|d| match crate::store::parse_date_lenient(&d) {
            Some(at) => {
                crate::store::format_millis(at.unix_timestamp() * 1000, "yyyy-MM-dd'T'HH:mm:ss'Z'")
                    .unwrap_or(d)
            }
            None => d,
        });
        Some(meta)
    }

    /// Link targets and the notes and comments on a page, as Tika writes
    /// them after the page's text.
    fn annotations(&self, page: &Dict, text: &mut Text) {
        let Some(Obj::Array(annots)) = get(page, "Annots").map(|a| self.resolve(a)) else { return };
        for a in annots.iter().take(10_000) {
            let Some(annot) = self.dict(Some(a)) else { continue };
            let subtype = get(&annot, "Subtype").and_then(Obj::name).unwrap_or(b"");
            if let Some(action) = self.dict(get(&annot, "A"))
                && get(&action, "S").and_then(Obj::name) == Some(b"URI")
                && let Some(Obj::Str(uri)) = get(&action, "URI").map(|u| self.resolve(u))
            {
                let link = String::from_utf8_lossy(&uri).into_owned();
                if !link.trim().is_empty() {
                    text.start("div");
                    text.chars(&link);
                    text.end("div");
                }
            }
            // the markup annotations: everything but links, widgets, popups
            // and the like
            if matches!(
                subtype,
                b"Link"
                    | b"Widget"
                    | b"Popup"
                    | b"FileAttachment"
                    | b"Sound"
                    | b"Movie"
                    | b"Screen"
                    | b"PrinterMark"
                    | b"TrapNet"
                    | b"Watermark"
                    | b"3D"
            ) {
                continue;
            }
            let field = |key: &str| match get(&annot, key).map(|o| self.resolve(o)) {
                Some(Obj::Str(s)) => Some(text_string(&s)),
                _ => None,
            };
            let (title, subject, contents) = (field("T"), field("Subj"), field("Contents"));
            if title.is_some() || subject.is_some() || contents.is_some() {
                text.start("div");
                for part in [title, subject, contents].into_iter().flatten() {
                    text.start("div");
                    text.chars(&part);
                    text.end("div");
                }
                text.end("div");
            }
        }
    }

    /// The bookmarks, as nested lists.
    fn outline(&self, root: &Dict, text: &mut Text) {
        let Some(outlines) = self.dict(get(root, "Outlines")) else { return };
        let mut seen = HashSet::new();
        self.outline_level(&outlines, text, &mut seen, 0);
    }

    fn outline_level(&self, node: &Dict, text: &mut Text, seen: &mut HashSet<u32>, depth: usize) {
        let Some(mut current) = get(node, "First").cloned() else { return };
        if depth > MAX_DEPTH {
            return;
        }
        text.start("ul");
        for _ in 0..100_000 {
            if let Obj::Ref(n) = current
                && !seen.insert(n)
            {
                break;
            }
            let Some(item) = self.dict(Some(&current)) else { break };
            text.start("li");
            if let Some(Obj::Str(t)) = get(&item, "Title").map(|t| self.resolve(t)) {
                text.chars(&text_string(&t));
            }
            text.end("li");
            self.outline_level(&item, text, seen, depth + 1);
            match get(&item, "Next") {
                Some(next) => current = next.clone(),
                None => break,
            }
        }
        text.end("ul");
    }
}

/// Find every `n g obj` in a file whose table cannot be trusted. A later
/// definition of an object replaces an earlier one, as an incremental update
/// does; objects inside object streams are found through those streams.
fn scan(bytes: &[u8]) -> (HashMap<u32, Loc>, Dict) {
    let mut locs = HashMap::new();
    let mut at = 0;
    while let Some(p) = find(bytes, b"obj", at) {
        at = p + 3;
        if bytes.get(p + 3).is_some_and(|c| !is_ws(*c) && !is_delim(*c)) {
            continue;
        }
        // walk back over `g`, whitespace, `n`
        let mut i = p;
        let back = |i: &mut usize, digits: bool| -> bool {
            let end = *i;
            while *i > 0
                && (if digits { bytes[*i - 1].is_ascii_digit() } else { is_ws(bytes[*i - 1]) })
            {
                *i -= 1;
            }
            *i < end
        };
        if !back(&mut i, false)
            || !back(&mut i, true)
            || !back(&mut i, false)
            || !back(&mut i, true)
        {
            continue;
        }
        if i > 0 && !is_ws(bytes[i - 1]) && !is_delim(bytes[i - 1]) {
            continue;
        }
        let mut lex = Lexer::new(bytes, i);
        if let Some(Obj::Int(n)) = lex.object(0)
            && n >= 0
        {
            locs.insert(n as u32, Loc::At(i));
        }
    }
    let mut trailer = Dict::new();
    if let Some(t) = rfind(bytes, b"trailer") {
        let mut lex = Lexer::new(bytes, t + 7);
        if let Some(Obj::Dict(d)) = lex.object(0) {
            trailer = d;
        }
    }
    // the objects inside object streams, and the trailer keys a
    // cross-reference stream carries
    let pdf = Pdf {
        bytes,
        locs: locs.clone(),
        scanned: RefCell::new(Some(locs.clone())),
        cache: RefCell::new(HashMap::new()),
        trailer: Dict::new(),
        crypt: None,
        resolving: RefCell::new(0),
    };
    let mut ids: Vec<(u32, usize)> = locs
        .iter()
        .filter_map(|(k, v)| match v {
            Loc::At(o) => Some((*k, *o)),
            _ => None,
        })
        .collect();
    ids.sort_by_key(|(_, o)| *o);
    for (id, offset) in ids {
        // only an object whose dictionary says so is worth parsing whole
        let window = &bytes[offset..bytes.len().min(offset + 512)];
        let stm = find(window, b"/ObjStm", 0).is_some();
        let xref = find(window, b"/XRef", 0).is_some();
        if !stm && !xref {
            continue;
        }
        let Some((_, Obj::Stream(s))) = pdf.parse_indirect(offset) else { continue };
        if xref {
            for key in ["Root", "Info", "Encrypt", "ID"] {
                if let Some(v) = get(&s.dict, key)
                    && get(&trailer, key).is_none()
                {
                    trailer.push((key.as_bytes().to_vec(), v.clone()));
                }
            }
        }
        if stm && let Some(data) = pdf.decode(&s, false) {
            let n = get(&s.dict, "N").and_then(Obj::num).unwrap_or(0.0) as usize;
            let mut lex = Lexer::new(&data, 0);
            for index in 0..n.min(1_000_000) {
                let (Some(Obj::Int(num)), Some(Obj::Int(_))) = (lex.object(0), lex.object(0))
                else {
                    break;
                };
                locs.entry(num as u32).or_insert(Loc::InStream(id, index as u32));
            }
        }
    }
    (locs, trailer)
}

// ---- filters ---------------------------------------------------------------

/// Inflate as much as inflates: a stream cut short still has its text up to
/// the cut, and PDFBox keeps it.
fn inflate(data: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    let zlib = data.len() >= 2
        && (data[0] & 0x0f) == 8
        && (u16::from(data[0]) << 8 | u16::from(data[1])) % 31 == 0;
    let mut reader: Box<dyn Read> = if zlib {
        Box::new(flate2::read::ZlibDecoder::new(data))
    } else {
        Box::new(flate2::read::DeflateDecoder::new(data))
    };
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.len() > MAX_DECODED {
                    return None;
                }
            }
        }
    }
    Some(out)
}

fn predict(data: Vec<u8>, parms: Option<&Dict>) -> Option<Vec<u8>> {
    let Some(p) = parms else { return Some(data) };
    let predictor = get(p, "Predictor").and_then(Obj::num).unwrap_or(1.0) as i64;
    if predictor < 2 {
        return Some(data);
    }
    let colors = get(p, "Colors").and_then(Obj::num).unwrap_or(1.0).clamp(1.0, 64.0) as usize;
    let bpc =
        get(p, "BitsPerComponent").and_then(Obj::num).unwrap_or(8.0).clamp(1.0, 16.0) as usize;
    let columns =
        get(p, "Columns").and_then(Obj::num).unwrap_or(1.0).clamp(1.0, 1_000_000.0) as usize;
    let bpp = (colors * bpc).div_ceil(8).max(1);
    let row = (colors * bpc * columns).div_ceil(8);
    if predictor == 2 {
        let mut out = data;
        if bpc == 8 {
            for r in out.chunks_mut(row.max(1)) {
                for i in bpp..r.len() {
                    r[i] = r[i].wrapping_add(r[i - bpp]);
                }
            }
        }
        return Some(out);
    }
    let mut out = Vec::with_capacity(data.len());
    let mut prev = vec![0u8; row];
    for chunk in data.chunks(row + 1) {
        if chunk.is_empty() {
            break;
        }
        let kind = chunk[0];
        let mut cur: Vec<u8> = chunk[1..].to_vec();
        cur.resize(row, 0);
        for i in 0..row {
            let left = if i >= bpp { cur[i - bpp] } else { 0 };
            let up = prev[i];
            let upleft = if i >= bpp { prev[i - bpp] } else { 0 };
            cur[i] = match kind {
                1 => cur[i].wrapping_add(left),
                2 => cur[i].wrapping_add(up),
                3 => cur[i].wrapping_add(((left as u16 + up as u16) / 2) as u8),
                4 => {
                    let p = left as i16 + up as i16 - upleft as i16;
                    let (pa, pb, pc) =
                        ((p - left as i16).abs(), (p - up as i16).abs(), (p - upleft as i16).abs());
                    let pred = if pa <= pb && pa <= pc {
                        left
                    } else if pb <= pc {
                        up
                    } else {
                        upleft
                    };
                    cur[i].wrapping_add(pred)
                }
                _ => cur[i],
            };
        }
        out.extend_from_slice(&cur);
        prev = cur;
    }
    Some(out)
}

fn lzw(data: &[u8], early: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut table: Vec<Vec<u8>> = Vec::new();
    let reset = |table: &mut Vec<Vec<u8>>| {
        table.clear();
        for i in 0..256 {
            table.push(vec![i as u8]);
        }
        table.push(Vec::new());
        table.push(Vec::new());
    };
    reset(&mut table);
    let mut width = 9;
    let (mut bits, mut nbits) = (0u32, 0u32);
    let mut prev: Option<Vec<u8>> = None;
    for &b in data {
        bits = bits << 8 | b as u32;
        nbits += 8;
        while nbits >= width {
            let code = ((bits >> (nbits - width)) & ((1 << width) - 1)) as usize;
            nbits -= width;
            if code == 256 {
                reset(&mut table);
                width = 9;
                prev = None;
                continue;
            }
            if code == 257 {
                return out;
            }
            let entry = if code < table.len() {
                table[code].clone()
            } else if let Some(p) = &prev {
                let mut e = p.clone();
                e.push(p[0]);
                e
            } else {
                return out;
            };
            out.extend_from_slice(&entry);
            if out.len() > MAX_DECODED {
                return out;
            }
            if let Some(p) = prev.take()
                && table.len() < 4096
            {
                let mut e = p;
                e.push(entry[0]);
                table.push(e);
            }
            prev = Some(entry);
            let limit = table.len() + usize::from(early);
            width = if limit >= 2048 {
                12
            } else if limit >= 1024 {
                11
            } else if limit >= 512 {
                10
            } else {
                9
            };
        }
    }
    out
}

fn ascii_hex(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut high: Option<u8> = None;
    for &c in data {
        if c == b'>' {
            break;
        }
        let v = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => continue,
        };
        match high.take() {
            Some(h) => out.push(h << 4 | v),
            None => high = Some(v),
        }
    }
    if let Some(h) = high {
        out.push(h << 4);
    }
    out
}

fn ascii85(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut group = [0u8; 5];
    let mut n = 0;
    let body = data.strip_prefix(b"<~").unwrap_or(data);
    for &c in body {
        if c == b'~' {
            break;
        }
        if is_ws(c) {
            continue;
        }
        if c == b'z' && n == 0 {
            out.extend_from_slice(&[0, 0, 0, 0]);
            continue;
        }
        if !(b'!'..=b'u').contains(&c) {
            continue;
        }
        group[n] = c - b'!';
        n += 1;
        if n == 5 {
            let v = group.iter().fold(0u64, |acc, d| acc * 85 + *d as u64) as u32;
            out.extend_from_slice(&v.to_be_bytes());
            n = 0;
        }
    }
    if n > 1 {
        for g in group.iter_mut().skip(n) {
            *g = 84;
        }
        let v = group.iter().fold(0u64, |acc, d| acc * 85 + *d as u64) as u32;
        out.extend_from_slice(&v.to_be_bytes()[..n - 1]);
    }
    out
}

fn run_length(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let len = data[i];
        i += 1;
        match len {
            128 => break,
            0..=127 => {
                let n = len as usize + 1;
                out.extend_from_slice(&data[i..(i + n).min(data.len())]);
                i += n;
            }
            _ => {
                if let Some(b) = data.get(i) {
                    out.extend(std::iter::repeat_n(*b, 257 - len as usize));
                }
                i += 1;
            }
        }
    }
    out
}

// ---- encryption ------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Cipher {
    Identity,
    Rc4,
    Aes128,
    Aes256,
}

struct Crypt {
    key: Vec<u8>,
    streams: Cipher,
    strings: Cipher,
    encrypt_metadata: bool,
    encrypt_ref: Option<u32>,
}

const PAD: [u8; 32] = [
    0x28, 0xBF, 0x4E, 0x5E, 0x4E, 0x75, 0x8A, 0x41, 0x64, 0x00, 0x4E, 0x56, 0xFF, 0xFA, 0x01, 0x08,
    0x2E, 0x2E, 0x00, 0xB6, 0xD0, 0x68, 0x3E, 0x80, 0x2F, 0x0C, 0xA9, 0xFE, 0x64, 0x53, 0x69, 0x7A,
];

impl Crypt {
    /// The key for the empty user password, or the refusal PDFBox gives
    /// when that password does not open the file.
    fn open(pdf: &Pdf, enc: &Dict, id0: &[u8], encrypt_ref: Option<u32>) -> Result<Crypt, Failure> {
        let filter = get(enc, "Filter").and_then(Obj::name).unwrap_or(b"Standard");
        if filter != b"Standard" {
            return Err(encrypted());
        }
        let num = |k: &str| get(enc, k).map(|o| pdf.resolve(o)).and_then(|o| o.num());
        let bytes = |k: &str| match get(enc, k).map(|o| pdf.resolve(o)) {
            Some(Obj::Str(s)) => s,
            _ => Vec::new(),
        };
        let v = num("V").unwrap_or(0.0) as i64;
        let r = num("R").unwrap_or(2.0) as i64;
        let (o, u) = (bytes("O"), bytes("U"));
        let p = num("P").unwrap_or(0.0) as i64 as i32;
        let encrypt_metadata = !matches!(get(enc, "EncryptMetadata"), Some(Obj::Bool(false)));
        let method_of = |name: Option<&[u8]>| -> Cipher {
            let Some(name) = name else { return Cipher::Rc4 };
            if name == b"Identity" {
                return Cipher::Identity;
            }
            let cf = pdf
                .dict(get(enc, "CF"))
                .and_then(|cf| pdf.dict(get(&cf, std::str::from_utf8(name).unwrap_or(""))));
            match cf.as_ref().and_then(|c| get(c, "CFM")).and_then(Obj::name) {
                Some(b"AESV2") => Cipher::Aes128,
                Some(b"AESV3") => Cipher::Aes256,
                Some(b"None") => Cipher::Identity,
                _ => Cipher::Rc4,
            }
        };
        let (streams, strings) = if v >= 4 {
            (
                method_of(get(enc, "StmF").and_then(Obj::name)),
                method_of(get(enc, "StrF").and_then(Obj::name)),
            )
        } else {
            (Cipher::Rc4, Cipher::Rc4)
        };
        let key = if r >= 5 {
            if u.len() < 48 {
                return Err(encrypted());
            }
            let ue = bytes("UE");
            let check = if r == 5 { sha256(&[&u[32..40]]) } else { hash_2b(b"", &u[32..40], &[]) };
            if check[..32] != u[..32] || ue.len() < 32 {
                return Err(encrypted());
            }
            let intermediate =
                if r == 5 { sha256(&[&u[40..48]]) } else { hash_2b(b"", &u[40..48], &[]) };
            aes_cbc(&intermediate[..32], &[0u8; 16], &ue[..32], false, false)
        } else {
            let length =
                if v >= 2 { (num("Length").unwrap_or(40.0) as usize / 8).clamp(5, 16) } else { 5 };
            let length = if r == 2 { 5 } else { length };
            let mut input = Vec::new();
            input.extend_from_slice(&PAD);
            input.extend_from_slice(&o[..o.len().min(32)]);
            input.extend_from_slice(&p.to_le_bytes());
            input.extend_from_slice(id0);
            if r >= 4 && !encrypt_metadata {
                input.extend_from_slice(&[0xff; 4]);
            }
            let mut key = md5::compute(&input).0.to_vec();
            if r >= 3 {
                for _ in 0..50 {
                    key = md5::compute(&key[..length]).0.to_vec();
                }
            }
            key.truncate(length);
            let opens = if r == 2 {
                rc4(&key, &PAD) == u.get(..32).unwrap_or(&[])
            } else {
                let mut digest = md5::compute([&PAD[..], id0].concat()).0.to_vec();
                digest = rc4(&key, &digest);
                for i in 1..=19u8 {
                    let k: Vec<u8> = key.iter().map(|b| b ^ i).collect();
                    digest = rc4(&k, &digest);
                }
                u.len() >= 16 && digest[..16] == u[..16]
            };
            if !opens {
                return Err(encrypted());
            }
            key
        };
        Ok(Crypt { key, streams, strings, encrypt_metadata, encrypt_ref })
    }

    fn decrypt(&self, data: &[u8], num: u32, generation: u16, stream: bool) -> Vec<u8> {
        let cipher = if stream { self.streams } else { self.strings };
        match cipher {
            Cipher::Identity => data.to_vec(),
            Cipher::Aes256 => {
                if data.len() < 32 {
                    return Vec::new();
                }
                aes_cbc(&self.key, &data[..16], &data[16..], true, false)
            }
            Cipher::Rc4 | Cipher::Aes128 => {
                let mut input = self.key.clone();
                input.extend_from_slice(&num.to_le_bytes()[..3]);
                input.extend_from_slice(&generation.to_le_bytes());
                if cipher == Cipher::Aes128 {
                    input.extend_from_slice(b"sAlT");
                }
                let digest = md5::compute(&input).0;
                let key = &digest[..(self.key.len() + 5).min(16)];
                if cipher == Cipher::Rc4 {
                    rc4(key, data)
                } else if data.len() < 32 {
                    Vec::new()
                } else {
                    aes_cbc(key, &data[..16], &data[16..], true, false)
                }
            }
        }
    }
}

fn rc4(key: &[u8], data: &[u8]) -> Vec<u8> {
    if key.is_empty() {
        return data.to_vec();
    }
    let mut s: [u8; 256] = std::array::from_fn(|i| i as u8);
    let mut j = 0u8;
    for i in 0..256 {
        j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
        s.swap(i, j as usize);
    }
    let (mut i, mut j) = (0u8, 0u8);
    data.iter()
        .map(|b| {
            i = i.wrapping_add(1);
            j = j.wrapping_add(s[i as usize]);
            s.swap(i as usize, j as usize);
            b ^ s[s[i as usize].wrapping_add(s[j as usize]) as usize]
        })
        .collect()
}

/// AES in CBC mode, with a 128- or 256-bit key. Decryption strips the
/// padding when asked; encryption is only used by the revision 6 key hash,
/// which never pads.
fn aes_cbc(key: &[u8], iv: &[u8], data: &[u8], strip_padding: bool, encrypt: bool) -> Vec<u8> {
    use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit, generic_array::GenericArray};
    let whole = data.len() / 16 * 16;
    let mut out = data[..whole].to_vec();
    let mut prev: [u8; 16] = iv.get(..16).and_then(|v| v.try_into().ok()).unwrap_or([0; 16]);
    macro_rules! run {
        ($cipher:expr) => {{
            let cipher = $cipher;
            for block in out.chunks_mut(16) {
                if encrypt {
                    for (b, p) in block.iter_mut().zip(prev.iter()) {
                        *b ^= p;
                    }
                    cipher.encrypt_block(GenericArray::from_mut_slice(block));
                    prev.copy_from_slice(block);
                } else {
                    let saved: [u8; 16] = block.try_into().unwrap_or([0; 16]);
                    cipher.decrypt_block(GenericArray::from_mut_slice(block));
                    for (b, p) in block.iter_mut().zip(prev.iter()) {
                        *b ^= p;
                    }
                    prev = saved;
                }
            }
        }};
    }
    match key.len() {
        16 => run!(aes::Aes128::new(GenericArray::from_slice(key))),
        32 => run!(aes::Aes256::new(GenericArray::from_slice(key))),
        _ => return Vec::new(),
    }
    if strip_padding
        && let Some(&pad) = out.last()
        && (1..=16).contains(&pad)
        && out.len() >= pad as usize
    {
        out.truncate(out.len() - pad as usize);
    }
    out
}

fn sha256(parts: &[&[u8]]) -> Vec<u8> {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().to_vec()
}

/// The key hash of revision 6: SHA-256, then rounds of AES and SHA-2 of a
/// width the previous round picks.
fn hash_2b(password: &[u8], salt: &[u8], udata: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    let mut k = sha256(&[password, salt, udata]);
    let mut round = 0usize;
    loop {
        let mut k1 = Vec::with_capacity(64 * (password.len() + k.len() + udata.len()));
        for _ in 0..64 {
            k1.extend_from_slice(password);
            k1.extend_from_slice(&k);
            k1.extend_from_slice(udata);
        }
        let e = aes_cbc(&k[..16], &k[16..32], &k1, false, true);
        let modulo = e[..16].iter().map(|b| *b as u32).sum::<u32>() % 3;
        k = match modulo {
            0 => sha2::Sha256::digest(&e).to_vec(),
            1 => sha2::Sha384::digest(&e).to_vec(),
            _ => sha2::Sha512::digest(&e).to_vec(),
        };
        round += 1;
        if round >= 64 && (*e.last().unwrap_or(&0) as usize) <= round - 32 {
            break;
        }
        if round > 1000 {
            break;
        }
    }
    k.truncate(32);
    k
}

// ---- strings and dates -----------------------------------------------------

/// A PDF text string: UTF-16 behind a byte-order mark, UTF-8 behind one, and
/// PDFDocEncoding otherwise.
fn text_string(s: &[u8]) -> String {
    if let Some(body) = s.strip_prefix(&[0xFE, 0xFF]) {
        let units: Vec<u16> =
            body.chunks(2).map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)])).collect();
        return String::from_utf16_lossy(&units);
    }
    if let Some(body) = s.strip_prefix(&[0xFF, 0xFE]) {
        let units: Vec<u16> =
            body.chunks(2).map(|c| u16::from_le_bytes([c[0], *c.get(1).unwrap_or(&0)])).collect();
        return String::from_utf16_lossy(&units);
    }
    if let Some(body) = s.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(body).into_owned();
    }
    s.iter()
        .map(|b| match b {
            0x18 => '\u{02D8}',
            0x19 => '\u{02C7}',
            0x1A => '\u{02C6}',
            0x1B => '\u{02D9}',
            0x1C => '\u{02DD}',
            0x1D => '\u{02DB}',
            0x1E => '\u{02DA}',
            0x1F => '\u{02DC}',
            0x80 => '\u{2022}',
            0x81 => '\u{2020}',
            0x82 => '\u{2021}',
            0x83 => '\u{2026}',
            0x84 => '\u{2014}',
            0x85 => '\u{2013}',
            0x86 => '\u{0192}',
            0x87 => '\u{2044}',
            0x88 => '\u{2039}',
            0x89 => '\u{203A}',
            0x8A => '\u{2212}',
            0x8B => '\u{2030}',
            0x8C => '\u{201E}',
            0x8D => '\u{201C}',
            0x8E => '\u{201D}',
            0x8F => '\u{2018}',
            0x90 => '\u{2019}',
            0x91 => '\u{201A}',
            0x92 => '\u{2122}',
            0x93 => '\u{FB01}',
            0x94 => '\u{FB02}',
            0x95 => '\u{0141}',
            0x96 => '\u{0152}',
            0x97 => '\u{0160}',
            0x98 => '\u{0178}',
            0x99 => '\u{017D}',
            0x9A => '\u{0131}',
            0x9B => '\u{0142}',
            0x9C => '\u{0153}',
            0x9D => '\u{0161}',
            0x9E => '\u{017E}',
            0xA0 => '\u{20AC}',
            other => *other as char,
        })
        .collect()
}

/// `D:YYYYMMDDHHmmSSOHH'mm'` as the moment it names, written in UTC to the
/// second.
fn pdf_date(raw: &str) -> Option<String> {
    let s = raw.trim().trim_start_matches("D:");
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() < 4 {
        return None;
    }
    let field = |from: usize, len: usize, default: i64| -> i64 {
        digits.get(from..from + len).and_then(|d| d.parse().ok()).unwrap_or(default)
    };
    let (year, month, day) = (field(0, 4, 1970), field(4, 2, 1), field(6, 2, 1));
    let (hour, minute, second) = (field(8, 2, 0), field(10, 2, 0), field(12, 2, 0));
    let rest = &s[digits.len()..];
    let offset_minutes = match rest.chars().next() {
        Some(sign @ ('+' | '-')) => {
            let nums: Vec<i64> = rest[1..]
                .split(|c: char| !c.is_ascii_digit())
                .filter(|p| !p.is_empty())
                .filter_map(|p| p.parse().ok())
                .collect();
            let m = nums.first().copied().unwrap_or(0) * 60 + nums.get(1).copied().unwrap_or(0);
            if sign == '-' { -m } else { m }
        }
        _ => 0,
    };
    let date = boostcore::time::Date::from_calendar_date(
        year as i32,
        boostcore::time::Month::try_from(month.clamp(1, 12) as u8).ok()?,
        day.clamp(1, 31) as u8,
    )
    .ok()?;
    let time = boostcore::time::Time::from_hms(
        hour.clamp(0, 23) as u8,
        minute.clamp(0, 59) as u8,
        second.clamp(0, 59) as u8,
    )
    .ok()?;
    let at = boostcore::time::PrimitiveDateTime::new(date, time).assume_utc();
    let millis = (at.unix_timestamp() - offset_minutes * 60) * 1000;
    crate::store::format_millis(millis, "yyyy-MM-dd'T'HH:mm:ss'Z'")
}

// ---- fonts -----------------------------------------------------------------

#[derive(Default)]
struct FontCache {
    fonts: HashMap<Vec<u8>, Rc<Font>>,
}

struct Font {
    composite: bool,
    /// the byte lengths codes are read in, from the codespace ranges
    codespace: Codespace,
    to_unicode: HashMap<u32, String>,
    /// for a simple font, what each code is by its encoding
    encoding: Option<[Option<char>; 256]>,
    /// the codes of a composite font are UTF-16 code units
    unicode_codes: bool,
    first_char: u32,
    widths: Vec<f64>,
    cid_widths: Vec<(u32, u32, f64)>,
    default_width: f64,
    missing_width: f64,
    /// ASCII widths of a standard font used without its own
    standard: Option<&'static [u16; 95]>,
    space_width: f64,
    /// the height of a glyph, as a fraction of the font size
    height: f64,
    /// a Type 3 font's glyph space is its own
    scale: f64,
}

impl Font {
    fn load(pdf: &Pdf, dict: &Dict) -> Font {
        let subtype = get(dict, "Subtype").and_then(Obj::name).unwrap_or(b"").to_vec();
        let composite = subtype == b"Type0";
        let base = get(dict, "BaseFont")
            .and_then(Obj::name)
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .unwrap_or_default();
        let mut font = Font {
            composite,
            codespace: Vec::new(),
            to_unicode: HashMap::new(),
            encoding: None,
            unicode_codes: false,
            first_char: 0,
            widths: Vec::new(),
            cid_widths: Vec::new(),
            default_width: 1000.0,
            missing_width: 0.0,
            standard: None,
            space_width: 0.0,
            height: 0.578,
            scale: 0.001,
        };
        if let Some(Obj::Stream(s)) = get(dict, "ToUnicode").map(|t| pdf.resolve(t))
            && let Some(data) = pdf.decode(&s, true)
        {
            let (map, space) = parse_cmap(&data);
            font.to_unicode = map;
            if !composite {
                font.codespace = space;
            }
        }
        let descendant = if composite {
            match get(dict, "DescendantFonts").map(|d| pdf.resolve(d)) {
                Some(Obj::Array(a)) => a.first().and_then(|d| pdf.dict(Some(d))),
                _ => None,
            }
        } else {
            None
        };
        let descriptor = pdf.dict(get(descendant.as_ref().unwrap_or(dict), "FontDescriptor"));
        if let Some(fd) = &descriptor {
            let n = |k: &str| get(fd, k).map(|o| pdf.resolve(o)).and_then(|o| o.num());
            font.missing_width = n("MissingWidth").unwrap_or(0.0);
            let bbox_height = match get(fd, "FontBBox").map(|o| pdf.resolve(o)) {
                Some(Obj::Array(b)) if b.len() == 4 => {
                    (b[3].num().unwrap_or(0.0) - b[1].num().unwrap_or(0.0)).abs() / 2.0
                }
                _ => 0.0,
            };
            let mut height = bbox_height;
            let cap = n("CapHeight").unwrap_or(0.0);
            if cap != 0.0 && (cap < height || height == 0.0) {
                height = cap;
            }
            let (ascent, descent) = (n("Ascent").unwrap_or(0.0), n("Descent").unwrap_or(0.0));
            if cap > ascent
                && ascent > 0.0
                && descent < 0.0
                && ((ascent - descent) / 2.0 < height || height == 0.0)
            {
                height = (ascent - descent) / 2.0;
            }
            if height > 0.0 {
                font.height = height / 1000.0;
            }
        }
        if composite {
            let encoding = get(dict, "Encoding").map(|e| pdf.resolve(e));
            match encoding {
                Some(Obj::Name(n)) => {
                    let name = String::from_utf8_lossy(&n).into_owned();
                    font.unicode_codes = name.contains("UCS2") || name.contains("UTF16");
                    font.codespace = vec![(vec![0, 0], vec![0xff, 0xff])];
                }
                Some(Obj::Stream(s)) => {
                    if let Some(data) = pdf.decode(&s, true) {
                        let (_, space) = parse_cmap(&data);
                        font.codespace = space;
                    }
                }
                _ => font.codespace = vec![(vec![0, 0], vec![0xff, 0xff])],
            }
            if font.codespace.is_empty() {
                font.codespace = vec![(vec![0, 0], vec![0xff, 0xff])];
            }
            if let Some(d) = &descendant {
                font.default_width =
                    get(d, "DW").and_then(|o| pdf.resolve(o).num()).unwrap_or(1000.0);
                if let Some(Obj::Array(w)) = get(d, "W").map(|o| pdf.resolve(o)) {
                    let mut i = 0;
                    while i + 1 < w.len() && font.cid_widths.len() < 200_000 {
                        let first = pdf.resolve(&w[i]).num().unwrap_or(0.0) as u32;
                        match pdf.resolve(&w[i + 1]) {
                            Obj::Array(list) => {
                                for (k, v) in list.iter().enumerate() {
                                    let c = first + k as u32;
                                    font.cid_widths.push((
                                        c,
                                        c,
                                        pdf.resolve(v).num().unwrap_or(0.0),
                                    ));
                                }
                                i += 2;
                            }
                            other => {
                                let last = other.num().unwrap_or(0.0) as u32;
                                let width = w
                                    .get(i + 2)
                                    .map(|v| pdf.resolve(v).num().unwrap_or(0.0))
                                    .unwrap_or(0.0);
                                font.cid_widths.push((first, last, width));
                                i += 3;
                            }
                        }
                    }
                }
            }
        } else {
            if subtype == b"Type3" {
                if let Some(Obj::Array(m)) = get(dict, "FontMatrix").map(|o| pdf.resolve(o)) {
                    font.scale = m.first().and_then(|v| pdf.resolve(v).num()).unwrap_or(0.001);
                }
                font.height = 0.5;
            }
            font.first_char =
                get(dict, "FirstChar").and_then(|o| pdf.resolve(o).num()).unwrap_or(0.0) as u32;
            if let Some(Obj::Array(w)) = get(dict, "Widths").map(|o| pdf.resolve(o)) {
                font.widths =
                    w.iter().take(65536).map(|v| pdf.resolve(v).num().unwrap_or(0.0)).collect();
            }
            let family = base.split('+').next_back().unwrap_or("").to_string();
            let standard_font =
                ["Helvetica", "Arial", "Times", "Courier", "Symbol", "ZapfDingbats"]
                    .iter()
                    .any(|f| family.starts_with(f));
            if font.widths.is_empty() {
                if family.starts_with("Times") {
                    font.standard = Some(&tables::TIMES);
                    font.height = 0.558;
                } else if family.starts_with("Courier") {
                    font.missing_width = 600.0;
                    font.height = 0.5275;
                } else {
                    font.standard = Some(&tables::HELVETICA);
                }
            }
            // the encoding: a name, or a dictionary of a base and the
            // differences from it
            let truetype = subtype == b"TrueType";
            let symbolic = descriptor
                .as_ref()
                .and_then(|fd| get(fd, "Flags"))
                .and_then(|f| pdf.resolve(f).num())
                .map(|f| (f as i64) & 4 != 0)
                .unwrap_or(false);
            let default_table = if truetype { &tables::WIN_ANSI } else { &tables::STANDARD };
            let table_of = |name: &[u8]| -> Option<&'static [u16; 256]> {
                match name {
                    b"WinAnsiEncoding" => Some(&tables::WIN_ANSI),
                    b"MacRomanEncoding" => Some(&tables::MAC_ROMAN),
                    b"StandardEncoding" => Some(&tables::STANDARD),
                    _ => None,
                }
            };
            let mut table: [Option<char>; 256] = [None; 256];
            let fill = |table: &mut [Option<char>; 256], from: &[u16; 256]| {
                for (i, cp) in from.iter().enumerate() {
                    table[i] = if *cp == 0 { None } else { char::from_u32(*cp as u32) };
                }
            };
            match get(dict, "Encoding").map(|e| pdf.resolve(e)) {
                Some(Obj::Name(n)) => fill(&mut table, table_of(&n).unwrap_or(default_table)),
                Some(Obj::Dict(e)) => {
                    let base_table = get(&e, "BaseEncoding").and_then(Obj::name).and_then(table_of);
                    fill(
                        &mut table,
                        base_table.unwrap_or(if symbolic && !standard_font {
                            &tables::WIN_ANSI
                        } else {
                            default_table
                        }),
                    );
                    if let Some(Obj::Array(diffs)) = get(&e, "Differences").map(|d| pdf.resolve(d))
                    {
                        let mut code = 0usize;
                        for d in diffs {
                            match pdf.resolve(&d) {
                                Obj::Int(n) => code = n.clamp(0, 255) as usize,
                                Obj::Name(glyph) => {
                                    if code < 256 {
                                        table[code] = glyph_char(&glyph);
                                    }
                                    code += 1;
                                }
                                _ => {}
                            }
                        }
                    }
                }
                _ => {
                    if let Some(builtin) =
                        descriptor.as_ref().and_then(|fd| type1_encoding(pdf, fd))
                    {
                        table = builtin;
                    } else {
                        fill(
                            &mut table,
                            if symbolic && !standard_font {
                                &tables::WIN_ANSI
                            } else {
                                default_table
                            },
                        );
                    }
                }
            }
            font.encoding = Some(table);
        }
        // the width of a space, or four fifths of the average width where
        // the font has no space
        let space = font.width_of(32);
        font.space_width = if space > 0.0 {
            space
        } else {
            let known: Vec<f64> = font.widths.iter().copied().filter(|w| *w > 0.0).collect();
            if known.is_empty() {
                0.0
            } else {
                known.iter().sum::<f64>() / known.len() as f64 * 0.8
            }
        };
        if font.space_width == 0.0 {
            font.space_width = 1000.0 * 0.25;
        }
        font
    }

    fn width_of(&self, code: u32) -> f64 {
        if self.composite {
            for (a, b, w) in &self.cid_widths {
                if (*a..=*b).contains(&code) {
                    return *w;
                }
            }
            return self.default_width;
        }
        if code >= self.first_char
            && let Some(w) = self.widths.get((code - self.first_char) as usize)
        {
            return *w;
        }
        if let Some(std) = self.standard {
            if (32..127).contains(&code) {
                return std[(code - 32) as usize] as f64;
            }
            return 500.0;
        }
        self.missing_width
    }

    /// Split a string shown with this font into codes.
    fn codes(&self, s: &[u8]) -> Vec<(u32, usize)> {
        if !self.composite && self.codespace.iter().all(|(lo, _)| lo.len() == 1) {
            return s.iter().map(|b| (*b as u32, 1)).collect();
        }
        let mut out = Vec::new();
        let mut i = 0;
        while i < s.len() {
            let mut taken = None;
            for len in 1..=4 {
                let Some(code) = s.get(i..i + len) else { break };
                if self.codespace.iter().any(|(lo, hi)| {
                    lo.len() == len
                        && code
                            .iter()
                            .zip(lo.iter().zip(hi.iter()))
                            .all(|(c, (l, h))| c >= l && c <= h)
                }) {
                    taken = Some(len);
                    break;
                }
            }
            let len = taken.unwrap_or(if self.composite { 2.min(s.len() - i) } else { 1 });
            let code = s[i..i + len].iter().fold(0u32, |acc, b| acc << 8 | *b as u32);
            out.push((code, len));
            i += len;
        }
        out
    }

    fn unicode(&self, code: u32) -> Option<String> {
        if let Some(u) = self.to_unicode.get(&code) {
            return Some(u.clone());
        }
        if let Some(table) = &self.encoding {
            return table
                .get(code as usize)
                .copied()
                .flatten()
                .map(String::from)
                .or_else(|| char::from_u32(code).map(String::from));
        }
        if self.unicode_codes {
            return char::from_u32(code).map(String::from);
        }
        None
    }
}

/// A glyph name as the character it names.
fn glyph_char(name: &[u8]) -> Option<char> {
    let name = std::str::from_utf8(name).ok()?;
    let name = name.split('.').next().unwrap_or(name);
    if let Ok(i) = tables::GLYPHS.binary_search_by(|(n, _)| (*n).cmp(name)) {
        return Some(tables::GLYPHS[i].1);
    }
    if let Some(hex) = name.strip_prefix("uni").filter(|h| h.len() >= 4) {
        return u32::from_str_radix(&hex[..4], 16).ok().and_then(char::from_u32);
    }
    if let Some(hex) = name.strip_prefix('u').filter(|h| (4..=6).contains(&h.len())) {
        return u32::from_str_radix(hex, 16).ok().and_then(char::from_u32);
    }
    None
}

/// The encoding an embedded Type 1 font program declares in the clear part
/// of its program: `dup 65 /A put`.
fn type1_encoding(pdf: &Pdf, fd: &Dict) -> Option<[Option<char>; 256]> {
    let Obj::Stream(s) = pdf.resolve(get(fd, "FontFile")?) else { return None };
    let data = pdf.decode(&s, true)?;
    let clear_len = get(&s.dict, "Length1")
        .and_then(|l| pdf.resolve(l).num())
        .map(|l| l as usize)
        .unwrap_or(data.len())
        .min(data.len());
    let clear = String::from_utf8_lossy(&data[..clear_len]);
    if clear.contains("/Encoding StandardEncoding") {
        let mut table = [None; 256];
        for (i, cp) in tables::STANDARD.iter().enumerate() {
            table[i] = if *cp == 0 { None } else { char::from_u32(*cp as u32) };
        }
        return Some(table);
    }
    let mut table = [None; 256];
    let mut any = false;
    let words: Vec<&str> = clear.split_whitespace().collect();
    for w in words.windows(4) {
        if w[0] == "dup"
            && w[3] == "put"
            && let Ok(code) = w[1].parse::<usize>()
            && code < 256
            && let Some(name) = w[2].strip_prefix('/')
        {
            table[code] = glyph_char(name.as_bytes());
            any = true;
        }
    }
    any.then_some(table)
}

/// A CMap: what codes map to, and the codespace ranges that say how long a
/// code is.
/// The low and high ends of each range of codes a CMap says are one length.
type Codespace = Vec<(Vec<u8>, Vec<u8>)>;

fn parse_cmap(data: &[u8]) -> (HashMap<u32, String>, Codespace) {
    let mut map = HashMap::new();
    let mut space = Vec::new();
    let mut lex = Lexer::new(data, 0);
    let mut operands: Vec<Obj> = Vec::new();
    let unicode_of = |o: &Obj| -> Option<String> {
        match o {
            Obj::Str(s) => {
                let units: Vec<u16> =
                    s.chunks(2)
                        .map(|c| {
                            if c.len() == 2 {
                                u16::from_be_bytes([c[0], c[1]])
                            } else {
                                c[0] as u16
                            }
                        })
                        .collect();
                Some(String::from_utf16_lossy(&units))
            }
            Obj::Name(n) => glyph_char(n).map(String::from),
            _ => None,
        }
    };
    let code_of = |s: &[u8]| s.iter().fold(0u32, |acc, b| acc << 8 | *b as u32);
    let mut guard = 0;
    while lex.pos < data.len() && guard < 5_000_000 {
        guard += 1;
        let before = lex.pos;
        let Some(o) = lex.object(0) else {
            if lex.pos == before {
                lex.pos += 1;
            }
            continue;
        };
        match o {
            Obj::Op(op) => {
                match op.as_slice() {
                    b"endcodespacerange" => {
                        for pair in operands.chunks(2) {
                            if let (Some(Obj::Str(lo)), Some(Obj::Str(hi))) =
                                (pair.first(), pair.get(1))
                                && !lo.is_empty()
                                && lo.len() == hi.len()
                                && lo.len() <= 4
                            {
                                space.push((lo.clone(), hi.clone()));
                            }
                        }
                    }
                    b"endbfchar" => {
                        for pair in operands.chunks(2) {
                            if let (Some(Obj::Str(src)), Some(dst)) = (pair.first(), pair.get(1))
                                && src.len() <= 4
                                && let Some(u) = unicode_of(dst)
                            {
                                map.insert(code_of(src), u);
                            }
                        }
                    }
                    b"endbfrange" => {
                        for triple in operands.chunks(3) {
                            let (Some(Obj::Str(lo)), Some(Obj::Str(hi)), Some(dst)) =
                                (triple.first(), triple.get(1), triple.get(2))
                            else {
                                continue;
                            };
                            if lo.len() > 4 || hi.len() > 4 {
                                continue;
                            }
                            let (lo, hi) = (code_of(lo), code_of(hi));
                            if hi < lo || hi - lo > 65535 {
                                continue;
                            }
                            match dst {
                                Obj::Array(list) => {
                                    for (k, d) in list.iter().enumerate() {
                                        if let Some(u) = unicode_of(d) {
                                            map.insert(lo + k as u32, u);
                                        }
                                    }
                                }
                                Obj::Str(start) if !start.is_empty() => {
                                    for k in 0..=(hi - lo) {
                                        let mut bytes = start.clone();
                                        // the last byte counts up through the range
                                        let last = bytes.len() - 1;
                                        let v = bytes[last] as u32 + k;
                                        bytes[last] = v as u8;
                                        if v > 255 && last > 0 {
                                            bytes[last - 1] =
                                                bytes[last - 1].wrapping_add((v >> 8) as u8);
                                        }
                                        if let Some(u) = unicode_of(&Obj::Str(bytes)) {
                                            map.insert(lo + k, u);
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                operands.clear();
            }
            other => {
                if operands.len() < 1_000_000 {
                    operands.push(other);
                }
            }
        }
    }
    (map, space)
}

// ---- the content stream ----------------------------------------------------

type Matrix = [f64; 6];

fn multiply(a: &Matrix, b: &Matrix) -> Matrix {
    [
        a[0] * b[0] + a[1] * b[2],
        a[0] * b[1] + a[1] * b[3],
        a[2] * b[0] + a[3] * b[2],
        a[2] * b[1] + a[3] * b[3],
        a[4] * b[0] + a[5] * b[2] + b[4],
        a[4] * b[1] + a[5] * b[3] + b[5],
    ]
}

const IDENTITY: Matrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

#[derive(Clone)]
struct GraphicsState {
    ctm: Matrix,
    font: Option<Rc<Font>>,
    font_key: Vec<u8>,
    size: f64,
    char_spacing: f64,
    word_spacing: f64,
    scale: f64,
    leading: f64,
    rise: f64,
}

struct Interpreter {
    gs: GraphicsState,
    stack: Vec<GraphicsState>,
    tm: Matrix,
    tlm: Matrix,
}

impl Interpreter {
    fn new() -> Interpreter {
        Interpreter {
            gs: GraphicsState {
                ctm: IDENTITY,
                font: None,
                font_key: Vec::new(),
                size: 0.0,
                char_spacing: 0.0,
                word_spacing: 0.0,
                scale: 1.0,
                leading: 0.0,
                rise: 0.0,
            },
            stack: Vec::new(),
            tm: IDENTITY,
            tlm: IDENTITY,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run(
        &mut self,
        pdf: &Pdf,
        content: &[u8],
        resources: &Dict,
        fonts: &mut FontCache,
        layout: &mut Layout,
        depth: usize,
        seen: &mut HashSet<u32>,
    ) {
        if depth > 12 {
            return;
        }
        let mut lex = Lexer::new(content, 0);
        let mut operands: Vec<Obj> = Vec::new();
        let mut ops = 0usize;
        while lex.pos < content.len() {
            let before = lex.pos;
            let Some(o) = lex.object(0) else {
                if lex.pos == before {
                    lex.pos += 1;
                }
                continue;
            };
            let Obj::Op(op) = o else {
                if operands.len() < 10_000 {
                    operands.push(o);
                }
                continue;
            };
            ops += 1;
            if ops > 5_000_000 {
                break;
            }
            let n = |i: usize| operands.get(i).and_then(Obj::num).unwrap_or(0.0);
            match op.as_slice() {
                b"q" => {
                    if self.stack.len() < 256 {
                        self.stack.push(self.gs.clone());
                    }
                }
                b"Q" => {
                    if let Some(g) = self.stack.pop() {
                        self.gs = g;
                    }
                }
                b"cm" if operands.len() >= 6 => {
                    let m = [n(0), n(1), n(2), n(3), n(4), n(5)];
                    self.gs.ctm = multiply(&m, &self.gs.ctm);
                }
                b"BT" => {
                    self.tm = IDENTITY;
                    self.tlm = IDENTITY;
                }
                b"Tf" if operands.len() >= 2 => {
                    self.gs.size = n(1);
                    if let Some(Obj::Name(key)) = operands.first() {
                        self.gs.font_key = key.clone();
                        self.gs.font = fonts.fonts.get(key).cloned().or_else(|| {
                            let fdict = pdf.dict(get(resources, "Font"))?;
                            let dict = pdf.dict(get(&fdict, std::str::from_utf8(key).ok()?))?;
                            let font = Rc::new(Font::load(pdf, &dict));
                            fonts.fonts.insert(key.clone(), font.clone());
                            Some(font)
                        });
                    }
                }
                b"Tc" => self.gs.char_spacing = n(0),
                b"Tw" => self.gs.word_spacing = n(0),
                b"Tz" => self.gs.scale = n(0) / 100.0,
                b"TL" => self.gs.leading = n(0),
                b"Ts" => self.gs.rise = n(0),
                b"Td" => {
                    self.tlm = multiply(&[1.0, 0.0, 0.0, 1.0, n(0), n(1)], &self.tlm);
                    self.tm = self.tlm;
                }
                b"TD" => {
                    self.gs.leading = -n(1);
                    self.tlm = multiply(&[1.0, 0.0, 0.0, 1.0, n(0), n(1)], &self.tlm);
                    self.tm = self.tlm;
                }
                b"Tm" if operands.len() >= 6 => {
                    self.tlm = [n(0), n(1), n(2), n(3), n(4), n(5)];
                    self.tm = self.tlm;
                }
                b"T*" => self.next_line(),
                b"Tj" => {
                    if let Some(Obj::Str(s)) = operands.first() {
                        self.show(s, layout);
                    }
                }
                b"'" => {
                    self.next_line();
                    if let Some(Obj::Str(s)) = operands.first() {
                        self.show(s, layout);
                    }
                }
                b"\"" => {
                    self.gs.word_spacing = n(0);
                    self.gs.char_spacing = n(1);
                    self.next_line();
                    if let Some(Obj::Str(s)) = operands.get(2) {
                        self.show(s, layout);
                    }
                }
                b"TJ" => {
                    if let Some(Obj::Array(items)) = operands.first() {
                        for item in items {
                            match item {
                                Obj::Str(s) => self.show(s, layout),
                                other => {
                                    if let Some(adjust) = other.num() {
                                        let tx = -adjust / 1000.0 * self.gs.size * self.gs.scale;
                                        self.tm =
                                            multiply(&[1.0, 0.0, 0.0, 1.0, tx, 0.0], &self.tm);
                                    }
                                }
                            }
                        }
                    }
                }
                b"BI" => {
                    // an inline image: its data runs to `EI`
                    if let Some(at) = find(content, b"ID", lex.pos) {
                        let mut p = at + 2;
                        loop {
                            let Some(e) = find(content, b"EI", p) else {
                                lex.pos = content.len();
                                break;
                            };
                            let before_ok = e > 0 && is_ws(content[e - 1]);
                            let after_ok = content.get(e + 2).is_none_or(|c| is_ws(*c));
                            if before_ok && after_ok {
                                lex.pos = e + 2;
                                break;
                            }
                            p = e + 2;
                        }
                    }
                }
                b"Do" => {
                    if let Some(Obj::Name(key)) = operands.first()
                        && let Some(xobjects) = pdf.dict(get(resources, "XObject"))
                        && let Some(r @ Obj::Ref(id)) =
                            get(&xobjects, std::str::from_utf8(key).unwrap_or("")).cloned()
                        && !seen.contains(&id)
                        && let Obj::Stream(form) = pdf.resolve(&r)
                        && get(&form.dict, "Subtype").and_then(Obj::name) == Some(b"Form")
                        && let Some(data) = pdf.decode(&form, true)
                    {
                        seen.insert(id);
                        let saved = (self.gs.clone(), self.tm, self.tlm, self.stack.len());
                        if let Some(Obj::Array(m)) =
                            get(&form.dict, "Matrix").map(|o| pdf.resolve(o))
                            && m.len() == 6
                        {
                            let mm = [
                                m[0].num().unwrap_or(1.0),
                                m[1].num().unwrap_or(0.0),
                                m[2].num().unwrap_or(0.0),
                                m[3].num().unwrap_or(1.0),
                                m[4].num().unwrap_or(0.0),
                                m[5].num().unwrap_or(0.0),
                            ];
                            self.gs.ctm = multiply(&mm, &self.gs.ctm);
                        }
                        let inner = pdf
                            .dict(get(&form.dict, "Resources"))
                            .unwrap_or_else(|| resources.clone());
                        let mut inner_fonts = FontCache::default();
                        self.run(pdf, &data, &inner, &mut inner_fonts, layout, depth + 1, seen);
                        seen.remove(&id);
                        self.gs = saved.0;
                        self.tm = saved.1;
                        self.tlm = saved.2;
                        self.stack.truncate(saved.3);
                    }
                }
                _ => {}
            }
            operands.clear();
        }
    }

    fn next_line(&mut self) {
        self.tlm = multiply(&[1.0, 0.0, 0.0, 1.0, 0.0, -self.gs.leading], &self.tlm);
        self.tm = self.tlm;
    }

    fn show(&mut self, s: &[u8], layout: &mut Layout) {
        let Some(font) = self.gs.font.clone() else { return };
        let size = self.gs.size;
        let th = self.gs.scale;
        for (code, len) in font.codes(s) {
            let w0 = font.width_of(code) * font.scale;
            let params = [size * th, 0.0, 0.0, size, 0.0, self.gs.rise];
            let trm = multiply(&multiply(&params, &self.tm), &self.gs.ctm);
            let spacing = self.gs.char_spacing
                + if len == 1 && code == 32 { self.gs.word_spacing } else { 0.0 };
            let tx = (w0 * size + spacing) * th;
            let next = multiply(&multiply(&[1.0, 0.0, 0.0, 1.0, tx, 0.0], &self.tm), &self.gs.ctm);
            if let Some(unicode) = font.unicode(code).filter(|_| layout.glyphs < 2_000_000) {
                // positions are read in the direction the text runs, so text
                // set on its side still reads left to right
                let angle = trm[1].atan2(trm[0]);
                let turn = ((angle / std::f64::consts::FRAC_PI_2).round() as i64).rem_euclid(4);
                let (x, y) = rotate_point(trm[4], trm[5], turn);
                let (nx, _) = rotate_point(next[4], next[5], turn);
                let scale_x = (trm[0] * trm[0] + trm[1] * trm[1]).sqrt();
                let scale_y = (trm[2] * trm[2] + trm[3] * trm[3]).sqrt();
                let width = (nx - x).abs();
                layout.add(Glyph {
                    text: unicode,
                    x,
                    y: -y,
                    width,
                    height: font.height * scale_y,
                    space: font.space_width * font.scale * scale_x,
                    font: Rc::as_ptr(&font) as usize,
                    size: (size * 1000.0) as i64,
                });
            }
            self.tm = multiply(&[1.0, 0.0, 0.0, 1.0, tx, 0.0], &self.tm);
        }
    }
}

/// A point turned back by `quarters` quarter turns.
fn rotate_point(x: f64, y: f64, quarters: i64) -> (f64, f64) {
    match quarters {
        1 => (y, -x),
        2 => (-x, -y),
        3 => (-y, x),
        _ => (x, y),
    }
}

// ---- layout ----------------------------------------------------------------

struct Glyph {
    text: String,
    x: f64,
    /// down the page
    y: f64,
    width: f64,
    height: f64,
    space: f64,
    font: usize,
    size: i64,
}

enum Piece {
    Text(String),
    Space,
}

/// PDFBox's text stripper, reduced to what it does for text written in
/// content order: lines by vertical overlap, words by gaps, paragraphs by
/// drops and indents.
struct Layout {
    line: Vec<Piece>,
    output: Vec<Out>,
    last: Option<Glyph>,
    last_line_start: Option<(f64, bool, bool)>,
    max_y: f64,
    max_height: f64,
    end_of_last: Option<f64>,
    last_word_spacing: f64,
    previous_average: f64,
    started: bool,
    glyphs: usize,
}

enum Out {
    Line(String),
    LineSeparator,
    ParagraphStart,
    ParagraphEnd,
}

impl Layout {
    fn new() -> Layout {
        Layout {
            line: Vec::new(),
            output: Vec::new(),
            last: None,
            last_line_start: None,
            max_y: f64::MIN,
            max_height: -1.0,
            end_of_last: None,
            last_word_spacing: -1.0,
            previous_average: -1.0,
            started: false,
            glyphs: 0,
        }
    }

    fn add(&mut self, g: Glyph) {
        self.glyphs += 1;
        if let Some(last) = &self.last
            && (last.font != g.font || last.size != g.size)
        {
            self.previous_average = -1.0;
        }
        let word_spacing = g.space;
        let delta_space = if word_spacing == 0.0 || word_spacing.is_nan() {
            f64::MAX
        } else if self.last_word_spacing < 0.0 {
            word_spacing * 0.5
        } else {
            (word_spacing + self.last_word_spacing) / 2.0 * 0.5
        };
        let chars = g.text.chars().count().max(1) as f64;
        let average = if self.previous_average < 0.0 {
            g.width / chars
        } else {
            (self.previous_average + g.width / chars) / 2.0
        };
        let delta_char = average * 0.3;
        let mut expected = self
            .end_of_last
            .map(|end| if delta_char > delta_space { end + delta_space } else { end + delta_char });
        if let Some(last_ends_in_space) = self.last.as_ref().map(|l| l.text.ends_with(' ')) {
            if !overlap(g.y, g.height, self.max_y, self.max_height) {
                let line = std::mem::take(&mut self.line);
                self.write_line(line);
                self.line_separation(&g, self.max_height);
                expected = None;
                self.max_y = f64::MIN;
                self.max_height = -1.0;
            }
            if let Some(e) = expected
                && e < g.x
                && !last_ends_in_space
            {
                self.line.push(Piece::Space);
            }
        }
        if g.y >= self.max_y {
            self.max_y = g.y;
        }
        self.end_of_last = Some(g.x + g.width);
        if !self.started && self.last.is_none() {
            self.output.push(Out::ParagraphStart);
            self.last_line_start = Some((g.x, true, false));
            self.started = true;
        }
        self.max_height = self.max_height.max(g.height);
        self.last_word_spacing = word_spacing;
        self.previous_average = average;
        self.line.push(Piece::Text(g.text.clone()));
        self.last = Some(g);
    }

    fn line_separation(&mut self, g: &Glyph, max_height: f64) {
        let Some(last) = &self.last else { return };
        let mut paragraph = false;
        let mut hanging = false;
        match self.last_line_start {
            None => paragraph = true,
            Some((start_x, start_is_paragraph, start_hanging)) => {
                let y_gap = (g.y - last.y).abs();
                let x_gap = g.x - start_x;
                if y_gap > 2.5 * max_height {
                    paragraph = true;
                } else if x_gap > 2.0 * g.space {
                    if !start_is_paragraph {
                        paragraph = true;
                    } else {
                        hanging = true;
                    }
                } else if x_gap < -g.space {
                    if !start_is_paragraph {
                        paragraph = true;
                    }
                } else if x_gap.abs() < 0.25 * g.width && start_hanging {
                    hanging = true;
                }
            }
        }
        if paragraph {
            self.output.push(Out::LineSeparator);
            self.output.push(Out::ParagraphEnd);
            self.output.push(Out::ParagraphStart);
        } else {
            self.output.push(Out::LineSeparator);
        }
        self.last_line_start = Some((g.x, paragraph, hanging));
    }

    fn write_line(&mut self, line: Vec<Piece>) {
        let mut s = String::new();
        for p in line {
            match p {
                Piece::Text(t) => s.push_str(&t),
                Piece::Space => s.push(' '),
            }
        }
        self.output.push(Out::Line(ligatures(&s)));
    }

    fn finish(mut self, text: &mut Text) {
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            self.write_line(line);
            self.output.push(Out::ParagraphEnd);
        }
        for o in self.output {
            match o {
                Out::Line(s) => text.chars(&s),
                Out::LineSeparator => text.chars("\n"),
                Out::ParagraphStart => text.start("p"),
                Out::ParagraphEnd => text.end("p"),
            }
        }
    }
}

fn overlap(y1: f64, h1: f64, y2: f64, h2: f64) -> bool {
    (y1 - y2).abs() < 0.1 || (y2 <= y1 && y2 >= y1 - h1) || (y1 <= y2 && y1 >= y2 - h2)
}

/// The ligature characters written out as the letters they join.
fn ligatures(s: &str) -> String {
    if !s.chars().any(|c| ('\u{FB00}'..='\u{FB06}').contains(&c)) {
        return s.to_string();
    }
    s.chars()
        .map(|c| match c {
            '\u{FB00}' => "ff".to_string(),
            '\u{FB01}' => "fi".to_string(),
            '\u{FB02}' => "fl".to_string(),
            '\u{FB03}' => "ffi".to_string(),
            '\u{FB04}' => "ffl".to_string(),
            '\u{FB05}' | '\u{FB06}' => "st".to_string(),
            other => other.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PDF of the given objects, with a correct cross-reference table.
    fn build(objects: &[Vec<u8>], trailer_extra: &str) -> Vec<u8> {
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref = out.len();
        out.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for o in offsets {
            out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R {trailer_extra} >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    fn stream(data: &[u8], compress: bool) -> Vec<u8> {
        let body = if compress {
            use std::io::Write;
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            z.write_all(data).unwrap();
            z.finish().unwrap()
        } else {
            data.to_vec()
        };
        let filter = if compress { " /Filter /FlateDecode" } else { "" };
        let mut out = format!("<< /Length {}{filter} >>\nstream\n", body.len()).into_bytes();
        out.extend_from_slice(&body);
        out.extend_from_slice(b"\nendstream");
        out
    }

    fn sample(compress: bool) -> Vec<u8> {
        let content: &[u8] = b"BT /F1 12 Tf 72 720 Td (This is a test, with umlauts, from M\\374nchen) Tj ET\n\
                        BT /F1 12 Tf 72 690 Td (Also contains newlines for testing.) Tj ET\n\
                        BT /F1 12 Tf 72 660 Td (And one more.) Tj 0 -14 Td [(Same) -300 (paragraph)] TJ ET\n";
        build(
            &[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
                b"<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                    .to_vec(),
                stream(content, compress),
                b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec(),
                b"<< /Title (A title) /Author <FEFF004A0061006E0065> /CreationDate (D:20210327111200+07'00') >>"
                    .to_vec(),
            ],
            "/Info 6 0 R",
        )
    }

    fn read(bytes: &[u8]) -> Result<(String, Meta), Failure> {
        let mut text = Text::new(None);
        let meta = parse(bytes, &mut text)?;
        Ok((text.out, meta))
    }

    #[test]
    fn lines_paragraphs_and_the_info_dictionary() {
        for compress in [false, true] {
            let (text, meta) = read(&sample(compress)).unwrap();
            assert_eq!(
                text,
                "This is a test, with umlauts, from M\u{fc}nchen\n\nAlso contains newlines for testing.\n\n\
                 And one more.\nSame paragraph\n\n"
            );
            assert_eq!(meta.title.as_deref(), Some("A title"));
            assert_eq!(meta.author.as_deref(), Some("Jane"));
            assert_eq!(meta.date.as_deref(), Some("2021-03-27T04:12:00Z"));
        }
    }

    #[test]
    fn a_to_unicode_map_names_the_characters() {
        let cmap: &[u8] = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap\n\
                     1 begincodespacerange <0000> <FFFF> endcodespacerange\n\
                     2 beginbfchar <0001> <0E20> <0002> <0E32> endbfchar\n\
                     1 beginbfrange <0010> <0012> <0041> endbfrange\nendcmap end end";
        let pdf = build(
            &[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
                b"<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                    .to_vec(),
                stream(b"BT /F1 14 Tf 72 700 Td <00010002> Tj 0 -60 Td <001000110012> Tj ET", true),
                b"<< /Type /Font /Subtype /Type0 /Encoding /Identity-H /DescendantFonts [6 0 R] /ToUnicode 7 0 R >>"
                    .to_vec(),
                b"<< /Type /Font /Subtype /CIDFontType2 /DW 600 >>".to_vec(),
                stream(cmap, false),
            ],
            "",
        );
        assert_eq!(read(&pdf).unwrap().0, "\u{e20}\u{e32}\n\nABC\n\n");
    }

    #[test]
    fn a_broken_table_is_found_by_scanning() {
        let mut pdf = sample(false);
        let at = rfind(&pdf, b"startxref").unwrap();
        pdf.truncate(at);
        pdf.extend_from_slice(b"startxref\n9\n%%EOF\n");
        assert!(read(&pdf).unwrap().0.starts_with("This is a test"));
    }

    #[test]
    fn a_password_is_refused() {
        let pdf = build(
            &[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
                b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
                b"<< /Filter /Standard /V 2 /R 3 /Length 128 /P -4 /O <00> /U <0102030405060708090A0B0C0D0E0F10> >>"
                    .to_vec(),
            ],
            "/Encrypt 3 0 R /ID [<00112233> <00112233>]",
        );
        assert_eq!(read(&pdf).unwrap_err().kind, "encrypted_document_exception");
    }

    #[test]
    fn damaged_files_end_quickly() {
        let whole = sample(true);
        for cut in (0..whole.len()).step_by(7) {
            let _ = read(&whole[..cut]);
        }
        let deep = [b"%PDF-1.4\n1 0 obj ".to_vec(), vec![b'['; 100_000]].concat();
        let _ = read(&deep);
        let _ = read(
            b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n2 0 obj<</Type/Pages/Kids[2 0 R]>>endobj\ntrailer<</Root 1 0 R>>",
        );
        let _ = read(b"%PDF-1.4\n1 0 obj<</Length 1 0 R>>stream\nabc\nendstream endobj\ntrailer<</Root 1 0 R>>");
    }

    #[test]
    fn filters_and_dates() {
        assert_eq!(ascii_hex(b"48 65 6C6C6F>"), b"Hello");
        assert_eq!(ascii85(b"<~87cURD]i,\"Ebo80~>"), b"Hello World!");
        assert_eq!(run_length(&[2, b'a', b'b', b'c', 254, b'x', 128]), b"abcxxx");
        assert_eq!(pdf_date("D:20260102030405Z").as_deref(), Some("2026-01-02T03:04:05Z"));
        assert_eq!(pdf_date("D:2026").as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(
            rc4(b"Key", b"Plaintext"),
            [0xBB, 0xF3, 0x16, 0xE8, 0xD9, 0x40, 0xAF, 0x0A, 0xD3]
        );
    }
}
