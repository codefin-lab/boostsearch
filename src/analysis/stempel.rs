//! Polish, stemmed by the table Stempel reads.
//!
//! Stempel is not an algorithm written out in code the way Snowball is: it is
//! a trie of patch commands, learned from a dictionary of words and their
//! stems, and shipped as a table. What is here is the reader for that table
//! and the interpreter for the commands in it -- the Egothor `MultiTrie2` and
//! `Diff`, which is what OpenSearch's analysis-stempel plugin runs through
//! Lucene.
//!
//! The table itself is `stemmer_20000.tbl`, vendored beside this file from
//! Apache Lucene's `lucene-analysis-stempel` jar. It was built by the Egothor
//! project and is BSD-licensed; see `LICENSE-STEMPEL`.
//!
//! Everything works in UTF-16 code units rather than in `char`, because the
//! table's keys and the commands' parameters are Java `char`s and the
//! commands index into the word the way `StringBuilder` does. Counting
//! anything else would cut a different word.

use std::sync::OnceLock;

/// One cell of a row: where this character leads, and what it says.
struct Cell {
    next: i32,
    cmd: i32,
}

/// A row of the matrix a trie is stored as, keyed by the character taken.
struct Row {
    cells: std::collections::HashMap<u16, Cell>,
}

impl Row {
    fn cmd(&self, ch: u16) -> i32 {
        self.cells.get(&ch).map_or(-1, |c| c.cmd)
    }

    fn next(&self, ch: u16) -> i32 {
        self.cells.get(&ch).map_or(-1, |c| c.next)
    }
}

/// A trie of patch commands: a word read one character at a time leads to the
/// command that turns it into its stem.
struct Trie {
    rows: Vec<Row>,
    cmds: Vec<Vec<u16>>,
    root: i32,
    /// whether a key is read left to right; the Polish table is read from the
    /// end of the word, which is where its inflections are
    forward: bool,
}

impl Trie {
    fn row(&self, index: i32) -> Option<&Row> {
        usize::try_from(index).ok().and_then(|i| self.rows.get(i))
    }

    /// The command stored last on the path this key walks.
    ///
    /// A key that runs off the end of the trie keeps whatever it passed on
    /// the way, which is the point: a word the table never saw is still
    /// stemmed by the longest ending it shares with one that was.
    fn last_on_path(&self, key: &[u16]) -> Option<&[u16]> {
        if key.is_empty() {
            return None;
        }
        let mut now = self.row(self.root)?;
        let mut last: Option<&[u16]> = None;
        let mut at: usize = if self.forward { 0 } else { key.len() - 1 };
        let step: isize = if self.forward { 1 } else { -1 };
        let mut next_char = || {
            let ch = key[at];
            at = at.wrapping_add_signed(step);
            ch
        };
        for _ in 0..key.len() - 1 {
            let ch = next_char();
            let w = now.cmd(ch);
            if w >= 0 {
                last = self.cmds.get(w as usize).map(|c| c.as_slice());
            }
            let w = now.next(ch);
            match self.row(w) {
                Some(row) => now = row,
                None => return last,
            }
        }
        let w = now.cmd(next_char());
        if w >= 0 { self.cmds.get(w as usize).map(|c| c.as_slice()) } else { last }
    }
}

/// A trie of tries: one command is stored in pieces, a piece per level, and
/// the pieces are delimited by the command that skips characters.
pub struct MultiTrie2 {
    tries: Vec<Trie>,
    forward: bool,
}

/// The mark a trie puts at the end of a command: nothing follows it.
const EOM: u16 = b'*' as u16;

impl MultiTrie2 {
    /// The whole command for a key, gathered level by level.
    fn last_on_path(&self, key: &[u16]) -> Vec<u16> {
        let mut result: Vec<u16> = Vec::new();
        let mut key = key.to_vec();
        let mut last_key = key.clone();
        let mut previous: Option<Vec<u16>> = None;
        let mut last_ch = u16::from(b' ');
        for trie in &self.tries {
            let Some(r) = trie.last_on_path(&last_key) else { return result };
            if r == [EOM] {
                return result;
            }
            // a command that cannot follow the one before it ends the answer
            // where it stands rather than being read as part of it
            if cannot_follow(last_ch, r[0]) || r.len() < 2 {
                return result;
            }
            last_ch = r[r.len() - 2];
            let piece = r.to_vec();
            // a piece that begins by skipping characters is read against what
            // is left of the key rather than against the whole of it
            if piece[0] == u16::from(b'-') {
                if let Some(before) = &previous {
                    match skip(&key, length_pp(before), self.forward) {
                        Some(rest) => key = rest,
                        None => return result,
                    }
                }
                match skip(&key, length_pp(&piece), self.forward) {
                    Some(rest) => key = rest,
                    None => return result,
                }
            }
            result.extend_from_slice(&piece);
            previous = Some(piece);
            if !key.is_empty() {
                last_key = key.clone();
            }
        }
        result
    }

