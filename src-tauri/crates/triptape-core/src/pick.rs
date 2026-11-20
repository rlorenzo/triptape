//! Best-of pick: the top N photos of a trip, without near-repeats.
//!
//! Photos are taken best score first. A photo that repeats a scene already
//! picked (shot within [`SAME_SCENE_SECS`] and looking alike, or within
//! [`BURST_SECS`]) waits, unless it shows different people: the same view
//! with someone else in it is a different keepsake. Waiting photos only fill
//! the slots left once every distinct scene is in.

use crate::hash::phash_distance;
use crate::types::{Asset, AssetKind};
use std::collections::HashSet;

/// Two alike-looking photos this close in time are one scene.
pub const SAME_SCENE_SECS: i64 = 120;
/// Photos this close in time are one burst, whatever they look like.
pub const BURST_SECS: i64 = 5;
/// Perceptual-hash distance at or under which two photos look alike.
pub const SAME_SCENE_PHASH_DISTANCE: u32 = 14;

/// What the pick needs to know about one photo.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candidate {
    /// Capture time (unix seconds); unknown never matches a scene.
    pub taken_at: Option<i64>,
    pub phash: Option<u64>,
    /// Who is tagged in the photo (`None`: nobody, or unknown).
    pub people_sig: Option<u64>,
    /// Higher is better.
    pub score: f32,
}

/// True when `a` and `b` repeat one scene with the same people in it.
pub fn repeats(a: &Candidate, b: &Candidate) -> bool {
    if a.people_sig != b.people_sig {
        return false;
    }
    let (Some(ta), Some(tb)) = (a.taken_at, b.taken_at) else {
        return false;
    };
    let dt = (ta - tb).abs();
    if dt <= BURST_SECS {
        return true;
    }
    let (Some(p), Some(q)) = (a.phash, b.phash) else {
        return false;
    };
    dt <= SAME_SCENE_SECS && phash_distance(p, q) <= SAME_SCENE_PHASH_DISTANCE
}

/// Indices of the `count` best candidates, ascending (the input's order).
/// Distinct scenes come first, best score first (the earlier photo wins a
/// tie); repeats fill what is left, also best first.
pub fn pick_top(candidates: &[Candidate], count: usize) -> Vec<usize> {
    let mut ranked: Vec<usize> = (0..candidates.len()).collect();
    ranked.sort_by(|&a, &b| {
        candidates[b]
            .score
            .total_cmp(&candidates[a].score)
            .then(a.cmp(&b))
    });
    let mut picked: Vec<usize> = vec![];
    let mut waiting: Vec<usize> = vec![];
    for i in ranked {
        if picked.len() == count {
            break;
        }
        if picked
            .iter()
            .any(|&p| repeats(&candidates[p], &candidates[i]))
        {
            waiting.push(i);
        } else {
            picked.push(i);
        }
    }
    let room = count.saturating_sub(picked.len());
    picked.extend(waiting.into_iter().take(room));
    picked.sort_unstable();
    picked
}

/// How much the photo library's own aesthetic score (0..1) adds.
pub const LIBRARY_SCORE_WEIGHT: f32 = 0.3;
/// Added for a photo the user captioned: curated photos rank above every
/// uncaptioned one (scores otherwise stay under 2), so they are picked
/// first, still without repeating a scene.
pub const CURATED_BONUS: f32 = 2.0;
/// Smallest photo worth keeping: under a megapixel is a thumbnail or a
/// screenshot crop, not a keepsake.
pub const MIN_PIXELS: u64 = 1_000_000;

/// Best-of score for one photo: its technical quality and story bonuses
/// (people, your caption), plus the photo library's aesthetic score when
/// the export carried one. Unscored photos sit at the neutral 0.5. A photo
/// the user captioned gets [`CURATED_BONUS`] on top.
pub fn photo_score(a: &Asset) -> f32 {
    let own = a
        .score
        .as_ref()
        .map_or(0.5, |s| (s.tech_quality() + s.bonus).clamp(0.0, 1.5));
    let library = a.library_score.map_or(0.0, |l| l.clamp(0.0, 1.0));
    let curated = if a.user_caption.is_some() {
        CURATED_BONUS
    } else {
        0.0
    };
    own + LIBRARY_SCORE_WEIGHT * library + curated
}

