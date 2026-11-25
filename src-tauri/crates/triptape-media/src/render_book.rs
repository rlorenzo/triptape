//! Keepsake 3 — Photo Book (vendor JPEG pack + proof/press PDFs).
//!
//! PLAN.md Sec 11 reality check: no vendor takes an imposed PDF, so the
//! product is per-page sRGB JPEGs at exact trim size + a screen proof PDF +
//! a vendor guide. Layout geometry is computed in pure Rust (testable);
//! Typst CLI turns the JPEGs + captions into `proof.pdf` (screen) and
//! `press.pdf` (bleed) — when Typst is unavailable the JPEG pack, guide,
//! and manifest still ship with a recorded fallback.

use crate::titlecard::split_dateline;
use image::{Rgb, RgbImage};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use triptape_ai_local::FaceBox;
use triptape_core::plan::SAME_MOMENT_SECS;

/// Book preset (PLAN.md Sec 11: only two in v1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BookPreset {
    /// Classic Square: 12x12 in class. 24–60 pages.
    P1,
    /// Travel Landscape: 11x8.5 in class. 24–80 pages.
    P2,
}

impl BookPreset {
    pub fn from_flag(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "P1" => Some(BookPreset::P1),
            "P2" => Some(BookPreset::P2),
            _ => None,
        }
    }

    /// Trim size in pixels at 300 dpi: (w, h).
    pub fn trim_px(&self) -> (u32, u32) {
        match self {
            BookPreset::P1 => (3600, 3600),
            BookPreset::P2 => (3300, 2550),
        }
    }

    /// Trim size in inches: (w, h).
    pub fn trim_inches(&self) -> (f64, f64) {
        let (w, h) = self.trim_px();
        (f64::from(w) / 300.0, f64::from(h) / 300.0)
    }

    /// (min, default max) counted interior pages.
    pub fn page_range(&self) -> (usize, usize) {
        match self {
            BookPreset::P1 => (24, 60),
            BookPreset::P2 => (24, 80),
        }
    }

    /// The counted-page cap for one export: the preset's default max, or
    /// the owner's choice. Vendors differ (Shutterfly 111, Snapfish 150,
    /// Mixbook 400), so the owner picks the cap their book allows.
    pub fn page_cap(&self, requested: Option<usize>) -> Result<usize, String> {
        let (min, max) = self.page_range();
        match requested {
            None => Ok(max),
            Some(n) if (min..=MAX_BOOK_PAGES).contains(&n) => Ok(n),
            Some(n) => Err(format!(
                "{n} pages is outside {min}-{MAX_BOOK_PAGES} for {}",
                self.name()
            )),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            BookPreset::P1 => "Classic Square 12x12",
            BookPreset::P2 => "Travel Landscape 11x8.5",
        }
    }
}

/// Most pages any supported vendor binds (Mixbook's 400-page limit).
pub const MAX_BOOK_PAGES: usize = 400;

/// One placeable still for the book.
#[derive(Debug, Clone)]
pub struct BookPhoto {
    pub asset_id: String,
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub caption: Option<String>,
    /// Index into the `chapters` titles given to [`render_book`].
    pub chapter: Option<usize>,
    /// Print score: technical quality plus story bonuses (people, the
    /// user's caption). Ranks photos within a chapter when the book is
    /// trimmed to its page cap; [`NEUTRAL_SCORE`] when unscored.
    pub score: f32,
    /// The user curated this photo (wrote its caption): it is placed before
    /// any scored pick and never dropped silently. The chapter-place
    /// fallback caption does not count.
    pub keep: bool,
    /// Pinned by the owner (`--book` or `--cover`): placed even when its
    /// caption repeats another kept photo's. Its caption is the owner's
    /// only when they wrote one (see [`BookPhoto::own_caption`]).
    pub pinned: bool,
    /// Capture time (unix seconds), perceptual hash and tagged people: what
    /// [`BookPhoto::repeats`] needs to tell a near-identical shot of the same
    /// people from a new scene.
    pub taken_at: Option<i64>,
    pub phash: Option<u64>,
    pub people_sig: Option<u64>,
    /// Image embedding, when the local model made one: lets the repeat
    /// test catch the same pose reframed, which pHash misses.
    pub embedding: Option<triptape_core::embedding::Embedding>,
    /// 1-based trip day (local date), numbered as the plan's chapters are.
    /// Pages never mix two days, and each day's first page names it.
    pub day: Option<u32>,
    /// Where the photo was taken, as the book names it (a named place, a
    /// visited attraction, else the chapter's place). The page's location
    /// strip lists it even when the user's caption is shown instead.
    pub place: Option<String>,
    /// The owner-named place the photo was taken in (the Flower Dome, the
    /// Cloud Forest): pages never mix two, so each visit reads as its own.
    pub area: Option<String>,
    /// People (or detected faces) in the photo: cropping a group shot's
    /// edges cuts faces, so grid layouts give such photos room.
    pub people: u32,
    /// People tagged in the photo library (no statues or strangers).
    pub tagged_people: u32,
    /// Largest detected face's side as a fraction of the photo's shorter
    /// side (0 when unknown): a big face is someone the photo is of.
    pub face_size: f32,
    /// Pinned to the cover by the owner, with the tile's horizontal focus
    /// (0 left edge .. 1 right edge).
    pub cover_focus: Option<f32>,
    /// The owner's crop focus, (x, y) as fractions of the photo: crops
    /// centre there instead of on detected faces.
    pub crop_focus: Option<(f32, f32)>,
    /// A video shown as one frame, this many milliseconds in.
    pub still_ms: Option<u32>,
    /// The owner wants this photo in the biggest cell on its page.
    pub feature: bool,
    /// The owner wants this photo never cropped.
    pub whole: bool,
    /// The owner wants this photo's page laid out as one row (`--row`).
    pub row: bool,
    /// The owner's caption speaks for the whole page (`--page-caption`):
    /// it prints in the page's bottom line, not under this photo.
    pub caption_for_page: bool,
    /// In one of the owner's page groups: its stretch prints on one page.
    pub page_group: bool,
    /// The photo's place is a city the trip stopped in (not a sight).
    pub place_is_city: bool,
    /// The itinerary puts the shot on board the ship.
    pub aboard: bool,
    /// Detected faces (normalised boxes), when the face models ran: page
    /// layouts avoid cells that would cut them.
    pub faces: Vec<FaceBox>,
    /// The owner chose this photo for the closing page.
    pub closing: bool,
    /// Named people's regions from the photo library: framing keeps them
    /// first.
    pub family: Vec<KnownBox>,
}

impl BookPhoto {
    /// Whether the caption is the owner's words: a pin without a caption
    /// of its own falls back to its place name, which is not.
    fn own_caption(&self) -> bool {
        self.keep && !(self.pinned && self.caption == self.place)
    }

    /// A 0x0 neutral-score photo with nothing set, for tests to fill in
    /// with struct-update syntax.
    #[cfg(test)]
    pub(crate) fn blank() -> Self {
        Self {
            asset_id: String::new(),
            path: PathBuf::new(),
            width: 0,
            height: 0,
            caption: None,
            chapter: None,
            score: NEUTRAL_SCORE,
            keep: false,
            pinned: false,
            taken_at: None,
            phash: None,
            people_sig: None,
            embedding: None,
            day: None,
            people: 0,
            place: None,
            area: None,
            tagged_people: 0,
            face_size: 0.0,
            cover_focus: None,
            crop_focus: None,
            still_ms: None,
            feature: false,
            whole: false,
            row: false,
            caption_for_page: false,
            page_group: false,
            place_is_city: false,
            aboard: false,
            faces: vec![],
            closing: false,
            family: vec![],
        }
    }

    /// Whether the photo is of people: someone tagged in the library, or a
    /// face big enough to be the subject.
    fn is_of_people(&self) -> bool {
        self.tagged_people > 0 || self.face_size >= SUBJECT_FACE
    }

    /// Whether the file is a PNG: a screenshot or a composite, not a
    /// camera photo.
    fn is_png(&self) -> bool {
        self.path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("png"))
    }

    /// The photo's file name for render notes, else its asset id.
    fn display_name(&self) -> String {
        self.path.file_name().map_or_else(
            || self.asset_id.clone(),
            |n| n.to_string_lossy().into_owned(),
        )
    }

    /// Whether two photos may trade places in the page order: the same
    /// chapter, day and owner-named place, so no swap carries a photo
    /// across a page break between two visits or two days.
    fn same_stretch(&self, other: &BookPhoto) -> bool {
        (self.chapter, self.day) == (other.chapter, other.day) && self.area == other.area
    }

    /// Whether this photo repeats `other`: the same people at the same
    /// moment ([`triptape_core::plan::same_moment_at`]). With embeddings,
    /// the same scene in another framing counts too, as in the film's
    /// photo picks.
    fn repeats(&self, other: &BookPhoto) -> bool {
        // Unknown people (no tags on this copy) match anyone: a folder can
        // hold the same shot twice with tags on only one copy, and an
        // untagged frame of a tagged burst is still that burst.
        let same_people = match (self.people_sig, other.people_sig) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        };
        same_people && triptape_core::plan::same_moment_at(self.moment(), other.moment())
    }

    fn moment(&self) -> triptape_core::plan::Moment<'_> {
        triptape_core::plan::Moment {
            taken_at: self.taken_at,
            phash: self.phash,
            embedding: self.embedding.as_ref(),
        }
    }
}

/// Score of a photo nothing could be scored from.
pub const NEUTRAL_SCORE: f32 = 0.5;

/// Effective-dpi triage for one placed photo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DpiVerdict {
    Ok,
    Warn,
    Block,
}

pub fn dpi_verdict(photo_w: u32, photo_h: u32, slot_w_in: f64, slot_h_in: f64) -> DpiVerdict {
    let dpi = (photo_w as f64 / slot_w_in).min(photo_h as f64 / slot_h_in);
    if dpi >= 300.0 {
        DpiVerdict::Ok
    } else if dpi >= MIN_PRINT_DPI {
        DpiVerdict::Warn
    } else {
        DpiVerdict::Block
    }
}

/// Page kinds. `CoverProxy` is proof-only: never counted, never exported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PageKind {
    CoverProxy,
    Hero,
    PhotoCaption,
    Grid,
    SpreadLeft,
    SpreadRight,
    Map,
    Divider,
    Closing,
}

impl PageKind {
    /// Counted interior pages (cover-proxy excluded; spread halves each
    /// count — a spread always totals 2).
    pub fn counts(&self) -> usize {
        match self {
            PageKind::CoverProxy => 0,
            _ => 1,
        }
    }
}

/// Counted interior pages for a page-kind sequence.
pub fn count_pages(kinds: &[PageKind]) -> usize {
    kinds.iter().map(|k| k.counts()).sum()
}

/// One flowed page: its kind plus how many photos it consumes (0 for
/// cover-proxy, map, closing, and the right spread half; a divider consumes
/// its full-page background).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlowPage {
    pub kind: PageKind,
    pub take: usize,
}

/// Most divider pages in one book; trips with more chapters open an even
/// spread of them.
const MAX_DIVIDERS: usize = 6;

/// How many of the opening chapter's first photos may become the hero: far
/// enough to pass over a soft arrival snapshot, near enough that page 1
/// still shows the start of the trip.
const HERO_WINDOW: usize = 8;

/// Photo indices in story order as pages would consume them: hero, then the
/// spread's wide photo, then the rest. Every photo appears exactly once.
fn story_order(n_photos: usize, spread: Option<usize>) -> Vec<usize> {
    let mut order: Vec<usize> = (0..n_photos.min(1)).collect();
    order.extend(spread);
    order.extend((1..n_photos).filter(|&i| Some(i) != spread));
    order
}

/// Photo indices in the order `flow`'s pages consume them: [`story_order`]
/// with each page's photos chosen for print sharpness. The single ordering
/// both [`planned_blocked`] and [`render_book`] walk, so the dpi prune and
/// the rendered pages always agree. Photos only move within the stretch of
/// one chapter a page set draws from, so every page keeps the chapters
/// [`flow_pages`] planned for it:
/// - the hero is the best-scored of the opening chapter's first
///   [`HERO_WINDOW`] photos that reaches 300 dpi full-page (else the first);
/// - a divider is backed by its chapter's earliest photo that is not
///   blocked full-page (soft is allowed; else its first), pulled to the
///   chapter's front;
/// - a photo that would print soft on a 1- or 2-up page but sharp in a
///   3/4-up cell trades places with the nearest photo of its chapter on a
///   3/4-up page of the same run that is sharp in the bigger slot.
fn consumption_order(photos: &[BookPhoto], flow: &[FlowPage], preset: BookPreset) -> Vec<usize> {
    let has_spread = flow.iter().any(|p| p.kind == PageKind::SpreadLeft);
    let spread = wide_index(photos).filter(|_| has_spread);
    let mut order = story_order(photos.len(), spread);
    if order.is_empty() {
        return order;
    }
    let sharp_in = |i: usize, (w, h): (f64, f64)| {
        dpi_verdict(photos[i].width, photos[i].height, w, h) == DpiVerdict::Ok
    };
    let sharp =
        |i: usize, kind: PageKind, take: usize| sharp_in(i, slot_inches(preset, kind, take));
    // Position (in `order`) of the best-scored photo among `positions` that
    // is sharp full-page as `kind`.
    let best_sharp_at = |order: &[usize], positions: &[usize], kind: PageKind| {
        best_scored(
            positions
                .iter()
                .copied()
                .filter(|&pos| sharp(order[pos], kind, 1)),
            |pos| photos[order[pos]].score,
        )
    };

    // Hero: the opening chapter's leading photos in `order` (position 0,
    // then the rest after the spread) share one chapter, so swapping which
    // of them is the hero leaves every page's chapters unchanged.
    let rest_at = 1 + usize::from(spread.is_some());
    let lead: Vec<usize> = std::iter::once(0)
        .chain(rest_at..order.len())
        .take_while(|&pos| photos[order[pos]].same_stretch(&photos[order[0]]))
        .take(HERO_WINDOW)
        .collect();
    if let Some(pos) = best_sharp_at(&order, &lead, PageKind::Hero).filter(|&pos| pos != 0) {
        // The old hero takes the first rest slot; the others keep story order.
        order[rest_at..=pos].rotate_right(1);
        order.swap(0, rest_at);
    }

    // Each page with where it starts in `order`.
    let pages: Vec<(&FlowPage, usize)> = flow
        .iter()
        .scan(0, |at, p| {
            let s = *at;
            *at += p.take;
            Some((p, s))
        })
        .collect();
    let divider_at: Vec<usize> = pages
        .iter()
        .filter(|(p, _)| p.kind == PageKind::Divider)
        .map(|&(_, s)| s)
        .collect();
    for (d, &at) in divider_at.iter().enumerate() {
        let limit = divider_at.get(d + 1).copied().unwrap_or(order.len());
        let opener = &photos[order[at]];
        let run: Vec<usize> = (at..limit)
            .take_while(|&pos| photos[order[pos]].same_stretch(opener))
            .collect();
        // The earliest that prints well enough full-page (soft is allowed,
        // blocked is not), so the title page does not jump ahead of the
        // photos taken before it (a castle view from the end of the visit
        // opening the day). A title page is a portrait of the day: of the
        // section's first few shots, the earliest with people (not a plate
        // of food) comes first.
        let (w, h) = slot_inches(preset, PageKind::Divider, 1);
        let printable = |pos: usize| {
            let p = &photos[order[pos]];
            dpi_verdict(p.width, p.height, w, h) != DpiVerdict::Block
        };
        // A tagged family face beats a stranger's elbow behind a plate.
        // Only photos from the chapter's first moments: a divider must not
        // pull a shot from later in the visit ahead of the arrival.
        let first_at = photos[order[run[0]]].taken_at;
        let window: Vec<usize> = run[..run.len().min(DIVIDER_WINDOW)]
            .iter()
            .copied()
            .filter(|&pos| {
                photos[order[pos]]
                    .taken_at
                    .zip(first_at)
                    .is_none_or(|(t, f)| t - f <= DIVIDER_SECS)
            })
            .collect();
        let tier = |pos: usize| {
            let p = &photos[order[pos]];
            if p.family.iter().any(|k| !k.body) {
                0
            } else if p.is_of_people() {
                1
            } else {
                2
            }
        };
        if let Some(pos) = window
            .iter()
            .copied()
            .filter(|&pos| printable(pos) && tier(pos) < 2)
            .min_by_key(|&pos| tier(pos))
            .or_else(|| run.iter().copied().find(|&pos| printable(pos)))
        {
            order[at..=pos].rotate_right(1);
        }
    }

    // Soft photos in a big cell (a 1-up page, or a large grid cell) move
    // into a quarter-page-or-smaller cell of the same run (pages between
    // dividers), swapping with the nearest photo of the same chapter there
    // that stays sharp in the bigger slot. Cells are sized for the photos
    // the page holds before the swaps.
    // A cell counts as small up to a quarter page (1% float slack).
    let small_cell = {
        let (w, h) = preset.trim_inches();
        w * h / 4.0 * 1.01
    };
    for run in pages.split(|(p, _)| p.kind == PageKind::Divider) {
        let (small_cells, big_cells): (Vec<_>, Vec<_>) = run
            .iter()
            .filter(|(p, _)| matches!(p.kind, PageKind::PhotoCaption | PageKind::Grid))
            .flat_map(|&(p, s)| {
                let slot: Vec<&BookPhoto> =
                    order[s..s + p.take].iter().map(|&i| &photos[i]).collect();
                slot_sizes(preset, p.kind, &slot)
                    .into_iter()
                    .enumerate()
                    .map(move |(k, size)| (s + k, size))
                    .collect::<Vec<_>>()
            })
            .partition(|&(_, (w, h))| w * h <= small_cell);
        for &(pos, size) in &big_cells {
            let soft = order[pos];
            if sharp_in(soft, size) {
                continue;
            }
            let partner = small_cells
                .iter()
                .filter(|&&(c, small)| {
                    // Only a shot from the same moment: a swap must not
                    // break the book's time order.
                    photos[order[c]]
                        .taken_at
                        .zip(photos[soft].taken_at)
                        .is_none_or(|(a, b)| (a - b).abs() <= SAME_MOMENT_SECS)
                        && photos[order[c]].same_stretch(&photos[soft])
                        && sharp_in(order[c], size)
                        && sharp_in(soft, small)
                })
                .map(|&(c, _)| c)
                .min_by_key(|&c| c.abs_diff(pos));
            if let Some(c) = partner {
                order.swap(pos, c);
            }
        }
    }
    // The owner's featured photo leads its page (top left) when it was
    // taken within [`FEATURE_LEAD_SECS`] of the page's first photo: a
    // moment, not a reordering of the day.
    for &(p, s) in &pages {
        if p.kind != PageKind::Grid || p.take < 2 {
            continue;
        }
        let end = (s + p.take).min(order.len());
        let Some(pos) = (s + 1..end).find(|&k| photos[order[k]].feature) else {
            continue;
        };
        let near = photos[order[pos]]
            .taken_at
            .zip(photos[order[s]].taken_at)
            .is_some_and(|(a, b)| (a - b).abs() <= FEATURE_LEAD_SECS);
        if near && !photos[order[s]].feature {
            order[s..=pos].rotate_right(1);
        }
    }
    order
}

/// Page sizes for `n` photos at up to `per_page` a page, as even as the
/// fewest pages allow: 9 photos at 4 a page are 3 + 3 + 3, not 4 + 4 + 1,
/// so no photo (a landscape left over from a day of family shots) is
/// blown up to a page of its own unless the stretch holds only that one.
fn even_split(n: usize, per_page: usize) -> Vec<usize> {
    let pages = n.div_ceil(per_page.max(1));
    (0..pages)
        .map(|k| n / pages + usize::from(k < n % pages))
        .collect()
}

/// Most photos one page holds (three rows of three).
const MAX_PAGE_PHOTOS: usize = 9;

/// Flow photos into a page plan. `chapter_of` holds each photo's chapter in
/// story order and `wide_idx` the first wide (≥1.5:1) photo after the hero.
/// Density adapts so small books still clear the 24-page vendor minimum (one
/// photo per page when it fits under the cap, grids of up to 4 otherwise). A
/// spread pair at counted pages 2–3 uses the wide photo when at least 5 photos
/// remain — always starting on an even (left) page. The hero opens its own
/// chapter; every other chapter with a placed photo gets a divider that
/// consumes one of its photos as background (up to [`MAX_DIVIDERS`];
/// [`consumption_order`] picks which), and no photo page crosses one. No
/// photo page mixes two trip days either: `day_of` holds each photo's day
/// in story order (empty: days unknown, no day breaks).
///
/// A chapter whose first photo is marked in `no_divider` gets no divider:
/// its heading goes on its first photo page (a captioned plate of food
/// opens the day in time order, on a shared page, instead of yielding to a
/// later family photo pulled ahead of it). A stretch holding a photo marked
/// in `one_page` (the owner's page group) prints on one page, up to
/// [`MAX_PAGE_PHOTOS`]. Either may be empty: no photo marked.
pub fn flow_pages(
    chapter_of: &[Option<usize>],
    day_of: &[Option<u32>],
    no_divider: &[bool],
    one_page: &[bool],
    wide_idx: Option<usize>,
    has_map: bool,
    cap: usize,
) -> Vec<FlowPage> {
    let n_photos = chapter_of.len();
    let mut pages = vec![FlowPage {
        kind: PageKind::CoverProxy,
        take: 0,
    }];
    if n_photos == 0 {
        pages.push(FlowPage {
            kind: PageKind::Closing,
            take: 0,
        });
        return pages;
    }
    pages.push(FlowPage {
        kind: PageKind::Hero,
        take: 1,
    });
    let spread = wide_idx.filter(|_| n_photos >= 6);
    if spread.is_some() {
        pages.push(FlowPage {
            kind: PageKind::SpreadLeft,
            take: 1,
        });
        pages.push(FlowPage {
            kind: PageKind::SpreadRight,
            take: 0,
        });
    }
    let counted_before: usize = pages.iter().map(|p| p.kind.counts()).sum();
    // Closing page plus the optional map.
    let extras = 1 + usize::from(has_map);
    let rest_idx: Vec<usize> = story_order(n_photos, spread)
        .into_iter()
        .skip(1 + usize::from(spread.is_some()))
        .collect();
    let rest: Vec<Option<usize>> = rest_idx.iter().map(|&i| chapter_of[i]).collect();
    let day_at = |k: usize| day_of.get(rest_idx[k]).copied().flatten();
    // Whether the photo at `k` is marked in `flags` (empty: none is).
    let marked = |flags: &[bool], k: usize| flags.get(rest_idx[k]).copied().unwrap_or(false);
    // A chapter opens at its first photo left after the hero and spread.
    let mut opened: HashSet<usize> = chapter_of[0].into_iter().collect();
    let openings: Vec<usize> = rest
        .iter()
        .enumerate()
        .filter(|(_, c)| c.is_some_and(|c| opened.insert(c)))
        .map(|(i, _)| i)
        .collect();
    // With more chapters than dividers, a lone-photo chapter's divider would
    // use up its photo and lead straight into the next, undivided chapter's
    // pages under the wrong title, so only chapters with photos to spare
    // compete for the limited dividers.
    let openings: Vec<usize> = openings
        .into_iter()
        .filter(|&o| !marked(no_divider, o))
        .collect();
    let eligible: Vec<usize> = if openings.len() > MAX_DIVIDERS {
        let mut photos_in: HashMap<Option<usize>, usize> = HashMap::new();
        for c in &rest {
            *photos_in.entry(*c).or_default() += 1;
        }
        openings
            .into_iter()
            .filter(|&o| photos_in[&rest[o]] > 1)
            .collect()
    } else {
        openings
    };
    let dividers: Vec<usize> = evenly_spaced(eligible.len(), MAX_DIVIDERS)
        .into_iter()
        .map(|j| eligible[j])
        .collect();
    // Photo-page runs between dividers, each cut into one segment per trip
    // day so no page mixes two days. Each divider consumes its photo as a
    // full-page background, so that photo never repeats later in the book.
    let mut bounds: Vec<(usize, usize)> = vec![];
    let mut start = 0;
    for &d in &dividers {
        bounds.push((start, d));
        start = d + 1;
    }
    bounds.push((start, rest.len()));
    let grouped = |k: usize| marked(one_page, k);
    // Each segment's length and whether it must stay on one page.
    let runs: Vec<Vec<(usize, bool)>> = bounds
        .into_iter()
        .map(|(a, b)| {
            let mut segs: Vec<(usize, bool)> = vec![];
            for k in a..b {
                match segs.last_mut() {
                    Some((n, g)) if day_at(k) == day_at(k - 1) => {
                        *n += 1;
                        *g |= grouped(k);
                    }
                    _ => segs.push((1, grouped(k))),
                }
            }
            segs
        })
        .collect();
    // An owner's group that fits one page prints whole on it.
    let pages_for = |&(len, one): &(usize, bool), per: usize| {
        if one && len <= MAX_PAGE_PHOTOS {
            vec![len]
        } else {
            even_split(len, per)
        }
    };
    let photo_budget = cap
        .saturating_sub(counted_before + dividers.len() + extras)
        .max(1);
    let per_page = (1..=4)
        .find(|&p| {
            runs.iter()
                .flatten()
                .map(|s| pages_for(s, p).len())
                .sum::<usize>()
                <= photo_budget
        })
        .unwrap_or(4);
    for (i, run) in runs.iter().enumerate() {
        if i > 0 {
            pages.push(FlowPage {
                kind: PageKind::Divider,
                take: 1,
            });
        }
        for seg in run {
            for take in pages_for(seg, per_page) {
                pages.push(FlowPage {
                    kind: if take == 1 {
                        PageKind::PhotoCaption
                    } else {
                        PageKind::Grid
                    },
                    take,
                });
            }
        }
    }
    if has_map {
        pages.push(FlowPage {
            kind: PageKind::Map,
            take: 0,
        });
    }
    pages.push(FlowPage {
        kind: PageKind::Closing,
        take: 0,
    });
    pages
}

/// The size in inches each photo of `slot` gets on a `kind` page, in slot
/// order. Grid pages lay out by photo shape ([`grid_layout`]); every other
/// page gives each photo the kind's slot.
pub fn slot_sizes(preset: BookPreset, kind: PageKind, slot: &[&BookPhoto]) -> Vec<(f64, f64)> {
    if kind == PageKind::Grid && slot.len() > 1 {
        let (w, h) = preset.trim_inches();
        return sharp_cells(slot, grid_cells(slot, w, h), 1.0)
            .into_iter()
            .map(|(_, _, cw, ch)| (cw, ch))
            .collect();
    }
    vec![slot_inches(preset, kind, slot.len().max(1)); slot.len()]
}

/// Photo slot size in inches for dpi triage (photo area of the layout); grid
/// pages size each photo by [`slot_sizes`].
pub fn slot_inches(preset: BookPreset, kind: PageKind, take: usize) -> (f64, f64) {
    match preset {
        BookPreset::P1 => match kind {
            PageKind::Hero | PageKind::PhotoCaption => (12.0, 12.0),
            PageKind::SpreadLeft | PageKind::SpreadRight => (24.0, 12.0),
            PageKind::Grid if take <= 2 => (11.0, 5.5),
            PageKind::Grid => (5.5, 5.5),
            _ => (12.0, 12.0),
        },
        BookPreset::P2 => match kind {
            PageKind::Hero | PageKind::PhotoCaption => (11.0, 8.5),
            PageKind::SpreadLeft | PageKind::SpreadRight => (22.0, 8.5),
            PageKind::Grid if take <= 2 => (10.0, 3.8),
            PageKind::Grid => (5.0, 3.8),
            _ => (11.0, 8.5),
        },
    }
}

#[derive(Debug, Clone)]
pub struct PreflightIssue {
    pub level: &'static str, // "warn" | "block"
    pub page: Option<usize>,
    pub message: String,
}

/// Preflight over the flowed book: dpi per placement, page-count caps.
/// Face-gutter and font-embedding checks are by-construction here (no face
/// boxes in the headless path; Typst embeds its own fonts) and recorded in
/// the guide instead of blocking.
pub fn preflight(
    preset: BookPreset,
    max: usize,
    kinds: &[PageKind],
    placements: &[(usize, DpiVerdict)],
) -> Vec<PreflightIssue> {
    let mut issues = vec![];
    let counted = count_pages(kinds);
    let (min, _) = preset.page_range();
    if counted < min {
        issues.push(PreflightIssue {
            level: "block",
            page: None,
            message: format!("{counted} pages below the {min}-page vendor minimum; add photos"),
        });
    }
    if counted > max {
        issues.push(PreflightIssue {
            level: "block",
            page: None,
            message: format!("{counted} pages over the {max}-page cap set for this export; remove photos or raise the cap"),
        });
    }
    for (page, v) in placements {
        match v {
            DpiVerdict::Ok => {}
            DpiVerdict::Warn => issues.push(PreflightIssue {
                level: "warn",
                page: Some(*page),
                message: "photo at 200-299 dpi at its slot; export allowed but may look soft".into(),
            }),
            DpiVerdict::Block => issues.push(PreflightIssue {
                level: "block",
                page: Some(*page),
                message: "photo below 200 dpi even in the smallest slot; use a smaller layout, replace, or remove (or override: print anyway)".into(),
            }),
        }
    }
    issues
}

#[derive(Debug, Clone)]
pub struct BookOptions {
    pub preset: BookPreset,
    pub trip_name: String,
    pub typst_bin: String,
    /// FFmpeg used to decode stills the `image` crate cannot read (HEIC).
    pub ffmpeg_bin: String,
    /// Counted-page cap; `None` keeps the preset's default max.
    pub max_pages: Option<usize>,
    /// Progress and cancel handle (default: silent, never cancelled).
    pub progress: crate::progress::Progress,
    /// The trip's dates for the cover (`October 3–30, 2026`); `None` puts
    /// a generic line there.
    pub dates: Option<String>,
    /// Face detection for crops that keep heads in frame; `None` (no
    /// models installed) crops every photo around its centre.
    pub faces: Option<FaceFinder>,
    /// The owner's itinerary as a journey: its whole route, on a parchment
    /// map, becomes the back cover.
    pub journey: Option<triptape_core::journey::Journey>,
    /// The owner's own back-cover picture, used in place of the drawn map.
    pub back_cover_image: Option<PathBuf>,
    /// The owner's own front-cover picture (the titled route map): the
    /// photo mosaic then moves to the back cover, as sights without people.
    pub front_cover_image: Option<PathBuf>,
    /// The photos of a book the owner approved page by page (asset ids):
    /// when set, the pages print only these and the owner's pins, so an
    /// edit to one page no longer reshuffles the automatic picks on the
    /// others.
    pub approved: Option<HashSet<String>>,
}

/// Finds the faces in a decoded photo, as boxes normalised to 0..1.
#[derive(Clone)]
pub struct FaceFinder(pub std::sync::Arc<FindFaces>);

/// A face detector over a decoded photo.
pub type FindFaces = dyn Fn(&RgbImage) -> Vec<FaceBox> + Send + Sync;

impl std::fmt::Debug for FaceFinder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FaceFinder")
    }
}

/// How page painters decode and crop a photo.
struct Fit<'a> {
    ffmpeg: &'a str,
    faces: Option<&'a FaceFinder>,
    /// Faces found so far, by asset id: a photo on the cover and a page,
    /// or both halves of a spread, runs the detector once.
    found: std::cell::RefCell<HashMap<String, Vec<FaceBox>>>,
}

#[derive(Debug)]
pub struct BookOutput {
    pub dir: PathBuf,
    /// Photo-collage cover (proof/press first page; not a counted page).
    pub cover: Option<PathBuf>,
    /// Route-map back cover (proof/press last page; not a counted page).
    pub back_cover: Option<PathBuf>,
    pub jpeg_pages: Vec<PathBuf>,
    pub proof_pdf: Option<PathBuf>,
    pub press_pdf: Option<PathBuf>,
    pub guide_path: PathBuf,
    pub counted_pages: usize,
    pub fallbacks: Vec<String>,
}

/// Largest-face size (of the shorter side) the closing photo needs: the
/// family close enough to read on the last page, not a statue with them
/// at its foot.
const CLOSING_FACE: f32 = 0.12;

