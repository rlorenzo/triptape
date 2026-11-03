//! Stage 7 — Plan: one versioned AI Plan JSON for all three renderers.
//!
//! Hero shot list in story order, 3–5 reel selects varied by day/place,
//! chapters by day/place, template captions, map polyline + day markers,
//! music pick with rationale. Per-entry identity (asset id, content hash,
//! size, mtime, rendition) feeds the pre-render source-integrity check.
//! Sec 5.4 degradation eligibility lives here too.

use crate::cluster::story_order;
use crate::types::{Asset, AssetKind, Chapter, MusicPick, Plan, PlanEntry, Rendition, Score};
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub const PLAN_VERSION: u32 = 1;

/// Score assumed for assets missing from the score map.
const DEFAULT_SCORE: f32 = 0.5;
/// Hero lists longer than this drop entries below [`HERO_MIN_SCORE`].
const HERO_TRIM_ABOVE: usize = 40;
const HERO_MIN_SCORE: f32 = 0.35;
/// Reel pack size and quality bar (the first [`MIN_REELS`] picks skip the bar).
const MAX_REELS: usize = 5;
const MIN_REELS: usize = 3;
const REEL_MIN_SCORE: f32 = 0.3;
/// Chapter label for assets with no cluster.
const UNGROUPED: &str = "Ungrouped";

/// Build the plan from scored, clustered assets.
///
/// `cluster_of` maps asset id → cluster label; `scores` maps asset id →
/// score (only `total` is read). Deterministic: no wall-clock, no RNG.
pub fn build_plan(
    assets: &[Asset],
    cluster_of: &HashMap<String, String>,
    scores: &HashMap<String, Score>,
) -> Plan {
    let ordered = story_order(assets);
    let score_of = |id: &str| scores.get(id).map_or(DEFAULT_SCORE, |s| s.total);

    // Hero: story order, one entry per identical content hash (pHash
    // near-dupe stacks are resolved upstream via cluster picks).
    let mut seen_hash: HashSet<&str> = HashSet::new();
    let mut hero: Vec<PlanEntry> = ordered
        .iter()
        .filter(|a| {
            a.content_hash
                .as_deref()
                .is_none_or(|h| seen_hash.insert(h))
        })
        .map(|a| plan_entry(a))
        .collect();
    if hero.len() > HERO_TRIM_ABOVE {
        hero.retain(|e| score_of(&e.asset_id) >= HERO_MIN_SCORE);
    }

    // Group by cluster label in story order (drives chapters and reels).
    let mut groups: Vec<(&str, Vec<&Asset>)> = vec![];
    let mut group_idx: HashMap<&str, usize> = HashMap::new();
    for a in &ordered {
        let label = cluster_of.get(&a.id).map_or(UNGROUPED, String::as_str);
        let i = *group_idx.entry(label).or_insert_with(|| {
            groups.push((label, vec![]));
            groups.len() - 1
        });
        groups[i].1.push(a);
    }

    // Reel selects: round-robin over clusters (by label), best-scored first.
    let mut by_score: Vec<(&str, Vec<&Asset>)> = groups.clone();
    by_score.sort_by_key(|g| g.0);
    for (_, members) in &mut by_score {
        members.sort_by(|a, b| score_of(&b.id).total_cmp(&score_of(&a.id)));
    }
    let mut reel_selects: Vec<PlanEntry> = vec![];
    'rounds: for round in 0..MAX_REELS {
        for (_, members) in &by_score {
            if reel_selects.len() >= MAX_REELS {
                break 'rounds;
            }
            if let Some(a) = members.get(round) {
                if score_of(&a.id) >= REEL_MIN_SCORE || reel_selects.len() < MIN_REELS {
                    reel_selects.push(plan_entry(a));
                }
            }
        }
    }

    // Chapters: one per cluster label, in story order.
    let chapters: Vec<Chapter> = groups
        .into_iter()
        .map(|(label, members)| Chapter {
            title: label.to_string(),
            asset_ids: members.iter().map(|a| a.id.clone()).collect(),
        })
        .collect();

    // Template captions (local-VLM fallback): never empty, always editable.
    let captions: HashMap<String, String> = ordered
        .iter()
        .map(|a| {
            let when = a
                .taken_at
                .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                .map(|d| d.format("%B %-d").to_string())
                .unwrap_or_else(|| "the trip".to_string());
            let what = match a.kind {
                AssetKind::Video => "a favorite clip",
                AssetKind::LivePhoto => "a living moment",
                AssetKind::Photo => "a favorite photo",
            };
            (a.id.clone(), format!("{what} from {when}"))
        })
        .collect();

    // Map polyline: GPS fixes in story order.
    let map_polyline: Vec<(f64, f64)> = ordered.iter().filter_map(|a| a.gps).collect();

    Plan {
        plan_version: PLAN_VERSION,
        schema_version: Plan::SCHEMA_VERSION,
        hero,
        reel_selects,
        chapters,
        captions,
        map_polyline,
        music: MusicPick {
            track: "morning-light".to_string(),
            rationale: "Vibe fit: mid-tempo acoustic for mixed daylight outdoor moments"
                .to_string(),
        },
    }
}

