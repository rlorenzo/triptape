//! Stage 7 — Plan: one versioned AI Plan JSON for all three renderers.
//!
//! Hero shot list in story order, 3–5 reel selects varied by day/place,
//! chapters by day/place, template captions, map polyline + day markers,
//! music pick with rationale. Per-entry identity (asset id, content hash,
//! size, mtime, rendition) feeds the pre-render source-integrity check.
//! Sec 5.4 degradation eligibility lives here too.

use crate::clip_window::MAX_WINDOW_SECS;
use crate::cluster::{day_of, gps_outliers_in, same_day, story_positions, story_tracks};
use crate::embedding::{same_scene, Embedding};
use crate::party::Track;
use crate::types::{
    distinct_assets, Asset, AssetKind, Chapter, CollageEdits, MusicPick, Plan, PlanEntry,
    Rendition, Score,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

/// Weekday + date as shown to viewers, `Wed, May 1`.
const DAY_FORMAT: &str = "%a, %b %-d";

/// Factual caption dateline, `Wed, May 1 · 10:00 AM` (UTC), used by the
/// local caption fallback when no place is known. `None` when `taken_at`
/// (unix seconds) is out of range.
pub fn dateline(taken_at: i64) -> Option<String> {
    let dt = crate::metadata::local_datetime(taken_at, None)?;
    Some(format!(
        "{} · {}",
        dt.strftime(DAY_FORMAT),
        dt.strftime("%-I:%M %p")
    ))
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
const PLACES_CARD_MIN_SECS: f64 = 4.5;
/// Longest the closing places card holds, however many places.
pub const PLACES_CARD_MAX_SECS: f64 = 9.0;
/// Places card hold before reading time.
const PLACES_CARD_BASE_SECS: f64 = 3.0;
/// Reading time per place name on the closing card.
const PLACES_CARD_SECS_PER_PLACE: f64 = 0.2;

/// How long the closing places card holds for `places` names: long enough
/// to read them (28 names were on screen for 4.5 s), within a ceiling.
/// Shared by the renderer and the planner's estimate.
pub fn places_card_secs(places: usize) -> f64 {
    (PLACES_CARD_BASE_SECS + PLACES_CARD_SECS_PER_PLACE * places as f64)
        .clamp(PLACES_CARD_MIN_SECS, PLACES_CARD_MAX_SECS)
}

/// Longest a caption may stretch its shot.
pub const CAPTION_MAX_SECS: f64 = 8.0;
/// Reading time per caption word, on top of [`CAPTION_BASE_SECS`].
const CAPTION_SECS_PER_WORD: f64 = 0.3;
/// Time to notice a caption before reading it.
const CAPTION_BASE_SECS: f64 = 1.5;

/// Seconds a shot must stay on screen for its caption to be read: a
/// 25-word caption on a 1.6 s Live Photo could not be. `0.0` without one.
/// Shared by the renderer and the planner's estimate.
pub fn caption_read_secs(caption: Option<&str>) -> f64 {
    let words = caption.map_or(0, |c| c.split_whitespace().count());
    if words == 0 {
        return 0.0;
    }
    (CAPTION_BASE_SECS + CAPTION_SECS_PER_WORD * words as f64).min(CAPTION_MAX_SECS)
}
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
pub const SAME_MOMENT_SECS: i64 = 120;
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

/// Most other hero entries a collage may reach across: between its first
/// and last member at most this many entries (clips, Live Photo motion,
/// panoramas, captioned stills) play on their own. The collage plays at its
/// first member's position, so no member moves earlier than this many
/// segments (roughly 10-20 s of film), and never into another chapter.
pub const COLLAGE_REACH: usize = 3;

/// Most seconds of capture time between a collage's first photo and any
/// other photo it joins automatically. A chapter can be a whole day, and a
/// sparse evening pick must not be pulled into a midday collage; half an
/// hour keeps a collage to one outing. A photo with no capture time never
/// joins automatically. The user's own joins are not bound by it.
pub const COLLAGE_WINDOW_SECS: i64 = 30 * 60;

/// One hero entry, in playback order, as [`collage_groups`] sees it.
/// Build them with [`collage_slots`], which applies the user's edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupSlot<C> {
    /// The entry's chapter; a collage never spans two chapters.
    pub chapter: C,
    /// Whether the entry may share a collage: a [`Candidacy::Plain`]
    /// entry, or a [`Candidacy::IfJoined`] one the user joined, and not set
    /// to play on its own by the user.
    pub eligible: bool,
    /// The user joined this photo to the previous collage photo (the
    /// nearest earlier eligible slot), however many
    /// entries lie between them. Honoured within one chapter, and while the
    /// photos joined this way in a row number at most [`MAX_COLLAGE`].
    pub join_prev: bool,
    /// Capture time (unix seconds): automatic collages stay within
    /// [`COLLAGE_WINDOW_SECS`] of their first photo.
    pub taken_at: Option<i64>,
}

/// Grouping slots for hero entries in playback order, given as
/// `(asset id, chapter, candidacy, capture time)`, with the user's collage `edits`
/// applied: a `solo` photo arrives ineligible, so it plays on its own, a
/// [`Candidacy::IfJoined`] entry (captioned still, Live Photo motion) is
/// eligible only when the user joined it to a neighbouring candidate, and a
/// `joins` link between a photo and the previous collage photo sets its
/// `join_prev`. Links between
/// photos that are no longer neighbouring collage photos (one was removed,
/// captioned or set solo, or a photo now sits between them) do nothing,
/// like a swap whose asset is gone. The planner estimate, the renderer and
/// the Story editor all build their slots here.
pub fn collage_slots<'a, C: PartialEq>(
    entries: impl IntoIterator<Item = (&'a str, C, Candidacy, Option<i64>)>,
    edits: &CollageEdits,
) -> Vec<GroupSlot<C>> {
    let entries: Vec<_> = entries.into_iter().collect();
    let candidate = |i: usize| entries[i].2 != Candidacy::Never && !edits.is_solo(entries[i].0);
    let plain = |i: usize| entries[i].2 == Candidacy::Plain && candidate(i);
    // An `IfJoined` entry is eligible while a link of its still holds: it
    // is linked to its nearest eligible neighbour, before or after, in its
    // chapter. Start from every candidate and drop the ones whose links
    // can't hold until none change, so a saved join that a move broke
    // never puts the photo beside one it wasn't joined to.
    let mut can_join: Vec<bool> = (0..entries.len()).map(candidate).collect();
    let nearest = |can: &[bool], i: usize, side: &mut dyn Iterator<Item = usize>| {
        side.take_while(|&j| entries[j].1 == entries[i].1)
            .find(|&j| can[j])
    };
    loop {
        let mut changed = false;
        for i in 0..entries.len() {
            if !can_join[i] || plain(i) {
                continue;
            }
            let held = [
                nearest(&can_join, i, &mut (0..i).rev()),
                nearest(&can_join, i, &mut (i + 1..entries.len())),
            ]
            .into_iter()
            .flatten()
            .any(|j| edits.linked(entries[j].0, entries[i].0));
            if !held {
                can_join[i] = false;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut prev: Option<&str> = None;
    entries
        .into_iter()
        .enumerate()
        .map(|(i, (id, chapter, _, taken_at))| {
            let eligible = can_join[i];
            let join_prev = eligible && prev.is_some_and(|p| edits.linked(p, id));
            if eligible {
                prev = Some(id);
            }
            GroupSlot {
                chapter,
                eligible,
                join_prev,
                taken_at,
            }
        })
        .collect()
}

/// Whether `b` is close enough in capture time to `a` (a collage's first
/// photo, or the shot that last showed a caption) to count as the same
/// moment: both dated, within [`COLLAGE_WINDOW_SECS`].
pub fn within_window(a: Option<i64>, b: Option<i64>) -> bool {
    matches!((a, b), (Some(x), Some(y)) if (x - y).abs() <= COLLAGE_WINDOW_SECS)
}

/// Why a hero entry plays on its own unless the user joins it to a
/// neighbour (a clip or a panorama never joins; see [`Candidacy`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaysAlone {
    /// A video clip.
    Clip,
    /// A Live Photo playing its motion.
    Motion,
    /// A still with a caption: a caption describes one photo.
    Caption,
    /// A panorama: it pans solo.
    Panorama,
}

/// Why a hero entry plays on its own without the user's say, if it does:
/// only a still that is not a panorama and has no caption joins on its own.
/// A panorama reports [`PlaysAlone::Panorama`] whatever its caption, since a
/// caption never makes it joinable.
pub fn plays_alone(rendition: &Rendition, panorama: bool, captioned: bool) -> Option<PlaysAlone> {
    match rendition {
        Rendition::Clip { .. } => Some(PlaysAlone::Clip),
        Rendition::Motion { .. } => Some(PlaysAlone::Motion),
        Rendition::Still if panorama => Some(PlaysAlone::Panorama),
        Rendition::Still if captioned => Some(PlaysAlone::Caption),
        Rendition::Still => None,
    }
}

/// How a hero entry may take part in a collage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Candidacy {
    /// A plain still: the planner groups it with its neighbours.
    Plain,
    /// A captioned still or a Live Photo playing its motion: plays on its
    /// own unless the user joined it to a neighbour (the collage then
    /// shows its caption, and a Live Photo plays its motion in its cell).
    IfJoined,
    /// A clip or a panorama: always plays on its own.
    Never,
}

impl Candidacy {
    /// The candidacy of an entry that plays alone for `why` ([`plays_alone`]).
    pub fn of(why: Option<PlaysAlone>) -> Self {
        match why {
            None => Self::Plain,
            Some(PlaysAlone::Motion | PlaysAlone::Caption) => Self::IfJoined,
            Some(PlaysAlone::Clip | PlaysAlone::Panorama) => Self::Never,
        }
    }
}

/// An entry's [`Candidacy`]. The renderer, [`estimate_hero_secs`] and the
/// Story editor all decide through this (or [`entry_candidacy`]).
pub fn collage_candidacy(rendition: &Rendition, panorama: bool, captioned: bool) -> Candidacy {
    Candidacy::of(plays_alone(rendition, panorama, captioned))
}

/// [`collage_candidacy`] from what the renderer knows of an entry (see
/// [`entry_plays_alone`]).
pub fn entry_candidacy(
    rendition: &Rendition,
    aspect: Option<f64>,
    caption: Option<&str>,
) -> Candidacy {
    Candidacy::of(entry_plays_alone(rendition, aspect, caption))
}

/// [`plays_alone`] from what the renderer knows of an entry: its
/// rendition, its display aspect (width / height, rotation applied) and its
/// caption. The renderer's resolved entries and the Story editor both
/// decide through this, so the editor shows the collages the film will play.
pub fn entry_plays_alone(
    rendition: &Rendition,
    aspect: Option<f64>,
    caption: Option<&str>,
) -> Option<PlaysAlone> {
    plays_alone(
        rendition,
        aspect.is_some_and(is_panorama),
        has_caption(caption),
    )
}

/// Each asset's chapter in `plan`: the first of `plan.chapters` holding it.
pub fn chapter_index(plan: &Plan) -> HashMap<&str, usize> {
    let mut out: HashMap<&str, usize> = HashMap::new();
    for (i, c) in plan.chapters.iter().enumerate() {
        for id in &c.asset_ids {
            out.entry(id).or_insert(i);
        }
    }
    out
}

/// Why a photo playing on its own can't join a neighbour
/// ([`HeroGrouping::join_partner`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinBlock {
    /// No other collage photo of its chapter on that side.
    NoNeighbour,
    /// The neighbour's collage holds [`MAX_COLLAGE`] photos already.
    Full,
}

/// A plan's hero as the renderer will group it, for showing and editing
/// collages without rendering. Built from the edited plan: each entry's
/// chapter is the first of `plan.chapters` holding it, its caption the
/// plan's, and only entries whose source is `present` play (the export
/// skips missing files before the renderer groups the rest).
pub struct HeroGrouping {
    /// Hero indices that play, in playback order.
    pub playing: Vec<usize>,
    /// Per hero index: chapter and why it plays alone, if it must.
    chapter: Vec<usize>,
    alone: Vec<Option<PlaysAlone>>,
    ids: Vec<String>,
    taken_at: Vec<Option<i64>>,
}

impl HeroGrouping {
    /// Grouping of `plan` (normally an edited plan) over its project's
    /// `assets`: their display aspects, and whether each source is on disk.
    pub fn from_assets(plan: &Plan, assets: &[Asset]) -> Self {
        let by_id: HashMap<&str, &Asset> = assets.iter().map(|a| (a.id.as_str(), a)).collect();
        Self::new(
            plan,
            |id| by_id.get(id).and_then(|a| a.display_aspect()),
            |id| by_id.get(id).and_then(|a| a.taken_at),
            |id| by_id.get(id).is_some_and(|a| a.path.exists()),
        )
    }

    /// `aspect_of` gives an asset's display aspect, `taken_of` its capture
    /// time, `present` whether its source is on disk.
    pub fn new(
        plan: &Plan,
        aspect_of: impl Fn(&str) -> Option<f64>,
        taken_of: impl Fn(&str) -> Option<i64>,
        present: impl Fn(&str) -> bool,
    ) -> Self {
        let chapter_of = chapter_index(plan);
        let hero = &plan.hero;
        Self {
            playing: (0..hero.len())
                .filter(|&i| present(&hero[i].asset_id))
                .collect(),
            chapter: hero
                .iter()
                .map(|e| {
                    chapter_of
                        .get(e.asset_id.as_str())
                        .copied()
                        .unwrap_or(usize::MAX)
                })
                .collect(),
            alone: hero
                .iter()
                .map(|e| {
                    entry_plays_alone(
                        &e.rendition,
                        aspect_of(&e.asset_id),
                        plan.captions.get(&e.asset_id).map(String::as_str),
                    )
                })
                .collect(),
            ids: hero.iter().map(|e| e.asset_id.clone()).collect(),
            taken_at: hero.iter().map(|e| taken_of(&e.asset_id)).collect(),
        }
    }

    /// The hero entry's chapter (index into `plan.chapters`).
    pub fn chapter(&self, hero_index: usize) -> usize {
        self.chapter[hero_index]
    }

    /// Why the hero entry plays on its own unless the user joins it
    /// ([`entry_plays_alone`]), if it does.
    pub fn plays_alone(&self, hero_index: usize) -> Option<PlaysAlone> {
        self.alone[hero_index]
    }

    /// Whether the hero entry is a partner [`Self::join_partner`] offers
    /// under `collage`: a plain still not set solo by the user. Captioned
    /// stills and Live Photo motion can be joined too (the CLI's `--join`),
    /// but the editor does not offer them as neighbours.
    fn collage_photo(&self, hero_index: usize, collage: &CollageEdits) -> bool {
        self.alone[hero_index].is_none() && !collage.is_solo(&self.ids[hero_index])
    }

    /// The neighbouring collage photo hero entry `i` could join, before it
    /// (`back`) or after it: the nearest playing collage photo of its
    /// chapter, as [`collage_slots`] links them, if joining them under
    /// `collage` puts the two in one collage.
    pub fn join_partner(
        &self,
        collage: &CollageEdits,
        i: usize,
        back: bool,
    ) -> Result<usize, JoinBlock> {
        let pos = self
            .playing
            .binary_search(&i)
            .map_err(|_| JoinBlock::NoNeighbour)?;
        let in_chapter = |k: &&usize| self.chapter[**k] == self.chapter[i];
        let mut side: Box<dyn Iterator<Item = &usize>> = if back {
            Box::new(self.playing[..pos].iter().rev().take_while(in_chapter))
        } else {
            Box::new(self.playing[pos + 1..].iter().take_while(in_chapter))
        };
        let k = *side
            .find(|&&k| self.collage_photo(k, collage))
            .ok_or(JoinBlock::NoNeighbour)?;
        let (a, b) = if back { (k, i) } else { (i, k) };
        let mut joined = collage.clone();
        joined.join(self.ids[a].clone(), self.ids[b].clone());
        self.groups(&joined)
            .iter()
            .any(|g| g.contains(&a) && g.contains(&b))
            .then_some(k)
            .ok_or(JoinBlock::Full)
    }

    /// The playing entries' segments under `collage` (normally the edited
    /// plan's own `plan.collage`), as hero indices: [`collage_slots`] and
    /// [`collage_groups`], exactly as the renderer calls them.
    pub fn groups(&self, collage: &CollageEdits) -> Vec<Vec<usize>> {
        let slots = collage_slots(
            self.playing.iter().map(|&i| {
                (
                    self.ids[i].as_str(),
                    self.chapter[i],
                    Candidacy::of(self.alone[i]),
                    self.taken_at[i],
                )
            }),
            collage,
        );
        collage_groups(&slots)
            .into_iter()
            .map(|g| g.into_iter().map(|k| self.playing[k]).collect())
            .collect()
    }
}

/// How hero entries play: each group is one segment, a list of indices
/// into `slots` in ascending (playback) order. A group of one plays solo; a
/// longer group is a collage of eligible stills. Groups come back ordered
/// by their first member, which is where the segment plays, and every
/// index appears in exactly one group.
///
/// Eligible stills of one chapter are grouped even when other entries sit
/// between them, as long as a group spans at most [`COLLAGE_REACH`] other
/// entries and no entry of another chapter, and every member was taken
/// within [`COLLAGE_WINDOW_SECS`] of the group's first; those entries keep
/// their own positions. Among the ways to split a chapter's stills into groups of at
/// most [`MAX_COLLAGE`], the chosen one leaves the fewest stills solo, then
/// moves stills across the fewest entries, then makes the fewest segments,
/// then balances group sizes (five stills in a row play as 3 + 2, not 4 + 1).
///
/// The user's overrides come first: a photo set solo arrives ineligible
/// (see [`collage_slots`]), and a `join_prev` link keeps its photo in the previous
/// collage photo's group even beyond the reach and the time window
/// (entries it crosses do not count against the reach); links still never cross a chapter, and a row
/// of linked photos longer than [`MAX_COLLAGE`] drops the links past it.
/// The planner's length estimate, the renderer and the Story editor all
/// group through this, so the planned length matches the film.
pub fn collage_groups<C: PartialEq>(slots: &[GroupSlot<C>]) -> Vec<Vec<usize>> {
    // Runs of collage photos in which each neighbouring pair could share a
    // group, with whether each is linked to the one before it; a run never
    // crosses a chapter change.
    let mut runs: Vec<(Vec<usize>, Vec<bool>)> = vec![];
    let mut groups: Vec<Vec<usize>> = vec![];
    // Photos in the current row of linked photos.
    let mut linked_row = 0;
    for (i, s) in slots.iter().enumerate() {
        if !s.eligible {
            groups.push(vec![i]);
            continue;
        }
        let prev = runs.last().and_then(|(r, _)| r.last().copied());
        let same_chapter = prev.is_some_and(|p| slots[p..i].iter().all(|o| o.chapter == s.chapter));
        let linked = same_chapter && s.join_prev && linked_row < MAX_COLLAGE;
        let joins = linked
            || (same_chapter
                && prev.is_some_and(|p| {
                    i - p - 1 <= COLLAGE_REACH && within_window(slots[p].taken_at, s.taken_at)
                }));
        linked_row = if linked { linked_row + 1 } else { 1 };
        match runs.last_mut() {
            Some((r, l)) if joins => {
                r.push(i);
                l.push(linked);
            }
            _ => runs.push((vec![i], vec![false])),
        }
    }
    for (run, linked) in &runs {
        let times: Vec<Option<i64>> = run.iter().map(|&i| slots[i].taken_at).collect();
        groups.extend(split_run(run, linked, &times));
    }
    groups.sort_unstable_by_key(|g| g[0]);
    groups
}

/// Best split of one run (ascending entry indices) into consecutive
/// groups, never between a photo and the one it is `linked` to, with every
/// unlinked member within [`COLLAGE_WINDOW_SECS`] of its group's first
/// (`times`, per run member); see [`collage_groups`] for the order of
/// preferences.
fn split_run(run: &[usize], linked: &[bool], times: &[Option<i64>]) -> Vec<Vec<usize>> {
    // Cost of a split, compared in order: solo stills, entries crossed,
    // segments, sum of squared sizes (lower is more balanced). On a full
    // tie the larger group comes first (3 + 2, not 2 + 3).
    type Cost = (usize, usize, usize, usize);
    // best[j]: cheapest split of run[..j] and where its last group starts.
    let mut best: Vec<Option<(Cost, usize)>> = vec![None; run.len() + 1];
    best[0] = Some(((0, 0, 0, 0), 0));
    for j in 1..=run.len() {
        for start in (j.saturating_sub(MAX_COLLAGE)..j).rev() {
            if linked[start] {
                // A group may not start between two linked photos.
                continue;
            }
            let n = j - start;
            if !(start + 1..j).all(|k| linked[k] || within_window(times[start], times[k])) {
                continue;
            }
            // Entries crossed between members; a user link crosses freely.
            let crossed: usize = (start + 1..j)
                .filter(|&k| !linked[k])
                .map(|k| run[k] - run[k - 1] - 1)
                .sum();
            let Some((b, _)) = best[start].filter(|_| crossed <= COLLAGE_REACH) else {
                continue;
            };
            let cost = (
                b.0 + usize::from(n == 1),
                b.1 + crossed,
                b.2 + 1,
                b.3 + n * n,
            );
            if best[j].is_none_or(|(c, _)| cost < c) {
                best[j] = Some((cost, start));
            }
        }
    }
    let mut out = vec![];
    let mut j = run.len();
    while j > 0 {
        // Each row of linked photos (at most MAX_COLLAGE, crossing nothing
        // that counts, bound by no window) can be a group of its own, and a
        // lone photo always can, so the run always splits.
        let (_, start) = best[j].expect("every run splits");
        out.push(run[start..j].to_vec());
        j = start;
    }
    out.reverse();
    out
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
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HeroCards {
    /// Closing route map (the plan has a GPS route).
    pub route: bool,
    /// The opening route map card after the title too (off by default:
    /// the film opens on its first shot and saves the map for the end).
    pub opening_map: bool,
    /// Chapter stops the closing map travels between; fewer than two keep
    /// the static closing card (see [`crate::route_stops::closing_map_secs`]).
    pub route_stops: usize,
    /// Closing map length when the journey's own schedule sets it (an
    /// itinerary recap); `None` derives it from `route_stops`.
    pub route_secs: Option<f64>,
    /// Distinct places on the closing card (0: no card).
    pub places: usize,
}

/// Screen seconds one entry takes on its own (a still joining a collage
/// costs less than this; see [`estimate_hero_secs`]).
fn solo_secs(e: &PlanEntry, pano_secs_of: impl Fn(&str) -> Option<f64>, read_secs: f64) -> f64 {
    let secs = match e.rendition {
        Rendition::Still => pano_secs_of(&e.asset_id).unwrap_or(EST_STILL_SECS),
        Rendition::Motion {
            trim_start,
            trim_end,
        }
        | Rendition::Clip {
            trim_start,
            trim_end,
        } => (trim_end - trim_start).clamp(MIN_SEGMENT_SECS, MAX_CLIP_SECS),
    };
    // A caption holds its shot until it can be read.
    secs.max(read_secs)
}

/// What one rendered segment is, for transition overlaps.
#[derive(Clone, Copy, PartialEq)]
enum Seg<'c> {
    Title,
    Card,
    Chapter(&'c str),
}

/// Estimated rendered seconds for hero entries in story order, as the
/// renderer builds them: the fixed cards around the entries, same-chapter
/// stills grouped into collages by [`collage_groups`], clips padded to
/// [`MIN_SEGMENT_SECS`], and every cut overlapped by the transition the
/// renderer assigns it (opening and closing cut 0.7 s, chapter breaks and
/// card cuts cycling 0.8/0.5/0.5 s, others ~0.54 s). Panoramas and
/// captioned stills (`captioned`) always play solo.
pub fn estimate_hero_secs<'e, 'c>(
    entries: impl IntoIterator<Item = &'e PlanEntry>,
    chapter_of: impl Fn(&str) -> &'c str,
    pano_secs_of: impl Fn(&str) -> Option<f64>,
    caption_secs: impl Fn(&str) -> f64,
    taken_of: impl Fn(&str) -> Option<i64>,
    cards: HeroCards,
) -> f64 {
    estimate_hero_secs_with(
        entries,
        chapter_of,
        pano_secs_of,
        caption_secs,
        taken_of,
        cards,
        &CollageEdits::default(),
    )
}

/// Distinct place names the closing card lists (the film's own name for a
/// chapter when it has one).
fn distinct_places(chapters: &[Chapter]) -> usize {
    chapters
        .iter()
        .flat_map(|c| c.film_places())
        .collect::<HashSet<_>>()
        .len()
}

/// Estimated seconds of an (edited) plan's film, with the inputs the
/// planner budgets with: route and places cards from the plan, panorama
/// holds from `assets`, the plan's captions and its collage overrides.
pub fn estimate_plan_secs(plan: &Plan, assets: &[Asset]) -> f64 {
    let chapter = chapter_index(plan);
    // Chapter labels only need to tell chapters apart.
    let labels: Vec<String> = (0..=plan.chapters.len()).map(|i| i.to_string()).collect();
    let pano: HashMap<&str, f64> = assets
        .iter()
        .filter_map(|a| {
            let asp = a.display_aspect().filter(|&x| is_panorama(x))?;
            Some((a.id.as_str(), pano_motion(asp).hold_secs))
        })
        .collect();
    let taken: HashMap<&str, i64> = assets
        .iter()
        .filter_map(|a| Some((a.id.as_str(), a.taken_at?)))
        .collect();
    let cards = HeroCards {
        route: !plan.map_polyline.is_empty(),
        opening_map: false,
        route_stops: crate::route_stops::animated_stop_count(
            &crate::route_stops::chapter_route_stops(&plan.chapters, assets, &plan.map_polyline),
        ),
        route_secs: None,
        places: distinct_places(&plan.chapters),
    };
    estimate_hero_secs_with(
        &plan.hero,
        |id| labels[chapter.get(id).copied().unwrap_or(plan.chapters.len())].as_str(),
        |id| pano.get(id).copied(),
        |id| caption_read_secs(plan.captions.get(id).map(String::as_str)),
        |id| taken.get(id).copied(),
        cards,
        &plan.collage,
    )
}

/// [`estimate_hero_secs`] under the user's collage overrides (an edited
/// plan's `collage`), grouped as the renderer groups them.
pub fn estimate_hero_secs_with<'e, 'c>(
    entries: impl IntoIterator<Item = &'e PlanEntry>,
    chapter_of: impl Fn(&str) -> &'c str,
    pano_secs_of: impl Fn(&str) -> Option<f64>,
    caption_secs: impl Fn(&str) -> f64,
    taken_of: impl Fn(&str) -> Option<i64>,
    cards: HeroCards,
    collage: &CollageEdits,
) -> f64 {
    let mut segs: Vec<(Seg<'c>, f64)> = vec![(Seg::Title, EST_TITLE_SECS)];
    if cards.route && cards.opening_map {
        segs.push((Seg::Card, EST_MAP_SECS));
    }
    let first_hero = segs.len();
    let entries: Vec<&PlanEntry> = entries.into_iter().collect();
    let slots = collage_slots(
        entries.iter().map(|e| {
            (
                e.asset_id.as_str(),
                chapter_of(&e.asset_id),
                collage_candidacy(
                    &e.rendition,
                    pano_secs_of(&e.asset_id).is_some(),
                    caption_secs(&e.asset_id) > 0.0,
                ),
                taken_of(&e.asset_id),
            )
        }),
        collage,
    );
    for g in collage_groups(&slots) {
        let secs = match g.len() {
            1 => solo_secs(
                entries[g[0]],
                &pano_secs_of,
                caption_secs(&entries[g[0]].asset_id),
            ),
            n => collage_hold_secs(n, EST_STILL_SECS),
        };
        segs.push((Seg::Chapter(slots[g[0]].chapter), secs));
    }
    if segs.len() == first_hero {
        // No entries: the renderer refuses to build a film from cards alone.
        return segs.iter().map(|s| s.1).sum();
    }
    if cards.route {
        segs.push((
            Seg::Card,
            cards
                .route_secs
                .unwrap_or_else(|| crate::route_stops::closing_map_secs(cards.route_stops)),
        ));
    }
    if cards.places > 0 {
        segs.push((Seg::Card, places_card_secs(cards.places)));
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
/// Every chapter first gets its best entry, one per unit (dropping the
/// weakest chapters' picks only if even those overrun the ceiling). Remaining time is dealt
/// out one entry at a time to the chapter with the least estimated screen
/// time relative to `size^CHAPTER_TIME_EXPONENT`, taking that chapter's
/// best remaining entry, so big days get more screen time without crowding
/// out small ones. Where `unit_of` puts a chapter's entries in several
/// units (a split day's capture-order clusters), the chapter's weight is the
/// sum over its units instead, so telling a day party by party, in fewer
/// and larger chapters, does not shrink its share of the film.
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
#[allow(clippy::too_many_arguments)]
fn select_hero_by_budget<'a>(
    cands: Vec<PlanEntry>,
    chapter_of: impl Fn(&str) -> &'a str + Copy,
    unit_of: impl Fn(&str) -> Option<usize>,
    score_of: impl Fn(&PlanEntry) -> f32,
    similar: impl Fn(&str, &str) -> bool,
    pano_secs_of: impl Fn(&str) -> Option<f64> + Copy,
    caption_secs: impl Fn(&str) -> f64 + Copy,
    taken_of: impl Fn(&str) -> Option<i64> + Copy,
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
    // Each chapter's members grouped by unit, units in order of their best
    // member, members best first.
    let units: Vec<Vec<Vec<usize>>> = chapters
        .iter()
        .map(|(_, m)| {
            let mut slot: HashMap<Option<usize>, usize> = HashMap::new();
            let mut groups: Vec<Vec<usize>> = vec![];
            for &i in m {
                let k = *slot.entry(unit_of(&cands[i].asset_id)).or_insert_with(|| {
                    groups.push(vec![]);
                    groups.len() - 1
                });
                groups[k].push(i);
            }
            groups
        })
        .collect();
    // Share weight per chapter: size^exponent, summed over its units.
    let weight: Vec<f64> = units
        .iter()
        .map(|u| {
            u.iter()
                .map(|g| (g.len() as f64).powf(CHAPTER_TIME_EXPONENT))
                .sum()
        })
        .collect();
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
            caption_secs,
            taken_of,
            budget.cards,
        )
    };
    // Every chapter's best entry (each unit's, on a split day), weakest
    // dropped first if over budget (the strongest always stays, so the film
    // is never empty).
    let mut reps: Vec<usize> = units.iter().flatten().map(|g| g[0]).collect();
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
        .map(|t| {
            t.iter()
                .map(|&i| solo_secs(&cands[i], pano_secs_of, caption_secs(&cands[i].asset_id)))
                .sum()
        })
        .collect();
    let mut total = estimate(&picked);
    for allow_similar in [false, true] {
        let mut closed = vec![false; chapters.len()];
        loop {
            let next = chapters
                .iter()
                .enumerate()
                .filter(|(ci, _)| !closed[*ci])
                .min_by(|(a, _), (b, _)| {
                    let share = |ci: usize| secs[ci] / weight[ci];
                    share(*a).total_cmp(&share(*b)).then(a.cmp(b))
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
    pub start: jiff::civil::Date,
    pub end: jiff::civil::Date,
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
            jiff::civil::Date::strptime("%Y-%m-%d", s.trim())
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
    /// book flow are untouched. See [`apply_edits`] for placement; a pin
    /// lengthens the film rather than replacing a shot.
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
    /// Film picks the user placed right before or after another film pick
    /// of the same day, oldest first, at most one per moved asset. Film
    /// order only: chapters, reels and the book are untouched. Ignored
    /// while either asset is out of the film. See [`apply_moves`].
    #[serde(default)]
    pub moves: Vec<FilmMove>,
    /// Live Photos the user set to play their motion in the film ("Play as
    /// Live Photo") instead of the still the planner chose, oldest first.
    /// Applies to Live Photos with a companion video only; see
    /// [`apply_edits`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub motion: Vec<String>,
    /// Film photos the user set to play on their own ("Show on its own")
    /// or joined into one collage ("Join with next"), oldest first. Stored
    /// flat (`solo`, `joins`) beside the other edits. See [`collage_slots`].
    #[serde(flatten)]
    pub collage: CollageEdits,
    /// Book cover pins: asset id -> horizontal focus percent (see
    /// [`crate::types::Plan::cover`]).
    #[serde(default)]
    pub cover: BTreeMap<String, u8>,
    /// Per-photo crop focus (see [`crate::types::Plan::crop`]).
    #[serde(default)]
    pub crop: BTreeMap<String, (u8, u8)>,
    /// Photos pinned into the book (see [`crate::types::Plan::book`]).
    #[serde(default)]
    pub book: Vec<String>,
    /// The book's closing photo (see [`crate::types::Plan::closing`]).
    #[serde(default)]
    pub closing: Option<String>,
    /// Book layout choices (see [`crate::types::BookLayout`]).
    #[serde(default)]
    pub book_layout: crate::types::BookLayout,
}

impl PlanEdits {
    pub fn is_empty(&self) -> bool {
        self.excluded.is_empty()
            && self.included.is_empty()
            && self.swaps.is_empty()
            && self.place_overrides.is_empty()
            && self.caption_overrides.is_empty()
            && self.moves.is_empty()
            && self.motion.is_empty()
            && self.collage.is_empty()
            && self.cover.is_empty()
            && self.crop.is_empty()
            && self.book.is_empty()
            && self.closing.is_none()
            && self.book_layout.is_empty()
    }

    /// Play a Live Photo's motion in the film, or put it back to the
    /// planner's choice.
    pub fn set_motion(&mut self, asset: String, on: bool) {
        self.motion.retain(|id| *id != asset);
        if on {
            self.motion.push(asset);
        }
    }

    /// Pin a photo into the book, or unpin it.
    pub fn set_book(&mut self, asset: String, on: bool) {
        self.book.retain(|id| *id != asset);
        if on {
            self.book.push(asset);
        }
    }

    /// Set (or clear, `None`) where a photo's crops centre, in percent.
    pub fn set_crop(&mut self, asset: String, focus: Option<(u8, u8)>) {
        set_pin(
            &mut self.crop,
            asset,
            focus.map(|(x, y)| (x.min(100), y.min(100))),
        );
    }

    /// Pin `asset` to the book cover with a horizontal focus percent, or
    /// unpin it (`None`).
    pub fn set_cover(&mut self, asset: String, focus: Option<u8>) {
        set_pin(&mut self.cover, asset, focus.map(|f| f.min(100)));
    }

    /// Play `asset` right after (or before) `anchor` in the film,
    /// replacing any earlier move of `asset`.
    pub fn set_move(&mut self, asset: String, anchor: String, after: bool) {
        self.unmove(&asset);
        self.moves.push(FilmMove {
            asset,
            anchor,
            after,
        });
    }

    /// Undo the move of `asset`; false when it had none.
    pub fn unmove(&mut self, asset: &str) -> bool {
        let before = self.moves.len();
        self.moves.retain(|m| m.asset != asset);
        self.moves.len() != before
    }

    /// Set or (when blank) clear the place label for a chapter.
    pub fn set_place(&mut self, chapter_key: String, place: Option<String>) {
        set_override(&mut self.place_overrides, chapter_key, place);
    }

    /// Set or (when blank) clear the caption for an asset.
    pub fn set_caption(&mut self, asset_id: String, caption: Option<String>) {
        set_override(&mut self.caption_overrides, asset_id, caption);
    }

    /// Hide an asset's caption, its own and the library's: a blank
    /// override counts as no caption (see [`has_caption`]), which
    /// [`Self::set_caption`] would clear instead.
    pub fn hide_caption(&mut self, asset_id: String) {
        self.caption_overrides.insert(asset_id, String::new());
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

/// "Play this one right before/after that one" in the film.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FilmMove {
    /// The film pick that moves.
    pub asset: String,
    /// The film pick it plays next to (same day).
    pub anchor: String,
    /// Right after the anchor (`true`) or right before it.
    pub after: bool,
}

/// Why a film move of `asset` next to `anchor` can't apply to `hero`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveRejection {
    /// The moving shot (`anchor: false`) or its anchor isn't a film pick.
    NotInFilm { anchor: bool },
    /// A shot can't move next to itself.
    SameShot,
    /// The two aren't on one capture-local day.
    OtherDay,
}

/// Check a film move against `hero` ([`FilmMove`]); `None` when it applies.
pub fn move_rejection(hero: &[PlanEntry], asset: &Asset, anchor: &Asset) -> Option<MoveRejection> {
    let in_film = |a: &Asset| hero.iter().any(|e| e.asset_id == a.id);
    if !in_film(asset) {
        Some(MoveRejection::NotInFilm { anchor: false })
    } else if !in_film(anchor) {
        Some(MoveRejection::NotInFilm { anchor: true })
    } else if asset.id == anchor.id {
        Some(MoveRejection::SameShot)
    } else if !same_day(asset, anchor) {
        Some(MoveRejection::OtherDay)
    } else {
        None
    }
}

/// Apply the user's film moves to `hero` ([`PlanEdits::moves`]).
///
/// A move applies only while both assets are film picks of one capture-
/// local day and neither is removed (`dead`); otherwise it is skipped and
/// kept for when it fits again. All of a clip's windows move together, in
/// their order, landing after the anchor's last window or before its first.
/// Deterministic when moves chain: a move whose anchor was moved itself is
/// applied after the anchor's own move (so "B after A, C after B" plays A,
/// B, C whatever order they were made in); in a loop, the oldest move is
/// applied last and wins.
fn apply_moves(
    hero: &mut Vec<PlanEntry>,
    by_id: &HashMap<&str, &Asset>,
    moves: &[FilmMove],
    dead: &HashSet<&str>,
) {
    let move_of: HashMap<&str, usize> = moves
        .iter()
        .enumerate()
        .map(|(i, m)| (m.asset.as_str(), i))
        .collect();
    // Dependency order: each move after the move of its anchor.
    let mut order: Vec<usize> = Vec::with_capacity(moves.len());
    let mut state = vec![0u8; moves.len()]; // 0 new, 1 visiting, 2 done
    for root in 0..moves.len() {
        let mut stack = vec![(root, false)];
        while let Some((i, expanded)) = stack.pop() {
            if expanded {
                state[i] = 2;
                order.push(i);
                continue;
            }
            if state[i] != 0 {
                continue;
            }
            state[i] = 1;
            stack.push((i, true));
            if let Some(&j) = move_of.get(moves[i].anchor.as_str()) {
                if state[j] == 0 {
                    stack.push((j, false));
                }
            }
        }
    }
    for i in order {
        let m = &moves[i];
        let (x, y) = (m.asset.as_str(), m.anchor.as_str());
        if dead.contains(x) || dead.contains(y) {
            continue;
        }
        let (Some(a), Some(b)) = (by_id.get(x), by_id.get(y)) else {
            continue;
        };
        if move_rejection(hero, a, b).is_some() {
            continue;
        }
        let moved: Vec<PlanEntry> = hero.iter().filter(|e| e.asset_id == x).cloned().collect();
        hero.retain(|e| e.asset_id != x);
        let at = if m.after {
            hero.iter()
                .rposition(|e| e.asset_id == y)
                .map_or(0, |p| p + 1)
        } else {
            hero.iter().position(|e| e.asset_id == y).unwrap_or(0)
        };
        hero.splice(at..at, moved);
    }
}

/// Insert `value` under `key`, or remove the key for `None`.
fn set_pin<V>(map: &mut BTreeMap<String, V>, key: String, value: Option<V>) {
    match value {
        Some(v) => map.insert(key, v),
        None => map.remove(&key),
    };
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
/// then pinned inclusions, then film moves ([`apply_moves`]), then
/// exclusions, then caption overrides on top.
///
/// Inclusions (hero only) put each pinned asset into `hero` at its
/// story-order position ([`crate::cluster::story_order`]) with the
/// same rendition rules as a swap-in (clips keep their window, Live
/// Photos are stills). Pins must belong to this plan's chapters and are
/// skipped when already picked. A pin adds to the film: nothing is taken
/// out to make room, so each pin lengthens the film by its own duration.
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

    include_pins(&mut out, assets, &by_id, edits, &dead);
    apply_moves(&mut out.hero, &by_id, &edits.moves, &dead);

    // Live Photos the user set to play their motion, pins and swap-ins
    // included (they arrive as stills).
    for e in &mut out.hero {
        if edits.motion.contains(&e.asset_id) {
            if let Some(motion) = by_id.get(e.asset_id.as_str()).and_then(|a| live_motion(a)) {
                e.rendition = motion;
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

    // Collage overrides ride along for the grouping (`collage_slots`),
    // which ignores any that no longer fit the film.
    out.collage = edits.collage.clone();
    out.cover = edits
        .cover
        .iter()
        .filter(|(id, _)| !dead.contains(id.as_str()))
        .map(|(id, f)| (id.clone(), *f))
        .collect();
    out.crop = edits.crop.clone();
    // A book pin may name any photo of the project, not only one the plan
    // holds: the book places a set-aside near-duplicate (the better take of
    // a burst) beside the shot nearest it in time.
    let known = |id: &str| by_id.contains_key(id) || in_plan.contains(id);
    out.book = edits
        .book
        .iter()
        .filter(|id| known(id.as_str()) && !dead.contains(id.as_str()))
        .cloned()
        .collect();
    out.closing = edits
        .closing
        .clone()
        .filter(|id| in_plan.contains(id.as_str()) && !dead.contains(id.as_str()));
    out.book_layout = edits.book_layout.clone();
    // Layout choices follow the photos the book can place (above).
    out.book_layout.retain_assets(|id| {
        (in_plan.contains(id) || out.book.iter().any(|b| b == id)) && !dead.contains(id)
    });
    out.collage
        .retain_assets(|id| in_plan.contains(id) && !dead.contains(id));
    out
}

/// Film entry for an asset the user put there (swap-in or pin). The film
/// shows Live Photos as stills unless the plan picked them for motion, so a
/// user pick is a still (unless it is in `PlanEdits::motion`, which
/// [`apply_edits`] applies afterwards); clips keep their window.
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
    assets: &[Asset],
    by_id: &HashMap<&str, &Asset>,
    edits: &PlanEdits,
    dead: &HashSet<&str>,
) {
    if edits.included.is_empty() {
        return;
    }
    // Story positions over the project's assets; unknown assets sort last.
    let pos = story_positions(assets);
    let key_of = |id: &str| pos.get(id).copied().unwrap_or(usize::MAX);
    for id in &edits.included {
        let Some(asset) = by_id.get(id.as_str()) else {
            continue;
        };
        if out.chapter_of(id).is_none()
            || dead.contains(id.as_str())
            || out.hero.iter().any(|e| e.asset_id == *id)
        {
            continue;
        }
        // Swaps keep their slot, so the hero may be out of story order:
        // land right after the last entry earlier in story order, so
        // nothing told before the pin plays after it.
        let key = key_of(&asset.id);
        let at = out
            .hero
            .iter()
            .rposition(|e| key_of(&e.asset_id) < key)
            .map_or(0, |i| i + 1);
        out.hero.insert(at, user_hero_entry(asset));
    }
}

/// The lowest-scored hero pick in `id`'s chapter that the user did not
/// choose (pins and swap replacements are theirs): the shot a pin of `id`
/// was preferred over, for learning ([`crate::learn`]). `None` when the
/// chapter has no such pick.
pub fn weakest_unchosen_pick<'p>(
    plan: &'p Plan,
    assets: &[Asset],
    edits: &PlanEdits,
    id: &str,
) -> Option<&'p str> {
    let chosen: HashSet<&str> = edits
        .included
        .iter()
        .chain(edits.swaps.iter().map(|(_, to)| to))
        .map(String::as_str)
        .collect();
    let chapter: HashSet<&str> = plan
        .chapter_of(id)?
        .asset_ids
        .iter()
        .map(String::as_str)
        .collect();
    // One pass over the assets, not one per comparison.
    let score: HashMap<&str, f32> = assets
        .iter()
        .filter(|a| chapter.contains(a.id.as_str()))
        .map(|a| {
            (
                a.id.as_str(),
                a.score.as_ref().map_or(DEFAULT_SCORE, |s| s.total),
            )
        })
        .collect();
    let score_of = |id: &str| score.get(id).copied().unwrap_or(DEFAULT_SCORE);
    plan.hero
        .iter()
        .map(|e| e.asset_id.as_str())
        .filter(|h| *h != id && !chosen.contains(h) && chapter.contains(h))
        .min_by(|a, b| score_of(a).total_cmp(&score_of(b)))
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
    // Story order: capture time, a split day told party by party.
    let tracked = story_tracks(assets);
    let chapter_label = |id: &str| cluster_of.get(id).map_or(UNGROUPED, String::as_str);
    let ordered = chapters_together(&tracked, chapter_label);
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
    let home = crate::places::home_bases(assets);
    let chapters: Vec<Chapter> = groups
        .iter()
        .map(|(label, members)| {
            let gps: Vec<(f64, f64)> = members.iter().filter_map(|a| a.gps).collect();
            let first_date = members.iter().filter_map(|a| local_date(a)).min();
            // GPS naming first; a user override (e.g. the ship's name for
            // days at sea) only fills what GPS honestly cannot name.
            let place = chapter_place(&gps, &home).or_else(|| {
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
                day_index: first_date.zip(trip_start).map(|(d, s)| trip_day(d, s)),
                date_label: first_date.map(|d| d.strftime(DAY_FORMAT).to_string()),
                film_place: None,
                film_place_changes: vec![],
            }
        })
        .collect();

    // Map polyline: GPS fixes in story order, minus fixes that imply
    // impossible speed. Capture times separate those from real flights.
    // On a split day the line follows one party's stretch, then the next
    // party's (one hop between them), as the film tells it, instead of
    // zig-zagging between parties in capture order.
    let outliers = gps_outliers_in(&tracked);
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
                (Some(first), Some(last)) => (last - first).get_days() as usize + 1,
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
            opening_map: false,
            route_stops: crate::route_stops::animated_stop_count(
                &crate::route_stops::chapter_route_stops(&chapters, assets, &map_polyline),
            ),
            route_secs: None,
            places: distinct_places(&chapters),
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
    // A split day's capture-order clusters, which keep its share of the
    // film (see `select_hero_by_budget`); every other day is one unit per
    // chapter, exactly as before.
    let split_days: HashSet<String> = tracked
        .iter()
        .filter(|(_, t)| *t != Track::Shared)
        .map(|(a, _)| day_of(a))
        .collect();
    let capture_unit: HashMap<String, usize> = crate::cluster::cluster_by_capture(
        ordered
            .iter()
            .copied()
            .filter(|a| split_days.contains(&day_of(a))),
    )
    .into_iter()
    .enumerate()
    .flat_map(|(k, c)| c.asset_ids.into_iter().map(move |id| (id, k)))
    .collect();
    let hero = select_hero_by_budget(
        hero,
        chapter_label,
        |id| capture_unit.get(id).copied(),
        entry_score,
        similar,
        pano_secs_of,
        |id| caption_read_secs(by_id.get(id).and_then(|a| a.user_caption.as_deref())),
        |id| by_id.get(id).and_then(|a| a.taken_at),
        budget,
    );

    // Reel selects: trip highlights. Rank every asset trip-wide by score
    // (motion-capable assets get a small ranking bonus — a clip or Live
    // Photo makes a stronger reel), then greedily take distinct day/place
    // moments so the pack spans the trip instead of stopping at the first
    // few days. Fill to the minimum from the remaining best when there
    // are not enough distinct moments. Presented in story order.
    let rank_of = |a: &Asset| {
        let motion = matches!(a.kind, AssetKind::Video) || a.has_live_motion();
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
        collage: Default::default(),
        map_polyline,
        music: MusicPick {
            track: "morning-light".to_string(),
            rationale: "Vibe fit: mid-tempo acoustic for mixed daylight outdoor moments"
                .to_string(),
        },
        cover: Default::default(),
        crop: Default::default(),
        book: Default::default(),
        closing: Default::default(),
        book_layout: Default::default(),
    }
}

/// Story order with each chapter of a split day kept in one piece.
///
/// Chapters come from clustering every asset, while the plan sees only the
/// kept ones (near-dupes suppressed), so a shot told with one party here
/// can belong to another party's chapter: a captioned copy of a photo
/// whose original was the only nearby shot of its phone. On a day with a
/// split stretch, the day's shots are regrouped by chapter, chapters in
/// the order of their median member, each chapter's shots in story order.
/// Every other day is untouched.
fn chapters_together<'a, 'c>(
    tracked: &[(&'a Asset, Track)],
    chapter_of: impl Fn(&str) -> &'c str,
) -> Vec<&'a Asset> {
    let mut out = Vec::with_capacity(tracked.len());
    for run in tracked.chunk_by(|(a, _), (b, _)| same_day(a, b)) {
        if run.iter().all(|(_, t)| *t == Track::Shared) {
            out.extend(run.iter().map(|(a, _)| *a));
            continue;
        }
        let mut members: HashMap<&str, Vec<usize>> = HashMap::new();
        for (p, (a, _)) in run.iter().enumerate() {
            members.entry(chapter_of(&a.id)).or_default().push(p);
        }
        let median: HashMap<&str, usize> = members
            .iter()
            .map(|(c, ps)| (*c, ps[(ps.len() - 1) / 2]))
            .collect();
        let mut idx: Vec<usize> = (0..run.len()).collect();
        idx.sort_by_key(|&p| (median[chapter_of(&run[p].0.id)], p));
        out.extend(idx.into_iter().map(|p| run[p].0));
    }
    out
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

/// What "same moment" needs to know about one shot, borrowed from an
/// [`Asset`] or a lighter caller-side record (the best-of pick).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Moment<'a> {
    /// Capture time (unix seconds); unknown never matches.
    pub taken_at: Option<i64>,
    pub phash: Option<u64>,
    /// Image embedding, when the local model produced one.
    pub embedding: Option<&'a Embedding>,
}

impl<'a> Moment<'a> {
    pub fn of(a: &'a Asset) -> Self {
        Self {
            taken_at: a.taken_at,
            phash: a.phash,
            embedding: a.embedding.as_ref(),
        }
    }
}

/// Whether `a` and `b` show the same moment; see [`same_moment_at`].
fn same_moment(a: &Asset, b: &Asset) -> bool {
    same_moment_at(Moment::of(a), Moment::of(b))
}

/// Whether two shots show the same moment: a burst (within
/// [`SAME_MOMENT_BURST_SECS`]), or close in time ([`SAME_MOMENT_SECS`])
/// and the same scene: similar pixels (pHash) or, when both carry an
/// embedding, the same scene in another framing
/// ([`crate::embedding::SAME_SCENE_COSINE`]). Without embeddings this is
/// the pHash-only rule.
pub fn same_moment_at(a: Moment, b: Moment) -> bool {
    let (Some(ta), Some(tb)) = (a.taken_at, b.taken_at) else {
        return false;
    };
    let dt = (ta - tb).abs();
    if dt <= SAME_MOMENT_BURST_SECS {
        return true;
    }
    if dt > SAME_MOMENT_SECS {
        return false;
    }
    let pixels_alike = matches!(
        (a.phash, b.phash),
        (Some(p), Some(q)) if crate::hash::phash_distance(p, q) <= SAME_MOMENT_PHASH_DISTANCE
    );
    pixels_alike || same_scene(a.embedding, b.embedding)
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
        if a.has_live_motion() {
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
fn chapter_place(gps: &[(f64, f64)], home: &HashSet<String>) -> Option<String> {
    // An attraction only when the chapter's photos visited it (see
    // `places`), else the city at the medoid: 2 km around the medoid named
    // whatever the day walked past.
    crate::places::chapter_attraction(gps, home).or_else(|| crate::geo::city_name(medoid(gps)?))
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

/// A trip's dates for the cover: `October 3–30, 2026`, `October 28 –
/// November 3, 2026`, or `December 28, 2026 – January 4, 2027`.
pub fn date_range_label(start: jiff::civil::Date, end: jiff::civil::Date) -> String {
    let (a, b) = (start.min(end), start.max(end));
    let md = |d: jiff::civil::Date| d.strftime("%B %-d").to_string();
    if a == b {
        format!("{}, {}", md(a), a.year())
    } else if a.year() != b.year() {
        format!("{}, {} \u{2013} {}, {}", md(a), a.year(), md(b), b.year())
    } else if a.month() != b.month() {
        format!("{} \u{2013} {}, {}", md(a), md(b), b.year())
    } else {
        format!("{}\u{2013}{}, {}", md(a), b.day(), b.year())
    }
}

/// 1-based trip day of local date `d` when the trip starts on `start`
/// (dates before the start count as day 1).
pub fn trip_day(d: jiff::civil::Date, start: jiff::civil::Date) -> u32 {
    ((d - start).get_days() + 1).max(1) as u32
}

/// Display title for a cluster label: `2026-10-12 · place 21` (or a bare
/// `2026-10-12`) becomes `Mon, Oct 12`; anything else passes through.
fn human_chapter_title(label: &str) -> String {
    let date_part = label.split_once(" · ").map_or(label, |(d, _)| d);
    jiff::civil::Date::strptime("%Y-%m-%d", date_part).map_or_else(
        |_| label.to_string(),
        |d| d.strftime(DAY_FORMAT).to_string(),
    )
}

/// An asset's score: the fresh one when scoring just ran, else the persisted
/// one (which still carries the clip windows).
fn score_for_asset<'s>(scores: &'s HashMap<String, Score>, a: &'s Asset) -> Option<&'s Score> {
    scores.get(&a.id).or(a.score.as_ref())
}

/// The motion rendition of a Live Photo with a companion video: its first
/// three seconds at most. `None` for any other asset.
fn live_motion(a: &Asset) -> Option<Rendition> {
    a.has_live_motion().then(|| Rendition::Motion {
        trim_start: 0.0,
        trim_end: a.duration_secs.unwrap_or(3.0).clamp(0.5, 3.0),
    })
}

/// Plan entry for one asset. Videos play the best window picked at ingest
/// (`score.clip_windows`) and fall back to the start of the clip. This is
/// the asset's single entry: reels, swap-ins and pins use it, so only the
/// hero film built from scratch shows several windows ([`plan_entries`]).
fn plan_entry(a: &Asset, score: Option<&Score>) -> PlanEntry {
    let mut window = 0;
    let rendition = match a.kind {
        AssetKind::LivePhoto => {
            // Motion-first for video; book renderers always use still.
            live_motion(a).unwrap_or(Rendition::Still)
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
        assert_eq!(
            dateline(1_714_557_600).as_deref(),
            Some("Wed, May 1 · 10:00 AM")
        );
        assert_eq!(dateline(i64::MAX), None);
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
        let none = HashSet::new();
        let chapter_place = |g: &[(f64, f64)]| super::chapter_place(g, &none);
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
        // A home base (the hotel's neighbourhood) never names a chapter.
        let home = HashSet::from(["Tokyo DisneySea".to_string()]);
        let near_hotel = super::chapter_place(&[(35.6267, 139.8881); 3], &home);
        assert!(near_hotel.is_some_and(|n| n != "Tokyo DisneySea"));
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
            ..Default::default()
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
    fn book_pin_setters_clamp_replace_and_clear() {
        let mut edits = PlanEdits::default();
        edits.set_cover("a".into(), Some(30));
        edits.set_cover("a".into(), Some(250));
        assert_eq!(edits.cover["a"], 100);
        edits.set_crop("a".into(), Some((120, 40)));
        assert_eq!(edits.crop["a"], (100, 40));
        edits.set_book("b".into(), true);
        edits.set_book("c".into(), true);
        edits.set_book("b".into(), true);
        assert_eq!(edits.book, ["c", "b"]);
        edits.set_cover("a".into(), None);
        edits.set_crop("a".into(), None);
        edits.set_book("b".into(), false);
        edits.set_book("c".into(), false);
        assert!(edits.is_empty());
    }

    #[test]
    fn a_hidden_caption_beats_the_librarys() {
        let assets = vec![asset("a", 1_714_557_600, AssetKind::Photo)];
        let mut base = build_plan(&assets, &HashMap::new(), &HashMap::new());
        base.captions.insert("a".into(), "From the library".into());
        let mut edits = PlanEdits::default();
        edits.hide_caption("a".into());
        assert!(!edits.is_empty());
        let out = apply_edits(&base, &assets, &edits);
        assert!(!has_caption(out.captions.get("a").map(String::as_str)));
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
    fn included_asset_lands_in_story_order_and_lengthens_the_film() {
        // Hero holds a, c, e (scores 0.2, 0.4, 0.6); pin d.
        let (assets, base) = pin_fixture(&[0, 2, 4]);
        let mut edits = PlanEdits::default();
        edits.set_included("d".into(), true);
        let out = apply_edits(&base, &assets, &edits);
        // Nothing makes room; d sits between c and e.
        assert_eq!(hero_ids(&out), ["a", "c", "d", "e"]);
        // Pinning an already-picked asset changes nothing.
        let mut edits = PlanEdits::default();
        edits.set_included("c".into(), true);
        assert_eq!(
            hero_ids(&apply_edits(&base, &assets, &edits)),
            ["a", "c", "e"]
        );
    }

    #[test]
    fn weakest_unchosen_pick_skips_the_users_picks() {
        // Hero a, c, e (scores 0.2, 0.4, 0.6).
        let (assets, base) = pin_fixture(&[0, 2, 4]);
        let mut edits = PlanEdits::default();
        assert_eq!(
            weakest_unchosen_pick(&base, &assets, &edits, "d"),
            Some("a")
        );
        // Pins and swap replacements are the user's, never the loser.
        edits.set_included("a".into(), true);
        assert_eq!(
            weakest_unchosen_pick(&base, &assets, &edits, "d"),
            Some("c")
        );
        edits.swap("x".into(), "c".into());
        assert_eq!(
            weakest_unchosen_pick(&base, &assets, &edits, "d"),
            Some("e")
        );
        // The pinned asset itself is never its own rival.
        assert_eq!(weakest_unchosen_pick(&base, &assets, &edits, "e"), None);
        // Outside the plan's chapters: nothing to compare with.
        assert_eq!(weakest_unchosen_pick(&base, &assets, &edits, "zz"), None);
    }

    #[test]
    fn pins_never_take_a_shot_out() {
        let (assets, base) = pin_fixture(&[0]);
        let mut edits = PlanEdits::default();
        edits.set_included("a".into(), true);
        edits.set_included("b".into(), true);
        edits.set_included("c".into(), true);
        let out = apply_edits(&base, &assets, &edits);
        assert_eq!(hero_ids(&out), ["a", "b", "c"]);
        // A swap replacement stays alongside the pin.
        let (assets, base) = pin_fixture(&[0, 1]);
        let mut edits = PlanEdits::default();
        edits.swap("a".into(), "e".into()); // hero: e, b
        edits.set_included("c".into(), true);
        let out = apply_edits(&base, &assets, &edits);
        assert_eq!(hero_ids(&out), ["e", "b", "c"]);
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
        assert_eq!((ids.as_str(), pins), ("a,c", vec!["c".to_string()]));
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
        assert_eq!(ids, "a,d");
        let (ids, pins) = hero(&[0], &|e| {
            e.set_included("b".into(), true);
            e.swap("b".into(), "c".into());
            e.swap("c".into(), "b".into());
        });
        assert_eq!((ids.as_str(), pins), ("a,b", vec!["b".to_string()]));
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
        // must land after c, not before e.
        let (assets, base) = pin_fixture(&[0, 1, 2]);
        let mut edits = PlanEdits::default();
        edits.swap("a".into(), "e".into());
        edits.set_included("d".into(), true);
        let out = apply_edits(&base, &assets, &edits);
        assert_eq!(hero_ids(&out), ["e", "b", "c", "d"]);
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
        // "e" drops out of every chapter, so it is not in this plan.
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
            let day = jiff::civil::Date::strptime("%Y-%m-%d", date).unwrap();
            crate::metadata::wall_to_unix(day.at(12, 0, 0, 0)).unwrap()
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
            |id| caption_read_secs(plan.captions.get(id).map(String::as_str)),
            |_| Some(0),
            HeroCards {
                route: !plan.map_polyline.is_empty(),
                opening_map: true,
                route_stops: 0,
                route_secs: None,
                places: distinct_places(&plan.chapters),
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
            opening_map: true,
            route_stops: 0,
            route_secs: None,
            places: 1,
        };
        assert!(
            (estimate_hero_secs(&entries, ch, |_| None, |_| 0.0, |_| Some(0), cards) - 33.18).abs()
                < 1e-9
        );
        // Without a route or places only the title card remains.
        let bare = HeroCards {
            route: false,
            opening_map: true,
            route_stops: 0,
            route_secs: None,
            places: 0,
        };
        // 2.5 + 7 + 4 + 6 + 2.5 + 4 = 26 s; cuts 0.7 + 0.54 + 0.8 + 0.54 +
        // 0.7 = 3.28 s.
        assert!(
            (estimate_hero_secs(&entries, ch, |_| None, |_| 0.0, |_| Some(0), bare) - 22.72).abs()
                < 1e-9
        );
    }

    #[test]
    fn captioned_stills_play_solo_in_the_estimate() {
        let bare = HeroCards {
            route: false,
            opening_map: true,
            route_stops: 0,
            route_secs: None,
            places: 0,
        };
        let est = |ids: &[&str], caps: &[(&str, &str)]| {
            let entries: Vec<PlanEntry> = ids.iter().map(|i| still_entry(i)).collect();
            let caps: HashMap<&str, &str> = caps.iter().copied().collect();
            estimate_hero_secs(
                &entries,
                |_| "X",
                |_| None,
                |id| caption_read_secs(caps.get(id).copied()),
                |_| Some(0),
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
        // The pano stays solo with its 8 s hold; x1 reaches across it to
        // collage with x2 and x3 (6 s) rather than play alone: title 2.5 +
        // collage 6 + pano 8 = 16.5 s, cuts 0.7 + 0.7 = 1.4 s.
        let bare = HeroCards {
            route: false,
            opening_map: true,
            route_stops: 0,
            route_secs: None,
            places: 0,
        };
        assert!(
            (estimate_hero_secs(&entries, |_| "X", pano, |_| 0.0, |_| Some(0), bare) - 15.1).abs()
                < 1e-9
        );
    }

    #[test]
    fn date_range_label_reads_naturally() {
        let d = jiff::civil::date;
        assert_eq!(
            date_range_label(d(2026, 10, 3), d(2026, 10, 30)),
            "October 3\u{2013}30, 2026"
        );
        assert_eq!(
            date_range_label(d(2026, 10, 28), d(2026, 11, 3)),
            "October 28 \u{2013} November 3, 2026"
        );
        assert_eq!(
            date_range_label(d(2026, 12, 28), d(2027, 1, 4)),
            "December 28, 2026 \u{2013} January 4, 2027"
        );
        assert_eq!(
            date_range_label(d(2026, 7, 4), d(2026, 7, 4)),
            "July 4, 2026"
        );
    }

    #[test]
    fn trip_day_counts_from_one() {
        let start = jiff::civil::date(2026, 10, 8);
        assert_eq!(trip_day(start, start), 1);
        assert_eq!(trip_day(jiff::civil::date(2026, 10, 11), start), 4);
        // Before the start (clock skew): still day 1.
        assert_eq!(trip_day(jiff::civil::date(2026, 10, 7), start), 1);
    }

    #[test]
    fn display_aspect_applies_rotation() {
        let mut a = asset("a", 0, AssetKind::Photo);
        assert_eq!(a.display_aspect(), None);
        a.width = Some(16350);
        a.height = Some(3686);
        assert!(is_panorama(a.display_aspect().unwrap()));
        assert!(!a.rotation_swaps_axes());
        a.rotation_deg = 90;
        assert!(a.rotation_swaps_axes());
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
        opening_map: true,
        route_stops: 0,
        route_secs: None,
        places: 0,
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
            |_| None,
            score_of,
            similar,
            |_| None,
            |_| 0.0,
            |_| Some(0),
            HeroBudget {
                ceiling_secs,
                cards: NO_CARDS,
            },
        )
        .into_iter()
        .map(|e| e.asset_id)
        .collect()
    }

    /// A split day's chapter keeps the share its capture-order clusters
    /// had: each unit gets a pick, and the chapter weighs as its units.
    #[test]
    fn a_chapter_split_into_units_keeps_their_share() {
        let ids: Vec<String> = ["a", "b"]
            .iter()
            .flat_map(|l| (1..=8).map(move |k| format!("{l}{k}")))
            .collect();
        let cands: Vec<PlanEntry> = ids.iter().map(|id| clip_entry(id, 5.0)).collect();
        // Within each chapter the lower numbers score higher.
        let score_of = |e: &PlanEntry| 1.0 - e.asset_id[1..].parse::<f32>().unwrap() / 10.0;
        let run = |unit_of: &dyn Fn(&str) -> Option<usize>| -> Vec<String> {
            select_hero_by_budget(
                cands.clone(),
                letter,
                unit_of,
                score_of,
                |_, _| false,
                |_| None,
                |_| 0.0,
                |_| Some(0),
                HeroBudget {
                    ceiling_secs: 60.0,
                    cards: NO_CARDS,
                },
            )
            .into_iter()
            .map(|e| e.asset_id)
            .collect()
        };
        let count = |v: &[String], l: &str| v.iter().filter(|id| id.starts_with(l)).count();
        let plain = run(&|_| None);
        assert!(!plain.contains(&"a7".to_string()), "{plain:?}");
        // Chapter a was four capture-order clusters of two.
        let split = run(&|id: &str| {
            id.starts_with('a')
                .then(|| (id[1..].parse::<usize>().unwrap() - 1) / 2)
        });
        assert!(
            count(&split, "a") > count(&plain, "a"),
            "{plain:?} {split:?}"
        );
        assert!(count(&split, "a") > count(&split, "b"), "{split:?}");
        for unit_rep in ["a1", "a3", "a5", "a7"] {
            assert!(split.contains(&unit_rep.to_string()), "{split:?}");
        }
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
            |_| 0.0,
            |_| Some(0),
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
        let reps = estimate_hero_secs(
            [&cands()[0]],
            letter,
            |_| None,
            |_| 0.0,
            |_| Some(0),
            NO_CARDS,
        );
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
        let ceiling = estimate_hero_secs(
            &cands[..0],
            letter,
            |_| None,
            |_| 0.0,
            |_| Some(0),
            NO_CARDS,
        ) + 20.0 * 2.5;
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
            |_| 0.0,
            |_| Some(0),
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
    fn same_moment_decision_table_with_embeddings() {
        let scene = Embedding::from_f32(&[1.0, 0.0, 0.0]).unwrap();
        let reframed = Embedding::from_f32(&[0.9, 0.3, 0.1]).unwrap(); // cos ~0.95
        let elsewhere = Embedding::from_f32(&[0.1, 0.2, 1.0]).unwrap(); // cos ~0.1
        fn m(t: i64, phash: u64, e: Option<&Embedding>) -> Moment<'_> {
            Moment {
                taken_at: Some(t),
                phash: Some(phash),
                embedding: e,
            }
        }
        let (scene, reframed, elsewhere) = (&scene, &reframed, &elsewhere);
        let near = SAME_MOMENT_SECS;
        let base = m(0, 0, Some(scene));
        // Near pHash: same moment with or without embeddings.
        assert!(same_moment_at(base, m(near, 0, Some(elsewhere))));
        assert!(same_moment_at(m(0, 0, None), m(near, 0, None)));
        // Far pHash but the same scene by embedding: caught.
        assert!(same_moment_at(base, m(near, u64::MAX, Some(reframed))));
        // Far pHash and another scene: distinct.
        assert!(!same_moment_at(base, m(near, u64::MAX, Some(elsewhere))));
        // Same scene but outside the time window: distinct (window unchanged).
        assert!(!same_moment_at(base, m(near + 1, u64::MAX, Some(reframed))));
        assert!(!same_moment_at(base, m(near + 1, 0, Some(scene))));
        // A missing embedding on either side falls back to pHash only.
        assert!(!same_moment_at(
            m(0, 0, None),
            m(near, u64::MAX, Some(reframed))
        ));
        assert!(!same_moment_at(base, m(near, u64::MAX, None)));
        // Unknown time never matches, whatever the pixels.
        let undated = Moment {
            taken_at: None,
            ..m(0, 0, Some(scene))
        };
        assert!(!same_moment_at(base, undated));
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
        let bare = estimate_hero_secs(&e, letter, |_| None, |_| 0.0, |_| Some(0), NO_CARDS);
        let full = estimate_hero_secs(
            &e,
            letter,
            |_| None,
            |_| 0.0,
            |_| Some(0),
            HeroCards {
                route: true,
                opening_map: true,
                route_stops: 0,
                route_secs: None,
                places: 1,
            },
        );
        // title 2.5 + still 4 - 0.7 vs + map 3 + closing map 4.5 + places 4.5.
        assert!((bare - 5.8).abs() < 1e-9, "{bare}");
        assert!((full - bare - 10.0).abs() < 1e-9, "{full}");
        // No entries: nothing but the title, and no overlap to subtract.
        assert_eq!(
            estimate_hero_secs([], letter, |_| None, |_| 0.0, |_| Some(0), NO_CARDS),
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
                |_| None,
                score_of,
                |_, _| false,
                |_| None,
                |_| 0.0,
                |_| Some(0),
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

    /// Slots from a compact spec: one char per entry, `s` an eligible
    /// still, `j` one the user joined to the previous collage photo,
    /// anything else (`o`: one the user set solo) an entry that plays solo;
    /// `|` starts the next chapter.
    fn slots(spec: &str) -> Vec<GroupSlot<usize>> {
        let mut out = vec![];
        for (chapter, part) in spec.split('|').enumerate() {
            out.extend(part.chars().map(|c| GroupSlot {
                chapter,
                eligible: matches!(c, 's' | 'j'),
                join_prev: c == 'j',
                taken_at: Some(0),
            }));
        }
        out
    }

    #[test]
    fn a_solo_photo_plays_alone_and_its_neighbours_regroup() {
        assert_eq!(groups_of("sss")[0], vec![0, 1, 2]);
        assert_eq!(groups_of("sos"), vec![vec![0, 2], vec![1]]);
        assert_eq!(groups_of("soss"), vec![vec![0, 2, 3], vec![1]]);
        assert_eq!(groups_of("os"), vec![vec![0], vec![1]]);
    }

    #[test]
    fn a_join_reaches_past_the_reach_but_not_past_a_chapter() {
        let far = format!("s{}j", "c".repeat(COLLAGE_REACH + 3));
        assert_eq!(groups_of(&far)[0], vec![0, COLLAGE_REACH + 4]);
        assert!(groups_of("s|j").iter().all(|g| g.len() == 1));
        // The link pins the split: five stills play 3 + 2, but 2 and 3
        // are joined, so they play 2 + 3.
        assert_eq!(groups_of("sssss"), vec![vec![0, 1, 2], vec![3, 4]]);
        assert_eq!(groups_of("sssjs"), vec![vec![0, 1], vec![2, 3, 4]]);
        // Links past a full collage do nothing.
        assert_eq!(
            groups_of("sjjjj"),
            vec![vec![0, 1, 2, 3], vec![4]],
            "{MAX_COLLAGE} at most"
        );
    }

    #[test]
    fn collage_slots_apply_edits_by_asset_id() {
        let edits = CollageEdits {
            solo: vec!["b".into(), "clip".into()],
            joins: vec![("a".into(), "c".into()), ("c".into(), "e".into())],
        };
        let (s, never) = (Candidacy::Plain, Candidacy::Never);
        let entries = [
            ("a", 0, s, Some(0)),
            ("b", 0, s, Some(0)),
            ("c", 0, s, Some(0)),
            ("clip", 0, never, Some(0)),
            ("d", 0, s, Some(0)),
            ("e", 0, s, Some(0)),
        ];
        let s = collage_slots(entries, &edits);
        let flags: Vec<(bool, bool)> = s.iter().map(|g| (g.eligible, g.join_prev)).collect();
        // b is solo, so a and c are neighbours and their link holds; a solo
        // clip means nothing; d now sits between c and e, so that link
        // waits until d is gone.
        assert_eq!(
            flags,
            [
                (true, false),
                (false, false),
                (true, true),
                (false, false),
                (true, false),
                (true, false)
            ]
        );
        let s = collage_slots(entries.into_iter().filter(|e| e.0 != "d"), &edits);
        assert!(s[4].join_prev);
    }

    #[test]
    fn collage_edits_load_from_older_edits_and_reach_the_edited_plan() {
        // Edits saved before collage edits existed still load.
        let old: PlanEdits = serde_json::from_str(r#"{"excluded":["x"]}"#).unwrap();
        assert!(old.collage.is_empty());
        // ...and collage edits store flat beside the others, as they always have.
        let flat: PlanEdits =
            serde_json::from_str(r#"{"solo":["s"],"joins":[["a","b"]]}"#).unwrap();
        assert_eq!(flat.collage.solo, ["s"]);
        let json = serde_json::to_value(&flat).unwrap();
        assert_eq!(json["joins"], serde_json::json!([["a", "b"]]));
        assert!(json.get("collage").is_none());

        let mut e = CollageEdits::default();
        e.join("a".into(), "b".into());
        e.set_solo("c".into(), true);
        e.join("c".into(), "d".into());
        assert_eq!(e.joins.len(), 2);
        assert!(!e.is_solo("c"), "joining puts c back in the collages");
        e.set_solo("a".into(), true);
        assert_eq!(
            e.joins,
            vec![("c".into(), "d".into())],
            "solo drops a's join"
        );
        e.unjoin("d");
        e.set_solo("a".into(), false);
        assert!(e.is_empty());

        // apply_edits hands live overrides to the plan; removed assets' go.
        let assets: Vec<Asset> = ["a", "b", "c"]
            .iter()
            .enumerate()
            .map(|(i, id)| asset(id, 1_714_557_600 + i as i64 * 60, AssetKind::Photo))
            .collect();
        let base = build_plan(&assets, &HashMap::new(), &HashMap::new());
        assert!(base.collage.is_empty());
        let edits = PlanEdits {
            excluded: vec!["c".into()],
            collage: CollageEdits {
                solo: vec!["a".into(), "c".into(), "gone".into()],
                joins: vec![("a".into(), "b".into()), ("b".into(), "c".into())],
            },
            ..Default::default()
        };
        let out = apply_edits(&base, &assets, &edits);
        assert_eq!(out.collage.solo, vec!["a".to_string()]);
        assert_eq!(out.collage.joins, vec![("a".into(), "b".into())]);
        // A stored plan never carries overrides in its JSON.
        assert!(!base.to_json().unwrap().contains("collage"));
    }

    #[test]
    fn estimate_follows_collage_overrides() {
        let entries: Vec<PlanEntry> = ["a1", "a2", "a3"].map(still_entry).to_vec();
        let est = |c: &CollageEdits| {
            estimate_hero_secs_with(
                &entries,
                letter,
                |_| None,
                |_| 0.0,
                |_| Some(0),
                NO_CARDS,
                c,
            )
        };
        let auto = est(&CollageEdits::default());
        let solo = est(&CollageEdits {
            solo: vec!["a2".into()],
            ..Default::default()
        });
        // One collage of 3 (6 s) becomes a collage of 2 (5 s) plus a solo
        // still (4 s) and one more cut.
        assert!(solo - auto > 2.0, "{auto} -> {solo}");
        assert_eq!(
            auto,
            estimate_hero_secs(&entries, letter, |_| None, |_| 0.0, |_| Some(0), NO_CARDS)
        );
    }

    #[test]
    fn collage_overrides_keep_every_entry_once() {
        let mut seed = 7u64;
        let mut next = move |n: u64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 33) % n
        };
        for _ in 0..2000 {
            let len = 1 + next(40) as usize;
            let mut chapter = 0;
            let mut t = 0i64;
            // Links land anywhere, even on entries collage_slots would
            // never mark: grouping must still be sound. Shots come 0-19
            // minutes apart, a few undated.
            let v: Vec<GroupSlot<usize>> = (0..len)
                .map(|_| {
                    chapter += usize::from(next(8) == 0);
                    t += next(20) as i64 * 60;
                    GroupSlot {
                        chapter,
                        eligible: next(3) != 0,
                        join_prev: next(3) == 0,
                        taken_at: (next(12) != 0).then_some(t),
                    }
                })
                .collect();
            let groups = collage_groups(&v);
            let mut seen: Vec<usize> = groups.iter().flatten().copied().collect();
            seen.sort_unstable();
            assert_eq!(seen, (0..len).collect::<Vec<_>>(), "{groups:?}");
            assert!(groups.windows(2).all(|w| w[0][0] < w[1][0]));
            let group_of = |i: usize| groups.iter().position(|g| g.contains(&i)).unwrap();
            let photo = |i: usize| v[i].eligible;
            for g in &groups {
                assert!(g.windows(2).all(|w| w[0] < w[1]), "chronological {g:?}");
                if g.len() > 1 {
                    let (a, b) = (g[0], g[g.len() - 1]);
                    assert!(g.len() <= MAX_COLLAGE, "{g:?}");
                    assert!(g.iter().all(|&i| photo(i)), "solo or ineligible in {g:?}");
                    assert!(v[a..=b].iter().all(|s| s.chapter == v[a].chapter));
                    // Without links among them, the reach and the time
                    // window still hold.
                    if g[1..].iter().all(|&i| !v[i].join_prev) {
                        assert!(b - a + 1 - g.len() <= COLLAGE_REACH, "{g:?}");
                        assert!(
                            g.iter()
                                .all(|&i| within_window(v[a].taken_at, v[i].taken_at)),
                            "{g:?}"
                        );
                    }
                }
            }
            // A lone link (neither neighbour linked too) in one chapter
            // always holds.
            let photos: Vec<usize> = (0..len).filter(|&i| photo(i)).collect();
            for w in photos.windows(2) {
                let (p, i) = (w[0], w[1]);
                let after = photos.iter().find(|&&k| k > i);
                if v[i].join_prev
                    && !v[p].join_prev
                    && after.is_none_or(|&k| !v[k].join_prev)
                    && v[p..=i].iter().all(|s| s.chapter == v[i].chapter)
                {
                    assert_eq!(group_of(p), group_of(i), "{p}-{i}: {groups:?}");
                }
            }
        }
    }

    fn groups_of(spec: &str) -> Vec<Vec<usize>> {
        collage_groups(&slots(spec))
    }

    #[test]
    fn collages_stay_within_the_time_window_unless_the_user_joins() {
        let at = |spec: &str, mins: &[Option<i64>]| {
            let mut v = slots(spec);
            for (s, m) in v.iter_mut().zip(mins) {
                s.taken_at = m.map(|m| m * 60);
            }
            collage_groups(&v)
        };
        // Midday stills and one from that evening, one entry apart: the
        // evening shot plays alone.
        assert_eq!(
            at("sscs", &[Some(0), Some(5), Some(6), Some(8 * 60)]),
            vec![vec![0, 1], vec![2], vec![3]]
        );
        // Within 30 minutes of the first photo: one collage.
        assert_eq!(
            at("sss", &[Some(0), Some(20), Some(30)]),
            vec![vec![0, 1, 2]]
        );
        // 31 minutes from the first, though 11 from the second: split.
        assert_eq!(
            at("ssss", &[Some(0), Some(20), Some(31), Some(40)]),
            vec![vec![0, 1], vec![2, 3]]
        );
        // An undated still never joins on its own (and, as a photo in
        // between, ends the run like a change of chapter would).
        assert_eq!(
            at("sss", &[Some(0), None, Some(1)]),
            vec![vec![0], vec![1], vec![2]]
        );
        assert_eq!(
            at("ssss", &[Some(0), Some(1), None, Some(2)]),
            vec![vec![0, 1], vec![2], vec![3]]
        );
        // The user's join overrides the window.
        assert_eq!(at("sj", &[Some(0), Some(8 * 60)]), vec![vec![0, 1]]);
        assert_eq!(at("sj", &[Some(0), None]), vec![vec![0, 1]]);
    }

    #[test]
    fn lone_still_joins_its_chapter_mate_across_a_clip() {
        // s c s: the stills share one collage at the first one's place,
        // the clip keeps its own position after it.
        assert_eq!(groups_of("scs"), vec![vec![0, 2], vec![1]]);
        // Three stills with a clip among them make one collage rather
        // than a pair and a lone still.
        assert_eq!(groups_of("sscs"), vec![vec![0, 1, 3], vec![2]]);
        // Two pairs either side of a clip stay two collages: nothing needs
        // to move.
        assert_eq!(groups_of("sscss"), vec![vec![0, 1], vec![2], vec![3, 4]]);
    }

    #[test]
    fn collage_reach_is_bounded() {
        let within = format!("s{}s", "c".repeat(COLLAGE_REACH));
        let beyond = format!("s{}s", "c".repeat(COLLAGE_REACH + 1));
        assert_eq!(groups_of(&within)[0], vec![0, COLLAGE_REACH + 1]);
        assert!(groups_of(&beyond).iter().all(|g| g.len() == 1));
        // The bound covers everything a group spans, not each hop: in
        // s c c s c c s any two neighbours fit, all three do not.
        let groups = groups_of("sccsccs");
        assert_eq!(groups.iter().filter(|g| g.len() == 2).count(), 1);
        assert!(groups.iter().all(|g| g.len() <= 2), "{groups:?}");
    }

    #[test]
    fn collages_never_cross_a_chapter() {
        assert!(groups_of("s|s").iter().all(|g| g.len() == 1));
        assert_eq!(groups_of("ss|ss"), vec![vec![0, 1], vec![2, 3]]);
        // An entry of another chapter between two stills also keeps them
        // apart.
        let mut v = slots("scs");
        v[1].chapter = 7;
        assert!(collage_groups(&v).iter().all(|g| g.len() == 1));
    }

    #[test]
    fn ineligible_entries_play_solo_in_place() {
        // Captioned stills, panoramas, clips and motion picks are
        // ineligible: never grouped, never moved.
        assert_eq!(groups_of("pcms"), vec![vec![0], vec![1], vec![2], vec![3]]);
    }

    #[test]
    fn candidacy_keeps_clips_and_panoramas_out_and_lets_the_user_join_the_rest() {
        let still = Rendition::Still;
        let clip = Rendition::Clip {
            trim_start: 0.0,
            trim_end: 3.0,
        };
        let motion = Rendition::Motion {
            trim_start: 0.0,
            trim_end: 3.0,
        };
        assert_eq!(collage_candidacy(&still, false, false), Candidacy::Plain);
        assert_eq!(collage_candidacy(&still, false, true), Candidacy::IfJoined);
        assert_eq!(
            collage_candidacy(&motion, false, false),
            Candidacy::IfJoined
        );
        assert_eq!(collage_candidacy(&motion, false, true), Candidacy::IfJoined);
        assert_eq!(collage_candidacy(&clip, false, false), Candidacy::Never);
        assert_eq!(collage_candidacy(&still, true, false), Candidacy::Never);
        // A caption does not make a panorama joinable.
        assert_eq!(collage_candidacy(&still, true, true), Candidacy::Never);
        assert_eq!(
            entry_candidacy(&still, Some(4.4), Some("Pano")),
            Candidacy::Never
        );
    }

    /// Groups of entries given as `(id, candidacy)`, one chapter, all shot
    /// at once, under `edits`.
    fn joined_groups(entries: &[(&str, Candidacy)], edits: &CollageEdits) -> Vec<Vec<usize>> {
        let slots = collage_slots(entries.iter().map(|&(id, c)| (id, 0, c, Some(0))), edits);
        collage_groups(&slots)
    }

    #[test]
    fn a_captioned_or_motion_entry_joins_a_collage_only_when_the_user_joins_it() {
        use Candidacy::{IfJoined, Never, Plain};
        // The Japan case: a captioned Live Photo, then two joined stills.
        let film = [("live", IfJoined), ("b", Plain), ("c", Plain)];
        let mut edits = CollageEdits::default();
        edits.join("b".into(), "c".into());
        assert_eq!(joined_groups(&film, &edits), [vec![0], vec![1, 2]]);
        edits.join("live".into(), "b".into());
        assert_eq!(joined_groups(&film, &edits), [vec![0, 1, 2]]);
        // From the other side, and across a clip that plays on its own.
        let film = [("a", Plain), ("clip", Never), ("cap", IfJoined)];
        let mut edits = CollageEdits::default();
        edits.join("a".into(), "cap".into());
        assert_eq!(joined_groups(&film, &edits), [vec![0, 2], vec![1]]);
        // Two joined captioned entries make a collage by themselves.
        let film = [("x", IfJoined), ("y", IfJoined), ("z", IfJoined)];
        let mut edits = CollageEdits::default();
        edits.join("x".into(), "y".into());
        assert_eq!(joined_groups(&film, &edits), [vec![0, 1], vec![2]]);
        // An unjoined captioned entry between does not block the join.
        let film = [("a", Plain), ("x", IfJoined), ("b", IfJoined)];
        let mut edits = CollageEdits::default();
        edits.join("a".into(), "b".into());
        assert_eq!(joined_groups(&film, &edits), [vec![0, 2], vec![1]]);
        // A move that breaks a saved join leaves the captioned photo on
        // its own, never beside photos it wasn't joined to (`x` was joined
        // to `c`, then moved after `a`).
        let film = [
            ("a", Plain),
            ("x", IfJoined),
            ("b", IfJoined),
            ("c", Plain),
            ("d", Plain),
        ];
        let mut edits = CollageEdits::default();
        edits.join("a".into(), "b".into());
        edits.join("x".into(), "c".into());
        let groups = joined_groups(&film, &edits);
        assert!(groups.contains(&vec![1]), "{groups:?}");
        assert!(groups.iter().any(|g| g.starts_with(&[0, 2])), "{groups:?}");
        // A stale link into another chapter does not make it eligible, so
        // it is not grouped with its own chapter's stills either.
        let mut edits = CollageEdits::default();
        edits.join("a".into(), "cap".into());
        let film = [("a", 0, Plain), ("cap", 1, IfJoined), ("b", 1, Plain)];
        let slots = collage_slots(film.iter().map(|&(id, ch, c)| (id, ch, c, Some(0))), &edits);
        assert!(!slots[1].eligible);
        assert_eq!(collage_groups(&slots), [vec![0], vec![1], vec![2]]);
        // Clips and panoramas never join, whatever the edits say; a solo
        // captioned entry stays out too.
        let film = [("a", Plain), ("clip", Never)];
        let mut edits = CollageEdits::default();
        edits.join("a".into(), "clip".into());
        assert!(joined_groups(&film, &edits).iter().all(|g| g.len() == 1));
        let film = [("a", Plain), ("cap", IfJoined)];
        let mut edits = CollageEdits::default();
        edits.join("a".into(), "cap".into());
        edits.solo.push("cap".into());
        assert!(joined_groups(&film, &edits).iter().all(|g| g.len() == 1));
        // With no edits nothing changes: they play on their own.
        assert!(joined_groups(&film, &CollageEdits::default())
            .iter()
            .all(|g| g.len() == 1));
    }

    #[test]
    fn the_estimate_counts_a_joined_captioned_entry_in_its_collage() {
        let mut entries: Vec<PlanEntry> = ["a1", "a2", "a3"].map(still_entry).to_vec();
        entries[0].rendition = Rendition::Motion {
            trim_start: 0.0,
            trim_end: 3.0,
        };
        let est = |c: &CollageEdits| {
            estimate_hero_secs_with(
                &entries,
                letter,
                |_| None,
                |id| if id == "a1" { 2.0 } else { 0.0 },
                |_| Some(0),
                NO_CARDS,
                c,
            )
        };
        let mut edits = CollageEdits::default();
        edits.join("a2".into(), "a3".into());
        let apart = est(&edits);
        edits.join("a1".into(), "a2".into());
        let together = est(&edits);
        // One collage of three (6 s) beats a motion solo plus a pair.
        assert!(together < apart, "{apart} -> {together}");
        assert!((together - est_one_collage_of_three()).abs() < 1e-9);
    }

    /// The estimate of three plain stills in one collage, the yardstick for
    /// a joined entry that is not plain.
    fn est_one_collage_of_three() -> f64 {
        let entries: Vec<PlanEntry> = ["a1", "a2", "a3"].map(still_entry).to_vec();
        estimate_hero_secs_with(
            &entries,
            letter,
            |_| None,
            |_| 0.0,
            |_| Some(0),
            NO_CARDS,
            &CollageEdits::default(),
        )
    }

    #[test]
    fn hero_grouping_groups_a_joined_captioned_motion_entry() {
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut live = still_entry("a1");
        live.rendition = Rendition::Motion {
            trim_start: 0.0,
            trim_end: 3.0,
        };
        let plan = Plan {
            plan_version: PLAN_VERSION,
            schema_version: Plan::SCHEMA_VERSION,
            hero: vec![
                live,
                still_entry("a2"),
                still_entry("a3"),
                clip_entry("a4", 3.0),
            ],
            reel_selects: vec![],
            chapters: vec![Chapter {
                asset_ids: ids(&["a1", "a2", "a3", "a4"]),
                ..Default::default()
            }],
            captions: [("a1".to_string(), "Egg tarts".to_string())].into(),
            collage: Default::default(),
            map_polyline: vec![],
            music: MusicPick {
                track: "t".into(),
                rationale: "r".into(),
            },
            cover: Default::default(),
            crop: Default::default(),
            book: Default::default(),
            closing: Default::default(),
            book_layout: Default::default(),
        };
        let g = HeroGrouping::new(&plan, |_| None, |_| Some(0), |_| true);
        assert_eq!(g.plays_alone(0), Some(PlaysAlone::Motion));
        let none = CollageEdits::default();
        // The captioned Live Photo can be joined to the next plain still;
        // a plain still does not offer it as its neighbour.
        assert_eq!(g.join_partner(&none, 0, false), Ok(1));
        assert_eq!(g.join_partner(&none, 1, true), Err(JoinBlock::NoNeighbour));
        // A clip never joins.
        assert_eq!(g.join_partner(&none, 3, true), Err(JoinBlock::Full));
        let mut joined = CollageEdits::default();
        joined.join("a2".into(), "a3".into());
        joined.join("a1".into(), "a2".into());
        assert_eq!(g.groups(&joined), [vec![0, 1, 2], vec![3]]);
        assert_eq!(g.groups(&none), [vec![0], vec![1, 2], vec![3]]);
    }

    fn live_asset(id: &str, secs: Option<f64>, companion: bool) -> Asset {
        let mut a = asset(id, T0, AssetKind::LivePhoto);
        a.companion_path = companion.then(|| PathBuf::from(format!("/t/{id}.mov")));
        a.duration_secs = secs;
        a
    }

    #[test]
    fn play_as_live_photo_switches_a_still_to_its_motion() {
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let assets = vec![
            live_asset("lp", Some(2.0), true),
            live_asset("bare", Some(2.0), false),
            asset("ph", T0, AssetKind::Photo),
            live_asset("pin", None, true),
        ];
        let plan = Plan {
            plan_version: PLAN_VERSION,
            schema_version: Plan::SCHEMA_VERSION,
            hero: ["lp", "bare", "ph"].map(still_entry).to_vec(),
            reel_selects: vec![],
            chapters: vec![Chapter {
                asset_ids: ids(&["lp", "bare", "ph", "pin"]),
                ..Default::default()
            }],
            captions: Default::default(),
            collage: Default::default(),
            map_polyline: vec![],
            music: MusicPick {
                track: "t".into(),
                rationale: "r".into(),
            },
            cover: Default::default(),
            crop: Default::default(),
            book: Default::default(),
            closing: Default::default(),
            book_layout: Default::default(),
        };
        let rendition = |p: &Plan, id: &str| {
            p.hero
                .iter()
                .find(|e| e.asset_id == id)
                .map(|e| e.rendition.clone())
        };
        let mut edits = PlanEdits::default();
        for id in ["lp", "bare", "ph", "pin", "gone"] {
            edits.set_motion(id.into(), true);
        }
        edits.set_included("pin".into(), true);
        let out = apply_edits(&plan, &assets, &edits);
        let motion = |end: f64| Rendition::Motion {
            trim_start: 0.0,
            trim_end: end,
        };
        // Same trim as the planner's own Motion pick.
        assert_eq!(rendition(&out, "lp"), Some(motion(2.0)));
        // A pin plays motion too (3 s without a known duration).
        assert_eq!(rendition(&out, "pin"), Some(motion(3.0)));
        // No companion, or no Live Photo: still a still.
        assert_eq!(rendition(&out, "bare"), Some(Rendition::Still));
        assert_eq!(rendition(&out, "ph"), Some(Rendition::Still));
        // Excluding it drops it as usual; unsetting restores the still.
        edits.set_excluded("lp".into(), true);
        assert_eq!(rendition(&apply_edits(&plan, &assets, &edits), "lp"), None);
        edits.set_excluded("lp".into(), false);
        edits.set_motion("lp".into(), false);
        assert_eq!(
            rendition(&apply_edits(&plan, &assets, &edits), "lp"),
            Some(Rendition::Still)
        );
        // Old edits load, and an empty list is not stored.
        let old: PlanEdits = serde_json::from_str(r#"{"excluded":["x"]}"#).unwrap();
        assert!(old.motion.is_empty());
        let json = serde_json::to_value(PlanEdits::default()).unwrap();
        assert!(json.get("motion").is_none());
        assert!(!PlanEdits {
            motion: vec!["lp".into()],
            ..Default::default()
        }
        .is_empty());
    }

    #[test]
    fn hero_grouping_reads_chapters_captions_aspect_and_presence() {
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let plan = Plan {
            plan_version: PLAN_VERSION,
            schema_version: Plan::SCHEMA_VERSION,
            hero: vec![
                still_entry("a1"),
                clip_entry("a2", 3.0),
                still_entry("a3"),
                still_entry("a4"),
                still_entry("b1"),
                still_entry("b2"),
                still_entry("b3"),
            ],
            reel_selects: vec![],
            chapters: vec![
                Chapter {
                    asset_ids: ids(&["a1", "a2", "a3", "a4"]),
                    ..Default::default()
                },
                Chapter {
                    asset_ids: ids(&["b1", "b2", "b3"]),
                    ..Default::default()
                },
            ],
            // A blank caption is no caption; a real one keeps b2 solo.
            captions: [("a3", "  "), ("b2", "Mia on the ferry")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            collage: Default::default(),
            map_polyline: vec![],
            music: MusicPick {
                track: "t".into(),
                rationale: "r".into(),
            },
            cover: Default::default(),
            crop: Default::default(),
            book: Default::default(),
            closing: Default::default(),
            book_layout: Default::default(),
        };
        // a4 is a panorama: solo. a1 and a3 join across the clip; b1 and
        // b3 across the captioned b2; nothing joins across the chapters.
        let pano = |id: &str| (id == "a4").then_some(4.0);
        let all = |_: &str| true;
        let none = CollageEdits::default();
        assert_eq!(
            HeroGrouping::new(&plan, pano, |_| Some(0), all).groups(&none),
            vec![vec![0, 2], vec![1], vec![3], vec![4, 6], vec![5]]
        );
        // Without the panorama, a4 joins a1 and a3.
        let plain = HeroGrouping::new(&plan, |_| None, |_| Some(0), all);
        assert_eq!(plain.groups(&none)[0], vec![0, 2, 3]);
        assert_eq!(
            (plain.chapter(4), plain.plays_alone(5)),
            (1, Some(PlaysAlone::Caption))
        );
        // A missing a3 does not play: a1 and a4 group without it.
        let gone = HeroGrouping::new(&plan, |_| None, |_| Some(0), |id| id != "a3");
        assert_eq!(gone.groups(&none)[0], vec![0, 3]);
        assert!(gone.groups(&none).iter().all(|g| !g.contains(&2)));
        // The user's edits regroup it.
        let solo = CollageEdits {
            solo: vec!["a3".into()],
            ..Default::default()
        };
        assert_eq!(plain.groups(&solo)[0], vec![0, 3]);
        assert_eq!(entry_plays_alone(&Rendition::Still, Some(1.5), None), None);
        assert_eq!(
            entry_plays_alone(&Rendition::Still, Some(4.0), None),
            Some(PlaysAlone::Panorama)
        );
        assert_eq!(
            entry_plays_alone(&Rendition::Still, Some(4.0), Some("x")),
            Some(PlaysAlone::Panorama)
        );
        assert_eq!(plain.plays_alone(1), Some(PlaysAlone::Clip));
        // Join partners: the nearest collage photo of the chapter, never
        // across a chapter, and only for a playing entry.
        assert_eq!(
            plain.join_partner(&none, 4, true),
            Err(JoinBlock::NoNeighbour)
        );
        assert_eq!(plain.join_partner(&none, 4, false), Ok(6));
        assert_eq!(
            gone.join_partner(&none, 2, true),
            Err(JoinBlock::NoNeighbour)
        );
        let a1_solo = CollageEdits {
            solo: vec!["a1".into()],
            ..Default::default()
        };
        assert_eq!(plain.join_partner(&a1_solo, 0, false), Ok(2));
        assert_eq!(
            plain.join_partner(&a1_solo, 2, true),
            Err(JoinBlock::NoNeighbour)
        );
    }

    #[test]
    fn collage_sizes_are_balanced() {
        let lens =
            |n: usize| -> Vec<usize> { groups_of(&"s".repeat(n)).iter().map(Vec::len).collect() };
        assert_eq!(lens(1), [1]);
        assert_eq!(lens(4), [4]);
        assert_eq!(lens(5), [3, 2]);
        assert_eq!(lens(6), [3, 3]);
        assert_eq!(lens(7), [4, 3]);
        assert_eq!(lens(8), [4, 4]);
        assert_eq!(lens(9), [3, 3, 3]);
        assert!(lens(13).iter().all(|&n| (2..=MAX_COLLAGE).contains(&n)));
    }

    #[test]
    fn collage_groups_show_every_entry_once_and_avoid_lone_stills() {
        let mut seed = 99u64;
        let mut next = move |n: u64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 33) % n
        };
        for _ in 0..500 {
            let len = 1 + next(40) as usize;
            let mut chapter = 0;
            let mut t = 0i64;
            // Shots 0-19 minutes apart, a few undated.
            let v: Vec<GroupSlot<usize>> = (0..len)
                .map(|_| {
                    chapter += usize::from(next(8) == 0);
                    t += next(20) as i64 * 60;
                    GroupSlot {
                        chapter,
                        eligible: next(3) != 0,
                        join_prev: false,
                        taken_at: (next(12) != 0).then_some(t),
                    }
                })
                .collect();
            let groups = collage_groups(&v);
            let mut seen: Vec<usize> = groups.iter().flatten().copied().collect();
            seen.sort_unstable();
            assert_eq!(seen, (0..len).collect::<Vec<_>>(), "{groups:?}");
            assert!(groups.windows(2).all(|w| w[0][0] < w[1][0]));
            let group_of = |i: usize| groups.iter().find(|g| g.contains(&i)).unwrap();
            let fits = |g: &[usize]| {
                let (a, b) = (g[0], g[g.len() - 1]);
                g.len() <= MAX_COLLAGE
                    && b - a + 1 - g.len() <= COLLAGE_REACH
                    && v[a..=b].iter().all(|s| s.chapter == v[a].chapter)
                    && g.iter()
                        .all(|&i| within_window(v[a].taken_at, v[i].taken_at))
            };
            for g in &groups {
                assert!(g.windows(2).all(|w| w[0] < w[1]), "chronological {g:?}");
                if g.len() > 1 {
                    assert!(g.iter().all(|&i| v[i].eligible), "{g:?}");
                    assert!(fits(g), "{g:?}");
                    continue;
                }
                let i = g[0];
                if !v[i].eligible {
                    continue;
                }
                // A still left alone cannot join the group of its nearest
                // eligible neighbour on either side.
                let prev = (0..i).rev().find(|&j| v[j].eligible);
                let next = (i + 1..len).find(|&j| v[j].eligible);
                for j in prev.into_iter().chain(next) {
                    let mut merged = group_of(j).clone();
                    merged.push(i);
                    merged.sort_unstable();
                    assert!(!fits(&merged), "{i} alone beside {j}: {groups:?}");
                }
            }
        }
    }

    #[test]
    fn estimate_matches_regrouped_segments() {
        let clip = PlanEntry {
            rendition: Rendition::Clip {
                trim_start: 0.0,
                trim_end: 6.0,
            },
            ..still_entry("x2")
        };
        let entries = [
            still_entry("x1"),
            clip,
            still_entry("x3"),
            still_entry("y1"),
        ];
        let ch = |id: &str| if id.starts_with('x') { "X" } else { "Y" };
        let bare = HeroCards {
            route: false,
            opening_map: true,
            route_stops: 0,
            route_secs: None,
            places: 0,
        };
        // Title 2.5, x1+x3 collage 5, clip 6, y1 solo 4 = 17.5 s; cuts
        // opening 0.7, plain 0.54, closing 0.7 = 1.94 s.
        let est = estimate_hero_secs(&entries, ch, |_| None, |_| 0.0, |_| Some(0), bare);
        assert!((est - 15.56).abs() < 1e-9, "{est}");
        // Captioning x3 leaves x1 nobody to pair with: x1 solo 4, clip 6,
        // x3 solo 4, y1 solo 4 + title 2.5 = 20.5 s; cuts 0.7 + 0.54 +
        // 0.54 + 0.7 = 2.48 s.
        let short = |id: &str| if id == "x3" { 3.0 } else { 0.0 };
        let est = estimate_hero_secs(&entries, ch, |_| None, short, |_| Some(0), bare);
        assert!((est - 18.02).abs() < 1e-9, "{est}");
        // A caption that takes 6 s to read holds x3 for 6 s, not 4.
        let long = |id: &str| if id == "x3" { 6.0 } else { 0.0 };
        let est = estimate_hero_secs(&entries, ch, |_| None, long, |_| Some(0), bare);
        assert!((est - 20.02).abs() < 1e-9, "{est}");
    }

    fn split_fixture() -> Vec<Asset> {
        let (city, park) = ((22.282, 114.154), (22.313, 114.043));
        let t0 = 1_792_285_200;
        let mut assets = vec![];
        for k in 0..8 {
            let mut c = asset(
                &format!("IMG_{}", 1502 + k),
                t0 + k * 3000,
                AssetKind::Photo,
            );
            c.gps = Some(city);
            let mut p = asset(
                &format!("IMG_{}", 5801 + k),
                t0 + k * 3000 + 1500,
                AssetKind::Photo,
            );
            p.gps = Some(park);
            assets.extend([c, p]);
        }
        assets[0].user_caption = Some("I toured the city".into());
        assets
    }

    fn split_plan(assets: &[Asset]) -> Plan {
        let cluster_of = crate::cluster::chapter_labels(&crate::cluster::cluster_assets(assets));
        build_plan_with_length(
            assets,
            &cluster_of,
            &HashMap::new(),
            &[],
            FilmLength::Secs(3600.0),
        )
    }

    fn owned(ids: Vec<&str>) -> Vec<String> {
        ids.into_iter().map(String::from).collect()
    }

    #[test]
    fn a_split_day_plays_the_owners_party_then_the_others() {
        let assets = split_fixture();
        let plan = split_plan(&assets);
        let hero = hero_ids(&plan);
        let first_park = hero.iter().position(|id| id.starts_with("IMG_58")).unwrap();
        assert!(
            hero[..first_park].iter().all(|id| id.starts_with("IMG_15")),
            "{hero:?}"
        );
        assert!(
            hero[first_park..].iter().all(|id| id.starts_with("IMG_58")),
            "{hero:?}"
        );
        assert_eq!(plan.chapters.len(), 2);
        assert_eq!(
            plan.chapters[1].place.as_deref(),
            Some("Hong Kong Disneyland")
        );
        assert_ne!(plan.chapters[0].place, plan.chapters[1].place);
        // The route follows one party, then hops once to the other.
        let hops = plan
            .map_polyline
            .windows(2)
            .filter(|w| crate::geo::haversine_m(w[0], w[1]) > 3000.0)
            .count();
        assert_eq!(hops, 1, "{:?}", plan.map_polyline);
    }

    fn moved(plan: &Plan, assets: &[Asset], moves: &[(&str, &str, bool)]) -> Vec<String> {
        let mut edits = PlanEdits::default();
        for (x, y, after) in moves {
            edits.set_move(x.to_string(), y.to_string(), *after);
        }
        owned(hero_ids(&apply_edits(plan, assets, &edits)))
    }

    #[test]
    fn a_move_places_a_shot_next_to_another_of_its_day() {
        let assets = split_fixture();
        let plan = split_plan(&assets);
        let base = owned(hero_ids(&plan));
        let at = |v: &[String], id: &str| v.iter().position(|x| x == id).unwrap();
        // Within a chapter: the fourth city shot opens the day.
        let v = moved(&plan, &assets, &[("IMG_1505", "IMG_1502", false)]);
        assert_eq!(at(&v, "IMG_1505") + 1, at(&v, "IMG_1502"));
        assert_eq!(v.len(), base.len());
        // Across the day's chapters: a park shot right after a city shot.
        let v = moved(&plan, &assets, &[("IMG_5803", "IMG_1504", true)]);
        assert_eq!(at(&v, "IMG_1504") + 1, at(&v, "IMG_5803"));
        // Chapters are untouched.
        let mut edits = PlanEdits::default();
        edits.set_move("IMG_5803".into(), "IMG_1504".into(), true);
        let members = |p: &Plan| -> Vec<Vec<String>> {
            p.chapters.iter().map(|c| c.asset_ids.clone()).collect()
        };
        assert_eq!(
            members(&apply_edits(&plan, &assets, &edits)),
            members(&plan)
        );
        // Chained, in either order of making: 1502, then 1507, then 1506.
        for moves in [
            [
                ("IMG_1507", "IMG_1502", true),
                ("IMG_1506", "IMG_1507", true),
            ],
            [
                ("IMG_1506", "IMG_1507", true),
                ("IMG_1507", "IMG_1502", true),
            ],
        ] {
            let v = moved(&plan, &assets, &moves);
            let (a, b, c) = (at(&v, "IMG_1502"), at(&v, "IMG_1507"), at(&v, "IMG_1506"));
            assert_eq!((b, c), (a + 1, a + 2), "{v:?}");
        }
        // Undone cleanly.
        let mut edits = PlanEdits::default();
        edits.set_move("IMG_1505".into(), "IMG_1502".into(), false);
        assert!(edits.unmove("IMG_1505"));
        assert!(!edits.unmove("IMG_1505"));
        assert!(edits.is_empty());
        assert_eq!(
            hero_ids(&apply_edits(&plan, &assets, &edits)),
            hero_ids(&plan)
        );
    }

    #[test]
    fn moves_across_days_or_out_of_the_film_are_ignored() {
        let mut assets = split_fixture();
        let mut next_day = asset("IMG_1600", 1_792_285_200 + 86_400, AssetKind::Photo);
        next_day.gps = Some((22.282, 114.154));
        assets.push(next_day);
        let plan = split_plan(&assets);
        assert!(hero_ids(&plan).contains(&"IMG_1600"));
        let base = owned(hero_ids(&plan));
        assert_eq!(
            moved(&plan, &assets, &[("IMG_1600", "IMG_1502", false)]),
            base
        );
        assert_eq!(moved(&plan, &assets, &[("IMG_1502", "gone", true)]), base);
        // A removed anchor leaves the shot where it was.
        let mut edits = PlanEdits::default();
        edits.set_move("IMG_1505".into(), "IMG_1502".into(), false);
        edits.set_excluded("IMG_1502".into(), true);
        let mut want = base.clone();
        want.retain(|id| id != "IMG_1502");
        assert_eq!(owned(hero_ids(&apply_edits(&plan, &assets, &edits))), want);
    }

    #[test]
    fn move_rejection_names_why_a_move_cant_apply() {
        let mut assets = split_fixture();
        let mut next_day = asset("IMG_1600", 1_792_285_200 + 86_400, AssetKind::Photo);
        next_day.gps = Some((22.282, 114.154));
        assets.push(next_day);
        let plan = split_plan(&assets);
        let get = |id: &str| assets.iter().find(|a| a.id == id).unwrap();
        let outside = asset("IMG_9999", 1_792_285_200, AssetKind::Photo);
        let check = |x: &Asset, y: &Asset| move_rejection(&plan.hero, x, y);
        assert_eq!(check(get("IMG_1505"), get("IMG_1502")), None);
        assert_eq!(
            check(&outside, get("IMG_1502")),
            Some(MoveRejection::NotInFilm { anchor: false })
        );
        assert_eq!(
            check(get("IMG_1502"), &outside),
            Some(MoveRejection::NotInFilm { anchor: true })
        );
        assert_eq!(
            check(get("IMG_1502"), get("IMG_1502")),
            Some(MoveRejection::SameShot)
        );
        assert_eq!(
            check(get("IMG_1600"), get("IMG_1502")),
            Some(MoveRejection::OtherDay)
        );
    }

    #[test]
    fn moves_survive_a_replan_and_old_edits_still_load() {
        let assets = split_fixture();
        let mut edits = PlanEdits::default();
        edits.set_move("IMG_1505".into(), "IMG_1502".into(), false);
        let json = serde_json::to_string(&edits).unwrap();
        let loaded: PlanEdits = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, edits);
        // A rebuilt plan of the same assets takes the stored move.
        let replanned = split_plan(&assets);
        let v = owned(hero_ids(&apply_edits(&replanned, &assets, &loaded)));
        let at = |id: &str| v.iter().position(|x| x == id).unwrap();
        assert_eq!(at("IMG_1505") + 1, at("IMG_1502"));
        let old: PlanEdits = serde_json::from_str(r#"{"excluded":["a"]}"#).unwrap();
        assert!(old.moves.is_empty());
    }
}
