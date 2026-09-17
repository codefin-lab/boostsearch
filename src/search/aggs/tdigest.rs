//! The t-digest OpenSearch estimates percentiles with, ported step for step.
//!
//! OpenSearch 3.1 keeps an `AVLTreeDigest` from t-digest 3.3 (its
//! `TDigestState`), and a percentile it reports is that sketch's estimate, not
//! the exact value at the rank. The estimate depends on everything the sketch
//! went through: the order the values arrived in, which centroid each one was
//! folded into, and how the shards' sketches were merged into one. So the port
//! keeps the library's arithmetic as it is -- the same scale function, the same
//! nearest-centroid search, the same order of floating-point operations -- and
//! `percentiles`, `percentile_ranks` and `median_absolute_deviation` replay the
//! same journey the reference's sketches take.
//!
//! The library holds its centroids in a balanced tree; here they are a pair of
//! vectors kept in the same order, which the sketch is small enough for. The
//! tree's rule that a centroid inserted next to an equal one comes after it is
//! kept, because the order of equal centroids decides which of them a later
//! value is folded into.
//!
//! Two things the vectors cannot follow, and both need a value that repeats.
//! The library's tree carries the weight of each subtree alongside it, and a
//! centroid whose weight grows without its mean moving -- which is what adding
//! a value it already stands for does -- is written in place, leaving those
//! sums one short. The weight before a centroid is what decides how much room
//! it has, so from there the reference admits a merge this port refuses: over
//! 2,000 whole numbers drawn from 0 to 5,000 the reference's 99th percentile
//! reads 4957.2 where this reads 4960.0. And where a value falls exactly
//! between two centroids the library picks one of them at random, so the
//! reference does not settle on one answer either: over 300 documents holding
//! ten distinct values it gave six different medians in six asks.

/// `AVLTreeDigest` with the `K_2` scale function, the library's default.
#[derive(Clone)]
pub(crate) struct AvlTreeDigest {
    compression: f64,
    means: Vec<f64>,
    counts: Vec<i64>,
    count: i64,
    min: f64,
    max: f64,
}

/// `ScaleFunction.K_2.max`: the largest share of the data one centroid may
/// stand for at `q`, where `Z = 4 log(n / compression) + 24`.
fn scale_max(q: f64, compression: f64, n: f64) -> f64 {
    let z = 4.0 * java_log(n / compression) + 24.0;
    z * q * (1.0 - q) / compression
}

