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

/// Weekday + date as shown to viewers, `Wed, May 1`.
const DAY_FORMAT: &str = "%a, %b %-d";

/// Factual caption dateline, `Wed, May 1 · 10:00 AM` (UTC), used by the
/// local caption fallback when no place is known.
pub fn dateline(dt: &chrono::DateTime<chrono::Utc>) -> String {
    format!("{} · {}", dt.format(DAY_FORMAT), dt.format("%-I:%M %p"))
}

pub const PLAN_VERSION: u32 = 1;

/// Score assumed for assets missing from the score map.
const DEFAULT_SCORE: f32 = 0.5;
/// Most GPS fixes compared when picking a chapter's medoid.
const MEDOID_MAX_FIXES: usize = 256;
/// Hero lists longer than this drop entries below [`HERO_MIN_SCORE`].
const HERO_TRIM_ABOVE: usize = 40;
const HERO_MIN_SCORE: f32 = 0.35;
/// Hard cap on hero entries. At ~4 s per still plus clip trims this
/// holds a roughly 4-6 minute film; same-chapter stills are also grouped
/// into collages at render time, so each day can show several photos.
/// Above the cap, keep each chapter's best entry, then the best-scored
/// remainder, restored to story order.
const HERO_CAP: usize = 110;
/// Reel pack size and quality bar (the first [`MIN_REELS`] picks skip the bar).
const MAX_REELS: usize = 5;
const MIN_REELS: usize = 3;
const REEL_MIN_SCORE: f32 = 0.3;
/// Reel ranking bonus for motion-capable assets (clip / Live Photo with
/// companion): motion makes a stronger reel.
const REEL_MOTION_BONUS: f32 = 0.08;
/// Chapter label for assets with no cluster.
const UNGROUPED: &str = "Ungrouped";

/// A user-supplied place label for a date range, applied only when GPS
/// names nothing — e.g. a cruise ship's name for days at sea, which no
/// gazetteer can know. Supplied at scan time (`--place-override`), never
/// hardcoded in the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceOverride {
    pub start: chrono::NaiveDate,
    pub end: chrono::NaiveDate,
    pub label: String,
}

/// Parse `YYYY-MM-DD..YYYY-MM-DD=Label` entries separated by `;`.
pub fn parse_place_overrides(spec: &str) -> Result<Vec<PlaceOverride>, String> {
    let mut out = vec![];
    for part in spec.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let (range, label) = part
            .split_once('=')
            .ok_or_else(|| format!("place override {part:?} is missing =Label"))?;
        let (a, b) = range
            .split_once("..")
            .ok_or_else(|| format!("place override {part:?} is missing a .. date range"))?;
        let parse = |s: &str| {
            chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
                .map_err(|e| format!("place override {part:?}: bad date: {e}"))
        };
        let (start, end) = (parse(a)?, parse(b)?);
        if end < start || label.trim().is_empty() {
            return Err(format!(
                "place override {part:?} has an empty range or label"
            ));
        }
        out.push(PlaceOverride {
            start,
            end,
            label: label.trim().to_string(),
        });
    }
    Ok(out)
}

/// Build the plan from scored, clustered assets.
///
/// `cluster_of` maps asset id → cluster label; `scores` maps asset id →
/// score (only `total` is read). Deterministic: no wall-clock, no RNG.
pub fn build_plan(
    assets: &[Asset],
    cluster_of: &HashMap<String, String>,
    scores: &HashMap<String, Score>,
) -> Plan {
    build_plan_with_overrides(assets, cluster_of, scores, &[])
}

