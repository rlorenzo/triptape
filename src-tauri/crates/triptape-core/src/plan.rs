//! Stage 7 — Plan: one versioned AI Plan JSON for all three renderers.
//!
//! Hero shot list in story order, 3–5 reel selects varied by day/place,
//! chapters by day/place, template captions, map polyline + day markers,
//! music pick with rationale. Per-entry identity (asset id, content hash,
//! size, mtime, rendition) feeds the pre-render source-integrity check.
//! Sec 5.4 degradation eligibility lives here too.

use crate::clip_window::MAX_WINDOW_SECS;
use crate::cluster::{gps_outliers, story_key, story_order};
use crate::types::{
    distinct_assets, Asset, AssetKind, Chapter, MusicPick, Plan, PlanEntry, Rendition, Score,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

/// Weekday + date as shown to viewers, `Wed, May 1`.
const DAY_FORMAT: &str = "%a, %b %-d";

/// Factual caption dateline, `Wed, May 1 · 10:00 AM` (UTC), used by the
/// local caption fallback when no place is known.
pub fn dateline(dt: &chrono::DateTime<chrono::Utc>) -> String {
    format!("{} · {}", dt.format(DAY_FORMAT), dt.format("%-I:%M %p"))
}

/// 2: `captions` holds only the user's words (no auto place captions).
/// 3: a hero entry carries a `window` index, so one long clip can appear as
/// several sibling entries (one per scene). Version 2 plans load as is:
/// every entry is window 0, exactly what they held.
pub const PLAN_VERSION: u32 = 3;
/// Last plan version whose `captions` copied the chapter place onto photos.
const AUTO_PLACE_CAPTIONS_VERSION: u32 = 1;

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

/// Most stills shown together in one hero collage segment.
pub const MAX_COLLAGE: usize = 4;
/// Photos at least this wide (or this tall, inverted) pan instead of
/// sitting as a thin strip: width/height >= 2 or <= 1/2.
pub const PANO_MIN_ASPECT: f64 = 2.0;
/// Pan hold bounds, seconds. Base hold grows with elongation
/// (`PANO_BASE_SECS + PANO_SECS_PER_ASPECT * e`) within the first two.
pub const PANO_MIN_SECS: f64 = 6.0;
pub const PANO_MAX_SECS: f64 = 8.0;
const PANO_BASE_SECS: f64 = 4.5;
const PANO_SECS_PER_ASPECT: f64 = 0.8;
/// Fastest allowed average pan, in frame-widths per second (the eased
/// pan peaks at 1.5x this mid-travel). Very wide images
/// first hold longer (up to [`PANO_LONG_SECS`]), then pan less of their
/// width (centered) rather than whip across it or hold past the cap.
pub const PANO_MAX_SPEED: f64 = 0.3;
/// Longest a panorama stays on screen.
pub const PANO_LONG_SECS: f64 = 10.0;
/// Output frame aspect the pan is measured against.
const FRAME_ASPECT: f64 = 16.0 / 9.0;
/// Minimum on-screen time for a hero segment; shorter clips are
/// freeze-padded up to it.
pub const MIN_SEGMENT_SECS: f64 = 2.5;
/// Longest a single hero clip segment runs.
pub const MAX_CLIP_SECS: f64 = 30.0;

// Hero timing model. The renderer (`triptape-media` render_video) uses the
// three constants above directly; the rest mirror its `VideoOptions`
// defaults, collage hold, transition overlaps and closing cards so the plan
// can fill a time budget. A test in render_video checks this model against
// the renderer's own transition assignment. Keep in sync.
/// Solo still hold.
const EST_STILL_SECS: f64 = 4.0;
/// Title card hold.
const EST_TITLE_SECS: f64 = 2.5;
/// Opening route map hold (only with a GPS route).
const EST_MAP_SECS: f64 = 3.0;
/// Closing places card hold (only when a chapter has a named place).
const EST_PLACES_SECS: f64 = 4.5;
/// Overlap of the first cut (circleopen) and the last (circleclose).
const EST_BOOKEND_OVERLAP_SECS: f64 = 0.7;
/// Overlaps at chapter breaks and other cuts into or out of a card: the
/// renderer cycles fadeblack (0.8 s), smoothleft, smoothleft (0.5 s each).
const EST_BREAK_OVERLAPS_SECS: [f64; 3] = [0.8, 0.5, 0.5];
/// Overlap of an ordinary cut: 0.5 s, plus an occasional 0.7 s circleopen
/// in the transition rotation.
const EST_PLAIN_OVERLAP_SECS: f64 = 0.54;
/// Hero time is dealt to chapters in proportion to `size^0.75`: a chapter
/// with twice the photos gets ~1.7x the time (the old square root gave
/// 1.4x, which starved big days like a theme-park day to a few shots).
const CHAPTER_TIME_EXPONENT: f64 = 0.75;
/// A requested film length is a minimum: the plan fills to this fraction
/// over it, absorbing small errors in the timing model.
pub const LENGTH_OVERSHOOT: f64 = 0.03;
/// Two shots this close in time are one moment when they also look alike.
const SAME_MOMENT_SECS: i64 = 120;
/// Shots this close in time are one moment (a burst), looks aside.
const SAME_MOMENT_BURST_SECS: i64 = 5;
/// Looks-alike bound (pHash Hamming distance) for [`SAME_MOMENT_SECS`].
/// Looser than the dupe-collapse bound: these are different frames of the
/// same moment, not retakes.
const SAME_MOMENT_PHASH_DISTANCE: u32 = 14;
/// Weight of an asset's percentile within its chapter in its technical
/// quality (the rest is the absolute quality). Kept small so story signals
/// (people, captions) still outrank a few percentiles of sharpness.
const REL_QUALITY_WEIGHT: f32 = 0.2;
/// Weight of an asset's capped colourfulness percentile within its
/// chapter, on the same scale as [`REL_QUALITY_WEIGHT`]'s percentile term
/// but centred (the chapter median adds nothing). Small: colour breaks ties
/// between similar frames, it never outranks people or sharpness.
const COLOUR_WEIGHT: f32 = 0.10;
/// Live Photos per chapter that keep their motion in the hero film; the
/// rest play as stills so they can form collages.
pub const MOTION_PER_CHAPTER: usize = 1;

/// Whether a display aspect (width / height) is a panorama: >= 2:1, or
/// a tall one <= 1:2.
pub fn is_panorama(aspect: f64) -> bool {
    aspect.is_finite()
        && aspect > 0.0
        && (aspect >= PANO_MIN_ASPECT || aspect <= 1.0 / PANO_MIN_ASPECT)
}

/// How a panorama pans. `hold_secs` is the segment length; `coverage` is
/// the fraction (0..=1] of the image's overflow beyond the frame that the
/// pan travels (less than 1 only for extremely long images, centered).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanoMotion {
    pub hold_secs: f64,
    pub coverage: f64,
}

/// Frame-widths (wide) or frame-heights (tall) of a panorama beyond the
/// landscape output frame, along the pan axis. A wide image is scaled to
/// the frame height, a tall one to the frame width.
fn pano_overflow(aspect: f64) -> f64 {
    let along = if aspect >= 1.0 {
        aspect / FRAME_ASPECT
    } else {
        FRAME_ASPECT / aspect
    };
    (along - 1.0).max(0.0)
}

/// Fraction of the overflow a pan of `secs` may travel while keeping its
/// average speed within [`PANO_MAX_SPEED`].
pub fn pano_coverage(aspect: f64, secs: f64) -> f64 {
    let overflow = pano_overflow(aspect);
    if overflow > 0.0 {
        (PANO_MAX_SPEED * secs / overflow).min(1.0)
    } else {
        1.0
    }
}

/// Hold length and travel for a panorama of this display aspect.
pub fn pano_motion(aspect: f64) -> PanoMotion {
    let elongation = if aspect >= 1.0 { aspect } else { 1.0 / aspect };
    let overflow = pano_overflow(aspect);
    let mut hold =
        (PANO_BASE_SECS + PANO_SECS_PER_ASPECT * elongation).clamp(PANO_MIN_SECS, PANO_MAX_SECS);
    if overflow / hold > PANO_MAX_SPEED {
        hold = (overflow / PANO_MAX_SPEED).min(PANO_LONG_SECS);
    }
    PanoMotion {
        hold_secs: hold,
        coverage: pano_coverage(aspect, hold),
    }
}

/// Whether a caption counts: any non-blank text. A captioned still plays
/// solo so its words describe only that photo, never join a collage; the
/// renderer and [`estimate_hero_secs`] both decide through this.
pub fn has_caption(caption: Option<&str>) -> bool {
    caption.is_some_and(|c| !c.trim().is_empty())
}

/// Hold for a collage of `n` photos (1 = a solo still): `still_secs`
/// scaled so more photos get more time (n=2: 1.25x, 3: 1.5x, 4: 1.75x).
pub fn collage_hold_secs(n: usize, still_secs: f64) -> f64 {
    still_secs * (0.75 + 0.25 * n.max(1) as f64)
}

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

/// Which fixed cards the renderer will add around the hero entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeroCards {
    /// Opening and closing route maps (the plan has a GPS route).
    pub route: bool,
    /// Chapter stops the closing map travels between; fewer than two keep
    /// the static closing card (see [`crate::route_stops::closing_map_secs`]).
    pub route_stops: usize,
    /// Closing places card (some chapter has a named place).
    pub places: bool,
}

/// Screen seconds one entry takes on its own (a still joining a collage
/// costs less than this; see [`estimate_hero_secs`]).
fn solo_secs(e: &PlanEntry, pano_secs_of: impl Fn(&str) -> Option<f64>) -> f64 {
    match e.rendition {
        Rendition::Still => pano_secs_of(&e.asset_id).unwrap_or(EST_STILL_SECS),
        Rendition::Motion {
            trim_start,
            trim_end,
        }
        | Rendition::Clip {
            trim_start,
            trim_end,
        } => (trim_end - trim_start).clamp(MIN_SEGMENT_SECS, MAX_CLIP_SECS),
    }
}

/// What one rendered segment is, for transition overlaps.
#[derive(Clone, Copy, PartialEq)]
enum Seg<'c> {
    Title,
    Card,
    Chapter(&'c str),
}

