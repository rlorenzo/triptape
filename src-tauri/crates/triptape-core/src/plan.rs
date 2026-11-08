//! Stage 7 — Plan: one versioned AI Plan JSON for all three renderers.
//!
//! Hero shot list in story order, 3–5 reel selects varied by day/place,
//! chapters by day/place, template captions, map polyline + day markers,
//! music pick with rationale. Per-entry identity (asset id, content hash,
//! size, mtime, rendition) feeds the pre-render source-integrity check.
//! Sec 5.4 degradation eligibility lives here too.

use crate::cluster::{gps_outliers, story_order};
use crate::types::{Asset, AssetKind, Chapter, MusicPick, Plan, PlanEntry, Rendition, Score};
use std::collections::{BTreeMap, HashMap, HashSet};
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

/// Auto film length: this many seconds per square root of trip days, so a
/// weekend gets ~3 min, a week ~4.5 min and a month ~10 min (bigger trips
/// get a longer film, but not linearly longer).
pub const AUTO_SECS_PER_SQRT_DAY: f64 = 100.0;
/// Auto length floor and ceiling.
pub const AUTO_MIN_FILM_SECS: f64 = 120.0;
pub const AUTO_MAX_FILM_SECS: f64 = 720.0;
/// Bounds for a user-requested length (`scan --length`).
pub const MIN_FILM_SECS: f64 = 30.0;
pub const MAX_FILM_SECS: f64 = 3600.0;

// Hero timing model. Mirrors the renderer (`triptape-media` render_video:
// `VideoOptions` defaults, collage hold, clip freeze-pad, transition
// overlaps, closing cards) so the plan can fill a time budget. Keep in sync.
/// Solo still hold.
const EST_STILL_SECS: f64 = 4.0;
/// Collage hold (renderer: `still_secs * 1.5`).
const EST_COLLAGE_SECS: f64 = 6.0;
/// Most stills per collage (renderer `MAX_COLLAGE`).
const EST_MAX_COLLAGE: usize = 4;
/// Clips shorter than this are freeze-padded up to it.
const EST_MIN_CLIP_SECS: f64 = 2.5;
/// Average transition overlap per cut.
const EST_CUT_OVERLAP_SECS: f64 = 0.55;
/// Title, opening map, closing map and places card.
const EST_FIXED_SECS: f64 = 2.5 + 3.0 + 4.5 + 4.5;

/// How long the hero film should be.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum FilmLength {
    /// Scale with the trip ([`auto_film_secs`]).
    #[default]
    Auto,
    /// A requested length in seconds.
    Secs(f64),
}

/// Default film length for a trip spanning `trip_days` calendar days.
pub fn auto_film_secs(trip_days: usize) -> f64 {
    (AUTO_SECS_PER_SQRT_DAY * (trip_days.max(1) as f64).sqrt())
        .clamp(AUTO_MIN_FILM_SECS, AUTO_MAX_FILM_SECS)
}

/// Parse a film length: `auto`, plain seconds (`600`), or `10m`, `90s`,
/// `4m30s`. Lengths outside [`MIN_FILM_SECS`]..=[`MAX_FILM_SECS`] are refused.
pub fn parse_film_length(spec: &str) -> Result<FilmLength, String> {
    let s = spec.trim().to_ascii_lowercase();
    if s == "auto" {
        return Ok(FilmLength::Auto);
    }
    let bad = || format!("film length {spec:?}: want auto, seconds, or e.g. 10m / 4m30s");
    let secs = if let Ok(n) = s.parse::<f64>() {
        n
    } else {
        let (mins, rest) = match s.split_once('m') {
            Some((m, r)) => (m.parse::<f64>().map_err(|_| bad())?, r),
            None => (0.0, s.as_str()),
        };
        let secs = match rest.strip_suffix('s') {
            Some(n) => n.parse::<f64>().map_err(|_| bad())?,
            None if rest.is_empty() => 0.0,
            None => return Err(bad()),
        };
        mins * 60.0 + secs
    };
    if !secs.is_finite() || !(MIN_FILM_SECS..=MAX_FILM_SECS).contains(&secs) {
        return Err(format!(
            "film length {spec:?} must be between {MIN_FILM_SECS} s and {} min",
            MAX_FILM_SECS / 60.0
        ));
    }
    Ok(FilmLength::Secs(secs))
}