/// The natural logarithm as Java's `Math.log` gives it, which is fdlibm's.
///
/// The platform's `ln` rounds differently in the last place for a few
/// arguments in every hundred, and the scale function is taken from it: one
/// place out moves a centroid's edge, and a percentile came out a hair from
/// the reference's. The cardinality sketch counts with the same logarithm.
#[allow(clippy::excessive_precision)] // fdlibm's constants, as it writes them
pub(crate) fn java_log(x: f64) -> f64 {
    const LN2_HI: f64 = 6.93147180369123816490e-01;
    const LN2_LO: f64 = 1.90821492927058770002e-10;
    const TWO54: f64 = 1.80143985094819840000e+16;
    const LG1: f64 = 6.666666666666735130e-01;
    const LG2: f64 = 3.999999999940941908e-01;
    const LG3: f64 = 2.857142874366239149e-01;
    const LG4: f64 = 2.222219843214978396e-01;
    const LG5: f64 = 1.818357216161805012e-01;
    const LG6: f64 = 1.531383769920937332e-01;
    const LG7: f64 = 1.479819860511658591e-01;
    let mut x = x;
    let mut hx = (x.to_bits() >> 32) as u32 as i32;
    let lx = x.to_bits() as u32;
    let mut k: i32 = 0;
    if hx < 0x0010_0000 {
        if ((hx & 0x7fff_ffff) as u32 | lx) == 0 {
            return f64::NEG_INFINITY;
        }
        if hx < 0 {
            return f64::NAN;
        }
        k -= 54;
        x *= TWO54;
        hx = (x.to_bits() >> 32) as u32 as i32;
    }
    if hx >= 0x7ff0_0000 {
        return x + x;
    }
    k += (hx >> 20) - 1023;
    hx &= 0x000f_ffff;
    let i = (hx + 0x95f64) & 0x10_0000;
    // x or x/2, whichever lies in [sqrt(2)/2, sqrt(2)]
    x = f64::from_bits(
        (((hx | (i ^ 0x3ff0_0000)) as u32 as u64) << 32) | (x.to_bits() & 0xffff_ffff),
    );
    k += i >> 20;
    let f = x - 1.0;
    if (0x000f_ffff & (2 + hx)) < 3 {
        if f == 0.0 {
            if k == 0 {
                return 0.0;
            }
            let dk = k as f64;
            return dk * LN2_HI + dk * LN2_LO;
        }
        let r = f * f * (0.5 - 0.33333333333333333 * f);
        if k == 0 {
            return f - r;
        }
        let dk = k as f64;
        return dk * LN2_HI - ((r - dk * LN2_LO) - f);
    }
    let s = f / (2.0 + f);
    let dk = k as f64;
    let z = s * s;
    let mut i = hx - 0x6147a;
    let w = z * z;
    let j = 0x6b851 - hx;
    let t1 = w * (LG2 + w * (LG4 + w * LG6));
    let t2 = z * (LG1 + w * (LG3 + w * (LG5 + w * LG7)));
    i |= j;
    let r = t2 + t1;
    if i > 0 {
        let hfsq = 0.5 * f * f;
        if k == 0 {
            return f - (hfsq - s * (hfsq + r));
        }
        return dk * LN2_HI - ((hfsq - (s * (hfsq + r) + dk * LN2_LO)) - f);
    }
    if k == 0 {
        return f - s * (f - r);
    }
    dk * LN2_HI - ((s * (f - r) - dk * LN2_LO) - f)
}

/// The library's weighted mean of two points, kept between them.
fn weighted_average(x1: f64, w1: f64, x2: f64, w2: f64) -> f64 {
    let sorted = |x1: f64, w1: f64, x2: f64, w2: f64| {
        let x = (x1 * w1 + x2 * w2) / (w1 + w2);
        x1.max(x.min(x2))
    };
    if x1 <= x2 { sorted(x1, w1, x2, w2) } else { sorted(x2, w2, x1, w1) }
}

impl AvlTreeDigest {
    pub(crate) fn new(compression: f64) -> AvlTreeDigest {
        AvlTreeDigest {
            compression,
            means: Vec::new(),
            counts: Vec::new(),
            count: 0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        }
    }

    pub(crate) fn size(&self) -> i64 {
        self.count
    }

    /// `AVLGroupTree.floor`: the last centroid whose mean is below `x`.
    fn floor(&self, x: f64) -> Option<usize> {
        let at = self.means.partition_point(|m| *m < x);
        (at > 0).then(|| at - 1)
    }

    /// Where the tree would put a centroid of this mean: after any that are
    /// equal to it, since it treats a new node as the greater of two equals.
    fn insertion_point(&self, mean: f64) -> usize {
        self.means.partition_point(|m| *m <= mean)
    }

    fn insert(&mut self, mean: f64, count: i64) {
        let at = self.insertion_point(mean);
        self.means.insert(at, mean);
        self.counts.insert(at, count);
    }

    /// The weight of every centroid before this one.
    fn head_sum(&self, at: usize) -> i64 {
        self.counts[..at].iter().sum()
    }