/// Estimated rendered seconds for hero entries in story order, as the
/// renderer builds them: the fixed cards around the entries, consecutive
/// same-chapter stills grouped into collages, clips padded to
/// [`MIN_SEGMENT_SECS`], and every cut overlapped by the transition the
/// renderer assigns it (opening and closing cut 0.7 s, chapter breaks and
/// card cuts cycling 0.8/0.5/0.5 s, others ~0.54 s). Panoramas and
/// captioned stills (`captioned`) always play solo.
pub fn estimate_hero_secs<'e, 'c>(
    entries: impl IntoIterator<Item = &'e PlanEntry>,
    chapter_of: impl Fn(&str) -> &'c str,
    pano_secs_of: impl Fn(&str) -> Option<f64>,
    captioned: impl Fn(&str) -> bool,
    cards: HeroCards,
) -> f64 {
    let mut segs: Vec<(Seg<'c>, f64)> = vec![(Seg::Title, EST_TITLE_SECS)];
    if cards.route {
        segs.push((Seg::Card, EST_MAP_SECS));
    }
    let first_hero = segs.len();
    let mut run: Option<(&str, usize)> = None; // (chapter, stills in collage)
    for e in entries {
        let chapter = chapter_of(&e.asset_id);
        let pano_secs = pano_secs_of(&e.asset_id);
        match e.rendition {
            // A panorama (with its own pan hold) or a captioned still is
            // always solo, and breaks the run.
            Rendition::Still if pano_secs.is_some() || captioned(&e.asset_id) => {
                segs.push((Seg::Chapter(chapter), pano_secs.unwrap_or(EST_STILL_SECS)));
                run = None;
            }
            Rendition::Still => match &mut run {
                Some((c, n)) if *c == chapter && *n < MAX_COLLAGE => {
                    // Growing a solo still into a collage, or a collage by one.
                    if let Some(last) = segs.last_mut() {
                        last.1 = collage_hold_secs(*n + 1, EST_STILL_SECS);
                    }
                    *n += 1;
                }
                _ => {
                    segs.push((Seg::Chapter(chapter), EST_STILL_SECS));
                    run = Some((chapter, 1));
                }
            },
            Rendition::Motion { .. } | Rendition::Clip { .. } => {
                segs.push((Seg::Chapter(chapter), solo_secs(e, &pano_secs_of)));
                run = None;
            }
        }
    }
    if segs.len() == first_hero {
        // No entries: the renderer refuses to build a film from cards alone.
        return segs.iter().map(|s| s.1).sum();
    }
    if cards.route {
        segs.push((
            Seg::Card,
            crate::route_stops::closing_map_secs(cards.route_stops),
        ));
    }
    if cards.places {
        segs.push((Seg::Card, EST_PLACES_SECS));
    }
    let n = segs.len();
    let mut overlap = 0.0;
    let mut breaks = 0usize;
    // A cut into or out of a card, or between chapters, is a chapter break.
    let is_break = |a: Seg, b: Seg| a == Seg::Card || b == Seg::Card || (a != Seg::Title && a != b);
    for i in 1..n {
        // The opening cut and (when there is a middle) the closing cut.
        let bookend = i == 1 || (i == n - 1 && n > 2);
        overlap += if bookend {
            EST_BOOKEND_OVERLAP_SECS
        } else if is_break(segs[i - 1].0, segs[i].0) {
            breaks += 1;
            EST_BREAK_OVERLAPS_SECS[(breaks - 1) % EST_BREAK_OVERLAPS_SECS.len()]
        } else {
            EST_PLAIN_OVERLAP_SECS
        };
    }
    segs.iter().map(|s| s.1).sum::<f64>() - overlap
}

/// How much hero film to plan: picks keep coming until none fits under
/// `ceiling_secs`, so the film lands as close under it as the shots allow.
#[derive(Debug, Clone, Copy)]
struct HeroBudget {
    ceiling_secs: f64,
    cards: HeroCards,
}

/// Pick hero entries (given in story order) to fill `budget`.
///
/// Every chapter first gets its best entry (dropping the weakest chapters'
/// picks only if even those overrun the ceiling). Remaining time is dealt
/// out one entry at a time to the chapter with the least estimated screen
/// time relative to `size^CHAPTER_TIME_EXPONENT`, taking that chapter's
/// best remaining entry, so big days get more screen time without crowding
/// out small ones.
///
/// An entry that would overrun the ceiling is skipped and the chapter's
/// next-best, cheaper entry tried, so the film fills the budget instead of
/// stopping at the first clip that does not fit. While better options
/// remain, an entry that `similar` calls the same moment as one the chapter
/// already has is passed over, and so is a further window of a clip the
/// chapter already shows (sibling windows are different scenes, so not the
/// same moment, but they would play back to back); when every chapter has
/// run dry the budget is plentiful, and a second pass allows both. Keeps
/// story order.
fn select_hero_by_budget<'a>(
    cands: Vec<PlanEntry>,
    chapter_of: impl Fn(&str) -> &'a str + Copy,
    score_of: impl Fn(&PlanEntry) -> f32,
    similar: impl Fn(&str, &str) -> bool,
    pano_secs_of: impl Fn(&str) -> Option<f64> + Copy,
    captioned: impl Fn(&str) -> bool + Copy,
    budget: HeroBudget,
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
            score_of(&cands[y])
                .total_cmp(&score_of(&cands[x]))
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
            pano_secs_of,
            captioned,
            budget.cards,
        )
    };
    // Every chapter's best entry, weakest dropped first if over budget
    // (the strongest always stays, so the film is never empty).
    let mut reps: Vec<usize> = chapters.iter().map(|(_, m)| m[0]).collect();
    for &r in &reps {
        picked[r] = true;
    }
    reps.sort_by(|&x, &y| {
        score_of(&cands[x])
            .total_cmp(&score_of(&cands[y]))
            .then(y.cmp(&x))
    });
    if let Some((_, weaker)) = reps.split_last() {
        for &r in weaker {
            if estimate(&picked) <= budget.ceiling_secs {
                break;
            }
            picked[r] = false;
        }
    }
    // Deal the rest of the budget across chapters by estimated screen time
    // (a clip costs more than a still joining a collage).
    let mut taken: Vec<Vec<usize>> = chapters
        .iter()
        .map(|(_, m)| m.iter().copied().filter(|&i| picked[i]).collect())
        .collect();
    let mut secs: Vec<f64> = taken
        .iter()
        .map(|t| t.iter().map(|&i| solo_secs(&cands[i], pano_secs_of)).sum())
        .collect();
    let mut total = estimate(&picked);
    for allow_similar in [false, true] {
        let mut closed = vec![false; chapters.len()];
        loop {
            let next = chapters
                .iter()
                .enumerate()
                .filter(|(ci, _)| !closed[*ci])
                .min_by(|(a, (_, ma)), (b, (_, mb))| {
                    let share =
                        |ci: usize, n: usize| secs[ci] / (n as f64).powf(CHAPTER_TIME_EXPONENT);
                    share(*a, ma.len())
                        .total_cmp(&share(*b, mb.len()))
                        .then(a.cmp(b))
                })
                .map(|(ci, _)| ci);
            let Some(ci) = next else { break };
            // First entry (best-first) that is allowed and still fits.
            let mut fitted = None;
            for &i in &chapters[ci].1 {
                if picked[i]
                    || (!allow_similar
                        && taken[ci].iter().any(|&t| {
                            cands[i].asset_id == cands[t].asset_id
                                || similar(&cands[i].asset_id, &cands[t].asset_id)
                        }))
                {
                    continue;
                }
                picked[i] = true;
                let grown = estimate(&picked);
                if grown <= budget.ceiling_secs {
                    fitted = Some((i, grown));
                    break;
                }
                picked[i] = false;
            }
            match fitted {
                Some((i, grown)) => {
                    secs[ci] += (grown - total).max(0.0);
                    total = grown;
                    taken[ci].push(i);
                }
                None => closed[ci] = true,
            }
        }
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
    /// Assets the user pinned into the hero film ("Add to film"), oldest
    /// first. Mutually exclusive with `excluded`. Hero only: reels and the
    /// book flow are untouched. See [`apply_edits`] for placement and the
    /// displacement rule.
    #[serde(default)]
    pub included: Vec<String>,
    /// Hero/reel entry asset id -> replacement asset id (same chapter), in
    /// the order the user made them: a later swap can replace an earlier
    /// swap's replacement, so this is a list rather than a map.
    #[serde(default)]
    pub swaps: Vec<(String, String)>,
    /// Asset id of any chapter member -> user place label for that chapter.
    #[serde(default)]
    pub place_overrides: BTreeMap<String, String>,
    /// Asset id -> user caption (beats the caption embedded in the file).
    #[serde(default)]
    pub caption_overrides: BTreeMap<String, String>,
}