    /// The stem of a word, or `None` where the commands leave nothing behind.
    pub fn stem(&self, word: &str) -> Option<String> {
        let key: Vec<u16> = word.encode_utf16().collect();
        let cmd = self.last_on_path(&key);
        let mut buffer = key;
        apply(&mut buffer, &cmd);
        (!buffer.is_empty()).then(|| String::from_utf16_lossy(&buffer))
    }
}

/// Whether one command may be read after another. A skip cannot follow a
/// skip, and a delete cannot follow a delete: the two would mean the same
/// characters twice.
fn cannot_follow(after: u16, goes: u16) -> bool {
    (after == u16::from(b'-') || after == u16::from(b'D')) && after == goes
}

/// What is left of a key once `count` characters have been taken off the end
/// it is read from. `None` where there are not that many.
fn skip(key: &[u16], count: usize, forward: bool) -> Option<Vec<u16>> {
    if count > key.len() {
        return None;
    }
    Some(match forward {
        true => key[count..].to_vec(),
        false => key[..key.len() - count].to_vec(),
    })
}

/// How many characters of the key a command consumes.
fn length_pp(cmd: &[u16]) -> usize {
    let mut len: i64 = 0;
    let mut i = 0;
    while i + 1 < cmd.len() {
        let param = i64::from(cmd[i + 1]) - i64::from(b'a') + 1;
        match u8::try_from(cmd[i]) {
            Ok(b'-') | Ok(b'D') => len += param,
            Ok(b'R') => len += 1,
            _ => {}
        }
        i += 2;
    }
    usize::try_from(len).unwrap_or(0)
}

/// The patch command applied to the word it was found for.
///
/// The commands walk the word from its end: `-` skips, `R` replaces, `D`
/// deletes and `I` inserts. A command that would reach outside the word
/// leaves it as it stands, which is what Lucene's `Diff` does by catching the
/// exception rather than by checking first.
fn apply(dest: &mut Vec<u16>, diff: &[u16]) {
    let mut pos = dest.len() as i64 - 1;
    if pos < 0 {
        return;
    }
    for pair in diff.as_chunks::<2>().0 {
        let (cmd, param) = (pair[0], pair[1]);
        let count = i64::from(param) - i64::from(b'a') + 1;
        match u8::try_from(cmd) {
            Ok(b'-') => pos = pos - count + 1,
            Ok(b'R') => {
                if pos < 0 || pos as usize >= dest.len() {
                    return;
                }
                dest[pos as usize] = param;
            }
            Ok(b'D') => {
                let end = pos + 1;
                pos -= count - 1;
                if pos < 0 || pos as usize > dest.len() || pos > end {
                    return;
                }
                let end = (end as usize).min(dest.len());
                dest.drain(pos as usize..end);
            }
            Ok(b'I') => {
                pos += 1;
                if pos < 0 || pos as usize > dest.len() {
                    return;
                }
                dest.insert(pos as usize, param);
            }
            _ => {}
        }
        pos -= 1;
    }
}

