//! HDR histogram percentiles, and the aggregations that need the raw values.
//!
//! BoostCore computes percentiles with a t-digest, which answers a different
//! question from OpenSearch's HDR option: HDR reports the *highest equivalent
//! value* of the bucket a value falls in, so its answers are reproducible and
//! slightly above the input. Matching it means implementing the bucketing.

use std::collections::BTreeMap;

/// `subBucketCount` for three significant digits is 2048, i.e. 11 bits.
const SUB_BITS: u32 = 11;

/// The value range a `DoubleHistogram` starts out covering, before any
/// recording pulls it down: HdrHistogram opens at 2^800 so that the first
/// values recorded force the range downwards rather than upwards.
const INITIAL_LOW: f64 = 6.668014432879854e240; // 2^800

/// How much wider than the asked-for range the integer histogram is: a ratio
/// of 2 needs three binary orders of magnitude to hold every boundary.
const INTERNAL_RATIO: f64 = 8.0;

pub struct HdrHistogram {
    /// bucket floor (in fixed-point units) -> how many values landed in it
    buckets: BTreeMap<u64, u64>,
    pub count: u64,
    /// the bottom of the double range the histogram currently covers
    low: f64,
    /// the first double value the range no longer covers
    high: f64,
}

impl Default for HdrHistogram {
    fn default() -> Self {
        Self {
            buckets: BTreeMap::new(),
            count: 0,
            low: INITIAL_LOW,
            high: INITIAL_LOW * INTERNAL_RATIO,
        }
    }
}

fn unit_size(iv: u64) -> u64 {
    let sub_mask: u64 = (1 << SUB_BITS) - 1;
    let lz_base = 64 - SUB_BITS;
    let nlz = (iv | sub_mask).leading_zeros();
    let bucket = lz_base.saturating_sub(nlz);
    1u64 << bucket
}

impl HdrHistogram {
    /// The values a histogram holds are integers, and the scale between them
    /// and the doubles recorded is chosen by what has been recorded so far:
    /// the covered range starts absurdly high and is halved until the first
    /// value fits, then doubled again if a later value runs off the top. Every
    /// step is a whole binary order, so the integers already recorded only
    /// need shifting rather than rebucketing.
    fn fit(&mut self, value: f64) {
        while value < self.low {
            self.low /= 2.0;
            self.high /= 2.0;
            self.rescale(true);
        }
        while value >= self.high {
            // halving the recorded integers is only reversible while none of
            // them would land in the bottom half of the first bucket, where
            // the scale does not change; when one would, the range grows at
            // the top instead and the scale stays as it was
            let least = self.buckets.keys().find(|k| **k > 0).copied().unwrap_or(u64::MAX);
            if least >= 1 << SUB_BITS {
                self.low *= 2.0;
                self.high *= 2.0;
                self.rescale(false);
            } else {
                self.high *= 2.0;
            }
        }
    }

    /// Move every recorded value one binary order, keeping the double it
    /// stands for the same.
    fn rescale(&mut self, up: bool) {
        if self.buckets.is_empty() {
            return;
        }
        self.buckets =
            self.buckets.iter().map(|(k, n)| (if up { k << 1 } else { k >> 1 }, *n)).collect();
    }

    /// The integers per unit of value, once the range has settled.
    fn scale(&self) -> f64 {
        (1u64 << (SUB_BITS - 1)) as f64 / self.low
    }

    pub fn record(&mut self, value: f64) {
        if !value.is_finite() || value < 0.0 {
            return;
        }
        if value > 0.0 {
            self.fit(value);
        }
        let iv = (value * self.scale()) as u64;
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
        let scale = self.scale();
        let target = ((percentile / 100.0) * self.count as f64).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (floor, n) in &self.buckets {
            seen += n;
            if seen >= target {
                let unit = unit_size(*floor);
                return Some((floor + unit - 1) as f64 / scale);
            }
        }
        self.buckets.keys().next_back().map(|floor| (floor + unit_size(*floor) - 1) as f64 / scale)
    }
}

/// Median absolute deviation: the median of each value's distance from the
/// median. OpenSearch computes it on a t-digest sketch, so it reports an
/// approximation rather than the exact statistic.
pub fn median_absolute_deviation(values: &mut [f64]) -> Option<f64> {
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
    // the value standing at that share of the way through, counted the way
    // OpenSearch counts it: the one whose place is reached, not a point
    // interpolated between two of them
    let at = (q * sorted.len() as f64).floor() as usize;
    sorted[at.min(sorted.len() - 1)]
}