/// Estimated rendered seconds for hero entries in story order, grouping
/// consecutive same-chapter stills into collages the way the renderer does.
fn estimate_hero_secs<'e, 'c>(
    entries: impl IntoIterator<Item = &'e PlanEntry>,
    chapter_of: impl Fn(&str) -> &'c str,
) -> f64 {
    let mut total = EST_FIXED_SECS;
    let mut segments = 4usize; // the fixed cards
    let mut run: Option<(&str, usize)> = None; // (chapter, stills in collage)
    for e in entries {
        let chapter = chapter_of(&e.asset_id);
        match e.rendition {
            Rendition::Still => match &mut run {
                Some((c, n)) if *c == chapter && *n < EST_MAX_COLLAGE => {
                    // Growing a solo still into a collage, or a collage by one.
                    if *n == 1 {
                        total += EST_COLLAGE_SECS - EST_STILL_SECS;
                    }
                    *n += 1;
                }
                _ => {
                    total += EST_STILL_SECS;
                    segments += 1;
                    run = Some((chapter, 1));
                }
            },
            Rendition::Motion {
                trim_start,
                trim_end,
            }
            | Rendition::Clip {
                trim_start,
                trim_end,
            } => {
                total += (trim_end - trim_start)
                    .clamp(0.5, 30.0)
                    .max(EST_MIN_CLIP_SECS);
                segments += 1;
                run = None;
            }
        }
    }
    total - EST_CUT_OVERLAP_SECS * segments.saturating_sub(1) as f64
}

/// Pick hero entries (given in story order) to fill `budget_secs`.
///
/// Every chapter first gets its best entry (dropping the weakest chapters'
/// picks only if even those overrun the budget). Remaining time is dealt
/// out one entry at a time to the chapter with the fewest picks relative
/// to `sqrt(chapter size)`, taking that chapter's best remaining entry, so
/// big days get more screen time without crowding out small ones. Stops at
/// the first pick that would overrun the budget. Keeps story order.
fn select_hero_by_budget<'a>(
    cands: Vec<PlanEntry>,
    chapter_of: impl Fn(&str) -> &'a str + Copy,
    score_of: impl Fn(&str) -> f32,
    budget_secs: f64,
) -> Vec<PlanEntry> {
    // Chapters in story order, each with its candidates best-first.
    let mut chapters: Vec<(&str, Vec<usize>)> = vec![];
    for (i, e) in cands.iter().enumerate() {
        let c = chapter_of(&e.asset_id);
        match chapters.iter_mut().find(|(l, _)| *l == c) {
            Some((_, m)) => m.push(i),
            None => chapters.push((c, vec![i])),
        }
    }
    for (_, members) in &mut chapters {
        members.sort_by(|&x, &y| {
            score_of(&cands[y].asset_id)
                .total_cmp(&score_of(&cands[x].asset_id))
                .then(x.cmp(&y))
        });
    }
    let mut picked = vec![false; cands.len()];
    let estimate = |picked: &[bool]| {
        estimate_hero_secs(
            cands
                .iter()
                .zip(picked)
                .filter(|(_, p)| **p)
                .map(|(e, _)| e),
            chapter_of,
        )
    };
    // Every chapter's best entry, weakest dropped first if over budget.
    let mut reps: Vec<usize> = chapters.iter().map(|(_, m)| m[0]).collect();
    for &r in &reps {
        picked[r] = true;
    }
    reps.sort_by(|&x, &y| {
        score_of(&cands[x].asset_id)
            .total_cmp(&score_of(&cands[y].asset_id))
            .then(y.cmp(&x))
    });
    for &r in &reps {
        if estimate(&picked) <= budget_secs || picked.iter().filter(|p| **p).count() <= 1 {
            break;
        }
        picked[r] = false;
    }
    // Deal the rest of the budget across chapters.
    let mut taken: Vec<usize> = chapters
        .iter()
        .map(|(_, m)| usize::from(picked[m[0]]))
        .collect();
    loop {
        let next = chapters
            .iter()
            .enumerate()
            .filter(|(ci, (_, m))| taken[*ci] < m.len())
            .min_by(|(a, (_, ma)), (b, (_, mb))| {
                let share = |ci: usize, n: usize| taken[ci] as f64 / (n as f64).sqrt();
                share(*a, ma.len())
                    .total_cmp(&share(*b, mb.len()))
                    .then(a.cmp(b))
            })
            .map(|(ci, _)| ci);
        let Some(ci) = next else { break };
        let i = chapters[ci].1[taken[ci]];
        picked[i] = true;
        if estimate(&picked) > budget_secs {
            picked[i] = false;
            break;
        }
        taken[ci] += 1;
    }
    cands
        .into_iter()
        .zip(picked)
        .filter_map(|(e, p)| p.then_some(e))
        .collect()
}

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