impl PlanEdits {
    pub fn is_empty(&self) -> bool {
        self.excluded.is_empty()
            && self.included.is_empty()
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
    /// Removing also un-pins the asset.
    pub fn set_excluded(&mut self, asset_id: String, excluded: bool) {
        self.excluded.retain(|id| *id != asset_id);
        if excluded {
            self.included.retain(|id| *id != asset_id);
            self.excluded.push(asset_id);
        }
    }

    /// Pin an asset into the hero film, or un-pin it. Pinning also
    /// restores it if it was removed.
    pub fn set_included(&mut self, asset_id: String, included: bool) {
        self.included.retain(|id| *id != asset_id);
        if included {
            self.excluded.retain(|id| *id != asset_id);
            self.included.push(asset_id);
        }
    }

    /// Replace a pick, superseding the earlier swap of the same pick. That
    /// swap stays if a later swap or a pin brought the asset back: replay
    /// needs it to vacate its original slot first, and its replacement is a
    /// separate pick the user still holds. Using an excluded asset as the
    /// replacement restores it. Swapping a pinned asset moves its pin to the
    /// replacement (keeping its place in pin order), so replay neither
    /// re-adds the swapped-out pin nor misses a replacement for a pin the
    /// base hero never held; the swap record still covers reels and base
    /// picks.
    pub fn swap(&mut self, from: String, to: String) {
        let pin = self.included.iter().position(|id| *id == from);
        if let Some(i) = self.swaps.iter().rposition(|(f, _)| *f == from) {
            if pin.is_none() && !self.swaps[i + 1..].iter().any(|(_, t)| *t == from) {
                self.swaps.remove(i);
            }
        }
        if let Some(i) = pin {
            self.included[i] = to.clone();
            let mut seen = false;
            self.included
                .retain(|id| *id != to || !std::mem::replace(&mut seen, true));
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

/// Rename a chapter.
fn rename_chapter(ch: &mut Chapter, label: &str) {
    ch.place = Some(label.to_string());
    ch.title = label.to_string();
}

/// Migrate a plan saved before captions became the user's words only: its
/// captions mixed the chapter place with embedded captions, so rebuild them
/// from the assets' own `user_caption`. Story-editor overrides are applied
/// on top by [`apply_edits`], as for current plans.
fn with_user_captions_only(plan: &Plan, by_id: &HashMap<&str, &Asset>) -> Plan {
    let mut out = plan.clone();
    out.captions = plan
        .chapters
        .iter()
        .flat_map(|c| &c.asset_ids)
        .filter_map(|id| Some((id.clone(), by_id.get(id.as_str())?.user_caption.clone()?)))
        .collect();
    out.plan_version = PLAN_VERSION;
    out
}

/// Edits address assets, not entries. A long multi-scene clip shows as
/// sibling hero entries `(asset id, window)`; every edit on its asset id
/// covers all of them: removing it removes every window, swapping it
/// replaces all windows with the one replacement (in the first window's
/// slot), and its caption is shared. Pinning or swapping in such a clip
/// adds a single entry with its best window. Single-window assets behave as
/// they always did.
///
/// Apply user edits to a stored plan. Pure and deterministic:
/// place overrides first (they rename whole chapters), then swaps,
/// then pinned inclusions, then exclusions, then caption overrides on top.
///
/// Inclusions (hero only) put each pinned asset into `hero` at its
/// story-order position (capture time, then id: [`story_order`]) with the
/// same rendition rules as a swap-in (clips keep their window, Live
/// Photos are stills). Pins must belong to this plan's chapters and are
/// skipped when already picked. To keep the film at the length the user
/// asked for, a pin displaces the lowest-scored unpinned hero entry of its
/// own chapter (pins and swap replacements are never displaced); a chapter
/// with nothing displaceable just grows the film.
pub fn apply_edits(plan: &Plan, assets: &[Asset], edits: &PlanEdits) -> Plan {
    let by_id: HashMap<&str, &Asset> = assets.iter().map(|a| (a.id.as_str(), a)).collect();
    let legacy;
    let plan = if plan.plan_version <= AUTO_PLACE_CAPTIONS_VERSION {
        legacy = with_user_captions_only(plan, &by_id);
        &legacy
    } else {
        plan
    };
    if edits.is_empty() {
        return plan.clone();
    }
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
            rename_chapter(ch, label);
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
        for (is_hero, picks) in [(true, &mut out.hero), (false, &mut out.reel_selects)] {
            if picks.iter().any(|e| e.asset_id == *to) {
                picks.retain(|e| e.asset_id != *from);
            } else if let Some(entry) = picks.iter_mut().find(|e| e.asset_id == *from) {
                *entry = if is_hero {
                    user_hero_entry(asset)
                } else {
                    plan_entry(asset, asset.score.as_ref())
                };
                // A multi-scene clip is one pick: the replacement takes the
                // slot of its first window and the other windows go.
                picks.retain(|e| e.asset_id != *from);
            }
        }
    }

    include_pins(&mut out, &by_id, edits, &dead);

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

/// Film entry for an asset the user put there (swap-in or pin). The film
/// shows Live Photos as stills unless the plan picked them for motion, so a
/// user pick is a still; clips keep their window.
fn user_hero_entry(asset: &Asset) -> PlanEntry {
    let mut entry = plan_entry(asset, asset.score.as_ref());
    if matches!(entry.rendition, Rendition::Motion { .. }) {
        entry.rendition = Rendition::Still;
    }
    entry
}

/// Insert the user's pinned assets into the hero (see [`apply_edits`]).
fn include_pins(
    out: &mut Plan,
    by_id: &HashMap<&str, &Asset>,
    edits: &PlanEdits,
    dead: &HashSet<&str>,
) {
    // Pins and swap replacements are the user's picks: never displaced.
    let pinned: HashSet<&str> = edits
        .included
        .iter()
        .chain(edits.swaps.iter().map(|(_, to)| to))
        .map(String::as_str)
        .collect();
    let score_of = |id: &str| {
        by_id
            .get(id)
            .and_then(|a| a.score.as_ref())
            .map_or(DEFAULT_SCORE, |s| s.total)
    };
    // Unknown assets sort last.
    let key_of = |id: &str| by_id.get(id).map_or((i64::MAX, ""), |a| story_key(a));
    for id in &edits.included {
        let Some(asset) = by_id.get(id.as_str()) else {
            continue;
        };
        let Some(chapter) = out.chapter_of(id) else {
            continue;
        };
        if dead.contains(id.as_str()) || out.hero.iter().any(|e| e.asset_id == *id) {
            continue;
        }
        let same_chapter: HashSet<&str> = chapter.asset_ids.iter().map(String::as_str).collect();
        let weakest = out
            .hero
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                let id = e.asset_id.as_str();
                !pinned.contains(id) && same_chapter.contains(id)
            })
            .min_by(|(_, a), (_, b)| score_of(&a.asset_id).total_cmp(&score_of(&b.asset_id)))
            .map(|(i, _)| i);
        if let Some(i) = weakest {
            out.hero.remove(i);
        }
        // Swaps keep their slot, so the hero may be out of story order:
        // land right after the last earlier capture, so nothing captured
        // before the pin plays after it.
        let key = story_key(asset);
        let at = out
            .hero
            .iter()
            .rposition(|e| key_of(&e.asset_id) < key)
            .map_or(0, |i| i + 1);
        out.hero.insert(at, user_hero_entry(asset));
    }
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

/// Weight of a window's own quality when ranking sibling windows of a clip.
/// Small: it only breaks ties between windows of one clip.
const WINDOW_TIEBREAK: f32 = 0.01;

/// [`build_plan_with_overrides`] with a chosen hero film length.
pub fn build_plan_with_length(
    assets: &[Asset],
    cluster_of: &HashMap<String, String>,
    scores: &HashMap<String, Score>,
    overrides: &[PlaceOverride],
    length: FilmLength,
) -> Plan {
    let ordered = story_order(assets);
    let chapter_label = |id: &str| cluster_of.get(id).map_or(UNGROUPED, String::as_str);
    // Story score per asset: technical quality relative to its chapter plus
    // the story bonuses (people, caption, clip). Assets without a score keep
    // the neutral default.
    let story_scores = relative_scores(&ordered, chapter_label, scores);
    let score_of = |id: &str| story_scores.get(id).copied().unwrap_or(DEFAULT_SCORE);
    let entry = |a: &Asset| plan_entry(a, score_for_asset(scores, a));
    let local_date = |a: &Asset| crate::metadata::local_date(a.taken_at?, a.tz_offset_secs);

    // Group by cluster label in story order (drives chapters and reels).
    let mut groups: Vec<(&str, Vec<&Asset>)> = vec![];
    let mut group_idx: HashMap<&str, usize> = HashMap::new();
    for a in &ordered {
        let label = chapter_label(&a.id);
        let i = *group_idx.entry(label).or_insert_with(|| {
            groups.push((label, vec![]));
            groups.len() - 1
        });
        groups[i].1.push(a);
    }

    // Chapters: one per cluster label, in story order, named by place
    // when the chapter's GPS medoid lands near a populated place
    // (bundled gazetteer, never a network lookup). The day is shown once
    // per chapter (overlays, book day-marker pages) — photos inside a
    // chapter need no per-photo timestamp.
    let trip_start = ordered.iter().filter_map(|a| local_date(a)).min();
    let chapters: Vec<Chapter> = groups
        .iter()
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

    // Map polyline: GPS fixes in story order, minus fixes that imply
    // impossible speed. Capture times separate those from real flights.
    let outliers = gps_outliers(assets);
    let map_polyline: Vec<(f64, f64)> = ordered
        .iter()
        .filter(|a| !outliers.contains(&a.id))
        .filter_map(|a| a.gps)
        .collect();

    // Hero: story order, one entry per identical content hash (pHash
    // near-dupe stacks are resolved upstream via cluster picks). Live
    // Photos play as stills so they can form collages, except the best one
    // of each chapter, which keeps its motion.
    let motion_ok = motion_picks(&ordered, chapter_label, &score_of);
    let mut seen_hash: HashSet<&str> = HashSet::new();
    let mut hero: Vec<PlanEntry> = ordered
        .iter()
        .filter(|a| {
            a.content_hash
                .as_deref()
                .is_none_or(|h| seen_hash.insert(h))
        })
        .flat_map(|a| {
            let mut entries = plan_entries(a, score_for_asset(scores, a));
            for e in &mut entries {
                if !motion_ok.contains(a.id.as_str())
                    && matches!(e.rendition, Rendition::Motion { .. })
                {
                    e.rendition = Rendition::Still;
                }
            }
            entries
        })
        .collect();
    // The threshold counts assets: a clip's extra windows are not extra assets.
    if distinct_assets(&hero) > HERO_TRIM_ABOVE {
        hero.retain(|e| score_of(&e.asset_id) >= HERO_MIN_SCORE);
    }
    let target_secs = match length {
        FilmLength::Secs(s) => s,
        FilmLength::Auto => {
            // Inclusive calendar span, so days without captures still count.
            let dates = || ordered.iter().filter_map(|a| local_date(a));
            let span = match (dates().min(), dates().max()) {
                (Some(first), Some(last)) => (last - first).num_days() as usize + 1,
                _ => 1,
            };
            auto_film_secs(span)
        }
    };
    let budget = HeroBudget {
        // A requested length is a minimum, so aim a little over it.
        ceiling_secs: match length {
            FilmLength::Secs(_) => target_secs * (1.0 + LENGTH_OVERSHOOT),
            FilmLength::Auto => target_secs,
        },
        cards: HeroCards {
            route: !map_polyline.is_empty(),
            route_stops: crate::route_stops::animated_stop_count(
                &crate::route_stops::chapter_route_stops(&chapters, assets, &map_polyline),
            ),
            places: chapters.iter().any(|c| c.place.is_some()),
        },
    };
    let by_id: HashMap<&str, &Asset> = ordered.iter().map(|a| (a.id.as_str(), *a)).collect();
    let similar = |x: &str, y: &str| match (by_id.get(x), by_id.get(y)) {
        (Some(a), Some(b)) => same_moment(a, b),
        _ => false,
    };
    // Panoramas pan solo for longer than a plain still; the estimator
    // needs each one's hold to fill the budget honestly.
    let pano_hold: HashMap<&str, f64> = assets
        .iter()
        .filter_map(|a| {
            let asp = a.display_aspect().filter(|&x| is_panorama(x))?;
            Some((a.id.as_str(), pano_motion(asp).hold_secs))
        })
        .collect();
    let pano_secs_of = |id: &str| pano_hold.get(id).copied();
    // Sibling windows of one clip share its story score; the better window
    // wins the near-tie, so a tight budget keeps the clip's best scene.
    let entry_score = |e: &PlanEntry| {
        let windows = by_id
            .get(e.asset_id.as_str())
            .and_then(|a| score_for_asset(scores, a))
            .map(|s| &s.clip_windows[..])
            .filter(|w| w.len() > 1);
        let bonus = windows
            .and_then(|w| w.get(e.window as usize))
            .map_or(0.0, |w| WINDOW_TIEBREAK * w.quality);
        score_of(&e.asset_id) + bonus
    };
    // Place windows before selection: the estimate groups stills into
    // collages by playback order, and selection keeps candidate order, so
    // every budget check sees the order that will be rendered.
    place_sibling_windows(&mut hero, &by_id, chapter_label);
    let hero = select_hero_by_budget(
        hero,
        chapter_label,
        entry_score,
        similar,
        pano_secs_of,
        |id| has_caption(by_id.get(id).and_then(|a| a.user_caption.as_deref())),
        budget,
    );

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
            reel_selects.push(entry(a));
        }
    }
    // Pass 2: fill remaining slots with the next-best qualifying assets.
    for a in &ranked {
        if reel_selects.len() >= MAX_REELS {
            break;
        }
        if !picked_ids.contains(a.id.as_str()) && qualifies(a, reel_selects.len()) {
            picked_ids.insert(&a.id);
            reel_selects.push(entry(a));
        }
    }
    let story_pos: HashMap<&str, usize> = ordered
        .iter()
        .enumerate()
        .map(|(i, a)| (a.id.as_str(), i))
        .collect();
    reel_selects.sort_by_key(|e| story_pos[e.asset_id.as_str()]);

    // Captions are the user's own words only: a caption embedded in the file
    // (EXIF/XMP) or typed in the Story editor. The chapter place is never
    // copied onto each photo: the film's lower third already names it, and
    // the book and reels fall back to the chapter place at export time.
    let captions: HashMap<String, String> = ordered
        .iter()
        .filter_map(|a| Some((a.id.clone(), a.user_caption.clone()?)))
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

/// Move each further window of a clip to where it was captured: the clip's
/// capture time plus the window's start, within its chapter, so photos taken
/// while the clip ran play between its windows instead of the windows
/// running back to back. Windows with no capture time stay put. A clip's
/// windows keep their order and never move before its earlier windows.
fn place_sibling_windows<'a>(
    hero: &mut Vec<PlanEntry>,
    by_id: &HashMap<&str, &Asset>,
    chapter_of: impl Fn(&str) -> &'a str,
) {
    // Playback moment of an entry on the capture timeline.
    let moment = |e: &PlanEntry| {
        let start = match e.rendition {
            Rendition::Clip { trim_start, .. } if e.window > 0 => trim_start,
            _ => 0.0,
        };
        let taken = by_id.get(e.asset_id.as_str())?.taken_at?;
        Some(taken as f64 + start)
    };
    let mut kept = Vec::with_capacity(hero.len());
    let mut siblings = vec![];
    for e in std::mem::take(hero) {
        match moment(&e).filter(|_| e.window > 0) {
            Some(at) => siblings.push((at, e)),
            None => kept.push(e),
        }
    }
    for (at, e) in siblings {
        let chapter = chapter_of(&e.asset_id);
        let in_chapter = |k: &PlanEntry| chapter_of(&k.asset_id) == chapter;
        let after_sibling = kept
            .iter()
            .rposition(|k| k.asset_id == e.asset_id)
            .map_or(0, |p| p + 1);
        let to = match kept
            .iter()
            .rposition(|k| in_chapter(k) && moment(k).is_some_and(|m| m <= at))
        {
            Some(p) => p + 1,
            None => kept.iter().position(in_chapter).unwrap_or(kept.len()),
        };
        kept.insert(to.max(after_sibling), e);
    }
    *hero = kept;
}

/// Whether `a` and `b` show the same moment: a burst (within
/// [`SAME_MOMENT_BURST_SECS`]), or close in time
/// ([`SAME_MOMENT_SECS`]) with similar pixels.
fn same_moment(a: &Asset, b: &Asset) -> bool {
    let (Some(ta), Some(tb)) = (a.taken_at, b.taken_at) else {
        return false;
    };
    let dt = (ta - tb).abs();
    if dt <= SAME_MOMENT_BURST_SECS {
        return true;
    }
    let (Some(p), Some(q)) = (a.phash, b.phash) else {
        return false;
    };
    dt <= SAME_MOMENT_SECS && crate::hash::phash_distance(p, q) <= SAME_MOMENT_PHASH_DISTANCE
}

/// Mid-rank percentile (0..1) of `v` in an ascending `sorted` slice.
fn midrank_pct(sorted: &[f32], v: f32) -> f32 {
    let less = sorted.partition_point(|&q| q < v);
    let same = sorted.partition_point(|&q| q <= v) - less;
    (less as f32 + same as f32 / 2.0) / sorted.len() as f32
}

/// One chapter member for ranking.
struct ChapterMember<'a> {
    id: &'a str,
    quality: f32,
    bonus: f32,
    /// Capped colourfulness; `None` when unknown.
    colour: Option<f32>,
}

/// Story score per asset: technical quality made relative to the asset's
/// chapter, plus its story bonuses.
///
/// Quality is [`Score::tech_quality`] (already squeezed by the scorer, and
/// kept apart from the clamped `total` so saturation can't make a bonus
/// lower an asset's rank). Each asset's
/// percentile within its chapter (mid-rank, so a lone asset sits at 0.5)
/// is blended in at [`REL_QUALITY_WEIGHT`], so a chapter's sharpest frames
/// are favoured over equal frames of a dull chapter without letting
/// sharpness beat people or captions. Colourfulness, capped at
/// [`crate::score::COLOUR_CAP`], is ranked the same way and blended in,
/// centred, at [`COLOUR_WEIGHT`]. Assets without a score are left out.
fn relative_scores<'a>(
    ordered: &[&Asset],
    chapter_of: impl Fn(&str) -> &'a str,
    scores: &HashMap<String, Score>,
) -> HashMap<String, f32> {
    let mut by_chapter: HashMap<&str, Vec<ChapterMember>> = HashMap::new();
    for a in ordered {
        let Some(s) = score_for_asset(scores, a) else {
            continue;
        };
        by_chapter
            .entry(chapter_of(&a.id))
            .or_default()
            .push(ChapterMember {
                id: a.id.as_str(),
                quality: s.tech_quality(),
                bonus: s.bonus,
                colour: s.colourfulness.map(|c| c.min(crate::score::COLOUR_CAP)),
            });
    }
    let mut out = HashMap::new();
    for members in by_chapter.values() {
        let mut qualities: Vec<f32> = members.iter().map(|m| m.quality).collect();
        qualities.sort_by(f32::total_cmp);
        let mut colours: Vec<f32> = members.iter().filter_map(|m| m.colour).collect();
        colours.sort_by(f32::total_cmp);
        for m in members {
            let pct = midrank_pct(&qualities, m.quality);
            // Assets without colour data (old scores, video) stay neutral.
            let colour_pct = m.colour.map_or(0.5, |c| midrank_pct(&colours, c));
            let relative = (1.0 - REL_QUALITY_WEIGHT) * m.quality
                + REL_QUALITY_WEIGHT * crate::score::squeeze_quality(pct)
                + COLOUR_WEIGHT * crate::score::TECH_SPAN * (colour_pct - 0.5);
            out.insert(m.id.to_string(), (relative + m.bonus).clamp(0.0, 1.0));
        }
    }
    out
}

