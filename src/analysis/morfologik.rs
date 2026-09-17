//! Words looked up in a Morfologik dictionary.
//!
//! A Morfologik dictionary is a finite-state automaton holding every inflected
//! form of every word, each followed by a separator and by an instruction for
//! turning the form back into its lemma. A form may be several words at once
//! -- `колу` is the dative of `кола`, of `коло` and of `кіл` -- so a lookup
//! answers with a list rather than with one word, and that is what makes this
//! a lemmatiser rather than a stemmer.
//!
//! What is here is a reader for the `CFSA2` container and for the four ways a
//! lemma may be encoded against its form, which is what OpenSearch's
//! analysis-ukrainian plugin runs through Lucene's `MorfologikFilter`.
//!
//! The dictionary itself is seven megabytes and is not vendored; see
//! `docs/ukrainian.md` for where it is read from.

use std::path::PathBuf;
use std::sync::OnceLock;

/// The magic the container begins with, and the version this reads.
const MAGIC: [u8; 4] = *b"\\fsa";
const VERSION_CFSA2: u8 = 0xc6;

/// The automaton was compiled with a count of strings in front of each node.
const FLAG_NUMBERS: u16 = 1 << 8;

/// The target of this arc is whatever follows the node's last arc, so the arc
/// carries no address.
const BIT_TARGET_NEXT: u8 = 1 << 7;
/// This is the node's last arc.
const BIT_LAST_ARC: u8 = 1 << 6;
/// A sequence may end here.
const BIT_FINAL_ARC: u8 = 1 << 5;
/// The low bits of the flag byte are an index into the label table, or zero
/// where the label is written out after the flags.
const LABEL_INDEX_MASK: u8 = (1 << 5) - 1;

/// How a lemma is written against the form it was found for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Encoder {
    /// the lemma itself, as it stands
    None,
    /// how much to cut off the end of the form, then what to put there
    Suffix,
    /// the same for both ends
    Prefix,
    /// a piece taken out of the middle, and the end replaced
    Infix,
}

impl Encoder {
    /// How many bytes of a code are the instruction rather than the letters.
    fn prefix_bytes(self) -> usize {
        match self {
            Encoder::None => 0,
            Encoder::Suffix => 1,
            Encoder::Prefix => 2,
            Encoder::Infix => 3,
        }
    }

    /// The lemma a code stands for, read against the form.
    ///
    /// `255` in any of the counts means the whole form goes: a lemma that
    /// shares nothing with its form is written out rather than patched.
    fn decode(self, form: &[u8], code: &[u8]) -> Option<Vec<u8>> {
        let remove_everything = 255usize;
        let at = |i: usize| code.get(i).map(|b| usize::from(b.wrapping_sub(b'A')));
        match self {
            Encoder::None => Some(code.to_vec()),
            Encoder::Suffix => {
                let mut truncate = at(0)?;
                if truncate == remove_everything {
                    truncate = form.len();
                }
                let keep = form.len().checked_sub(truncate)?;
                let mut out = form[..keep].to_vec();
                out.extend_from_slice(code.get(1..)?);
                Some(out)
            }
            Encoder::Prefix => {
                let (mut front, mut back) = (at(0)?, at(1)?);
                if front == remove_everything || back == remove_everything {
                    front = form.len();
                    back = 0;
                }
                let keep = form.len().checked_sub(front.checked_add(back)?)?;
                let mut out = form.get(front..front + keep)?.to_vec();
                out.extend_from_slice(code.get(2..)?);
                Some(out)
            }
            Encoder::Infix => {
                let (mut index, mut length, mut back) = (at(0)?, at(1)?, at(2)?);
                if length == remove_everything || back == remove_everything {
                    index = 0;
                    length = form.len();
                    back = 0;
                }
                let taken = index.checked_add(length)?.checked_add(back)?;
                let keep = form.len().checked_sub(taken)?;
                let mut out = form.get(..index)?.to_vec();
                out.extend_from_slice(form.get(index + length..index + length + keep)?);
                out.extend_from_slice(code.get(3..)?);
                Some(out)
            }
        }
    }
}

/// The automaton, as the bytes of its arcs and the table its labels are
/// indexed against.
struct Automaton {
    arcs: Vec<u8>,
    labels: Vec<u8>,
    has_numbers: bool,
    root: usize,
}

impl Automaton {
    /// The arcs of a node begin after the count in front of it, where there
    /// is one.
    fn first_arc(&self, node: usize) -> usize {
        if self.has_numbers { self.skip_vint(node) } else { node }
    }

    fn flags(&self, arc: usize) -> u8 {
        self.arcs.get(arc).copied().unwrap_or(0)
    }