/// User edits to an auto-generated plan, made in the Story editor. The
/// stored plan is never mutated: edits live separately and are applied on
/// read (`Db::edited_plan`), so they can be undone. They belong to one
/// project; importing a folder again creates a new project without them.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlanEdits {
    /// Assets removed from every keepsake pick (hero, reels, book flow),
    /// oldest removal first.
    #[serde(default)]
    pub excluded: Vec<String>,
    /// Hero/reel entry asset id -> replacement asset id (same chapter), in
    /// the order the user made them: a later swap can replace an earlier
    /// swap's replacement, so this is a list rather than a map.
    #[serde(default)]
    pub swaps: Vec<(String, String)>,
    /// Asset id of any chapter member -> user place label for that chapter.
    #[serde(default)]
    pub place_overrides: BTreeMap<String, String>,
    /// Asset id -> user caption (beats embedded and place captions).
    #[serde(default)]
    pub caption_overrides: BTreeMap<String, String>,
}

impl PlanEdits {
    pub fn is_empty(&self) -> bool {
        self.excluded.is_empty()
            && self.swaps.is_empty()
            && self.place_overrides.is_empty()
            && self.caption_overrides.is_empty()
    }

    /// Set or (when blank) clear the place label for a chapter.
    pub fn set_place(&mut self, chapter_key: String, place: Option<String>) {
        set_override(&mut self.place_overrides, chapter_key, place);
    }

    /// Set or (when blank) clear the caption for an asset.
    pub fn set_caption(&mut self, asset_id: String, caption: Option<String>) {
        set_override(&mut self.caption_overrides, asset_id, caption);
    }

    /// Remove an asset from the keepsakes, or restore it.
    pub fn set_excluded(&mut self, asset_id: String, excluded: bool) {
        self.excluded.retain(|id| *id != asset_id);
        if excluded {
            self.excluded.push(asset_id);
        }
    }

    /// Replace a pick, superseding the earlier swap of the same pick. That
    /// swap stays if a later one brought the asset back: replay needs it to
    /// vacate its original slot first. Using an excluded asset as the
    /// replacement restores it.
    pub fn swap(&mut self, from: String, to: String) {
        if let Some(i) = self.swaps.iter().rposition(|(f, _)| *f == from) {
            if !self.swaps[i + 1..].iter().any(|(_, t)| *t == from) {
                self.swaps.remove(i);
            }
        }
        self.set_excluded(to.clone(), false);
        self.swaps.push((from, to));
    }
}

fn set_override(map: &mut BTreeMap<String, String>, key: String, value: Option<String>) {
    match value.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => map.insert(key, v.to_string()),
        None => map.remove(&key),
    };
}

/// Rename a chapter; members captioned with the old place follow the rename.
fn rename_chapter(ch: &mut Chapter, label: &str, captions: &mut HashMap<String, String>) {
    if let Some(old) = ch.place.replace(label.to_string()) {
        for id in &ch.asset_ids {
            if captions.get(id).is_some_and(|c| *c == old) {
                captions.insert(id.clone(), label.to_string());
            }
        }
    }
    ch.title = label.to_string();
}

