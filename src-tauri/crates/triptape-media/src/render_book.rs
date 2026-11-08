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

    /// (min, max) counted interior pages.
    pub fn page_range(&self) -> (usize, usize) {
        match self {
            BookPreset::P1 => (24, 60),
            BookPreset::P2 => (24, 80),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            BookPreset::P1 => "Classic Square 12x12",
            BookPreset::P2 => "Travel Landscape 11x8.5",
        }
    }
}

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
}

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
    } else if dpi >= 200.0 {
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

/// Photo indices in the order pages consume them: hero, then the spread's
/// wide photo, then the rest in story order. Every photo appears exactly once.
fn consumption_order(n_photos: usize, spread: Option<usize>) -> Vec<usize> {
    let mut order: Vec<usize> = (0..n_photos.min(1)).collect();
    order.extend(spread);
    order.extend((1..n_photos).filter(|&i| Some(i) != spread));
    order
}

/// Flow photos into a page plan. `chapter_of` holds each photo's chapter in
/// story order and `wide_idx` the first wide (≥1.5:1) photo after the hero.
/// Density adapts so small books still clear the 24-page vendor minimum (one
/// photo per page when it fits under the cap, grids of up to 4 otherwise). A
/// spread pair at counted pages 2–3 uses the wide photo when at least 5 photos
/// remain — always starting on an even (left) page. The hero opens its own
/// chapter; every other chapter with a placed photo gets a divider backed by
/// its first photo (up to [`MAX_DIVIDERS`]), and no photo page crosses one.
pub fn flow_pages(
    chapter_of: &[Option<usize>],
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
    let rest: Vec<Option<usize>> = consumption_order(n_photos, spread)
        .into_iter()
        .skip(1 + usize::from(spread.is_some()))
        .map(|i| chapter_of[i])
        .collect();
    // A chapter opens at its first photo left after the hero and spread.
    let mut opened: HashSet<usize> = chapter_of[0].into_iter().collect();
    let openings: Vec<usize> = rest
        .iter()
        .enumerate()
        .filter(|(_, c)| c.is_some_and(|c| opened.insert(c)))
        .map(|(i, _)| i)
        .collect();
    let dividers: Vec<usize> = evenly_spaced(openings.len(), MAX_DIVIDERS)
        .into_iter()
        .map(|j| openings[j])
        .collect();
    // Photo-page runs between dividers. Each divider consumes its photo as a
    // full-page background, so that photo never repeats later in the book.
    let mut run_lens: Vec<usize> = vec![];
    let mut start = 0;
    for &d in &dividers {
        run_lens.push(d - start);
        start = d + 1;
    }
    run_lens.push(rest.len() - start);
    let photo_budget = cap
        .saturating_sub(counted_before + dividers.len() + extras)
        .max(1);
    let per_page = (1..=4)
        .find(|&p| run_lens.iter().map(|l| l.div_ceil(p)).sum::<usize>() <= photo_budget)
        .unwrap_or(4);
    for (i, &run) in run_lens.iter().enumerate() {
        if i > 0 {
            pages.push(FlowPage {
                kind: PageKind::Divider,
                take: 1,
            });
        }
        let mut left = run;
        while left > 0 {
            let take = left.min(per_page);
            pages.push(FlowPage {
                kind: if take == 1 {
                    PageKind::PhotoCaption
                } else {
                    PageKind::Grid
                },
                take,
            });
            left -= take;
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

/// Photo slot size in inches for dpi triage (photo area of the layout).
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
    kinds: &[PageKind],
    placements: &[(usize, DpiVerdict)],
) -> Vec<PreflightIssue> {
    let mut issues = vec![];
    let counted = count_pages(kinds);
    let (min, max) = preset.page_range();
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
            message: format!("{counted} pages over the {max}-page preset cap; remove photos"),
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
    /// Progress and cancel handle (default: silent, never cancelled).
    pub progress: crate::progress::Progress,
}

#[derive(Debug)]
pub struct BookOutput {
    pub dir: PathBuf,
    /// Photo-collage cover (proof/press first page; not a counted page).
    pub cover: Option<PathBuf>,
    pub jpeg_pages: Vec<PathBuf>,
    pub proof_pdf: Option<PathBuf>,
    pub press_pdf: Option<PathBuf>,
    pub guide_path: PathBuf,
    pub counted_pages: usize,
    pub fallbacks: Vec<String>,
}

/// How many photos after the hero may supply the opening spread.
const SPREAD_WINDOW: usize = 11;

/// First wide (≥1.5:1) photo in the opening stretch after the hero: the
/// spread candidate. Pulling the first wide shot of the whole trip (say,
/// an Osaka landscape from day 8) to page 2 would break the book's
/// chronology, so no early wide photo means no spread.
fn wide_index(photos: &[BookPhoto]) -> Option<usize> {
    photos
        .iter()
        .skip(1)
        .take(SPREAD_WINDOW)
        .position(|p| p.height > 0 && p.width as f64 / p.height as f64 >= 1.5)
        .map(|i| i + 1)
}

/// The page plan for exactly these photos.
fn book_flow(photos: &[BookPhoto], has_map: bool, cap: usize) -> Vec<FlowPage> {
    let chapter_of: Vec<Option<usize>> = photos.iter().map(|p| p.chapter).collect();
    flow_pages(&chapter_of, wide_index(photos), has_map, cap)
}

/// Largest selection size found whose flowed book fits `cap` counted pages.
/// Pages depend on which chapters the selection keeps, not only on its size,
/// so each candidate is flowed for real. That also makes fit non-monotonic:
/// the search may settle a little low, but what it returns always fits.
fn fitting_count(pool: &[BookPhoto], has_map: bool, cap: usize) -> usize {
    let fits = |n: usize| {
        let picked: Vec<BookPhoto> = select_for_cap(pool, n)
            .0
            .into_iter()
            .map(|i| pool[i].clone())
            .collect();
        count_pages(
            &book_flow(&picked, has_map, cap)
                .iter()
                .map(|p| p.kind)
                .collect::<Vec<_>>(),
        ) <= cap
    };
    if fits(pool.len()) {
        return pool.len();
    }
    let (mut lo, mut hi) = (0usize, pool.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if fits(mid) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

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

/// Pick `keep` indices for the cap: captioned (user-curated) photos are
/// reserved first, then the opener if a slot remains, and the remaining
/// slots are spread evenly over the other photos. Returns the sorted selection plus any captioned
/// indices that still could not fit (more captions than slots).
fn select_for_cap(photos: &[BookPhoto], keep: usize) -> (Vec<usize>, Vec<usize>) {
    let total = photos.len();
    if keep >= total {
        return ((0..total).collect(), vec![]);
    }
    if keep == 0 {
        return (
            vec![],
            (0..total).filter(|&i| has_caption(&photos[i])).collect(),
        );
    }
    // Captioned photos reserve first; the opener joins only if a slot
    // remains after every captioned photo is in.
    let captioned: Vec<usize> = (0..total).filter(|&i| has_caption(&photos[i])).collect();
    if captioned.len() > keep {
        let picked: Vec<usize> = evenly_spaced(captioned.len(), keep)
            .into_iter()
            .map(|j| captioned[j])
            .collect();
        let dropped = captioned
            .iter()
            .copied()
            .filter(|i| !picked.contains(i))
            .collect();
        return (picked, dropped);
    }
    let mut reserved = captioned;
    if !reserved.contains(&0) && reserved.len() < keep {
        reserved.push(0);
    }
    reserved.sort_unstable();
    let others: Vec<usize> = (0..total).filter(|i| !reserved.contains(i)).collect();
    let mut sel = reserved.clone();
    sel.extend(
        evenly_spaced(others.len(), keep - reserved.len())
            .into_iter()
            .map(|j| others[j]),
    );
    sel.sort_unstable();
    (sel, vec![])
}

fn has_caption(p: &BookPhoto) -> bool {
    p.caption.as_deref().is_some_and(|c| !c.trim().is_empty())
}

/// Simulate the renderer's page assignment and return asset ids whose
/// dpi verdict blocks at their assigned slot (hero/spread/caption/grid/divider).
fn planned_blocked(photos: &[BookPhoto], preset: BookPreset, has_map: bool) -> Vec<String> {
    if photos.is_empty() {
        return vec![];
    }
    let (_, cap) = preset.page_range();
    let flow = book_flow(photos, has_map, cap);
    let has_spread = flow.iter().any(|p| p.kind == PageKind::SpreadLeft);
    let order = consumption_order(photos.len(), wide_index(photos).filter(|_| has_spread));
    let mut blocked = vec![];
    let mut cursor = 0usize;
    for fp in &flow {
        match fp.kind {
            PageKind::CoverProxy => {}
            PageKind::Hero | PageKind::PhotoCaption | PageKind::Grid | PageKind::Divider => {
                let slot = &order[cursor..(cursor + fp.take).min(order.len())];
                cursor += slot.len();
                let (sw, sh) = slot_inches(preset, fp.kind, fp.take.max(1));
                for oi in slot {
                    let p = &photos[*oi];
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
    std::fs::create_dir_all(out_dir).map_err(|e| crate::MediaError::Io(e.to_string()))?;
    let mut fallbacks: Vec<String> = vec![];
    // Auto-select for the preset cap: a big trip flows past the cap even at
    // 4 photos/page, so sample evenly across story order instead of
    // blocking the export (PLAN Sec 11: preflight blocks, curation selects).
    let (_, cap_pre) = opts.preset.page_range();
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
            // Captioned (user-kept) photos are never dropped silently: they
            // stay in and surface as a blocking preflight issue instead.
            let ok = has_caption(p)
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
    // Cap selection + placement-aware prune, iterated: a photo that blocks
    // at its assigned slot (a mid-size photo can pass the smallest slot yet
    // block as hero) is excluded and the selection is redone from the
    // remaining viable photos, so unselected photos refill the gap instead
    // of the book shrinking. Each pass excludes at least one new photo, so
    // it terminates; the post-render preflight stays as guard.
    let mut excluded: HashSet<String> = HashSet::new();
    let mut cap_note: Option<(usize, usize)>;
    let mut cap_dropped: Vec<String>;
    let pruned: Vec<BookPhoto> = loop {
        let pool: Vec<BookPhoto> = viable
            .iter()
            .filter(|p| !excluded.contains(&p.asset_id))
            .cloned()
            .collect();
        // Cap math runs on the survivors: dropping the only wide photo
        // removes the spread, which can let more photos fit.
        let keep = fitting_count(&pool, route_card.is_some(), cap_pre);
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
        let blocked: Vec<String> = planned_blocked(&sel, opts.preset, route_card.is_some());
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
            "book: {} captioned photos did not fit the {cap_pre}-page cap (kept an even spread; first omitted: {})",
            cap_dropped.len(),
            ids.join(", ")
        ));
    }
    if let Some((keep, of)) = cap_note {
        fallbacks.push(format!(
            "book: selected {keep} of {of} photos (captioned photos first, the rest evenly across the trip) to fit the {cap_pre}-page cap"
        ));
    }
    let photos: &[BookPhoto] = &pruned;

    let (_, cap) = opts.preset.page_range();
    let flow = book_flow(photos, route_card.is_some(), cap);
    let kinds: Vec<PageKind> = flow.iter().map(|p| p.kind).collect();
    // Page-count caps depend only on the flow: block before any image work.
    let early: Vec<String> = preflight(opts.preset, &kinds, &[])
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
    // Consumption order: hero, then the spread wide photo, then the rest
    // in story order — every photo placed exactly once.
    let has_spread = flow.iter().any(|p| p.kind == PageKind::SpreadLeft);
    let order = consumption_order(photos.len(), wide_index(photos).filter(|_| has_spread));
    let cover = draw_cover_collage(
        photos,
        tw,
        th,
        opts,
        &out_dir.join("cover.jpg"),
        &mut fallbacks,
    );
    let mut cursor = 0usize;
    let mut page_no = 0usize;
    let mut announced: HashSet<usize> = HashSet::new();

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
        let out = out_dir.join(&fname);
        match fp.kind {
            PageKind::Hero | PageKind::PhotoCaption | PageKind::Grid => {
                let slot: Vec<&BookPhoto> = order[cursor..(cursor + fp.take).min(order.len())]
                    .iter()
                    .map(|oi| &photos[*oi])
                    .collect();
                cursor += slot.len();
                let (sw, sh) = slot_inches(opts.preset, fp.kind, fp.take.max(1));
                for p in &slot {
                    placements.push((page_no, dpi_verdict(p.width, p.height, sw, sh)));
                }
                let mut page_caps = page_caption(&slot, fp.kind);
                // The hero opens its chapter: no divider precedes it.
                if fp.kind == PageKind::Hero {
                    page_caps = titled(opening_title(slot[0], chapters, &mut announced), page_caps);
                }
                if !page_caps.is_empty() {
                    captions.entry(page_no).or_default().extend(page_caps);
                }
                match fp.kind {
                    // Caption pages are full-bleed too: the caption burn lays
                    // its strip over the photo's lower edge.
                    PageKind::Hero | PageKind::PhotoCaption => {
                        draw_fullbleed(slot[0], tw, th, &out, &opts.ffmpeg_bin, &mut fallbacks)?
                    }
                    _ => draw_grid(&slot, tw, th, &out, &opts.ffmpeg_bin, &mut fallbacks)?,
                }
            }
            PageKind::SpreadLeft | PageKind::SpreadRight => {
                // The wide photo is order[1]; both halves show its sides.
                let p = &photos[*order
                    .get(1)
                    .unwrap_or(&0)
                    .min(&photos.len().saturating_sub(1))];
                if fp.kind == PageKind::SpreadLeft {
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
                    captions
                        .entry(page_no)
                        .or_default()
                        .extend(titled(title, caption_of(p)));
                    cursor += 1;
                }
                draw_half(
                    p,
                    fp.kind == PageKind::SpreadLeft,
                    tw,
                    th,
                    &out,
                    &opts.ffmpeg_bin,
                    &mut fallbacks,
                )?;
            }
            PageKind::Map => {
                draw_map_page(route_card.unwrap(), tw, th, &out)?;
                captions
                    .entry(page_no)
                    .or_default()
                    .push(crate::titlecard::ROUTE_LABEL.to_string());
            }
            PageKind::Divider => {
                // Consume the background photo: it belongs to this
                // day-marker page and is not repeated later.
                let bg = order.get(cursor).map_or(&photos[0], |&oi| &photos[oi]);
                cursor += fp.take;
                let (sw, sh) = slot_inches(opts.preset, fp.kind, fp.take);
                placements.push((page_no, dpi_verdict(bg.width, bg.height, sw, sh)));
                draw_photo_backed_page(bg, tw, th, &out, &opts.ffmpeg_bin, &mut fallbacks)?;
                let lines = divider_lines(bg, chapters, &mut announced);
                if !lines.is_empty() {
                    captions.entry(page_no).or_default().extend(lines);
                }
            }
            PageKind::Closing => {
                if photos.is_empty() {
                    draw_text_placeholder(fp.kind, tw, th, &out)?;
                } else {
                    let last = &photos[photos.len() - 1];
                    draw_photo_backed_page(last, tw, th, &out, &opts.ffmpeg_bin, &mut fallbacks)?;
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
        opts.progress.counted("book", "page", page_no, total_pages);
    }

    let issues = preflight(opts.preset, &kinds, &placements);
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
    opts.progress.check_cancelled()?;
    opts.progress.step("book", "proof and press PDFs", None);
    let proof =
        match write_typst_and_compile(&jpegs, &captions, opts, out_dir, false, cover.as_deref()) {
            Ok(p) => Some(p),
            Err(e) => {
                // Never leave a previous export's PDF beside the new pages.
                let _ = std::fs::remove_file(out_dir.join("proof.pdf"));
                fallbacks.push(format!("proof.pdf skipped: {e}"));
                None
            }
        };
    let press =
        match write_typst_and_compile(&jpegs, &captions, opts, out_dir, true, cover.as_deref()) {
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
        let mut v: Vec<usize> = captions
            .iter()
            .filter(|(_, l)| !l.is_empty())
            .map(|(k, _)| *k)
            .collect();
        v.sort_unstable();
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
        guide_text(opts.preset, &opts.trip_name, counted, captions_burned),
    )
    .map_err(|e| crate::MediaError::Io(e.to_string()))?;
    let manifest = serde_json::json!({
        "version": 1,
        "preset": opts.preset.name(),
        "trim_px": {"w": tw, "h": th},
        "cover": cover.as_ref().map(|c| c.file_name().unwrap().to_string_lossy().into_owned()),
        "counted_pages": counted,
        "pages": jpegs.iter().enumerate().map(|(i, p)| serde_json::json!({
            "page": i + 1,
            "file": p.file_name().unwrap().to_string_lossy(),
            "kind": kinds[i + 1], // +1: kinds[0] is the uncounted cover-proxy
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
        jpeg_pages: jpegs,
        proof_pdf: proof,
        press_pdf: press,
        guide_path,
        counted_pages: counted,
        fallbacks,
    })
}

/// The title of the chapter `p` opens, once per chapter.
fn opening_title(
    p: &BookPhoto,
    chapters: &[String],
    announced: &mut HashSet<usize>,
) -> Option<String> {
    p.chapter
        .filter(|&c| announced.insert(c))
        .and_then(|c| chapters.get(c).cloned())
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

/// Caption lines for a divider page: its background photo's chapter title
/// plus that photo's own caption, since the photo is consumed here.
fn divider_lines(
    bg: &BookPhoto,
    chapters: &[String],
    announced: &mut HashSet<usize>,
) -> Vec<String> {
    titled(opening_title(bg, chapters, announced), caption_of(bg))
}

/// Caption lines for one photo page. Single-photo pages keep the photo's
/// full caption. Grid pages collapse to ONE dateline (or day range) so a
/// page never repeats near-identical per-photo template text; distinct
/// user prose on a grid keeps at most the first two lines.
fn page_caption(slot: &[&BookPhoto], kind: PageKind) -> Vec<String> {
    let mut caps: Vec<String> = vec![];
    for p in slot {
        for c in caption_of(p) {
            if !caps.contains(&c) {
                caps.push(c);
            }
        }
    }
    if kind != PageKind::Grid || caps.len() <= 1 {
        return caps;
    }
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
    caps.truncate(2);
    caps
}

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

fn load(photo: &BookPhoto, ffmpeg_bin: &str, fallbacks: &mut Vec<String>) -> RgbImage {
    match image::open(&photo.path) {
        // The image crate ignores EXIF orientation (the FFmpeg/sips decode
        // path orients its output), so portrait JPEGs would print sideways
        // without this.
        Ok(d) => {
            let exif = triptape_core::metadata::read_image_exif(&photo.path);
            let t = exif
                .orientation
                .map(triptape_core::metadata::orientation_transform)
                .unwrap_or(triptape_core::metadata::OrientationTransform {
                    mirror_h: false,
                    rotate_cw_deg: 0,
                });
            crate::ffmpeg::orient_image(d, t).to_rgb8()
        }
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

/// Cover-fit `src` into (tw, th), centered.
pub(crate) fn cover_fit(src: &RgbImage, tw: u32, th: u32) -> RgbImage {
    let (sw, sh) = (src.width().max(1) as f64, src.height().max(1) as f64);
    let scale = (tw as f64 / sw).max(th as f64 / sh);
    let (nw, nh) = ((sw * scale).round() as u32, (sh * scale).round() as u32);
    let big = image::imageops::resize(
        src,
        nw.max(1),
        nh.max(1),
        image::imageops::FilterType::Triangle,
    );
    let (x, y) = (nw.saturating_sub(tw) / 2, nh.saturating_sub(th) / 2);
    image::imageops::crop_imm(&big, x.min(nw - 1), y.min(nh - 1), tw.min(nw), th.min(nh)).to_image()
}

fn draw_fullbleed(
    p: &BookPhoto,
    tw: u32,
    th: u32,
    out: &Path,
    ff: &str,
    fb: &mut Vec<String>,
) -> Result<(), crate::MediaError> {
    save_jpg(&cover_fit(&load(p, ff, fb), tw, th), out)
}

fn draw_half(
    p: &BookPhoto,
    left: bool,
    tw: u32,
    th: u32,
    out: &Path,
    ff: &str,
    fb: &mut Vec<String>,
) -> Result<(), crate::MediaError> {
    let full = cover_fit(&load(p, ff, fb), tw * 2, th);
    let x = if left { 0 } else { tw };
    save_jpg(
        &image::imageops::crop_imm(&full, x, 0, tw, th).to_image(),
        out,
    )
}

fn draw_grid(
    slot: &[&BookPhoto],
    tw: u32,
    th: u32,
    out: &Path,
    ff: &str,
    fb: &mut Vec<String>,
) -> Result<(), crate::MediaError> {
    let mut page = solid(tw, th, Rgb([18, 20, 26]));
    let gap = (tw / 100).max(8);
    let cells: Vec<(u32, u32, u32, u32)> = match slot.len() {
        1 => vec![(gap, gap, tw - 2 * gap, th - 2 * gap)],
        2 => vec![
            (gap, gap, tw - 2 * gap, (th - 3 * gap) / 2),
            (gap, (th + gap) / 2, tw - 2 * gap, (th - 3 * gap) / 2),
        ],
        3 => vec![
            (gap, gap, tw - 2 * gap, (th - 3 * gap) / 3),
            (
                gap,
                gap + (th + gap) / 3,
                (tw - 3 * gap) / 2,
                (th - 3 * gap) / 3,
            ),
            (
                gap + (tw + gap) / 2,
                gap + (th + gap) / 3,
                (tw - 3 * gap) / 2,
                (th - 3 * gap) / 3,
            ),
        ],
        _ => {
            let cw = (tw - 3 * gap) / 2;
            let ch = (th - 3 * gap) / 2;
            vec![
                (gap, gap, cw, ch),
                (2 * gap + cw, gap, cw, ch),
                (gap, 2 * gap + ch, cw, ch),
                (2 * gap + cw, 2 * gap + ch, cw, ch),
            ]
        }
    };
    for (p, (x, y, w, h)) in slot.iter().zip(cells) {
        let fit = cover_fit(&load(p, ff, fb), w.max(1), h.max(1));
        image::imageops::replace(&mut page, &fit, x as i64, y as i64);
    }
    save_jpg(&page, out)
}

fn draw_map_page(card: &Path, tw: u32, th: u32, out: &Path) -> Result<(), crate::MediaError> {
    let d = image::open(card).map_err(|e| crate::MediaError::Io(format!("map card: {e}")))?;
    save_jpg(&cover_fit(&d.to_rgb8(), tw, th), out)
}

/// Scale every channel of `pixels` to `pct`% brightness.
fn darken<'a>(pixels: impl Iterator<Item = &'a mut Rgb<u8>>, pct: u16) {
    for px in pixels {
        for c in px.0.iter_mut() {
            *c = (u16::from(*c) * pct / 100) as u8;
        }
    }
}

/// Full-bleed photo darkened to 45% so burned chapter text stays readable.
fn draw_photo_backed_page(
    p: &BookPhoto,
    tw: u32,
    th: u32,
    out: &Path,
    ffmpeg_bin: &str,
    fallbacks: &mut Vec<String>,
) -> Result<(), crate::MediaError> {
    let mut img = cover_fit(&load(p, ffmpeg_bin, fallbacks), tw, th);
    darken(img.pixels_mut(), 45);
    save_jpg(&img, out)
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
        "#set page(width: 11in, height: auto, fill: none, margin: 0in)\n#set text(font: \"DejaVu Sans\", fill: white)\n#align(center)[#text(size: 44pt, weight: \"bold\")[{}]]\n#align(center)[#text(size: 20pt)[{}]]\n",
        typ_escape(title),
        typ_escape(sub)
    );
    crate::titlecard::render_text_png(typst_bin, &doc, scratch_png, 200).ok()?;
    let img = image::open(scratch_png).ok();
    let _ = std::fs::remove_file(scratch_png);
    img
}

/// Build `cover.jpg`: a mosaic of evenly-spaced trip photos with a darkened
/// title band. Returns None (with a fallback note) when no photo decodes.
fn draw_cover_collage(
    photos: &[BookPhoto],
    tw: u32,
    th: u32,
    opts: &BookOptions,
    out: &Path,
    fallbacks: &mut Vec<String>,
) -> Option<PathBuf> {
    if photos.is_empty() {
        return None;
    }
    let picks = evenly_spaced(photos.len(), photos.len().min(12));
    let (cols, rows) = cover_grid(picks.len());
    let gap = 14u32;
    let tile_w = (tw - gap * (cols + 1)) / cols;
    let tile_h = (th - gap * (rows + 1)) / rows;
    let mut canvas = solid(tw, th, Rgb([18, 20, 26]));
    for (k, pi) in picks.iter().enumerate() {
        let (cx, cy) = ((k as u32) % cols, (k as u32) / cols);
        let tile = cover_fit(
            &load(&photos[*pi], &opts.ffmpeg_bin, fallbacks),
            tile_w,
            tile_h,
        );
        image::imageops::replace(
            &mut canvas,
            &tile,
            (gap + cx * (tile_w + gap)) as i64,
            (gap + cy * (tile_h + gap)) as i64,
        );
    }
    // Darken a bottom title band, then composite the Typst title over it.
    let band_h = th / 5;
    darken(
        canvas
            .enumerate_pixels_mut()
            .filter(|(_, y, _)| *y >= th - band_h)
            .map(|(_, _, p)| p),
        25,
    );
    match typst_text_png(
        &opts.typst_bin,
        &opts.trip_name,
        "TripTape Photo Book",
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

fn save_jpg(img: &RgbImage, out: &Path) -> Result<(), crate::MediaError> {
    let f = std::fs::File::create(out)
        .map_err(|e| crate::MediaError::Io(format!("save {}: {e}", out.display())))?;
    image::codecs::jpeg::JpegEncoder::new_with_quality(std::io::BufWriter::new(f), 90)
        .encode_image(img)
        .map_err(|e| crate::MediaError::Io(format!("save {}: {e}", out.display())))
}

// -- Typst -----------------------------------------------------------------

/// Write the Typst source and compile it. `bleed` selects press geometry
/// (trim + 0.125 in each side); otherwise screen proof geometry.
fn write_typst_and_compile(
    jpegs: &[PathBuf],
    captions: &HashMap<usize, Vec<String>>,
    opts: &BookOptions,
    out_dir: &Path,
    bleed: bool,
    cover: Option<&Path>,
) -> Result<PathBuf, crate::MediaError> {
    let (tw_in, th_in) = match opts.preset {
        BookPreset::P1 => (12.0, 12.0),
        BookPreset::P2 => (11.0, 8.5),
    };
    let (pw, ph) = if bleed {
        (tw_in + 0.25, th_in + 0.25)
    } else {
        (tw_in, th_in)
    };
    let pdf_name = if bleed { "press.pdf" } else { "proof.pdf" };
    let typ_name = if bleed { "press.typ" } else { "proof.typ" };
    let mut typ = format!(
        "#set page(width: {pw}in, height: {ph}in, margin: 0in)\n#set text(font: \"DejaVu Sans\", size: 14pt, fill: white)\n"
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
    for (i, jpg) in jpegs.iter().enumerate() {
        let page_no = i + 1;
        typ.push_str(&format!(
            "#rect(width: 100%, height: 100%, fill: rgb(18, 20, 26))\n#place(center + horizon)[#image(\"{}\", width: 100%, height: 100%, fit: \"cover\")]\n",
            jpg.file_name().unwrap().to_string_lossy()
        ));
        if let Some(lines) = captions.get(&page_no).filter(|l| !l.is_empty()) {
            typ.push_str(&format!(
                "#place(bottom + center)[#rect(fill: rgb(18, 20, 26).transparentize(20%), inset: 8pt)[{}]]\n",
                typ_escape(&lines.join(" · "))
            ));
        }
        if i + 1 < jpegs.len() {
            typ.push_str("#pagebreak()\n");
        }
    }
    let typ_path = out_dir.join(typ_name);
    std::fs::write(&typ_path, &typ).map_err(|e| crate::MediaError::Io(e.to_string()))?;
    let pdf = out_dir.join(pdf_name);
    let st = Command::new(&opts.typst_bin)
        .args([
            "compile",
            &typ_path.to_string_lossy(),
            &pdf.to_string_lossy(),
        ])
        .output()
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
/// matching page JPEGs. Proof page N+1 is interior page N (page 1 is the
/// uncounted cover-proxy); proof geometry equals trim, so sizes match.
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
    let pages = captioned
        .iter()
        .map(|n| (n + 1).to_string())
        .collect::<Vec<_>>()
        .join(",");
    let result = (|| {
        let st = Command::new(&opts.typst_bin)
            .args([
                "compile",
                &out_dir.join("proof.typ").to_string_lossy(),
                &raster.join("p{p}.png").to_string_lossy(),
                "--ppi",
                "300",
                "--pages",
                &pages,
            ])
            .output()
            .map_err(|e| crate::MediaError::Io(format!("spawn {}: {e}", opts.typst_bin)))?;
        if !st.status.success() {
            return Err(crate::MediaError::Io(format!(
                "typst raster failed: {}",
                String::from_utf8_lossy(&st.stderr).trim()
            )));
        }
        for &n in captioned {
            let png = raster.join(format!("p{}.png", n + 1));
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

fn typ_escape(t: &str) -> String {
    t.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace('#', "\\#")
}

fn guide_text(preset: BookPreset, trip: &str, counted: usize, captions_burned: bool) -> String {
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
        cover.jpg is a suggested photo-collage cover (not counted); use it as the vendor cover or design your own in the vendor designer.\n\
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
    fn darken_scales_channels() {
        let mut img = RgbImage::from_pixel(2, 2, Rgb([200, 100, 0]));
        darken(img.pixels_mut(), 45);
        assert!(img.pixels().all(|p| *p == Rgb([90, 45, 0])));
        // Bottom-row-only darken (cover title band).
        let mut img = RgbImage::from_pixel(1, 2, Rgb([100, 100, 100]));
        darken(
            img.enumerate_pixels_mut()
                .filter(|(_, y, _)| *y >= 1)
                .map(|(_, _, p)| p),
            25,
        );
        assert_eq!(*img.get_pixel(0, 0), Rgb([100, 100, 100]));
        assert_eq!(*img.get_pixel(0, 1), Rgb([25, 25, 25]));
    }

    #[test]
    fn grid_caption_collapses_to_one_dateline() {
        let mk = |cap: &str| BookPhoto {
            asset_id: cap.to_string(),
            path: PathBuf::from("/x.jpg"),
            width: 100,
            height: 100,
            caption: Some(cap.to_string()),
            chapter: None,
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

    /// The chapters behind each page: (kind, chapter of each consumed photo).
    fn pages_with_chapters(
        chapter_of: &[Option<usize>],
        wide_idx: Option<usize>,
        cap: usize,
    ) -> Vec<(PageKind, Vec<Option<usize>>)> {
        let flow = flow_pages(chapter_of, wide_idx, false, cap);
        let has_spread = flow.iter().any(|p| p.kind == PageKind::SpreadLeft);
        let order = consumption_order(chapter_of.len(), wide_idx.filter(|_| has_spread));
        let mut cursor = 0;
        flow.iter()
            .map(|p| {
                let taken = order[cursor..cursor + p.take]
                    .iter()
                    .map(|&i| chapter_of[i])
                    .collect();
                cursor += p.take;
                (p.kind, taken)
            })
            .collect()
    }

    #[test]
    fn dividers_interleave_never_stack() {
        let kinds: Vec<PageKind> = flow_pages(&even_chapters(200, 20), None, false, 60)
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
        let flow = flow_pages(&even_chapters(200, 20), None, false, 60);
        let takes: usize = flow.iter().map(|p| p.take).sum();
        assert_eq!(takes, 200, "every photo placed exactly once");
        // Small books and divider-heavy trips: still every photo, once.
        for n in 0..40 {
            for d in 0..10 {
                for wide in [None, Some(1)] {
                    let wide = wide.filter(|_| n > 1);
                    let flow = flow_pages(&even_chapters(n, d), wide, false, 60);
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
            caption: None,
            chapter: None,
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
        let flow = flow_pages(&chapter_of, None, false, 60);
        let dividers = flow.iter().filter(|p| p.kind == PageKind::Divider).count();
        assert_eq!(dividers, 1);
        // The spread's wide photo uses up chapter 1: only chapter 2 remains.
        let chapter_of = sized_chapters(&[3, 1, 3]);
        let pages = pages_with_chapters(&chapter_of, Some(3), 60);
        let divided: Vec<Option<usize>> = pages
            .iter()
            .filter(|(k, _)| *k == PageKind::Divider)
            .map(|(_, taken)| taken[0])
            .collect();
        assert_eq!(divided, [Some(2)]);
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
                path: PathBuf::new(),
                width: if i == 1 { 6000 } else { 4000 },
                height: 3000,
                caption: None,
                chapter: chapters[i],
            })
            .collect();
        let keep = fitting_count(&pool, true, 60);
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
        let idx = evenly_spaced(901, 10);
        assert_eq!(idx.first(), Some(&0), "keeps the opener");
        assert!(idx.windows(2).all(|w| w[0] < w[1]), "story order preserved");
        assert!(*idx.last().unwrap() > 800, "spans to the end of the trip");
    }

    #[test]
    fn cap_selection_reserves_captioned_photos() {
        let mk = |i: usize, cap: bool| BookPhoto {
            asset_id: format!("a{i}"),
            path: PathBuf::new(),
            width: 4000,
            height: 3000,
            caption: cap.then(|| "note".to_string()),
            chapter: None,
        };
        let photos: Vec<BookPhoto> = (0..100).map(|i| mk(i, i == 37 || i == 38)).collect();
        let (sel, dropped) = select_for_cap(&photos, 10);
        assert_eq!(sel.len(), 10);
        assert!(sel.contains(&0) && sel.contains(&37) && sel.contains(&38));
        assert!(sel.windows(2).all(|w| w[0] < w[1]), "story order preserved");
        assert!(dropped.is_empty());

        let all_cap: Vec<BookPhoto> = (0..20).map(|i| mk(i, true)).collect();
        let (sel, dropped) = select_for_cap(&all_cap, 5);
        assert_eq!(sel.len(), 5);
        assert_eq!(
            sel.len() + dropped.len(),
            20,
            "every captioned omission reported"
        );
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
        let flow = flow_pages(&[None; 12], Some(1), false, 60);
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
        let flow = flow_pages(&even_chapters(30, 2), None, false, 60);
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
    fn divider_lines_match_background_chapter_and_keep_prose() {
        let photo = |chapter: Option<usize>, caption: Option<&str>| BookPhoto {
            asset_id: "p".into(),
            path: PathBuf::new(),
            width: 100,
            height: 100,
            caption: caption.map(String::from),
            chapter,
        };
        let titles = ["Tokyo".to_string(), "Kyoto".to_string()];
        let mut seen = HashSet::new();
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
    fn planned_blocked_accounts_for_divider_backgrounds() {
        let photo = |id: &str, px: u32, chapter: usize| BookPhoto {
            asset_id: id.into(),
            path: PathBuf::from(id),
            width: px,
            height: px,
            caption: None,
            chapter: Some(chapter),
        };
        // Flow: hero (p0), divider background (p1), photo page (p2).
        // 1200px clears the smallest slot's 200 dpi but prints at 100 dpi full-page.
        let low_res_background = [
            photo("p0", 8000, 0),
            photo("p1", 1200, 1),
            photo("p2", 8000, 1),
        ];
        assert_eq!(
            planned_blocked(&low_res_background, BookPreset::P1, false),
            vec!["p1".to_string()]
        );
        let low_res_page = [
            photo("p0", 8000, 0),
            photo("p1", 8000, 1),
            photo("p2", 10, 1),
        ];
        assert_eq!(
            planned_blocked(&low_res_page, BookPreset::P1, false),
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
            caption: None,
            chapter: None,
        };
        // Flow: hero, then one caption page. Each size clears 200 dpi inside
        // the photo area an inset layout would have, but not at full bleed.
        for (preset, w, h, inset) in [
            (BookPreset::P1, 2400, 2000, (11.0, 9.0)),
            (BookPreset::P2, 2000, 1500, (10.0, 6.0)),
        ] {
            assert_ne!(dpi_verdict(w, h, inset.0, inset.1), DpiVerdict::Block);
            let photos = [photo("hero", 8000, 8000), photo("soft", w, h)];
            assert_eq!(planned_blocked(&photos, preset, false), vec!["soft"]);
        }
    }

    #[test]
    fn flow_packs_large_sets_into_grids() {
        let flow = flow_pages(&[None; 200], None, false, 60);
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
        let issues = preflight(BookPreset::P1, &over, &[]);
        assert!(issues
            .iter()
            .any(|i| i.level == "block" && i.message.contains("cap")));
        let tiny = vec![PageKind::Hero, PageKind::Closing];
        let issues = preflight(BookPreset::P1, &tiny, &[]);
        assert!(issues
            .iter()
            .any(|i| i.level == "block" && i.message.contains("minimum")));
        let ok_kinds = vec![PageKind::Hero; 30];
        let issues = preflight(
            BookPreset::P1,
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
                chapter: None,
            })
            .collect();
        let out = dir.path().join("book");
        let opts = BookOptions {
            preset: BookPreset::P1,
            trip_name: "Test".into(),
            typst_bin: "typst".into(),
            ffmpeg_bin: "ffmpeg".into(),
            progress: Default::default(),
        };
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
                    caption: None,
                    chapter,
                }
            })
            .collect();
        let titles: Vec<String> = ["Lisbon", "Sintra", "Porto", "Braga"]
            .map(String::from)
            .to_vec();
        let out = dir.path().join("book");
        let opts = BookOptions {
            preset: BookPreset::P1,
            trip_name: "Test".into(),
            typst_bin: "typst-not-installed".into(),
            ffmpeg_bin: "ffmpeg".into(),
            progress: Default::default(),
        };
        render_book(&photos, None, &titles, &out, &opts).unwrap();
        let typ = std::fs::read_to_string(out.join("proof.typ")).unwrap();
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
    fn preset_geometry() {
        assert_eq!(BookPreset::P1.trim_px(), (3600, 3600), "12in at 300dpi");
        assert_eq!(BookPreset::P2.trim_px(), (3300, 2550), "11x8.5in at 300dpi");
        assert_eq!(BookPreset::from_flag("p1"), Some(BookPreset::P1));
        assert_eq!(BookPreset::from_flag("P2"), Some(BookPreset::P2));
        assert_eq!(BookPreset::from_flag("A4"), None);
    }
}