/// The `count` best photos of `assets`, in the order given (story order
/// when the assets come from the database). Videos, photos in `excluded`
/// (removed in the Story editor), photos under [`MIN_PIXELS`] and
/// byte-identical copies of a photo already seen are left out; near-repeats
/// follow [`pick_top`].
pub fn pick_photos<'a>(
    assets: &'a [Asset],
    excluded: &HashSet<&str>,
    count: usize,
) -> Vec<&'a Asset> {
    let mut seen: HashSet<&str> = HashSet::new();
    let pool: Vec<&Asset> = assets
        .iter()
        .filter(|a| a.kind != AssetKind::Video && !excluded.contains(a.id.as_str()))
        .filter(|a| match (a.width, a.height) {
            (Some(w), Some(h)) => u64::from(w) * u64::from(h) >= MIN_PIXELS,
            _ => true,
        })
        .filter(|a| a.content_hash.as_deref().is_none_or(|h| seen.insert(h)))
        .collect();
    let candidates: Vec<Candidate> = pool
        .iter()
        .map(|a| Candidate {
            taken_at: a.taken_at,
            phash: a.phash,
            people_sig: a.people_sig,
            score: photo_score(a),
        })
        .collect();
    pick_top(&candidates, count)
        .into_iter()
        .map(|i| pool[i])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn photo(id: &str, taken_at: i64, hash: &str) -> Asset {
        let mut a = Asset::new(id, format!("/trip/{id}.heic").into(), AssetKind::Photo);
        a.taken_at = Some(taken_at);
        a.content_hash = Some(hash.to_string());
        (a.width, a.height) = (Some(4032), Some(3024));
        a
    }

    #[test]
    fn pick_photos_drops_copies_videos_small_and_removed_photos() {
        let mut clip = photo("clip", 5000, "h-clip");
        clip.kind = AssetKind::Video;
        let mut tiny = photo("tiny", 6000, "h-tiny");
        (tiny.width, tiny.height) = (Some(640), Some(480));
        let mut liked = photo("liked", 7000, "h-liked");
        liked.library_score = Some(0.9);
        let assets = [
            photo("a", 1000, "h-a"),
            photo("a-copy", 1000, "h-a"),
            clip,
            tiny,
            liked,
            photo("cut", 8000, "h-cut"),
            photo("z", 9000, "h-z"),
        ];
        let excluded = HashSet::from(["cut"]);
        let ids = |n| -> Vec<&str> {
            pick_photos(&assets, &excluded, n)
                .iter()
                .map(|a| a.id.as_str())
                .collect()
        };
        assert_eq!(ids(10), ["a", "liked", "z"]);
        // The library's favourite outranks equal-scored neighbours.
        assert_eq!(ids(1), ["liked"]);
    }

    #[test]
    fn a_captioned_photo_is_picked_before_any_uncaptioned_one() {
        let mut best = photo("best", 1000, "h-best");
        best.library_score = Some(1.0);
        let mut mine = photo("mine", 5000, "h-mine");
        mine.user_caption = Some("Lanterns at dusk".to_string());
        let assets = [best, mine];
        let picked = pick_photos(&assets, &HashSet::new(), 1);
        assert_eq!(picked[0].id, "mine");
    }

    fn c(taken_at: i64, phash: u64, people_sig: Option<u64>, score: f32) -> Candidate {
        Candidate {
            taken_at: Some(taken_at),
            phash: Some(phash),
            people_sig,
            score,
        }
    }

    #[test]
    fn a_repeat_loses_its_slot_to_a_distinct_scene() {
        // 0 and 1: same view seconds apart. 2: another scene, lower score.
        let all = [
            c(1000, 0b1111, None, 0.9),
            c(1030, 0b1110, None, 0.8),
            c(9000, u64::MAX, None, 0.3),
        ];
        assert_eq!(pick_top(&all, 2), [0, 2]);
        // With room for everything the repeat comes back.
        assert_eq!(pick_top(&all, 3), [0, 1, 2]);
        assert_eq!(pick_top(&all, 10), [0, 1, 2]);
        assert!(pick_top(&all, 0).is_empty());
    }

    #[test]
    fn the_same_view_with_different_people_is_kept() {
        let all = [
            c(1000, 0b1111, Some(1), 0.9),
            c(1030, 0b1110, Some(2), 0.8),
            c(1060, 0b1111, Some(1), 0.7),
            c(9000, u64::MAX, None, 0.3),
        ];
        // 2 repeats 0 (same people); 1 has someone else in it.
        assert_eq!(pick_top(&all, 3), [0, 1, 3]);
        // Tagged and untagged versions of a view differ too.
        assert!(!repeats(&all[0], &c(1001, 0b1111, None, 0.5)));
    }

    #[test]
    fn scenes_need_both_time_and_looks_but_bursts_only_time() {
        let base = c(1000, 0, None, 0.5);
        // Alike but ten minutes later: a return visit, not a repeat.
        assert!(!repeats(&base, &c(1600, 0, None, 0.5)));
        // A minute later and nothing alike: a different subject.
        assert!(!repeats(&base, &c(1060, u64::MAX, None, 0.5)));
        // Three seconds later: one burst even when the hash moved.
        assert!(repeats(&base, &c(1003, u64::MAX, None, 0.5)));
        // Unknown capture time never matches.
        let undated = Candidate {
            taken_at: None,
            ..base
        };
        assert!(!repeats(&base, &undated));
    }

    #[test]
    fn ties_keep_the_earlier_photo() {
        let all = [c(1000, 0, None, 0.5), c(1010, 0, None, 0.5)];
        assert_eq!(pick_top(&all, 1), [0]);
    }
}
