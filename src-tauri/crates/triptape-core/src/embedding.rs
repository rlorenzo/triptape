//! Image embeddings as a "same scene" signal for near-duplicate decisions.
//!
//! The vectors come from an on-device model (DINOv2-small, see
//! `triptape-ai-local`); core only stores and compares them. They are a
//! description of the picture's content, not a face or identity embedding,
//! and like every analysis column they never leave the device: they are not
//! serialized into plans, exports or logs.
//!
//! Storage: each vector is L2-normalised by the model wrapper, then quantised
//! to `i8` with a per-vector max-abs scale (the scale itself is dropped:
//! cosine similarity does not depend on it). DINOv2-small gives 384 values,
//! so one embedding is a 384-byte BLOB: about 1.1 MB for a 2,900-photo trip,
//! against 4.5 MB as `f32`. Quantisation moves cosine by well under 0.01
//! (see the tests), far below the gap between the thresholds and the
//! typical unrelated pair.
//!
//! Calibration (2,883 stills of a real trip, `docs/gates.md` G4 evidence):
//! random pairs median 0.12 (p95 0.36), same-minute pairs median 0.79, pHash
//! near-duplicates median 0.97 (p25 0.92). Contact sheets of pairs with a
//! far pHash showed DINOv2 cosine is mostly a "same place" signal: 0.6 to
//! 0.85 is the same landmark with other people or compositions (distinct
//! keepsakes), 0.90 to 0.95 mixes reframed retakes with different people in
//! front of the same view, and 0.95 and up were retakes.

/// Cosine at or above which two shots inside the short same-moment window
/// (`plan` / best-of pick, 2 minutes) show the same scene in another
/// framing. Soft use (ordering and "wait your turn", nothing is dropped), so
/// it admits the 0.90 to 0.95 band's reframings.
pub const SAME_SCENE_COSINE: f32 = 0.90;

/// Cosine at or above which two stills inside the near-duplicate window
/// (`cluster::collapse_near_dupes`, 10 minutes) are retakes. Collapsing
/// keeps only the best shot in the plan, so this stays where every pair
/// checked was a retake.
pub const NEAR_DUPE_COSINE: f32 = 0.95;

/// A quantised, direction-only image embedding (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Embedding(Vec<i8>);

impl Embedding {
    /// Quantise a model output. `None` for an empty, all-zero or non-finite
    /// vector: nothing to compare, so callers fall back to pHash.
    pub fn from_f32(v: &[f32]) -> Option<Self> {
        if v.is_empty() || v.iter().any(|x| !x.is_finite()) {
            return None;
        }
        let max = v.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        if max <= 0.0 {
            return None;
        }
        Some(Self(
            v.iter()
                .map(|x| (x / max * 127.0).round().clamp(-127.0, 127.0) as i8)
                .collect(),
        ))
    }

    /// Read a stored BLOB back. `None` for an empty blob.
    pub fn from_blob(bytes: &[u8]) -> Option<Self> {
        if bytes.is_empty() {
            return None;
        }
        Some(Self(bytes.iter().map(|&b| b as i8).collect()))
    }

    /// The bytes stored in the `assets.embedding` BLOB column.
    pub fn to_blob(&self) -> Vec<u8> {
        self.0.iter().map(|&x| x as u8).collect()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Cosine similarity (-1..1). `None` when the vectors come from
    /// different models (lengths differ) or one of them is all zeros.
    pub fn cosine(&self, other: &Self) -> Option<f32> {
        if self.0.len() != other.0.len() {
            return None;
        }
        let (mut dot, mut na, mut nb) = (0i64, 0i64, 0i64);
        for (&a, &b) in self.0.iter().zip(&other.0) {
            let (a, b) = (i64::from(a), i64::from(b));
            dot += a * b;
            na += a * a;
            nb += b * b;
        }
        if na == 0 || nb == 0 {
            return None;
        }
        Some((dot as f64 / ((na as f64).sqrt() * (nb as f64).sqrt())) as f32)
    }
}

/// True when both embeddings are present and at least `threshold` alike
/// ([`SAME_SCENE_COSINE`] or [`NEAR_DUPE_COSINE`]). Missing on either side
/// is never a match, so callers keep their pHash decision.
pub fn alike(a: Option<&Embedding>, b: Option<&Embedding>, threshold: f32) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.cosine(b).is_some_and(|c| c >= threshold),
        _ => false,
    }
}