    /// `add(double x, int w)`: the value folded into the nearest centroid that
    /// has room for it, or kept as a centroid of its own.
    pub(crate) fn add(&mut self, x: f64, w: i64) {
        if x.is_nan() {
            return;
        }
        if x < self.min {
            self.min = x;
        }
        if x > self.max {
            self.max = x;
        }
        if self.means.is_empty() {
            self.insert(x, w);
            self.count = w;
            return;
        }
        let mut start = self.floor(x).unwrap_or(0);
        let mut min_distance = f64::MAX;
        let mut last_neighbor = self.means.len();
        let mut neighbor = start;
        while neighbor < self.means.len() {
            let z = (self.means[neighbor] - x).abs();
            if z < min_distance {
                start = neighbor;
                min_distance = z;
            } else if z > min_distance {
                // as soon as the distance grows, the nearest is behind us
                last_neighbor = neighbor;
                break;
            }
            neighbor += 1;
        }
        // Among the centroids exactly as near as each other, the library takes
        // one at random; the first is taken here. They are only ever more than
        // one when a value falls midway between two centroids or sits on
        // several with the same mean, and the reference's own answer is not
        // settled in that case.
        let mut closest = None;
        for neighbor in start..last_neighbor {
            let q0 = self.head_sum(neighbor) as f64 / self.count as f64;
            let q1 = q0 + self.counts[neighbor] as f64 / self.count as f64;
            let n = self.count as f64;
            let k = n * scale_max(q0, self.compression, n).min(scale_max(q1, self.compression, n));
            if (self.counts[neighbor] + w) as f64 <= k {
                closest = Some(neighbor);
                break;
            }
        }
        match closest {
            None => self.insert(x, w),
            Some(at) => {
                let mean = weighted_average(self.means[at], self.counts[at] as f64, x, w as f64);
                let count = self.counts[at] + w;
                if mean == self.means[at] {
                    // the tree updates a centroid in place when its mean does
                    // not move, which keeps equal centroids from shuffling
                    self.counts[at] = count;
                } else {
                    self.means.remove(at);
                    self.counts.remove(at);
                    self.insert(mean, count);
                }
            }
        }
        self.count += w;
        if self.means.len() as f64 > 20.0 * self.compression {
            // may happen when the values arrive in order
            self.compress();
        }
    }

    /// `add(TDigest other)`: the other's centroids added one at a time, in
    /// order, which is how a shard's sketch reaches the merged one.
    pub(crate) fn add_digest(&mut self, other: &AvlTreeDigest) {
        for (mean, count) in other.centroids() {
            self.add(mean, count);
        }
    }

    pub(crate) fn compress(&mut self) {
        if self.means.len() <= 1 {
            return;
        }
        let total = self.count as f64;
        let limit = |n: f64| total * scale_max(n / total, self.compression, total);
        let mut n0 = 0.0;
        let mut k0 = limit(n0);
        let mut node = 0usize;
        let mut w0 = self.counts[node];
        let mut n1 = n0 + w0 as f64;
        let mut w1 = 0i64;
        loop {
            let after = node + 1;
            while after < self.means.len() {
                w1 = self.counts[after];
                let k1 = limit(n1 + w1 as f64);
                if (w0 + w1) as f64 > k0.min(k1) {
                    break;
                }
                let mean =
                    weighted_average(self.means[node], w0 as f64, self.means[after], w1 as f64);
                self.means[node] = mean;
                self.counts[node] = w0 + w1;
                self.means.remove(after);
                self.counts.remove(after);
                n1 += w1 as f64;
                w0 += w1;
            }
            if after >= self.means.len() {
                break;
            }
            node = after;
            n0 = n1;
            k0 = limit(n0);
            w0 = w1;
            n1 = n0 + w0 as f64;
        }
    }

    pub(crate) fn centroids(&self) -> Vec<(f64, i64)> {
        self.means.iter().copied().zip(self.counts.iter().copied()).collect()
    }

