//! Live Photo pairing (two-tier, never guess-merge real videos).
//!
//! Tier 1 (authoritative): shared content-identifier UUID in both the still
//! and MOV metadata, even if basenames differ or files sit in subfolders.
//! Tier 2 (fallback): same stem (case-insensitive) + MOV duration <= 3.5 s
//! with a video stream, in the same parent directory, and no conflicting
//! content identifiers. A same-stem MOV over ~5 s is a real video and is
//! never auto-merged. The 3.5–5 s band is left unmerged (ambiguous).

use crate::types::PairingConfidence;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Upper bound for the Tier-2 basename fallback (seconds).
pub const TIER2_MAX_SECS: f64 = 3.5;

/// A still candidate for pairing.
#[derive(Debug, Clone)]
pub struct StillFile {
    pub path: PathBuf,
    pub content_id: Option<String>,
}

/// A MOV candidate for pairing.
#[derive(Debug, Clone)]
pub struct MotionFile {
    pub path: PathBuf,
    pub content_id: Option<String>,
    pub duration_secs: Option<f64>,
    pub has_video_stream: bool,
}

/// A paired Live Photo.
#[derive(Debug, Clone)]
pub struct LivePhotoPair {
    pub still: PathBuf,
    pub motion: PathBuf,
    pub confidence: PairingConfidence,
}

/// Outcome of pairing: merged pairs plus leftover stills and motions.
#[derive(Debug, Default)]
pub struct PairingResult {
    pub pairs: Vec<LivePhotoPair>,
    pub unpaired_stills: Vec<PathBuf>,
    pub unpaired_motions: Vec<PathBuf>,
}

fn stem_lower(path: &Path) -> Option<String> {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
}

/// Content-identifier, treating an empty string as absent.
fn content_id(id: &Option<String>) -> Option<&str> {
    id.as_deref().filter(|s| !s.is_empty())
}

