//! HyperLogLog++, the sketch a `cardinality` aggregation counts with.
//!
//! OpenSearch does not count distinct values exactly: it hashes each one and
//! keeps a `HyperLogLogPlusPlus` sketch, whose answer is an estimate. How far
//! the estimate is from the truth depends on `precision_threshold`, which
//! decides how many registers the sketch has. Below the threshold the sketch
//! is a hash set counted by linear counting, and its answer is exact for a
//! small enough set; past it the sketch is the usual bank of registers and the
//! answer is corrected by the paper's empirical bias tables. Counting exactly
//! here reads 60 where the reference reads 51 over the aggregation corpus.
//!
//! The port follows OpenSearch's `HyperLogLogPlusPlus`,
//! `AbstractHyperLogLog`, `AbstractLinearCounting` and the hashes
//! `CardinalityAggregator` feeds them, so an estimate from the same values
//! comes out the same. The shards do not need separate sketches: a register
//! holds the largest run length any value gave it and a hash set holds each
//! encoded hash once, so merging one sketch per shard leaves what one sketch
//! over all the values holds.

use super::hll_tables::{BIAS_DATA, RAW_ESTIMATE_DATA, THRESHOLDS};
use super::tdigest::java_log;

/// The precision the sparse hash set encodes its hashes at.
const P2: u32 = 25;
const BIAS_K: usize = 6;
const MAX_LOAD_FACTOR: f64 = 0.75;
const MIN_PRECISION: u32 = 4;
const MAX_PRECISION: u32 = 18;

/// The precision used when the request does not ask for one.
pub(crate) const DEFAULT_PRECISION: u32 = 14;

/// `PackedInts.bitsRequired`.
fn bits_required(bits: i64) -> u32 {
    (64 - bits.leading_zeros()).max(1)
}

/// `precisionFromThreshold`: the precision at which a hash set holding that
/// many entries still fits the registers the sketch has.
pub(crate) fn precision_from_threshold(count: i64) -> u32 {
    let hash_table_entries = (count as f64 / MAX_LOAD_FACTOR).ceil() as i64;
    bits_required(hash_table_entries.saturating_mul(4)).clamp(MIN_PRECISION, MAX_PRECISION)
}

fn linear_counting(m: i64, v: i64) -> i64 {
    java_round(m as f64 * java_log(m as f64 / v as f64))
}

/// `Math.round`, which is not Rust's: it takes the floor of one half more.
fn java_round(x: f64) -> i64 {
    (x + 0.5).floor() as i64
}

/// `BitMixer.mix64`, the hash a numeric value is counted under.
pub(crate) fn mix64(z: i64) -> i64 {
    let mut z = z as u64;
    z = (z ^ (z >> 32)).wrapping_mul(0x4cd6_944c_5cc2_0b6d);
    z = (z ^ (z >> 29)).wrapping_mul(0xfc12_c5b1_9d32_59e9);
    (z ^ (z >> 32)) as i64
}

/// `MurmurHash3.hash128(..., 0, hash).h1`, the hash a text value is counted
/// under.
pub(crate) fn murmur3_h1(key: &[u8]) -> i64 {
    const C1: u64 = 0x87c3_7b91_1142_53d5;
    const C2: u64 = 0x4cf5_ad43_2745_937f;
    fn fmix(mut k: u64) -> u64 {
        k ^= k >> 33;
        k = k.wrapping_mul(0xff51_afd7_ed55_8ccd);
        k ^= k >> 33;
        k = k.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        k ^= k >> 33;
        k
    }
    let length = key.len();
    let mut h1: u64 = 0;
    let mut h2: u64 = 0;
    let blocks = length / 16;
    for i in 0..blocks {
        let at = i * 16;
        let mut k1 = u64::from_le_bytes(key[at..at + 8].try_into().unwrap());
        let mut k2 = u64::from_le_bytes(key[at + 8..at + 16].try_into().unwrap());
        k1 = k1.wrapping_mul(C1).rotate_left(31).wrapping_mul(C2);
        h1 ^= k1;
        h1 = h1.rotate_left(27).wrapping_add(h2).wrapping_mul(5).wrapping_add(0x52dc_e729);
        k2 = k2.wrapping_mul(C2).rotate_left(33).wrapping_mul(C1);
        h2 ^= k2;
        h2 = h2.rotate_left(31).wrapping_add(h1).wrapping_mul(5).wrapping_add(0x3849_5ab5);
    }
    let tail = &key[blocks * 16..];
    let mut k1: u64 = 0;
    let mut k2: u64 = 0;
    // the tail, byte by byte from the far end, as the switch falls through
    for (i, b) in tail.iter().enumerate().skip(8) {
        k2 ^= (*b as u64) << ((i - 8) * 8);
    }
    if tail.len() > 8 {
        k2 = k2.wrapping_mul(C2).rotate_left(33).wrapping_mul(C1);
        h2 ^= k2;
    }
    for (i, b) in tail.iter().enumerate().take(8) {
        k1 ^= (*b as u64) << (i * 8);
    }
    if !tail.is_empty() {
        k1 = k1.wrapping_mul(C1).rotate_left(31).wrapping_mul(C2);
        h1 ^= k1;
    }
    h1 ^= length as u64;
    h2 ^= length as u64;
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    h1 = fmix(h1);
    h2 = fmix(h2);
    h1 = h1.wrapping_add(h2);
    h1 as i64
}