/// Photos at the end of the book the closing page never repeats.
const CLOSING_SKIPS_LAST: usize = 12;

/// How many of a section's first photos may back its title page.
const DIVIDER_WINDOW: usize = 8;

/// How many photos after the hero may supply the opening spread.
const SPREAD_WINDOW: usize = 11;

/// First wide (≥1.5:1) photo in the opening stretch after the hero: the
/// spread candidate. Pulling a later wide shot to pages 2–3 would break the
/// book's chronology, so the candidate must share the hero's chapter: a
/// Tokyo panorama 4 photos in must not jump ahead of the rest of the
/// departure-day photos. No early wide photo in that chapter means no
/// spread.
fn wide_index(photos: &[BookPhoto]) -> Option<usize> {
    let opening = photos.first()?.chapter;
    photos
        .iter()
        .skip(1)
        .take(SPREAD_WINDOW)
        .take_while(|p| p.chapter == opening)
        // An owner's page group stays whole: none of it becomes the spread.
        .position(|p| !p.page_group && p.height > 0 && p.width as f64 / p.height as f64 >= 1.5)
        .map(|i| i + 1)
}

/// The page plan for exactly these photos.
fn book_flow(photos: &[BookPhoto], has_map: bool, cap: usize) -> Vec<FlowPage> {
    let chapter_of: Vec<Option<usize>> = photos.iter().map(|p| p.chapter).collect();
    // A page break wherever the day or the owner-named place changes.
    let mut stretch = 0u32;
    let stretch_of: Vec<Option<u32>> = photos
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if i > 0 && (p.day, &p.area) != (photos[i - 1].day, &photos[i - 1].area) {
                stretch += 1;
            }
            Some(stretch)
        })
        .collect();
    // A day opening on the owner's captioned shot with no one in it (a
    // plate of food) keeps its time order: no divider pulls a later family
    // photo ahead of it. Nor does a divider take a photo out of the
    // owner's page group.
    let no_divider: Vec<bool> = photos
        .iter()
        .map(|p| p.page_group || (p.keep && !p.is_of_people()))
        .collect();
    let one_page: Vec<bool> = photos.iter().map(|p| p.page_group).collect();
    flow_pages(
        &chapter_of,
        &stretch_of,
        &no_divider,
        &one_page,
        wide_index(photos),
        has_map,
        cap,
    )
}

/// The selected photos as the pages print them: the owner's closing photo
/// (`closing_id`, once it passes the full-page check) off the pages, lone
/// captioned shots folded into a neighbouring stretch, and lone scenery
/// left out, as [`render_book`] does after selection.
fn printed_pool(mut picked: Vec<BookPhoto>, closing_id: Option<&str>) -> Vec<BookPhoto> {
    if let Some(id) = closing_id {
        picked.retain(|p| p.asset_id != id);
    }
    drop_lone_scenery(&mut picked);
    picked
}

/// Folds lone captioned shots into a neighbouring stretch, then removes
/// the lone scenery left over; returns the removed asset ids. Shared by
/// the page count and [`render_book`] so the two never disagree.
fn drop_lone_scenery(photos: &mut Vec<BookPhoto>) -> HashSet<String> {
    fold_lone_kept(photos);
    let lone: HashSet<String> = lone_scenery(photos).into_iter().collect();
    photos.retain(|p| !lone.contains(&p.asset_id));
    lone
}

/// Whether `p` may print on the pages at all. With the owner's front cover,
/// the pinned sights belong to the back cover alone (unless the back cover
/// is the owner's picture too, which shows no sights). With an
/// [`BookOptions::approved`] book, only its photos and the owner's pins
/// print.
fn on_pages(p: &BookPhoto, opts: &BookOptions) -> bool {
    let back_cover_only = opts.front_cover_image.is_some()
        && opts.back_cover_image.is_none()
        && p.cover_focus.is_some();
    let approved = opts
        .approved
        .as_ref()
        .is_none_or(|ids| p.pinned || p.closing || ids.contains(&p.asset_id));
    !back_cover_only && approved
}