/// Pair stills with motions. Pure function over pre-extracted metadata so it
/// is unit-testable without touching disk or ffprobe.
pub fn pair_live_photos(stills: Vec<StillFile>, motions: Vec<MotionFile>) -> PairingResult {
    let mut result = PairingResult::default();
    // Index motions by content-id for Tier 1.
    let mut by_id: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, m) in motions.iter().enumerate() {
        if let Some(id) = content_id(&m.content_id) {
            by_id.entry(id).or_default().push(i);
        }
    }
    let mut used_motion = vec![false; motions.len()];
    let mut used_still = vec![false; stills.len()];

    // Tier 1: content-identifier match.
    for (si, still) in stills.iter().enumerate() {
        if let Some(cands) = content_id(&still.content_id).and_then(|id| by_id.get(id)) {
            if let Some(&mi) = cands.iter().find(|&&mi| !used_motion[mi]) {
                used_motion[mi] = true;
                used_still[si] = true;
                result.pairs.push(LivePhotoPair {
                    still: still.path.clone(),
                    motion: motions[mi].path.clone(),
                    confidence: PairingConfidence::ContentId,
                });
            }
        }
    }

    // Tier 2: same directory + same stem + short MOV.
    let mut motions_by_stem: HashMap<(Option<&Path>, String), Vec<usize>> = HashMap::new();
    for (i, m) in motions.iter().enumerate() {
        if used_motion[i] {
            continue;
        }
        if let Some(stem) = stem_lower(&m.path) {
            motions_by_stem
                .entry((m.path.parent(), stem))
                .or_default()
                .push(i);
        }
    }
    for (si, still) in stills.iter().enumerate() {
        if used_still[si] {
            continue;
        }
        let Some(stem) = stem_lower(&still.path) else {
            continue;
        };
        let Some(cands) = motions_by_stem.get(&(still.path.parent(), stem)) else {
            continue;
        };
        // Eligible: has video stream, known duration <= TIER2_MAX_SECS, and
        // no conflicting content-ids (both present and different).
        // Unknown duration or > NEVER_MERGE: never merge.
        let still_id = content_id(&still.content_id);
        let pick = cands.iter().find(|&&mi| {
            let conflict = matches!(
                (still_id, content_id(&motions[mi].content_id)),
                (Some(a), Some(b)) if a != b
            );
            !used_motion[mi]
                && !conflict
                && motions[mi].has_video_stream
                && matches!(motions[mi].duration_secs, Some(d) if d <= TIER2_MAX_SECS)
        });
        if let Some(&mi) = pick {
            used_motion[mi] = true;
            used_still[si] = true;
            result.pairs.push(LivePhotoPair {
                still: still.path.clone(),
                motion: motions[mi].path.clone(),
                confidence: PairingConfidence::BasenameOnly,
            });
        }
    }

    for (si, s) in stills.iter().enumerate() {
        if !used_still[si] {
            result.unpaired_stills.push(s.path.clone());
        }
    }
    for (mi, m) in motions.iter().enumerate() {
        if !used_motion[mi] {
            result.unpaired_motions.push(m.path.clone());
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn still(p: &str, id: Option<&str>) -> StillFile {
        StillFile {
            path: PathBuf::from(p),
            content_id: id.map(|s| s.to_string()),
        }
    }

    fn motion(p: &str, id: Option<&str>, dur: Option<f64>) -> MotionFile {
        MotionFile {
            path: PathBuf::from(p),
            content_id: id.map(|s| s.to_string()),
            duration_secs: dur,
            has_video_stream: true,
        }
    }

    #[test]
    fn tier1_matches_across_different_names() {
        let r = pair_live_photos(
            vec![still("a/IMG_100.HEIC", Some("uuid-1"))],
            vec![motion("b/VID_other.MOV", Some("uuid-1"), Some(3.0))],
        );
        assert_eq!(r.pairs.len(), 1);
        assert_eq!(r.pairs[0].confidence, PairingConfidence::ContentId);
    }

    #[test]
    fn tier2_matches_same_stem_short_mov() {
        let r = pair_live_photos(
            vec![still("IMG_101.HEIC", None)],
            vec![motion("img_101.MOV", None, Some(3.0))],
        );
        assert_eq!(r.pairs.len(), 1);
        assert_eq!(r.pairs[0].confidence, PairingConfidence::BasenameOnly);
    }

    #[test]
    fn long_mov_never_merges() {
        for dur in [Some(5.1), Some(30.0), Some(6.0), None] {
            let r = pair_live_photos(
                vec![still("IMG_102.HEIC", None)],
                vec![motion("IMG_102.MOV", None, dur)],
            );
            assert!(r.pairs.is_empty(), "duration {dur:?} must not merge");
            assert_eq!(r.unpaired_motions.len(), 1);
        }
    }

    #[test]
    fn ambiguous_band_stays_unmerged() {
        // 3.5–5 s: too long for Tier 2, too short to be sure — leave alone.
        let r = pair_live_photos(
            vec![still("IMG_103.HEIC", None)],
            vec![motion("IMG_103.MOV", None, Some(4.2))],
        );
        assert!(r.pairs.is_empty());
    }

    #[test]
    fn tier2_rejects_conflicting_ids() {
        let r = pair_live_photos(
            vec![still("IMG_104.HEIC", Some("uuid-X"))],
            vec![motion("IMG_104.MOV", Some("uuid-Y"), Some(3.0))],
        );
        assert!(r.pairs.is_empty());
        // One side missing an id is not a conflict.
        let r = pair_live_photos(
            vec![still("IMG_105.HEIC", Some("uuid-X"))],
            vec![motion("IMG_105.MOV", None, Some(3.0))],
        );
        assert_eq!(r.pairs.len(), 1);
    }

    #[test]
    fn tier2_requires_same_directory() {
        let r = pair_live_photos(
            vec![still("trip1/IMG_106.HEIC", None)],
            vec![motion("trip2/IMG_106.MOV", None, Some(3.0))],
        );
        assert!(r.pairs.is_empty());
        let r = pair_live_photos(
            vec![still("trip1/IMG_106.HEIC", None)],
            vec![motion("trip1/IMG_106.MOV", None, Some(3.0))],
        );
        assert_eq!(r.pairs.len(), 1);
    }

    #[test]
    fn tier1_wins_over_stem_collision() {
        let r = pair_live_photos(
            vec![
                still("A.HEIC", Some("uuid-A")),
                still("B.HEIC", Some("uuid-B")),
            ],
            vec![
                motion("A.MOV", Some("uuid-B"), Some(3.0)),
                motion("B.MOV", Some("uuid-A"), Some(3.0)),
            ],
        );
        assert_eq!(r.pairs.len(), 2);
        for p in &r.pairs {
            assert_eq!(p.confidence, PairingConfidence::ContentId);
        }
    }
}