/// Live Photos that keep their motion in the hero film: the
/// [`MOTION_PER_CHAPTER`] best-scoring ones of each chapter (earliest on
/// ties). Every other Live Photo plays as its still.
fn motion_picks<'a, 'c>(
    ordered: &[&'a Asset],
    chapter_of: impl Fn(&str) -> &'c str,
    score_of: &impl Fn(&str) -> f32,
) -> HashSet<&'a str> {
    let mut by_chapter: HashMap<&'c str, Vec<&'a Asset>> = HashMap::new();
    for a in ordered {
        if a.kind == AssetKind::LivePhoto && a.companion_path.is_some() {
            by_chapter.entry(chapter_of(&a.id)).or_default().push(a);
        }
    }
    let mut out = HashSet::new();
    for mut live in by_chapter.into_values() {
        // `ordered` is story order, so a stable sort keeps the earliest first.
        live.sort_by(|x, y| score_of(&y.id).total_cmp(&score_of(&x.id)));
        out.extend(live.iter().take(MOTION_PER_CHAPTER).map(|a| a.id.as_str()));
    }
    out
}

/// Place name for a chapter from its GPS fixes: the geographic medoid
/// (the fix minimizing total distance to the others) names the place.
/// A raw mean breaks at the antimeridian and is dragged by outliers. The
/// search is quadratic, so large chapters use an even sample of their fixes.
fn chapter_place(gps: &[(f64, f64)]) -> Option<String> {
    crate::geo::place_name(medoid(gps)?)
}

/// Geographic medoid of `gps`: the fix minimizing total distance to the
/// others. Large sets use an even sample of their fixes.
pub(crate) fn medoid(gps: &[(f64, f64)]) -> Option<(f64, f64)> {
    let step = gps.len().div_ceil(MEDOID_MAX_FIXES).max(1);
    let sample: Vec<(f64, f64)> = gps.iter().copied().step_by(step).collect();
    let (_, medoid) = sample
        .iter()
        .map(|&p| {
            let total: f64 = sample.iter().map(|&g| crate::geo::haversine_m(p, g)).sum();
            (total, p)
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))?;
    Some(medoid)
}

/// Display title for a cluster label: `2026-10-12 · place 21` (or a bare
/// `2026-10-12`) becomes `Mon, Oct 12`; anything else passes through.
fn human_chapter_title(label: &str) -> String {
    let date_part = label.split_once(" · ").map_or(label, |(d, _)| d);
    chrono::NaiveDate::parse_from_str(date_part, "%Y-%m-%d")
        .map_or_else(|_| label.to_string(), |d| d.format(DAY_FORMAT).to_string())
}

/// An asset's score: the fresh one when scoring just ran, else the persisted
/// one (which still carries the clip windows).
fn score_for_asset<'s>(scores: &'s HashMap<String, Score>, a: &'s Asset) -> Option<&'s Score> {
    scores.get(&a.id).or(a.score.as_ref())
}

/// Plan entry for one asset. Videos play the best window picked at ingest
/// (`score.clip_windows`) and fall back to the start of the clip. This is
/// the asset's single entry: reels, swap-ins and pins use it, so only the
/// hero film built from scratch shows several windows ([`plan_entries`]).
fn plan_entry(a: &Asset, score: Option<&Score>) -> PlanEntry {
    let mut window = 0;
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
        AssetKind::Video => match score.and_then(Score::best_clip_window) {
            Some((i, w)) => {
                window = i as u32;
                clip_rendition(&w)
            }
            None => Rendition::Clip {
                trim_start: 0.0,
                trim_end: a.duration_secs.unwrap_or(5.0).clamp(1.0, MAX_WINDOW_SECS),
            },
        },
        AssetKind::Photo => Rendition::Still,
    };
    PlanEntry {
        asset_id: a.id.clone(),
        window,
        content_hash: a.content_hash.clone(),
        size_bytes: a.size_bytes,
        mtime: a.mtime,
        rendition,
    }
}

/// Hero entries for one asset: every playback window of a long multi-scene
/// clip, in time order, else just [`plan_entry`].
fn plan_entries(a: &Asset, score: Option<&Score>) -> Vec<PlanEntry> {
    let primary = plan_entry(a, score);
    let windows = match (a.kind, score) {
        (AssetKind::Video, Some(s)) if s.clip_windows.len() > 1 => &s.clip_windows,
        _ => return vec![primary],
    };
    windows
        .iter()
        .enumerate()
        .map(|(i, w)| PlanEntry {
            window: i as u32,
            rendition: clip_rendition(w),
            ..primary.clone()
        })
        .collect()
}

