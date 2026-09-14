//! The t-digest OpenSearch estimates percentiles with, ported step for step.
//!
//! OpenSearch 3.x keeps a `MergingDigest` from t-digest 3.3 (its
//! `TDigestState`), and a percentile it reports is that sketch's estimate, not
//! the exact value at the rank. The estimate depends on everything the sketch
//! went through: the order the values arrived in, when its buffer filled, and
//! every merge and copy on the way from the shard to the answer. So the port
//! keeps the library's arithmetic as it is -- the same scale function, the
//! same buffer sizes, the same order of floating-point operations -- and
//! `percentiles` and `median_absolute_deviation` replay the same journey the
//! reference's sketches take.

/// `MergingDigest` with the `K_2` scale function and the weight limit on,
/// which are the library's defaults.
#[derive(Clone)]
pub(crate) struct MergingDigest {
    merge_count: u32,
    public_compression: f64,
    compression: f64,
    last_used_cell: usize,
    total_weight: f64,
    weight: Vec<f64>,
    mean: Vec<f64>,
    unmerged_weight: f64,
    temp_used: usize,
    temp_weight: Vec<f64>,
    temp_mean: Vec<f64>,
    order: Vec<usize>,
    min: f64,
    max: f64,
}

/// `K_2`: `Z = 4 log(n / compression) + 24`.
fn normalizer(compression: f64, n: f64) -> f64 {
    compression / (4.0 * java_log(n / compression) + 24.0)
}