    pub(crate) fn quantile(&self, q: f64) -> f64 {
        let size = self.means.len();
        if size == 0 {
            return f64::NAN;
        }
        if size == 1 {
            return self.means[0];
        }
        let count = self.count as f64;
        // where the value would sit if the samples were a sorted array
        let index = q * count;
        if index < 1.0 {
            return self.min;
        }
        if index >= count - 1.0 {
            return self.max;
        }
        let mut current = 0usize;
        let mut current_weight = self.counts[current];
        if current_weight == 2 && index <= 2.0 {
            // the first centroid holds two samples, one of them the minimum,
            // so the other one's place is known
            return 2.0 * self.means[current] - self.min;
        }
        if self.counts[size - 1] == 2 && index > count - 2.0 {
            return 2.0 * self.means[size - 1] - self.max;
        }
        // the weight to the left of the current centroid's centre
        let mut so_far = current_weight as f64 / 2.0;
        if index < so_far {
            // between the minimum and the first centroid, with the sample that
            // stands at the minimum left out of the interpolation
            return weighted_average(self.min, so_far - index, self.means[current], index - 1.0);
        }
        for _ in 0..size - 1 {
            let next = current + 1;
            let next_weight = self.counts[next];
            let dw = (current_weight + next_weight) as f64 / 2.0;
            if index < so_far + dw {
                let mut left = 0.0;
                if current_weight == 1 {
                    if index < so_far + 0.5 {
                        return self.means[current];
                    }
                    left = 0.5;
                }
                let mut right = 0.0;
                if next_weight == 1 {
                    if index >= so_far + dw - 0.5 {
                        return self.means[next];
                    }
                    right = 0.5;
                }
                let w1 = index - so_far - left;
                let w2 = so_far + dw - index - right;
                return weighted_average(self.means[current], w2, self.means[next], w1);
            }
            so_far += dw;
            current = next;
            current_weight = next_weight;
        }
        // in the right half of the last centroid, interpolating to the maximum
        let w1 = index - so_far;
        let w2 = count - 1.0 - index;
        weighted_average(self.means[current], w2, self.max, w1)
    }

    /// The share of the data at or below `x`, which is what a
    /// `percentile_ranks` aggregation reports.
    pub(crate) fn cdf(&self, x: f64) -> f64 {
        let size = self.means.len();
        if size == 0 {
            return f64::NAN;
        }
        let n = self.count as f64;
        if size == 1 {
            if x < self.means[0] {
                return 0.0;
            } else if x > self.means[0] {
                return 1.0;
            }
            return 0.5;
        }
        if x < self.min {
            return 0.0;
        }
        if x == self.min {
            // one or more centroids stand at x; they count as one
            let mut dw = 0.0;
            for i in 0..size {
                if self.means[i] != x {
                    break;
                }
                dw += self.counts[i] as f64;
            }
            return dw / 2.0 / n;
        }
        if x > self.max {
            return 1.0;
        }
        if x == self.max {
            let mut dw = 0.0;
            let mut i = size;
            while i > 0 && self.means[i - 1] == x {
                dw += self.counts[i - 1] as f64;
                i -= 1;
            }
            return (n - dw / 2.0) / n;
        }
        let first_mean = self.means[0];
        if x < first_mean {
            return self.interpolate_tail(x, 0, first_mean, self.min);
        }
        let last_mean = self.means[size - 1];
        if x > last_mean {
            return 1.0 - self.interpolate_tail(x, size - 1, last_mean, self.max);
        }
        let mut a_mean = self.means[0];
        let mut a_weight = self.counts[0] as f64;
        if x == a_mean {
            return a_weight / 2.0 / n;
        }
        let mut b = 1usize;
        let mut b_mean = self.means[b];
        let mut b_weight = self.counts[b] as f64;
        let mut so_far = 0.0;
        while b_weight > 0.0 {
            if x == b_mean {
                so_far += a_weight;
                while b + 1 < size {
                    b += 1;
                    if x == self.means[b] {
                        b_weight += self.counts[b] as f64;
                    } else {
                        break;
                    }
                }
                return (so_far + b_weight / 2.0) / n;
            }
            if x < b_mean {
                // strictly between the two centroids
                if a_weight == 1.0 {
                    if b_weight == 1.0 {
                        // all of a is passed and none of b, nothing to spread
                        return (so_far + 1.0) / n;
                    }
                    let partial = (x - a_mean) / (b_mean - a_mean) * b_weight / 2.0;
                    return (so_far + 1.0 + partial) / n;
                } else if b_weight == 1.0 {
                    let partial = (x - a_mean) / (b_mean - a_mean) * a_weight / 2.0;
                    return (so_far + a_weight / 2.0 + partial) / n;
                }
                let partial = (x - a_mean) / (b_mean - a_mean) * (a_weight + b_weight) / 2.0;
                return (so_far + a_weight / 2.0 + partial) / n;
            }
            so_far += a_weight;
            if b + 1 < size {
                a_mean = b_mean;
                a_weight = b_weight;
                b += 1;
                b_mean = self.means[b];
                b_weight = self.counts[b] as f64;
            } else {
                b_weight = 0.0;
            }
        }
        f64::NAN
    }

