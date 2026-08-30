//! Named time zones, read from the system's zone files.
//!
//! A zone is not an offset: it is a list of moments at which the offset
//! changed. Answering "what was the offset there, then" means finding the last
//! change before the instant asked about, which is what a TZif file records.

use std::collections::HashMap;
use std::sync::OnceLock;

use parking_lot::RwLock;

/// A zone's history: the moments it changed, and the offset in force after
/// each. The offset before the first moment is the one at the front.
#[derive(Clone, Default)]
pub struct Zone {
    transitions: Vec<(i64, i32)>,
    initial: i32,
}

impl Zone {
    /// The offset in seconds east of UTC at an instant.
    pub fn offset_at(&self, unix_secs: i64) -> i32 {
        match self.transitions.partition_point(|(at, _)| *at <= unix_secs) {
            0 => self.initial,
            n => self.transitions[n - 1].1,
        }
    }
}

fn be32(b: &[u8]) -> i32 {
    i32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn be64(b: &[u8]) -> i64 {
    i64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// One TZif data block: how long it is, and what it says.
fn read_block(data: &[u8], at: usize, time_width: usize) -> Option<(Zone, usize)> {
    let head = data.get(at..at + 44)?;
    if &head[0..4] != b"TZif" {
        return None;
    }
    let count = |i: usize| be32(&head[20 + i * 4..]) as usize;
    let (isutcnt, isstdcnt, leapcnt, timecnt, typecnt, charcnt) =
        (count(0), count(1), count(2), count(3), count(4), count(5));
    let mut p = at + 44;
    let times: Vec<i64> = (0..timecnt)
        .map(|i| {
            let b = &data[p + i * time_width..];
            if time_width == 8 { be64(b) } else { be32(b) as i64 }
        })
        .collect();
    p += timecnt * time_width;
    let indices = data.get(p..p + timecnt)?.to_vec();
    p += timecnt;
    let types: Vec<(i32, bool)> = (0..typecnt)
        .map(|i| {
            let b = &data[p + i * 6..];
            (be32(b), b[4] != 0)
        })
        .collect();
    p += typecnt * 6;
    p += charcnt + leapcnt * (time_width + 4) + isstdcnt + isutcnt;

    // before the first change, a zone is on the first offset that is not a
    // daylight one, which is how the format says "standard time here"
    let initial = types
        .iter()
        .find(|(_, dst)| !*dst)
        .or_else(|| types.first())
        .map(|(off, _)| *off)
        .unwrap_or(0);
    let transitions = times
        .into_iter()
        .zip(indices.into_iter())
        .filter_map(|(t, i)| types.get(i as usize).map(|(off, _)| (t, *off)))
        .collect();
    Some((Zone { transitions, initial }, p))
}

fn load(name: &str) -> Option<Zone> {
    // a name is a path under the zone directory, and nothing else: it may not
    // climb out of it
    if name.contains("..") || name.starts_with('/') {
        return None;
    }
    let data = std::fs::read(format!("/usr/share/zoneinfo/{name}")).ok()?;
    let version = *data.get(4)?;
    let (zone, next) = read_block(&data, 0, 4)?;
    if version == b'2' || version == b'3' || version == b'4' {
        // the second block says the same thing with room for dates outside
        // what a 32-bit count of seconds can hold
        if let Some((wide, _)) = read_block(&data, next, 8) {
            return Some(wide);
        }
    }
    Some(zone)
}

fn cache() -> &'static RwLock<HashMap<String, Option<Zone>>> {
    static CACHE: OnceLock<RwLock<HashMap<String, Option<Zone>>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// The offset in seconds east of UTC that a zone was on at an instant.
///
/// A zone may be named, or written as the offset itself.
pub fn offset_at(zone: &str, unix_secs: i64) -> Option<i32> {
    let z = zone.trim();
    if z.is_empty() || z == "Z" || z.eq_ignore_ascii_case("utc") {
        return Some(0);
    }
    if let Some(rest) = z.strip_prefix(['+', '-']) {
        let sign = if z.starts_with('-') { -1 } else { 1 };
        let (h, m) = match rest.split_once(':') {
            Some((h, m)) => (h, m),
            None if rest.len() == 4 => (&rest[..2], &rest[2..]),
            None => (rest, "0"),
        };
        let h: i32 = h.parse().ok()?;
        let m: i32 = m.parse().ok()?;
        return Some(sign * (h * 3600 + m * 60));
    }
    if let Some(hit) = cache().read().get(z) {
        return hit.as_ref().map(|zone| zone.offset_at(unix_secs));
    }
    let loaded = load(z);
    let answer = loaded.as_ref().map(|zone| zone.offset_at(unix_secs));
    cache().write().insert(z.to_string(), loaded);
    answer
}