/// The natural logarithm as Java's `Math.log` gives it, which is fdlibm's.
///
/// The platform's `ln` rounds differently in the last place for a few
/// arguments in every hundred, and the scale function's normaliser is taken
/// from it: one place out in the normaliser moves a centroid's edge, and a
/// percentile came out a hair from the reference's.
#[allow(clippy::excessive_precision)] // fdlibm's constants, as it writes them
fn java_log(x: f64) -> f64 {
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

fn scale_max(q: f64, normalizer: f64) -> f64 {
    q * (1.0 - q) / normalizer
}

/// The library's weighted mean of two points, kept between them.
fn weighted_average(x1: f64, w1: f64, x2: f64, w2: f64) -> f64 {
    let sorted = |x1: f64, w1: f64, x2: f64, w2: f64| {
        let x = (x1 * w1 + x2 * w2) / (w1 + w2);
        x1.max(x.min(x2))
    };
    if x1 <= x2 { sorted(x1, w1, x2, w2) } else { sorted(x2, w2, x1, w1) }
}

impl MergingDigest {
    pub(crate) fn new(compression: f64) -> MergingDigest {
        let mut compression = compression;
        if compression < 10.0 {
            compression = 10.0;
        }
        // the weight limit is on, which leaves room for a few more centroids
        let mut size_fudge = 10.0;
        if compression < 30.0 {
            size_fudge += 20.0;
        }
        let mut size = (2.0 * compression + size_fudge).max(-1.0) as i64;
        let mut buffer_size = 5 * size;
        if buffer_size <= 2 * size {
            buffer_size = 2 * size;
        }
        // two levels of compression: the buffer is merged at a finer grain
        let scale = ((buffer_size / size) - 1).max(1) as f64;
        let public_compression = compression;
        let internal = scale.sqrt() * public_compression;
        if (size as f64) < internal + size_fudge {
            size = (internal + size_fudge).ceil() as i64;
        }
        if buffer_size <= 2 * size {
            buffer_size = 2 * size;
        }
        MergingDigest {
            merge_count: 0,
            public_compression,
            compression: internal,
            last_used_cell: 0,
            total_weight: 0.0,
            weight: vec![0.0; size as usize],
            mean: vec![0.0; size as usize],
            unmerged_weight: 0.0,
            temp_used: 0,
            temp_weight: vec![0.0; buffer_size as usize],
            temp_mean: vec![0.0; buffer_size as usize],
            order: vec![0; buffer_size as usize],
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        }
    }

    /// `add(double)` and `add(double, int)`.
    pub(crate) fn add(&mut self, x: f64, w: u64) {
        if x.is_nan() {
            return;
        }
        if self.temp_used >= self.temp_weight.len() - self.last_used_cell - 1 {
            self.merge_new_values(false, self.compression);
        }
        let at = self.temp_used;
        self.temp_used += 1;
        self.temp_weight[at] = w as f64;
        self.temp_mean[at] = x;
        self.unmerged_weight += w as f64;
        if x < self.min {
            self.min = x;
        }
        if x > self.max {
            self.max = x;
        }
    }

    /// `add(TDigest)`: each centroid of the other added as a weighted point.
    pub(crate) fn add_digest(&mut self, other: &mut MergingDigest) {
        for (m, w) in other.centroids() {
            self.add(m, w);
        }
    }

    /// `add(List<TDigest>)`: the other's centroids merged in all at once.
    pub(crate) fn add_all(&mut self, other: &mut MergingDigest) {
        other.compress();
        let n = other.last_used_cell;
        if n == 0 {
            return;
        }
        let mut m = vec![0.0; n.max(n + self.last_used_cell)];
        let mut w = vec![0.0; n.max(n + self.last_used_cell)];
        m[..n].copy_from_slice(&other.mean[..n]);
        w[..n].copy_from_slice(&other.weight[..n]);
        let mut total = 0.0;
        for x in &w[..n] {
            total += x;
        }
        let mut order = vec![0usize; n + self.last_used_cell];
        self.merge(&mut m, &mut w, n, &mut order, total, false, self.compression);
    }

    fn merge_new_values(&mut self, force: bool, compression: f64) {
        if self.total_weight == 0.0 && self.unmerged_weight == 0.0 {
            return;
        }
        if force || self.unmerged_weight > 0.0 {
            let mut m = std::mem::take(&mut self.temp_mean);
            let mut w = std::mem::take(&mut self.temp_weight);
            let mut order = std::mem::take(&mut self.order);
            let backwards = self.merge_count % 2 == 1;
            self.merge(
                &mut m,
                &mut w,
                self.temp_used,
                &mut order,
                self.unmerged_weight,
                backwards,
                compression,
            );
            self.temp_mean = m;
            self.temp_weight = w;
            self.order = order;
            self.merge_count += 1;
            self.temp_used = 0;
            self.unmerged_weight = 0.0;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn merge(
        &mut self,
        incoming_mean: &mut [f64],
        incoming_weight: &mut [f64],
        incoming_count: usize,
        order: &mut [usize],
        unmerged_weight: f64,
        run_backwards: bool,
        compression: f64,
    ) {
        let last = self.last_used_cell;
        incoming_mean[incoming_count..incoming_count + last].copy_from_slice(&self.mean[..last]);
        incoming_weight[incoming_count..incoming_count + last]
            .copy_from_slice(&self.weight[..last]);
        let count = incoming_count + last;
        // a stable sort: equal means keep the order they came in
        for (i, o) in order.iter_mut().enumerate().take(count) {
            *o = i;
        }
        order[..count].sort_by(|a, b| {
            incoming_mean[*a].partial_cmp(&incoming_mean[*b]).unwrap_or(std::cmp::Ordering::Equal)
        });
        self.total_weight += unmerged_weight;
        if run_backwards {
            order[..count].reverse();
        }
        self.last_used_cell = 0;
        self.mean[0] = incoming_mean[order[0]];
        self.weight[0] = incoming_weight[order[0]];
        let mut so_far = 0.0f64;
        let norm = normalizer(compression, self.total_weight);
        for (i, &ix) in order.iter().enumerate().take(count).skip(1) {
            let at = self.last_used_cell;
            let proposed = self.weight[at] + incoming_weight[ix];
            let q0 = so_far / self.total_weight;
            let q2 = (so_far + proposed) / self.total_weight;
            let mut add_this =
                proposed <= self.total_weight * scale_max(q0, norm).min(scale_max(q2, norm));
            if i == 1 || i == count - 1 {
                // the first and last centroids stay single points
                add_this = false;
            }
            if add_this {
                self.weight[at] += incoming_weight[ix];
                self.mean[at] = self.mean[at]
                    + (incoming_mean[ix] - self.mean[at]) * incoming_weight[ix] / self.weight[at];
                incoming_weight[ix] = 0.0;
            } else {
                so_far += self.weight[at];
                self.last_used_cell += 1;
                let at = self.last_used_cell;
                self.mean[at] = incoming_mean[ix];
                self.weight[at] = incoming_weight[ix];
                incoming_weight[ix] = 0.0;
            }
        }
        self.last_used_cell += 1;
        let n = self.last_used_cell;
        if run_backwards {
            self.mean[..n].reverse();
            self.weight[..n].reverse();
        }
        if self.total_weight > 0.0 {
            self.min = self.min.min(self.mean[0]);
            self.max = self.max.max(self.mean[n - 1]);
        }
    }

    pub(crate) fn compress(&mut self) {
        self.merge_new_values(true, self.public_compression);
    }

    pub(crate) fn size(&self) -> f64 {
        self.total_weight + self.unmerged_weight
    }

    /// `centroids()`, which compresses first.
    ///
    /// Each comes out as a `Centroid` object, whose constructor adds the mean
    /// to an empty centroid of the same weight: `0 + w * (mean - 0) / w`,
    /// which is not always the mean it was given, by one place in the last
    /// digit. Reading the mean straight from the array put a percentile over
    /// several shards a hair from the reference's.
    pub(crate) fn centroids(&mut self) -> Vec<(f64, u64)> {
        self.compress();
        (0..self.last_used_cell)
            .map(|i| {
                let count = self.weight[i] as i32;
                let mean = 0.0 + (count as f64 * (self.mean[i] - 0.0)) / count as f64;
                (mean, count as u64)
            })
            .collect()
    }

    pub(crate) fn quantile(&mut self, q: f64) -> f64 {
        self.merge_new_values(false, self.compression);
        let n = self.last_used_cell;
        if n == 0 {
            return f64::NAN;
        }
        if n == 1 {
            return self.mean[0];
        }
        let (mean, weight, total) = (&self.mean, &self.weight, self.total_weight);
        let index = q * total;
        if index < 1.0 {
            return self.min;
        }
        if weight[0] > 1.0 && index < weight[0] / 2.0 {
            return self.min + (index - 1.0) / (weight[0] / 2.0 - 1.0) * (mean[0] - self.min);
        }
        if index > total - 1.0 {
            return self.max;
        }
        if weight[n - 1] > 1.0 && total - index <= weight[n - 1] / 2.0 {
            return self.max
                - (total - index - 1.0) / (weight[n - 1] / 2.0 - 1.0) * (self.max - mean[n - 1]);
        }
        let mut so_far = weight[0] / 2.0;
        for i in 0..n - 1 {
            let dw = (weight[i] + weight[i + 1]) / 2.0;
            if so_far + dw > index {
                let mut left = 0.0;
                if weight[i] == 1.0 {
                    if index - so_far < 0.5 {
                        return mean[i];
                    }
                    left = 0.5;
                }
                let mut right = 0.0;
                if weight[i + 1] == 1.0 {
                    if so_far + dw - index <= 0.5 {
                        return mean[i + 1];
                    }
                    right = 0.5;
                }
                let z1 = index - so_far - left;
                let z2 = so_far + dw - index - right;
                return weighted_average(mean[i], z2, mean[i + 1], z1);
            }
            so_far += dw;
        }
        let z1 = index - total - weight[n - 1] / 2.0;
        let z2 = weight[n - 1] / 2.0 - z1;
        weighted_average(mean[n - 1], z1, self.max, z2)
    }

    /// The sketch as it arrives on the other side of the wire: the library's
    /// bytes, read back into a new digest, which `TDigestState` then merges
    /// into one more.
    pub(crate) fn round_trip(&mut self) -> MergingDigest {
        // `TDigestState.write` asks the size and then the bytes, and each
        // compresses the sketch again, every other time from the other end
        self.compress();
        self.compress();
        let mut read = MergingDigest::new(self.public_compression);
        read.min = self.min;
        read.max = self.max;
        let n = self.last_used_cell;
        read.last_used_cell = n;
        for i in 0..n {
            read.weight[i] = self.weight[i];
            read.mean[i] = self.mean[i];
            read.total_weight += self.weight[i];
        }
        let mut state = MergingDigest::new(self.public_compression);
        // reading back, it looks at the centroids to see whether there are
        // any, which compresses the copy once before it is merged in
        if !read.centroids().is_empty() {
            state.add_all(&mut read);
        }
        state
    }

    /// `computeMedianAbsoluteDeviation`: the median of how far each centroid
    /// lies from the median, one point per value the centroid stands for.
    pub(crate) fn median_absolute_deviation(&mut self) -> Option<f64> {
        if self.size() == 0.0 {
            return None;
        }
        let median = self.quantile(0.5);
        let mut deviations = MergingDigest::new(self.public_compression);
        for (m, w) in self.centroids() {
            let d = (median - m).abs();
            for _ in 0..w {
                deviations.add(d, 1);
            }
        }
        Some(deviations.quantile(0.5))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handful_of_values_are_kept_whole() {
        let mut d = MergingDigest::new(100.0);
        for v in [1.0, 2.0, 3.0] {
            d.add(v, 1);
        }
        assert_eq!(d.quantile(0.5), 2.0);
        assert_eq!(d.quantile(0.0), 1.0);
        assert_eq!(d.quantile(1.0), 3.0);
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

    #[test]
    fn the_buffer_sizes_are_the_librarys() {
        let d = MergingDigest::new(100.0);
        assert_eq!(d.weight.len(), 210);
        assert_eq!(d.temp_weight.len(), 1050);
        assert_eq!(d.compression, 200.0);
    }
}