/// Which of the two shapes the sketch is in, as OpenSearch's `algorithm` bit
/// says: a hash set of encoded hashes, or the registers themselves.
enum Shape {
    Linear { table: Vec<i32>, size: i32 },
    Dense { registers: Vec<u8> },
}

pub(crate) struct HyperLogLogPlusPlus {
    p: u32,
    m: usize,
    alpha_mm: f64,
    /// how many entries the hash set holds before the registers take over
    threshold: i32,
    shape: Shape,
}

impl HyperLogLogPlusPlus {
    pub(crate) fn new(precision: u32) -> HyperLogLogPlusPlus {
        let p = precision.clamp(MIN_PRECISION, MAX_PRECISION);
        let m = 1usize << p;
        let alpha = match p {
            4 => 0.673,
            5 => 0.697,
            _ => 0.7213 / (1.0 + 1.079 / m as f64),
        };
        // the hash set lives in the register bytes, four bytes to an entry
        let capacity = m / 4;
        HyperLogLogPlusPlus {
            p,
            m,
            alpha_mm: alpha * m as f64 * m as f64,
            threshold: (capacity as f64 * MAX_LOAD_FACTOR) as i32,
            shape: Shape::Linear { table: vec![0; capacity], size: 0 },
        }
    }

    pub(crate) fn collect(&mut self, hash: i64) {
        match &mut self.shape {
            Shape::Linear { .. } => {
                let encoded = encode_hash(hash, self.p);
                let new_size = self.add_encoded(encoded);
                if new_size > self.threshold {
                    self.upgrade_to_hll();
                }
            }
            Shape::Dense { registers } => {
                let index = index(hash, self.p) as usize;
                let run_len = run_len(hash, self.p);
                if run_len > registers[index] {
                    registers[index] = run_len;
                }
            }
        }
    }

    /// `LinearCounting.addEncoded`: open addressing, and nothing to do for a
    /// hash the set already holds.
    fn add_encoded(&mut self, encoded: i32) -> i32 {
        let Shape::Linear { table, size } = &mut self.shape else { return -1 };
        let mask = (table.len() - 1) as i32;
        let mut i = (encoded & mask) as usize;
        loop {
            let v = table[i];
            if v == 0 {
                table[i] = encoded;
                *size += 1;
                return *size;
            }
            if v == encoded {
                return -1;
            }
            i = ((i as i32 + 1) & mask) as usize;
        }
    }

    fn upgrade_to_hll(&mut self) {
        let Shape::Linear { table, .. } = &self.shape else { return };
        let encoded: Vec<i32> = table.iter().copied().filter(|v| *v != 0).collect();
        let mut registers = vec![0u8; self.m];
        for e in encoded {
            let index = decode_index(e, self.p);
            let run_len = decode_run_len(e, self.p);
            if run_len > registers[index] {
                registers[index] = run_len;
            }
        }
        self.shape = Shape::Dense { registers };
    }

    pub(crate) fn cardinality(&self) -> i64 {
        match &self.shape {
            Shape::Linear { size, .. } => {
                let m = 1i64 << P2;
                linear_counting(m, m - *size as i64)
            }
            Shape::Dense { registers } => {
                let mut inverse_sum = 0.0;
                let mut zeros = 0i64;
                for run_len in registers {
                    inverse_sum += 1.0 / (1u64 << *run_len) as f64;
                    if *run_len == 0 {
                        zeros += 1;
                    }
                }
                let e1 = self.alpha_mm / inverse_sum;
                let e2 = if e1 <= 5.0 * self.m as f64 { e1 - self.estimate_bias(e1) } else { e1 };
                let h =
                    if zeros != 0 { linear_counting(self.m as i64, zeros) } else { java_round(e2) };
                if h <= THRESHOLDS[(self.p - 4) as usize] { h } else { java_round(e2) }
            }
        }
    }

