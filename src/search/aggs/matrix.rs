//! The moments of several fields at once, and how they move together.
//!
//! The arithmetic here is OpenSearch's arithmetic rather than the textbook's.
//! Each shard accumulates its own moments a document at a time, and the shard
//! results are merged pairwise; a single pass over all the values would be
//! steadier, but it is not the same sum, and a correlation the suites pin to
//! twelve digits shows the difference.

/// The moments one shard has seen, field by field.
#[derive(Clone)]
pub(crate) struct Running {
    pub docs: f64,
    counts: Vec<f64>,
    sums: Vec<f64>,
    means: Vec<f64>,
    variances: Vec<f64>,
    skewness: Vec<f64>,
    kurtosis: Vec<f64>,
    /// only the upper triangle is kept, the way OpenSearch keeps it
    covariances: Vec<Vec<f64>>,
}

impl Running {
    pub(crate) fn new(width: usize) -> Running {
        Running {
            docs: 0.0,
            counts: vec![0.0; width],
            sums: vec![0.0; width],
            means: vec![0.0; width],
            variances: vec![0.0; width],
            skewness: vec![0.0; width],
            kurtosis: vec![0.0; width],
            covariances: vec![vec![0.0; width]; width],
        }
    }

    fn width(&self) -> usize {
        self.means.len()
    }

    /// One more document, holding a value for every field.
    pub(crate) fn add(&mut self, row: &[f64]) {
        self.docs += 1.0;
        let n = self.docs;
        let width = self.width();
        let mut deltas = vec![0.0f64; width];
        for (at, delta) in deltas.iter_mut().enumerate() {
            let value = row[at];
            self.counts[at] += 1.0;
            self.sums[at] += value;
            *delta = value * n - self.sums[at];
            if n == 1.0 {
                self.means[at] = value;
                continue;
            }
            let d = value - self.means[at];
            let dn = d / n;
            self.means[at] += dn;
            let m2 = self.variances[at];
            let m3 = self.skewness[at];
            let t1 = d * dn * (n - 1.0);
            self.variances[at] += t1;
            self.skewness[at] += t1 * dn * (n - 2.0) - 3.0 * dn * m2;
            let dn2 = dn * dn;
            self.kurtosis[at] +=
                t1 * dn2 * (n * n - 3.0 * n + 3.0) + 6.0 * dn2 * m2 - 4.0 * dn * m3;
        }
        if n > 1.0 {
            for a in 0..width {
                for b in a + 1..width {
                    self.covariances[a][b] += deltas[a] * deltas[b] / (n * (n - 1.0));
                }
            }
        }
    }

    /// What another shard saw, folded into what this one saw.
    pub(crate) fn merge(&mut self, other: &Running) {
        if other.docs == 0.0 {
            return;
        }
        if self.docs == 0.0 {
            *self = other.clone();
            return;
        }
        let width = self.width();
        let n_a = self.docs;
        let n_b = other.docs;
        self.docs += other.docs;
        let total = self.docs;
        let mut deltas = vec![0.0f64; width];
        for (at, delta) in deltas.iter_mut().enumerate() {
            let mean_a = self.means[at];
            let var_a = self.variances[at];
            let skew_a = self.skewness[at];
            let kurt_a = self.kurtosis[at];
            let mean_b = other.means[at];
            let var_b = other.variances[at];
            let skew_b = other.skewness[at];
            let kurt_b = other.kurtosis[at];

            self.counts[at] += other.counts[at];
            self.means[at] = (n_a * mean_a + n_b * mean_b) / (n_a + n_b);
            *delta = other.sums[at] / n_b - self.sums[at] / n_a;
            self.sums[at] += other.sums[at];

            let d = mean_b - mean_a;
            let d2 = d * d;
            let d3 = d * d2;
            let d4 = d2 * d2;
            let n2 = total * total;
            let na2 = n_a * n_a;
            let nb2 = n_b * n_b;
            self.variances[at] = var_a + var_b + d2 * n_a * n_b / total;
            let skew = skew_a + skew_b + d3 * n_a * n_b * (n_a - n_b) / n2;
            self.skewness[at] = skew + 3.0 * d * (n_a * var_b - n_b * var_a) / total;
            let kurt = kurt_a + kurt_b + d4 * n_a * n_b * (na2 - n_a * n_b + nb2) / (n2 * total);
            self.kurtosis[at] = kurt
                + 6.0 * d2 * (na2 * var_b + nb2 * var_a) / n2
                + 4.0 * d * (n_a * skew_b - n_b * skew_a) / total;
        }
        let f = n_a * n_b / total;
        for a in 0..width {
            for b in a + 1..width {
                self.covariances[a][b] += other.covariances[a][b] + f * deltas[a] * deltas[b];
            }
        }
    }

    /// The numbers the caller is answered with, worked out from the moments.
    pub(crate) fn described(&self) -> Described {
        let width = self.width();
        let n = self.docs;
        let n_less_one = n - 1.0;
        let skewness: Vec<f64> = (0..width)
            .map(|at| n.sqrt() * self.skewness[at] / self.variances[at].powf(1.5))
            .collect();
        let kurtosis: Vec<f64> = (0..width)
            .map(|at| n * self.kurtosis[at] / (self.variances[at] * self.variances[at]))
            .collect();
        let variances: Vec<f64> = self.variances.iter().map(|v| v / n_less_one).collect();
        let mut covariances = vec![vec![0.0f64; width]; width];
        for (a, row) in covariances.iter_mut().enumerate() {
            for (b, cell) in row.iter_mut().enumerate() {
                *cell = match a.cmp(&b) {
                    std::cmp::Ordering::Equal => variances[a],
                    std::cmp::Ordering::Less => self.covariances[a][b] / n_less_one,
                    std::cmp::Ordering::Greater => self.covariances[b][a] / n_less_one,
                };
            }
        }
        Described {
            count: n as usize,
            means: self.means.clone(),
            variances,
            skewness,
            kurtosis,
            covariances,
        }
    }
}

/// The moments as they are reported.
pub(crate) struct Described {
    pub count: usize,
    pub means: Vec<f64>,
    pub variances: Vec<f64>,
    pub skewness: Vec<f64>,
    pub kurtosis: Vec<f64>,
    pub covariances: Vec<Vec<f64>>,
}