fn plan_entry(a: &Asset) -> PlanEntry {
    let rendition = match a.kind {
        AssetKind::LivePhoto => {
            if a.companion_path.is_some() {
                // Motion-first for video; book renderers always use still.
                Rendition::Motion {
                    trim_start: 0.0,
                    trim_end: a.duration_secs.unwrap_or(3.0).clamp(0.5, 3.0),
                }
            } else {
                Rendition::Still
            }
        }
        AssetKind::Video => Rendition::Clip {
            trim_start: 0.0,
            trim_end: a.duration_secs.unwrap_or(5.0).clamp(1.0, 8.0),
        },
        AssetKind::Photo => Rendition::Still,
    };
    PlanEntry {
        asset_id: a.id.clone(),
        content_hash: a.content_hash.clone(),
        size_bytes: a.size_bytes,
        mtime: a.mtime,
        rendition,
    }
}

// -- Sec 5.4 degradation eligibility ---------------------------------------

/// Per-keepsake eligibility for a thin trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Eligibility {
    /// Hero film variant: "full" (3–5 min), "short" (≤30 s slideshow), or "disabled".
    pub video: &'static str,
    pub video_reason: String,
    /// Number of reels in the pack (0 disables the card).
    pub reels: usize,
    pub reels_reason: String,
    /// Book export pack enabled?
    pub book_enabled: bool,
    pub book_reason: String,
}

/// Decide keepsake eligibility from usable counts.
///
/// `usable` = assets surviving dedupe/scoring; `photos` = placeable stills;
/// `distinct_moments` = distinct day/place moments for reels.
pub fn plan_eligibility(usable: usize, photos: usize, distinct_moments: usize) -> Eligibility {
    let (video, video_reason) = if usable >= 40 {
        (
            "full",
            "Enough distinct moments for the full film".to_string(),
        )
    } else if usable >= 5 {
        (
            "short",
            "Small trip: a short (≤30 s) slideshow instead of the full film".to_string(),
        )
    } else {
        (
            "disabled",
            "Fewer than ~5 usable assets: add more photos or clips".to_string(),
        )
    };
    let reels = distinct_moments.min(5);
    let reels_reason = if reels == 0 {
        "Not enough distinct moments for a reel".to_string()
    } else {
        format!("{reels} reel(s) from distinct moments")
    };
    let (book_enabled, book_reason) = if photos >= 30 {
        (
            true,
            "Enough photos for the 24-page vendor minimum".to_string(),
        )
    } else {
        (
            false,
            "Add more photos for a printable book (24-page vendor minimum)".to_string(),
        )
    };
    Eligibility {
        video,
        video_reason,
        reels,
        reels_reason,
        book_enabled,
        book_reason,
    }
}

