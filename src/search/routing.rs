//! Which shard a document belongs to, hashed the way OpenSearch hashes it.

use super::*;

/// BoostCore cannot order a `terms` aggregation by a nested bucket's doc_count,
/// so strip that order and reapply it to the finished buckets ourselves.
/// Lucene's `StringHelper.murmurhash3_x86_32`, which is what OpenSearch hashes
/// a string term with when a terms aggregation is split into partitions.
/// Which shard a document is routed to.
///
/// OpenSearch hashes the routing value as UTF-16 -- each character as two
/// bytes, low byte first -- with seed zero, and folds the result by the shard
/// count the way a floor-mod does, so a negative hash still names a shard.
pub(crate) fn routing_shard(routing: &str, shards: u64) -> u64 {
    let mut bytes = Vec::with_capacity(routing.len() * 2);
    for c in routing.encode_utf16() {
        bytes.push((c & 0xff) as u8);
        bytes.push((c >> 8) as u8);
    }
    let hash = murmur3_x86_32(&bytes, 0) as i64;
    hash.rem_euclid(shards as i64) as u64
}

pub(crate) fn murmur3_x86_32(data: &[u8], seed: u32) -> i32 {
    const C1: u32 = 0xcc9e_2d51;
    const C2: u32 = 0x1b87_3593;
    let mut h1 = seed;
    let blocks = data.len() / 4;
    for i in 0..blocks {
        let mut k1 = u32::from_le_bytes([data[i * 4], data[i * 4 + 1], data[i * 4 + 2], data[i * 4 + 3]]);
        k1 = k1.wrapping_mul(C1).rotate_left(15).wrapping_mul(C2);
        h1 ^= k1;
        h1 = h1.rotate_left(13).wrapping_mul(5).wrapping_add(0xe654_6b64);
    }
    let tail = &data[blocks * 4..];
    let mut k1: u32 = 0;
    if tail.len() >= 3 {
        k1 ^= (tail[2] as u32) << 16;
    }
    if tail.len() >= 2 {
        k1 ^= (tail[1] as u32) << 8;
    }
    if !tail.is_empty() {
        k1 ^= tail[0] as u32;
        k1 = k1.wrapping_mul(C1).rotate_left(15).wrapping_mul(C2);
        h1 ^= k1;
    }
    h1 ^= data.len() as u32;
    h1 ^= h1 >> 16;
    h1 = h1.wrapping_mul(0x85eb_ca6b);
    h1 ^= h1 >> 13;
    h1 = h1.wrapping_mul(0xc2b2_ae35);
    h1 ^= h1 >> 16;
    h1 as i32
}

/// HPPC's `BitMixer.mix64`, the numeric counterpart of the hash above.
pub(crate) fn mix64(v: i64) -> i64 {
    let mut z = v as u64;
    z = (z ^ (z >> 32)).wrapping_mul(0x4cd6_944c_5cc2_0b6d);
    z = (z ^ (z >> 29)).wrapping_mul(0xfc12_c5b1_9d32_59e9);
    (z ^ (z >> 32)) as i64
}

/// Which partition a terms bucket key falls in, hashed the way OpenSearch
/// hashes it so the same key lands in the same partition here.
pub(crate) fn term_partition(key: &Value, num: i64) -> i64 {
    let hash = match key {
        Value::String(s) => murmur3_x86_32(s.as_bytes(), 31) as i64,
        Value::Number(n) => mix64(n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64)),
        Value::Bool(b) => mix64(*b as i64),
        _ => 0,
    };
    hash.rem_euclid(num.max(1))
}