/// Apply user edits to a stored plan. Pure and deterministic:
/// place overrides first (they rename whole chapters), then swaps,
/// then exclusions, then caption overrides on top.
pub fn apply_edits(plan: &Plan, assets: &[Asset], edits: &PlanEdits) -> Plan {
    if edits.is_empty() {
        return plan.clone();
    }
    let by_id: HashMap<&str, &Asset> = assets.iter().map(|a| (a.id.as_str(), a)).collect();
    let dead: HashSet<&str> = edits.excluded.iter().map(String::as_str).collect();
    // Swaps and captions only land on assets in this plan's book flow.
    let in_plan: HashSet<&str> = plan
        .chapters
        .iter()
        .flat_map(|c| &c.asset_ids)
        .map(String::as_str)
        .collect();
    let mut out = plan.clone();

    // Place overrides: key matches any member of the chapter.
    for (key, label) in &edits.place_overrides {
        for ch in out
            .chapters
            .iter_mut()
            .filter(|c| c.asset_ids.contains(key))
        {
            rename_chapter(ch, label, &mut out.captions);
        }
    }

    // Swaps: replace a hero/reel pick with the user's chosen asset. A removed
    // replacement still applies here and drops out with the exclusions below,
    // so removing it can't bring back the pick it replaced.
    for (from, to) in &edits.swaps {
        if !in_plan.contains(to.as_str()) {
            continue;
        }
        let Some(asset) = by_id.get(to.as_str()) else {
            continue;
        };
        // Never duplicate an asset in one keepsake: a replacement the stored
        // plan already picks takes over, and the old pick goes. This is how
        // a removed pick is restored as a replacement; `check_swap` rejects
        // replacements that are still picked.
        for picks in [&mut out.hero, &mut out.reel_selects] {
            if picks.iter().any(|e| e.asset_id == *to) {
                picks.retain(|e| e.asset_id != *from);
            } else if let Some(entry) = picks.iter_mut().find(|e| e.asset_id == *from) {
                *entry = plan_entry(asset);
            }
        }
    }

    // Exclusions: out of every pick and out of the book flow.
    out.hero.retain(|e| !dead.contains(e.asset_id.as_str()));
    out.reel_selects
        .retain(|e| !dead.contains(e.asset_id.as_str()));
    for ch in &mut out.chapters {
        ch.asset_ids.retain(|id| !dead.contains(id.as_str()));
    }
    out.chapters.retain(|c| !c.asset_ids.is_empty());
    out.captions.retain(|id, _| !dead.contains(id.as_str()));

    // Caption overrides last: the user's words win.
    for (id, caption) in &edits.caption_overrides {
        if in_plan.contains(id.as_str()) && !dead.contains(id.as_str()) {
            out.captions.insert(id.clone(), caption.clone());
        }
    }
    out
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
    build_plan_with_length(assets, cluster_of, scores, overrides, FilmLength::Auto)
}

