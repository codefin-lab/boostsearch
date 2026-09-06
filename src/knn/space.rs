//! What "close" means.
//!
//! Every space here answers two questions: how far apart are these two
//! vectors, and what score does that distance deserve. OpenSearch reports a
//! score rather than a distance, and the two run opposite ways -- nearer is a
//! higher score -- so each space says how its distance is turned into one.

/// The ways two vectors can be compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Space {
    /// straight-line distance, squared: the usual one
    L2,
    /// the sum of the differences, which cares about every dimension equally
    L1,
    /// the largest single difference
    Linf,
    /// the angle between them, which ignores how long they are
    Cosine,
    /// how much they point the same way, length included
    InnerProduct,
    /// how many positions differ, for vectors of bits
    Hamming,
}

impl Space {
    pub fn named(name: &str) -> Space {
        match name {
            "l1" => Space::L1,
            "linf" => Space::Linf,
            "cosinesimil" | "cosine" => Space::Cosine,
            "innerproduct" | "inner_product" => Space::InnerProduct,
            "hamming" | "hammingbit" => Space::Hamming,
            _ => Space::L2,
        }
    }

    /// How far apart two vectors are, in this space.
    ///
    /// Vectors of different lengths are infinitely far apart rather than an
    /// error: a query is checked against the mapping before it runs, and a
    /// document written before the dimension changed should not stop a search.
    pub fn distance(&self, a: &[f32], b: &[f32]) -> f32 {
        if a.len() != b.len() {
            return f32::INFINITY;
        }
        match self {
            Space::L2 => a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum(),
            Space::L1 => a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum(),
            Space::Linf => a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0.0, f32::max),
            Space::Hamming => a.iter().zip(b).filter(|(x, y)| x != y).count() as f32,
            Space::Cosine => {
                let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
                let length = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
                let (la, lb) = (length(a), length(b));
                if la == 0.0 || lb == 0.0 {
                    return f32::INFINITY;
                }
                // the distance is how far from pointing the same way they are
                1.0 - dot / (la * lb)
            }
            Space::InnerProduct => -a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>(),
        }
    }

    /// The score a distance earns, the way OpenSearch reports it.
    ///
    /// Nearer is higher, and every space lands in a range a caller can compare
    /// against `min_score` without knowing which space was used.
    pub fn score(&self, distance: f32) -> f32 {
        match self {
            // every distance space scores the same way: nearer is higher, and
            // nothing is ever negative
            Space::L2 | Space::L1 | Space::Linf | Space::Hamming | Space::Cosine => {
                1.0 / (1.0 + distance)
            }
            // an inner product that is positive scores above one, and one
            // that is negative lands between zero and one -- which is what
            // keeps every score positive, as a score has to be
            Space::InnerProduct => {
                let product = -distance;
                if product >= 0.0 { product + 1.0 } else { 1.0 / (1.0 - product) }
            }
        }
    }

    /// The distance a score stands for, which is what a radial search asking
    /// for a `min_score` has to be turned into.
    pub fn distance_of(&self, score: f32) -> f32 {
        match self {
            Space::L2 | Space::L1 | Space::Linf | Space::Hamming | Space::Cosine => {
                (1.0 / score) - 1.0
            }
            Space::InnerProduct => {
                if score >= 1.0 {
                    -(score - 1.0)
                } else {
                    -(1.0 - 1.0 / score)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Space;

    #[test]
    fn a_vector_is_closest_to_itself() {
        let v = [1.0f32, 2.0, 3.0];
        for space in [Space::L2, Space::L1, Space::Linf, Space::Cosine, Space::Hamming] {
            // a cosine is a division, and a division of a number by itself in
            // single precision is not exactly one
            let found = space.distance(&v, &v);
            assert!(found.abs() < 1e-6, "{space:?} on itself: {found}");
        }
    }

    #[test]
    fn nearer_scores_higher() {
        let asked = [1.0f32, 0.0];
        let near = [0.9f32, 0.0];
        let far = [0.0f32, 1.0];
        for space in [Space::L2, Space::L1, Space::Linf, Space::Cosine] {
            let (a, b) = (
                space.score(space.distance(&asked, &near)),
                space.score(space.distance(&asked, &far)),
            );
            assert!(a > b, "{space:?}: near {a} should score above far {b}");
        }
    }

    #[test]
    fn a_score_says_which_distance_it_came_from() {
        for space in [Space::L2, Space::L1, Space::Linf, Space::Cosine, Space::InnerProduct] {
            for distance in [0.0f32, 0.5, 2.0, 10.0] {
                let there_and_back = space.distance_of(space.score(distance));
                assert!(
                    (there_and_back - distance).abs() < 0.001,
                    "{space:?}: {distance} became {there_and_back}"
                );
            }
        }
    }
}