/// Largest selection size found whose flowed book fits `cap` counted pages.
/// Pages depend on which chapters the selection keeps, not only on its size,
/// so each candidate is flowed for real. That also makes fit non-monotonic:
/// the search may settle a little low, but what it returns always fits.
fn fitting_count(pool: &[BookPhoto], has_map: bool, cap: usize, closing_id: Option<&str>) -> usize {
    // Scenes depend only on the pool: link them once for every probe.
    let scene = scenes(pool);
    // Each size is flowed once: the look either side of the search below
    // revisits sizes the search already tried.
    let mut flowed: HashMap<usize, usize> = HashMap::new();
    let mut pages = |n: usize| {
        *flowed.entry(n).or_insert_with(|| {
            let picked: Vec<BookPhoto> = select_in_scenes(pool, &scene, n)
                .0
                .into_iter()
                .map(|i| pool[i].clone())
                .collect();
            // Counted as the pages will be printed: the owner's closing photo
            // leaves the pages and lone scenery shots drop (see
            // `render_book`), so the selection fills the cap instead of
            // falling short of it.
            let picked = printed_pool(picked, closing_id);
            count_pages(
                &book_flow(&picked, has_map, cap)
                    .iter()
                    .map(|p| p.kind)
                    .collect::<Vec<_>>(),
            )
        })
    };
    if pages(pool.len()) <= cap {
        return pool.len();
    }
    let (mut lo, mut hi) = (0usize, pool.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if pages(mid) <= cap {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    // Fit is not monotonic: one more photo can respace the picks across the
    // chapters and add a divider or two (264 photos made 109 pages, 265
    // made 112). Look a little either side and keep the selection that
    // fills the most pages within the cap, the larger on a tie.
    (lo.saturating_sub(FIT_LOOKAHEAD)..=(lo + FIT_LOOKAHEAD).min(pool.len()))
        .filter_map(|n| {
            let p = pages(n);
            (p <= cap).then_some((p, n))
        })
        .max()
        .map_or(lo, |(_, n)| n)
}

/// How many selection sizes either side of its search [`fitting_count`] tries.
const FIT_LOOKAHEAD: usize = 24;

/// Evenly spaced indices into `total`, always including the first item,
/// so a trimmed book still spans the whole trip in story order.
fn evenly_spaced(total: usize, keep: usize) -> Vec<usize> {
    if keep >= total {
        return (0..total).collect();
    }
    if keep == 0 {
        return vec![];
    }
    // i * total / keep is strictly increasing when keep < total.
    (0..keep).map(|i| i * total / keep).collect()
}

/// Pick `keep` indices for the cap. User-curated photos ([`BookPhoto::keep`])
/// are reserved first. The remaining slots are shared among the chapters in
/// proportion to their size (every chapter gets one while slots last), and
/// each chapter fills its share with its best-scored photos, one per
/// stretch of consecutive shots, so the picks are the best of each scene
/// and still span the chapter; a near-identical shot of the same people as
/// one already picked waits (see [`best_per_stretch`]). Returns the sorted
/// selection plus any curated indices that could not fit (more of them than
/// slots).
fn select_for_cap(photos: &[BookPhoto], keep: usize) -> (Vec<usize>, Vec<usize>) {
    select_in_scenes(photos, &scenes(photos), keep)
}

/// [`select_for_cap`] with the pool's [`scenes`] already linked.
fn select_in_scenes(
    photos: &[BookPhoto],
    scene: &[usize],
    keep: usize,
) -> (Vec<usize>, Vec<usize>) {
    let total = photos.len();
    if keep >= total {
        return ((0..total).collect(), vec![]);
    }
    let curated = distinct_curated(photos, scene);
    if curated.len() > keep {
        let picked: Vec<usize> = evenly_spaced(curated.len(), keep)
            .into_iter()
            .map(|j| curated[j])
            .collect();
        let dropped = curated
            .iter()
            .copied()
            .filter(|i| !picked.contains(i))
            .collect();
        return (picked, dropped);
    }
    // Uncurated photos by chapter, chapters and members in story order.
    let mut groups: Vec<(Option<usize>, Vec<usize>)> = vec![];
    for i in (0..total).filter(|&i| !photos[i].keep) {
        match groups.iter_mut().find(|(c, _)| *c == photos[i].chapter) {
            Some((_, members)) => members.push(i),
            None => groups.push((photos[i].chapter, vec![i])),
        }
    }
    let sizes: Vec<usize> = groups.iter().map(|(_, m)| m.len()).collect();
    let mut sel = curated;
    for ((_, members), quota) in groups.iter().zip(chapter_quotas(&sizes, keep - sel.len())) {
        let picks = best_per_stretch(photos, scene, members, quota, &sel);
        sel.extend(picks);
    }
    sel.sort_unstable();
    (sel, vec![])
}

/// Asset ids of photos that sit alone in their stretch (consecutive
/// photos of one day and owner-named place, as [`flow_pages`] cuts pages)
/// and show no one: such a photo would get a full page of its own.
fn lone_scenery(photos: &[BookPhoto]) -> Vec<String> {
    let key = |p: &BookPhoto| (p.day, p.area.clone());
    let mut out = vec![];
    let mut start = 0;
    while start < photos.len() {
        let end = (start..photos.len())
            .find(|&i| key(&photos[i]) != key(&photos[start]))
            .unwrap_or(photos.len());
        let p = &photos[start];
        let owners_page = p.page_group;
        if end - start == 1 && !p.keep && !owners_page && !p.is_of_people() && p.day.is_some() {
            out.push(p.asset_id.clone());
        }
        start = end;
    }
    out
}

/// An owner-kept photo with no one in it, alone in its stretch (a plate
/// of laksa between two sights), joins the next stretch of its day (else
/// the one before): it keeps its caption on a shared page rather than
/// taking a full page of its own.
fn fold_lone_kept(photos: &mut [BookPhoto]) {
    // Stretches as (start, end) runs of one (day, area).
    let mut runs: Vec<(usize, usize)> = vec![];
    for run in photos.chunk_by(|a, b| (a.day, &a.area) == (b.day, &b.area)) {
        let start = runs.last().map_or(0, |&(_, end)| end);
        runs.push((start, start + run.len()));
    }
    for (r, &(start, end)) in runs.iter().enumerate() {
        let p = &photos[start];
        // A page the owner gave this photo alone (`--own-page`) stays its own.
        if end - start != 1 || !p.keep || p.is_of_people() || p.day.is_none() || p.page_group {
            continue;
        }
        let day = p.day;
        // Only a neighbouring stretch of the same day: pages break on every
        // change of (day, area) in photo order, so taking the area of a
        // stretch further off would leave this photo alone all the same.
        let near = || {
            runs.get(r + 1)
                .into_iter()
                .chain(r.checked_sub(1).map(|i| &runs[i]))
                // An owner's page group takes no one it wasn't given.
                .filter(|&&(a, _)| photos[a].day == day && !photos[a].page_group)
        };
        // The next stretch, else the one before; one with two or more
        // photos first (a lone one may become the day's divider and leave
        // this one alone again).
        let target = near()
            .find(|&&(a, b)| b - a >= 2)
            .or_else(|| near().next())
            .map(|&(a, _)| photos[a].area.clone());
        if let Some(area) = target {
            photos[start].area = area;
        }
    }
}

/// Same caption text at most this many times in one day: an owner who
/// captioned a whole series ("Our door decorations") wants the series
/// named, not printed in full.
const MAX_SAME_CAPTION_PER_DAY: usize = 2;

/// The kept photos ([`BookPhoto::keep`]) worth printing: within one scene,
/// photos sharing a caption are one moment told once (the best-scored
/// stays), and one caption text appears at most
/// [`MAX_SAME_CAPTION_PER_DAY`] times a day. Different captions in one
/// scene are different stories and all stay.
fn distinct_curated(photos: &[BookPhoto], scene: &[usize]) -> Vec<usize> {
    let text = |i: usize| {
        photos[i]
            .caption
            .as_deref()
            .map(|c| c.trim().to_lowercase())
            .unwrap_or_default()
    };
    let mut best: HashMap<(usize, String), usize> = HashMap::new();
    // Pins always stay: only captioned photos are told once per moment.
    for i in (0..photos.len()).filter(|&i| photos[i].keep && !photos[i].pinned) {
        best.entry((scene[i], text(i)))
            .and_modify(|b| {
                if photos[i].score > photos[*b].score {
                    *b = i;
                }
            })
            .or_insert(i);
    }
    let mut kept: Vec<usize> = best.into_values().collect();
    // Per day and caption: the best few, by score.
    kept.sort_by(|&a, &b| photos[b].score.total_cmp(&photos[a].score));
    let mut seen: HashMap<(Option<u32>, String), usize> = HashMap::new();
    kept.retain(|&i| {
        let t = text(i);
        if t.is_empty() {
            return true;
        }
        let n = seen.entry((photos[i].day, t)).or_default();
        *n += 1;
        *n <= MAX_SAME_CAPTION_PER_DAY
    });
    kept.extend((0..photos.len()).filter(|&i| photos[i].pinned));
    kept.sort_unstable();
    kept
}

/// How far apart two shots may be and still be one scene when their
/// embeddings say they show the same scene (a bench sitting re-shot over a
/// few minutes).
const SCENE_EMBED_SECS: i64 = 300;

/// A scene id per photo (`photos` in story order): a photo joins the scene
/// of any earlier photo it [`BookPhoto::repeats`] within
/// [`SAME_MOMENT_SECS`] (the plan's same-moment window), else opens a new one. Linking
/// through neighbours keeps a whole posing session together even when its
/// first and last frames no longer look alike.
fn scenes(photos: &[BookPhoto]) -> Vec<usize> {
    let mut scene: Vec<usize> = Vec::with_capacity(photos.len());
    let mut next = 0;
    for (i, p) in photos.iter().enumerate() {
        let within = |j: usize, secs: i64| {
            photos[j]
                .taken_at
                .zip(p.taken_at)
                .is_some_and(|(a, b)| (b - a).abs() <= secs)
        };
        let linked = (0..i)
            .rev()
            .take_while(|&j| within(j, SCENE_EMBED_SECS))
            .find(|&j| {
                (within(j, SAME_MOMENT_SECS) && photos[j].repeats(p))
                    || triptape_core::embedding::same_scene(
                        photos[j].embedding.as_ref(),
                        p.embedding.as_ref(),
                    )
            });
        scene.push(linked.map_or_else(
            || {
                next += 1;
                next - 1
            },
            |j| scene[j],
        ));
    }
    scene
}

/// Share `slots` among chapters of the given `sizes` (`slots` must not
/// exceed their sum). Every chapter gets one slot first when there are
/// enough, so no chapter vanishes from the book; the rest go in proportion
/// to what each chapter has left (largest remainder, earlier chapter on
/// ties). With fewer slots than chapters the largest chapters win.
fn chapter_quotas(sizes: &[usize], slots: usize) -> Vec<usize> {
    let floor = usize::from(slots >= sizes.len());
    let mut quotas: Vec<usize> = sizes.iter().map(|&n| floor.min(n)).collect();
    let room: Vec<usize> = sizes.iter().zip(&quotas).map(|(n, q)| n - q).collect();
    let room_total: usize = room.iter().sum();
    let left = slots.saturating_sub(quotas.iter().sum());
    if room_total == 0 || left == 0 {
        return quotas;
    }
    let mut given = 0;
    for (q, r) in quotas.iter_mut().zip(&room) {
        let share = left * r / room_total;
        *q += share;
        given += share;
    }
    // Hand out what flooring left over, biggest fractional part first.
    let mut by_remainder: Vec<usize> = (0..sizes.len()).collect();
    by_remainder.sort_by_key(|&i| (std::cmp::Reverse(left * room[i] % room_total), i));
    for i in by_remainder {
        if given == left {
            break;
        }
        if quotas[i] < sizes[i] {
            quotas[i] += 1;
            given += 1;
        }
    }
    quotas
}

/// The best-scored of `cands` by `score`; the earlier candidate wins a
/// tie. `None` when there are no candidates.
fn best_scored(
    cands: impl IntoIterator<Item = usize>,
    score: impl Fn(usize) -> f32,
) -> Option<usize> {
    cands
        .into_iter()
        .reduce(|best, i| if score(i) > score(best) { i } else { best })
}

/// The best-scored photo of each of `quota` equal stretches of `members`
/// (indices into `photos`, in story order); the earlier photo wins a tie.
/// A photo whose [`scenes`] id matches one in `chosen` (picked for the
/// book already) or picked here waits, as in [`triptape_core::pick::pick_top`]: the stretch
/// takes its best photo that is a new scene, or the same view with other
/// people. A stretch with only repeats leaves its slot to the chapter's
/// best remaining new scene, and repeats fill only what nothing else can,
/// so the quota is always met.
fn best_per_stretch(
    photos: &[BookPhoto],
    scene: &[usize],
    members: &[usize],
    quota: usize,
    chosen: &[usize],
) -> Vec<usize> {
    let n = members.len();
    if quota >= n {
        return members.to_vec();
    }
    let mut seen: Vec<usize> = chosen.to_vec();
    let fresh_scene = |seen: &[usize], i: usize| !seen.iter().any(|&p| scene[p] == scene[i]);
    let mut picked: Vec<usize> = vec![];
    for j in 0..quota {
        let stretch = &members[j * n / quota..(j + 1) * n / quota];
        let fresh = stretch.iter().copied().filter(|&i| fresh_scene(&seen, i));
        if let Some(best) = best_scored(fresh, |i| photos[i].score) {
            picked.push(best);
            seen.push(best);
        }
    }
    // Leftover slots go to the rest of the chapter as `pick_top` ranks it:
    // new scenes first, then the waiting repeats, best first.
    let mut waiting: Vec<usize> = members
        .iter()
        .copied()
        .filter(|i| !picked.contains(i))
        .collect();
    waiting.sort_by(|&a, &b| photos[b].score.total_cmp(&photos[a].score).then(a.cmp(&b)));
    while picked.len() < quota {
        let at = waiting
            .iter()
            .position(|&i| fresh_scene(&seen, i))
            .unwrap_or(0);
        let next = waiting.remove(at);
        seen.push(next);
        picked.push(next);
    }
    picked.sort_unstable();
    picked
}

/// Simulate the renderer's page assignment and return asset ids whose
/// dpi verdict blocks at their assigned slot (hero/spread/caption/grid/divider).
fn planned_blocked(
    photos: &[BookPhoto],
    preset: BookPreset,
    cap: usize,
    has_map: bool,
) -> Vec<String> {
    if photos.is_empty() {
        return vec![];
    }
    let flow = book_flow(photos, has_map, cap);
    let order = consumption_order(photos, &flow, preset);
    let mut blocked = vec![];
    let mut cursor = 0usize;
    for fp in &flow {
        match fp.kind {
            PageKind::CoverProxy => {}
            PageKind::Hero | PageKind::PhotoCaption | PageKind::Grid | PageKind::Divider => {
                let slot: Vec<&BookPhoto> = order[cursor..(cursor + fp.take).min(order.len())]
                    .iter()
                    .map(|&oi| &photos[oi])
                    .collect();
                cursor += slot.len();
                for (p, (sw, sh)) in slot.iter().zip(slot_sizes(preset, fp.kind, &slot)) {
                    if dpi_verdict(p.width, p.height, sw, sh) == DpiVerdict::Block {
                        blocked.push(p.asset_id.clone());
                    }
                }
            }
            PageKind::SpreadLeft => {
                let p = &photos[*order.get(1).unwrap_or(&0)];
                let (sw, sh) = slot_inches(preset, fp.kind, 1);
                if dpi_verdict(p.width, p.height, sw, sh) == DpiVerdict::Block {
                    blocked.push(p.asset_id.clone());
                }
                cursor += 1;
            }
            _ => {}
        }
    }
    blocked
}

/// Render the full book pack.
pub fn render_book(
    photos_in: &[BookPhoto],
    route_card: Option<&Path>,
    chapters: &[String],
    out_dir: &Path,
    opts: &BookOptions,
) -> Result<BookOutput, crate::MediaError> {
    let (tw, th) = opts.preset.trim_px();
    // Auto-select for the page cap: a big trip flows past the cap even at
    // 4 photos/page, so sample evenly across story order instead of
    // blocking the export (PLAN Sec 11: preflight blocks, curation selects).
    let cap = opts
        .preset
        .page_cap(opts.max_pages)
        .map_err(crate::MediaError::InvalidArg)?;
    std::fs::create_dir_all(out_dir).map_err(|e| crate::MediaError::Io(e.to_string()))?;
    let mut fallbacks: Vec<String> = vec![];
    // Drop photos that cannot reach 200 dpi even in the smallest slot
    // (screenshots, tiny renders): preflight would block the whole pack on
    // them, and the plan's fix for that slot is "replace or remove".
    let (min_slot_w, min_slot_h) = match opts.preset {
        BookPreset::P1 => (5.5, 5.5),
        BookPreset::P2 => (5.0, 3.8),
    };
    let viable: Vec<BookPhoto> = photos_in
        .iter()
        .filter(|p| {
            // User-curated photos are never dropped silently: they
            // stay in and surface as a blocking preflight issue instead.
            let ok = p.keep
                || (p.width > 0
                    && p.height > 0
                    && dpi_verdict(p.width, p.height, min_slot_w, min_slot_h) != DpiVerdict::Block);
            if !ok {
                fallbacks.push(format!(
                    "book: {} excluded (below 200 dpi even in the smallest slot)",
                    p.asset_id
                ));
            }
            ok
        })
        .cloned()
        .collect();
    // The owner's closing photo, resolved before the cap math so the page
    // count takes exactly this photo off the pages. Like every page, it
    // must print sharp enough: a blocked choice gives way to the automatic
    // pick, with a note, and stays on the pages.
    let (closing_w, closing_h) = slot_inches(opts.preset, PageKind::Closing, 1);
    let mut owner_closing = viable.iter().find(|p| p.closing).cloned();
    if let Some(p) = owner_closing
        .take_if(|p| dpi_verdict(p.width, p.height, closing_w, closing_h) == DpiVerdict::Block)
    {
        fallbacks.push(format!(
            "closing: {} is too small to print a full page; another photo closes the book",
            p.display_name()
        ));
    }
    let closing_id: Option<String> = owner_closing.as_ref().map(|c| c.asset_id.clone());
    // Cap selection + placement-aware prune, iterated: a photo that blocks
    // at its assigned slot (a mid-size photo can pass the smallest slot yet
    // block as hero) is excluded and the selection is redone from the
    // remaining viable photos, so unselected photos refill the gap instead
    // of the book shrinking. Each pass excludes at least one new photo, so
    // it terminates; the post-render preflight stays as guard.
    let mut excluded: HashSet<String> = HashSet::new();
    let mut cap_note: Option<(usize, usize)>;
    let mut cap_dropped: Vec<String>;
    let mut pruned: Vec<BookPhoto> = loop {
        let pool: Vec<BookPhoto> = viable
            .iter()
            .filter(|p| !excluded.contains(&p.asset_id) && on_pages(p, opts))
            .cloned()
            .collect();
        // Cap math runs on the survivors: dropping the only wide photo
        // removes the spread, which can let more photos fit.
        let keep = fitting_count(&pool, route_card.is_some(), cap, closing_id.as_deref());
        let sel: Vec<BookPhoto> = if keep < pool.len() {
            let (sel, dropped) = select_for_cap(&pool, keep);
            // Plan captions are auto-generated for every photo, so caption
            // overflow is reported in the export notes, never a block.
            // Replaced every pass: only the final selection's omissions are
            // reported, never a photo a later pass brought back in.
            cap_dropped = dropped.iter().map(|&i| pool[i].asset_id.clone()).collect();
            cap_note = Some((keep, pool.len()));
            sel.into_iter().map(|i| pool[i].clone()).collect()
        } else {
            cap_note = None;
            cap_dropped = vec![];
            pool
        };
        let blocked: Vec<String> = planned_blocked(&sel, opts.preset, cap, route_card.is_some());
        if blocked.is_empty() {
            break sel;
        }
        for id in blocked {
            fallbacks.push(format!(
                "book: {id} excluded (below 200 dpi at its assigned slot)"
            ));
            excluded.insert(id);
        }
    };
    if !cap_dropped.is_empty() {
        let ids: Vec<&str> = cap_dropped.iter().take(10).map(String::as_str).collect();
        fallbacks.push(format!(
            "book: {} captioned photos did not fit the {cap}-page cap (kept an even spread; first omitted: {})",
            cap_dropped.len(),
            ids.join(", ")
        ));
    }
    if let Some((keep, of)) = cap_note {
        fallbacks.push(format!(
            "book: selected {keep} of {of} photos (your captioned photos first, then the best-scored of each chapter) to fit the {cap}-page cap"
        ));
    }
    // The closing page's photo: the owner's choice (taken off the pages,
    // so it never prints twice), else a strong people shot of the last
    // days the pages leave out, so the last page is new, not a repeat.
    if let Some(c) = &owner_closing {
        pruned.retain(|p| p.asset_id != c.asset_id);
    }
    let owner_chose_closing = owner_closing.is_some();
    let mut closing_pick: Option<BookPhoto> = owner_closing.or_else(|| {
        let placed: HashSet<&str> = pruned.iter().map(|p| p.asset_id.as_str()).collect();
        let last_day = viable.iter().filter_map(|p| p.day).max();
        viable
            .iter()
            .filter(|p| {
                // Big, sharp faces that print well on a full page (the
                // framing search in `cover_rank` last: it is the costly one;
                // it sees only tagged faces until the detector runs below).
                !placed.contains(p.asset_id.as_str())
                    && p.tagged_people >= 3
                    && p.people >= 4
                    && p.face_size >= CLOSING_FACE
                    && p.day.zip(last_day).is_some_and(|(d, l)| d + 1 >= l)
                    && dpi_verdict(p.width, p.height, closing_w, closing_h) == DpiVerdict::Ok
                    && cover_rank(p) == 2
            })
            .max_by(|a, b| a.score.total_cmp(&b.score))
            .cloned()
    });
    // A photo alone in its stretch (its day or named place) would fill a
    // page by itself: worth it for people or the owner's caption, not for
    // a plate of food or a candy bucket.
    for id in &drop_lone_scenery(&mut pruned) {
        fallbacks.push(format!(
            "book: {id} left out (alone on its page, no people)"
        ));
    }
    // Faces first, so page layouts can keep them whole: the detector runs
    // once per photo here and the painters reuse it.
    let found: std::cell::RefCell<HashMap<String, Vec<FaceBox>>> = Default::default();
    if let Some(finder) = opts.faces.as_ref() {
        opts.progress.step("book", "finding faces", None);
        for p in &mut pruned {
            let src = load(p, &opts.ffmpeg_bin, &mut fallbacks);
            p.faces = (finder.0)(&src);
            found
                .borrow_mut()
                .insert(p.asset_id.clone(), p.faces.clone());
        }
        // The closing photo is off the pages: its faces too, so its full
        // page frames like any other.
        if let Some(c) = closing_pick.as_mut() {
            let src = load(c, &opts.ffmpeg_bin, &mut fallbacks);
            c.faces = (finder.0)(&src);
            found
                .borrow_mut()
                .insert(c.asset_id.clone(), c.faces.clone());
        }
        // An automatic pick whose detected faces a full page would cut
        // gives way to the closing page's in-book pick.
        if !owner_chose_closing && closing_pick.as_ref().is_some_and(|c| cover_rank(c) != 2) {
            closing_pick = None;
        }
    }
    let photos: &[BookPhoto] = &pruned;

    let flow = book_flow(photos, route_card.is_some(), cap);
    let kinds: Vec<PageKind> = flow.iter().map(|p| p.kind).collect();
    // Page-count caps depend only on the flow: block before any image work.
    let early: Vec<String> = preflight(opts.preset, cap, &kinds, &[])
        .into_iter()
        .filter(|i| i.level == "block")
        .map(|i| format!("{} ({} photos)", i.message, photos.len()))
        .collect();
    if !early.is_empty() {
        return Err(crate::MediaError::Preflight(early.join("; ")));
    }

    let mut placements: Vec<(usize, DpiVerdict)> = vec![];
    let mut jpegs: Vec<PathBuf> = vec![];
    let mut captions: HashMap<usize, Vec<String>> = HashMap::new();
    // Grid pages: the user's captions, each on its own photo's cell.
    let mut cell_captions: CellCaptions = HashMap::new();
    // Photo pages that open a day: their banner reads as a heading.
    let mut bannered: HashSet<usize> = HashSet::new();
    // When the last photo page's latest photo was taken, for the
    // chronology check.
    let mut last_time: Option<i64> = None;
    // Consumption order: hero, then the spread wide photo, then the rest
    // in story order, each page's photos picked for sharpness — every photo
    // placed exactly once, exactly as `planned_blocked` simulated.
    let order = consumption_order(photos, &flow, opts.preset);
    let fit = Fit {
        ffmpeg: &opts.ffmpeg_bin,
        faces: opts.faces.as_ref(),
        found,
    };
    // The book's first pages open the trip: the cover shows other photos,
    // so turning the cover never shows the same shot again straight away.
    let opening: Vec<usize> = order.iter().take(COVER_SKIPS_FIRST).copied().collect();
    let back_cover_jpg = out_dir.join("back-cover.jpg");
    let cover_jpg = out_dir.join("cover.jpg");
    let (cover_tiles, cover) = match &opts.front_cover_image {
        // The owner's picture on the front.
        Some(front) => (
            vec![],
            image_cover(front, (tw, th), &cover_jpg, &mut fallbacks),
        ),
        None => {
            let tiles = cover_picks(photos, &opening);
            let cover = draw_cover_collage(
                photos,
                &tiles,
                (tw, th),
                opts,
                &fit,
                &cover_jpg,
                &mut fallbacks,
            );
            (tiles, cover)
        }
    };
    let back_cover = if opts.front_cover_image.is_some() && opts.back_cover_image.is_none() {
        // Under the owner's front cover and no back of their own, the back
        // shows the places themselves, without the family: the owner's
        // cover pins first (chosen sights, printed or not), then automatic
        // picks from shots the pages do not print.
        let printed: HashSet<&str> = photos.iter().map(|p| p.asset_id.as_str()).collect();
        let unprinted: Vec<&BookPhoto> = viable
            .iter()
            .filter(|p| p.cover_focus.is_some() || !printed.contains(p.asset_id.as_str()))
            .collect();
        let sights = sight_picks(&unprinted);
        draw_sights_mosaic(
            &unprinted,
            &sights,
            (tw, th),
            &fit,
            &back_cover_jpg,
            &mut fallbacks,
        )
    } else {
        make_back_cover(opts, (tw, th), &back_cover_jpg, &mut fallbacks)
    };
    let covers = (cover.as_deref(), back_cover.as_deref());
    let mut cursor = 0usize;
    let mut page_no = 0usize;
    let mut announced = Announced::default();
    // The photos each page shows, for the manifest.
    let mut page_photos: Vec<Vec<&BookPhoto>> = vec![];

    let total_pages = flow
        .iter()
        .filter(|p| p.kind != PageKind::CoverProxy)
        .count();
    opts.progress.counted("book", "page", 0, total_pages);
    for fp in &flow {
        if fp.kind == PageKind::CoverProxy {
            continue;
        }
        opts.progress.check_cancelled()?;
        page_no += 1;
        let fname = format!("page_{page_no:03}.jpg");
        let mut shown: Vec<&BookPhoto> = vec![];
        let out = out_dir.join(&fname);
        match fp.kind {
            PageKind::Hero | PageKind::PhotoCaption | PageKind::Grid => {
                let slot: Vec<&BookPhoto> = order[cursor..(cursor + fp.take).min(order.len())]
                    .iter()
                    .map(|oi| &photos[*oi])
                    .collect();
                cursor += slot.len();
                shown.clone_from(&slot);
                for (p, (sw, sh)) in slot.iter().zip(slot_sizes(opts.preset, fp.kind, &slot)) {
                    placements.push((page_no, dpi_verdict(p.width, p.height, sw, sh)));
                }
                // Captions on grid cells, placed once the page is drawn
                // and its faces are known.
                let mut pending_cells: Vec<(usize, Rect, String)> = vec![];
                let mut cell_texts: Vec<CellCaption> = vec![];
                let mut page_caps = if fp.kind == PageKind::Grid {
                    let (page, cells) = grid_captions(&slot);
                    cell_texts = cells;
                    page
                } else {
                    page_caption(&slot, fp.kind)
                };
                // The hero opens its chapter: no divider precedes it. Any
                // other page that starts a day no title has named carries
                // that day's banner.
                let title = if fp.kind == PageKind::Hero {
                    opening_title(slot[0], chapters, &mut announced)
                } else {
                    day_banner(slot[0], chapters, &mut announced)
                };
                if title.is_some() {
                    bannered.insert(page_no);
                }
                // The places of this page's photos, in the order visited:
                // a label names what the page shows, not the whole day.
                let page_w = opts.preset.trim_inches().0;
                let title =
                    title.map(|t| one_line_heading(places_in_time_order(&t, &slot), &slot, page_w));
                page_caps = titled(title, page_caps);
                if !page_caps.is_empty() {
                    captions.entry(page_no).or_default().extend(page_caps);
                }
                let strip = strip_px(
                    captions.get(&page_no).map_or(&[][..], Vec::as_slice),
                    bannered.contains(&page_no),
                    opts.preset,
                    th,
                );
                // Grid photos stop above the page's caption strip: a day
                // banner never lies over a face in the bottom row.
                let cells = match fp.kind {
                    PageKind::Grid => sharp_cells(
                        &slot,
                        grid_cells(&slot, f64::from(tw), f64::from(th.saturating_sub(strip))),
                        f64::from(tw) / opts.preset.trim_inches().0,
                    ),
                    _ => vec![FULL_PAGE; slot.len()],
                };
                if fp.kind == PageKind::Grid {
                    // Cell rects as fractions of the page, from the layout
                    // the page is drawn with.
                    let (pw, ph) = (f64::from(tw), f64::from(th));
                    pending_cells = cell_texts
                        .drain(..)
                        .map(|(k, text)| {
                            let (x, y, w, h) = cells[k];
                            (k, (x / pw, y / ph, w / pw, h / ph), text)
                        })
                        .collect();
                }
                // The hero is the opening's best shot pulled to the front,
                // not its earliest, so it neither checks nor sets the time
                // the next page must follow (as with the spread).
                if fp.kind != PageKind::Hero {
                    fallbacks.extend(time_order_notes(page_no, &slot, &cells, &mut last_time));
                }
                match fp.kind {
                    // Caption pages are full-bleed too: the caption burn lays
                    // its strip over the photo's lower edge.
                    PageKind::Hero | PageKind::PhotoCaption => {
                        draw_fullbleed(slot[0], (tw, th), strip, &out, &fit, &mut fallbacks)?
                    }
                    _ => {
                        // The page strip is below the grid: only the cell
                        // captions need bands of their own.
                        let bars = caption_bars(&pending_cells, slot.len(), opts.preset, th);
                        draw_grid(&slot, &cells, (tw, th), &bars, &out, &fit, &mut fallbacks)?;
                        if !pending_cells.is_empty() {
                            cell_captions.insert(
                                page_no,
                                pending_cells
                                    .drain(..)
                                    // In its own band below the photo
                                    // (`caption_bars`): never over it.
                                    .map(|(_, rect, text)| (rect, text))
                                    .collect(),
                            );
                        }
                    }
                }
            }
            PageKind::SpreadLeft | PageKind::SpreadRight => {
                // The wide photo is order[1]; both halves show its sides.
                let p = &photos[*order
                    .get(1)
                    .unwrap_or(&0)
                    .min(&photos.len().saturating_sub(1))];
                shown.push(p);
                if fp.kind == PageKind::SpreadLeft {
                    // One photo across both pages: checked once.
                    fallbacks.extend(time_order_notes(
                        page_no,
                        &[p],
                        &[FULL_PAGE],
                        &mut last_time,
                    ));
                    let (sw, sh) = slot_inches(opts.preset, fp.kind, 1);
                    placements.push((page_no, dpi_verdict(p.width, p.height, sw, sh)));
                    // Title only a chapter this spread uses up: otherwise
                    // its divider comes later, beside its other photos.
                    let chapter_continues =
                        order[2..].iter().any(|&oi| photos[oi].chapter == p.chapter);
                    let title = if chapter_continues {
                        None
                    } else {
                        opening_title(p, chapters, &mut announced)
                    };
                    if title.is_some() {
                        bannered.insert(page_no);
                    }
                    captions
                        .entry(page_no)
                        .or_default()
                        .extend(titled(title, page_caption(&[p], PageKind::PhotoCaption)));
                    cursor += 1;
                }
                draw_half(
                    p,
                    fp.kind == PageKind::SpreadLeft,
                    tw,
                    th,
                    &out,
                    &fit,
                    &mut fallbacks,
                )?;
            }
            PageKind::Map => {
                let card = route_card.unwrap();
                draw_map_page(card, tw, th, &out)?;
                // The owner's own map carries its own title.
                if opts.front_cover_image.as_deref() != Some(card) {
                    captions
                        .entry(page_no)
                        .or_default()
                        .push(crate::titlecard::ROUTE_LABEL.to_string());
                }
            }
            PageKind::Divider => {
                // Consume the background photo: it belongs to this
                // day-marker page and is not repeated later.
                let bg = order.get(cursor).map_or(&photos[0], |&oi| &photos[oi]);
                shown.push(bg);
                cursor += fp.take;
                let (sw, sh) = slot_inches(opts.preset, fp.kind, fp.take);
                placements.push((page_no, dpi_verdict(bg.width, bg.height, sw, sh)));
                fallbacks.extend(time_order_notes(
                    page_no,
                    &[bg],
                    &[FULL_PAGE],
                    &mut last_time,
                ));
                let lines = divider_lines(bg, chapters, &mut announced);
                let strip = strip_px(&lines, true, opts.preset, th);
                // Full brightness: the title sits in the caption strip, which
                // carries its own dark backing.
                draw_fullbleed(bg, (tw, th), strip, &out, &fit, &mut fallbacks)?;
                if !lines.is_empty() {
                    captions.entry(page_no).or_default().extend(lines);
                }
            }
            PageKind::Closing => {
                // The owner's closing photo is off the pages: it still
                // prints when it was the only photo.
                if closing_pick.is_none() && photos.is_empty() {
                    draw_text_placeholder(fp.kind, tw, th, &out)?;
                } else {
                    // The closing photo repeats one from the book: make it the
                    // best of the last days with people in it, not whatever
                    // came last (an airline tray), and never one from the
                    // last few pages or the cover's tiles.
                    let recent: HashSet<&str> = order[..cursor.min(order.len())]
                        .iter()
                        .rev()
                        .take(CLOSING_SKIPS_LAST)
                        .chain(&cover_tiles)
                        .map(|&i| photos[i].asset_id.as_str())
                        .collect();
                    let last_day = photos.iter().filter_map(|p| p.day).max();
                    let printable = |p: &BookPhoto| {
                        dpi_verdict(p.width, p.height, closing_w, closing_h) != DpiVerdict::Block
                    };
                    let last = closing_pick
                        .as_ref()
                        .or_else(|| {
                            photos
                                .iter()
                                .filter(|p| {
                                    // A people photo that fills a page well: no
                                    // panorama, no group a square would cut.
                                    !recent.contains(&p.asset_id.as_str())
                                        && p.day.zip(last_day).is_some_and(|(d, l)| d + 2 >= l)
                                        && printable(p)
                                        && cover_rank(p) == 2
                                })
                                .max_by(|a, b| a.score.total_cmp(&b.score))
                        })
                        // Else the last photo sharp enough for a full page: one
                        // that only cleared a grid cell would fail preflight.
                        .or_else(|| photos.iter().rev().find(|p| printable(p)))
                        .unwrap_or_else(|| &photos[photos.len() - 1]);
                    shown.push(last);
                    placements.push((
                        page_no,
                        dpi_verdict(last.width, last.height, closing_w, closing_h),
                    ));
                    draw_fullbleed(last, (tw, th), 0, &out, &fit, &mut fallbacks)?;
                    captions
                        .entry(page_no)
                        .or_default()
                        .push("The End".to_string());
                }
            }
            PageKind::CoverProxy => draw_text_placeholder(fp.kind, tw, th, &out)?,
        }
        // Header-only read: confirms the written page geometry without a decode.
        let (w, h) = image::image_dimensions(&out)
            .map_err(|e| crate::MediaError::Io(format!("read {fname}: {e}")))?;
        if (w, h) != (tw, th) {
            return Err(crate::MediaError::Io(format!(
                "{fname} is {w}x{h}, want {tw}x{th}"
            )));
        }
        jpegs.push(out);
        page_photos.push(shown);
        opts.progress.counted("book", "page", page_no, total_pages);
    }

    let issues = preflight(opts.preset, cap, &kinds, &placements);
    let blocks: Vec<&PreflightIssue> = issues.iter().filter(|i| i.level == "block").collect();
    if !blocks.is_empty() {
        let msg = blocks
            .iter()
            .map(|b| b.message.clone())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(crate::MediaError::Preflight(msg));
    }
    // The new pages rendered and passed preflight: only now drop page JPEGs
    // left over from a larger previous book, so a failed re-export never
    // deletes the prior pack's pages up front.
    if let Ok(entries) = std::fs::read_dir(out_dir) {
        for e in entries.flatten() {
            let path = e.path();
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("page_") && name.ends_with(".jpg") && !jpegs.contains(&path) {
                std::fs::remove_file(&path)
                    .map_err(|err| crate::MediaError::Io(format!("remove stale {name}: {err}")))?;
            }
        }
    }
    for w in issues.iter().filter(|i| i.level == "warn") {
        fallbacks.push(format!("page {}: {}", w.page.unwrap_or(0), w.message));
    }

    let counted = count_pages(&kinds);
    let texts = PageTexts {
        titles: day_title_pages(&kinds, &bannered, &captions),
        lines: captions,
        cells: cell_captions,
    };
    opts.progress.check_cancelled()?;
    opts.progress.step("book", "proof and press PDFs", None);
    let proof = match write_typst_and_compile(&jpegs, &texts, opts, out_dir, false, covers) {
        Ok(p) => Some(p),
        Err(e) => {
            // Never leave a previous export's PDF beside the new pages.
            let _ = std::fs::remove_file(out_dir.join("proof.pdf"));
            fallbacks.push(format!("proof.pdf skipped: {e}"));
            None
        }
    };
    let press = match write_typst_and_compile(&jpegs, &texts, opts, out_dir, true, covers) {
        Ok(p) => Some(p),
        Err(e) => {
            // Never leave a previous export's PDF beside the new pages.
            let _ = std::fs::remove_file(out_dir.join("press.pdf"));
            fallbacks.push(format!("press.pdf skipped: {e}"));
            None
        }
    };

    // Vendors print the JPEGs, not the PDFs: burn captions into the page
    // JPEGs by rasterizing the matching proof pages. Runs after press.pdf so
    // neither PDF picks up a doubled caption.
    let captioned: Vec<usize> = {
        let mut v: Vec<usize> = texts
            .lines
            .iter()
            .filter(|(_, l)| !l.is_empty())
            .map(|(k, _)| *k)
            .chain(texts.cells.keys().copied())
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    opts.progress.check_cancelled()?;
    opts.progress.step("book", "burning captions", None);
    let captions_burned = if captioned.is_empty() {
        true
    } else if proof.is_some() {
        match burn_captions(&jpegs, &captioned, opts, out_dir, (tw, th)) {
            Ok(()) => true,
            Err(e) => {
                fallbacks.push(format!("captions not burned into page JPEGs: {e}"));
                false
            }
        }
    } else {
        fallbacks.push("captions not burned into page JPEGs: Typst unavailable".into());
        false
    };

    let guide_path = out_dir.join("GUIDE.txt");
    std::fs::write(
        &guide_path,
        guide_text(
            opts.preset,
            &opts.trip_name,
            counted,
            captions_burned,
            back_cover.is_some(),
        ),
    )
    .map_err(|e| crate::MediaError::Io(e.to_string()))?;
    let manifest = serde_json::json!({
        "version": 1,
        "preset": opts.preset.name(),
        "trim_px": {"w": tw, "h": th},
        "cover": cover.as_ref().map(|c| c.file_name().unwrap().to_string_lossy().into_owned()),
        "back_cover": back_cover.as_ref().map(|c| c.file_name().unwrap().to_string_lossy().into_owned()),
        "counted_pages": counted,
        "pages": jpegs.iter().enumerate().map(|(i, p)| serde_json::json!({
            "page": i + 1,
            "file": p.file_name().unwrap().to_string_lossy(),
            "kind": kinds[i + 1], // +1: kinds[0] is the uncounted cover-proxy
            // Which originals the page shows, to find one while reviewing.
            "photos": page_photos[i].iter().map(|b| serde_json::json!({
                "asset_id": b.asset_id,
                "file": b.path.file_name().map(|f| f.to_string_lossy().into_owned()),
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "preflight_warnings": fallbacks.clone(),
    });
    std::fs::write(
        out_dir.join("book.json"),
        serde_json::to_string_pretty(&manifest)?,
    )
    .map_err(|e| crate::MediaError::Io(e.to_string()))?;

    Ok(BookOutput {
        dir: out_dir.to_path_buf(),
        cover,
        back_cover,
        jpeg_pages: jpegs,
        proof_pdf: proof,
        press_pdf: press,
        guide_path,
        counted_pages: counted,
        fallbacks,
    })
}

/// The asset ids printed on the pages of a rendered `book.json` (the
/// manifest [`render_book`] writes); `None` when the text is not one. The
/// closing page is left out: its automatic pick is a photo the pages do not
/// print, and approving it would put it on them (a closing photo that
/// repeats a page's is on that page anyway).
pub fn approved_ids(text: &str) -> Option<HashSet<String>> {
    let book: serde_json::Value = serde_json::from_str(text).ok()?;
    let ids: HashSet<String> = book
        .get("pages")?
        .as_array()?
        .iter()
        .filter(|page| page.get("kind").and_then(serde_json::Value::as_str) != Some("closing"))
        .filter_map(|page| page.get("photos")?.as_array())
        .flatten()
        .filter_map(|photo| Some(photo.get("asset_id")?.as_str()?.to_string()))
        .collect();
    (!ids.is_empty()).then_some(ids)
}

/// Chapters and trip days whose title the book has shown so far.
#[derive(Default)]
struct Announced {
    chapters: HashSet<usize>,
    days: HashSet<u32>,
}

/// The title of the chapter `p` opens, once per chapter. Showing it also
/// names `p`'s day, so that day gets no second banner. A second chapter
/// the same day (a title page midway through Jeju) names only its place:
/// the day and date were already shown.
fn opening_title(p: &BookPhoto, chapters: &[String], announced: &mut Announced) -> Option<String> {
    let title = p
        .chapter
        .filter(|&c| announced.chapters.insert(c))
        .and_then(|c| chapters.get(c).cloned())?;
    let day_shown = p.day.is_some_and(|d| !announced.days.insert(d));
    match title.split_once(" — ") {
        Some((_, place)) if day_shown => Some(place.to_string()),
        _ => Some(title),
    }
}

/// The day banner for a photo page starting with `p`: its chapter's title
/// (`Day 4 · Tue, Oct 6 — Tokyo`) when `p` begins a day no title page or
/// earlier banner has named. Chapters are single days, so the title names
/// the day.
fn day_banner(p: &BookPhoto, chapters: &[String], announced: &mut Announced) -> Option<String> {
    let day = p.day?;
    if announced.days.contains(&day) {
        return None;
    }
    // `opening_title` records the day as announced.
    opening_title(p, chapters, announced)
}

/// Chronology check for one photo page: its photos in reading order (top
/// to bottom, then left to right) and its first photo after the previous
/// page's last must run forward in time. Returns one note per break, for
/// the render's fallback list; `last` carries the latest time to the
/// next page.
fn time_order_notes(
    page_no: usize,
    slot: &[&BookPhoto],
    cells: &[Rect],
    last: &mut Option<i64>,
) -> Vec<String> {
    let mut reading: Vec<(Rect, &BookPhoto)> =
        cells.iter().copied().zip(slot.iter().copied()).collect();
    reading.sort_by(|(a, _), (b, _)| {
        // Same row when the tops are within a tenth of the cell height.
        if (a.1 - b.1).abs() < a.3.min(b.3) * 0.1 {
            a.0.total_cmp(&b.0)
        } else {
            a.1.total_cmp(&b.1)
        }
    });
    let mut notes = vec![];
    let mut prev: Option<(i64, &BookPhoto)> = None;
    for (_, p) in &reading {
        let Some(t) = p.taken_at else { continue };
        if let Some((pt, q)) = prev {
            if t + SAME_MOMENT_SECS < pt {
                notes.push(format!(
                    "page {page_no}: out of time order: {} comes after the later {}",
                    p.display_name(),
                    q.display_name()
                ));
            }
        } else if let Some(l) = *last {
            if t + SAME_MOMENT_SECS < l {
                notes.push(format!(
                    "page {page_no}: out of time order: {} is earlier than the page before",
                    p.display_name()
                ));
            }
        }
        if prev.is_none_or(|(pt, _)| t > pt) {
            prev = Some((t, p));
        }
    }
    if let Some((t, _)) = prev {
        *last = Some(last.map_or(t, |l| l.max(t)));
    }
    notes
}

/// Lead `caps` with a chapter title. An auto dateline or place caption would
/// only repeat the title, so those drop while user prose stays.
fn titled(title: Option<String>, caps: Vec<String>) -> Vec<String> {
    let Some(title) = title else {
        return caps;
    };
    let rest = caps
        .into_iter()
        .filter(|c| !is_dateline(c) && !title.contains(c.as_str()));
    std::iter::once(title.clone()).chain(rest).collect()
}

/// A day title (`Day 5 · Wed, Oct 7 — Tsurugaoka Hachimangū`) with the
/// places of its page's photos in the order they were taken, the title's
/// own place among them: a day that began in Tokyo reads `— Tokyo ·
/// Tsurugaoka Hachimangū`. A title whose place no photo on the page has
/// stays as it is, unless the page opens on board: then the place comes
/// where the page first goes ashore (`— Ship · Port · Temple`).
fn places_in_time_order(title: &str, slot: &[&BookPhoto]) -> String {
    let Some((head, place)) = title.split_once(" — ") else {
        return title.to_string();
    };
    // The ship first, then the cities, then the sights, each in the order
    // visited: "Carnival Luminosa · Fukuoka · Nanzoin Temple", "Singapore ·
    // Jewel Changi Airport".
    let rank = |pl: &str| {
        let shots = || slot.iter().filter(|p| p.place.as_deref() == Some(pl));
        if shots().any(|p| p.aboard) {
            0
        } else if shots().any(|p| p.place_is_city) {
            1
        } else {
            2
        }
    };

    // The heading over `places` as ordered: none said twice, at most
    // [`MAX_HEADING_PLACES`].
    let heading = |mut places: Vec<&str>| {
        drop_contained(&mut places);
        places.truncate(MAX_HEADING_PLACES);
        format!("{head} — {}", places.join(" \u{b7} "))
    };
    let ranked = |mut places: Vec<&str>| {
        places.sort_by_key(|pl| rank(pl));
        heading(places)
    };

    let mut seen: Vec<&str> = vec![];
    // On board, the ship alone: no deck or lounge ("Lido Deck") after it.
    let mut aboard_named = false;
    for p in slot {
        if let Some(pl) = p.place.as_deref() {
            if p.aboard {
                if aboard_named {
                    continue;
                }
                aboard_named = true;
            }
            if !seen.contains(&pl) {
                seen.push(pl);
            }
        }
    }
    // A heading that already names several places ("Tokyo · Tsukiji
    // Outer Market") keeps those the page does not show (its city) first,
    // then the page's places in the order visited ("· Togoshi Ginza"): the
    // durian at Marina Bay Sands before the Flower Dome after it.
    let parts: Vec<&str> = place.split(" \u{b7} ").collect();
    if !seen.contains(&place) && parts.len() > 1 && parts.iter().any(|p| seen.contains(p)) {
        let mut out: Vec<&str> = parts
            .iter()
            .copied()
            .filter(|p| !seen.contains(p))
            .collect();
        out.extend(seen.iter().copied());
        // The ship still leads: a sea-day title ("Carnival Luminosa ·
        // Fukuoka") over a page shot on board stays ship first.
        out.sort_by_key(|pl| rank(pl) != 0);
        return heading(out);
    }
    if !seen.contains(&place) {
        if !slot.first().is_some_and(|p| p.aboard) {
            // The page shows other places than the day's: the heading names
            // what the page shows (a mall, not the restaurant). No placed
            // photo: the day and date alone, never a place the page does
            // not show.
            if seen.is_empty() {
                return head.to_string();
            }
            return ranked(seen);
        }
        // After the places seen before the first placed photo ashore
        // (first when the page never goes ashore).
        let at = slot
            .iter()
            .position(|p| p.place.is_some() && !p.aboard)
            .map_or(0, |k| {
                let before = &slot[..k];
                seen.iter()
                    .filter(|&&s| before.iter().any(|p| p.place.as_deref() == Some(s)))
                    .count()
            });
        seen.insert(at, place);
    }
    ranked(seen)
}

/// Drop each place already inside another listed place: "Carnival Luminosa
/// Embarkation · Carnival Luminosa" says the ship twice.
fn drop_contained(places: &mut Vec<&str>) {
    let listed = places.clone();
    places.retain(|p| !listed.iter().any(|q| q != p && q.contains(p)));
}

/// Most places a day heading names.
const MAX_HEADING_PLACES: usize = 4;

/// A day heading on one line of its banner: when the places run onto a
/// second line, those no photo on the page shows (the day's city, named
/// for context) are dropped first, then the page's last places until it
/// fits. The first place always stays.
fn one_line_heading(heading: String, slot: &[&BookPhoto], page_w: f64) -> String {
    if heading_fits_one_line(&heading, page_w) {
        return heading;
    }
    let Some((head, places)) = heading.split_once(" — ") else {
        return heading;
    };
    let listed: Vec<&str> = places.split(" \u{b7} ").collect();
    let mut kept: Vec<&str> = listed
        .iter()
        .copied()
        .filter(|pl| slot.iter().any(|p| p.place.as_deref() == Some(*pl)))
        .collect();
    if kept.is_empty() {
        kept = listed;
    }
    // Still too long with the page's own places: the first ones visited,
    // as many as fit ("Independence Palace", the museum named on its own
    // page after).
    let line = |kept: &[&str]| format!("{head} — {}", kept.join(" \u{b7} "));
    while kept.len() > 1 && !heading_fits_one_line(&line(&kept), page_w) {
        kept.pop();
    }
    line(&kept)
}

/// Whether a day heading fits one line of the banner (24 pt, 14 pt inset).
fn heading_fits_one_line(text: &str, page_w: f64) -> bool {
    fits_one_line(text, 24.0, 14.0, page_w)
}

/// Whether `text` at `size` pt fits one line of a strip with `inset` pt
/// of padding each side on a `page_w`-inch page.
fn fits_one_line(text: &str, size: f64, inset: f64, page_w: f64) -> bool {
    text_width_em(text) * size <= page_w * 72.0 - 2.0 * inset
}

/// Advance widths (thousandths of an em) of printable ASCII, `' '..='~'`,
/// in Avenir Next as the burn sets it, measured with Typst.
const ASCII_ADVANCE: [u16; 95] = [
    250, 328, 405, 556, 580, 833, 704, 260, 300, 300, 444, 666, 260, 320, 260,
    370, // ' '..='/'
    580, 580, 580, 580, 580, 580, 580, 580, 580, 580, 300, 300, 666, 666, 666,
    482, // '0'..='?'
    800, 700, 636, 720, 757, 592, 562, 779, 718, 260, 492, 628, 510, 886, 764,
    850, // '@'..='O'
    580, 843, 599, 564, 570, 710, 623, 972, 649, 602, 572, 300, 370, 300, 666,
    500, // 'P'..='_'
    240, 534, 637, 500, 637, 572, 295, 632, 583, 250, 251, 510, 252, 883, 581,
    611, // '`'..='o'
    635, 635, 360, 444, 317, 581, 488, 746, 484, 488, 442, 300, 222, 300, 666, // 'p'..='~'
];

/// Width of `text` in ems as the burn sets it: each glyph's own advance
/// (an "M" is three times an "i"), so a heading of capitals and m's that
/// wraps is told from one as long that fits. Without kerning it reads a
/// little wide, never narrow.
fn text_width_em(text: &str) -> f64 {
    let thousandths: u32 = text
        .chars()
        .map(|c| match c {
            ' '..='~' => u32::from(ASCII_ADVANCE[c as usize - ' ' as usize]),
            '\u{b7}' => 260,
            '\u{2014}' => 600,
            '\u{2013}' => 400,
            '\u{2018}' | '\u{2019}' => 222,
            '\u{201c}' | '\u{201d}' => 387,
            '\u{2026}' => 1000,
            // Japanese and Korean script, and full-width forms: a full em.
            '\u{2e80}'..='\u{9fff}'
            | '\u{ac00}'..='\u{d7af}'
            | '\u{f900}'..='\u{faff}'
            | '\u{ff00}'..='\u{ffef}' => 1000,
            // Accented capitals ("Ō", "Æ") as wide as the widest capitals.
            c if c.is_uppercase() => 850,
            // Accented lowercase and the rest: as wide as the widest
            // lowercase ("ō" 0.61 em).
            _ => 620,
        })
        .sum();
    f64::from(thousandths) / 1000.0
}

/// Pages whose strip prints large: a divider or bannered page whose
/// heading starts a new day ("Day 21 · …"). A mid-day divider (a new place
/// on the same day) keeps the caption size, so large type always means a new
/// day. A book without day headings keeps every divider large.
fn day_title_pages(
    kinds: &[PageKind],
    bannered: &HashSet<usize>,
    captions: &HashMap<usize, Vec<String>>,
) -> HashSet<usize> {
    // kinds[0] is the uncounted cover-proxy, so kinds[n] is page n.
    let headed: Vec<usize> = (1..kinds.len())
        .filter(|&n| kinds[n] == PageKind::Divider || bannered.contains(&n))
        .collect();
    let is_day = |n: &usize| {
        captions
            .get(n)
            .and_then(|l| l.first())
            .is_some_and(|h| h.starts_with("Day "))
    };
    if headed.iter().any(is_day) {
        headed.into_iter().filter(is_day).collect()
    } else {
        headed.into_iter().collect()
    }
}

/// Caption lines for a divider page: its background photo's chapter title
/// plus that photo's own caption, since the photo is consumed here.
fn divider_lines(bg: &BookPhoto, chapters: &[String], announced: &mut Announced) -> Vec<String> {
    // A captioned background keeps its place too, as on photo pages.
    titled(
        opening_title(bg, chapters, announced).map(|t| places_in_time_order(&t, &[bg])),
        page_caption(&[bg], PageKind::PhotoCaption),
    )
}

/// A caption set on one grid cell: the slot index of its photo and its text.
type CellCaption = (usize, String);

/// Per page number: captions on grid photos, each with its photo's cell
/// `(x, y, w, h)` as fractions of the page.
type CellCaptions = HashMap<usize, Vec<(Rect, String)>>;

/// Everything burned over the page JPEGs, keyed by page number.
struct PageTexts {
    /// The bottom strip's lines.
    lines: HashMap<usize, Vec<String>>,
    /// Captions on individual grid photos.
    cells: CellCaptions,
    /// Chapter title (divider) pages, which get a larger strip.
    titles: HashSet<usize>,
}

/// Captions for a grid page: the user's own words go on the photo they
/// describe (a lounge caption must not sit under the plane photos beside
/// it), and the automatic place/date captions of the rest collapse into
/// the page line as [`page_caption`] does.
fn grid_captions(slot: &[&BookPhoto]) -> (Vec<String>, Vec<CellCaption>) {
    let mut cells = vec![];
    let mut lines: Vec<String> = vec![];
    // How many whole-page captions lead the line so far: a second one
    // follows the first, in story order.
    let mut leading = 0usize;
    for (k, p) in slot.iter().enumerate() {
        match caption_of(p).into_iter().next().filter(|_| p.own_caption()) {
            // Words the owner said speak for the whole page lead its
            // bottom line, before the places.
            Some(c) if p.caption_for_page => {
                lines.insert(leading, c);
                leading += 1;
                lines.extend(p.place.clone());
            }
            // The user's words go on the photo; its place still joins the
            // page's location strip (the lounge, the ramen hall). The same
            // words on several photos of one page print once.
            Some(c) => {
                if !cells
                    .iter()
                    .any(|(_, t): &(usize, String)| t.eq_ignore_ascii_case(&c))
                {
                    cells.push((k, c));
                }
                lines.extend(p.place.clone());
            }
            None => lines.extend(caption_of(p)),
        }
    }
    // A cell caption that only names a place the strip already names
    // ("Victoria Peak" over "Victoria Peak") says it twice.
    let strip = collapse_lines(lines, PageKind::Grid);
    cells.retain(|(_, t)| {
        !strip.iter().any(|l| {
            l.split(" · ")
                .any(|part| part.trim().eq_ignore_ascii_case(t.trim()))
        })
    });
    (strip, cells)
}

/// Caption lines for one photo page. Single-photo pages keep the photo's
/// full caption. Grid pages collapse to ONE dateline (or day range) so a
/// page never repeats near-identical per-photo template text; distinct
/// user prose on a grid keeps at most the first two lines.
fn page_caption(slot: &[&BookPhoto], kind: PageKind) -> Vec<String> {
    let mut lines: Vec<String> = vec![];
    for p in slot {
        // A captioned photo's place follows the user's words, unless the
        // words already name it ("…treats in Singapore · Singapore").
        let mut words = caption_of(p);
        let place = p.place.clone().filter(|pl| {
            p.own_caption()
                && !words
                    .iter()
                    .any(|w| w.to_lowercase().contains(&pl.to_lowercase()))
        });
        if place.is_some() {
            // The owner's sentence ends before its place: "…for 8 years. ·
            // Paradox Singapore". Place names are capitalised; a sentence
            // has lowercase words.
            for w in &mut words {
                let sentence = w
                    .split_whitespace()
                    .filter(|t| t.starts_with(|ch: char| ch.is_lowercase()))
                    .count()
                    >= 3;
                if sentence && !w.ends_with(SENTENCE_END) {
                    w.push('.');
                }
            }
        }
        lines.extend(words.into_iter().chain(place));
    }
    collapse_lines(lines, kind)
}

/// What a finished sentence ends with: a caption ending so needs no full
/// stop, and the line after it starts a new sentence.
pub(crate) const SENTENCE_END: [char; 7] = ['.', '!', '?', '\u{2026}', ')', '"', '\u{201d}'];

/// [`page_caption`]'s rules over raw lines: duplicates drop, and on grids
/// a less specific place, then datelines, collapse.
fn collapse_lines(lines: Vec<String>, kind: PageKind) -> Vec<String> {
    let mut caps: Vec<String> = vec![];
    for c in lines {
        if !caps.contains(&c) {
            caps.push(c);
        }
    }
    if kind != PageKind::Grid || caps.len() <= 1 {
        return caps;
    }
    // "Tokyo" beside "Tokyo DisneySea" says nothing more: keep the most
    // specific place.
    let all = caps.clone();
    caps.retain(|c| !all.iter().any(|o| o != c && o.contains(c.as_str())));
    if caps.iter().all(|c| is_dateline(c)) {
        let dates: Vec<String> = caps.iter().map(|c| dateline_date(c)).collect();
        if dates.iter().all(|d| *d == dates[0]) {
            return vec![dates[0].clone()];
        }
        return vec![format!(
            "{} – {}",
            date_only(&caps[0]),
            date_only(caps.last().unwrap())
        )];
    }
    // Two lines; a third when all three still fit one line of the strip
    // (14 pt, 8 pt inset) on the narrowest page ("Ho Chi Minh Campaign
    // Museum · War Remnants Museum · Saigon Central Post Office"): the
    // page names what it shows.
    let three_fit =
        caps.len() > 2 && fits_one_line(&caps[..3].join(" \u{b7} "), 14.0, 8.0, NARROWEST_PAGE_IN);
    caps.truncate(if three_fit { 3 } else { 2 });
    caps
}

/// Width (in) of the narrowest page any preset prints: Travel Landscape.
const NARROWEST_PAGE_IN: f64 = 11.0;

fn is_dateline(c: &str) -> bool {
    split_dateline(c).is_some()
}

fn dateline_date(c: &str) -> String {
    split_dateline(c).map_or(c, |(date, _)| date).to_string()
}

fn date_only(c: &str) -> String {
    let d = dateline_date(c);
    match d.split_once(" · ") {
        Some((head, rest)) if head.starts_with("Day ") => rest.to_string(),
        _ => d,
    }
}

fn caption_of(p: &BookPhoto) -> Vec<String> {
    p.caption
        .as_ref()
        .map(|c| {
            if c.trim().is_empty() {
                vec![]
            } else {
                vec![c.trim().to_string()]
            }
        })
        .unwrap_or_default()
}

// -- page painters (photo geometry real; caption text burned in via Typst) -

/// The image at `path`, upright. The image crate ignores EXIF orientation
/// (the FFmpeg/sips decode path orients its output), so portrait JPEGs
/// would print sideways without this.
fn open_oriented(path: &Path) -> image::ImageResult<image::DynamicImage> {
    let img = image::open(path)?;
    Ok(
        match triptape_core::metadata::read_image_exif(path).orientation {
            Some(o) => {
                crate::ffmpeg::orient_image(img, triptape_core::metadata::orientation_transform(o))
            }
            None => img,
        },
    )
}

fn load(photo: &BookPhoto, ffmpeg_bin: &str, fallbacks: &mut Vec<String>) -> RgbImage {
    if let Some(ms) = photo.still_ms {
        return video_frame(ffmpeg_bin, &photo.path, ms).unwrap_or_else(|| {
            fallbacks.push(format!(
                "{} frame at {:.1}s unreadable; gray placeholder",
                photo.display_name(),
                f64::from(ms) / 1000.0
            ));
            solid(800, 600, Rgb([90, 90, 95]))
        });
    }
    match open_oriented(&photo.path) {
        Ok(d) => d.to_rgb8(),
        Err(e) => match decode_via_ffmpeg(ffmpeg_bin, &photo.path) {
            Some(img) => img,
            None => {
                fallbacks.push(format!(
                    "{} unreadable ({e}); gray placeholder",
                    photo.asset_id
                ));
                solid(800, 600, Rgb([90, 90, 95]))
            }
        },
    }
}

/// One frame of the video at `path`, `ms` milliseconds in, upright (FFmpeg
/// applies the video's rotation), piped back as PNG so no temp file is shared.
fn video_frame(ffmpeg_bin: &str, path: &std::path::Path, ms: u32) -> Option<RgbImage> {
    let out = triptape_core::process::output(
        Command::new(ffmpeg_bin)
            .args([
                "-v",
                "error",
                "-ss",
                &format!("{:.3}", f64::from(ms) / 1000.0),
                "-i",
            ])
            .arg(path)
            .args(["-frames:v", "1", "-f", "image2pipe", "-c:v", "png", "-"]),
    )
    .ok()
    .filter(|o| o.status.success())?;
    image::load_from_memory(&out.stdout)
        .ok()
        .map(|d| d.to_rgb8())
}

/// Decode a still the `image` crate cannot read (HEIC) through FFmpeg
/// (with the macOS `sips` fallback for tmap/Dolby Vision items).
fn decode_via_ffmpeg(ffmpeg_bin: &str, path: &std::path::Path) -> Option<RgbImage> {
    let png = crate::ffmpeg::decode_still_to_png(ffmpeg_bin, path).ok()?;
    image::open(&png.path).ok().map(|d| d.to_rgb8())
}

fn solid(w: u32, h: u32, c: Rgb<u8>) -> RgbImage {
    let mut img = RgbImage::new(w, h);
    for p in img.pixels_mut() {
        *p = c;
    }
    img
}

/// `p` decoded and cover-fitted into `tw`x`th`, cropped to keep its
/// faces (with room above for the head) in frame. When they cannot all
/// fit, the biggest stay and the render notes the photo.
fn fit_photo(p: &BookPhoto, tw: u32, th: u32, fit: &Fit, fb: &mut Vec<String>) -> RgbImage {
    fit_photo_focused(p, p.crop_focus, (tw, th), 0.0, true, fit, fb)
}

/// [`fit_photo`] centred on `focus` (fractions of the photo) when set,
/// else keeping faces above a `reserve` band (see [`face_window_clear`]).
fn fit_photo_focused(
    p: &BookPhoto,
    focus: Option<(f32, f32)>,
    (tw, th): (u32, u32),
    reserve: f64,
    whole_if_cut: bool,
    fit: &Fit,
    fb: &mut Vec<String>,
) -> RgbImage {
    let src = load(p, fit.ffmpeg, fb);
    // Cover tiles and spreads (`whole_if_cut` false) always fill: bars
    // in the cover mosaic look broken.
    if p.whole && whole_if_cut {
        // The owner wants all of it (the glass floor at the bottom edge).
        let strip_px = (reserve.clamp(0.0, 1.0) * f64::from(th)).round() as u32;
        return contain_on_page(&src, tw, th, strip_px);
    }
    let (sw, sh) = (
        f64::from(src.width().max(1)),
        f64::from(src.height().max(1)),
    );
    let (tw_f, th_f) = (f64::from(tw), f64::from(th));
    let (window, cut) = match focus {
        // The owner's focus wins: they saw what the detector missed.
        Some((fx, fy)) => (
            focus_window(sw, sh, tw_f, th_f, (f64::from(fx), f64::from(fy))),
            false,
        ),
        None => {
            // Only a photo left to the detector pays for a detection.
            let faces = match fit.faces {
                Some(f) => fit
                    .found
                    .borrow_mut()
                    .entry(p.asset_id.clone())
                    .or_insert_with(|| (f.0)(&src))
                    .clone(),
                None => vec![],
            };
            frame_photo(p, (sw, sh), (tw_f, th_f), &faces, reserve)
        }
    };
    // Shown whole only when that still fills most of the cell: a 4:3 photo
    // pillarboxed in a wide strip leaves black voids worse than a crop.
    let fills = {
        let (sa, ca) = (sw / sh, tw_f / th_f);
        (sa / ca).min(ca / sa)
    };
    if cut && whole_if_cut && fills >= WHOLE_MIN_FILL {
        // No crop of this cell keeps every face: show the whole photo in it
        // on the page's dark ground rather than slice someone in half.
        fb.push(format!(
            "crop: {} cannot keep every face in its cell; shown whole",
            p.display_name()
        ));
        let strip_px = (reserve.clamp(0.0, 1.0) * th_f).round() as u32;
        return contain_on_page(&src, tw, th, strip_px);
    } else if cut {
        fb.push(format!(
            "crop: {} cannot keep every face in its cell; the biggest stay",
            p.display_name()
        ));
    }
    crop_window(&src, window, tw, th)
}

/// The source window a cover-fit into `tw`x`th` keeps, centred on `focus`
/// (fractions of the photo) as far as the photo's edges allow.
fn focus_window(sw: f64, sh: f64, tw: f64, th: f64, focus: (f64, f64)) -> (f64, f64, f64, f64) {
    let scale = (tw / sw).max(th / sh);
    let (cw, ch) = ((tw / scale).min(sw), (th / scale).min(sh));
    let x = (focus.0.clamp(0.0, 1.0) * sw - cw / 2.0).clamp(0.0, (sw - cw).max(0.0));
    let y = (focus.1.clamp(0.0, 1.0) * sh - ch / 2.0).clamp(0.0, (sh - ch).max(0.0));
    (x, y, cw, ch)
}

/// `src`'s `window` (x, y, w, h in pixels) scaled to `tw`x`th`.
fn crop_window(src: &RgbImage, (x, y, w, h): (f64, f64, f64, f64), tw: u32, th: u32) -> RgbImage {
    let x = (x.round() as u32).min(src.width().saturating_sub(1));
    let y = (y.round() as u32).min(src.height().saturating_sub(1));
    let w = (w.round() as u32).clamp(1, src.width().saturating_sub(x).max(1));
    let h = (h.round() as u32).clamp(1, src.height().saturating_sub(y).max(1));
    let window = image::imageops::crop_imm(src, x, y, w, h).to_image();
    image::imageops::resize(&window, tw, th, image::imageops::FilterType::Triangle)
}

/// Share of a crop's spare height left above the faces (the rest goes
/// below): children in strollers sit below the adults' faces more often
/// than a mascot towers over them (a mascot gets an owner crop focus).
const ABOVE_SHARE: f64 = 0.35;

/// Room kept around a face, as fractions of its box: the detector boxes
/// the face, so the top of the head and hair sit well above it.
const HEAD_ABOVE: f32 = 0.6;
const FACE_SIDE: f32 = 0.2;
const FACE_BELOW: f32 = 0.15;

/// A named person's region in a photo (from the photo library's face
/// tags): a face, or a body whose top stands in for the head.
#[derive(Debug, Clone, PartialEq)]
pub struct KnownBox {
    pub b: FaceBox,
    pub body: bool,
}

impl From<&triptape_core::types::KnownFace> for KnownBox {
    fn from(k: &triptape_core::types::KnownFace) -> Self {
        KnownBox {
            b: FaceBox {
                x: k.x,
                y: k.y,
                w: k.w,
                h: k.h,
            },
            body: k.body,
        }
    }
}

/// What framing weighs a family member's face (with head room) at.
const FAMILY_FACE_WEIGHT: f64 = 10.0;
/// What framing weighs the head end of a family member's body at.
const FAMILY_BODY_WEIGHT: f64 = 6.0;
/// What a detected face no family region covers weighs, on a photo with
/// family tags: most likely a stranger.
const STRANGER_WEIGHT: f64 = 0.5;
/// A target only partly in a window counts this many times against it:
/// a sliced face is worse than one left out.
const PARTIAL_PENALTY: f64 = 1.5;
/// A target's head room (its padded box) fully in the window earns this
/// share of its weight on top: nice to have, never worth slicing a face.
const ROOM_BONUS: f64 = 0.3;
/// Candidate positions tried along each axis.
const FRAMING_STEPS: usize = 25;

/// A region framing tries to keep: the face itself (`core`), which must
/// not be sliced, and the face with head room (`room`), a bonus.
#[derive(Debug, Clone)]
struct Target {
    core: FaceBox,
    room: FaceBox,
    weight: f64,
    /// A tagged family member (not a stranger the detector found).
    family: bool,
}

/// The regions framing tries to keep, each with its weight: family faces
/// (with head room), the top quarter of a family body (its head), and the
/// detected faces no family region covers, lightly (strangers).
fn framing_targets(p: &BookPhoto, detected: &[FaceBox]) -> Vec<Target> {
    let pad = |b: FaceBox| FaceBox {
        x: b.x - FACE_SIDE * b.w,
        y: b.y - HEAD_ABOVE * b.h,
        w: b.w * (1.0 + 2.0 * FACE_SIDE),
        h: b.h * (1.0 + HEAD_ABOVE + FACE_BELOW),
    };
    let mut out: Vec<Target> = p
        .family
        .iter()
        .map(|k| {
            if k.body {
                let head = FaceBox {
                    h: k.b.h * 0.25,
                    ..k.b.clone()
                };
                Target {
                    room: head.clone(),
                    core: head,
                    weight: FAMILY_BODY_WEIGHT,
                    family: true,
                }
            } else {
                Target {
                    core: k.b.clone(),
                    room: pad(k.b.clone()),
                    weight: FAMILY_FACE_WEIGHT,
                    family: true,
                }
            }
        })
        .collect();
    let covered = |d: &FaceBox| {
        let (cx, cy) = (d.x + d.w / 2.0, d.y + d.h / 2.0);
        p.family.iter().any(|k| {
            let b = &k.b;
            cx >= b.x - b.w * 0.25
                && cx <= b.x + b.w * 1.25
                && cy >= b.y - b.h * 0.25
                && cy <= b.y + b.h * if k.body { 0.5 } else { 1.25 }
        })
    };
    out.extend(detected.iter().filter(|d| !covered(d)).map(|d| Target {
        core: d.clone(),
        room: pad(d.clone()),
        weight: STRANGER_WEIGHT,
        family: false,
    }));
    out
}

/// The `tw`x`th`-shaped source window of an `sw`x`sh` photo that best
/// frames `targets` (see [`framing_targets`]): each target fully inside
/// adds its weight, one only partly inside subtracts
/// [`PARTIAL_PENALTY`] times it, and nothing counts for one left out. The
/// bottom `reserve` of the window (a caption strip) counts as outside.
/// Also whether a family member is cut or left out.
fn framing_window(
    sw: f64,
    sh: f64,
    tw: f64,
    th: f64,
    targets: &[Target],
    reserve: f64,
) -> ((f64, f64, f64, f64), bool) {
    let scale = (tw / sw).max(th / sh);
    let (cw, ch) = ((tw / scale).min(sw), (th / scale).min(sh));
    let usable_h = ch * (1.0 - reserve.clamp(0.0, 0.5));
    let (slack_x, slack_y) = ((sw - cw).max(0.0), (sh - ch).max(0.0));
    // Candidate offsets along an axis: evenly across its slack, or the
    // middle when there is none to speak of.
    let offsets = |slack: f64| -> Vec<f64> {
        if slack > 0.5 {
            (0..FRAMING_STEPS)
                .map(|i| slack * i as f64 / (FRAMING_STEPS - 1) as f64)
                .collect()
        } else {
            vec![slack / 2.0]
        }
    };
    let fraction_in = |b: &FaceBox, x: f64, y: f64| {
        let (bx0, by0) = (f64::from(b.x) * sw, f64::from(b.y) * sh);
        let (bx1, by1) = (bx0 + f64::from(b.w) * sw, by0 + f64::from(b.h) * sh);
        let (ix0, iy0) = (bx0.max(x).max(0.0), by0.max(y).max(0.0));
        let (ix1, iy1) = (bx1.min(x + cw).min(sw), by1.min(y + usable_h).min(sh));
        let inside = (ix1 - ix0).max(0.0) * (iy1 - iy0).max(0.0);
        // Only the part of the box within the photo can be framed.
        let area = (bx1.min(sw) - bx0.max(0.0)).max(0.0) * (by1.min(sh) - by0.max(0.0)).max(0.0);
        if area <= 0.0 {
            1.0
        } else {
            inside / area
        }
    };
    let mut best: Option<(f64, f64, f64, bool)> = None;
    let ys = offsets(slack_y);
    for x in offsets(slack_x) {
        for &y in &ys {
            let mut score = 0.0;
            let mut family_cut = false;
            for t in targets {
                let w = t.weight;
                let f = fraction_in(&t.core, x, y);
                if f >= 0.98 {
                    score += w;
                    if fraction_in(&t.room, x, y) >= 0.98 {
                        score += ROOM_BONUS * w;
                    }
                    continue;
                }
                if f > 0.02 {
                    score -= PARTIAL_PENALTY * w;
                }
                family_cut |= t.family;
            }
            // Ties: the window nearest the middle of the photo.
            let off = ((x - slack_x / 2.0).abs() + (y - slack_y / 2.0).abs()) / (sw + sh);
            let score = score - 0.01 * off;
            if best.is_none_or(|(s, ..)| score > s) {
                best = Some((score, x, y, family_cut));
            }
        }
    }
    let (_, x, y, cut) = best.unwrap_or((0.0, slack_x / 2.0, slack_y / 2.0, false));
    ((x, y, cw, ch), cut)
}

/// The window a photo is cropped to in a `tw`x`th` cell, and whether it
/// cuts someone who matters: by [`framing_window`] when the photo carries
/// family regions, else by the detected faces alone ([`face_window_clear`]).
fn frame_photo(
    p: &BookPhoto,
    (sw, sh): (f64, f64),
    (tw, th): (f64, f64),
    detected: &[FaceBox],
    reserve: f64,
) -> ((f64, f64, f64, f64), bool) {
    if !p.family.is_empty() {
        framing_window(sw, sh, tw, th, &framing_targets(p, detected), reserve)
    } else if !detected.is_empty() {
        face_window_clear(sw, sh, tw, th, detected, reserve)
    } else {
        (focus_window(sw, sh, tw, th, (0.5, 0.5)), false)
    }
}

/// Whether the photo's own pixel size can be framed into a `tw`x`th` cell
/// without cutting someone who matters.
fn frame_cuts(p: &BookPhoto, tw: f64, th: f64, reserve: f64) -> bool {
    p.width > 0
        && p.height > 0
        && frame_photo(
            p,
            (f64::from(p.width), f64::from(p.height)),
            (tw, th),
            &p.faces,
            reserve,
        )
        .1
}

/// The source window (x, y, w, h in pixels) a cover-fit of an `sw`x`sh`
/// photo into a `tw`x`th` cell keeps: as many faces as fit, biggest
/// first, each with its head room, centred between them; centred on the
/// photo when it has none. Faces stay above a band of `reserve` (a
/// fraction of the cell's height) at its bottom, where the page's caption
/// strip lies. Also whether a face was left out or cut.
fn face_window_clear(
    sw: f64,
    sh: f64,
    tw: f64,
    th: f64,
    faces: &[FaceBox],
    reserve: f64,
) -> ((f64, f64, f64, f64), bool) {
    let scale = (tw / sw).max(th / sh);
    let (cw, ch) = ((tw / scale).min(sw), (th / scale).min(sh));
    let below = reserve.clamp(0.0, 0.5) * ch;
    type Box4 = (f64, f64, f64, f64);
    // Each face as (with head room, bare), biggest first.
    let mut by_size: Vec<(Box4, Box4)> = faces
        .iter()
        .map(|f| {
            let (x, y, w, h) = (f.x, f.y, f.w, f.h);
            let padded = (
                f64::from(x - FACE_SIDE * w).max(0.0) * sw,
                f64::from(y - HEAD_ABOVE * h).max(0.0) * sh,
                f64::from(x + w + FACE_SIDE * w).min(1.0) * sw,
                (f64::from(y + h + FACE_BELOW * h) * sh + below).min(sh),
            );
            let bare = (
                f64::from(x) * sw,
                f64::from(y) * sh,
                f64::from(x + w).min(1.0) * sw,
                f64::from(y + h).min(1.0) * sh,
            );
            (padded, bare)
        })
        .collect();
    let area = |b: &Box4| (b.2 - b.0) * (b.3 - b.1);
    by_size.sort_by(|a, b| area(&b.1).total_cmp(&area(&a.1)));
    let union = |k: Option<Box4>, b: Box4| {
        k.map_or(b, |k| {
            (k.0.min(b.0), k.1.min(b.1), k.2.max(b.2), k.3.max(b.3))
        })
    };
    let fits = |u: Box4| u.2 - u.0 <= cw + 0.5 && u.3 - u.1 <= ch + 0.5;
    let mut keep: Option<Box4> = None;
    // The highest head room the kept faces want: the window rises toward
    // it as far as the faces allow.
    let mut head_top = f64::INFINITY;
    let mut cut = false;
    for (padded, bare) in by_size {
        // With its head room if that fits, else the face itself (a close-up
        // whose head room is taller than the cell).
        if fits(union(keep, padded)) {
            keep = Some(union(keep, padded));
        } else if fits(union(keep, bare)) {
            keep = Some(union(keep, bare));
        } else {
            cut = true;
            if keep.is_none() {
                // The biggest face alone overflows the cell: aim at it (and
                // its head room) rather than at a small background face.
                keep = Some(padded);
                break;
            }
            continue;
        }
        head_top = head_top.min(padded.1);
    }
    // Along each axis: centre on the kept faces, slid to hold them all.
    let place = |lo: f64, hi: f64, win: f64, full: f64| {
        let want = (lo + hi) / 2.0 - win / 2.0;
        let max_origin = (full - win).max(0.0);
        // The 0.5 px slack above (and rounding) can leave the faces a
        // touch wider than the window: then centre rather than panic in
        // `clamp` on an inverted range.
        let (from, to) = ((hi - win).max(0.0), lo.min(max_origin));
        let held = if from <= to {
            want.clamp(from, to)
        } else {
            want
        };
        held.clamp(0.0, max_origin)
    };
    let (x, y) = match keep {
        Some(k) => {
            // Vertically: as much head room as fits above the faces.
            let max_origin = (sh - ch).max(0.0);
            let y = if fits(k) {
                // Spare height goes mostly above the faces (a mascot's head,
                // a statue, a sign), less to the floor below them.
                let slack = (ch - (k.3 - k.1)).max(0.0);
                (k.1 - ABOVE_SHARE * slack)
                    .min(head_top)
                    .max(k.3 - ch)
                    .clamp(0.0, max_origin)
            } else {
                place(k.1, k.3, ch, sh)
            };
            (place(k.0, k.2, cw, sw), y)
        }
        None => ((sw - cw) / 2.0, (sh - ch) / 2.0),
    };
    ((x, y, cw, ch), cut)
}

fn draw_fullbleed(
    p: &BookPhoto,
    (tw, th): (u32, u32),
    strip_px: u32,
    out: &Path,
    ff: &Fit,
    fb: &mut Vec<String>,
) -> Result<(), crate::MediaError> {
    let reserve = f64::from(strip_px) / f64::from(th.max(1));
    // A wide group photo squared off for a full page would lose the
    // people at its sides: then the whole photo sits on the page instead,
    // above the strip.
    let cuts_faces = p.crop_focus.is_none() && frame_cuts(p, f64::from(tw), f64::from(th), reserve);
    if cuts_faces {
        let src = load(p, ff.ffmpeg, fb);
        return save_jpg(&contain_on_page(&src, tw, th, strip_px), out);
    }
    save_jpg(
        &fit_photo_focused(p, p.crop_focus, (tw, th), reserve, true, ff, fb),
        out,
    )
}

/// `src` whole, as large as fits in `tw`x`th` above a `strip_px` bottom
/// strip, centred on the book's dark ground.
fn contain_on_page(src: &RgbImage, tw: u32, th: u32, strip_px: u32) -> RgbImage {
    let avail_h = th.saturating_sub(strip_px).max(th / 2);
    let scale = (f64::from(tw) / f64::from(src.width().max(1)))
        .min(f64::from(avail_h) / f64::from(src.height().max(1)));
    let (w, h) = (
        ((f64::from(src.width()) * scale).round() as u32).clamp(1, tw),
        ((f64::from(src.height()) * scale).round() as u32).clamp(1, avail_h),
    );
    let fitted = image::imageops::resize(src, w, h, image::imageops::FilterType::Lanczos3);
    let mut page = solid(tw, th, Rgb([18, 20, 26]));
    image::imageops::replace(
        &mut page,
        &fitted,
        i64::from((tw - w) / 2),
        i64::from((avail_h - h) / 2),
    );
    page
}

fn draw_half(
    p: &BookPhoto,
    left: bool,
    tw: u32,
    th: u32,
    out: &Path,
    ff: &Fit,
    fb: &mut Vec<String>,
) -> Result<(), crate::MediaError> {
    // A spread's two halves are one photo: crop it, never letterbox it.
    let full = fit_photo_focused(p, p.crop_focus, (tw * 2, th), 0.0, false, ff, fb);
    let x = if left { 0 } else { tw };
    save_jpg(
        &image::imageops::crop_imm(&full, x, 0, tw, th).to_image(),
        out,
    )
}

/// A cell `(x, y, w, h)`.
type Rect = (f64, f64, f64, f64);

/// The whole page as one cell (any unit: the time check only compares).
const FULL_PAGE: Rect = (0.0, 0.0, 1.0, 1.0);

/// Row arrangements for `n` photos: up to 3 rows of 1–3 photos each.
fn row_splits(n: usize) -> Vec<Vec<usize>> {
    fn go(left: usize, rows: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
        if left == 0 {
            out.push(rows.clone());
            return;
        }
        if rows.len() == 3 {
            return;
        }
        for k in 1..=left.min(3) {
            rows.push(k);
            go(left - k, rows, out);
            rows.pop();
        }
    }
    let mut out = vec![];
    go(n, &mut vec![], &mut out);
    out
}

/// Cell width / height below which a cell is a sliver, and the share of
/// a photo's width below which a sliver guts it (a 4:3 shot in a third of
/// the page keeps a quarter of its width; a portrait in half a page keeps
/// two thirds and is fine).
const NARROW_CELL: f64 = 0.55;
/// What a grid layout pays for a sliver that guts its photo.
const NARROW_CELL_COST: f64 = 1.5;

/// Share of a cell a photo shown whole must still fill (else it is
/// cropped, keeping the biggest faces).
const WHOLE_MIN_FILL: f64 = 0.75;

/// How long after a chapter's first photo its divider may reach for a
/// background.
const DIVIDER_SECS: i64 = 20 * 60;

/// What a grid layout pays when the owner's featured photo is not in the
/// page's biggest cell: more than anything else a layout can cost.
const FEATURE_COST: f64 = 20.0;

/// How close in time a featured photo must be to its page's first photo to
/// lead the page.
const FEATURE_LEAD_SECS: i64 = 30 * 60;

/// What a never-cropped photo pays per unit of shape mismatch with its
/// cell (the bars it leaves).
const WHOLE_MISMATCH_COST: f64 = 10.0;

/// What a grid layout pays for a cell that cuts a detected face: more
/// than any amount of ordinary cropping.
const FACE_CUT_COST: f64 = 6.0;

/// Width / height from which a photo is a panorama: alone on a grid row
/// it spans the page at its own height.
const PANORAMA_ASPECT: f64 = 2.0;

/// The share of a cell a never-cropped photo fills: 1 when the two shapes
/// (width over height) match, less the more bars the cell leaves.
fn whole_fill(cell_aspect: f64, photo_aspect: f64) -> f64 {
    (cell_aspect / photo_aspect).min(photo_aspect / cell_aspect)
}

/// Cells `(x, y, w, h)` for `slot` on a `w`×`h` page with `gap` between
/// cells, one per photo in slot order. Rows of 1–3 photos fill the page;
/// within a row each photo's width follows its shape, and rows share the
/// height in proportion, so a photo is cropped only as much as the page
/// shape forces. Of every row split, the layout with the
/// least weighted cropping wins: crops cost more the more people a photo
/// holds (a group shot gets a full-width row instead of losing the faces
/// at its edges), trimming sky or table off a landscape costs less than
/// trimming its sides, and very unequal photo sizes cost a little. The
/// photos keep their slot order, read top to bottom and left to right,
/// so a page reads in time order.
fn grid_layout(slot: &[&BookPhoto], w: f64, h: f64, gap: f64) -> Vec<Rect> {
    const BALANCE: f64 = 0.3;
    // How much a photo's faces raise the cost of cropping or shrinking it.
    let people_weight = |p: &BookPhoto| 1.0 + 0.5 * f64::from(p.people.min(4));
    let n = slot.len();
    let aspect = |p: &BookPhoto| {
        if p.width > 0 && p.height > 0 {
            p.width as f64 / p.height as f64
        } else {
            1.0
        }
    };
    let mut best: Option<(f64, Vec<Rect>)> = None;
    // The owner laid this page out side by side: one row, full height
    // (three cabin doors, door to floor), however tall the cells.
    let side_by_side = n <= 3 && slot.iter().all(|p| p.row);
    let splits = if side_by_side {
        vec![vec![n]]
    } else {
        row_splits(n)
    };
    for rows in splits {
        // Photos keep their order (time order): only the rows vary.
        let avail_h = h - gap * (rows.len() + 1) as f64;
        let mut at = 0;
        // (photo, row, share of the row width); `natural` holds each
        // row's height before stretching to fill the page.
        let mut placed: Vec<(usize, usize, f64)> = vec![];
        let mut natural: Vec<f64> = vec![];
        // A panorama alone on its row keeps its own height (the full
        // page width, uncropped); the other rows share the rest.
        let mut fixed: Vec<bool> = vec![];
        for (r, &k) in rows.iter().enumerate() {
            let members = at..at + k;
            at += k;
            let sum: f64 = members.clone().map(|i| aspect(slot[i])).sum();
            let row_w = w - gap * (k + 1) as f64;
            natural.push(row_w / sum);
            fixed.push(k == 1 && sum >= PANORAMA_ASPECT);
            // Side by side, the owner's photos share the row evenly: a door
            // cropped out of a hallway panorama is as wide as the others.
            placed.extend(members.map(|i| {
                let share = if side_by_side {
                    1.0 / k as f64
                } else {
                    aspect(slot[i]) / sum
                };
                (i, r, share)
            }));
        }
        let total: f64 = natural.iter().sum();
        let held: f64 = natural
            .iter()
            .zip(&fixed)
            .filter(|(_, &f)| f)
            .map(|(n, _)| n)
            .sum();
        let free = total - held;
        // Panoramas keep their height only when the other rows can
        // take up the rest; else every row stretches alike.
        let hold = free > 0.0 && held < avail_h;
        let stretch = if hold {
            (avail_h - held) / free
        } else {
            avail_h / total
        };
        let row_h: Vec<f64> = natural
            .iter()
            .zip(&fixed)
            .map(|(&nh, &f)| if hold && f { nh } else { nh * stretch })
            .collect();
        let mut cells = vec![(0.0, 0.0, 0.0, 0.0); n];
        let mut cost = 0.0;
        let mut y = gap;
        let mut x = gap;
        let mut row = 0;
        for &(i, r, share) in &placed {
            if r != row {
                y += row_h[row] + gap;
                x = gap;
                row = r;
            }
            let row_w = w - gap * (rows[r] + 1) as f64;
            let cw = row_w * share;
            cells[i] = (x, y, cw, row_h[r]);
            x += cw + gap;
            let (a, c) = (aspect(slot[i]), cw / row_h[r]);
            let weight = people_weight(slot[i]);
            // A tall sliver (a third of the page wide, full height) leaves
            // no room around anyone: such layouts lose unless nothing else
            // fits.
            if c < NARROW_CELL && c / a < NARROW_CELL {
                cost += NARROW_CELL_COST;
            }
            // Its mirror: a wide strip keeping under half a photo's height
            // (a group shot or a tall statue squeezed into a ribbon) cuts
            // heads and feet alike. Panoramas are made for strips.
            if c > 1.0 / NARROW_CELL && a / c < NARROW_CELL && a < PANORAMA_ASPECT {
                cost += NARROW_CELL_COST;
            }
            // A cell that would cut a detected face costs far more than any
            // crop: that layout loses.
            // A whole photo is never cropped, so it cuts no one.
            if !slot[i].whole && frame_cuts(slot[i], cw, row_h[r], 0.0) {
                cost += FACE_CUT_COST;
            }
            cost += if slot[i].whole {
                // Never cropped: a mismatched cell leaves bars, so the
                // layout gives it a cell of its own shape.
                WHOLE_MISMATCH_COST * (1.0 - whole_fill(c, a))
            } else {
                weight
                    * if c < a {
                        1.0 - c / a // sides cut
                    } else {
                        (1.0 - a / c) * if a > 1.0 { 0.6 } else { 1.0 } // top/bottom cut
                    }
            };
        }
        // Unequal sizes: shrinking a photo costs as much more as
        // cropping it does, so people photos get the big cells.
        let areas: Vec<f64> = cells.iter().map(|c| c.2 * c.3).collect();
        // The owner's featured photo takes the biggest cell: by the area it
        // fills (a never-cropped photo fills only its shape of the cell).
        let shown: Vec<f64> = slot
            .iter()
            .zip(&cells)
            .map(|(p, c)| {
                let area = c.2 * c.3;
                if p.whole && c.3 > 0.0 {
                    area * whole_fill(c.2 / c.3, aspect(p))
                } else {
                    area
                }
            })
            .collect();
        let largest = |featured: bool| {
            slot.iter()
                .zip(&shown)
                .filter(|(p, _)| p.feature == featured)
                .map(|(_, &a)| a)
                .fold(0.0, f64::max)
        };
        // Clearly the biggest: as big as any other featured photo (within
        // 2%), and every unfeatured photo at most 90% of it.
        let floor = (largest(true) * 0.98).max(largest(false) / 0.9);
        if slot
            .iter()
            .zip(&shown)
            .any(|(p, &a)| p.feature && a < floor)
        {
            cost += FEATURE_COST;
        }
        let mean = areas.iter().sum::<f64>() / n as f64;
        cost += BALANCE
            * areas
                .iter()
                .zip(slot)
                .map(|(a, p)| {
                    let r = (a / mean).ln();
                    if r < 0.0 {
                        -r * people_weight(p)
                    } else {
                        r
                    }
                })
                .sum::<f64>();
        if best.as_ref().is_none_or(|(c, _)| cost < *c - 1e-9) {
            best = Some((cost, cells));
        }
    }
    best.map(|(_, cells)| cells).unwrap_or_default()
}

/// [`grid_layout`] on a `w` x `h` page (any unit) with the book's gutter,
/// 1% of the page width: one geometry for sizing, drawing and captions.
fn grid_cells(slot: &[&BookPhoto], w: f64, h: f64) -> Vec<Rect> {
    grid_layout(slot, w, h, w / 100.0)
}

/// Lowest print resolution a photo is stretched to.
const MIN_PRINT_DPI: f64 = 200.0;

/// What [`sharp_cells`] sizes to: a hair over the line, so rounding never
/// tips a photo under it.
const SHARP_DPI: f64 = MIN_PRINT_DPI + 1.0;

/// `cells` (in units of `per_inch` to the inch) with each low-resolution
/// photo's cell shrunk, about its centre, to what the photo fills at
/// [`MIN_PRINT_DPI`]: a small picture (the owner's 1536px "2009 and now"
/// collage) prints as big as it stays sharp, rather than being dropped for
/// a cell too big for it.
fn sharp_cells(slot: &[&BookPhoto], cells: Vec<Rect>, per_inch: f64) -> Vec<Rect> {
    slot.iter()
        .zip(cells)
        .map(|(p, (x, y, w, h))| {
            if p.width == 0 || p.height == 0 {
                return (x, y, w, h);
            }
            let (mw, mh) = (
                f64::from(p.width) / SHARP_DPI * per_inch,
                f64::from(p.height) / SHARP_DPI * per_inch,
            );
            let (cw, ch) = (w.min(mw), h.min(mh));
            (x + (w - cw) / 2.0, y + (h - ch) / 2.0, cw, ch)
        })
        .collect()
}

/// Paint `slot` into `cells` (from [`grid_cells`] at `tw`x`th`).
fn draw_grid(
    slot: &[&BookPhoto],
    cells: &[Rect],
    (tw, th): (u32, u32),
    bars: &[u32],
    out: &Path,
    ff: &Fit,
    fb: &mut Vec<String>,
) -> Result<(), crate::MediaError> {
    let mut page = solid(tw, th, Rgb([18, 20, 26]));
    for (k, (p, &(x, y, w, h))) in slot.iter().zip(cells).enumerate() {
        let (x, y) = (x.round() as u32, y.round() as u32);
        let (w, h) = (
            (w.round() as u32).min(tw - x),
            (h.round() as u32).min(th - y),
        );
        // A captioned photo leaves its caption a dark band of its own
        // below it, so the words never cover what the photo shows.
        let bar = bars.get(k).copied().unwrap_or(0).min(h / 2);
        let h = h - bar;
        let fit = fit_photo(p, w.max(1), h.max(1), ff, fb);
        image::imageops::replace(&mut page, &fit, i64::from(x), i64::from(y));
    }
    save_jpg(&page, out)
}

/// The whole route map on the page: the card is a wide film frame, and
/// cropping it to the page would cut off the ends of the trip (Singapore,
/// home). It sits centred on the book's dark ground.
fn draw_map_page(card: &Path, tw: u32, th: u32, out: &Path) -> Result<(), crate::MediaError> {
    // Upright: the owner's own front cover may be a JPEG that carries an
    // EXIF orientation, and the cover itself prints it oriented.
    let d = open_oriented(card).map_err(|e| crate::MediaError::Io(format!("map card: {e}")))?;
    save_jpg(&contain_on_page(&d.to_rgb8(), tw, th, 0), out)
}

fn draw_text_placeholder(
    kind: PageKind,
    tw: u32,
    th: u32,
    out: &Path,
) -> Result<(), crate::MediaError> {
    let mut page = solid(tw, th, Rgb([18, 20, 26]));
    let rule_y = th / 2;
    for y in rule_y..(rule_y + (th / 200).max(4)) {
        for x in tw / 4..tw * 3 / 4 {
            page.put_pixel(
                x,
                y,
                Rgb(if kind == PageKind::Divider {
                    [245, 178, 66]
                } else {
                    [94, 234, 212]
                }),
            );
        }
    }
    save_jpg(&page, out)
}

/// Collage grid (cols, rows) for `n` cover tiles.
fn cover_grid(n: usize) -> (u32, u32) {
    match n {
        0 | 1 => (1, 1),
        2..=4 => (2, 2),
        5 | 6 => (3, 2),
        7..=9 => (3, 3),
        _ => (4, 3),
    }
}

/// White text on transparency via Typst, for compositing over photos.
/// `scratch_png` is a temporary path, removed once decoded.
fn typst_text_png(
    typst_bin: &str,
    title: &str,
    sub: &str,
    scratch_png: &Path,
) -> Option<image::DynamicImage> {
    let doc = format!(
        "#set page(width: 11in, height: auto, fill: none, margin: 0in)\n#set text(font: {BOOK_FONTS}, fill: white)\n#align(center)[#text(size: 44pt, weight: \"bold\")[{}]]\n#align(center)[#text(size: 20pt, weight: \"medium\")[{}]]\n",
        typ_escape(title),
        typ_escape(sub)
    );
    crate::titlecard::render_text_png(typst_bin, &doc, scratch_png, 200).ok()?;
    let img = image::open(scratch_png).ok();
    let _ = std::fs::remove_file(scratch_png);
    img
}

/// Typst font list for every caption, title and the cover: the bundled
/// sans first, then sans faces macOS and Windows ship. Typst's own
/// fallback serif prints a comma like a full stop ("October 3–30. 2026").
const BOOK_FONTS: &str = r#"("DejaVu Sans", "Avenir Next", "Helvetica Neue", "Arial")"#;

/// Width / height of a cover tile in the 4×3 mosaic above the title band
/// (882×981 px on a 12-inch cover): faces are checked against it.
const COVER_TILE_ASPECT: f64 = 0.9;

/// Photos at the start of the book (hero, spread, the first pages) the
/// cover never repeats.
const COVER_SKIPS_FIRST: usize = 30;

/// Detected face size (of the photo's shorter side) from which a face is
/// someone the photo is of, not a stranger in the crowd or a far statue.
const SUBJECT_FACE: f32 = 0.08;

/// How much the cover wants a photo: 2 for a camera original of people
/// (tagged in the library, or a face big enough to be the subject), 1 for
/// another camera photo, 0 for a PNG (a screenshot or a "2009 vs 2026"
/// composite) or a panorama, whose middle slice in a square tile is
/// rarely its best part (the owner can pin one with a focus instead).
fn cover_rank(p: &BookPhoto) -> u8 {
    let png = p.is_png();
    let panorama = p.height > 0 && f64::from(p.width) / f64::from(p.height) >= PANORAMA_ASPECT;
    // Faces that a squarish tile would cut (a wide group) make a poor tile.
    let tile_cuts = frame_cuts(p, COVER_TILE_ASPECT, 1.0, 0.0);
    if png || panorama || tile_cuts {
        0
    } else if p.is_of_people() {
        2
    } else {
        1
    }
}

/// Indices of up to 12 cover tiles, in trip order. The owner's pins come
/// first. The rest split the trip into even stretches and take each
/// stretch's best photo: the best [`cover_rank`], then the best score, so
/// a stretch's tile is its strongest people shot, never just whichever
/// photo sat at the stretch's edge. `opening` (the hero and the spread)
/// never repeats on the cover.
fn cover_picks(photos: &[BookPhoto], opening: &[usize]) -> Vec<usize> {
    let want = photos.len().min(12);
    let mut picks: Vec<usize> = (0..photos.len())
        .filter(|&i| photos[i].cover_focus.is_some())
        .take(want)
        .collect();
    let pool: Vec<usize> = (0..photos.len())
        .filter(|i| !picks.contains(i) && !opening.contains(i))
        .collect();
    let slots = (want - picks.len()).min(pool.len());
    // One tile per day where the trip allows: two tiles of one scene (a
    // pinned waterfall beside another shot of it) waste the cover.
    let mut days: HashSet<u32> = picks.iter().filter_map(|&i| photos[i].day).collect();
    // Ranked once: each rank runs a framing search.
    let rank: Vec<u8> = photos.iter().map(cover_rank).collect();
    let better = |a: &usize, b: &usize| {
        rank[*a]
            .cmp(&rank[*b])
            .then(photos[*a].score.total_cmp(&photos[*b].score))
    };
    for k in 0..slots {
        let stretch = &pool[k * pool.len() / slots..(k + 1) * pool.len() / slots];
        let fresh = stretch
            .iter()
            .filter(|&&i| photos[i].day.is_none_or(|d| !days.contains(&d)))
            .max_by(|a, b| better(a, b));
        if let Some(&best) = fresh.or_else(|| stretch.iter().max_by(|a, b| better(a, b))) {
            days.extend(photos[best].day);
            picks.push(best);
        }
    }
    // A book that is nothing but its opening still gets a photo cover.
    if picks.is_empty() {
        picks.extend(
            opening
                .iter()
                .copied()
                .filter(|&i| i < photos.len())
                .take(want),
        );
    }
    picks.sort_unstable();
    picks.dedup();
    picks
}

/// Where a cover tile centres on its photo: the owner's cover focus
/// (`--cover REF@PCT`), so the tile shows that part of the photo. A centred
/// pin (`--cover REF`, stored as 50) is no focus at all: the tile keeps its
/// faces as any other does.
fn cover_tile_focus(p: &BookPhoto) -> Option<(f32, f32)> {
    p.cover_focus
        .filter(|&fx| fx != 0.5)
        .map(|fx| (fx, p.crop_focus.map_or(0.5, |c| c.1)))
        .or(p.crop_focus)
}

/// Build `cover.jpg`: a mosaic of the `picks` tiles (from [`cover_picks`])
/// above a title band. Returns None (with a fallback note) when no photo decodes.
fn draw_cover_collage(
    photos: &[BookPhoto],
    picks: &[usize],
    (tw, th): (u32, u32),
    opts: &BookOptions,
    fit: &Fit,
    out: &Path,
    fallbacks: &mut Vec<String>,
) -> Option<PathBuf> {
    if photos.is_empty() {
        return None;
    }
    let (cols, rows) = cover_grid(picks.len());
    let gap = 14u32;
    let tile_w = (tw - gap * (cols + 1)) / cols;
    // The title gets a band of its own below the photos: no tile's faces
    // ever sit under the title.
    let band_h = th / 6;
    let tile_h = (th - band_h - gap * (rows + 1)) / rows;
    let mut canvas = solid(tw, th, Rgb([18, 20, 26]));
    for (k, pi) in picks.iter().enumerate() {
        let (cx, cy) = ((k as u32) % cols, (k as u32) / cols);
        let p = &photos[*pi];
        let focus = cover_tile_focus(p);
        // Cover tiles always fill their cell: letterboxed tiles leave ragged
        // gaps in the mosaic.
        let tile = fit_photo_focused(p, focus, (tile_w, tile_h), 0.0, false, fit, fallbacks);
        image::imageops::replace(
            &mut canvas,
            &tile,
            (gap + cx * (tile_w + gap)) as i64,
            (gap + cy * (tile_h + gap)) as i64,
        );
    }
    // Composite the Typst title into the band below the tiles.
    match typst_text_png(
        &opts.typst_bin,
        &opts.trip_name,
        opts.dates.as_deref().unwrap_or("TripTape Photo Book"),
        &out.with_file_name("cover-title.png"),
    ) {
        Some(text) => {
            let text = text.to_rgba8();
            let scale = (tw as f32 * 0.86 / text.width() as f32).min(1.6);
            let nw = (text.width() as f32 * scale) as u32;
            let nh = (text.height() as f32 * scale) as u32;
            let text = image::imageops::resize(
                &text,
                nw.max(1),
                nh.max(1),
                image::imageops::FilterType::Lanczos3,
            );
            let mut base = image::DynamicImage::ImageRgb8(canvas.clone()).to_rgba8();
            image::imageops::overlay(
                &mut base,
                &text,
                ((tw - nw.min(tw)) / 2) as i64,
                (th - band_h + (band_h.saturating_sub(nh)) / 2) as i64,
            );
            canvas = image::DynamicImage::ImageRgba8(base).to_rgb8();
        }
        None => fallbacks.push("cover: Typst title unavailable; collage without text".into()),
    }
    match save_jpg(&canvas, out) {
        Ok(()) => Some(out.to_path_buf()),
        Err(e) => {
            fallbacks.push(format!("cover collage failed ({e}); text cover kept"));
            None
        }
    }
}

/// An owner's picture cropped to the page (`size`) around its centre: the
/// owner's cover or map is made for it.
fn page_crop(src: &Path, size: (u32, u32)) -> Result<RgbImage, crate::MediaError> {
    open_oriented(src)
        .map(|img| {
            let img = img.to_rgb8();
            let (sw, sh) = (
                f64::from(img.width().max(1)),
                f64::from(img.height().max(1)),
            );
            let window = focus_window(sw, sh, f64::from(size.0), f64::from(size.1), (0.5, 0.5));
            crop_window(&img, window, size.0, size.1)
        })
        .map_err(|e| crate::MediaError::Io(format!("{}: {e}", src.display())))
}

/// An owner's picture as a cover page, cropped to `size` around its centre.
fn image_cover(
    src: &Path,
    size: (u32, u32),
    out: &Path,
    fallbacks: &mut Vec<String>,
) -> Option<PathBuf> {
    let _ = std::fs::remove_file(out);
    save_cover(page_crop(src, size), out, "front cover", fallbacks)
}

/// Save a cover page made for `out`; `None` with a note naming `what` when
/// it could not be made or saved.
fn save_cover(
    made: Result<RgbImage, crate::MediaError>,
    out: &Path,
    what: &str,
    fallbacks: &mut Vec<String>,
) -> Option<PathBuf> {
    match made.and_then(|img| save_jpg(&img, out)) {
        Ok(()) => Some(out.to_path_buf()),
        Err(e) => {
            fallbacks.push(format!("{what} skipped: {e}"));
            None
        }
    }
}

/// Most tiles the sights mosaic holds (four by four).
const SIGHT_TILES: usize = 16;

/// Close-ups of the trip's sights for the back cover, in trip order: the
/// owner's cover pins first, the tiles left over filled with photos with no
/// one in them (no tagged person, no face), not panoramas or screenshots,
/// one per place the pins do not already show, best-scored first. A place
/// is the owner's named place, else the photo's place label.
fn sight_picks(photos: &[&BookPhoto]) -> Vec<usize> {
    let place_of = |p: &BookPhoto| p.area.clone().or_else(|| p.place.clone());
    // The owner's chosen sights (cover pins) fill the mosaic first.
    let mut pinned: Vec<usize> = (0..photos.len())
        .filter(|&i| photos[i].cover_focus.is_some())
        .collect();
    pinned.sort_by_key(|&i| photos[i].taken_at);
    pinned.truncate(SIGHT_TILES);
    let pinned_places: HashSet<String> =
        pinned.iter().filter_map(|&i| place_of(photos[i])).collect();
    let nobody = |p: &BookPhoto| {
        p.tagged_people == 0 && p.people == 0 && p.family.is_empty() && p.face_size < 0.01
    };
    let shaped = |p: &BookPhoto| {
        p.width > 0
            && p.height > 0
            && f64::from(p.width) / f64::from(p.height) < PANORAMA_ASPECT
            && !p.is_png()
    };
    let mut best: HashMap<String, usize> = HashMap::new();
    for (i, p) in photos.iter().copied().enumerate() {
        if p.cover_focus.is_some() || p.still_ms.is_some() || !nobody(p) || !shaped(p) {
            continue;
        }
        let Some(place) = place_of(p).filter(|place| !pinned_places.contains(place)) else {
            continue;
        };
        let keep = best.get(&place).is_none_or(|&j| p.score > photos[j].score);
        if keep {
            best.insert(place, i);
        }
    }
    let mut extra: Vec<usize> = best.into_values().collect();
    extra.sort_by(|&a, &b| photos[b].score.total_cmp(&photos[a].score));
    // Cut to the square grid the mosaic draws (16, 9 or 4 tiles) before
    // putting the picks in trip order, so what falls off is the
    // lowest-scored extra, not whatever came last in the trip. Under four
    // the count stands, for the mosaic's "too few sights" note.
    let total = pinned.len() + extra.len();
    let target = [SIGHT_TILES, 9, 4]
        .into_iter()
        .find(|&n| total >= n)
        .unwrap_or(total);
    pinned.truncate(target);
    extra.truncate(target - pinned.len());
    let mut picks = pinned;
    picks.extend(extra);
    // Trip order on the page.
    picks.sort_by_key(|&i| photos[i].taken_at);
    picks
}

/// The back cover as a full page of square sight tiles, no text. `None`
/// (with a note) when there are fewer than four sights to show.
fn draw_sights_mosaic(
    photos: &[&BookPhoto],
    picks: &[usize],
    (tw, th): (u32, u32),
    fit: &Fit,
    out: &Path,
    fallbacks: &mut Vec<String>,
) -> Option<PathBuf> {
    let _ = std::fs::remove_file(out);
    // The biggest square grid the sights fill: 16, 9 or 4 tiles.
    let Some(cols) = [4u32, 3, 2]
        .into_iter()
        .find(|&c| picks.len() >= (c * c) as usize)
    else {
        fallbacks.push(format!(
            "back cover sights skipped: {} sight(s) found, the mosaic needs at least 4",
            picks.len()
        ));
        return None;
    };
    let n = (cols * cols) as usize;
    let gap = 14u32;
    // At least a pixel a tile, however small the canvas.
    let tile_w = (tw.saturating_sub(gap * (cols + 1)) / cols).max(1);
    let tile_h = (th.saturating_sub(gap * (cols + 1)) / cols).max(1);
    let mut canvas = solid(tw, th, Rgb([18, 20, 26]));
    for (k, &pi) in picks.iter().take(n).enumerate() {
        let (cx, cy) = ((k as u32) % cols, (k as u32) / cols);
        let p = photos[pi];
        let focus = cover_tile_focus(p);
        let tile = fit_photo_focused(p, focus, (tile_w, tile_h), 0.0, false, fit, fallbacks);
        image::imageops::replace(
            &mut canvas,
            &tile,
            (gap + cx * (tile_w + gap)) as i64,
            (gap + cy * (tile_h + gap)) as i64,
        );
    }
    save_cover(Ok(canvas), out, "back cover sights", fallbacks)
}

/// Build `back-cover.jpg` at `size`: the owner's own picture when given,
/// else the journey's whole route on a parchment map under the trip's
/// name. `None` when there is neither, or (with a note) when it cannot be
/// made; a previous export's back cover never survives beside new pages.
fn make_back_cover(
    opts: &BookOptions,
    size: (u32, u32),
    out: &Path,
    fallbacks: &mut Vec<String>,
) -> Option<PathBuf> {
    let _ = std::fs::remove_file(out);
    let made = match (&opts.back_cover_image, &opts.journey) {
        (Some(src), _) => page_crop(src, size),
        (None, Some(journey)) => {
            crate::parchment::render_overview(journey, Some(&opts.trip_name), size, &opts.typst_bin)
        }
        (None, None) => return None,
    };
    save_cover(made, out, "back cover", fallbacks)
}

fn save_jpg(img: &RgbImage, out: &Path) -> Result<(), crate::MediaError> {
    let f = std::fs::File::create(out)
        .map_err(|e| crate::MediaError::Io(format!("save {}: {e}", out.display())))?;
    image::codecs::jpeg::JpegEncoder::new_with_quality(std::io::BufWriter::new(f), 90)
        .encode_image(img)
        .map_err(|e| crate::MediaError::Io(format!("save {}: {e}", out.display())))
}

// -- Typst -----------------------------------------------------------------

/// Blank pages between the cover and page 1 in `proof.pdf`: the back of
/// the cover (see [`write_typst_and_compile`]).
const PROOF_LEAD_BLANKS: usize = 1;

/// Write the Typst source and compile it. `bleed` selects press geometry
/// (trim + 0.125 in each side); otherwise screen proof geometry.
fn write_typst_and_compile(
    jpegs: &[PathBuf],
    texts: &PageTexts,
    opts: &BookOptions,
    out_dir: &Path,
    bleed: bool,
    (cover, back_cover): (Option<&Path>, Option<&Path>),
) -> Result<PathBuf, crate::MediaError> {
    let (tw_in, th_in) = opts.preset.trim_inches();
    let (pw, ph) = if bleed {
        (tw_in + 0.25, th_in + 0.25)
    } else {
        (tw_in, th_in)
    };
    let pdf_name = if bleed { "press.pdf" } else { "proof.pdf" };
    let typ_name = if bleed { "press.typ" } else { "proof.typ" };
    let mut typ = format!(
        "#set page(width: {pw}in, height: {ph}in, margin: 0in)\n#set text(font: {BOOK_FONTS}, size: 14pt, fill: white)\n"
    );
    match cover {
        Some(c) => typ.push_str(&format!(
            "#image(\"{}\", width: 100%, height: 100%, fit: \"cover\")\n#pagebreak()\n",
            c.file_name().unwrap().to_string_lossy()
        )),
        None => typ.push_str(&format!(
            "#rect(width: 100%, height: 100%, fill: rgb(18, 20, 26))\n#place(center + horizon)[#text(size: 28pt)[{}]]\n#pagebreak()\n",
            typ_escape(&opts.trip_name)
        )),
    }
    // The proof is for two-page viewing, which shows the cover alone like
    // a book (Preview does). A blank for the cover's back then faces page
    // 1, so page 1 opens on the right and every spread (2|3, 4|5, …) lands
    // side by side. Press files keep interior pages only.
    if !bleed {
        for _ in 0..PROOF_LEAD_BLANKS {
            typ.push_str("#box()\n#pagebreak()\n");
        }
    }
    for (i, jpg) in jpegs.iter().enumerate() {
        let page_no = i + 1;
        typ.push_str(&format!(
            "#rect(width: 100%, height: 100%, fill: rgb(18, 20, 26))\n#place(center + horizon)[#image(\"{}\", width: 100%, height: 100%, fit: \"cover\")]\n",
            jpg.file_name().unwrap().to_string_lossy()
        ));
        // Height (pt) of the page's bottom strip, which photo captions in
        // the bottom row must clear.
        let mut strip_pt = 0.0;
        if let Some(lines) = texts.lines.get(&page_no).filter(|l| !l.is_empty()) {
            // Chapter title pages and day banners carry a bigger strip, so
            // they read as an opening without dimming the photo.
            let (size, inset) = if texts.titles.contains(&page_no) {
                (24, 14)
            } else {
                (14, 8)
            };
            // [`page_caption`] already punctuates: "…for 8 years. ·
            // Paradox Singapore" keeps its full stop.
            let clean: Vec<&str> = lines.iter().map(|l| l.trim_end()).collect();
            let title_page = texts.titles.contains(&page_no);
            if title_page && clean.len() > 1 {
                // The day heading on its own line; the owner's words and
                // the places beneath it in caption size, never fused into
                // one three-line block.
                let head = clean[0];
                let rest = title_rest_lines(&clean[1..])
                    .iter()
                    .map(|l| format!("#text(size: 14pt)[{}]", typ_escape(l)))
                    .collect::<Vec<_>>()
                    .join(" \\ ");
                strip_pt = title_strip_pt(&clean, size, inset, pw);
                typ.push_str(&format!(
                    "#place(bottom + center)[#rect(fill: rgb(18, 20, 26).transparentize(20%), inset: {inset}pt)[#align(center)[#text(size: {size}pt)[{}] \\ {}]]]\n",
                    typ_escape(head),
                    rest
                ));
            } else {
                let text = clean.join(" · ");
                strip_pt = strip_height_pt(&text, size, inset, pw);
                typ.push_str(&format!(
                    "#place(bottom + center)[#rect(fill: rgb(18, 20, 26).transparentize(20%), inset: {inset}pt)[#text(size: {size}pt)[{}]]]\n",
                    typ_escape(&text)
                ));
            }
        }
        for (rect, text) in texts.cells.get(&page_no).into_iter().flatten() {
            // Grid cells end above the page strip, so a cell caption needs
            // no lift unless its cell still reaches into the strip's band.
            // Only by as much as the cell actually runs into the strip.
            let overlap = rect.1 + rect.3 - (1.0 - strip_pt / (ph * 72.0));
            let lift = (overlap * ph * 72.0).max(0.0);
            typ.push_str(&cell_caption_typ(
                (pw, ph),
                (tw_in, th_in),
                *rect,
                lift,
                text,
            ));
        }
        if i + 1 < jpegs.len() {
            typ.push_str("#pagebreak()\n");
        }
    }
    if let Some(b) = back_cover {
        typ.push_str(&format!(
            "#pagebreak()\n#image(\"{}\", width: 100%, height: 100%, fit: \"cover\")\n",
            b.file_name().unwrap().to_string_lossy()
        ));
    }
    let typ_path = out_dir.join(typ_name);
    std::fs::write(&typ_path, &typ).map_err(|e| crate::MediaError::Io(e.to_string()))?;
    let pdf = out_dir.join(pdf_name);
    let st = triptape_core::process::output(Command::new(&opts.typst_bin).args([
        "compile",
        &typ_path.to_string_lossy(),
        &pdf.to_string_lossy(),
    ]))
    .map_err(|e| crate::MediaError::Io(format!("spawn {}: {e}", opts.typst_bin)))?;
    if !st.status.success() {
        return Err(crate::MediaError::Io(format!(
            "typst compile failed: {}",
            String::from_utf8_lossy(&st.stderr).trim()
        )));
    }
    Ok(pdf)
}

/// Rasterize the captioned proof pages at 300 ppi and overwrite the
/// matching page JPEGs. Proof page N+1+[`PROOF_LEAD_BLANKS`] is interior
/// page N (page 1 is the uncounted cover-proxy); proof geometry equals
/// trim, so sizes match.
fn burn_captions(
    jpegs: &[PathBuf],
    captioned: &[usize],
    opts: &BookOptions,
    out_dir: &Path,
    (tw, th): (u32, u32),
) -> Result<(), crate::MediaError> {
    let raster = out_dir.join(".caption_raster");
    let _ = std::fs::remove_dir_all(&raster);
    std::fs::create_dir_all(&raster).map_err(|e| crate::MediaError::Io(e.to_string()))?;
    let proof_page = |n: usize| n + 1 + PROOF_LEAD_BLANKS;
    let pages = captioned
        .iter()
        .map(|&n| proof_page(n).to_string())
        .collect::<Vec<_>>()
        .join(",");
    let result = (|| {
        let st = triptape_core::process::output(Command::new(&opts.typst_bin).args([
            "compile",
            &out_dir.join("proof.typ").to_string_lossy(),
            &raster.join("p{p}.png").to_string_lossy(),
            "--ppi",
            "300",
            "--pages",
            &pages,
        ]))
        .map_err(|e| crate::MediaError::Io(format!("spawn {}: {e}", opts.typst_bin)))?;
        if !st.status.success() {
            return Err(crate::MediaError::Io(format!(
                "typst raster failed: {}",
                String::from_utf8_lossy(&st.stderr).trim()
            )));
        }
        for &n in captioned {
            let png = raster.join(format!("p{}.png", proof_page(n)));
            let img = image::open(&png)
                .map_err(|e| crate::MediaError::Io(format!("read {}: {e}", png.display())))?
                .to_rgb8();
            if img.dimensions() != (tw, th) {
                return Err(crate::MediaError::Io(format!(
                    "raster page {n} is {}x{}, want {tw}x{th}",
                    img.width(),
                    img.height()
                )));
            }
            save_jpg(&img, &jpegs[n - 1])?;
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&raster);
    result
}

/// Height in page pixels of the band each captioned cell keeps below its
/// photo: the caption box as [`cell_caption_typ`] draws it (11 pt text,
/// 6 pt inset, wrapped to the cell).
fn caption_bars(
    cells: &[(usize, Rect, String)],
    n: usize,
    preset: BookPreset,
    th: u32,
) -> Vec<u32> {
    let (w_in, h_in) = preset.trim_inches();
    let px_per_pt = f64::from(th) / (h_in * 72.0);
    let mut bars = vec![0u32; n];
    for (k, rect, text) in cells {
        let usable_pt = (rect.2 * w_in * 72.0 - 12.0).max(36.0);
        let lines = (text.chars().count() as f64 * 11.0 * 0.55 / usable_pt)
            .ceil()
            .max(1.0);
        let box_pt = lines * 11.0 * 1.3 + 12.0 + 10.0;
        if let Some(b) = bars.get_mut(*k) {
            *b = (box_pt * px_per_pt).round() as u32;
        }
    }
    bars
}

/// Pixels of a page (`th` tall at 300 dpi) the bottom caption strip will
/// cover: [`strip_height_pt`] at the size the burn uses (large for titles
/// and day banners).
fn strip_px(lines: &[String], title: bool, preset: BookPreset, th: u32) -> u32 {
    if lines.is_empty() {
        return 0;
    }
    let (size, inset) = if title { (24, 14) } else { (14, 8) };
    let (w_in, h_in) = preset.trim_inches();
    let pt = if title && lines.len() > 1 {
        title_strip_pt(lines, size, inset, w_in)
    } else {
        strip_height_pt(&lines.join(" · "), size, inset, w_in)
    };
    (pt / (h_in * 72.0) * f64::from(th)).round() as u32
}

/// Height (pt) of a title page's strip as the burn draws it: the heading
/// on its own line at `size`, the rest beneath it at caption size.
fn title_strip_pt<S: AsRef<str>>(lines: &[S], size: u32, inset: u32, page_w: f64) -> f64 {
    strip_height_pt(lines[0].as_ref(), size, inset, page_w)
        + title_rest_lines(&lines[1..])
            .iter()
            .map(|l| strip_height_pt(l, 14, 0, page_w))
            .sum::<f64>()
}

/// The lines under a title page's heading: the owner's sentence on a line
/// of its own, then the places joined with " · " ("…for 8 years." and
/// "Paradox Singapore" below it, not run together).
fn title_rest_lines<S: AsRef<str>>(rest: &[S]) -> Vec<String> {
    let mut out: Vec<Vec<&str>> = vec![];
    let mut new_line = true;
    for l in rest {
        let l = l.as_ref().trim_end();
        if new_line {
            out.push(vec![]);
        }
        out.last_mut().expect("pushed above").push(l);
        new_line = l.ends_with(SENTENCE_END);
    }
    out.iter().map(|g| g.join(" · ")).collect()
}

/// Height in points of a bottom strip of `text` at `size` pt with `inset`
/// padding on a `page_w`-inch page: a generous estimate (half an em per
/// character, wrapping at the page width) so a caption lifted above it
/// clears it.
fn strip_height_pt(text: &str, size: u32, inset: u32, page_w: f64) -> f64 {
    let (size, inset) = (f64::from(size), f64::from(inset));
    let width_pt = text.chars().count() as f64 * size * 0.55;
    let lines = (width_pt / (page_w * 72.0 - 2.0 * inset)).ceil().max(1.0);
    lines * size * 1.3 + 2.0 * inset
}

/// Typst for one caption on a grid photo whose cell is `rect` (fractions
/// of the page): a strip along the bottom of that photo, wrapped to the
/// cell's width. `page` is the PDF page and `trim` the page JPEG's size
/// (inches); with bleed the JPEG is cover-scaled onto the larger page, so
/// the cell is mapped the same way.
fn cell_caption_typ(
    page: (f64, f64),
    trim: (f64, f64),
    (x, y, w, h): Rect,
    lift_pt: f64,
    text: &str,
) -> String {
    let scale = (page.0 / trim.0).max(page.1 / trim.1);
    let (ox, oy) = (
        (page.0 - trim.0 * scale) / 2.0,
        (page.1 - trim.1 * scale) / 2.0,
    );
    let (sw, sh) = (trim.0 * scale, trim.1 * scale);
    format!(
        "#place(top + left, dx: {:.4}in, dy: {:.4}in)[#box(width: {:.4}in, height: {:.4}in)[#place(bottom + center, dy: {:.1}pt)[#block(fill: rgb(18, 20, 26).transparentize(20%), inset: 6pt, radius: 2pt)[#text(size: 11pt)[{}]]]]]\n",
        ox + x * sw,
        oy + y * sh,
        w * sw,
        h * sh,
        -6.0 - lift_pt,
        typ_escape(text)
    )
}

fn typ_escape(t: &str) -> String {
    t.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace('#', "\\#")
}

fn guide_text(
    preset: BookPreset,
    trip: &str,
    counted: usize,
    captions_burned: bool,
    back_cover: bool,
) -> String {
    let back_note = if back_cover {
        "\nback-cover.jpg is the trip's route map, for the back cover (not counted)."
    } else {
        ""
    };
    let (tw, th) = preset.trim_px();
    let caption_note = if captions_burned {
        "Captions are burned into the page JPEGs exactly as shown in proof.pdf."
    } else {
        "WARNING: captions are NOT in the page JPEGs (proof rendering was unavailable).\n\
        Add them in the vendor designer, or re-export with Typst installed before ordering."
    };
    format!(
        "TripTape book export — {trip} ({}, {counted} interior pages)\n\
        \n\
        These page JPEGs are rendered at exact trim size ({tw}x{th}px, 300 dpi sRGB, quality ~90).\n\
        cover.jpg is a suggested photo-collage cover (not counted); use it as the vendor cover or design your own in the vendor designer.{back_note}\n\
        \n\
        MIXBOOK / MILK / VISTAPRINT (generic steps; confirm against the vendor gate record):\n\
        1. Create a {} book with {counted} pages (above the 24-page vendor minimum; cover done separately).\n\
        2. Upload the page_*.jpg files in order (page_001.jpg = first interior page).\n\
        3. Drop each JPEG onto a full-bleed single-photo page layout, in order.\n\
        4. Spread halves (if any) go on facing left/right full-bleed halves in order.\n\
        5. Keep text/faces 0.5in from the gutter; nothing critical within 0.375in of trim.\n\
        6. Review proof.pdf on screen before ordering; it shows the exact layout.\n\
        \n\
        Files: page JPEGs + proof.pdf (screen) + press.pdf (0.125in bleed, local shops only).\n\
        {caption_note}\n",
        preset.name(),
        preset.name(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_approved_book_lists_the_photos_on_its_pages() {
        let book = r#"{"pages": [
            {"page": 1, "photos": [{"asset_id": "a", "file": "A.HEIC"}]},
            {"page": 2, "kind": "map"},
            {"page": 3, "kind": "grid", "photos": [{"asset_id": "b"}, {"asset_id": "a"}]},
            {"page": 4, "kind": "closing", "photos": [{"asset_id": "end"}]}
        ]}"#;
        let ids = approved_ids(book).unwrap();
        assert_eq!(ids, HashSet::from(["a".to_string(), "b".to_string()]));
        assert!(approved_ids("{}").is_none());
        assert!(approved_ids("not json").is_none());
    }

    #[test]
    fn a_still_is_one_frame_of_its_video() {
        let ff = &triptape_core::process::Toolchain::global().ffmpeg.bin;
        if Command::new(ff).arg("-version").output().is_err() {
            eprintln!("ffmpeg missing; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("clip.mp4");
        let made = Command::new(ff)
            .args(["-v", "error", "-f", "lavfi", "-i", "testsrc2=s=320x180:d=2"])
            .arg(&src)
            .status()
            .unwrap();
        assert!(made.success());
        let frame = video_frame(ff, &src, 1500).expect("a frame 1.5s in");
        assert_eq!(frame.dimensions(), (320, 180));
        assert!(video_frame(ff, &dir.path().join("missing.mp4"), 0).is_none());
    }

    /// Book options for a P1 test book named `Test`, run with `typst_bin`.
    fn refs(photos: &[BookPhoto]) -> Vec<&BookPhoto> {
        photos.iter().collect()
    }

    fn test_opts(typst_bin: &str) -> BookOptions {
        BookOptions {
            preset: BookPreset::P1,
            trip_name: "Test".into(),
            typst_bin: typst_bin.into(),
            ffmpeg_bin: "ffmpeg".into(),
            max_pages: None,
            progress: Default::default(),
            dates: None,
            faces: None,
            journey: None,
            back_cover_image: None,
            front_cover_image: None,
            approved: None,
        }
    }

    #[test]
    fn an_owner_sentence_ends_before_its_place() {
        let mut b = BookPhoto::blank();
        b.caption = Some("It served us well for 8 years".into());
        b.place = Some("Paradox Singapore".into());
        b.keep = true;
        let caps = page_caption(&[&b], PageKind::PhotoCaption);
        assert_eq!(
            caps,
            ["It served us well for 8 years.", "Paradox Singapore"]
        );
    }

    #[test]
    fn a_lone_captioned_plate_joins_the_next_stretch_of_its_day() {
        let shot = |id: &str, area: &str, people: u32, keep: bool| BookPhoto {
            asset_id: id.into(),
            day: Some(22),
            area: Some(area.into()),
            people,
            keep,
            ..BookPhoto::blank()
        };
        let mut photos = vec![
            shot("laksa", "food court", 0, true),
            shot("cable", "Mount Faber Peak", 3, false),
            shot("peak", "Mount Faber Peak", 4, false),
        ];
        fold_lone_kept(&mut photos);
        assert_eq!(photos[0].area.as_deref(), Some("Mount Faber Peak"));
        // Next door with two or more photos beats a lone one next door.
        let mut photos = vec![
            shot("cloud", "Cloud Forest", 3, false),
            shot("cloud2", "Cloud Forest", 4, false),
            shot("durian", "food court", 0, true),
            shot("dome", "Flower Dome", 3, false),
        ];
        fold_lone_kept(&mut photos);
        assert_eq!(photos[2].area.as_deref(), Some("Cloud Forest"));
        // A stretch past a lone neighbour is never taken: the neighbour
        // still splits the pages, so the photo would print alone anyway.
        let mut photos = vec![
            shot("durian", "food court", 0, true),
            shot("dome", "Flower Dome", 3, false),
            shot("cloud", "Cloud Forest", 3, false),
            shot("cloud2", "Cloud Forest", 4, false),
        ];
        fold_lone_kept(&mut photos);
        assert_eq!(photos[0].area.as_deref(), Some("Flower Dome"));
        // A lone people photo keeps its own stretch.
        let mut photos = vec![
            BookPhoto {
                tagged_people: 2,
                ..shot("selfie", "food court", 2, true)
            },
            shot("cable", "Mount Faber Peak", 3, false),
        ];
        fold_lone_kept(&mut photos);
        assert_eq!(photos[0].area.as_deref(), Some("food court"));
    }

    #[test]
    fn a_title_page_puts_the_owners_sentence_above_its_place() {
        let rest = ["It served us well for 8 years.", "Paradox Singapore"];
        assert_eq!(
            title_rest_lines(&rest),
            ["It served us well for 8 years.", "Paradox Singapore"]
        );
        assert_eq!(
            title_rest_lines(&["Mount Faber", "Singapore"]),
            ["Mount Faber · Singapore"]
        );
    }

    #[test]
    fn an_owner_group_stays_on_one_page() {
        let chapters: Vec<Option<usize>> = vec![Some(0); 8];
        let days: Vec<Option<u32>> = vec![
            Some(1),
            Some(1),
            Some(2),
            Some(2),
            Some(2),
            Some(2),
            Some(2),
            Some(2),
        ];
        let one: Vec<bool> = vec![false, false, true, true, true, true, true, true];
        let flow = flow_pages(&chapters, &days, &[], &one, None, false, 60);
        assert!(flow.iter().any(|p| p.take == 6), "{flow:?}");
    }

    #[test]
    fn a_featured_photo_leads_its_page_only_within_a_moment() {
        let shot = |t: i64, feature: bool| BookPhoto {
            taken_at: Some(t),
            feature,
            width: 4000,
            height: 3000,
            chapter: Some(0),
            day: Some(1),
            ..BookPhoto::blank()
        };
        let flow = vec![
            FlowPage {
                kind: PageKind::CoverProxy,
                take: 0,
            },
            FlowPage {
                kind: PageKind::Hero,
                take: 1,
            },
            FlowPage {
                kind: PageKind::Grid,
                take: 2,
            },
            FlowPage {
                kind: PageKind::Closing,
                take: 0,
            },
        ];
        // The plate at :00, the featured group 48 s later: the group leads.
        let near = vec![shot(0, false), shot(1000, false), shot(1048, true)];
        let order = consumption_order(&near, &flow, BookPreset::P1);
        assert_eq!(&order[1..], [2, 1]);
        // Fifty minutes apart: time order stays.
        let far = vec![shot(0, false), shot(1000, false), shot(4000, true)];
        let order = consumption_order(&far, &flow, BookPreset::P1);
        assert_eq!(&order[1..], [1, 2]);
    }

    #[test]
    fn sights_are_one_unpeopled_close_up_per_place() {
        let shot = |area: &str, people: u32, w: u32, score: f32, t: i64| BookPhoto {
            area: Some(area.into()),
            people,
            width: w,
            height: 3000,
            score,
            taken_at: Some(t),
            ..BookPhoto::blank()
        };
        let photos = vec![
            shot("Buddha", 0, 4000, 0.5, 3),
            shot("Buddha", 0, 4000, 0.9, 2),
            shot("Buddha", 3, 4000, 1.0, 1),
            shot("Harbour", 0, 16000, 1.0, 4),
            shot("Temple", 0, 4000, 0.4, 5),
        ];
        // The better Buddha close-up, the temple; no family shot, no panorama.
        assert_eq!(sight_picks(&refs(&photos)), [1, 4]);
        // A cover pin leads (family shot or not) and stands for its place;
        // automatic sights fill the tiles left over.
        let mut pinned = photos;
        pinned[2].cover_focus = Some(0.5);
        assert_eq!(sight_picks(&refs(&pinned)), [2, 4]);
    }

    #[test]
    fn an_approved_book_prints_just_its_photos_and_the_owners_pins() {
        let mut opts = test_opts("typst");
        let shot = |id: &str| BookPhoto {
            asset_id: id.into(),
            ..BookPhoto::blank()
        };
        let (seen, new) = (shot("seen"), shot("new"));
        let pin = BookPhoto {
            pinned: true,
            keep: true,
            ..shot("pin")
        };
        assert!([&seen, &new, &pin].iter().all(|p| on_pages(p, &opts)));
        opts.approved = Some(HashSet::from(["seen".to_string()]));
        assert!(on_pages(&seen, &opts) && on_pages(&pin, &opts));
        assert!(!on_pages(&new, &opts));
        // A captioned photo the approved book left out stays out.
        let worded = BookPhoto {
            keep: true,
            caption: Some("Lunch".into()),
            ..shot("worded")
        };
        assert!(!on_pages(&worded, &opts));
        let mine = pin;
        // A back-cover sight stays off the pages, pinned or not.
        opts.front_cover_image = Some(PathBuf::from("cover.jpg"));
        let sight = BookPhoto {
            cover_focus: Some(0.5),
            ..mine
        };
        assert!(!on_pages(&sight, &opts));
        // With the owner's back cover too there is no sights mosaic to
        // keep it for.
        opts.back_cover_image = Some(PathBuf::from("back.jpg"));
        assert!(on_pages(&sight, &opts));
    }

    #[test]
    fn sights_are_cut_to_the_grid_by_score_not_by_date() {
        let shot = |area: &str, score: f32, t: i64| BookPhoto {
            area: Some(area.into()),
            width: 4000,
            height: 3000,
            score,
            taken_at: Some(t),
            ..BookPhoto::blank()
        };
        // Six places, the best-scored ones late in the trip: a 2x2 grid
        // keeps the top four, in trip order.
        let mut photos = vec![
            shot("A", 0.1, 1),
            shot("B", 0.2, 2),
            shot("C", 0.6, 3),
            shot("D", 0.7, 4),
            shot("E", 0.8, 5),
            shot("F", 0.9, 6),
        ];
        assert_eq!(sight_picks(&refs(&photos)), [2, 3, 4, 5]);
        // A late cover pin keeps its tile; the weakest extra gives way.
        photos[0].cover_focus = Some(0.5);
        photos[5].cover_focus = Some(0.5);
        assert_eq!(sight_picks(&refs(&photos)), [0, 3, 4, 5]);
    }

    #[test]
    fn a_mosaic_full_of_pins_shows_the_earliest_pins_alone() {
        // Eighteen pins, listed latest first, and one unpinned sight that
        // outscores them all: the sixteen tiles are the earliest pins.
        let mut photos: Vec<BookPhoto> = (0..18)
            .map(|k| BookPhoto {
                area: Some(format!("pin {k}")),
                cover_focus: Some(0.5),
                taken_at: Some(100 - k),
                ..BookPhoto::blank()
            })
            .collect();
        photos.push(BookPhoto {
            area: Some("elsewhere".into()),
            width: 4000,
            height: 3000,
            score: 0.9,
            taken_at: Some(1),
            ..BookPhoto::blank()
        });
        let want: Vec<usize> = (2..18).rev().collect();
        assert_eq!(sight_picks(&refs(&photos)), want);
    }

    #[test]
    fn a_mosaic_short_of_four_sights_is_skipped_with_a_note() {
        let dir = tempfile::tempdir().unwrap();
        let mut notes = Vec::new();
        let fit = Fit {
            ffmpeg: "ffmpeg",
            faces: None,
            found: Default::default(),
        };
        let photos = vec![BookPhoto::blank(); 3];
        let out = dir.path().join("back-cover.jpg");
        assert!(draw_sights_mosaic(
            &refs(&photos),
            &[0, 1, 2],
            (800, 600),
            &fit,
            &out,
            &mut notes
        )
        .is_none());
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("3 sight(s)"), "{notes:?}");
    }

    #[test]
    fn only_day_headings_print_large() {
        let kinds = vec![
            PageKind::CoverProxy,
            PageKind::Divider,
            PageKind::Grid,
            PageKind::Divider,
        ];
        let caps: HashMap<usize, Vec<String>> = [
            (
                1,
                vec!["Day 19 · Wed, Oct 21 — Ho Chi Minh City".to_string()],
            ),
            (3, vec!["Disney Adventure".to_string()]),
        ]
        .into();
        let titles = day_title_pages(&kinds, &HashSet::new(), &caps);
        assert_eq!(
            titles,
            HashSet::from([1]),
            "the mid-day divider stays small"
        );
    }

    #[test]
    fn a_chapter_opening_on_a_marked_photo_gets_no_divider() {
        let chapters: Vec<Option<usize>> = vec![Some(0), Some(0), Some(1), Some(1), Some(1)];
        let plain = flow_pages(&chapters, &[], &[], &[], None, false, 60);
        assert!(plain.iter().any(|p| p.kind == PageKind::Divider));
        let marked = flow_pages(
            &chapters,
            &[],
            &[false, false, true, false, false],
            &[],
            None,
            false,
            60,
        );
        assert!(marked.iter().all(|p| p.kind != PageKind::Divider));
    }

    #[test]
    fn captioned_photos_still_name_their_place() {
        let shot = |cap: &str, place: &str, keep: bool| BookPhoto {
            caption: Some(cap.to_string()),
            place: Some(place.to_string()),
            keep,
            ..BookPhoto::blank()
        };
        let ramen = shot("Best ramen of the trip.", "Canal City Ramen Stadium", true);
        let other = shot("Fukuoka", "Fukuoka", false);
        let (line, cells) = grid_captions(&[&ramen, &other]);
        assert_eq!(cells, [(0, "Best ramen of the trip.".to_string())]);
        assert_eq!(line, ["Canal City Ramen Stadium", "Fukuoka"]);
        // A single captioned photo shows its words, then its place.
        assert_eq!(
            page_caption(&[&ramen], PageKind::PhotoCaption),
            ["Best ramen of the trip.", "Canal City Ramen Stadium"]
        );
    }

    #[test]
    fn a_low_resolution_photo_prints_as_big_as_it_stays_sharp() {
        let small = BookPhoto {
            width: 1536,
            height: 1024,
            ..BookPhoto::blank()
        };
        let big = BookPhoto {
            width: 4032,
            height: 3024,
            ..BookPhoto::blank()
        };
        let cells = vec![(0.0, 0.0, 7.76, 5.36), (7.9, 0.0, 4.0, 5.36)];
        let sharp = sharp_cells(&[&small, &big], cells.clone(), 1.0);
        // Shrunk about its centre to what 1536x1024 fills at 200 dpi.
        let (x, y, w, h) = sharp[0];
        assert!(w < 7.68 && h < 5.12 && w > 7.6, "{:?}", sharp[0]);
        assert!((x + w / 2.0 - 3.88).abs() < 1e-9 && (y + h / 2.0 - 2.68).abs() < 1e-9);
        assert_ne!(dpi_verdict(1536, 1024, w, h), DpiVerdict::Block);
        // A sharp photo keeps its cell.
        assert_eq!(sharp[1], cells[1]);
    }

    #[test]
    fn a_caption_for_the_whole_page_leads_its_bottom_line() {
        let shot = |cap: Option<&str>| BookPhoto {
            caption: cap.map(str::to_string),
            place: Some("Pokémon Center Osaka".to_string()),
            keep: true,
            ..BookPhoto::blank()
        };
        let mut first = shot(Some("Alora posing as the various anime characters."));
        first.caption_for_page = true;
        let (rest, more) = (shot(None), shot(None));
        let (line, cells) = grid_captions(&[&first, &rest, &more]);
        assert!(cells.is_empty(), "{cells:?}");
        assert_eq!(
            line,
            [
                "Alora posing as the various anime characters.",
                "Pokémon Center Osaka"
            ]
        );
        // Two such captions keep their story order.
        let mut second = shot(Some("Then the gift shop."));
        second.caption_for_page = true;
        let (line, _) = grid_captions(&[&first, &second, &more]);
        assert_eq!(
            line[..2],
            [
                "Alora posing as the various anime characters.",
                "Then the gift shop."
            ]
        );
    }

    #[test]
    fn grid_caption_collapses_to_one_dateline() {
        let mk = |cap: &str| BookPhoto {
            asset_id: cap.to_string(),
            path: PathBuf::from("/x.jpg"),
            width: 100,
            height: 100,
            caption: Some(cap.to_string()),
            ..BookPhoto::blank()
        };
        let a = mk("Day 4 · Sun, Oct 5 · 9:00 AM");
        let b = mk("Day 4 · Sun, Oct 5 · 4:32 PM");
        let c = mk("Day 5 · Mon, Oct 6 · 8:15 AM");
        let u = mk("Kids at the market");
        // Same day: one dateline, no repeated per-photo template text.
        assert_eq!(
            page_caption(&[&a, &b], PageKind::Grid),
            vec!["Day 4 · Sun, Oct 5".to_string()]
        );
        // Spanning days: one range line.
        assert_eq!(
            page_caption(&[&a, &c], PageKind::Grid),
            vec!["Sun, Oct 5 – Mon, Oct 6".to_string()]
        );
        // Single-photo pages keep the full caption with time.
        assert_eq!(
            page_caption(&[&b], PageKind::PhotoCaption),
            vec!["Day 4 · Sun, Oct 5 · 4:32 PM".to_string()]
        );
        // User prose is not dateline-collapsed.
        assert_eq!(page_caption(&[&u, &a], PageKind::Grid).len(), 2);
    }

    #[test]
    fn a_grid_names_a_third_place_when_the_line_still_fits() {
        let lines = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let saigon = lines(&[
            "Ho Chi Minh Campaign Museum",
            "War Remnants Museum",
            "Saigon Central Post Office",
        ]);
        assert_eq!(collapse_lines(saigon.clone(), PageKind::Grid), saigon);
        // Too long for one line: the first two.
        let long = lines(&[
            "Final ice cream pose for Alora. She always wanted a photo whenever we saw an ice cream sign.",
            "Carnival Luminosa",
            "Victoria Harbour",
        ]);
        assert_eq!(collapse_lines(long.clone(), PageKind::Grid), long[..2]);
        // The width it is measured against is the narrowest preset's.
        let narrowest = [BookPreset::P1, BookPreset::P2]
            .iter()
            .map(|p| p.trim_inches().0)
            .fold(f64::INFINITY, f64::min);
        assert_eq!(NARROWEST_PAGE_IN, narrowest);
        // Never a fourth.
        let four = lines(&["Tokyo Tower", "Zojo-ji", "Shiba Park", "Atago Shrine"]);
        assert_eq!(collapse_lines(four.clone(), PageKind::Grid), four[..3]);
    }

    #[test]
    fn text_width_follows_each_glyph() {
        // An "M" is wider than an "i"; ten of each tell apart.
        assert!(text_width_em(&"M".repeat(10)) > 3.0 * text_width_em(&"i".repeat(10)));
        // Kanji take a full em; an accented capital is a capital's width.
        assert_eq!(text_width_em("浅草寺"), 3.0);
        assert!(text_width_em("Ō") > text_width_em("ō"));
        // As Typst sets them at 24 pt on a 12 in page (836 pt of banner):
        // the Day 19 heading wraps (870 pt), the Day 21 one fits (819 pt).
        let wraps = "Day 19 · Wed, Oct 21 — Independence Palace · Ho Chi Minh Campaign Museum";
        let fits = "Day 21 · Fri, Oct 23 — Marina Bay Sands · Flower Dome, Gardens by the Bay";
        assert!(!heading_fits_one_line(wraps, 12.0));
        assert!(heading_fits_one_line(fits, 12.0));
    }

    /// `n` photos split evenly into `d` chapters (no chapters when `d` is 0).
    fn even_chapters(n: usize, d: usize) -> Vec<Option<usize>> {
        (0..n).map(|i| (d > 0).then(|| i * d / n)).collect()
    }

    /// One `Some(chapter)` per photo from per-chapter sizes.
    fn sized_chapters(sizes: &[usize]) -> Vec<Option<usize>> {
        sizes
            .iter()
            .enumerate()
            .flat_map(|(c, &n)| std::iter::repeat_n(Some(c), n))
            .collect()
    }

    /// A photo of `w`x`h` px in `chapter`.
    fn sized(i: usize, w: u32, h: u32, chapter: Option<usize>, score: f32) -> BookPhoto {
        BookPhoto {
            asset_id: format!("a{i}"),
            width: w,
            height: h,
            chapter,
            score,
            ..BookPhoto::blank()
        }
    }

    /// The photos behind each page of the P1 book, in consumption order.
    fn pages_of(photos: &[BookPhoto], cap: usize) -> Vec<(PageKind, Vec<usize>)> {
        let flow = book_flow(photos, false, cap);
        let order = consumption_order(photos, &flow, BookPreset::P1);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert!(sorted.iter().copied().eq(0..photos.len()), "a permutation");
        let mut cursor = 0;
        flow.iter()
            .map(|p| {
                let taken = order[cursor..cursor + p.take].to_vec();
                cursor += p.take;
                (p.kind, taken)
            })
            .collect()
    }

    /// The chapters behind each page: (kind, chapter of each consumed photo).
    /// Every photo is too small for any sharp slot, so pages keep story
    /// order; `wide_idx` is the only wide photo.
    fn pages_with_chapters(
        chapter_of: &[Option<usize>],
        wide_idx: Option<usize>,
        cap: usize,
    ) -> Vec<(PageKind, Vec<Option<usize>>)> {
        let photos: Vec<BookPhoto> = chapter_of
            .iter()
            .enumerate()
            .map(|(i, &c)| {
                let w = if Some(i) == wide_idx { 200 } else { 100 };
                sized(i, w, 100, c, NEUTRAL_SCORE)
            })
            .collect();
        assert_eq!(wide_index(&photos), wide_idx, "wide photo in the window");
        chapter_pages_of(&photos, cap)
    }

    /// [`pages_of`] with each consumed photo replaced by its chapter.
    fn chapter_pages_of(photos: &[BookPhoto], cap: usize) -> Vec<(PageKind, Vec<Option<usize>>)> {
        pages_of(photos, cap)
            .into_iter()
            .map(|(k, taken)| (k, taken.iter().map(|&i| photos[i].chapter).collect()))
            .collect()
    }

    #[test]
    fn dividers_interleave_never_stack() {
        let kinds: Vec<PageKind> =
            flow_pages(&even_chapters(200, 20), &[], &[], &[], None, false, 60)
                .iter()
                .map(|p| p.kind)
                .collect();
        let divs: Vec<usize> = kinds
            .iter()
            .enumerate()
            .filter(|(_, k)| **k == PageKind::Divider)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(divs.len(), MAX_DIVIDERS, "dividers capped: {divs:?}");
        assert!(
            divs.windows(2).all(|w| w[1] > w[0] + 1),
            "dividers never consecutive: {divs:?}"
        );
        assert_eq!(cover_grid(1), (1, 1));
        assert_eq!(cover_grid(4), (2, 2));
        assert_eq!(cover_grid(12), (4, 3));
        // Consumption accounting: dividers take their background photo.
        let flow = flow_pages(&even_chapters(200, 20), &[], &[], &[], None, false, 60);
        let takes: usize = flow.iter().map(|p| p.take).sum();
        assert_eq!(takes, 200, "every photo placed exactly once");
        // Small books and divider-heavy trips: still every photo, once.
        for n in 0..40 {
            for d in 0..10 {
                for wide in [None, Some(1)] {
                    let wide = wide.filter(|_| n > 1);
                    let flow = flow_pages(&even_chapters(n, d), &[], &[], &[], wide, false, 60);
                    let takes: usize = flow.iter().map(|p| p.take).sum();
                    assert_eq!(takes, n, "n={n} d={d} wide={wide:?}");
                }
            }
        }
    }

    #[test]
    fn spread_wide_photo_comes_from_the_opening_only() {
        let bp = |w: u32, h: u32| BookPhoto {
            asset_id: format!("{w}x{h}"),
            path: PathBuf::from("/x.jpg"),
            width: w,
            height: h,
            ..BookPhoto::blank()
        };
        // Wide shot at index 20 (a later chapter): no spread.
        let mut photos: Vec<BookPhoto> = (0..25).map(|_| bp(100, 100)).collect();
        photos[20] = bp(200, 100);
        assert_eq!(wide_index(&photos), None);
        // Window boundary: the last eligible index is chosen, the next is not.
        let mut edge: Vec<BookPhoto> = (0..25).map(|_| bp(100, 100)).collect();
        edge[SPREAD_WINDOW + 1] = bp(200, 100);
        assert_eq!(wide_index(&edge), None);
        edge[SPREAD_WINDOW] = bp(200, 100);
        assert_eq!(wide_index(&edge), Some(SPREAD_WINDOW));
        // Wide shot inside the opening stretch: chosen.
        photos[5] = bp(200, 100);
        assert_eq!(wide_index(&photos), Some(5));
        // The hero (index 0) is never the spread.
        let mut first_wide: Vec<BookPhoto> = (0..15).map(|_| bp(100, 100)).collect();
        first_wide[0] = bp(200, 100);
        assert_eq!(wide_index(&first_wide), None);
        // A wide shot inside the window but past the hero's chapter (the
        // Tokyo panorama after the departure-day photos): no spread, so
        // the departure day stays ahead of Tokyo.
        let mut crossing: Vec<BookPhoto> = (0..15)
            .map(|i| BookPhoto {
                chapter: Some(usize::from(i >= 4)),
                ..bp(100, 100)
            })
            .collect();
        crossing[5] = BookPhoto {
            chapter: Some(1),
            ..bp(200, 100)
        };
        assert_eq!(wide_index(&crossing), None);
        crossing[3] = BookPhoto {
            chapter: Some(0),
            ..bp(200, 100)
        };
        assert_eq!(wide_index(&crossing), Some(3));
    }

    #[test]
    fn dividers_open_each_chapter_at_its_first_photo() {
        // Uneven chapters: even spacing would stack dividers in the long
        // first chapter and leave the short ones with no title.
        let chapter_of = sized_chapters(&[30, 2, 1, 30]);
        let pages = pages_with_chapters(&chapter_of, None, 60);
        let mut seen: HashSet<usize> = chapter_of[0].into_iter().collect();
        let mut current = chapter_of[0];
        let mut divided = vec![];
        for (kind, taken) in &pages {
            if *kind == PageKind::Divider {
                let c = taken[0].unwrap();
                assert!(seen.insert(c), "chapter {c} opened twice");
                divided.push(c);
                current = taken[0];
            } else {
                assert!(
                    taken.iter().all(|c| *c == current),
                    "{kind:?} page crosses a chapter: {taken:?}"
                );
            }
        }
        assert_eq!(
            divided,
            [1, 2, 3],
            "each later chapter opens once, in order"
        );

        // Dense enough for grids: chapter runs still stay separate and fit.
        let chapter_of = sized_chapters(&[120, 3, 1, 90]);
        let pages = pages_with_chapters(&chapter_of, None, 60);
        assert!(pages.iter().any(|(k, _)| *k == PageKind::Grid));
        assert!(pages.len() <= 61, "cap honored: {}", pages.len());
        let mut current = chapter_of[0];
        for (kind, taken) in &pages {
            if *kind == PageKind::Divider {
                current = taken[0];
            } else {
                assert!(taken.iter().all(|c| *c == current), "{taken:?}");
            }
        }
    }

    #[test]
    fn chapters_without_placed_photos_get_no_divider() {
        // Chapter 1 has no photo here, and the hero opens chapter 0.
        let chapter_of = [Some(0), Some(0), Some(2), Some(2)];
        let flow = flow_pages(&chapter_of, &[], &[], &[], None, false, 60);
        let dividers = flow.iter().filter(|p| p.kind == PageKind::Divider).count();
        assert_eq!(dividers, 1);
        // The spread comes from the hero's chapter, so it never uses up a
        // later chapter's photo: chapters 1 and 2 both keep their dividers.
        let chapter_of = sized_chapters(&[4, 1, 3]);
        let pages = pages_with_chapters(&chapter_of, Some(2), 60);
        assert!(pages.iter().any(|(k, _)| *k == PageKind::SpreadLeft));
        let divided: Vec<Option<usize>> = pages
            .iter()
            .filter(|(k, _)| *k == PageKind::Divider)
            .map(|(_, taken)| taken[0])
            .collect();
        assert_eq!(divided, [Some(1), Some(2)]);
    }

    #[test]
    fn divider_limit_spreads_across_chapters_at_their_first_photos() {
        let chapter_of = sized_chapters(&[5; 20]);
        let pages = pages_with_chapters(&chapter_of, None, 60);
        let divided: Vec<usize> = pages
            .iter()
            .filter(|(k, _)| *k == PageKind::Divider)
            .map(|(_, taken)| taken[0].unwrap())
            .collect();
        assert_eq!(divided.len(), MAX_DIVIDERS);
        assert!(divided.windows(2).all(|w| w[0] < w[1]));
        assert!(
            *divided.last().unwrap() >= 15,
            "spans the trip: {divided:?}"
        );
    }

    /// Pages of a flow carry the chapters they consume: each divider opens
    /// its own chapter, no photo page crosses a divider, and chapters past
    /// [`MAX_DIVIDERS`] still follow in story order.
    fn assert_chapter_aware(pages: &[(PageKind, Vec<Option<usize>>)], spread: bool, ctx: &str) {
        let skip = 2 + usize::from(spread); // cover-proxy, hero, spread
        let mut body: Vec<Option<usize>> = vec![];
        for (i, (kind, taken)) in pages.iter().enumerate().skip(skip) {
            body.extend(taken);
            if *kind != PageKind::Divider {
                continue;
            }
            let c = taken[0];
            // Photo pages up to the next divider: they open with this
            // chapter and never reach a chapter a later divider opens.
            let run: Vec<Option<usize>> = pages[i + 1..]
                .iter()
                .take_while(|(k, _)| *k != PageKind::Divider)
                .flat_map(|(_, t)| t.iter().copied())
                .collect();
            let next = pages[i + 1..]
                .iter()
                .find(|(k, _)| *k == PageKind::Divider)
                .map(|(_, t)| t[0]);
            if let Some(&first) = run.first() {
                assert_eq!(first, c, "{ctx}: divider {c:?} opens {run:?}");
            }
            assert!(
                run.iter().all(|r| *r >= c && next.is_none_or(|n| *r < n)),
                "{ctx}: run after divider {c:?} (next {next:?}) crosses: {run:?}"
            );
        }
        assert!(
            body.windows(2).all(|w| w[0] <= w[1]),
            "{ctx}: chapters out of story order: {body:?}"
        );
    }

    #[test]
    fn flow_is_chapter_aware_for_random_layouts() {
        // Small xorshift: no rand crate, reproducible cases.
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut rnd = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        // 48 MP, 12 MP, a cropped edit, a square: sharp in different slots,
        // so the sharpness picks move photos within their chapters.
        let dims = [(5712, 4284), (4032, 3024), (1708, 1280), (3000, 3000)];
        for case in 0..400 {
            let n_chapters = 1 + rnd(14);
            let sizes: Vec<usize> = (0..n_chapters)
                .map(|_| if rnd(4) == 0 { 1 } else { 1 + rnd(40) })
                .collect();
            let chapter_of = sized_chapters(&sizes);
            let wide = (rnd(2) == 0 && chapter_of.len() > 1)
                .then(|| 1 + rnd(SPREAD_WINDOW.min(chapter_of.len() - 1)));
            let cap = 24 + rnd(120);
            let photos: Vec<BookPhoto> = chapter_of
                .iter()
                .enumerate()
                .map(|(i, &c)| {
                    let (w, h) = if Some(i) == wide {
                        (6000, 3000)
                    } else {
                        dims[rnd(dims.len())]
                    };
                    sized(i, w, h, c, rnd(100) as f32 / 100.0)
                })
                .collect();
            let pages = chapter_pages_of(&photos, cap);
            let spread = pages.iter().any(|(k, _)| *k == PageKind::SpreadLeft);
            let ctx = format!("case {case} sizes {sizes:?} wide {wide:?} cap {cap}");
            assert_chapter_aware(&pages, spread, &ctx);
            let taken: usize = pages.iter().map(|(_, t)| t.len()).sum();
            assert_eq!(taken, chapter_of.len(), "{ctx}: every photo once");
        }
    }

    /// Placements below 300 dpi at their slot in the P1 book.
    fn soft_placements(photos: &[BookPhoto], cap: usize) -> usize {
        pages_of(photos, cap)
            .iter()
            .filter(|(k, _)| *k != PageKind::CoverProxy)
            .flat_map(|(k, taken)| {
                let slot: Vec<&BookPhoto> = taken.iter().map(|&i| &photos[i]).collect();
                slot_sizes(BookPreset::P1, *k, &slot)
                    .into_iter()
                    .zip(taken)
                    .map(|((w, h), &i)| dpi_verdict(photos[i].width, photos[i].height, w, h))
                    .collect::<Vec<_>>()
            })
            .filter(|v| *v != DpiVerdict::Ok)
            .count()
    }

    #[test]
    fn a_day_title_lists_its_places_as_they_came() {
        let at = |place: &str| BookPhoto {
            place: Some(place.to_string()),
            ..BookPhoto::blank()
        };
        let (tokyo, shrine) = (at("Tokyo"), at("Tsurugaoka Hachimangū"));
        let title = "Day 5 · Wed, Oct 7 — Tsurugaoka Hachimangū";
        assert_eq!(
            places_in_time_order(title, &[&tokyo, &shrine, &shrine]),
            "Day 5 · Wed, Oct 7 — Tokyo · Tsurugaoka Hachimangū"
        );
        // The title's place not on the page: the page's own place instead,
        // and a page with no placed photo keeps the title.
        assert_eq!(
            places_in_time_order(title, &[&tokyo]),
            "Day 5 · Wed, Oct 7 — Tokyo"
        );
        assert_eq!(
            places_in_time_order(title, &[&BookPhoto::blank()]),
            "Day 5 · Wed, Oct 7"
        );
        let boarding = at("Carnival Luminosa Embarkation");
        let mut ship = at("Carnival Luminosa");
        ship.aboard = true;
        assert_eq!(
            places_in_time_order("Day 7 — Carnival Luminosa Embarkation", &[&boarding, &ship]),
            "Day 7 — Carnival Luminosa Embarkation"
        );
        // A port morning on board, then ashore: the ship, then the port.
        let mut ship = at("Carnival Luminosa");
        ship.aboard = true;
        let temple = at("Nanzoin Temple");
        assert_eq!(
            places_in_time_order("Day 11 · Tue, Oct 13 — Fukuoka", &[&ship, &ship, &temple]),
            "Day 11 · Tue, Oct 13 — Carnival Luminosa · Fukuoka · Nanzoin Temple"
        );
        // A heading naming two places already gains the day's others.
        let market = at("Tsukiji Outer Market");
        let ginza = at("Togoshi Ginza");
        assert_eq!(
            places_in_time_order(
                "Day 6 · Thu, Oct 8 — Tokyo · Tsukiji Outer Market",
                &[&market, &ginza]
            ),
            "Day 6 · Thu, Oct 8 — Tokyo · Tsukiji Outer Market · Togoshi Ginza"
        );
        // A ship-and-port title over a page shot only on board: the ship
        // first, as on every other page.
        assert_eq!(
            places_in_time_order(
                "Day 11 · Tue, Oct 13 — Carnival Luminosa · Fukuoka",
                &[&ship]
            ),
            "Day 11 · Tue, Oct 13 — Carnival Luminosa · Fukuoka"
        );
        assert_eq!(places_in_time_order("Day 5", &[&tokyo]), "Day 5");
        // A city leads its sights, whatever order the photos came in.
        let jewel = at("Jewel Changi Airport");
        let mut singapore = at("Singapore");
        singapore.place_is_city = true;
        assert_eq!(
            places_in_time_order("Day 28 — Jewel Changi Airport", &[&jewel, &singapore]),
            "Day 28 — Singapore · Jewel Changi Airport"
        );
        // A placeless photo does not shift where the port goes, and a spot
        // on board (a deck) is not listed after the ship.
        let mut deck = at("Lido Deck");
        deck.aboard = true;
        let blank = BookPhoto::blank();
        assert_eq!(
            places_in_time_order(
                "Day 11 · Tue, Oct 13 — Fukuoka",
                &[&ship, &blank, &deck, &temple]
            ),
            "Day 11 · Tue, Oct 13 — Carnival Luminosa · Fukuoka · Nanzoin Temple"
        );
    }

    #[test]
    fn a_heading_too_long_for_one_line_drops_the_city_the_page_does_not_show() {
        let at = |place: &str| BookPhoto {
            place: Some(place.to_string()),
            ..BookPhoto::blank()
        };
        let (durian, dome) = (
            at("Marina Bay Sands"),
            at("Flower Dome, Gardens by the Bay"),
        );
        let slot = [&durian, &dome];
        let heading = places_in_time_order(
            "Day 21 · Fri, Oct 23 — Singapore · Flower Dome, Gardens by the Bay",
            &slot,
        );
        assert_eq!(
            heading,
            "Day 21 · Fri, Oct 23 — Singapore · Marina Bay Sands · Flower Dome, Gardens by the Bay"
        );
        assert_eq!(
            one_line_heading(heading, &slot, 12.0),
            "Day 21 · Fri, Oct 23 — Marina Bay Sands · Flower Dome, Gardens by the Bay"
        );
        // One that fits keeps its city.
        let tokyo = "Day 6 · Thu, Oct 8 — Tokyo · Tsukiji Outer Market";
        assert_eq!(
            one_line_heading(tokyo.to_string(), &[&at("Tsukiji Outer Market")], 12.0),
            tokyo
        );
        // The page's own places still too long for the 12 in book: the
        // first visited, as many as fit.
        let (palace, museum) = (at("Independence Palace"), at("Ho Chi Minh Campaign Museum"));
        assert_eq!(
            one_line_heading(
                "Day 19 · Wed, Oct 21 — Independence Palace · Ho Chi Minh Campaign Museum"
                    .to_string(),
                &[&palace, &museum],
                12.0
            ),
            "Day 19 · Wed, Oct 21 — Independence Palace"
        );
        // A single place stays, however long.
        let long = "Day 19 · Wed, Oct 21 — Ho Chi Minh Campaign Museum and War Remnants Museum";
        assert_eq!(one_line_heading(long.to_string(), &[&museum], 12.0), long);
    }

    #[test]
    fn pages_are_checked_for_time_order() {
        let at = |t: i64| BookPhoto {
            taken_at: Some(t),
            ..BookPhoto::blank()
        };
        let (a, b, c) = (at(1_000), at(5_000), at(9_000));
        let grid = [
            (0.0, 0.0, 5.0, 5.0),
            (5.0, 0.0, 5.0, 5.0),
            (0.0, 5.0, 10.0, 5.0),
        ];
        let mut last = None;
        assert!(time_order_notes(1, &[&a, &b, &c], &grid, &mut last).is_empty());
        // The later photo top left: a break.
        let mut last = None;
        assert_eq!(
            time_order_notes(2, &[&b, &a, &c], &grid, &mut last).len(),
            1
        );
        // A page that starts before the previous one ended.
        assert_eq!(time_order_notes(3, &[&a], &[grid[0]], &mut last).len(), 1);
        // A burst a minute apart is the same moment.
        let mut last = None;
        let d = at(1_060);
        assert!(time_order_notes(4, &[&d, &a], &grid[..2], &mut last).is_empty());
    }

    #[test]
    fn pinned_photos_sharing_a_place_name_all_stay() {
        let pin = |i: usize| BookPhoto {
            asset_id: format!("p{i}"),
            caption: Some("Tokyo".into()),
            place: Some("Tokyo".into()),
            day: Some(1),
            keep: true,
            pinned: true,
            ..BookPhoto::blank()
        };
        let photos: Vec<BookPhoto> = (0..4).map(pin).collect();
        // One scene, one place, more than the day's caption cap: all kept.
        assert_eq!(distinct_curated(&photos, &[0; 4]), [0, 1, 2, 3]);
        // Their place name is not the owner's words.
        assert!(!photos[0].own_caption());
        let worded = BookPhoto {
            caption: Some("Our first ramen".into()),
            ..pin(0)
        };
        assert!(worded.own_caption());
    }

    #[test]
    fn only_captioned_cells_keep_a_band_sized_to_their_words() {
        let cell = (0.0, 0.0, 0.5, 0.5);
        let short = (0, cell, "Tea".to_string());
        let long = (2, cell, "Tea ".repeat(40));
        let bars = caption_bars(&[short, long], 3, BookPreset::P1, 3600);
        assert_eq!(bars.len(), 3);
        assert!(bars[0] > 0 && bars[1] == 0, "{bars:?}");
        assert!(bars[2] > bars[0], "longer words wrap: {bars:?}");
    }

    #[test]
    fn a_lone_photo_of_no_one_gives_up_its_page() {
        let shot = |id: &str, day: u32, of_people: bool, keep: bool| BookPhoto {
            asset_id: id.to_string(),
            day: Some(day),
            tagged_people: u32::from(of_people),
            keep,
            ..BookPhoto::blank()
        };
        let photos = [
            shot("a", 1, true, false),
            shot("b", 1, false, false),
            shot("food", 2, false, false),
            shot("kid", 3, true, false),
            shot("menu", 4, false, true),
        ];
        assert_eq!(lone_scenery(&photos), ["food"]);
    }

    #[test]
    fn the_owners_own_page_is_neither_dropped_nor_folded() {
        let shot = |id: &str, area: &str, keep: bool, page_group: bool| BookPhoto {
            asset_id: id.to_string(),
            day: Some(6),
            area: Some(area.into()),
            keep,
            page_group,
            ..BookPhoto::blank()
        };
        let mut photos = vec![
            shot("statue", "Kirby Cafe \u{b7} own page statue", false, true),
            shot("plate", "Kirby Cafe \u{b7} own page plate", true, true),
            shot("alley", "Shinsaibashi-suji", false, false),
            shot("puffs", "Shinsaibashi-suji", false, false),
        ];
        assert!(lone_scenery(&photos).is_empty());
        fold_lone_kept(&mut photos);
        assert_eq!(
            photos[1].area.as_deref(),
            Some("Kirby Cafe \u{b7} own page plate")
        );
    }

    #[test]
    fn the_page_count_sees_only_photos_that_print_on_pages() {
        let shot = |id: &str, day: u32, of_people: bool, closing: bool| BookPhoto {
            asset_id: id.to_string(),
            day: Some(day),
            tagged_people: u32::from(of_people),
            closing,
            ..BookPhoto::blank()
        };
        let picked = vec![
            shot("a", 1, true, false),
            shot("b", 1, false, false),
            shot("food", 2, false, false),
            shot("end", 3, true, true),
        ];
        let ids: Vec<String> = printed_pool(picked, Some("end"))
            .into_iter()
            .map(|p| p.asset_id)
            .collect();
        assert_eq!(ids, ["a", "b"]);
    }

    /// [`face_window_clear`] with no caption strip.
    fn face_window(
        sw: f64,
        sh: f64,
        tw: f64,
        th: f64,
        faces: &[FaceBox],
    ) -> ((f64, f64, f64, f64), bool) {
        face_window_clear(sw, sh, tw, th, faces, 0.0)
    }

    #[test]
    fn framing_trims_head_room_before_it_slices_a_face() {
        // IMG_6872: three family faces stacked down a 4032x3024 selfie, in
        // a wide cell that can't hold all of them with head room. Every
        // face stays whole; only spare room goes.
        let fb = |x: f32, y: f32, w: f32, h: f32| KnownBox {
            b: FaceBox { x, y, w, h },
            body: false,
        };
        let p = BookPhoto {
            width: 4032,
            height: 3024,
            family: vec![
                fb(0.15, 0.37, 0.12, 0.16),
                fb(0.69, 0.59, 0.19, 0.25),
                fb(0.31, 0.82, 0.08, 0.10),
            ],
            ..BookPhoto::blank()
        };
        let ((_, y, _, h), cut) = frame_photo(&p, (4032.0, 3024.0), (2.3, 1.0), &[], 0.0);
        let (top, bottom) = (y / 3024.0, (y + h) / 3024.0);
        assert!(
            top <= 0.37 && bottom >= 0.92,
            "all three faces: {top:.3}..{bottom:.3}"
        );
        assert!(!cut);
    }

    #[test]
    fn framing_keeps_the_family_over_strangers() {
        let fb = |x: f32, y: f32, side: f32| FaceBox {
            x,
            y,
            w: side,
            h: side * 1.33,
        };
        // A wide 4000x3000 photo into a square cell: Lana at the far left,
        // three strangers at the right.
        let p = BookPhoto {
            width: 4000,
            height: 3000,
            family: vec![KnownBox {
                b: fb(0.05, 0.4, 0.08),
                body: false,
            }],
            ..BookPhoto::blank()
        };
        let strangers = [
            fb(0.75, 0.4, 0.06),
            fb(0.82, 0.4, 0.06),
            fb(0.89, 0.4, 0.06),
        ];
        let ((x, _, w, _), cut) = frame_photo(&p, (4000.0, 3000.0), (1.0, 1.0), &strangers, 0.0);
        assert!(
            x <= 0.05 * 4000.0 && x + w >= 0.13 * 4000.0,
            "Lana in: x={x}"
        );
        assert!(!cut);
        // Two family members too far apart for the cell: one stays whole,
        // the other is left out rather than sliced, and the cut is noted.
        let p2 = BookPhoto {
            family: vec![
                KnownBox {
                    b: fb(0.05, 0.4, 0.08),
                    body: false,
                },
                KnownBox {
                    b: fb(0.85, 0.4, 0.08),
                    body: false,
                },
            ],
            ..p.clone()
        };
        let targets = framing_targets(&p2, &[]);
        let ((x, _, w, _), cut) = framing_window(4000.0, 3000.0, 1.0, 1.0, &targets, 0.0);
        let whole = |b: &FaceBox| {
            let (x0, x1) = (f64::from(b.x) * 4000.0, f64::from(b.x + b.w) * 4000.0);
            (x0 >= x && x1 <= x + w) || x1 <= x || x0 >= x + w
        };
        assert!(
            targets.iter().all(|t| whole(&t.core)),
            "nobody sliced: x={x}"
        );
        assert!(cut, "someone left out");
    }

    #[test]
    fn a_panorama_spans_its_row() {
        let photos: Vec<BookPhoto> = [(5712, 4284), (4284, 5712), (16242, 3854), (5712, 4284)]
            .iter()
            .enumerate()
            .map(|(i, &(w, h))| sized(i, w, h, None, NEUTRAL_SCORE))
            .collect();
        let slot: Vec<&BookPhoto> = photos.iter().collect();
        let cells = grid_layout(&slot, 100.0, 100.0, 1.0);
        let (_, _, w, h) = cells[2];
        assert!(w > 97.0, "full width: {cells:?}");
        assert!(
            (w / h - 16242.0 / 3854.0).abs() < 0.05,
            "uncropped: {cells:?}"
        );
    }

    #[test]
    fn crops_keep_faces_and_their_heads() {
        let face = |x: f32, y: f32, side: f32| FaceBox {
            x,
            y,
            w: side,
            h: side,
        };
        // A portrait 3000x4000 into a 2:1 cell keeps a 3000x1500 band; a
        // face near the top keeps its head instead of the centred band.
        let ((x, y, w, h), cut) = face_window(3000.0, 4000.0, 2.0, 1.0, &[face(0.4, 0.1, 0.15)]);
        assert_eq!((x, w, h), (0.0, 3000.0, 1500.0));
        assert!(
            y <= (0.1 - 0.15 * HEAD_ABOVE) as f64 * 4000.0,
            "head room: y={y}"
        );
        assert!(!cut);
        // A selfie face at the right edge of a wide photo in a square cell.
        let ((x, _, w, _), cut) = face_window(4000.0, 3000.0, 1.0, 1.0, &[face(0.85, 0.3, 0.12)]);
        assert!(x + w >= 0.97 * 4000.0, "right side kept: x={x} w={w}");
        assert!(!cut);
        // Two faces too far apart for the cell: the bigger stays, noted.
        let ((x, _, w, _), cut) = face_window(
            6000.0,
            3000.0,
            1.0,
            1.0,
            &[face(0.02, 0.4, 0.05), face(0.8, 0.4, 0.15)],
        );
        assert!(x + w >= 0.95 * 6000.0, "the big face's side: x={x}");
        assert!(cut);
        // Faces padded a fraction of a pixel wider than the window still
        // count as kept, and place them without panicking.
        let ((x, _, w, _), cut) =
            face_window(4000.0, 3000.0, 1.0, 1.0, &[face(0.15, 0.33, 0.5358)]);
        assert_eq!(w, 3000.0);
        assert!((0.0..=1000.0).contains(&x), "in the photo: x={x}");
        assert!(!cut);
        // A close-up whose padded face is taller than a wide cell's band
        // stays the centre even with a small face elsewhere.
        let ((_, y, _, h), cut) = face_window(
            3000.0,
            4000.0,
            2.0,
            1.0,
            &[face(0.3, 0.4, 0.4), face(0.05, 0.02, 0.04)],
        );
        let mid = (0.4 - 0.4 * HEAD_ABOVE + 0.4 + 0.4 + 0.4 * FACE_BELOW) as f64 / 2.0 * 4000.0;
        assert!((y + h / 2.0 - mid).abs() < 1.0, "on the close-up: y={y}");
        assert!(cut);
        // A close-up whose head room is taller than the cell: the face
        // still stays whole, with what head room fits.
        let ((_, y, _, h), cut) = face_window(3000.0, 4000.0, 2.0, 1.0, &[face(0.3, 0.15, 0.25)]);
        assert!(
            y <= 0.15 * 4000.0 && y + h >= 0.4 * 4000.0,
            "face kept: y={y}"
        );
        assert!(!cut);
        // No faces: centred, as before.
        let ((x, y, _, _), cut) = face_window(4000.0, 3000.0, 1.0, 1.0, &[]);
        assert_eq!((x, y, cut), (500.0, 0.0, false));
    }

    #[test]
    fn each_photo_meets_the_face_detector_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jpg");
        RgbImage::new(64, 48).save(&path).unwrap();
        let p = BookPhoto {
            asset_id: "a".into(),
            path,
            width: 64,
            height: 48,
            ..BookPhoto::blank()
        };
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let finder = FaceFinder(std::sync::Arc::new(move |_: &RgbImage| {
            counted.fetch_add(1, Ordering::SeqCst);
            vec![FaceBox {
                x: 0.8,
                y: 0.4,
                w: 0.1,
                h: 0.1,
            }]
        }));
        let fit = Fit {
            ffmpeg: "ffmpeg",
            faces: Some(&finder),
            found: Default::default(),
        };
        let mut fb = vec![];
        // The owner's focus places the crop: no detector run.
        fit_photo_focused(&p, Some((0.5, 0.5)), (10, 10), 0.0, false, &fit, &mut fb);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        // A spread's two halves and a cover tile: one detector run.
        for (w, h) in [(40, 20), (40, 20), (10, 10)] {
            let img = fit_photo(&p, w, h, &fit, &mut fb);
            assert_eq!(img.dimensions(), (w, h));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(fb.is_empty(), "{fb:?}");
    }

    #[test]
    fn no_photo_is_left_alone_on_a_page() {
        assert_eq!(even_split(9, 4), vec![3, 3, 3], "not 4 + 4 + 1");
        assert_eq!(even_split(5, 4), vec![3, 2]);
        assert_eq!(even_split(8, 4), vec![4, 4]);
        assert_eq!(even_split(1, 4), vec![1], "a lone photo keeps its page");
        assert_eq!(even_split(3, 1), vec![1, 1, 1]);
        assert!(even_split(0, 4).is_empty());
        // A day of 9 photos in a book dense enough for grids: no solo page.
        let flow = flow_pages(&[Some(0); 9], &[Some(1); 9], &[], &[], None, false, 4);
        assert!(
            flow.iter().all(|p| p.kind != PageKind::PhotoCaption),
            "{:?}",
            flow.iter().map(|p| (p.kind, p.take)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn full_pages_prefer_sharp_photos() {
        // 12 MP prints at 252 dpi full-page; 48 MP at 357.
        let (mp12, mp48) = ((4032, 3024), (5712, 4284));
        let book = |sharp: &[(usize, f32)]| -> Vec<BookPhoto> {
            (0..15)
                .map(|i| {
                    let chapter = Some(usize::from(i >= 10));
                    match sharp.iter().find(|(j, _)| *j == i) {
                        Some(&(_, score)) => sized(i, mp48.0, mp48.1, chapter, score),
                        None => sized(i, mp12.0, mp12.1, chapter, NEUTRAL_SCORE),
                    }
                })
                .collect()
        };
        // Chapter 0 is photos 0-9, chapter 1 is 10-14. Photo 9 scores best
        // but lies past the hero window.
        let photos = book(&[(3, 0.6), (5, 0.9), (9, 1.0), (12, 0.7)]);
        let pages = pages_of(&photos, 60);
        assert_eq!(pages[1], (PageKind::Hero, vec![5]), "best sharp opener");
        assert_eq!(pages[2].1, [0], "the first photo follows the hero");
        let divider = pages.iter().find(|(k, _)| *k == PageKind::Divider);
        // Soft (252 dpi) still prints: the chapter's earliest photo backs
        // it, so the title page keeps time order.
        assert_eq!(divider.unwrap().1, [10], "the earliest printable photo");
        let after: Vec<usize> = pages
            .iter()
            .skip_while(|(k, _)| *k != PageKind::Divider)
            .skip(1)
            .flat_map(|(_, t)| t.iter().copied())
            .collect();
        assert_eq!(after, [11, 12, 13, 14], "the rest keep story order");
        assert!(soft_placements(&photos, 60) < soft_placements(&book(&[]), 60));
        // No sharp candidate: today's choice, the chapters' first photos.
        let pages = pages_of(&book(&[(9, 1.0)]), 60);
        assert_eq!(pages[1].1, [0]);
        let divider = pages.iter().find(|(k, _)| *k == PageKind::Divider);
        assert_eq!(divider.unwrap().1, [10]);
    }

    #[test]
    fn soft_photos_move_from_wide_cells_to_small_ones() {
        // 2560x1800 prints at ~218 dpi in a 2-up cell (11.8x5.8 in), ~309
        // in a 4-up one (5.8x5.8 in); 48 MP is sharp everywhere, the hero
        // page included.
        let book = |n: usize, soft: usize| -> Vec<BookPhoto> {
            (0..n)
                .map(|i| {
                    let (w, h) = if i == soft {
                        (2560, 1800)
                    } else {
                        (5712, 4284)
                    };
                    sized(i, w, h, None, NEUTRAL_SCORE)
                })
                .collect()
        };
        // Hero plus 130: a day of 4-up pages, then a day of two shots on a
        // closing 2-up holding the soft one, which must not leave its day.
        let mut photos = book(131, 130);
        for (i, p) in photos.iter_mut().enumerate() {
            p.day = Some(if i < 129 { 1 } else { 2 });
        }
        let pages = pages_of(&photos, 40);
        let (last_kind, last) = &pages[pages.len() - 2];
        assert_eq!((*last_kind, last.len()), (PageKind::Grid, 2));
        assert!(last.contains(&130), "no other day's page takes it");
        assert_eq!(soft_placements(&photos, 40), 1);
        // One day: a shot from the same moment on a 4-up page takes it...
        let at = |gap: i64| {
            let mut photos = book(131, 130);
            for (i, p) in photos.iter_mut().enumerate() {
                p.taken_at = Some(i as i64 * gap);
            }
            photos
        };
        assert_eq!(soft_placements(&at(10), 40), 0, "a burst swaps");
        // ...but not one from an hour earlier: that would break time order.
        assert_eq!(soft_placements(&at(3_600), 40), 1, "hours apart stay");
        // All pages 2-up: no small cell to move to, so it stays (and warns).
        let photos = book(21, 20);
        let pages = pages_of(&photos, 24);
        assert!(pages
            .iter()
            .all(|(k, t)| *k != PageKind::Grid || t.len() <= 2));
        assert_eq!(soft_placements(&photos, 24), 1);
    }

    #[test]
    fn bottom_row_captions_clear_the_page_strip() {
        // One line of 14 pt with 8 pt inset on a 12 in page.
        let strip = strip_height_pt("Taipei · Carnival Luminosa", 14, 8, 12.0);
        assert!((strip - (14.0 * 1.3 + 16.0)).abs() < 1e-9);
        // Long text wraps to more lines.
        assert!(strip_height_pt(&"word ".repeat(80), 14, 8, 12.0) > 2.0 * strip);
        let lifted = cell_caption_typ(
            (12.0, 12.0),
            (12.0, 12.0),
            (0.0, 0.5, 0.5, 0.49),
            strip,
            "Tea",
        );
        assert!(
            lifted.contains(&format!("dy: {:.1}pt", -6.0 - strip)),
            "{lifted}"
        );
        let level = cell_caption_typ(
            (12.0, 12.0),
            (12.0, 12.0),
            (0.0, 0.0, 0.5, 0.49),
            0.0,
            "Tea",
        );
        assert!(level.contains("dy: -6.0pt"), "{level}");
    }

    #[test]
    fn pages_never_mix_two_named_places() {
        // One chapter, one day: a walk in, the Flower Dome, the Cloud Forest.
        let areas = [
            None,
            None,
            Some("Flower Dome"),
            Some("Flower Dome"),
            Some("Flower Dome"),
            Some("Cloud Forest"),
            Some("Cloud Forest"),
            Some("Cloud Forest"),
            Some("Cloud Forest"),
        ];
        let photos: Vec<BookPhoto> = (0..60)
            .map(|i| BookPhoto {
                day: Some(1),
                area: areas[i % 9].filter(|_| i >= 30).map(String::from),
                ..sized(i, 4032, 3024, Some(0), NEUTRAL_SCORE)
            })
            .collect();
        for (kind, taken) in pages_of(&photos, 24) {
            let on_page: HashSet<Option<&str>> =
                taken.iter().map(|&i| photos[i].area.as_deref()).collect();
            assert!(on_page.len() <= 1, "{kind:?} mixes {on_page:?}");
        }
    }

    #[test]
    fn a_row_page_lays_its_photos_side_by_side_full_height() {
        let door = |row: bool| BookPhoto {
            width: 3024,
            height: 4032,
            row,
            ..BookPhoto::blank()
        };
        let doors = [door(true), door(true), door(true)];
        let cells = grid_layout(&doors.iter().collect::<Vec<_>>(), 12.0, 12.0, 0.2);
        // One row: every cell starts at the top margin and runs the page.
        assert!(cells
            .iter()
            .all(|c| (c.1 - 0.2).abs() < 1e-9 && (c.3 - 11.6).abs() < 1e-9));
        assert!(cells[0].0 < cells[1].0 && cells[1].0 < cells[2].0);
        // A door cropped out of a hallway panorama gets the same width.
        let hall = BookPhoto {
            width: 9840,
            height: 3726,
            row: true,
            ..BookPhoto::blank()
        };
        let cells = grid_layout(&[&hall, &doors[1], &doors[2]], 12.0, 12.0, 0.2);
        assert!(
            cells.iter().all(|c| (c.2 - cells[1].2).abs() < 1e-9),
            "{cells:?}"
        );
        // Without the owner's choice the slivers lose to stacked rows.
        let plain = [door(false), door(false), door(false)];
        let cells = grid_layout(&plain.iter().collect::<Vec<_>>(), 12.0, 12.0, 0.2);
        assert!(cells.iter().any(|c| c.3 < 11.0));
    }

    #[test]
    fn a_featured_photo_takes_the_clearly_biggest_cell() {
        let photo = |feature: bool| BookPhoto {
            width: 4032,
            height: 3024,
            feature,
            ..BookPhoto::blank()
        };
        let area = |c: &Rect| c.2 * c.3;
        // The one of four like photos the owner features gets a cell every
        // other photo is at most 90% of.
        for k in [0, 3] {
            let photos: Vec<BookPhoto> = (0..4).map(|i| photo(i == k)).collect();
            let cells = grid_layout(&photos.iter().collect::<Vec<_>>(), 12.0, 12.0, 0.2);
            let featured = area(&cells[k]);
            for (i, c) in cells.iter().enumerate() {
                assert!(i == k || area(c) <= featured * 0.9 + 1e-9, "{k}: {cells:?}");
            }
        }
        // Unfeatured, the same four share the page evenly.
        let plain: Vec<BookPhoto> = (0..4).map(|_| photo(false)).collect();
        let cells = grid_layout(&plain.iter().collect::<Vec<_>>(), 12.0, 12.0, 0.2);
        assert!(cells
            .iter()
            .all(|c| (area(c) - area(&cells[0])).abs() < 1e-6));
    }

    #[test]
    fn places_inside_another_listed_place_are_dropped() {
        let mut places = vec![
            "Carnival Luminosa Embarkation",
            "Carnival Luminosa",
            "Fukuoka",
        ];
        drop_contained(&mut places);
        assert_eq!(places, ["Carnival Luminosa Embarkation", "Fukuoka"]);
    }

    #[test]
    fn an_owner_picture_is_cropped_to_the_page_about_its_centre() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("map.png");
        // Left half red, right half blue: a centred crop keeps both.
        RgbImage::from_fn(400, 100, |x, _| {
            image::Rgb(if x < 200 { [255, 0, 0] } else { [0, 0, 255] })
        })
        .save(&src)
        .unwrap();
        let page = page_crop(&src, (100, 100)).unwrap();
        assert_eq!(page.dimensions(), (100, 100));
        assert!(page.get_pixel(5, 50)[0] > 200 && page.get_pixel(95, 50)[2] > 200);
        assert!(page_crop(&dir.path().join("missing.png"), (100, 100)).is_err());
        // A cover that cannot be made leaves a note, not a stale file.
        let out = dir.path().join("cover.jpg");
        let mut notes = vec![];
        assert!(image_cover(&src, (100, 100), &out, &mut notes).is_some() && out.is_file());
        assert!(image_cover(
            &dir.path().join("missing.png"),
            (100, 100),
            &out,
            &mut notes
        )
        .is_none());
        assert!(!out.exists() && notes.len() == 1, "{notes:?}");
    }

    #[test]
    fn grid_layout_fits_photo_shapes() {
        let photo = |w: u32, h: u32, people: u32| BookPhoto {
            width: w,
            height: h,
            people,
            ..BookPhoto::blank()
        };
        let (land, tall) = (photo(4032, 3024, 0), photo(3024, 4032, 0));
        let group = photo(4032, 3024, 6);
        let fills_page = |cells: &[(f64, f64, f64, f64)]| {
            for &(x, y, w, h) in cells {
                assert!(x >= 0.0 && y >= 0.0 && x + w <= 12.0 + 1e-9 && y + h <= 12.0 + 1e-9);
            }
            // The lowest cell reaches the bottom margin: no empty band.
            let bottom = cells.iter().map(|c| c.1 + c.3).fold(0.0, f64::max);
            assert!((bottom - (12.0 - 0.12)).abs() < 1e-6, "bottom {bottom}");
        };
        // Four plain landscapes: the 2x2.
        let cells = grid_layout(&[&land, &land, &land, &land], 12.0, 12.0, 0.12);
        fills_page(&cells);
        assert!(cells.iter().all(|c| (c.2 - cells[0].2).abs() < 1e-6));
        // A group shot opening the page gets a full-width row: no faces
        // cut off its sides.
        let cells = grid_layout(&[&group, &land, &land, &land], 12.0, 12.0, 0.12);
        fills_page(&cells);
        assert!(cells[0].2 > 11.0, "group cell {:?}", cells[0]);
        let (a, c) = (4.0 / 3.0, cells[0].2 / cells[0].3);
        assert!(c >= a - 1e-6, "group photo keeps its full width");
        // Two portraits sit side by side, not stacked into wide strips.
        let cells = grid_layout(&[&tall, &tall], 12.0, 12.0, 0.12);
        fills_page(&cells);
        assert!((cells[0].1 - cells[1].1).abs() < 1e-6, "same row");
        // Three photos fill the page too (the old 3-up left a third empty).
        let cells = grid_layout(&[&land, &land, &land], 12.0, 12.0, 0.12);
        fills_page(&cells);
    }

    #[test]
    fn tiny_photos_block_even_smallest_slot() {
        // A 1000x800 screenshot cannot print at 200 dpi in a 5.5in slot.
        assert_eq!(dpi_verdict(1000, 800, 5.5, 5.5), DpiVerdict::Block);
        // A phone HEIC at full res clears it comfortably.
        assert_eq!(dpi_verdict(5712, 4284, 5.5, 5.5), DpiVerdict::Ok);
    }

    #[test]
    fn cap_selection_fits_and_spans_trip() {
        let chapters = even_chapters(901, 6);
        let pool: Vec<BookPhoto> = (0..901)
            .map(|i| BookPhoto {
                asset_id: format!("a{i}"),
                width: if i == 1 { 6000 } else { 4000 },
                height: 3000,
                chapter: chapters[i],
                ..BookPhoto::blank()
            })
            .collect();
        let keep = fitting_count(&pool, true, 60, None);
        assert!(keep < 901, "901 photos cannot fit 60 pages untrimmed");
        let picked: Vec<BookPhoto> = select_for_cap(&pool, keep)
            .0
            .into_iter()
            .map(|i| pool[i].clone())
            .collect();
        let kinds: Vec<PageKind> = book_flow(&picked, true, 60)
            .iter()
            .map(|p| p.kind)
            .collect();
        assert!(count_pages(&kinds) <= 60, "selected set must fit the cap");
        let chapters_kept: HashSet<Option<usize>> = picked.iter().map(|p| p.chapter).collect();
        assert_eq!(chapters_kept.len(), 6, "every chapter stays in the book");
        let idx = evenly_spaced(901, 10);
        assert_eq!(idx.first(), Some(&0), "keeps the opener");
        assert!(idx.windows(2).all(|w| w[0] < w[1]), "story order preserved");
        assert!(*idx.last().unwrap() > 800, "spans to the end of the trip");
    }

    fn scored(i: usize, chapter: usize, score: f32, keep: bool) -> BookPhoto {
        BookPhoto {
            keep,
            ..sized(i, 4000, 3000, Some(chapter), score)
        }
    }

    #[test]
    fn cap_selection_reserves_curated_photos() {
        let photos: Vec<BookPhoto> = (0..100)
            .map(|i| scored(i, 0, NEUTRAL_SCORE, i == 37 || i == 38))
            .collect();
        let (sel, dropped) = select_for_cap(&photos, 10);
        assert_eq!(sel.len(), 10);
        assert!(sel.contains(&37) && sel.contains(&38));
        assert!(sel.windows(2).all(|w| w[0] < w[1]), "story order preserved");
        assert!(dropped.is_empty());

        let all_kept: Vec<BookPhoto> = (0..20).map(|i| scored(i, 0, 0.5, true)).collect();
        let (sel, dropped) = select_for_cap(&all_kept, 5);
        assert_eq!(sel.len(), 5);
        assert_eq!(
            sel.len() + dropped.len(),
            20,
            "every curated omission reported"
        );
    }

    #[test]
    fn cap_selection_ranks_by_score_within_a_chapter() {
        // One chapter, one slot: the best-scored photo wins, wherever it is.
        let mut photos: Vec<BookPhoto> = (0..10).map(|i| scored(i, 0, 0.4, false)).collect();
        photos[7].score = 0.9;
        assert_eq!(select_for_cap(&photos, 1).0, [7]);
        // Two slots: the best of each half, so the picks still span the
        // chapter instead of clumping on one scene.
        photos[8].score = 0.8;
        photos[2].score = 0.5;
        assert_eq!(select_for_cap(&photos, 2).0, [2, 7]);
        // A place-fallback caption reserves nothing: only `keep` does.
        photos[0].caption = Some("Tokyo, Japan".to_string());
        assert_eq!(select_for_cap(&photos, 1).0, [7]);
    }

    #[test]
    fn cap_selection_keeps_every_chapter_in_proportion() {
        // Chapters of 60, 30, 9 and 1 photos; the best photo is each
        // chapter's last one.
        let sizes = [60usize, 30, 9, 1];
        let mut photos = vec![];
        for (c, &n) in sizes.iter().enumerate() {
            for j in 0..n {
                let score = if j == n - 1 { 0.9 } else { 0.4 };
                photos.push(scored(photos.len(), c, score, false));
            }
        }
        let (sel, _) = select_for_cap(&photos, 20);
        assert_eq!(sel.len(), 20);
        let per_chapter: Vec<usize> = (0..sizes.len())
            .map(|c| {
                sel.iter()
                    .filter(|&&i| photos[i].chapter == Some(c))
                    .count()
            })
            .collect();
        assert!(per_chapter.iter().all(|&n| n >= 1), "{per_chapter:?}");
        assert!(per_chapter[0] > per_chapter[1] && per_chapter[1] > per_chapter[2]);
        // Each chapter's best photo made it.
        for last in [59, 89, 98, 99] {
            assert!(sel.contains(&last), "best photo {last} missing: {sel:?}");
        }
        // Fewer slots than chapters: the largest chapters keep theirs.
        let (sel, _) = select_for_cap(&photos, 2);
        assert_eq!(sel, [59, 89]);
    }

    #[test]
    fn posing_session_is_one_scene() {
        // A bench session: frames 30 s apart, each like its neighbour
        // (pHash 8 apart) though the ends no longer look alike (pHash 24).
        // One copy carries people tags, the rest none. Then a new scene.
        let frame = |i: usize, t: i64, phash: u64, sig: Option<u64>| BookPhoto {
            taken_at: Some(t),
            phash: Some(phash),
            people_sig: sig,
            ..scored(i, 0, 0.5, false)
        };
        let photos = vec![
            frame(0, 0, 0, None),
            frame(1, 30, 0xff, Some(5)),
            frame(2, 60, 0xffff, None),
            frame(3, 90, 0xff_ffff, None),
            frame(4, 3600, 0, None),
        ];
        assert!(!photos[0].repeats(&photos[3]), "ends differ");
        assert_eq!(scenes(&photos), [0, 0, 0, 0, 1]);
        // Two picks: one from the session, one from the next scene.
        let sel = select_for_cap(&photos, 2).0;
        assert_eq!(sel.len(), 2);
        assert!(sel.contains(&4), "{sel:?}");
        // Two different tagged groups in one place stay separate scenes.
        let other = frame(2, 60, 0xffff, Some(9));
        let photos = [photos[1].clone(), other];
        assert_eq!(scenes(&photos), [0, 1]);
    }

    #[test]
    fn cap_selection_skips_repeats_of_the_same_people() {
        // Eight shots an hour apart; 1 and 2 are one burst (same second)
        // across two stretches of the 4-of-8 pick.
        let book = |people_of_2: Option<u64>| -> Vec<BookPhoto> {
            (0..8)
                .map(|i| {
                    let score = [0.5, 0.9, 0.8, 0.5, 0.5, 0.5, 0.5, 0.5][i];
                    BookPhoto {
                        taken_at: Some(if i == 2 { 3600 } else { i as i64 * 3600 }),
                        people_sig: if i == 2 { people_of_2 } else { Some(7) },
                        ..scored(i, 0, score, false)
                    }
                })
                .collect()
        };
        // Same people: the burst's second shot waits; its stretch takes 3.
        assert_eq!(select_for_cap(&book(Some(7)), 4).0, [1, 3, 4, 6]);
        // The same view with someone else in it is a new keepsake.
        assert_eq!(select_for_cap(&book(Some(9)), 4).0, [1, 2, 4, 6]);
        // Everything one burst: repeats still fill the quota, best first.
        let mut burst = book(Some(7));
        burst.iter_mut().for_each(|p| p.taken_at = Some(0));
        assert_eq!(select_for_cap(&burst, 4).0, [0, 1, 2, 3]);
        // A curated photo is reserved first and never dropped; the better
        // shot it repeats is the one that waits.
        let mut curated = book(Some(7));
        curated[2].keep = true;
        let (sel, dropped) = select_for_cap(&curated, 4);
        assert!(dropped.is_empty());
        assert!(sel.contains(&2) && !sel.contains(&1), "{sel:?}");
        assert!(sel.windows(2).all(|w| w[0] < w[1]), "story order: {sel:?}");
    }

    #[test]
    fn chapter_quotas_use_every_slot_and_never_overfill() {
        for (sizes, slots) in [
            (vec![60, 30, 9, 1], 20),
            (vec![1, 1, 1], 2),
            (vec![5, 5], 9),
            (vec![3], 0),
            (vec![7, 1, 1, 40], 49),
        ] {
            let q = chapter_quotas(&sizes, slots);
            assert_eq!(q.iter().sum::<usize>(), slots, "{sizes:?} / {slots}");
            assert!(
                q.iter().zip(&sizes).all(|(q, n)| q <= n),
                "{sizes:?} -> {q:?}"
            );
        }
        assert_eq!(chapter_quotas(&[60, 30, 9, 1], 20), [11, 6, 2, 1]);
    }

    #[test]
    fn page_counting_rules() {
        let kinds = vec![
            PageKind::CoverProxy,
            PageKind::Hero,
            PageKind::SpreadLeft,
            PageKind::SpreadRight,
            PageKind::Closing,
        ];
        assert_eq!(count_pages(&kinds), 4, "cover-proxy uncounted, spread = 2");
        assert_eq!(PageKind::CoverProxy.counts(), 0);
    }

    #[test]
    fn flow_spread_alignment_and_density() {
        let flow = flow_pages(&[None; 12], &[], &[], &[], Some(1), false, 60);
        let kinds: Vec<PageKind> = flow.iter().map(|p| p.kind).collect();
        let spread_at = kinds
            .iter()
            .position(|k| *k == PageKind::SpreadLeft)
            .unwrap();
        let counted_before: usize = kinds[..spread_at].iter().map(|k| k.counts()).sum();
        assert_eq!(
            counted_before, 1,
            "spread starts on counted page 2 (even/left)"
        );
        assert_eq!(kinds[spread_at + 1], PageKind::SpreadRight);
        // 12 photos one-per-page: 1 hero + 1 spread + 10 singles + closing = 14.
        assert_eq!(count_pages(&kinds), 14);
        // 30 photos still clear the 24-page minimum without padding.
        let flow = flow_pages(&even_chapters(30, 2), &[], &[], &[], None, false, 60);
        let kinds: Vec<PageKind> = flow.iter().map(|p| p.kind).collect();
        let counted = count_pages(&kinds);
        assert!(
            counted >= 24,
            "30 photos must clear the minimum, got {counted}"
        );
        assert!(counted <= 60);
        assert_eq!(kinds[0], PageKind::CoverProxy);
        assert_eq!(*kinds.last().unwrap(), PageKind::Closing);
    }

    #[test]
    fn title_pages_keep_a_captioned_photo_place() {
        let bg = BookPhoto {
            chapter: Some(0),
            caption: Some("Best ramen of the trip.".into()),
            place: Some("Canal City Ramen Stadium".into()),
            keep: true,
            ..BookPhoto::blank()
        };
        let titles = ["Day 11 — Fukuoka".to_string()];
        assert_eq!(
            divider_lines(&bg, &titles, &mut Announced::default()),
            // The heading names the page's place; the caption keeps its words.
            [
                "Day 11 — Canal City Ramen Stadium",
                "Best ramen of the trip.",
            ]
        );
    }

    #[test]
    fn divider_lines_match_background_chapter_and_keep_prose() {
        let photo = |chapter: Option<usize>, caption: Option<&str>| BookPhoto {
            asset_id: "p".into(),
            width: 100,
            height: 100,
            caption: caption.map(String::from),
            chapter,
            ..BookPhoto::blank()
        };
        let titles = ["Tokyo".to_string(), "Kyoto".to_string()];
        let mut seen = Announced::default();
        // Title follows the photo's chapter, not the divider's position.
        assert_eq!(
            divider_lines(&photo(Some(1), None), &titles, &mut seen),
            ["Kyoto"]
        );
        // Same chapter again: no repeated title, and user prose survives.
        assert_eq!(
            divider_lines(&photo(Some(1), Some("Bamboo")), &titles, &mut seen),
            ["Bamboo"]
        );
        // Auto datelines and place captions would repeat the title.
        assert_eq!(
            titled(
                Some("Day 2 — Kyoto, Japan".to_string()),
                vec!["Kyoto, Japan".to_string()]
            ),
            ["Day 2 — Kyoto, Japan"]
        );
        assert_eq!(
            divider_lines(
                &photo(Some(0), Some("Day 1 · Sun, Oct 4 · 9:00 AM")),
                &titles,
                &mut seen
            ),
            ["Tokyo"]
        );
        // A photo outside every chapter gets no title.
        assert!(divider_lines(&photo(None, None), &titles, &mut seen).is_empty());
    }

    #[test]
    fn each_day_gets_one_banner() {
        let photo = |chapter: usize, day: u32| BookPhoto {
            chapter: Some(chapter),
            day: Some(day),
            ..BookPhoto::blank()
        };
        let titles = [
            "Day 3 — Tokyo".to_string(),
            "Day 3 — Tokyo".to_string(),
            "Day 4 — Tokyo".to_string(),
        ];
        let mut seen = Announced::default();
        assert_eq!(
            day_banner(&photo(0, 3), &titles, &mut seen).as_deref(),
            Some("Day 3 — Tokyo")
        );
        // Same day, next page or a second chapter that day: no banner.
        assert_eq!(day_banner(&photo(0, 3), &titles, &mut seen), None);
        assert_eq!(day_banner(&photo(1, 3), &titles, &mut seen), None);
        // A divider already named day 4: its pages get no banner.
        assert!(divider_lines(&photo(2, 4), &titles, &mut seen)[0].contains("Day 4"));
        assert_eq!(day_banner(&photo(2, 4), &titles, &mut seen), None);
        // A second title page on day 4 names only its place.
        let more = [titles.to_vec(), vec!["Day 4 — Ginza".to_string()]].concat();
        assert_eq!(divider_lines(&photo(3, 4), &more, &mut seen)[0], "Ginza");
        // Unknown day: no banner.
        assert_eq!(day_banner(&BookPhoto::blank(), &titles, &mut seen), None);
    }

    #[test]
    fn planned_blocked_accounts_for_divider_backgrounds() {
        let photo = |id: &str, px: u32, chapter: usize| BookPhoto {
            asset_id: id.into(),
            path: PathBuf::from(id),
            width: px,
            height: px,
            chapter: Some(chapter),
            ..BookPhoto::blank()
        };
        // Flow: hero (p0), divider background (p1), photo page (p2).
        // 1200px clears the smallest slot's 200 dpi but prints at 100 dpi full-page.
        let low_res_background = [
            photo("p0", 8000, 0),
            photo("p1", 1200, 1),
            photo("p2", 8000, 1),
        ];
        assert_eq!(
            planned_blocked(&low_res_background, BookPreset::P1, 60, false),
            vec!["p1".to_string()]
        );
        let low_res_page = [
            photo("p0", 8000, 0),
            photo("p1", 8000, 1),
            photo("p2", 10, 1),
        ];
        assert_eq!(
            planned_blocked(&low_res_page, BookPreset::P1, 60, false),
            vec!["p2".to_string()]
        );
    }

    #[test]
    fn planned_blocked_checks_caption_pages_at_full_trim() {
        let photo = |id: &str, w: u32, h: u32| BookPhoto {
            asset_id: id.into(),
            path: PathBuf::from(id),
            width: w,
            height: h,
            ..BookPhoto::blank()
        };
        // Flow: hero, then one caption page. Each size clears 200 dpi inside
        // the photo area an inset layout would have, but not at full bleed.
        for (preset, w, h, inset) in [
            (BookPreset::P1, 2400, 2000, (11.0, 9.0)),
            (BookPreset::P2, 2000, 1500, (10.0, 6.0)),
        ] {
            assert_ne!(dpi_verdict(w, h, inset.0, inset.1), DpiVerdict::Block);
            let photos = [photo("hero", 8000, 8000), photo("soft", w, h)];
            let (_, cap) = preset.page_range();
            assert_eq!(planned_blocked(&photos, preset, cap, false), vec!["soft"]);
        }
    }

    #[test]
    fn flow_packs_large_sets_into_grids() {
        let flow = flow_pages(&[None; 200], &[], &[], &[], None, false, 60);
        let kinds: Vec<PageKind> = flow.iter().map(|p| p.kind).collect();
        assert!(count_pages(&kinds) <= 60, "cap honored");
        assert!(kinds.contains(&PageKind::Grid));
        let takes: usize = flow.iter().map(|p| p.take).sum();
        assert_eq!(takes, 200, "every photo placed exactly once");
    }

    #[test]
    fn preflight_dpi_and_caps() {
        assert_eq!(dpi_verdict(3600, 3600, 12.0, 12.0), DpiVerdict::Ok);
        assert_eq!(dpi_verdict(3000, 3000, 12.0, 12.0), DpiVerdict::Warn);
        assert_eq!(dpi_verdict(2000, 2000, 12.0, 12.0), DpiVerdict::Block);
        let over = vec![PageKind::Grid; 61];
        let issues = preflight(BookPreset::P1, 60, &over, &[]);
        assert!(issues
            .iter()
            .any(|i| i.level == "block" && i.message.contains("cap")));
        // A raised cap lets the same 61 pages through.
        assert!(preflight(BookPreset::P1, 111, &over, &[]).is_empty());
        let tiny = vec![PageKind::Hero, PageKind::Closing];
        let issues = preflight(BookPreset::P1, 60, &tiny, &[]);
        assert!(issues
            .iter()
            .any(|i| i.level == "block" && i.message.contains("minimum")));
        let ok_kinds = vec![PageKind::Hero; 30];
        let issues = preflight(
            BookPreset::P1,
            60,
            &ok_kinds,
            &[(3, DpiVerdict::Warn), (5, DpiVerdict::Block)],
        );
        assert!(issues
            .iter()
            .any(|i| i.level == "warn" && i.page == Some(3)));
        assert!(issues
            .iter()
            .any(|i| i.level == "block" && i.page == Some(5)));
    }

    /// End-to-end: captions must reach the vendor JPEGs, not just the PDFs.
    /// Skipped when the Typst CLI is not on PATH. Ignored by default: full
    /// 3600px pages take minutes in debug builds — run with
    /// `cargo test -p triptape-media --release -- --ignored`.
    #[test]
    #[ignore]
    fn captions_burned_into_vendor_jpegs() {
        if Command::new("typst").arg("--version").output().is_err() {
            eprintln!("typst not on PATH; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.jpg");
        save_jpg(&solid(3000, 3000, Rgb([100, 100, 100])), &src).unwrap();
        let photos: Vec<BookPhoto> = (0..24)
            .map(|i| BookPhoto {
                asset_id: format!("a{i}"),
                path: src.clone(),
                width: 3000,
                height: 3000,
                caption: (i == 1).then(|| "Lisbon at dusk".to_string()),
                ..BookPhoto::blank()
            })
            .collect();
        let out = dir.path().join("book");
        let opts = test_opts("typst");
        let res = render_book(&photos, None, &[], &out, &opts).unwrap();
        assert!(
            !res.fallbacks
                .iter()
                .any(|f| f.contains("captions not burned")),
            "{:?}",
            res.fallbacks
        );
        let bright = |name: &str| {
            let img = image::open(out.join(name)).unwrap().to_rgb8();
            let (w, h) = img.dimensions();
            (h * 8 / 10..h)
                .flat_map(|y| (0..w).map(move |x| (x, y)))
                .filter(|&(x, y)| img.get_pixel(x, y).0.iter().all(|&c| c > 220))
                .count()
        };
        assert!(
            bright("page_002.jpg") > 100,
            "caption text missing from JPEG"
        );
        assert_eq!(bright("page_003.jpg"), 0, "uncaptioned page gained text");
        // Caption pages are full-bleed: no dark band under the photo.
        let page3 = image::open(out.join("page_003.jpg")).unwrap().to_rgb8();
        let (w, h) = page3.dimensions();
        assert!(
            page3.get_pixel(w / 2, h - 1).0.iter().all(|&c| c > 80),
            "photo does not reach the page's bottom edge"
        );
        let guide = std::fs::read_to_string(&res.guide_path).unwrap();
        assert!(guide.contains("burned into the page JPEGs"));
        assert!(!out.join(".caption_raster").exists());
    }

    /// End-to-end: every chapter's title reaches the page that opens it, once,
    /// including a chapter whose only photo is on the spread. Reads the Typst
    /// source, so Typst itself is not needed. Ignored by default like the test
    /// above: full-size pages are slow in debug builds.
    #[test]
    #[ignore]
    fn chapter_titles_reach_their_opening_pages() {
        let dir = tempfile::tempdir().unwrap();
        let (square, wide) = (dir.path().join("square.jpg"), dir.path().join("wide.jpg"));
        save_jpg(&solid(3000, 3000, Rgb([100, 100, 100])), &square).unwrap();
        save_jpg(&solid(6000, 3000, Rgb([100, 100, 100])), &wide).unwrap();
        let chapter_of = sized_chapters(&[20, 1, 2, 7]);
        let photos: Vec<BookPhoto> = chapter_of
            .iter()
            .enumerate()
            .map(|(i, &chapter)| {
                let is_wide = i == 20;
                BookPhoto {
                    asset_id: format!("a{i}"),
                    path: if is_wide {
                        wide.clone()
                    } else {
                        square.clone()
                    },
                    width: if is_wide { 6000 } else { 3000 },
                    height: 3000,
                    chapter,
                    ..BookPhoto::blank()
                }
            })
            .collect();
        let titles: Vec<String> = ["Lisbon", "Sintra", "Porto", "Braga"]
            .map(String::from)
            .to_vec();
        let out = dir.path().join("book");
        let opts = test_opts("typst-not-installed");
        render_book(&photos, None, &titles, &out, &opts).unwrap();
        let typ = std::fs::read_to_string(out.join("proof.typ")).unwrap();
        // Proof opens page 1 on the right; press files have no blanks.
        assert_eq!(
            typ.matches("#box()\n#pagebreak()").count(),
            PROOF_LEAD_BLANKS
        );
        if let Ok(press) = std::fs::read_to_string(out.join("press.typ")) {
            assert!(!press.contains("#box()\n#pagebreak()"));
        }
        let at: Vec<usize> = titles
            .iter()
            .map(|t| {
                assert_eq!(typ.matches(t.as_str()).count(), 1, "{t} once");
                typ.find(t.as_str()).unwrap()
            })
            .collect();
        // Hero (Lisbon), spread (Sintra, used up), then dividers in order.
        assert!(at.windows(2).all(|w| w[0] < w[1]), "{titles:?} at {at:?}");
    }

    #[test]
    fn an_owners_picture_becomes_the_back_cover_at_page_size() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("map.png");
        RgbImage::from_pixel(300, 200, Rgb([200, 150, 90]))
            .save(&src)
            .unwrap();
        let out = dir.path().join("back-cover.jpg");
        let mut notes = vec![];
        let mut opts = test_opts("typst-not-installed");
        assert!(make_back_cover(&opts, (120, 120), &out, &mut notes).is_none());
        opts.back_cover_image = Some(src);
        let made = make_back_cover(&opts, (120, 120), &out, &mut notes);
        assert_eq!(made.as_deref(), Some(out.as_path()));
        assert_eq!(
            image::open(&out).unwrap().to_rgb8().dimensions(),
            (120, 120)
        );
        assert!(notes.is_empty());
        // A picture that cannot be read is noted, and no stale file is left.
        opts.back_cover_image = Some(dir.path().join("nope.png"));
        let missing = make_back_cover(&opts, (120, 120), &out, &mut notes);
        assert!(missing.is_none() && !out.exists());
        assert!(notes[0].starts_with("back cover skipped"), "{notes:?}");
        // With no picture the route map is drawn; without Typst it is noted.
        opts.back_cover_image = None;
        opts.journey = Some(triptape_core::journey::Journey::build(
            &triptape_core::itinerary::Itinerary::from_json(
                r#"{"stops": [
                  { "id": "a", "label": "Lisbon", "lat": 38.72, "lon": -9.14 },
                  { "id": "b", "label": "Porto", "lat": 41.15, "lon": -8.61 }
                ], "legs": [{ "from": "a", "to": "b", "mode": "ground" }]}"#,
            )
            .unwrap(),
            &[],
        ));
        assert!(make_back_cover(&opts, (120, 120), &out, &mut notes).is_none());
        assert_eq!(notes.len(), 2, "{notes:?}");
        let guide = guide_text(BookPreset::P1, "Trip", 24, true, true);
        assert!(guide.contains("back-cover.jpg"));
        assert!(!guide_text(BookPreset::P1, "Trip", 24, true, false).contains("back-cover"));
    }

    #[test]
    fn cover_prefers_camera_photos_of_people() {
        let shot = |path: &str, tagged: u32, face: f32, (w, h): (u32, u32)| BookPhoto {
            path: PathBuf::from(path),
            tagged_people: tagged,
            people: tagged,
            face_size: face,
            width: w,
            height: h,
            ..BookPhoto::blank()
        };
        let photo = (4032, 3024);
        assert_eq!(
            cover_rank(&shot("IMG_1028.HEIC", 3, 0.0, photo)),
            2,
            "tagged"
        );
        assert_eq!(
            cover_rank(&shot("IMG_2.HEIC", 0, 0.2, photo)),
            2,
            "a big face"
        );
        // A stranger far off or a distant statue is not the subject.
        assert_eq!(cover_rank(&shot("IMG_3.HEIC", 0, 0.02, photo)), 1);
        assert_eq!(
            cover_rank(&shot("IMG_1.HEIC", 0, 0.0, photo)),
            1,
            "no people"
        );
        assert_eq!(
            cover_rank(&shot("E5B14CFD.png", 3, 0.3, photo)),
            0,
            "composite"
        );
        assert_eq!(
            cover_rank(&shot("PANO.HEIC", 2, 0.1, (16000, 3800))),
            0,
            "panorama"
        );
    }

    #[test]
    fn cover_picks_take_each_stretchs_best_and_honour_pins() {
        let shot = |i: usize, tagged: u32, score: f32| BookPhoto {
            asset_id: format!("a{i}"),
            path: PathBuf::from(format!("IMG_{i}.HEIC")),
            width: 4032,
            height: 3024,
            tagged_people: tagged,
            score,
            ..BookPhoto::blank()
        };
        // 48 photos, 4 per stretch: a people shot in each, the best-scored
        // one at the stretch's third slot.
        let photos: Vec<BookPhoto> = (0..48)
            .map(|i| match i % 4 {
                2 => shot(i, 2, 0.9),
                1 => shot(i, 1, 0.5),
                _ => shot(i, 0, 0.95),
            })
            .collect();
        let picks = cover_picks(&photos, &[]);
        assert_eq!(picks, (0..12).map(|k| 4 * k + 2).collect::<Vec<_>>());
        // The hero never repeats; a pin always shows.
        let mut pinned = photos.clone();
        pinned[0].cover_focus = Some(0.8);
        let picks = cover_picks(&pinned, &[2]);
        assert_eq!(picks.len(), 12);
        assert!(picks.contains(&0) && !picks.contains(&2), "{picks:?}");
        assert!(picks.windows(2).all(|w| w[0] < w[1]));
        // A pinned day gives no second tile while other days remain.
        let mut days = photos.clone();
        for (i, p) in days.iter_mut().enumerate() {
            p.day = Some((i / 4) as u32);
        }
        days[2].cover_focus = Some(0.5);
        let picks = cover_picks(&days, &[]);
        let first_day: Vec<_> = picks.iter().filter(|&&i| i < 4).collect();
        assert_eq!(first_day, [&2], "{picks:?}");
        // Small trip: every photo but the opening.
        assert_eq!(cover_picks(&photos[..3], &[1]), vec![0, 2]);
        // Nothing but the opening: the cover still shows it.
        assert_eq!(cover_picks(&photos[..1], &[0]), vec![0]);
        assert_eq!(cover_picks(&photos[..2], &[0, 1]), vec![0, 1]);
    }

    #[test]
    fn preset_geometry() {
        assert_eq!(BookPreset::P1.trim_px(), (3600, 3600), "12in at 300dpi");
        assert_eq!(BookPreset::P2.trim_px(), (3300, 2550), "11x8.5in at 300dpi");
        assert_eq!(BookPreset::from_flag("p1"), Some(BookPreset::P1));
        assert_eq!(BookPreset::from_flag("P2"), Some(BookPreset::P2));
        assert_eq!(BookPreset::from_flag("A4"), None);
    }

    #[test]
    fn page_cap_defaults_and_bounds() {
        assert_eq!(BookPreset::P1.page_cap(None), Ok(60));
        assert_eq!(BookPreset::P2.page_cap(None), Ok(80));
        assert_eq!(BookPreset::P1.page_cap(Some(111)), Ok(111));
        assert_eq!(BookPreset::P1.page_cap(Some(24)), Ok(24));
        assert_eq!(
            BookPreset::P1.page_cap(Some(MAX_BOOK_PAGES)),
            Ok(MAX_BOOK_PAGES)
        );
        assert!(BookPreset::P1.page_cap(Some(23)).is_err());
        assert!(BookPreset::P1.page_cap(Some(MAX_BOOK_PAGES + 1)).is_err());
    }

    #[test]
    fn raised_cap_keeps_more_photos() {
        // 400 photos overflow 60 pages even at 4/page; 111 holds more of them.
        let pool: Vec<BookPhoto> = (0..400)
            .map(|i| BookPhoto {
                asset_id: format!("a{i}"),
                path: PathBuf::from(format!("a{i}")),
                width: 6000,
                height: 4000,
                ..BookPhoto::blank()
            })
            .collect();
        let at_60 = fitting_count(&pool, false, 60, None);
        let at_111 = fitting_count(&pool, false, 111, None);
        assert!(at_111 > at_60, "{at_111} > {at_60}");
        let flow = book_flow(&pool[..at_111], false, 111);
        let kinds: Vec<PageKind> = flow.iter().map(|p| p.kind).collect();
        assert!(count_pages(&kinds) <= 111);
    }
}