    fn label(&self, arc: usize) -> u8 {
        let index = self.flags(arc) & LABEL_INDEX_MASK;
        if index > 0 {
            self.labels.get(usize::from(index)).copied().unwrap_or(0)
        } else {
            self.arcs.get(arc + 1).copied().unwrap_or(0)
        }
    }

    fn is_last(&self, arc: usize) -> bool {
        self.flags(arc) & BIT_LAST_ARC != 0
    }

    fn is_final(&self, arc: usize) -> bool {
        self.flags(arc) & BIT_FINAL_ARC != 0
    }

    fn next_arc(&self, arc: usize) -> usize {
        if self.is_last(arc) { 0 } else { self.skip_arc(arc) }
    }

    /// Where the arc after this one begins: past the flags, past the label
    /// where it is written out, and past the address where there is one.
    fn skip_arc(&self, arc: usize) -> usize {
        let flags = self.flags(arc);
        let mut at = arc + 1;
        if flags & LABEL_INDEX_MASK == 0 {
            at += 1;
        }
        if flags & BIT_TARGET_NEXT == 0 {
            at = self.skip_vint(at);
        }
        at
    }

    /// The node this arc leads to, or zero where it leads nowhere: a sequence
    /// that ends here and goes no further.
    fn target(&self, arc: usize) -> usize {
        if self.flags(arc) & BIT_TARGET_NEXT != 0 {
            // the target follows the node, so walk to the node's last arc and
            // take the byte after it
            let mut last = arc;
            while !self.is_last(last) {
                last = self.next_arc(last);
                if last == 0 {
                    return 0;
                }
            }
            self.skip_arc(last)
        } else {
            let at = if self.flags(arc) & LABEL_INDEX_MASK == 0 { arc + 2 } else { arc + 1 };
            self.read_vint(at)
        }
    }

    fn is_terminal(&self, arc: usize) -> bool {
        self.target(arc) == 0
    }

    /// The arc of this node labelled with this byte, or zero where it has
    /// none.
    fn arc(&self, node: usize, label: u8) -> usize {
        let mut arc = self.first_arc(node);
        while arc != 0 {
            if self.label(arc) == label {
                return arc;
            }
            arc = self.next_arc(arc);
        }
        0
    }

    fn read_vint(&self, mut at: usize) -> usize {
        let mut value = 0usize;
        let mut shift = 0u32;
        loop {
            let b = self.arcs.get(at).copied().unwrap_or(0);
            value |= usize::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return value;
            }
            at += 1;
            shift += 7;
        }
    }

    fn skip_vint(&self, mut at: usize) -> usize {
        while self.arcs.get(at).copied().unwrap_or(0) & 0x80 != 0 {
            at += 1;
        }
        at + 1
    }

    /// Every sequence of labels reachable from a node, in the order the
    /// automaton stores them -- which is the order a dictionary's lemmas were
    /// compiled in, and so the order they are to be answered in.
    fn sequences(&self, node: usize) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut stack = vec![self.first_arc(node)];
        let mut buffer: Vec<u8> = Vec::new();
        while let Some(&arc) = stack.last() {
            if arc == 0 {
                stack.pop();
                continue;
            }
            let depth = stack.len() - 1;
            *stack.last_mut().expect("the stack is not empty here") = self.next_arc(arc);
            buffer.truncate(depth);
            buffer.push(self.label(arc));
            if !self.is_terminal(arc) {
                stack.push(self.first_arc(self.target(arc)));
            }
            if self.is_final(arc) {
                out.push(buffer.clone());
            }
            // a malformed automaton must not be walked forever
            if out.len() > 4096 || stack.len() > 4096 {
                break;
            }
        }
        out
    }
}

/// A dictionary: the automaton, and what its `.info` says about how to read
/// what comes after the separator.
pub struct Dictionary {
    fsa: Automaton,
    separator: u8,
    encoder: Encoder,
}

impl Dictionary {
    /// Read the automaton and the properties beside it.
    fn read(fsa: &[u8], info: &str) -> Option<Dictionary> {
        let mut separator = b'+';
        let mut encoder = Encoder::Suffix;
        for line in info.lines() {
            let line = line.trim();
            let Some((key, value)) = line.split_once('=') else { continue };
            if line.starts_with('#') {
                continue;
            }
            match key.trim() {
                "fsa.dict.separator" => separator = *value.trim().as_bytes().first()?,
                "fsa.dict.encoder" => {
                    encoder = match value.trim().to_ascii_uppercase().as_str() {
                        "NONE" => Encoder::None,
                        "SUFFIX" => Encoder::Suffix,
                        "PREFIX" => Encoder::Prefix,
                        "INFIX" => Encoder::Infix,
                        _ => return None,
                    }
                }
                // a dictionary written in anything but UTF-8 would need its
                // bytes transcoded, and none of the ones this reads is
                "fsa.dict.encoding" => {
                    let encoding = value.trim().to_ascii_lowercase().replace('-', "");
                    if encoding != "utf8" {
                        return None;
                    }
                }
                _ => {}
            }
        }
        Some(Dictionary { fsa: automaton(fsa)?, separator, encoder })
    }

