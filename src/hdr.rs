//! HDR histogram percentiles, and the aggregations that need the raw values.
//!
//! tantivy computes percentiles with a t-digest, which answers a different
//! question from OpenSearch's HDR option: HDR reports the *highest equivalent
//! value* of the bucket a value falls in, so its answers are reproducible and
//! slightly above the input. Matching it means implementing the bucketing.

use std::collections::BTreeMap;

/// Values are held as fixed-point integers; this is the scale OpenSearch's
/// DoubleHistogram settles on for the ranges these aggregations see.
const RATIO: f64 = 1024.0;

/// `subBucketCount` for three significant digits is 2048, i.e. 11 bits.
const SUB_BITS: u32 = 11;

#[derive(Default)]
pub struct HdrHistogram {
    /// bucket floor (in fixed-point units) -> how many values landed in it
    buckets: BTreeMap<u64, u64>,
    pub count: u64,
}

fn unit_size(iv: u64) -> u64 {
    let sub_mask: u64 = (1 << SUB_BITS) - 1;
    let lz_base = 64 - SUB_BITS;
    let nlz = (iv | sub_mask).leading_zeros();
    let bucket = lz_base.saturating_sub(nlz);
    1u64 << bucket
}

impl HdrHistogram {
    pub fn record(&mut self, value: f64) {
        if !value.is_finite() || value < 0.0 {
            return;
        }
        let iv = (value * RATIO) as u64;
        let unit = unit_size(iv);
        let floor = (iv / unit) * unit;
        *self.buckets.entry(floor).or_insert(0) += 1;
        self.count += 1;
    }

    /// The value at a percentile, as HdrHistogram reports it: walk the buckets
    /// in order until the cumulative count reaches `ceil(p/100 * n)`, then give
    /// back the highest value that bucket still considers equivalent.
    pub fn value_at(&self, percentile: f64) -> Option<f64> {
        if self.count == 0 {
            return None;
        }
        let target = ((percentile / 100.0) * self.count as f64).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (floor, n) in &self.buckets {
            seen += n;
            if seen >= target {
                let unit = unit_size(*floor);
                return Some((floor + unit - 1) as f64 / RATIO);
            }
        }
        self.buckets
            .keys()
            .next_back()
            .map(|floor| (floor + unit_size(*floor) - 1) as f64 / RATIO)
    }
}

/// Median absolute deviation: the median of each value's distance from the
/// median. OpenSearch computes it on a t-digest sketch, so it reports an
/// approximation rather than the exact statistic.
pub fn median_absolute_deviation(values: &mut Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = quantile(values, 0.5);
    let mut deviations: Vec<f64> = values.iter().map(|v| (v - median).abs()).collect();
    deviations.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(quantile(&deviations, 0.5))
}

/// Linear-interpolated quantile over a sorted slice, which is what a t-digest
/// converges to once every value is kept.
pub fn quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let pos = q * (sorted.len() as f64 - 1.0);
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    if lo == hi {
        return sorted[lo];
    }
    sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo as f64)
}