// -- Source integrity (Sec 8) -----------------------------------------------

/// Pre-render check outcome for one plan entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Integrity {
    /// Path + size + mtime all match; no hash read needed.
    Ok,
    /// Original is gone (or unreadable).
    Missing { reason: String },
    /// Same path, different bytes — offer "Use new version" or Skip.
    Changed { reason: String },
}

/// Fast path + size + mtime check; hash only on mismatch.
///
/// `actual_hash` is a closure so callers read bytes only when needed.
pub fn check_source_integrity(
    entry: &PlanEntry,
    path: &Path,
    actual_size: Option<u64>,
    actual_mtime: Option<i64>,
    actual_hash: impl FnOnce() -> Option<String>,
) -> Integrity {
    if !path.exists() || actual_size.is_none() {
        return Integrity::Missing {
            reason: format!(
                "{} is missing; offer Relink, Rescan, or Skip",
                entry.asset_id
            ),
        };
    }
    if actual_size == Some(entry.size_bytes) && actual_mtime == Some(entry.mtime) {
        return Integrity::Ok;
    }
    match (actual_hash(), entry.content_hash.as_deref()) {
        (Some(a), Some(b)) if a == b => Integrity::Ok,
        _ => Integrity::Changed {
            reason: format!(
                "{} changed on disk; offer Use new version or Skip",
                entry.asset_id
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AssetKind;
    use std::path::PathBuf;

    fn asset(id: &str, taken: i64, kind: AssetKind) -> Asset {
        let mut a = Asset::new(id, PathBuf::from(format!("/t/{id}.jpg")), kind);
        a.taken_at = Some(taken);
        a.size_bytes = 100;
        a.mtime = taken;
        a.content_hash = Some(format!("h{id}"));
        a
    }

    fn scored(ids: &[&str]) -> HashMap<String, Score> {
        ids.iter()
            .enumerate()
            .map(|(i, id)| {
                let s = Score {
                    total: 0.9 - i as f32 * 0.05,
                    highlight: true,
                    ..Default::default()
                };
                (id.to_string(), s)
            })
            .collect()
    }

    #[test]
    fn plan_shape_and_versions() {
        let assets = vec![
            asset("a", 1_714_557_600, AssetKind::Photo),
            asset("b", 1_714_557_900, AssetKind::Video),
            asset("c", 1_714_558_000, AssetKind::LivePhoto),
        ];
        let mut co = HashMap::new();
        co.insert("a".into(), "2024-05-01".into());
        co.insert("b".into(), "2024-05-01".into());
        co.insert("c".into(), "2024-05-02".into());
        let plan = build_plan(&assets, &co, &scored(&["a", "b", "c"]));
        assert_eq!(plan.plan_version, PLAN_VERSION);
        assert_eq!(plan.schema_version, Plan::SCHEMA_VERSION);
        assert_eq!(plan.hero.len(), 3);
        assert_eq!(plan.hero[0].asset_id, "a", "hero keeps story order");
        assert_eq!(plan.chapters.len(), 2);
        assert_eq!(plan.captions.len(), 3);
        assert_eq!(plan.reel_selects.len(), 3, "3 distinct moments -> 3 reels");
        assert!(plan.reel_selects.len() <= 5);
        assert!(!plan.music.rationale.is_empty());
        // Live Photo without motion file falls back to still.
        assert!(matches!(plan.hero[2].rendition, Rendition::Still));
        // Round-trips through JSON.
        let back = Plan::from_json(&plan.to_json().unwrap()).unwrap();
        assert_eq!(back.hero.len(), 3);
    }

    #[test]
    fn live_photo_motion_first() {
        let mut a = asset("lp", 1_714_557_600, AssetKind::LivePhoto);
        a.companion_path = Some("/t/lp.mov".into());
        a.duration_secs = Some(3.0);
        let plan = build_plan(&[a], &HashMap::new(), &HashMap::new());
        assert!(matches!(plan.hero[0].rendition, Rendition::Motion { .. }));
    }

    #[test]
    fn degradation_thresholds() {
        let full = plan_eligibility(50, 40, 6);
        assert_eq!(full.video, "full");
        assert_eq!(full.reels, 5, "capped at 5");
        assert!(full.book_enabled);
        let small = plan_eligibility(10, 10, 1);
        assert_eq!(small.video, "short");
        assert_eq!(small.reels, 1, "1–2 reels is a valid pack");
        assert!(!small.book_enabled);
        assert!(small.book_reason.contains("24-page"));
        let tiny = plan_eligibility(1, 1, 0);
        assert_eq!(tiny.video, "disabled");
        assert_eq!(tiny.reels, 0);
        assert!(tiny.reels_reason.contains("Not enough distinct moments"));
    }

    #[test]
    fn integrity_fast_path_and_changed() {
        let e = PlanEntry {
            asset_id: "a".into(),
            content_hash: Some("h1".into()),
            size_bytes: 100,
            mtime: 7,
            rendition: Rendition::Still,
        };
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.jpg");
        std::fs::write(&p, vec![0u8; 100]).unwrap();
        // Exact fast-path match: hash closure must not run.
        let r = check_source_integrity(&e, &p, Some(100), Some(7), || {
            panic!("hash must not be read on fast-path match")
        });
        assert_eq!(r, Integrity::Ok);
        // Size mismatch + same bytes.
        let r = check_source_integrity(&e, &p, Some(50), Some(7), || Some("h1".into()));
        assert_eq!(r, Integrity::Ok);
        // Size mismatch + different bytes.
        let r = check_source_integrity(&e, &p, Some(50), Some(7), || Some("other".into()));
        assert!(matches!(r, Integrity::Changed { .. }));
        // Missing file.
        let r = check_source_integrity(&e, &dir.path().join("gone.jpg"), None, None, || None);
        assert!(matches!(r, Integrity::Missing { .. }));
    }

    #[test]
    fn hero_trims_low_scores_when_large() {
        let assets: Vec<Asset> = (0..41)
            .map(|i| asset(&format!("a{i:02}"), 1_714_557_600 + i, AssetKind::Photo))
            .collect();
        let mut scores = HashMap::new();
        scores.insert(
            "a00".to_string(),
            Score {
                total: 0.1,
                ..Default::default()
            },
        );
        let plan = build_plan(&assets, &HashMap::new(), &scores);
        assert_eq!(plan.hero.len(), 40, "low scorer dropped above 40");
        assert!(plan.hero.iter().all(|e| e.asset_id != "a00"));
        assert_eq!(plan.chapters.len(), 1);
        assert_eq!(plan.chapters[0].title, UNGROUPED);
    }

    #[test]
    fn reels_capped_and_round_robin() {
        let assets: Vec<Asset> = (0..12)
            .map(|i| asset(&format!("a{i:02}"), 1_714_557_600 + i, AssetKind::Photo))
            .collect();
        let co: HashMap<String, String> = assets
            .iter()
            .enumerate()
            .map(|(i, a)| (a.id.clone(), format!("c{}", i % 2)))
            .collect();
        let plan = build_plan(&assets, &co, &HashMap::new());
        assert_eq!(plan.reel_selects.len(), MAX_REELS);
        // Alternates clusters: c0, c1, c0, ...
        assert_eq!(plan.reel_selects[0].asset_id, "a00");
        assert_eq!(plan.reel_selects[1].asset_id, "a01");
        assert_eq!(plan.chapters.len(), 2);
    }

    #[test]
    fn duplicate_content_hash_collapses_in_hero() {
        let a = asset("a", 1_714_557_600, AssetKind::Photo);
        let mut b = asset("b", 1_714_557_700, AssetKind::Photo);
        b.content_hash = a.content_hash.clone();
        let plan = build_plan(&[a, b], &HashMap::new(), &HashMap::new());
        assert_eq!(plan.hero.len(), 1);
        assert_eq!(plan.hero[0].asset_id, "a");
    }
}