    /// The lemmas of one form, in the order the dictionary holds them. A form
    /// the dictionary does not know has none.
    pub fn lemmas(&self, word: &str) -> Vec<String> {
        let form = word.as_bytes();
        if form.is_empty() || form.contains(&self.separator) {
            return Vec::new();
        }
        // the form has to be in the automaton whole, and to be the start of
        // something longer -- the separator and the lemma's code
        let mut node = self.fsa.root;
        for (i, byte) in form.iter().enumerate() {
            let arc = self.fsa.arc(node, *byte);
            if arc == 0 {
                return Vec::new();
            }
            let last = i + 1 == form.len();
            if (last && self.fsa.is_final(arc)) || self.fsa.is_terminal(arc) {
                return Vec::new();
            }
            node = self.fsa.target(arc);
        }
        let arc = self.fsa.arc(node, self.separator);
        if arc == 0 || self.fsa.is_final(arc) {
            return Vec::new();
        }
        let mut out = Vec::new();
        for sequence in self.fsa.sequences(self.fsa.target(arc)) {
            // the code runs up to the next separator; what follows it is the
            // part of speech, which a search has no use for
            let end = (self.encoder.prefix_bytes()..sequence.len())
                .find(|at| sequence[*at] == self.separator)
                .unwrap_or(sequence.len());
            if let Some(lemma) = self.encoder.decode(form, &sequence[..end])
                && let Ok(text) = String::from_utf8(lemma)
            {
                out.push(text);
            }
        }
        out
    }
}

/// The container around the arcs: the magic, the version, the flags and the
/// table the frequent labels are indexed against.
fn automaton(bytes: &[u8]) -> Option<Automaton> {
    if bytes.get(..4)? != MAGIC || *bytes.get(4)? != VERSION_CFSA2 {
        return None;
    }
    let flags = u16::from_be_bytes([*bytes.get(5)?, *bytes.get(6)?]);
    let size = usize::from(*bytes.get(7)?);
    let labels = bytes.get(8..8 + size)?.to_vec();
    let mut fsa = Automaton {
        arcs: bytes.get(8 + size..)?.to_vec(),
        labels,
        has_numbers: flags & FLAG_NUMBERS != 0,
        root: 0,
    };
    // the automaton begins with a dummy node standing for the terminating
    // state; the root is what its first arc leads to
    fsa.root = fsa.target(fsa.first_arc(0));
    Some(fsa)
}

/// The Ukrainian dictionary, read once from wherever it was found.
pub fn ukrainian() -> Option<&'static Dictionary> {
    static DICT: OnceLock<Option<Dictionary>> = OnceLock::new();
    DICT.get_or_init(|| {
        for dir in dictionary_dirs() {
            let Ok(fsa) = std::fs::read(dir.join("ukrainian.dict")) else { continue };
            let info = std::fs::read_to_string(dir.join("ukrainian.info")).unwrap_or_default();
            if let Some(dictionary) = Dictionary::read(&fsa, &info) {
                return Some(dictionary);
            }
        }
        None
    })
    .as_ref()
}

/// Where the dictionary is looked for, in order. `docs/ukrainian.md` says
/// what each of these is for and what happens when none of them holds it.
fn dictionary_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(dir) = std::env::var("VELOSEARCH_UKRAINIAN_DICT") {
        out.push(PathBuf::from(dir));
    }
    for base in [std::env::var("VELOSEARCH_CONFIG").ok(), std::env::var("VELOSEARCH_DATA").ok()]
        .into_iter()
        .flatten()
    {
        out.push(PathBuf::from(&base).join("analysis-ukrainian"));
        out.push(PathBuf::from(&base).join("config").join("analysis-ukrainian"));
    }
    out.push(PathBuf::from("config").join("analysis-ukrainian"));
    if let Ok(home) = std::env::var("HOME") {
        out.push(PathBuf::from(home).join("velo-fixtures").join("ukrainian-dict"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::ukrainian;

    /// The dictionary is somebody else's data and is not vendored, so a
    /// machine without it skips this rather than failing.
    #[test]
    fn ukrainian_forms_are_read_back_as_their_lemmas() {
        let Some(dictionary) = ukrainian() else { return };
        assert_eq!(dictionary.lemmas("колу"), ["кола", "коло", "кіл"]);
        // a word that is its own lemma still answers with itself
        assert_eq!(dictionary.lemmas("кола").first().map(String::as_str), Some("кола"));
        // and a word no Ukrainian dictionary holds answers with nothing
        assert!(dictionary.lemmas("zzzqqq").is_empty());
    }
}
