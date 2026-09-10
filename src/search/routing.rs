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
        let mut k1 =
            u32::from_le_bytes([data[i * 4], data[i * 4 + 1], data[i * 4 + 2], data[i * 4 + 3]]);
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

/// How many routing shards the reference gives an index of this many shards
/// when none is asked for: the shard count doubled as far as 1,024 allows,
/// at least once -- which is what lets it be split later.
pub(crate) fn default_routing_shards(shards: u64) -> u64 {
    let shards = shards.max(1);
    let log2_shards = 64 - (shards - 1).leading_zeros() as u64;
    let splits = 10u64.saturating_sub(log2_shards).max(1);
    shards << splits
}

/// Which shard a routing value lands on, by the reference's fold where the
/// index records its routing shards: the hash taken modulo the routing
/// shards, then divided down to the real ones. An index without the record
/// -- every one made before this -- is folded by its shard count, as it was
/// written, so none of its documents moves.
pub(crate) fn routing_shard_in(routing: &str, shards: u64, routing_shards: Option<u64>) -> u64 {
    let shards = shards.max(1);
    match routing_shards {
        Some(rns) if rns >= shards && rns % shards == 0 => {
            let mut bytes = Vec::with_capacity(routing.len() * 2);
            for c in routing.encode_utf16() {
                bytes.push((c & 0xff) as u8);
                bytes.push((c >> 8) as u8);
            }
            let hash = murmur3_x86_32(&bytes, 0) as i64;
            (hash.rem_euclid(rns as i64) as u64) / (rns / shards)
        }
        _ => routing_shard(routing, shards),
    }
}

#[cfg(test)]
mod routing_shard_tests {
    use super::*;

    #[test]
    fn ids_land_on_the_shard_opensearch_3_1_puts_them_on() {
        // read back from OpenSearch 3.1.0 with `explain`, which names the
        // shard of each hit, for indices made with the default routing shards
        let keys =
            ["f0", "f1", "f2", "f7", "doc-42", "ab", "x", "q9", "k3", "user_1234", "ไทย", "é"];
        let seen: [(u64, [u64; 12]); 3] = [
            (2, [0, 1, 0, 0, 1, 0, 1, 0, 1, 1, 1, 0]),
            (3, [2, 2, 1, 1, 2, 2, 2, 1, 1, 0, 0, 1]),
            (5, [3, 2, 4, 4, 0, 3, 0, 2, 4, 2, 3, 4]),
        ];
        for (shards, want) in seen {
            let rns = Some(default_routing_shards(shards));
            let got: Vec<u64> = keys.iter().map(|k| routing_shard_in(k, shards, rns)).collect();
            assert_eq!(got, want, "{shards} shards");
        }
    }

    #[test]
    fn routing_shards_default_as_the_reference_computes_them() {
        assert_eq!(default_routing_shards(1), 1024);
        assert_eq!(default_routing_shards(2), 1024);
        assert_eq!(default_routing_shards(3), 768);
        assert_eq!(default_routing_shards(5), 640);
    }

    #[test]
    fn an_index_without_routing_shards_folds_as_before() {
        for id in ["1", "a", "doc-42", "zz"] {
            assert_eq!(routing_shard_in(id, 2, None), routing_shard(id, 2));
            assert!(routing_shard_in(id, 2, Some(1024)) < 2);
        }
    }
}