/// [`alike`] at [`SAME_SCENE_COSINE`].
pub fn same_scene(a: Option<&Embedding>, b: Option<&Embedding>) -> bool {
    alike(a, b, SAME_SCENE_COSINE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(v: &[f32]) -> Vec<f32> {
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter().map(|x| x / n).collect()
    }

    fn cos_f32(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    /// Deterministic pseudo-random vector (xorshift), roughly zero-mean.
    fn pseudo(seed: u64, n: usize) -> Vec<f32> {
        let mut s = seed.max(1);
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s % 2001) as f32 / 1000.0 - 1.0
            })
            .collect()
    }

    #[test]
    fn cosine_of_identical_orthogonal_and_opposite() {
        let a = Embedding::from_f32(&[1.0, 0.0, 0.0]).unwrap();
        let b = Embedding::from_f32(&[0.0, 2.0, 0.0]).unwrap();
        let c = Embedding::from_f32(&[-3.0, 0.0, 0.0]).unwrap();
        assert!((a.cosine(&a).unwrap() - 1.0).abs() < 1e-6);
        assert!(a.cosine(&b).unwrap().abs() < 1e-6);
        assert!((a.cosine(&c).unwrap() + 1.0).abs() < 1e-6);
    }

    #[test]
    fn quantised_cosine_tracks_float_cosine() {
        for seed in 1..40u64 {
            let a = unit(&pseudo(seed, 384));
            let noise = pseudo(seed * 7919, 384);
            // Mix toward a second vector so pairs span the cosine range.
            let mix = (seed % 10) as f32 / 10.0;
            let b = unit(
                &a.iter()
                    .zip(&noise)
                    .map(|(x, n)| x * (1.0 - mix) + n * mix * 0.1)
                    .collect::<Vec<_>>(),
            );
            let want = cos_f32(&a, &b);
            let got = Embedding::from_f32(&a)
                .unwrap()
                .cosine(&Embedding::from_f32(&b).unwrap())
                .unwrap();
            assert!((got - want).abs() < 0.01, "seed {seed}: {got} vs {want}");
        }
    }

    #[test]
    fn degenerate_vectors_have_no_embedding() {
        assert_eq!(Embedding::from_f32(&[]), None);
        assert_eq!(Embedding::from_f32(&[0.0, 0.0]), None);
        assert_eq!(Embedding::from_f32(&[1.0, f32::NAN]), None);
        assert_eq!(Embedding::from_blob(&[]), None);
    }

    #[test]
    fn mismatched_lengths_never_compare() {
        let a = Embedding::from_f32(&[1.0, 0.5]).unwrap();
        let b = Embedding::from_f32(&[1.0, 0.5, 0.2]).unwrap();
        assert_eq!(a.cosine(&b), None);
        assert!(!same_scene(Some(&a), Some(&b)));
    }

    #[test]
    fn blob_round_trip_is_exact_and_compact() {
        let e = Embedding::from_f32(&unit(&pseudo(3, 384))).unwrap();
        let blob = e.to_blob();
        assert_eq!(blob.len(), 384, "one byte per dimension");
        assert_eq!(Embedding::from_blob(&blob), Some(e));
    }

    #[test]
    fn same_scene_needs_both_and_the_threshold() {
        let a = Embedding::from_f32(&[1.0, 0.0]).unwrap();
        // cos ~0.92: same scene, not yet a retake.
        let near = Embedding::from_f32(&[0.92, 0.39]).unwrap();
        // cos ~0.8: same place at best.
        let far = Embedding::from_f32(&[0.8, 0.6]).unwrap();
        // cos ~0.97: a retake.
        let retake = Embedding::from_f32(&[0.97, 0.24]).unwrap();
        assert!(same_scene(Some(&a), Some(&near)));
        assert!(!alike(Some(&a), Some(&near), NEAR_DUPE_COSINE));
        assert!(alike(Some(&a), Some(&retake), NEAR_DUPE_COSINE));
        assert!(!same_scene(Some(&a), Some(&far)));
        assert!(!same_scene(Some(&a), None));
        assert!(!same_scene(None, None));
    }
}