/// A reader over the table, with the sizes and the byte order Java's
/// `DataInput` writes.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Option<u8> {
        let b = *self.bytes.get(self.at)?;
        self.at += 1;
        Some(b)
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes([self.byte()?, self.byte()?]))
    }

    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_be_bytes([self.byte()?, self.byte()?, self.byte()?, self.byte()?]))
    }

    fn count(&mut self) -> Option<usize> {
        usize::try_from(self.i32()?).ok()
    }

    /// A string as `writeUTF` wrote it: its length in bytes, then modified
    /// UTF-8, which is UTF-8 over UTF-16 code units -- a character outside the
    /// basic plane is written as its two surrogates, one after the other.
    fn text(&mut self) -> Option<Vec<u16>> {
        let len = usize::from(self.u16()?);
        let end = self.at.checked_add(len)?;
        if end > self.bytes.len() {
            return None;
        }
        let mut out = Vec::new();
        while self.at < end {
            let first = self.byte()?;
            let unit = match first {
                0x00..=0x7f => u16::from(first),
                0xc0..=0xdf => {
                    let second = self.byte()?;
                    (u16::from(first & 0x1f) << 6) | u16::from(second & 0x3f)
                }
                _ => {
                    let second = self.byte()?;
                    let third = self.byte()?;
                    (u16::from(first & 0x0f) << 12)
                        | (u16::from(second & 0x3f) << 6)
                        | u16::from(third & 0x3f)
                }
            };
            out.push(unit);
        }
        self.at = end;
        Some(out)
    }

    fn row(&mut self) -> Option<Row> {
        let mut cells = std::collections::HashMap::new();
        for _ in 0..self.count()? {
            let ch = self.u16()?;
            let cmd = self.i32()?;
            // how many commands the subtrie held before it was packed, and how
            // many characters this way discards: neither is read back, but
            // both are in the file
            let _cnt = self.i32()?;
            let next = self.i32()?;
            let _skip = self.i32()?;
            cells.insert(ch, Cell { next, cmd });
        }
        Some(Row { cells })
    }

    fn trie(&mut self) -> Option<Trie> {
        let forward = self.byte()? != 0;
        let root = self.i32()?;
        let mut cmds = Vec::new();
        for _ in 0..self.count()? {
            cmds.push(self.text()?);
        }
        let mut rows = Vec::new();
        for _ in 0..self.count()? {
            rows.push(self.row()?);
        }
        Some(Trie { rows, cmds, root, forward })
    }
}

/// The table as it is stored: a method name, then the tries.
///
/// The name says how the commands inside were broken up. Only a table whose
/// name holds `M` is a multi trie, and the Polish one is `-0ME2`; a plain
/// `Trie` table is not one this reads, and such a table is refused rather
/// than read as something it is not.
fn parse(bytes: &[u8]) -> Option<MultiTrie2> {
    let mut reader = Reader { bytes, at: 0 };
    let method = String::from_utf16_lossy(&reader.text()?).to_ascii_uppercase();
    if !method.contains('M') {
        return None;
    }
    let forward = reader.byte()? != 0;
    // how many characters of a command one level of the trie holds; a
    // `MultiTrie2` delimits by the skip command instead and does not use it
    let _by = reader.i32()?;
    let mut tries = Vec::new();
    for _ in 0..reader.count()? {
        tries.push(reader.trie()?);
    }
    Some(MultiTrie2 { tries, forward })
}

/// The Polish table, read once.
pub fn polish() -> Option<&'static MultiTrie2> {
    static TABLE: OnceLock<Option<MultiTrie2>> = OnceLock::new();
    TABLE.get_or_init(|| parse(include_bytes!("stemmer_20000.tbl"))).as_ref()
}

/// The words `polish_stem` leaves alone.
///
/// Lucene's `StempelFilter` does not look a short word up at all: the table
/// was learned from whole words, and what it says about two characters is
/// noise. Three is the length it uses.
pub const MIN_LENGTH: usize = 3;

/// One word, stemmed as `polish_stem` stems it. A word the table has nothing
/// to say about is the word itself.
pub fn stem(word: &str) -> String {
    if word.encode_utf16().count() < MIN_LENGTH {
        return word.to_string();
    }
    match polish().and_then(|table| table.stem(word)) {
        Some(stemmed) => stemmed,
        None => word.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::stem;

    /// What the table answers for these words. The first two are the cases
    /// the reference's own tests name; the rest are ordinary inflections, and
    /// they show the table is read rather than a handful of words looked up.
    #[test]
    fn polish_words_are_cut_to_their_stems() {
        let cases = [
            ("studenci", "student"),
            ("studenta", "student"),
            ("studentów", "student"),
            ("miastach", "miasto"),
            ("kotów", "kot"),
        ];
        for (word, want) in cases {
            assert_eq!(stem(word), want, "{word}");
        }
    }

    /// The point of a stemmer is not what one word becomes but that two forms
    /// of the same word become the same thing. What the stem of `książka`
    /// looks like is the table's business; that its nominative and its
    /// genitive meet is the search's.
    #[test]
    fn two_forms_of_a_word_meet() {
        assert_eq!(stem("książka"), stem("książki"));
        assert_eq!(stem("miasto"), stem("miastach"));
    }

    /// A word too short to stem, and a word in another script, are left as
    /// they stand rather than being cut into something else.
    #[test]
    fn short_words_and_foreign_words_are_left_alone() {
        assert_eq!(stem("do"), "do");
        assert_eq!(stem("a"), "a");
    }
}