fn clip_rendition(w: &crate::types::ClipSpan) -> Rendition {
    Rendition::Clip {
        trim_start: w.start,
        trim_end: w.end,
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

    fn span(start: f64, end: f64, quality: f32) -> crate::types::ClipSpan {
        crate::types::ClipSpan {
            start,
            end,
            quality,
        }
    }

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
    fn video_entry_uses_best_window_else_start_of_clip() {
        let mut v = asset("v", 1, AssetKind::Video);
        v.duration_secs = Some(40.0);
        let trim = |v: &Asset, scores: &HashMap<String, Score>| match &build_plan(
            std::slice::from_ref(v),
            &HashMap::new(),
            scores,
        )
        .hero[0]
            .rendition
        {
            Rendition::Clip {
                trim_start,
                trim_end,
            } => (*trim_start, *trim_end),
            other => panic!("not a clip: {other:?}"),
        };
        assert_eq!(trim(&v, &HashMap::new()), (0.0, 8.0));
        let mut scores = scored(&["v"]);
        assert_eq!(trim(&v, &scores), (0.0, 8.0));
        scores.get_mut("v").unwrap().clip_windows = vec![span(12.5, 18.5, 0.0)];
        assert_eq!(trim(&v, &scores), (12.5, 18.5));
        // A persisted score on the asset applies when no map entry exists.
        v.score = scores.remove("v");
        assert_eq!(trim(&v, &HashMap::new()), (12.5, 18.5));
        // Live Photo motion is never windowed.
        let mut lp = asset("l", 2, AssetKind::LivePhoto);
        lp.companion_path = Some(PathBuf::from("/t/l.mov"));
        lp.duration_secs = Some(3.0);
        let s = Score {
            clip_windows: vec![span(1.0, 2.0, 0.0)],
            ..Default::default()
        };
        assert_eq!(
            plan_entry(&lp, Some(&s)).rendition,
            Rendition::Motion {
                trim_start: 0.0,
                trim_end: 3.0
            }
        );
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
        // Fushimi Inari cluster plus one far outlier: still Fushimi Inari.
        let mut gps: Vec<(f64, f64)> = (0..8)
            .map(|i| (34.9671 + i as f64 * 0.001, 135.7727))
            .collect();
        gps.push((-33.8688, 151.2093)); // Sydney outlier
        assert_eq!(chapter_place(&gps).as_deref(), Some("Fushimi Inari-taisha"));
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
        assert_eq!(
            chapter_place(&many).as_deref(),
            Some("Fushimi Inari-taisha")
        );
    }

    #[test]
    fn chapters_name_places_without_auto_captions() {
        // Fushimi Inari GPS on every asset: chapter is named by place, the
        // title prefers the place over the date, and no caption is invented.
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
        assert_eq!(
            plan.chapters[0].place.as_deref(),
            Some("Fushimi Inari-taisha")
        );
        assert_eq!(plan.chapters[0].title, "Fushimi Inari-taisha");
        // The place is the chapter's, never copied onto each photo.
        assert!(plan.captions.is_empty(), "got: {:?}", plan.captions);
    }

    #[test]
    fn edits_exclude_swap_rename_and_recaption() {
        let mk = |id: &str, t: i64| {
            let mut a = asset(id, t, AssetKind::Photo);
            a.gps = Some((34.9671, 135.7727)); // Fushimi Inari
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
        assert_eq!(
            base.chapters[0].place.as_deref(),
            Some("Fushimi Inari-taisha")
        );

        let edits = PlanEdits {
            excluded: vec!["b".into()],
            included: vec![],
            swaps: vec![("a".into(), "c".into())],
            place_overrides: [("a".to_string(), "Our Ryokan".to_string())].into(),
            caption_overrides: [("c".to_string(), "Breakfast view".to_string())].into(),
        };
        let out = apply_edits(&base, &assets, &edits);
        // Rename hit the chapter; no auto captions to follow it.
        assert!(!out.captions.contains_key("a"));
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
        base.hero = vec![plan_entry(&assets[0], None), plan_entry(&assets[1], None)];
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
        base.hero = vec![plan_entry(&assets[1], None)]; // only "z" is picked
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
        base.hero = vec![plan_entry(&assets[0], None), plan_entry(&assets[3], None)];
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

    /// Five photos in one chapter, ids "a".."e" in capture order; the base
    /// hero holds `picked`.
    fn pin_fixture(picked: &[usize]) -> (Vec<Asset>, Plan) {
        let ids = ["a", "b", "c", "d", "e"];
        let assets: Vec<Asset> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let mut a = asset(id, 1_714_557_600 + i as i64 * 300, AssetKind::Photo);
                a.score = Some(Score {
                    total: 0.2 + i as f32 * 0.1,
                    ..Default::default()
                });
                a
            })
            .collect();
        let co = ids
            .iter()
            .map(|id| (id.to_string(), "2024-05-01".to_string()))
            .collect();
        let mut base = build_plan(&assets, &co, &scored(&ids));
        base.hero = picked
            .iter()
            .map(|&i| plan_entry(&assets[i], assets[i].score.as_ref()))
            .collect();
        (assets, base)
    }

    fn hero_ids(p: &Plan) -> Vec<&str> {
        p.hero.iter().map(|e| e.asset_id.as_str()).collect()
    }

    #[test]
    fn include_and_exclude_are_mutually_exclusive() {
        let mut edits = PlanEdits::default();
        edits.set_included("a".into(), true);
        edits.set_included("a".into(), true);
        assert_eq!(edits.included, ["a"]);
        edits.set_excluded("a".into(), true);
        assert!(edits.included.is_empty());
        assert_eq!(edits.excluded, ["a"]);
        edits.set_included("a".into(), true);
        assert!(edits.excluded.is_empty());
        assert!(!edits.is_empty());
        edits.set_included("a".into(), false);
        assert!(edits.is_empty());
    }

    #[test]
    fn included_asset_lands_in_story_order_and_displaces_the_weakest() {
        // Hero holds a, c, e (scores 0.2, 0.4, 0.6); pin d.
        let (assets, base) = pin_fixture(&[0, 2, 4]);
        let mut edits = PlanEdits::default();
        edits.set_included("d".into(), true);
        let out = apply_edits(&base, &assets, &edits);
        // Weakest unpinned (a) makes room; d sits between c and e.
        assert_eq!(hero_ids(&out), ["c", "d", "e"]);
        // Pinning an already-picked asset changes nothing.
        let mut edits = PlanEdits::default();
        edits.set_included("c".into(), true);
        assert_eq!(
            hero_ids(&apply_edits(&base, &assets, &edits)),
            ["a", "c", "e"]
        );
    }

    #[test]
    fn pins_are_never_displaced_and_fall_back_to_growing_the_film() {
        let (assets, base) = pin_fixture(&[0]);
        let mut edits = PlanEdits::default();
        edits.set_included("a".into(), true);
        edits.set_included("b".into(), true);
        edits.set_included("c".into(), true);
        let out = apply_edits(&base, &assets, &edits);
        assert_eq!(hero_ids(&out), ["a", "b", "c"]);
        // A swap replacement counts as pinned too.
        let (assets, base) = pin_fixture(&[0, 1]);
        let mut edits = PlanEdits::default();
        edits.swap("a".into(), "e".into()); // hero: e, b
        edits.set_included("c".into(), true);
        let out = apply_edits(&base, &assets, &edits);
        assert_eq!(hero_ids(&out), ["c", "e"]);
    }

    #[test]
    fn swapping_a_pin_moves_the_pin_to_the_replacement() {
        let hero = |picked: &[usize], f: &dyn Fn(&mut PlanEdits)| {
            let (assets, base) = pin_fixture(picked);
            let mut edits = PlanEdits::default();
            f(&mut edits);
            let out = apply_edits(&base, &assets, &edits);
            (hero_ids(&out).join(","), edits.included)
        };
        // Pin absent from the base hero, then swapped.
        let (ids, pins) = hero(&[0], &|e| {
            e.set_included("b".into(), true);
            e.swap("b".into(), "c".into());
        });
        assert_eq!((ids.as_str(), pins), ("c", vec!["c".to_string()]));
        // Pin the base hero already holds, then swapped: the original stays out.
        let (ids, _) = hero(&[0, 1], &|e| {
            e.set_included("a".into(), true);
            e.swap("a".into(), "c".into());
        });
        assert_eq!(ids, "c,b");
        // Chained, and back to the first pin.
        let (ids, _) = hero(&[0], &|e| {
            e.set_included("b".into(), true);
            e.swap("b".into(), "c".into());
            e.swap("c".into(), "d".into());
        });
        assert_eq!(ids, "d");
        let (ids, pins) = hero(&[0], &|e| {
            e.set_included("b".into(), true);
            e.swap("b".into(), "c".into());
            e.swap("c".into(), "b".into());
        });
        assert_eq!((ids.as_str(), pins), ("b", vec!["b".to_string()]));
        // A swapped-out pick pinned back, then swapped: both replacements stay.
        let (ids, pins) = hero(&[0], &|e| {
            e.swap("a".into(), "b".into());
            e.set_included("a".into(), true);
            e.swap("a".into(), "c".into());
        });
        assert_eq!((ids.as_str(), pins), ("b,c", vec!["c".to_string()]));
    }

    #[test]
    fn pin_never_plays_before_an_earlier_capture() {
        // Hero a, b, c; swap a -> e keeps a's slot: e, b, c. Pinning d
        // displaces b (weakest) and must land after c, not before e.
        let (assets, base) = pin_fixture(&[0, 1, 2]);
        let mut edits = PlanEdits::default();
        edits.swap("a".into(), "e".into());
        edits.set_included("d".into(), true);
        let out = apply_edits(&base, &assets, &edits);
        assert_eq!(hero_ids(&out), ["e", "c", "d"]);
    }

    #[test]
    fn included_clips_keep_their_window_and_live_photos_are_stills() {
        let mut v = asset("v", 1_714_557_600, AssetKind::Video);
        v.duration_secs = Some(40.0);
        v.score = Some(Score {
            clip_windows: vec![span(5.0, 12.0, 0.0)],
            ..Default::default()
        });
        let mut live = asset("l", 1_714_557_900, AssetKind::LivePhoto);
        live.companion_path = Some(PathBuf::from("/t/l.mov"));
        live.duration_secs = Some(2.0);
        let assets = vec![v, live];
        let co = ["v", "l"]
            .iter()
            .map(|id| (id.to_string(), "2024-05-01".to_string()))
            .collect();
        let mut base = build_plan(&assets, &co, &scored(&["v", "l"]));
        base.hero.clear();
        let mut edits = PlanEdits::default();
        edits.set_included("l".into(), true);
        edits.set_included("v".into(), true);
        let out = apply_edits(&base, &assets, &edits);
        assert_eq!(hero_ids(&out), ["v", "l"]);
        assert_eq!(
            out.hero[0].rendition,
            Rendition::Clip {
                trim_start: 5.0,
                trim_end: 12.0
            }
        );
        assert_eq!(out.hero[1].rendition, Rendition::Still);
    }

    #[test]
    fn pins_outside_the_plan_or_removed_are_skipped() {
        let (assets, mut base) = pin_fixture(&[0, 2]);
        // "e" drops out of every chapter: nothing to trade it against.
        for ch in &mut base.chapters {
            ch.asset_ids.retain(|id| id != "e");
        }
        let mut edits = PlanEdits::default();
        edits.set_included("e".into(), true);
        edits.set_included("ghost".into(), true);
        edits.set_included("d".into(), true);
        // A pin that is also removed (hand-edited JSON) stays out.
        edits.excluded.push("d".into());
        assert_eq!(hero_ids(&apply_edits(&base, &assets, &edits)), ["a", "c"]);
    }

    #[test]
    fn chapter_of_finds_the_member_chapter() {
        let (_, base) = pin_fixture(&[]);
        assert!(base.chapter_of("c").is_some());
        assert!(base.chapter_of("ghost").is_none());
    }

    #[test]
    fn edits_json_without_included_still_loads() {
        let edits: PlanEdits = serde_json::from_str(r#"{"excluded":["a"]}"#).unwrap();
        assert!(edits.included.is_empty());
    }

    #[test]
    fn only_user_captions_reach_the_plan() {
        let mut a = asset("a", 1_714_557_600, AssetKind::Photo);
        a.gps = Some((34.9671, 135.7727)); // Fushimi Inari
        a.user_caption = Some("Mia's first shinkansen ride".into());
        let mut b = asset("b", 1_714_557_900, AssetKind::Photo);
        b.gps = Some((34.9671, 135.7727));
        let assets = vec![a, b];
        let mut co = HashMap::new();
        co.insert("a".into(), "2024-05-01 · place 1".into());
        co.insert("b".into(), "2024-05-01 · place 1".into());
        let plan = build_plan(&assets, &co, &scored(&["a", "b"]));
        assert_eq!(plan.captions["a"], "Mia's first shinkansen ride");
        assert!(!plan.captions.contains_key("b"));
        assert_eq!(plan.captions.len(), 1);
    }

    #[test]
    fn legacy_plans_drop_auto_place_captions() {
        // A v1 plan stored the chapter place as every photo's caption.
        let mut a = asset("a", 1_714_557_600, AssetKind::Photo);
        a.gps = Some((34.9671, 135.7727));
        a.user_caption = Some("Mia's first shinkansen ride".into());
        let mut b = asset("b", 1_714_557_900, AssetKind::Photo);
        b.gps = Some((34.9671, 135.7727));
        let mut c = asset("c", 1_714_558_200, AssetKind::Photo);
        c.gps = Some((34.9671, 135.7727));
        let assets = vec![a, b, c];
        let co: HashMap<String, String> = ["a", "b", "c"]
            .iter()
            .map(|id| (id.to_string(), "2024-05-01 · place 1".to_string()))
            .collect();
        let mut legacy = build_plan(&assets, &co, &scored(&["a", "b", "c"]));
        legacy.plan_version = AUTO_PLACE_CAPTIONS_VERSION;
        for id in ["a", "b", "c"] {
            legacy.captions.insert(id.into(), "Kyoto, Japan".into());
        }

        // No edits: the place captions go, the embedded caption is restored.
        let out = apply_edits(&legacy, &assets, &PlanEdits::default());
        assert_eq!(out.plan_version, PLAN_VERSION);
        assert_eq!(out.captions["a"], "Mia's first shinkansen ride");
        assert_eq!(out.captions.len(), 1);

        // Rename plus a typed override: the override wins, nothing stale stays.
        let edits = PlanEdits {
            place_overrides: [("b".to_string(), "Our Ryokan".to_string())].into(),
            caption_overrides: [("c".to_string(), "Breakfast view".to_string())].into(),
            ..Default::default()
        };
        let out = apply_edits(&legacy, &assets, &edits);
        assert_eq!(out.chapters[0].place.as_deref(), Some("Our Ryokan"));
        assert_eq!(out.captions["a"], "Mia's first shinkansen ride");
        assert!(!out.captions.contains_key("b"));
        assert_eq!(out.captions["c"], "Breakfast view");

        // Current plans are left as stored.
        let mut current = legacy.clone();
        current.plan_version = PLAN_VERSION;
        let out = apply_edits(&current, &assets, &PlanEdits::default());
        assert_eq!(out.captions["b"], "Kyoto, Japan");
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
        port.gps = Some((34.9671, 135.7727)); // Fushimi Inari
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
        assert_eq!(place_of("port"), Some("Fushimi Inari-taisha"));
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
            window: 0,
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
        estimate_hero_secs(
            &plan.hero,
            |id| clusters.get(id).map_or(UNGROUPED, String::as_str),
            |_| None,
            |id| has_caption(plan.captions.get(id).map(String::as_str)),
            HeroCards {
                route: !plan.map_polyline.is_empty(),
                route_stops: 0,
                places: plan.chapters.iter().any(|c| c.place.is_some()),
            },
        )
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
            plan.hero.len() < 130 && (120.0..=120.0 * 1.03).contains(&secs),
            "fills 120 s, at most 3% over: {} entries, {secs:.1} s",
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
    fn tight_budget_drops_weakest_chapters_but_never_empties() {
        // 20 one-photo chapters scored by index; 30 s fits only a few.
        let assets = photos(20);
        let mut scores = HashMap::new();
        let mut clusters = HashMap::new();
        for i in 0..20 {
            let id = format!("a{i:02}");
            scores.insert(
                id.clone(),
                Score {
                    total: 0.5 + i as f32 / 100.0,
                    ..Default::default()
                },
            );
            clusters.insert(id, format!("Day {i}"));
        }
        let hero = |secs| {
            build_plan_with_length(&assets, &clusters, &scores, &[], FilmLength::Secs(secs)).hero
        };
        let kept = hero(30.0);
        assert!((1..20).contains(&kept.len()), "{} entries", kept.len());
        let weakest_kept = kept.iter().map(|e| e.asset_id.as_str()).min().unwrap();
        assert_eq!(
            weakest_kept,
            format!("a{:02}", 20 - kept.len()),
            "strongest chapters kept"
        );
        assert_eq!(hero(1.0).len(), 1, "even a too-short budget keeps one shot");
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
    fn chapters_share_screen_time_not_shot_counts() {
        // Two 10-entry chapters: one of 8 s clips, one of photos. Balanced
        // by seconds, the photo chapter gets more shots than the clip one.
        let assets: Vec<Asset> = (0..20)
            .map(|i| {
                let kind = if i < 10 {
                    AssetKind::Video
                } else {
                    AssetKind::Photo
                };
                let mut a = asset(&format!("a{i:02}"), 1_714_557_600 + i * 60, kind);
                a.duration_secs = Some(8.0);
                a
            })
            .collect();
        let clusters: HashMap<String, String> = (0..20)
            .map(|i| {
                let day = if i < 10 { "Day 1" } else { "Day 2" };
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
        let clips = plan
            .hero
            .iter()
            .filter(|e| clusters[&e.asset_id] == "Day 1")
            .count();
        let stills = plan.hero.len() - clips;
        assert!(
            clips >= 1 && stills > clips,
            "clips={clips} stills={stills}"
        );
    }

    #[test]
    fn auto_length_counts_calendar_span_not_capture_days() {
        // Plenty of photos on 4 days spread over a 30-day trip: the auto
        // length follows the 30-day span, not the 4 capture days.
        let day = 86_400;
        let assets: Vec<Asset> = (0..400)
            .map(|i| {
                let d = [0, 9, 19, 29][i as usize % 4];
                asset(
                    &format!("a{i:03}"),
                    1_714_557_600 + d * day + i * 60,
                    AssetKind::Photo,
                )
            })
            .collect();
        let none = HashMap::new();
        let len = |length| {
            build_plan_with_length(&assets, &none, &HashMap::new(), &[], length)
                .hero
                .len()
        };
        let auto = len(FilmLength::Auto);
        // An explicit length is a minimum (up to 3% over); auto is not.
        let explicit = len(FilmLength::Secs(auto_film_secs(30)));
        assert!(
            auto <= explicit && explicit as f64 <= auto as f64 * 1.06,
            "auto={auto} explicit={explicit}"
        );
        assert!(
            auto > len(FilmLength::Secs(auto_film_secs(4))),
            "auto={auto}"
        );
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
            window: 0,
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
        // Segments: title 2.5, map 3, 4-photo collage 7, solo 4, clip 6,
        // padded clip 2.5, solo 4, closing map 4.5, places 4.5 = 38 s. Cuts:
        // opening 0.7, map->hero break 0.8, plain 0.54, chapter break 0.5,
        // plain 0.54 x2, hero->closing break 0.5, closing 0.7 = 4.82 s.
        let cards = HeroCards {
            route: true,
            route_stops: 0,
            places: true,
        };
        assert!(
            (estimate_hero_secs(&entries, ch, |_| None, |_| false, cards) - 33.18).abs() < 1e-9
        );
        // Without a route or places only the title card remains.
        let bare = HeroCards {
            route: false,
            route_stops: 0,
            places: false,
        };
        // 2.5 + 7 + 4 + 6 + 2.5 + 4 = 26 s; cuts 0.7 + 0.54 + 0.8 + 0.54 +
        // 0.7 = 3.28 s.
        assert!((estimate_hero_secs(&entries, ch, |_| None, |_| false, bare) - 22.72).abs() < 1e-9);
    }

    #[test]
    fn captioned_stills_play_solo_in_the_estimate() {
        let bare = HeroCards {
            route: false,
            route_stops: 0,
            places: false,
        };
        let est = |ids: &[&str], caps: &[(&str, &str)]| {
            let entries: Vec<PlanEntry> = ids.iter().map(|i| still_entry(i)).collect();
            let caps: HashMap<&str, &str> = caps.iter().copied().collect();
            estimate_hero_secs(
                &entries,
                |_| "X",
                |_| None,
                |id| has_caption(caps.get(id).copied()),
                bare,
            )
        };
        // Middle: collage of 2 (5 s), solo (4 s), collage of 2 (5 s); cuts
        // 0.7 + 0.54 + 0.7 = 1.94 s, the title 2.5 s.
        let mid = est(&["a", "b", "c", "d", "e"], &[("c", "Ferry")]);
        assert!((mid - 14.56).abs() < 1e-9, "{mid}");
        // Leading and trailing captioned stills never join their neighbours.
        let lead = est(&["a", "b", "c"], &[("a", "Ferry")]);
        let trail = est(&["a", "b", "c"], &[("c", "Ferry")]);
        assert!((lead - 10.1).abs() < 1e-9, "{lead}");
        assert!((trail - 10.1).abs() < 1e-9, "{trail}");
        // A blank caption is no caption: all three still collage.
        let blank = est(&["a", "b", "c"], &[("a", "  ")]);
        assert!((blank - 7.8).abs() < 1e-9, "{blank}");
        assert!(!has_caption(Some(" \n")) && has_caption(Some(" x ")) && !has_caption(None));
    }

    #[test]
    fn edited_captions_count_as_captions() {
        let assets = vec![asset("a", 1_714_557_600, AssetKind::Photo)];
        let base = build_plan(&assets, &HashMap::new(), &HashMap::new());
        let edits = PlanEdits {
            caption_overrides: [("a".to_string(), "Breakfast view".to_string())].into(),
            ..Default::default()
        };
        let out = apply_edits(&base, &assets, &edits);
        assert!(has_caption(out.captions.get("a").map(String::as_str)));
        let blank = PlanEdits {
            caption_overrides: [("a".to_string(), "   ".to_string())].into(),
            ..Default::default()
        };
        let out = apply_edits(&base, &assets, &blank);
        assert!(!has_caption(out.captions.get("a").map(String::as_str)));
    }

    #[test]
    fn panorama_detection_and_hold() {
        assert!(is_panorama(4.4));
        assert!(is_panorama(2.0));
        assert!(is_panorama(0.5));
        assert!(!is_panorama(1.99));
        assert!(!is_panorama(16.0 / 9.0));
        assert!(!is_panorama(0.75));
        assert!(!is_panorama(0.0) && !is_panorama(f64::NAN));
        // Holds grow with aspect inside 6..=8 s; full travel up to ~6:1.
        let mid = pano_motion(2.0);
        assert!((PANO_MIN_SECS..=PANO_MAX_SECS).contains(&mid.hold_secs));
        let wide = pano_motion(4.4);
        assert!(wide.hold_secs >= mid.hold_secs && wide.hold_secs <= PANO_MAX_SECS);
        assert_eq!(wide.coverage, 1.0);
        // Tall pans along the frame height: a 1:4 strip is ~7.1 frame
        // heights tall, so it holds long and travels part of the way.
        let tall = pano_motion(0.25);
        let tall_overflow = (16.0 / 9.0) / 0.25 - 1.0;
        assert!(tall.hold_secs > PANO_MAX_SECS && tall.coverage < 1.0);
        assert!(tall_overflow * tall.coverage / tall.hold_secs <= PANO_MAX_SPEED + 1e-9);
        // Extreme: longer hold, bounded speed, partial coverage.
        let huge = pano_motion(12.0);
        assert!(huge.hold_secs > PANO_MAX_SECS && huge.hold_secs <= PANO_LONG_SECS);
        assert_eq!(huge.hold_secs, 10.0, "on-screen cap is 10 s");
        assert!(huge.coverage < 1.0);
        let overflow = 12.0 / (16.0 / 9.0) - 1.0;
        assert!(overflow * huge.coverage / huge.hold_secs <= PANO_MAX_SPEED + 1e-9);
        // No aspect ever holds past the 10 s cap or beats the speed bound.
        for aspect in [2.0, 3.0, 4.4, 6.0, 7.5, 12.0, 30.0, 0.5, 0.25, 0.1] {
            let m = pano_motion(aspect);
            assert!(m.hold_secs <= 10.0 + 1e-9, "{aspect}: {m:?}");
            assert!(m.coverage > 0.0 && m.coverage <= 1.0, "{aspect}: {m:?}");
        }
        // A shortened pan travels less, at the same speed bound.
        let short = pano_coverage(12.0, MIN_SEGMENT_SECS);
        assert!(short < huge.coverage);
        assert!(overflow * short / MIN_SEGMENT_SECS <= PANO_MAX_SPEED + 1e-9);
    }

    #[test]
    fn panorama_is_solo_with_own_hold_in_estimate() {
        let still = |id: &str| PlanEntry {
            asset_id: id.into(),
            window: 0,
            content_hash: None,
            size_bytes: 0,
            mtime: 0,
            rendition: Rendition::Still,
        };
        let entries = [still("x1"), still("p"), still("x2"), still("x3")];
        let pano = |id: &str| (id == "p").then_some(8.0);
        // x1 solo 4, pano breaks the run 8, x2+x3 collage of 2 (5.0) = 19.5 s
        // plus title 2.5 = 19.5 + ... cuts 0.7 + 0.54 + 0.7 = 1.94 s.
        let bare = HeroCards {
            route: false,
            route_stops: 0,
            places: false,
        };
        assert!(
            (estimate_hero_secs(&entries, |_| "X", pano, |_| false, bare) - 17.56).abs() < 1e-9
        );
    }

    #[test]
    fn display_aspect_applies_rotation() {
        let mut a = asset("a", 0, AssetKind::Photo);
        assert_eq!(a.display_aspect(), None);
        a.width = Some(16350);
        a.height = Some(3686);
        assert!(is_panorama(a.display_aspect().unwrap()));
        a.rotation_deg = 90;
        assert!(a.display_aspect().unwrap() < 0.5);
        assert!(is_panorama(a.display_aspect().unwrap()));
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

    // -- story scoring, selection and timing ------------------------------

    fn still_entry(id: &str) -> PlanEntry {
        PlanEntry {
            asset_id: id.into(),
            window: 0,
            content_hash: None,
            size_bytes: 0,
            mtime: 0,
            rendition: Rendition::Still,
        }
    }

    fn clip_entry(id: &str, secs: f64) -> PlanEntry {
        PlanEntry {
            rendition: Rendition::Clip {
                trim_start: 0.0,
                trim_end: secs,
            },
            ..still_entry(id)
        }
    }

    /// Chapter = the id's first letter.
    fn letter(id: &str) -> &'static str {
        ["a", "b", "c", "d"]
            .into_iter()
            .find(|l| id.starts_with(l))
            .unwrap_or("z")
    }

    const NO_CARDS: HeroCards = HeroCards {
        route: false,
        route_stops: 0,
        places: false,
    };

    fn pick(
        cands: Vec<PlanEntry>,
        scores: &[(&str, f32)],
        ceiling_secs: f64,
        similar: impl Fn(&str, &str) -> bool,
    ) -> Vec<String> {
        let score_of = |e: &PlanEntry| {
            scores
                .iter()
                .find(|(i, _)| *i == e.asset_id)
                .map_or(0.5, |(_, s)| *s)
        };
        select_hero_by_budget(
            cands,
            letter,
            score_of,
            similar,
            |_| None,
            |_| false,
            HeroBudget {
                ceiling_secs,
                cards: NO_CARDS,
            },
        )
        .into_iter()
        .map(|e| e.asset_id)
        .collect()
    }

    #[test]
    fn selection_skips_a_pick_that_does_not_fit_and_tries_cheaper_ones() {
        let cands = vec![
            clip_entry("a1", 8.0),
            clip_entry("a2", 8.0),
            still_entry("a3"),
            still_entry("b1"),
        ];
        let scores = [("a1", 0.9), ("a2", 0.8), ("a3", 0.7), ("b1", 0.6)];
        let reps = estimate_hero_secs(
            [&cands[0], &cands[3]],
            letter,
            |_| None,
            |_| false,
            NO_CARDS,
        );
        // Room for a still after the reps, not for a second 8 s clip.
        let got = pick(cands, &scores, reps + 4.0, |_, _| false);
        assert_eq!(got, vec!["a1", "a3", "b1"], "{got:?}");
    }

    #[test]
    fn near_identical_shots_are_passed_over_until_nothing_else_is_left() {
        let cands = || {
            vec![
                clip_entry("a1", 3.0),
                clip_entry("a2", 3.0),
                clip_entry("a3", 3.0),
                clip_entry("a4", 3.0),
            ]
        };
        let scores = [("a1", 0.9), ("a2", 0.85), ("a3", 0.5), ("a4", 0.4)];
        // a2 is the same moment as a1.
        let twin = |x: &str, y: &str| {
            let pair = |p: &str, q: &str| (x == p && y == q) || (x == q && y == p);
            pair("a1", "a2")
        };
        let reps = estimate_hero_secs([&cands()[0]], letter, |_| None, |_| false, NO_CARDS);
        let tight = pick(cands(), &scores, reps + 7.0, twin);
        assert_eq!(tight, vec!["a1", "a3", "a4"], "twin skipped: {tight:?}");
        // With budget to spare the twin comes back in the second pass.
        let plenty = pick(cands(), &scores, 1000.0, twin);
        assert_eq!(plenty.len(), 4);
    }

    #[test]
    fn big_chapters_get_time_by_size_to_the_three_quarters() {
        // 16-clip day vs 4-clip day, equal clips and scores, ~20 clips of room.
        let mut cands: Vec<PlanEntry> = (0..16)
            .map(|i| clip_entry(&format!("a{i:02}"), 3.0))
            .collect();
        cands.extend((0..4).map(|i| clip_entry(&format!("b{i:02}"), 3.0)));
        let ceiling =
            estimate_hero_secs(&cands[..0], letter, |_| None, |_| false, NO_CARDS) + 20.0 * 2.5;
        let got = pick(cands, &[], ceiling, |_, _| false);
        let big = got.iter().filter(|i| i.starts_with('a')).count() as f64;
        let small = got.iter().filter(|i| i.starts_with('b')).count() as f64;
        assert!(small >= 1.0, "every chapter keeps its best shot");
        // size^0.75 predicts 2.8x the small day's screen time (sqrt: 2x).
        assert!(big / small > 2.3, "big={big} small={small}");
    }

    fn live(id: &str, taken: i64) -> Asset {
        let mut a = asset(id, taken, AssetKind::LivePhoto);
        a.companion_path = Some(PathBuf::from(format!("/t/{id}.mov")));
        a.duration_secs = Some(2.6);
        a
    }

    #[test]
    fn live_photos_play_as_stills_except_the_best_of_each_chapter() {
        let mut assets: Vec<Asset> = (0..6).map(|i| live(&format!("a{i}"), 100 + i)).collect();
        assets.extend((0..4).map(|i| live(&format!("b{i}"), 1000 + i)));
        let mut clusters = HashMap::new();
        let mut scores = HashMap::new();
        for a in &assets {
            let idx: f32 = a.id[1..].parse().unwrap();
            clusters.insert(a.id.clone(), a.id[..1].to_string());
            scores.insert(
                a.id.clone(),
                Score {
                    total: 0.5 + idx / 20.0,
                    ..Default::default()
                },
            );
        }
        let plan =
            build_plan_with_length(&assets, &clusters, &scores, &[], FilmLength::Secs(3600.0));
        let motion: Vec<&str> = plan
            .hero
            .iter()
            .filter(|e| matches!(e.rendition, Rendition::Motion { .. }))
            .map(|e| e.asset_id.as_str())
            .collect();
        assert_eq!(motion, vec!["a5", "b3"], "best live photo per chapter");
        assert_eq!(plan.hero.len(), 10);
        assert!(plan
            .hero
            .iter()
            .filter(|e| !motion.contains(&e.asset_id.as_str()))
            .all(|e| e.rendition == Rendition::Still));
        // Reels still prefer motion.
        assert!(plan
            .reel_selects
            .iter()
            .all(|e| matches!(e.rendition, Rendition::Motion { .. })));
    }

    #[test]
    fn swapped_in_live_photo_is_a_hero_still_but_a_reel_motion() {
        let assets = vec![asset("a", 100, AssetKind::Photo), live("l", 200)];
        let plan = build_plan(&assets, &HashMap::new(), &HashMap::new());
        let mut plan = plan;
        plan.hero = vec![still_entry("a")];
        plan.reel_selects = vec![still_entry("a")];
        let edits = PlanEdits {
            swaps: vec![("a".into(), "l".into())],
            ..Default::default()
        };
        let out = apply_edits(&plan, &assets, &edits);
        assert_eq!(out.hero[0].asset_id, "l");
        assert_eq!(out.hero[0].rendition, Rendition::Still);
        assert!(matches!(
            out.reel_selects[0].rendition,
            Rendition::Motion { .. }
        ));
    }

    fn with_bonus(quality: f32, bonus: f32) -> Score {
        Score {
            total: quality + bonus,
            bonus,
            ..Default::default()
        }
    }

    #[test]
    fn people_and_captions_beat_a_sharper_dull_photo_in_the_hero() {
        // One chapter, one hero slot beyond nothing: the group photo wins
        // even though the empty street is technically the best frame.
        let assets = vec![
            asset("street", 100, AssetKind::Photo),
            asset("group", 200, AssetKind::Photo),
            asset("bus", 300, AssetKind::Photo),
        ];
        let scores: HashMap<String, Score> = [
            ("street", with_bonus(0.74, 0.0)),
            ("group", with_bonus(0.62, 0.28)),
            ("bus", with_bonus(0.70, 0.0)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        // Room for exactly one still.
        let one = estimate_hero_secs(
            [&still_entry("x")],
            |_| UNGROUPED,
            |_| None,
            |_| false,
            NO_CARDS,
        );
        let plan = build_plan_with_length(
            &assets,
            &HashMap::new(),
            &scores,
            &[],
            FilmLength::Secs(one - 0.01),
        );
        let ids: Vec<&str> = plan.hero.iter().map(|e| e.asset_id.as_str()).collect();
        assert_eq!(ids, vec!["group"], "{ids:?}");
    }

    #[test]
    fn relative_quality_favours_the_best_of_a_dull_chapter() {
        let assets: Vec<Asset> = ["a1", "a2", "a3", "b1", "b2", "b3"]
            .iter()
            .enumerate()
            .map(|(i, id)| asset(id, 100 + i as i64, AssetKind::Photo))
            .collect();
        let q = |id: &str| match id {
            "a1" => 0.60,
            "a2" => 0.62,
            "a3" => 0.64, // best of a dull chapter
            "b1" => 0.64,
            "b2" => 0.68,
            _ => 0.72,
        };
        let scores: HashMap<String, Score> = assets
            .iter()
            .map(|a| (a.id.clone(), with_bonus(q(&a.id), 0.0)))
            .collect();
        let ordered: Vec<&Asset> = assets.iter().collect();
        let rel = relative_scores(&ordered, letter, &scores);
        // Same absolute quality, but a3 tops its chapter while b1 is last in
        // its own.
        assert!(rel["a3"] > rel["b1"], "{rel:?}");
        // Percentile never overturns a story bonus.
        let mut with_people = scores.clone();
        with_people.insert("a1".into(), with_bonus(0.60, 0.10));
        let rel = relative_scores(&ordered, letter, &with_people);
        assert!(rel["a1"] > rel["a3"], "{rel:?}");
        // Tied qualities share one mid-rank.
        let mut tied = scores.clone();
        tied.insert("a2".into(), with_bonus(0.60, 0.0));
        let rel = relative_scores(&ordered, letter, &tied);
        assert_eq!(rel["a1"], rel["a2"], "{rel:?}");
        assert!(rel["a3"] > rel["a1"], "{rel:?}");
        // A lone asset sits mid-chapter and keeps its bonus.
        let lone = vec![&assets[0]];
        let rel = relative_scores(&lone, |_| "x", &scores);
        assert!((rel["a1"] - 0.60).abs() < 0.1 * REL_QUALITY_WEIGHT + 1e-6);
    }

    fn with_colour(quality: f32, colour: Option<f32>) -> Score {
        Score {
            colourfulness: colour,
            ..with_bonus(quality, 0.0)
        }
    }

    #[test]
    fn colour_is_ranked_within_the_chapter_not_across() {
        let assets: Vec<Asset> = ["a1", "a2", "a3", "b1", "b2", "b3"]
            .iter()
            .enumerate()
            .map(|(i, id)| asset(id, 100 + i as i64, AssetKind::Photo))
            .collect();
        // Chapter b is a dim night: every frame is far less colourful than
        // chapter a, yet b3 is still b's most colourful and b1 its flattest.
        let colour = |id: &str| match id {
            "a1" => 20.0,
            "a2" => 45.0,
            "a3" => 70.0,
            "b1" => 1.0,
            "b2" => 3.0,
            _ => 6.0,
        };
        let scores: HashMap<String, Score> = assets
            .iter()
            .map(|a| (a.id.clone(), with_colour(0.6, Some(colour(&a.id)))))
            .collect();
        let ordered: Vec<&Asset> = assets.iter().collect();
        let rel = relative_scores(&ordered, letter, &scores);
        assert!(rel["a3"] > rel["a2"] && rel["a2"] > rel["a1"], "{rel:?}");
        assert!(rel["b3"] > rel["b1"], "{rel:?}");
        // Equal rank in each chapter, so the dull chapter is not penalised.
        assert!((rel["b3"] - rel["a3"]).abs() < 1e-6, "{rel:?}");
        assert!((rel["b1"] - rel["a1"]).abs() < 1e-6, "{rel:?}");
    }

    #[test]
    fn colour_is_capped_small_and_neutral_when_missing() {
        let assets = [
            asset("vivid", 100, AssetKind::Photo),
            asset("garish", 101, AssetKind::Photo),
            asset("old", 102, AssetKind::Photo),
        ];
        let ordered: Vec<&Asset> = assets.iter().collect();
        let cap = crate::score::COLOUR_CAP;
        let scores: HashMap<String, Score> = [
            ("vivid", with_colour(0.6, Some(cap))),
            ("garish", with_colour(0.6, Some(cap * 3.0))),
            ("old", with_colour(0.6, None)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let rel = relative_scores(&ordered, |_| "x", &scores);
        // Above the cap ties with the cap; a missing value sits mid-chapter.
        assert_eq!(rel["vivid"], rel["garish"], "{rel:?}");
        assert!((rel["old"] - rel["vivid"]).abs() < 1e-6, "{rel:?}");
        // The whole colour effect stays under half a people bonus.
        let flat = HashMap::from([
            ("vivid".to_string(), with_colour(0.6, Some(0.0))),
            ("garish".to_string(), with_colour(0.6, Some(cap))),
        ]);
        let two: Vec<&Asset> = assets[..2].iter().collect();
        let rel = relative_scores(&two, |_| "x", &flat);
        let spread = rel["garish"] - rel["vivid"];
        assert!(
            spread > 0.0 && spread < crate::score::PEOPLE_BONUS[0] / 2.0,
            "{spread}"
        );
    }

    #[test]
    fn saturated_total_does_not_let_a_caption_lower_the_rank() {
        // Equal technical quality; the caption pushes `total` past 1. Ranking
        // must read the stored quality, not `total - bonus`.
        let assets = [
            asset("plain", 100, AssetKind::Photo),
            asset("captioned", 101, AssetKind::Photo),
        ];
        let score = |total: f32, bonus: f32| Score {
            total,
            bonus,
            quality: Some(0.70),
            ..Default::default()
        };
        let scores = HashMap::from([
            ("plain".to_string(), score(0.98, 0.28)),
            ("captioned".to_string(), score(1.0, 0.38)),
        ]);
        let ordered: Vec<&Asset> = assets.iter().collect();
        let rel = relative_scores(&ordered, |_| "x", &scores);
        assert!(rel["captioned"] > rel["plain"], "{rel:?}");
        // Older scores without `quality` fall back to `total - bonus`.
        assert!((with_bonus(0.6, 0.1).tech_quality() - 0.6).abs() < 1e-6);
    }

    #[test]
    fn same_moment_needs_a_burst_or_close_time_with_similar_pixels() {
        let shot = |id: &str, taken: i64, phash: Option<u64>| {
            let mut a = asset(id, taken, AssetKind::Photo);
            a.phash = phash;
            a
        };
        let base = shot("a", 1_000, Some(0));
        // A burst counts even without pixels.
        assert!(same_moment(
            &base,
            &shot("b", 1_000 + SAME_MOMENT_BURST_SECS, None)
        ));
        let later = 1_000 + SAME_MOMENT_SECS;
        assert!(same_moment(&base, &shot("c", later, Some(0))));
        assert!(!same_moment(&base, &shot("d", later, Some(u64::MAX))));
        assert!(!same_moment(&base, &shot("e", later, None)));
        assert!(!same_moment(&base, &shot("f", later + 1, Some(0))));
        let mut undated = shot("g", 1_000, Some(0));
        undated.taken_at = None;
        assert!(!same_moment(&base, &undated));
    }

    #[test]
    fn near_identical_pair_in_a_chapter_is_not_used_twice_when_budget_is_tight() {
        // Two takes of one clip's moment 10 s apart, plus an unrelated clip
        // 40 min later, all in one chapter.
        let clip = |id: &str, taken: i64, phash: u64| {
            let mut a = asset(id, taken, AssetKind::Video);
            a.duration_secs = Some(3.0);
            a.phash = Some(phash);
            a
        };
        let assets = vec![
            clip("a1", 1_000, 0xF0F0_F0F0_F0F0_F0F0),
            clip("a2", 1_010, 0xF0F0_F0F0_F0F0_F0F1),
            clip("a3", 3_400, 0x0F0F_0F0F_0F0F_0F0F),
        ];
        let scores: HashMap<String, Score> = [("a1", 0.9), ("a2", 0.88), ("a3", 0.6)]
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string(),
                    Score {
                        total: *v,
                        ..Default::default()
                    },
                )
            })
            .collect();
        let clusters = HashMap::new();
        let ids = |secs| -> Vec<String> {
            build_plan_with_length(&assets, &clusters, &scores, &[], FilmLength::Secs(secs))
                .hero
                .into_iter()
                .map(|e| e.asset_id)
                .collect()
        };
        // Title + two 3 s clips fits 7.5 s; three do not.
        assert_eq!(ids(7.5), vec!["a1", "a3"]);
        // Plenty of time: the twin is allowed back.
        assert_eq!(ids(600.0), vec!["a1", "a2", "a3"]);
    }

    #[test]
    fn explicit_length_is_a_minimum_and_auto_a_maximum() {
        // 8 chapters of 30 stills, with a few clips mixed in.
        let mut assets = vec![];
        let mut clusters = HashMap::new();
        for ch in 0..8 {
            for i in 0..30 {
                let id = format!("{}{i:02}", (b'a' + ch as u8) as char);
                let kind = if i % 10 == 0 {
                    AssetKind::Video
                } else {
                    AssetKind::Photo
                };
                let mut a = asset(&id, 1_714_557_600 + ch * 1800 + i as i64 * 30, kind);
                if kind == AssetKind::Video {
                    a.duration_secs = Some(5.0 + (i % 7) as f64);
                }
                clusters.insert(id, format!("day {ch}"));
                assets.push(a);
            }
        }
        for target in [120.0_f64, 240.0, 400.0] {
            let plan = build_plan_with_length(
                &assets,
                &clusters,
                &HashMap::new(),
                &[],
                FilmLength::Secs(target),
            );
            let secs = hero_secs(&plan, &clusters);
            assert!(
                (target..=target * (1.0 + LENGTH_OVERSHOOT)).contains(&secs),
                "target {target}: estimated {secs:.1}"
            );
        }
        let auto =
            build_plan_with_length(&assets, &clusters, &HashMap::new(), &[], FilmLength::Auto);
        let secs = hero_secs(&auto, &clusters);
        let want = auto_film_secs(1);
        assert!(secs <= want && secs > want * 0.9, "{secs} vs {want}");
    }

    #[test]
    fn route_and_places_cards_shift_the_estimate() {
        let e = [still_entry("a1")];
        let bare = estimate_hero_secs(&e, letter, |_| None, |_| false, NO_CARDS);
        let full = estimate_hero_secs(
            &e,
            letter,
            |_| None,
            |_| false,
            HeroCards {
                route: true,
                route_stops: 0,
                places: true,
            },
        );
        // title 2.5 + still 4 - 0.7 vs + map 3 + closing map 4.5 + places 4.5.
        assert!((bare - 5.8).abs() < 1e-9, "{bare}");
        assert!((full - bare - 10.0).abs() < 1e-9, "{full}");
        // No entries: nothing but the title, and no overlap to subtract.
        assert_eq!(
            estimate_hero_secs([], letter, |_| None, |_| false, NO_CARDS),
            EST_TITLE_SECS
        );
    }

    // -- scene-split sibling windows --------------------------------------

    const T0: i64 = 1_714_557_600;

    /// A 60 s clip with three scene windows (best first by quality: 0 > 1 > 2
    /// unless `qualities` says otherwise), plus photos taken during and
    /// after it.
    fn clip_fixture(qualities: [f32; 3], photo_offsets: &[i64]) -> (Vec<Asset>, Score) {
        let mut v = asset("v", T0, AssetKind::Video);
        v.duration_secs = Some(60.0);
        let score = Score {
            total: 0.9,
            clip_windows: vec![
                span(8.0, 14.0, qualities[0]),
                span(28.0, 34.0, qualities[1]),
                span(48.0, 54.0, qualities[2]),
            ],
            ..Default::default()
        };
        v.score = Some(score.clone());
        let mut assets = vec![v];
        for (i, off) in photo_offsets.iter().enumerate() {
            let mut p = asset(&format!("p{i}"), T0 + off, AssetKind::Photo);
            p.score = Some(Score {
                total: 0.6,
                ..Default::default()
            });
            assets.push(p);
        }
        (assets, score)
    }

    fn one_chapter(assets: &[Asset]) -> HashMap<String, String> {
        assets
            .iter()
            .map(|a| (a.id.clone(), "2024-05-01".to_string()))
            .collect()
    }

    fn windows_of(plan: &Plan, id: &str) -> Vec<(u32, f64, f64)> {
        plan.hero
            .iter()
            .filter(|e| e.asset_id == id)
            .map(|e| match e.rendition {
                Rendition::Clip {
                    trim_start,
                    trim_end,
                } => (e.window, trim_start, trim_end),
                ref other => panic!("not a clip: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn plentiful_budget_plays_every_scene_window_of_a_long_clip() {
        let (assets, _) = clip_fixture([0.9, 0.8, 0.7], &[20, 40, 100]);
        let co = one_chapter(&assets);
        let plan =
            build_plan_with_length(&assets, &co, &HashMap::new(), &[], FilmLength::Secs(600.0));
        assert_eq!(plan.plan_version, PLAN_VERSION);
        assert_eq!(
            windows_of(&plan, "v"),
            vec![(0, 8.0, 14.0), (1, 28.0, 34.0), (2, 48.0, 54.0)]
        );
        // Photos taken while the clip ran sit between its windows: no two
        // windows play back to back.
        let ids: Vec<(&str, u32)> = plan.hero.iter().map(PlanEntry::key).collect();
        assert_eq!(
            ids,
            [
                ("v", 0),
                ("p0", 0),
                ("v", 1),
                ("p1", 0),
                ("v", 2),
                ("p2", 0)
            ]
        );
        // Entries have unique identities.
        let keys: HashSet<_> = ids.iter().collect();
        assert_eq!(keys.len(), ids.len());
    }

    #[test]
    fn sibling_windows_stay_adjacent_only_when_nothing_was_captured_between() {
        let (assets, _) = clip_fixture([0.9, 0.8, 0.7], &[100]);
        let co = one_chapter(&assets);
        let plan =
            build_plan_with_length(&assets, &co, &HashMap::new(), &[], FilmLength::Secs(600.0));
        let ids: Vec<(&str, u32)> = plan.hero.iter().map(PlanEntry::key).collect();
        assert_eq!(ids, [("v", 0), ("v", 1), ("v", 2), ("p0", 0)]);
    }

    #[test]
    fn sibling_windows_without_a_capture_time_stay_put() {
        let (mut assets, _) = clip_fixture([0.9, 0.8, 0.7], &[20, 40]);
        assets[0].taken_at = None;
        let by_id: HashMap<&str, &Asset> = assets.iter().map(|a| (a.id.as_str(), a)).collect();
        let v = plan_entries(&assets[0], assets[0].score.as_ref());
        let p = |id: &str| plan_entry(by_id[id], None);
        let mut hero = vec![v[0].clone(), v[1].clone(), p("p0"), v[2].clone(), p("p1")];
        let keys = |h: &[PlanEntry]| {
            h.iter()
                .map(|e| (e.asset_id.clone(), e.window))
                .collect::<Vec<_>>()
        };
        let before = keys(&hero);
        place_sibling_windows(&mut hero, &by_id, |_| "c");
        assert_eq!(keys(&hero), before);
    }

    #[test]
    fn tight_budget_prefers_one_window_per_clip_and_the_best_one() {
        // Window 1 is the clip's best; two 6 s clips fit, three do not.
        let cands = vec![
            clip_entry("a", 6.0),
            PlanEntry {
                window: 1,
                ..clip_entry("a", 6.0)
            },
            clip_entry("b", 6.0),
        ];
        let score_of = |e: &PlanEntry| match (e.asset_id.as_str(), e.window) {
            ("a", 1) => 0.91,
            ("a", _) => 0.90,
            _ => 0.5,
        };
        let budget = |ceiling_secs| HeroBudget {
            ceiling_secs,
            cards: NO_CARDS,
        };
        // One chapter per letter, so `a`'s windows compete with each other.
        let run = |ceiling| {
            select_hero_by_budget(
                cands.clone(),
                letter,
                score_of,
                |_, _| false,
                |_| None,
                |_| false,
                budget(ceiling),
            )
            .iter()
            .map(|e| (e.asset_id.clone(), e.window))
            .collect::<Vec<_>>()
        };
        // Chapter a's best entry is window 1; chapter b's is b. The sibling
        // waits for the second pass, so a roomy budget adds it.
        assert_eq!(run(15.0), [("a".to_string(), 1), ("b".to_string(), 0)]);
        assert_eq!(run(60.0).len(), 3);
    }

    /// Every film length in `lengths` keeps the estimate within the
    /// overshoot, unless the film is down to one entry.
    fn assert_fits_length(assets: &[Asset], lengths: impl IntoIterator<Item = f64>) {
        let co = one_chapter(assets);
        for secs in lengths {
            let plan =
                build_plan_with_length(assets, &co, &HashMap::new(), &[], FilmLength::Secs(secs));
            let est = hero_secs(&plan, &co);
            assert!(
                est <= secs * (1.0 + LENGTH_OVERSHOOT) || plan.hero.len() == 1,
                "{secs}: {est}"
            );
        }
    }

    #[test]
    fn extra_windows_do_not_overrun_the_film_length() {
        let (assets, _) = clip_fixture([0.9, 0.8, 0.7], &[20, 40, 100]);
        assert_fits_length(&assets, [30.0, 45.0, 90.0]);
    }

    #[test]
    fn placed_windows_do_not_split_collages_past_the_ceiling() {
        // Photo runs between every window: placement breaks what would be
        // one collage after an adjacent run of windows into solo segments,
        // so the budget must be checked against the placed order.
        let (assets, _) = clip_fixture([0.9, 0.8, 0.7], &[16, 18, 20, 36, 38, 40, 56, 57, 58]);
        assert_fits_length(&assets, (20..=120).map(f64::from));
    }

    #[test]
    fn single_window_and_unscored_clips_keep_one_entry() {
        let (mut assets, mut score) = clip_fixture([0.9, 0.8, 0.7], &[]);
        score.clip_windows.truncate(1);
        assets[0].score = Some(score);
        let co = one_chapter(&assets);
        let plan =
            build_plan_with_length(&assets, &co, &HashMap::new(), &[], FilmLength::Secs(600.0));
        assert_eq!(windows_of(&plan, "v"), vec![(0, 8.0, 14.0)]);
        // Reels never repeat an asset, whatever its windows.
        let (assets, _) = clip_fixture([0.9, 0.8, 0.7], &[]);
        let plan = build_plan(&assets, &one_chapter(&assets), &HashMap::new());
        assert_eq!(
            plan.reel_selects
                .iter()
                .filter(|e| e.asset_id == "v")
                .count(),
            1
        );
    }

    #[test]
    fn primary_entry_uses_the_best_window_and_remembers_its_index() {
        let (assets, score) = clip_fixture([0.5, 0.9, 0.7], &[]);
        let e = plan_entry(&assets[0], Some(&score));
        assert_eq!(e.window, 1);
        assert_eq!(
            e.rendition,
            Rendition::Clip {
                trim_start: 28.0,
                trim_end: 34.0
            }
        );
        // Converted old single windows have quality 0 and still resolve.
        let old = Score {
            clip_windows: vec![span(2.0, 8.0, 0.0)],
            ..Default::default()
        };
        assert_eq!(plan_entry(&assets[0], Some(&old)).window, 0);
    }

    fn sibling_plan() -> (Vec<Asset>, Plan) {
        let (assets, _) = clip_fixture([0.9, 0.8, 0.7], &[20, 100]);
        let co = one_chapter(&assets);
        let plan =
            build_plan_with_length(&assets, &co, &HashMap::new(), &[], FilmLength::Secs(600.0));
        assert_eq!(plan.hero.iter().filter(|e| e.asset_id == "v").count(), 3);
        (assets, plan)
    }

    #[test]
    fn excluding_a_clip_removes_every_window_and_restoring_brings_them_back() {
        let (assets, plan) = sibling_plan();
        let mut edits = PlanEdits::default();
        edits.set_excluded("v".into(), true);
        let out = apply_edits(&plan, &assets, &edits);
        assert!(out.hero.iter().all(|e| e.asset_id != "v"));
        edits.set_excluded("v".into(), false);
        let out = apply_edits(&plan, &assets, &edits);
        assert_eq!(out.hero.iter().filter(|e| e.asset_id == "v").count(), 3);
    }

    #[test]
    fn swapping_a_clip_replaces_all_its_windows_with_the_replacement() {
        let (mut assets, plan) = sibling_plan();
        let mut extra = asset("p9", T0 + 200, AssetKind::Photo);
        extra.score = Some(Score {
            total: 0.5,
            ..Default::default()
        });
        assets.push(extra);
        let mut base = plan.clone();
        base.chapters[0].asset_ids.push("p9".into());
        let mut edits = PlanEdits::default();
        edits.swap("v".into(), "p9".into());
        let out = apply_edits(&base, &assets, &edits);
        assert_eq!(out.hero.iter().filter(|e| e.asset_id == "p9").count(), 1);
        assert!(out.hero.iter().all(|e| e.asset_id != "v"));
        // The replacement took the first window's slot.
        assert_eq!(out.hero[0].asset_id, "p9");
        assert_eq!(out.hero.len(), plan.hero.len() - 2);
    }

    #[test]
    fn pinning_a_multi_window_clip_adds_one_entry_with_its_best_window() {
        let (assets, plan) = sibling_plan();
        let mut base = plan;
        base.hero.retain(|e| e.asset_id != "v");
        let mut edits = PlanEdits::default();
        edits.set_included("v".into(), true);
        let out = apply_edits(&base, &assets, &edits);
        let v: Vec<_> = out.hero.iter().filter(|e| e.asset_id == "v").collect();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].window, 0, "window 0 has the best quality here");
        // Already in the film (as siblings): a pin is a no-op.
        let (assets, plan) = sibling_plan();
        let out = apply_edits(&plan, &assets, &edits);
        assert_eq!(out.hero.len(), plan.hero.len());
    }

    #[test]
    fn captions_belong_to_the_asset_so_siblings_share_them() {
        let (assets, plan) = sibling_plan();
        let mut edits = PlanEdits::default();
        edits.set_caption("v".into(), Some("Walking in".into()));
        let out = apply_edits(&plan, &assets, &edits);
        assert_eq!(out.captions["v"], "Walking in");
    }

    #[test]
    fn plans_without_window_indices_load_as_window_zero() {
        let (assets, plan) = sibling_plan();
        let json = plan.to_json().unwrap();
        let back = Plan::from_json(&json).unwrap();
        assert_eq!(
            back.hero.iter().map(PlanEntry::key).collect::<Vec<_>>(),
            plan.hero.iter().map(PlanEntry::key).collect::<Vec<_>>()
        );
        // A stored v2 entry has no `window` key.
        let v2 = r#"{"asset_id":"x","content_hash":null,"size_bytes":1,"mtime":2,
            "rendition":"still"}"#;
        let e: PlanEntry = serde_json::from_str(v2).unwrap();
        assert_eq!(e.key(), ("x", 0));
        // A v2 plan still applies edits unchanged.
        let mut old = plan;
        old.plan_version = 2;
        old.hero.retain(|e| e.window == 0);
        let out = apply_edits(&old, &assets, &PlanEdits::default());
        assert_eq!(out.hero.len(), old.hero.len());
    }
}