    /// `estimateBias`: the bias measured near this raw estimate, averaged over
    /// the six nearest measurements by inverse distance.
    fn estimate_bias(&self, e: f64) -> f64 {
        let raw = RAW_ESTIMATE_DATA[(self.p - 4) as usize];
        let bias = BIAS_DATA[(self.p - 4) as usize];
        let mut weights = [0.0f64; BIAS_K];
        let mut index = bias.len() as i64 - BIAS_K as i64;
        for (i, r) in raw.iter().enumerate() {
            let w = 1.0 / (r - e).abs();
            let j = i % weights.len();
            if w.is_infinite() {
                return bias[i];
            } else if weights[j] >= w {
                index = i as i64 - BIAS_K as i64;
                break;
            }
            weights[j] = w;
        }
        let mut weight_sum = 0.0;
        let mut bias_sum = 0.0;
        for (i, w) in weights.iter().enumerate() {
            let b = bias[(index + i as i64) as usize];
            bias_sum += w * b;
            weight_sum += w;
        }
        bias_sum / weight_sum
    }
}

fn index(hash: i64, p: u32) -> i64 {
    ((hash as u64) >> (64 - p)) as i64
}

fn run_len(hash: i64, p: u32) -> u8 {
    (1 + ((hash as u64) << p).leading_zeros().min(64 - p)) as u8
}

/// `encodeHash`: the hash cut down to the 32 bits the hash set keeps, with the
/// run length carried along when the index alone would not give it.
fn encode_hash(hash: i64, p: u32) -> i32 {
    let e = ((hash as u64) >> (64 - P2)) as i64;
    let mask = (1i64 << (P2 - p)) - 1;
    let encoded = if e & mask == 0 {
        let run_len = 1 + ((hash as u64) << P2).leading_zeros().min(64 - P2) as i64;
        (e << 7) | (run_len << 1) | 1
    } else {
        e << 1
    };
    encoded as i32
}

fn decode_run_len(encoded: i32, p: u32) -> u8 {
    if encoded & 1 == 1 {
        (((encoded as u32 >> 1) & 0x3F) + (P2 - p)) as u8
    } else {
        let bits = (encoded as u32) << (31 + p - P2);
        (1 + bits.leading_zeros()) as u8
    }
}

fn decode_index(encoded: i32, p: u32) -> usize {
    let index =
        if encoded & 1 == 1 { (encoded as u32 >> 7) as u64 } else { (encoded as u32 >> 1) as u64 };
    (index >> (P2 - p)) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_precision_follows_the_threshold() {
        // read back from OpenSearch's precisionFromThreshold
        assert_eq!(precision_from_threshold(0), 4);
        assert_eq!(precision_from_threshold(10), 6);
        assert_eq!(precision_from_threshold(100), 10);
        assert_eq!(precision_from_threshold(3000), 14);
        assert_eq!(precision_from_threshold(40000), 18);
        assert_eq!(precision_from_threshold(1_000_000), 18);
    }

    #[test]
    fn the_hashes_are_the_references() {
        // read back from BitMixer.mix64 and MurmurHash3.hash128 on Java 21
        assert_eq!(mix64(0), 0);
        assert_eq!(mix64(1), -2508561340476696217);
        assert_eq!(mix64(59), 2283511683835779891);
        assert_eq!(mix64(1.5f64.to_bits() as i64), 2074684470874416807);
        assert_eq!(murmur3_h1(b""), 0);
        assert_eq!(murmur3_h1(b"s0"), 5931443823129705114);
        assert_eq!(murmur3_h1(b"123456789"), 4360720697772133540);
        assert_eq!(murmur3_h1(b"a rather longer value than sixteen bytes"), 8257159140255144017);
    }

    #[test]
    fn a_small_count_is_exact_and_a_coarse_sketch_is_not() {
        // the corpus's two cardinality aggregations: 4 keyword values at the
        // default precision, and 60 longs at a precision_threshold of 10
        let mut d = HyperLogLogPlusPlus::new(DEFAULT_PRECISION);
        for v in ["s0", "s1", "s2", "s3"] {
            d.collect(murmur3_h1(v.as_bytes()));
        }
        assert_eq!(d.cardinality(), 4);
        let mut d = HyperLogLogPlusPlus::new(precision_from_threshold(10));
        for v in 0..60i64 {
            d.collect(mix64(v));
        }
        assert_eq!(d.cardinality(), 51);
    }
}