/// [`build_plan_with_overrides`] with a chosen hero film length.
pub fn build_plan_with_length(
    assets: &[Asset],
    cluster_of: &HashMap<String, String>,
    scores: &HashMap<String, Score>,
    overrides: &[PlaceOverride],
    length: FilmLength,
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
    let local_date = |a: &Asset| crate::metadata::local_date(a.taken_at?, a.tz_offset_secs);
    let budget_secs = match length {
        FilmLength::Secs(s) => s,
        FilmLength::Auto => {
            let days: HashSet<_> = ordered.iter().filter_map(|a| local_date(a)).collect();
            auto_film_secs(days.len())
        }
    };
    let chapter_label = |id: &str| cluster_of.get(id).map_or(UNGROUPED, String::as_str);
    let hero = select_hero_by_budget(hero, chapter_label, |id| score_of(id), budget_secs);

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

    // Map polyline: GPS fixes in story order, minus fixes that imply
    // impossible speed. Capture times separate those from real flights.
    let outliers = gps_outliers(assets);
    let map_polyline: Vec<(f64, f64)> = ordered
        .iter()
        .filter(|a| !outliers.contains(&a.id))
        .filter_map(|a| a.gps)
        .collect();

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
    /// Hero film variant: "full" (length from [`FilmLength`]), "short" (≤30 s slideshow), or "disabled".
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
    fn map_polyline_keeps_flights_drops_impossible_jumps() {
        let fix = |id: &str, hours: i64, gps: (f64, f64)| {
            let mut a = asset(id, 1_714_500_000 + hours * 3600, AssetKind::Photo);
            a.gps = Some(gps);
            a
        };
        let la = (33.94, -118.4);
        let tokyo = (35.6, 139.7);
        // 8,800 km in 12 h is a flight; Sydney one hour later is bad GPS.
        let assets = vec![
            fix("a", 0, la),
            fix("b", 12, tokyo),
            fix("c", 13, (-33.87, 151.21)),
            fix("d", 14, tokyo),
        ];
        let plan = build_plan(&assets, &HashMap::new(), &HashMap::new());
        assert_eq!(plan.map_polyline, vec![la, tokyo, tokyo]);
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
    fn edits_exclude_swap_rename_and_recaption() {
        let mk = |id: &str, t: i64| {
            let mut a = asset(id, t, AssetKind::Photo);
            a.gps = Some((34.9671, 135.7727)); // Kyoto
            a
        };
        let assets = vec![
            mk("a", 1_714_557_600),
            mk("b", 1_714_557_900),
            mk("c", 1_714_558_200),
        ];
        let mut co = HashMap::new();
        for id in ["a", "b", "c"] {
            co.insert(id.into(), "2024-05-01 · place 1".into());
        }
        let base = build_plan(&assets, &co, &scored(&["a", "b", "c"]));
        assert_eq!(base.chapters[0].place.as_deref(), Some("Kyoto, Japan"));

        let edits = PlanEdits {
            excluded: vec!["b".into()],
            swaps: vec![("a".into(), "c".into())],
            place_overrides: [("a".to_string(), "Our Ryokan".to_string())].into(),
            caption_overrides: [("c".to_string(), "Breakfast view".to_string())].into(),
        };
        let out = apply_edits(&base, &assets, &edits);
        // Rename hit the chapter and its place-derived captions.
        assert_eq!(out.chapters[0].place.as_deref(), Some("Our Ryokan"));
        assert_eq!(out.chapters[0].title, "Our Ryokan");
        // Swap replaced the hero pick in place; exclusion removed b.
        assert!(out
            .hero
            .iter()
            .all(|e| e.asset_id != "a" && e.asset_id != "b"));
        assert!(out.hero.iter().any(|e| e.asset_id == "c"));
        assert_eq!(
            out.chapters[0].asset_ids,
            vec!["a".to_string(), "c".to_string()]
        );
        // Caption override wins over the renamed place.
        assert_eq!(out.captions["c"], "Breakfast view");
        // Empty edits are a clone.
        assert_eq!(
            apply_edits(&base, &assets, &PlanEdits::default())
                .hero
                .len(),
            base.hero.len()
        );
        // A swap target outside the plan (e.g. suppressed on rescan) is ignored.
        let stray = PlanEdits {
            swaps: vec![("a".into(), "gone".into())],
            ..Default::default()
        };
        let mut with_stray = assets.clone();
        with_stray.push(mk("gone", 1_714_558_500));
        let out = apply_edits(&base, &with_stray, &stray);
        assert!(out.hero.iter().all(|e| e.asset_id != "gone"));
        assert!(out.hero.iter().any(|e| e.asset_id == "a"));
    }

    #[test]
    fn edit_setters_replace_and_clear_blank_values() {
        let mut edits = PlanEdits::default();
        edits.set_caption("a".into(), Some(" first ".into()));
        edits.set_caption("a".into(), Some("second".into()));
        assert_eq!(edits.caption_overrides["a"], "second");
        edits.set_caption("a".into(), Some("   ".into()));
        assert!(edits.caption_overrides.is_empty());
        edits.set_place("k".into(), Some("Hotel".into()));
        edits.set_place("k".into(), None);
        assert!(edits.is_empty());
    }

    #[test]
    fn set_excluded_keeps_one_entry_in_removal_order() {
        let mut edits = PlanEdits::default();
        edits.set_excluded("b".into(), true);
        edits.set_excluded("a".into(), true);
        edits.set_excluded("b".into(), true);
        assert_eq!(edits.excluded, ["a", "b"]);
        edits.set_excluded("a".into(), false);
        assert_eq!(edits.excluded, ["b"]);
    }

    #[test]
    fn swap_supersedes_same_pick_and_restores_replacement() {
        let mut edits = PlanEdits::default();
        edits.set_excluded("d".into(), true);
        edits.swap("a".into(), "c".into());
        edits.swap("a".into(), "d".into());
        assert_eq!(edits.swaps, [("a".to_string(), "d".to_string())]);
        assert!(edits.excluded.is_empty());
    }

    #[test]
    fn swap_keeps_earlier_swap_when_the_pick_came_back() {
        let assets: Vec<Asset> = ["a", "b", "x", "y"]
            .iter()
            .enumerate()
            .map(|(i, id)| asset(id, 1_714_557_600 + i as i64 * 300, AssetKind::Photo))
            .collect();
        let co = ["a", "b", "x", "y"]
            .iter()
            .map(|id| (id.to_string(), "2024-05-01".to_string()))
            .collect();
        let mut base = build_plan(&assets, &co, &scored(&["a", "b", "x", "y"]));
        base.hero = vec![plan_entry(&assets[0]), plan_entry(&assets[1])];
        let mut edits = PlanEdits::default();
        edits.swap("a".into(), "x".into()); // [x, b]
        edits.swap("b".into(), "a".into()); // [x, a]
        edits.swap("a".into(), "y".into()); // [x, y]
        let out = apply_edits(&base, &assets, &edits);
        let ids: Vec<&str> = out.hero.iter().map(|e| e.asset_id.as_str()).collect();
        assert_eq!(ids, ["x", "y"]);
    }

    #[test]
    fn swaps_apply_in_the_order_they_were_made() {
        let assets: Vec<Asset> = ["a", "z", "c", "d"]
            .iter()
            .enumerate()
            .map(|(i, id)| asset(id, 1_714_557_600 + i as i64 * 300, AssetKind::Photo))
            .collect();
        let co = ["a", "z", "c", "d"]
            .iter()
            .map(|id| (id.to_string(), "2024-05-01".to_string()))
            .collect();
        let mut base = build_plan(&assets, &co, &scored(&["a", "z", "c", "d"]));
        base.hero = vec![plan_entry(&assets[1])]; // only "z" is picked
        let edits = PlanEdits {
            swaps: vec![("z".into(), "c".into()), ("c".into(), "d".into())],
            ..Default::default()
        };
        let out = apply_edits(&base, &assets, &edits);
        let ids: Vec<&str> = out.hero.iter().map(|e| e.asset_id.as_str()).collect();
        assert_eq!(ids, ["d"]);
    }

    #[test]
    fn removing_a_replacement_does_not_restore_the_pick_it_replaced() {
        let assets: Vec<Asset> = ["a", "b", "c", "keep"]
            .iter()
            .enumerate()
            .map(|(i, id)| asset(id, 1_714_557_600 + i as i64 * 300, AssetKind::Photo))
            .collect();
        let co = ["a", "b", "c", "keep"]
            .iter()
            .map(|id| (id.to_string(), "2024-05-01".to_string()))
            .collect();
        let mut base = build_plan(&assets, &co, &scored(&["a", "b", "c", "keep"]));
        base.hero = vec![plan_entry(&assets[0]), plan_entry(&assets[3])];
        base.reel_selects = base.hero.clone();
        let picks = |edits: &PlanEdits| {
            let out = apply_edits(&base, &assets, edits);
            let ids =
                |p: &[PlanEntry]| -> Vec<String> { p.iter().map(|e| e.asset_id.clone()).collect() };
            assert_eq!(ids(&out.hero), ids(&out.reel_selects));
            ids(&out.hero)
        };

        // Swap, then remove the replacement; restoring it brings the swap back.
        let mut edits = PlanEdits::default();
        edits.swap("a".into(), "b".into());
        edits.set_excluded("b".into(), true);
        assert_eq!(picks(&edits), ["keep"]);
        edits.set_excluded("b".into(), false);
        assert_eq!(picks(&edits), ["b", "keep"]);

        // Removing the last target of a chain drops the whole chain.
        edits.swap("b".into(), "c".into());
        edits.set_excluded("c".into(), true);
        assert_eq!(picks(&edits), ["keep"]);

        // Swapping back to the original and removing it stays removed too.
        let mut edits = PlanEdits::default();
        edits.swap("a".into(), "b".into());
        edits.swap("b".into(), "a".into());
        edits.set_excluded("a".into(), true);
        assert_eq!(picks(&edits), ["keep"]);
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

    fn photos(n: i64) -> Vec<Asset> {
        (0..n)
            .map(|i| {
                asset(
                    &format!("a{i:02}"),
                    1_714_557_600 + i * 60,
                    AssetKind::Photo,
                )
            })
            .collect()
    }

    fn hero_secs(plan: &Plan, clusters: &HashMap<String, String>) -> f64 {
        estimate_hero_secs(&plan.hero, |id| {
            clusters.get(id).map_or(UNGROUPED, String::as_str)
        })
    }

    #[test]
    fn hero_fills_length_keeping_story_order() {
        let assets = photos(130);
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
        let none = HashMap::new();
        let plan = build_plan_with_length(&assets, &none, &scores, &[], FilmLength::Secs(120.0));
        let secs = hero_secs(&plan, &none);
        assert!(
            plan.hero.len() < 130 && secs <= 120.0 && secs > 110.0,
            "fills ~120 s: {} entries, {secs:.1} s",
            plan.hero.len()
        );
        assert!(
            plan.hero.iter().all(|e| e.asset_id != "a00"),
            "lowest scorer dropped first"
        );
        let ids: Vec<&str> = plan.hero.iter().map(|e| e.asset_id.as_str()).collect();
        let mut sorted = ids.clone();
        sorted.sort_by_key(|id| id[1..].parse::<usize>().unwrap());
        assert_eq!(ids, sorted, "kept entries stay in story order");
    }

    #[test]
    fn longer_film_shows_more() {
        let assets = photos(130);
        let none = HashMap::new();
        let len = |secs| {
            build_plan_with_length(&assets, &none, &HashMap::new(), &[], FilmLength::Secs(secs))
                .hero
                .len()
        };
        assert!(len(60.0) < len(120.0));
        assert_eq!(len(3600.0), 130, "a long enough film keeps everything");
    }

    #[test]
    fn hero_length_keeps_every_chapter() {
        let assets = photos(111);
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
        let plan = build_plan_with_length(&assets, &clusters, &scores, &[], FilmLength::Secs(60.0));
        assert!(plan.hero.len() < 111);
        assert!(
            plan.hero.iter().any(|e| e.asset_id == "a110"),
            "lower-scored chapter keeps its best shot"
        );
    }

    #[test]
    fn big_chapters_get_more_time_but_not_all() {
        // Day 1 has 100 photos, Day 2 has 4: Day 2 is shown in full well
        // before Day 1 is, and Day 1 still gets the larger share.
        let assets = photos(104);
        let clusters: HashMap<String, String> = (0..104)
            .map(|i| {
                let day = if i < 100 { "Day 1" } else { "Day 2" };
                (format!("a{i:02}"), day.to_string())
            })
            .collect();
        let plan = build_plan_with_length(
            &assets,
            &clusters,
            &HashMap::new(),
            &[],
            FilmLength::Secs(60.0),
        );
        let day2 = plan
            .hero
            .iter()
            .filter(|e| clusters[&e.asset_id] == "Day 2")
            .count();
        let day1 = plan.hero.len() - day2;
        assert!(day1 > day2 && day2 >= 2, "day1={day1} day2={day2}");
    }

    #[test]
    fn auto_length_scales_with_trip_days() {
        assert_eq!(auto_film_secs(0), AUTO_MIN_FILM_SECS);
        assert_eq!(auto_film_secs(1), AUTO_MIN_FILM_SECS);
        assert!((auto_film_secs(7) - 264.6).abs() < 0.1);
        assert!((auto_film_secs(34) - 583.1).abs() < 0.1);
        assert_eq!(auto_film_secs(500), AUTO_MAX_FILM_SECS);
    }

    #[test]
    fn parses_film_lengths() {
        assert_eq!(parse_film_length("auto"), Ok(FilmLength::Auto));
        assert_eq!(parse_film_length("600"), Ok(FilmLength::Secs(600.0)));
        assert_eq!(parse_film_length("10m"), Ok(FilmLength::Secs(600.0)));
        assert_eq!(parse_film_length(" 4M30s "), Ok(FilmLength::Secs(270.0)));
        assert_eq!(parse_film_length("90s"), Ok(FilmLength::Secs(90.0)));
        for bad in ["", "ten", "10h", "5", "2h", "-60", "NaN", "m", "1m2"] {
            assert!(parse_film_length(bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn estimate_matches_renderer_grouping() {
        let still = |id: &str| PlanEntry {
            asset_id: id.into(),
            content_hash: None,
            size_bytes: 0,
            mtime: 0,
            rendition: Rendition::Still,
        };
        let clip = |id: &str, len: f64| PlanEntry {
            rendition: Rendition::Clip {
                trim_start: 0.0,
                trim_end: len,
            },
            ..still(id)
        };
        let ch = |id: &str| if id.starts_with('x') { "X" } else { "Y" };
        // x1..x5 -> collage(4) + solo; clip breaks the run; 1 s clip pads to 2.5.
        let entries = [
            still("x1"),
            still("x2"),
            still("x3"),
            still("x4"),
            still("x5"),
            clip("y1", 6.0),
            clip("y2", 1.0),
            still("y3"),
        ];
        let segs = 4 + 5;
        let want =
            EST_FIXED_SECS + 6.0 + 4.0 + 6.0 + 2.5 + 4.0 - EST_CUT_OVERLAP_SECS * (segs - 1) as f64;
        assert!((estimate_hero_secs(&entries, ch) - want).abs() < 1e-9);
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