/// [`build_plan`] plus user place overrides for GPS-unnamed chapters.
pub fn build_plan_with_overrides(
    assets: &[Asset],
    cluster_of: &HashMap<String, String>,
    scores: &HashMap<String, Score>,
    overrides: &[PlaceOverride],
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
    if hero.len() > HERO_CAP {
        let mut ranked: Vec<usize> = (0..hero.len()).collect();
        ranked.sort_by(|&x, &y| {
            score_of(&hero[y].asset_id)
                .total_cmp(&score_of(&hero[x].asset_id))
                .then(x.cmp(&y))
        });
        // Reserve each chapter's best entry first so the cap never erases a
        // chapter from the film; remaining slots go to the best scores.
        let label = |i: usize| {
            cluster_of
                .get(&hero[i].asset_id)
                .map_or(UNGROUPED, String::as_str)
        };
        let mut seen_label: HashSet<&str> = HashSet::new();
        let (reps, rest): (Vec<usize>, Vec<usize>) = ranked
            .into_iter()
            .partition(|&i| seen_label.insert(label(i)));
        let keep: HashSet<usize> = reps.into_iter().chain(rest).take(HERO_CAP).collect();
        let mut i = 0;
        hero.retain(|_| {
            let k = keep.contains(&i);
            i += 1;
            k
        });
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

    // Reel selects: trip highlights. Rank every asset trip-wide by score
    // (motion-capable assets get a small ranking bonus — a clip or Live
    // Photo makes a stronger reel), then greedily take distinct day/place
    // moments so the pack spans the trip instead of stopping at the first
    // few days. Fill to the minimum from the remaining best when there
    // are not enough distinct moments. Presented in story order.
    let rank_of = |a: &Asset| {
        let motion = matches!(a.kind, AssetKind::Video)
            || (matches!(a.kind, AssetKind::LivePhoto) && a.companion_path.is_some());
        score_of(&a.id) + if motion { REEL_MOTION_BONUS } else { 0.0 }
    };
    let mut ranked: Vec<&Asset> = ordered.to_vec();
    ranked.sort_by(|a, b| rank_of(b).total_cmp(&rank_of(a)));
    // Quality floor, waived while the pack is below its minimum size.
    let qualifies =
        |a: &Asset, picked: usize| score_of(&a.id) >= REEL_MIN_SCORE || picked < MIN_REELS;
    let mut reel_selects: Vec<PlanEntry> = vec![];
    let mut picked_ids: HashSet<&str> = HashSet::new();
    let mut used_clusters: HashSet<&str> = HashSet::new();
    // Pass 1: best asset of each distinct day/place moment.
    for a in &ranked {
        if reel_selects.len() >= MAX_REELS {
            break;
        }
        let label = cluster_of.get(&a.id).map_or(UNGROUPED, String::as_str);
        if !used_clusters.contains(label) && qualifies(a, reel_selects.len()) {
            used_clusters.insert(label);
            picked_ids.insert(&a.id);
            reel_selects.push(plan_entry(a));
        }
    }
    // Pass 2: fill remaining slots with the next-best qualifying assets.
    for a in &ranked {
        if reel_selects.len() >= MAX_REELS {
            break;
        }
        if !picked_ids.contains(a.id.as_str()) && qualifies(a, reel_selects.len()) {
            picked_ids.insert(&a.id);
            reel_selects.push(plan_entry(a));
        }
    }
    let story_pos: HashMap<&str, usize> = ordered
        .iter()
        .enumerate()
        .map(|(i, a)| (a.id.as_str(), i))
        .collect();
    reel_selects.sort_by_key(|e| story_pos[e.asset_id.as_str()]);

    // Chapters: one per cluster label, in story order, named by place
    // when the chapter's GPS medoid lands near a populated place
    // (bundled gazetteer, never a network lookup). The day is shown once
    // per chapter (overlays, book day-marker pages) — photos inside a
    // chapter need no per-photo timestamp.
    let local_date = |a: &Asset| crate::metadata::local_date(a.taken_at?, a.tz_offset_secs);
    let trip_start = ordered.iter().filter_map(|a| local_date(a)).min();
    let chapters: Vec<Chapter> = groups
        .into_iter()
        .map(|(label, members)| {
            let gps: Vec<(f64, f64)> = members.iter().filter_map(|a| a.gps).collect();
            let first_date = members.iter().filter_map(|a| local_date(a)).min();
            // GPS naming first; a user override (e.g. the ship's name for
            // days at sea) only fills what GPS honestly cannot name.
            let place = chapter_place(&gps).or_else(|| {
                let d = first_date?;
                overrides
                    .iter()
                    .find(|o| o.start <= d && d <= o.end)
                    .map(|o| o.label.clone())
            });
            Chapter {
                title: place.clone().unwrap_or_else(|| human_chapter_title(label)),
                asset_ids: members.iter().map(|a| a.id.clone()).collect(),
                place,
                day_index: match (first_date, trip_start) {
                    (Some(d), Some(s)) => Some(((d - s).num_days() + 1).max(1) as u32),
                    _ => None,
                },
                date_label: first_date.map(|d| d.format(DAY_FORMAT).to_string()),
            }
        })
        .collect();

    // Auto captions: the photo's chapter place, nothing else. Day/time
    // live on the chapter, not on every photo; without a pinned VLM we
    // never invent what a photo shows. Photos in unnamed places get no
    // auto caption (empty in Story review, ready for the user's words).
    // A caption the user embedded in the file itself always wins.
    let captions: HashMap<String, String> = chapters
        .iter()
        .filter_map(|c| c.place.as_deref().map(|p| (c, p)))
        .flat_map(|(c, p)| {
            c.asset_ids
                .iter()
                .map(move |id| (id.clone(), p.to_string()))
        })
        .chain(ordered.iter().filter_map(|a| {
            let caption = a.user_caption.clone()?;
            Some((a.id.clone(), caption))
        }))
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

/// Place name for a chapter from its GPS fixes: the geographic medoid
/// (the fix minimizing total distance to the others) names the place.
/// A raw mean breaks at the antimeridian and is dragged by outliers. The
/// search is quadratic, so large chapters use an even sample of their fixes.
fn chapter_place(gps: &[(f64, f64)]) -> Option<String> {
    let step = gps.len().div_ceil(MEDOID_MAX_FIXES).max(1);
    let sample: Vec<(f64, f64)> = gps.iter().copied().step_by(step).collect();
    let (_, medoid) = sample
        .iter()
        .map(|&p| {
            let total: f64 = sample.iter().map(|&g| crate::geo::haversine_m(p, g)).sum();
            (total, p)
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))?;
    crate::geo::place_name(medoid)
}

/// Display title for a cluster label: `2026-10-12 · place 21` (or a bare
/// `2026-10-12`) becomes `Mon, Oct 12`; anything else passes through.
fn human_chapter_title(label: &str) -> String {
    let date_part = label.split_once(" · ").map_or(label, |(d, _)| d);
    chrono::NaiveDate::parse_from_str(date_part, "%Y-%m-%d")
        .map_or_else(|_| label.to_string(), |d| d.format(DAY_FORMAT).to_string())
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

    #[test]
    fn dateline_is_weekday_date_and_clock() {
        let dt = chrono::DateTime::from_timestamp(1_714_557_600, 0).unwrap();
        assert_eq!(dateline(&dt), "Wed, May 1 · 10:00 AM");
    }
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
        // No GPS in this fixture: no place captions, but chapters still
        // carry trip-day context (shown once per chapter, not per photo).
        assert!(plan.captions.is_empty(), "got: {:?}", plan.captions);
        assert_eq!(plan.chapters[0].day_index, Some(1));
        assert_eq!(plan.chapters[0].date_label.as_deref(), Some("Wed, May 1"));
        // (Fixture timestamps all land on May 1 local; cluster labels differ.)
        assert_eq!(plan.chapters[1].day_index, Some(1));
        assert_eq!(plan.chapters[0].place, None);
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
    fn chapter_place_uses_medoid_not_mean() {
        // Kyoto cluster plus one far outlier: still Kyoto.
        let mut gps: Vec<(f64, f64)> = (0..8)
            .map(|i| (34.9671 + i as f64 * 0.001, 135.7727))
            .collect();
        gps.push((-33.8688, 151.2093)); // Sydney outlier
        assert_eq!(chapter_place(&gps).as_deref(), Some("Kyoto, Japan"));
        // Both sides of the antimeridian (Fiji): a raw mean would land
        // near longitude 0 and name nothing (or the wrong continent).
        let fiji = [(-16.81, -179.9), (-16.82, 179.9), (-16.80, -179.8)];
        let name = chapter_place(&fiji).expect("Fiji place");
        assert!(name.contains("Fiji"), "got {name}");
        assert_eq!(chapter_place(&[]), None);
        // A nearby attraction beats the city; open water names nothing.
        assert_eq!(
            chapter_place(&[(35.6267, 139.8881)]).as_deref(),
            Some("Tokyo DisneySea")
        );
        assert_eq!(chapter_place(&[(2.221, 105.205)]), None);
        // Past the sampling cap, the dense cluster still wins.
        let mut many = vec![(34.9671, 135.7727); 1_000];
        many.extend([(-33.8688, 151.2093); 20]);
        assert_eq!(chapter_place(&many).as_deref(), Some("Kyoto, Japan"));
    }

    #[test]
    fn chapters_name_places_and_captions_follow() {
        // Kyoto GPS on every asset: chapter is named by place, captions
        // carry the place, and the title prefers the place over the date.
        let mut assets = vec![
            asset("a", 1_714_557_600, AssetKind::Photo),
            asset("b", 1_714_557_900, AssetKind::Photo),
        ];
        for a in &mut assets {
            a.gps = Some((34.9671, 135.7727));
        }
        let mut co = HashMap::new();
        co.insert("a".into(), "2024-05-01 · place 1".into());
        co.insert("b".into(), "2024-05-01 · place 1".into());
        let plan = build_plan(&assets, &co, &scored(&["a", "b"]));
        assert_eq!(plan.chapters[0].place.as_deref(), Some("Kyoto, Japan"));
        assert_eq!(plan.chapters[0].title, "Kyoto, Japan");
        assert_eq!(plan.captions["a"], "Kyoto, Japan");
        assert_eq!(plan.captions["b"], "Kyoto, Japan");
    }

    #[test]
    fn user_caption_beats_place_caption() {
        let mut a = asset("a", 1_714_557_600, AssetKind::Photo);
        a.gps = Some((34.9671, 135.7727)); // Kyoto
        a.user_caption = Some("Mia's first shinkansen ride".into());
        let mut b = asset("b", 1_714_557_900, AssetKind::Photo);
        b.gps = Some((34.9671, 135.7727));
        let assets = vec![a, b];
        let mut co = HashMap::new();
        co.insert("a".into(), "2024-05-01 · place 1".into());
        co.insert("b".into(), "2024-05-01 · place 1".into());
        let plan = build_plan(&assets, &co, &scored(&["a", "b"]));
        assert_eq!(plan.captions["a"], "Mia's first shinkansen ride");
        assert_eq!(plan.captions["b"], "Kyoto, Japan");
    }

    #[test]
    fn place_overrides_fill_only_unnamed_chapters() {
        // Sea day (open-water GPS) inside the override range takes the
        // user's label; a GPS-named day in range keeps its place; a sea
        // day outside the range stays unnamed.
        let overrides = parse_place_overrides("2024-05-01..2024-05-03=SS Test Ship").unwrap();
        assert_eq!(overrides.len(), 1);
        assert!(parse_place_overrides("2024-05-03..2024-05-01=X").is_err());
        assert!(parse_place_overrides("May 1=X").is_err());
        let ts = |date: &str| {
            chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .unwrap()
                .and_utc()
                .timestamp()
        };
        let mut sea = asset("sea", ts("2024-05-02"), AssetKind::Photo);
        sea.gps = Some((2.221, 105.205)); // open water: names nothing
        let mut port = asset("port", ts("2024-05-02"), AssetKind::Photo);
        port.gps = Some((34.9671, 135.7727)); // Kyoto
        let mut far = asset("far", ts("2024-06-02"), AssetKind::Photo);
        far.gps = Some((2.221, 105.205));
        let assets = vec![sea, port, far];
        let mut co = HashMap::new();
        co.insert("sea".into(), "2024-05-02 · place 1".into());
        co.insert("port".into(), "2024-05-02 · place 2".into());
        co.insert("far".into(), "2024-06-02 · place 3".into());
        let plan =
            build_plan_with_overrides(&assets, &co, &scored(&["sea", "port", "far"]), &overrides);
        let place_of = |id: &str| {
            plan.chapters
                .iter()
                .find(|c| c.asset_ids.iter().any(|a| a == id))
                .and_then(|c| c.place.as_deref())
        };
        assert_eq!(place_of("sea"), Some("SS Test Ship"));
        assert_eq!(place_of("port"), Some("Kyoto, Japan"));
        assert_eq!(place_of("far"), None);
    }

    #[test]
    fn chapter_titles_are_human_dates() {
        assert_eq!(human_chapter_title("2026-10-12 · place 21"), "Mon, Oct 12");
        assert_eq!(human_chapter_title("2024-05-01"), "Wed, May 1");
        assert_eq!(human_chapter_title(UNGROUPED), UNGROUPED);
        assert_eq!(human_chapter_title("unknown-date"), "unknown-date");
    }

    #[test]
    fn reels_are_trip_highlights_not_first_days() {
        // Six day-clusters; the sharpest shot of the trip is on the last
        // day. Old selection took the best of the first five clusters and
        // never reached it.
        let mut assets = vec![];
        let mut co = HashMap::new();
        let mut scores = HashMap::new();
        for day in 1..=6u32 {
            for j in 0..2 {
                let id = format!("d{day}_{j}");
                let taken = 1_714_557_600 + i64::from(day) * 86_400 + i64::from(j) * 60;
                assets.push(asset(&id, taken, AssetKind::Photo));
                co.insert(id.clone(), format!("2024-05-0{day}"));
                scores.insert(
                    id,
                    Score {
                        total: 0.40 + day as f32 * 0.05 + j as f32 * 0.01,
                        highlight: false,
                        ..Default::default()
                    },
                );
            }
        }
        // The trip highlight: last day, best score by far.
        scores.insert(
            "d6_1".to_string(),
            Score {
                total: 0.99,
                highlight: true,
                ..Default::default()
            },
        );
        let plan = build_plan(&assets, &co, &scores);
        let ids: Vec<&str> = plan
            .reel_selects
            .iter()
            .map(|e| e.asset_id.as_str())
            .collect();
        assert_eq!(ids.len(), MAX_REELS, "full pack: {ids:?}");
        assert!(ids.contains(&"d6_1"), "trip highlight included: {ids:?}");
        assert!(
            !ids.contains(&"d1_1"),
            "weakest day's best is not a trip highlight: {ids:?}"
        );
        // Story order in the pack.
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted, "reels in story order: {ids:?}");
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
    fn hero_caps_keeping_story_order() {
        let assets: Vec<Asset> = (0..130)
            .map(|i| {
                asset(
                    &format!("a{i:02}"),
                    1_714_557_600 + i * 60,
                    AssetKind::Photo,
                )
            })
            .collect();
        let mut scores = HashMap::new();
        for i in 0..130 {
            scores.insert(
                format!("a{i:02}"),
                Score {
                    total: 0.35 + i as f32 / 100.0,
                    ..Default::default()
                },
            );
        }
        let plan = build_plan(&assets, &HashMap::new(), &scores);
        assert_eq!(
            plan.hero.len(),
            110,
            "hero capped at 110: {}",
            plan.hero.len()
        );
        assert!(
            plan.hero.iter().all(|e| e.asset_id != "a00"),
            "lowest scorer dropped by cap"
        );
        let ids: Vec<&str> = plan.hero.iter().map(|e| e.asset_id.as_str()).collect();
        let mut sorted = ids.clone();
        sorted.sort_by_key(|id| id[1..].parse::<usize>().unwrap());
        assert_eq!(ids, sorted, "kept entries stay in story order");
    }

    #[test]
    fn hero_cap_keeps_every_chapter() {
        let assets: Vec<Asset> = (0..111)
            .map(|i| {
                asset(
                    &format!("a{i:02}"),
                    1_714_557_600 + i * 60,
                    AssetKind::Photo,
                )
            })
            .collect();
        let mut scores = HashMap::new();
        let mut clusters = HashMap::new();
        for i in 0..111 {
            let id = format!("a{i:02}");
            let (total, day) = if i < 110 {
                (0.8, "Day 1")
            } else {
                (0.7, "Day 2")
            };
            scores.insert(
                id.clone(),
                Score {
                    total,
                    ..Default::default()
                },
            );
            clusters.insert(id, day.to_string());
        }
        let plan = build_plan(&assets, &clusters, &scores);
        assert_eq!(plan.hero.len(), 110);
        assert!(
            plan.hero.iter().any(|e| e.asset_id == "a110"),
            "lower-scored chapter keeps its best shot"
        );
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
    fn reels_capped_at_max() {
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
        // Distinct clusters first (a00 from c0, a01 from c1), then best
        // remaining fills the pack, all in story order.
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