    fn interpolate_tail(&self, x: f64, at: usize, mean: f64, extreme: f64) -> f64 {
        let count = self.counts[at] as f64;
        let n = self.count as f64;
        if count == 2.0 {
            // the other sample must be on the other side of the mean
            return 1.0 / n;
        }
        // the weight there is to spread, and how much of it is below x
        let weight = count / 2.0 - 1.0;
        let partial = (extreme - x) / (extreme - mean) * weight;
        (partial + 1.0) / n
    }

    /// `computeMedianAbsoluteDeviation`: the median of how far each centroid
    /// lies from the median, weighted by the values it stands for.
    pub(crate) fn median_absolute_deviation(&self) -> Option<f64> {
        if self.count == 0 {
            return None;
        }
        let median = self.quantile(0.5);
        let mut deviations = AvlTreeDigest::new(self.compression);
        for (mean, count) in self.centroids() {
            deviations.add((median - mean).abs(), count);
        }
        Some(deviations.quantile(0.5))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handful_of_values_are_kept_whole() {
        let mut d = AvlTreeDigest::new(100.0);
        for v in [1.0, 2.0, 3.0] {
            d.add(v, 1);
        }
        assert_eq!(d.quantile(0.5), 2.0);
        assert_eq!(d.quantile(0.0), 1.0);
        assert_eq!(d.quantile(1.0), 3.0);
        assert_eq!(d.centroids(), vec![(1.0, 1), (2.0, 1), (3.0, 1)]);
    }

    #[test]
    fn the_logarithm_is_javas() {
        // read back from Java 21's Math.log, where the platform's differs
        for (x, bits) in [
            (255.0f64 / 100.0, 4606606798952363446i64),
            (276.0 / 100.0, 4607251011683569796),
            (297.0 / 100.0, 4607581266377712482),
        ] {
            assert_eq!(java_log(x).to_bits() as i64, bits, "log({x})");
        }
    }

    /// The sketch the reference builds over the aggregation corpus, whose
    /// centroids and percentiles were read back from t-digest 3.3 itself.
    #[test]
    fn the_corpus_sketch_is_the_librarys() {
        let xs = [
            0.0, 5.286, 10.571, 1.429, 6.714, 12.0, 2.857, 8.143, 13.429, 4.286, 9.571, 0.429,
            5.714, 11.0, 1.857, 7.143, 12.429, 3.286, 8.571, 13.857, 4.714, 10.0, 0.857, 6.143,
            11.429, 2.286, 7.571, 12.857, 3.714, 9.0, 14.286, 5.143, 10.429, 1.286, 6.571, 11.857,
            2.714, 8.0, 13.286, 4.143, 9.429, 0.286, 5.571, 10.857, 1.714, 7.0, 12.286, 3.143,
            8.429, 13.714, 4.571, 9.857, 0.714, 6.0, 11.286, 2.143, 7.429, 12.714, 3.571, 8.857,
        ];
        let mut d = AvlTreeDigest::new(100.0);
        for x in xs {
            d.add(x, 1);
        }
        assert_eq!(d.quantile(0.01), 0.0);
        assert_eq!(d.quantile(0.25), 3.286);
        assert_eq!(d.quantile(0.5), 7.0715);
        assert_eq!(d.quantile(0.75), 10.857);
        assert_eq!(d.quantile(0.99), 14.286);
        assert_eq!(d.cdf(3.0) * 100.0, 21.666666666666668);
        assert_eq!(d.cdf(12.0) * 100.0, 84.16666666666667);
        // the first centroid to hold two values, where the sketch stops being
        // able to keep every value apart
        assert_eq!(d.centroids()[15], (3.6425, 2));
    }
}
