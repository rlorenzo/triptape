//! Keepsake 1 — Trip Video (hero film).
//!
//! Pipeline: normalize every plan entry to a uniform 1080p30/s48kHz-stereo
//! segment (Ken Burns stills, scored clip trims, title card, map push-in),
//! then assemble with a stepwise pairwise crossfade chain (never 5+ inputs
//! in one graph; each offset accounts for cumulative overlap), a final
//! loudnorm single pass, 1080p libx264 CRF19 faststart, an EDL JSON, and a
//! sidecar SRT (burned-in captions need libass, absent on this build).

use crate::ffmpeg::{self, path_arg as s, Capabilities};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use triptape_core::plan::{MAX_CLIP_SECS, MAX_COLLAGE, MIN_SEGMENT_SECS};
use triptape_core::types::{Plan, PlanEntry, Rendition};

/// Crossfade applied at the cut *into* a segment (PLAN.md Sec 9, 6-pack).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Transition {
    Fade,
    Dissolve,
    /// Signature look: dissolve filter; the blur ramp is omitted on builds
    /// without per-tail filter resources (recorded in the EDL/fallbacks).
    BlurDissolve,
    FadeBlack,
    FadeWhite,
    SmoothLeft,
    SmoothRight,
    CircleOpen,
    CircleClose,
}

impl Transition {
    /// Concrete `xfade` filter name (all supported by stock FFmpeg).
    pub fn xfade_name(&self) -> &'static str {
        match self {
            Transition::Fade => "fade",
            Transition::Dissolve | Transition::BlurDissolve => "dissolve",
            Transition::FadeBlack => "fadeblack",
            Transition::FadeWhite => "fadewhite",
            Transition::SmoothLeft => "smoothleft",
            Transition::SmoothRight => "smoothright",
            Transition::CircleOpen => "circleopen",
            Transition::CircleClose => "circleclose",
        }
    }
}

/// Pure xfade timeline math (uniform overlap): cut `i` (into segment
/// `i+1`) sits at `sum(durations[0..=i]) - overlap*(i+1)`. Stepwise assembly
/// must use these offsets so overlap never compounds.
pub fn xfade_offsets(durations: &[f64], overlap: f64) -> Vec<f64> {
    let n = durations.len().saturating_sub(1);
    xfade_offsets_var(durations, &vec![overlap; n])
}

/// Per-cut-overlap timeline math: cut `i` sits at
/// `sum(durations[0..=i]) - sum(overlaps[0..=i])`. A missing overlap entry
/// falls back to 0.5 s.
pub fn xfade_offsets_var(durations: &[f64], overlaps: &[f64]) -> Vec<f64> {
    let mut offsets = vec![];
    let mut acc = 0.0;
    let mut ov = 0.0;
    for (i, d) in durations.iter().enumerate() {
        acc += d;
        if i + 1 < durations.len() {
            ov += overlaps.get(i).copied().unwrap_or(0.5);
            offsets.push(acc - ov);
        }
    }
    offsets
}

/// Per-cut overlap for a transition: slow fades get room to breathe,
/// everything else takes the 0.5 s standard.
pub fn transition_overlap(t: &Transition) -> f64 {
    match t {
        Transition::FadeBlack => 0.8,
        Transition::CircleOpen | Transition::CircleClose => 0.7,
        _ => 0.5,
    }
}

/// What a segment shows, for kind-change transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegKind {
    Title,
    Map,
    Still,
    Clip,
}

/// Assign one transition per cut from story position.
///
/// * `chapter_break[i]` — cut into segment `i` starts a new chapter.
/// * `kinds` — segment kinds; a kind change dissolves (shorter than `n`
///   is fine: missing entries never count as a change).
/// * First cut opens with `circleopen`, final cut closes with `circleclose`.
/// * Chapter breaks cycle `fadeblack, smoothleft, smoothleft`, so slow
///   fades stay rare (max ~1 in 3) however many micro-chapters exist.
/// * The blur-dissolve signature is capped at 3.
/// * Everything else rotates `fade/dissolve/smoothleft/smoothright/
///   circleopen`, never repeating the same xfade filter twice in a row
///   (special picks that would repeat fall back to the rotation).
pub fn assign_transitions(
    n_segments: usize,
    chapter_break: &[bool],
    kinds: &[SegKind],
) -> Vec<Option<Transition>> {
    const ROT: [Transition; 5] = [
        Transition::Fade,
        Transition::Dissolve,
        Transition::SmoothLeft,
        Transition::SmoothRight,
        Transition::CircleOpen,
    ];
    let mut out: Vec<Option<Transition>> = vec![None; n_segments];
    if n_segments <= 1 {
        return out;
    }
    let mut prev: Option<&'static str> = None;
    let mut sig_used = 0u32;
    let mut break_n = 0u32;
    let mut rot = 0usize;
    // Next rotation pick whose filter differs from `prev`.
    let next_rot = |prev: Option<&str>, rot: &mut usize| -> Transition {
        for _ in 0..ROT.len() {
            let t = ROT[*rot % ROT.len()];
            *rot += 1;
            if Some(t.xfade_name()) != prev {
                return t;
            }
        }
        Transition::Fade
    };
    for (i, slot) in out.iter_mut().enumerate().skip(1) {
        // Special pick for this story position; `None` means "rotate".
        let special = if i == 1 {
            Some(Transition::CircleOpen)
        } else if i == n_segments - 1 && n_segments > 2 {
            Some(Transition::CircleClose)
        } else if chapter_break.get(i).copied().unwrap_or(false) {
            break_n += 1;
            Some(if break_n % 3 == 1 {
                Transition::FadeBlack
            } else {
                Transition::SmoothLeft
            })
        } else if kinds.get(i) != kinds.get(i - 1) {
            Some(Transition::Dissolve)
        } else if sig_used < 3 && i % 4 == 2 {
            Some(Transition::BlurDissolve)
        } else {
            None
        };
        // A special pick that would repeat the previous filter rotates too.
        let t = match special {
            Some(c) if Some(c.xfade_name()) != prev => c,
            _ => next_rot(prev, &mut rot),
        };
        if t == Transition::BlurDissolve {
            sig_used += 1;
        }
        *slot = Some(t);
        prev = Some(t.xfade_name());
    }
    out
}

/// Bundled fallback score (`resources/music/triptape-theme.mp3`): the hero
/// film always has a music bed, even when the user passes no `--music`.
static DEFAULT_MUSIC_BYTES: &[u8] =
    include_bytes!("../../../../resources/music/triptape-theme.mp3");

/// Resolved source files for one plan entry (export layer fills this).
#[derive(Debug, Clone)]
pub struct ResolvedEntry {
    pub entry: PlanEntry,
    pub still_path: PathBuf,
    pub motion_path: Option<PathBuf>,
    pub caption: Option<String>,
    /// Capture time (unix secs) for date lower-thirds; `None` skips the date.
    pub taken_at: Option<i64>,
    /// UTC offset at capture (seconds) for local-time display.
    pub tz_offset_secs: Option<i32>,
}

#[derive(Debug, Clone)]
pub struct VideoOptions {
    pub trip_name: String,
    pub short_mode: bool,
    pub still_secs: f64,
    pub overlap_secs: f64,
    pub title_secs: f64,
    pub map_secs: f64,
    pub music_path: Option<PathBuf>,
    /// Keep the original film audio at 0.12 under a ducked music bed.
    /// Default (false) mutes it: the music is the primary audio.
    pub keep_ambience: bool,
    /// Typst binary for title/lower-third text PNGs.
    pub typst_bin: String,
}

impl Default for VideoOptions {
    fn default() -> Self {
        Self {
            trip_name: "TripTape Film".into(),
            short_mode: false,
            still_secs: 4.0,
            overlap_secs: 0.6,
            title_secs: 2.5,
            map_secs: 3.0,
            music_path: None,
            keep_ambience: false,
            typst_bin: "typst".into(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct EdlSegment {
    pub index: usize,
    pub asset_id: String,
    pub kind: String,
    pub segment_secs: f64,
    /// Every asset shown in a collage segment (`asset_id` is its lead).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub collage_asset_ids: Vec<String>,
    pub transition_in: Option<Transition>,
    pub offset_secs: Option<f64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Edl {
    pub version: u32,
    pub trip_name: String,
    pub overlap_secs: f64,
    /// Per-cut overlap actually used (len = segments - 1).
    pub cut_overlaps_secs: Vec<f64>,
    pub segments: Vec<EdlSegment>,
    pub total_secs: f64,
    pub captions_sidecar: Option<String>,
    pub fallbacks: Vec<String>,
}

#[derive(Debug)]
pub struct VideoOutput {
    pub path: PathBuf,
    pub edl_path: PathBuf,
    pub srt_path: Option<PathBuf>,
    pub duration_secs: f64,
    pub fallbacks: Vec<String>,
}

/// Hard ceiling for a short film (PLAN.md: "short" is <=30 s).
const SHORT_FILM_SECS: f64 = 30.0;
/// Closing places card hold.
const CREDITS_SECS: f64 = 4.5;
/// Closing map recap runs this much longer than the opening map.
const CLOSING_MAP_EXTRA_SECS: f64 = 1.5;

/// Seconds the closing cards add, reserved up front in short mode.
fn closing_secs(has_route: bool, has_places: bool, map_secs: f64) -> f64 {
    let map = if has_route {
        map_secs + CLOSING_MAP_EXTRA_SECS
    } else {
        0.0
    };
    map + if has_places { CREDITS_SECS } else { 0.0 }
}

/// Choose at most `slots` entries for a short film so every chapter is seen,
/// instead of spending the whole budget on the first day. Slots are dealt to
/// chapters in turn, each chapter's picks are spaced evenly through it, and
/// the result keeps the plan's order.
fn spread_across_chapters<'a>(
    entries: &[&'a ResolvedEntry],
    slots: usize,
    chapter_of: impl Fn(&str) -> usize,
) -> Vec<&'a ResolvedEntry> {
    if entries.len() <= slots {
        return entries.to_vec();
    }
    let mut chapters: Vec<(usize, Vec<usize>)> = vec![];
    for (i, e) in entries.iter().enumerate() {
        let ch = chapter_of(&e.entry.asset_id);
        match chapters.iter_mut().find(|(c, _)| *c == ch) {
            Some((_, members)) => members.push(i),
            None => chapters.push((ch, vec![i])),
        }
    }
    let mut counts = vec![0usize; chapters.len()];
    let mut left = slots;
    while left > 0 {
        for (count, (_, members)) in counts.iter_mut().zip(&chapters) {
            if left > 0 && *count < members.len() {
                *count += 1;
                left -= 1;
            }
        }
    }
    let mut picked: Vec<usize> = chapters
        .iter()
        .zip(&counts)
        .flat_map(|((_, members), &k)| {
            (0..k).map(move |i| members[(2 * i + 1) * members.len() / (2 * k)])
        })
        .collect();
    picked.sort_unstable();
    picked.into_iter().map(|i| entries[i]).collect()
}

/// The most one segment may take of the remaining budget, so a long clip early
/// in a short film can't use up the time later chapters need.
fn fair_share(budget: f64, segments_left: usize) -> f64 {
    budget / segments_left.max(1) as f64
}

/// Sidecar text for a collage: each member's caption on its own line.
fn collage_caption(members: &[&ResolvedEntry]) -> Option<String> {
    let lines: Vec<&str> = members
        .iter()
        .filter_map(|m| m.caption.as_deref())
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// Synthetic segment ids for fixed cards in the hero's segment list.
fn is_card(id: &str) -> bool {
    matches!(id, "title" | "map" | "closing" | "credits")
}

/// Segment entry for a fixed card (not backed by a project asset).
fn card_entry(id: &str, path: &Path) -> ResolvedEntry {
    ResolvedEntry {
        entry: PlanEntry {
            asset_id: id.into(),
            content_hash: None,
            size_bytes: 0,
            mtime: 0,
            rendition: Rendition::Still,
        },
        still_path: path.to_path_buf(),
        motion_path: None,
        caption: None,
        taken_at: None,
        tz_offset_secs: None,
    }
}

/// Render a full-frame Typst card, falling back to the styled solid when
/// Typst is unavailable or fails. Returns whether the text card rendered.
fn render_card_png(
    opts: &VideoOptions,
    typst_ok: bool,
    doc: &str,
    png: &Path,
    label: &str,
    fallbacks: &mut Vec<String>,
) -> Result<bool, crate::MediaError> {
    if typst_ok {
        match crate::titlecard::render_text_png(&opts.typst_bin, doc, png, 72) {
            Ok(()) => return Ok(true),
            Err(e) => fallbacks.push(format!("{label} text failed ({e}); solid card")),
        }
    }
    crate::titlecard::save_title_card(&opts.trip_name, 1920, 1080, png)?;
    Ok(false)
}

/// Render the hero film. `work_dir` holds intermediates (deleted by the
/// caller on cancel); outputs land directly in their final paths.
pub fn render_hero_video(
    caps: &Capabilities,
    plan: &Plan,
    resolved: &[ResolvedEntry],
    map_card: Option<&Path>,
    work_dir: &Path,
    out_mp4: &Path,
    opts: &VideoOptions,
) -> Result<VideoOutput, crate::MediaError> {
    if !caps.libx264 {
        return Err(crate::MediaError::Ffmpeg(
            "libx264 missing; no hero encode".into(),
        ));
    }
    if !caps.xfade {
        return Err(crate::MediaError::Ffmpeg(
            "xfade missing; no hero assembly".into(),
        ));
    }
    std::fs::create_dir_all(work_dir).map_err(|e| crate::MediaError::Io(e.to_string()))?;
    let mut fallbacks: Vec<String> = vec![];
    if !caps.full_tonemap() && caps.tonemap {
        fallbacks.push("zscale absent: HDR tone-map uses tonemap alone".into());
    }
    if !caps.perspective {
        fallbacks.push("perspective absent: stills, clips and maps hold without motion".into());
    }
    if !caps.can_burn_captions() {
        fallbacks.push("libass absent: captions as sidecar SRT, not burned in".into());
    }
    let typst_ok = crate::titlecard::typst_available(&opts.typst_bin);
    if !typst_ok {
        fallbacks.push(format!(
            "typst unavailable ({}): solid title card and no overlay text",
            opts.typst_bin
        ));
    }

    // Chapter breaks: cut into segment i when its chapter differs.
    let chapter_of = |id: &str| {
        plan.chapters
            .iter()
            .position(|c| c.asset_ids.iter().any(|a| a == id))
            .unwrap_or(usize::MAX)
    };

    // 1. Normalize segments (skip missing sources, never fail the film).
    let still_secs = if opts.short_mode {
        3.0
    } else {
        opts.still_secs
    };
    let mut segs: Vec<(ResolvedEntry, PathBuf, f64, String)> = vec![];
    // Title card first: Typst full-frame card (trip name + date range)
    // when available, else the styled solid.
    let title_png = work_dir.join("title.png");
    let date_range = crate::titlecard::format_date_range(
        &resolved
            .iter()
            .filter_map(|r| r.taken_at.map(|t| (t, r.tz_offset_secs)))
            .collect::<Vec<_>>(),
    );
    let title_is_text = render_card_png(
        opts,
        typst_ok,
        &crate::titlecard::title_card_doc(&opts.trip_name, date_range.as_deref()),
        &title_png,
        "title",
        &mut fallbacks,
    )?;
    if title_is_text {
        fallbacks.push(format!(
            "title card shows trip name{}",
            date_range
                .as_deref()
                .map_or(String::new(), |d| format!(" + {d}"))
        ));
    }
    let title_mp4 = work_dir.join("seg_title.mp4");
    render_hold_segment(caps, &title_png, opts.title_secs, 0, &title_mp4)?;
    segs.push((
        card_entry("title", &title_png),
        title_mp4,
        opts.title_secs,
        "title".into(),
    ));

    // Map push-in when GPS exists and a card was rendered.
    if let Some(card) = map_card {
        if !plan.map_polyline.is_empty() {
            let map_mp4 = work_dir.join("seg_map.mp4");
            match crate::map::render_map_push_in(caps, card, &map_mp4, opts.map_secs, 0) {
                Ok(()) => segs.push((
                    card_entry("map", card),
                    map_mp4,
                    opts.map_secs,
                    "map".into(),
                )),
                Err(e) => fallbacks.push(format!("map segment skipped: {e}")),
            }
        }
    }

    let mut places: Vec<String> = vec![];
    for c in &plan.chapters {
        if let Some(p) = &c.place {
            if !places.contains(p) {
                places.push(p.clone());
            }
        }
    }

    // Hero entries (short mode: cap the whole film at 30 s).
    // Fixed segments (title, optional map, closing cards) come out of the
    // budget first; each entry is checked and clip-capped before it is
    // encoded.
    let mut budget = if opts.short_mode {
        let has_route = map_card.is_some() && !plan.map_polyline.is_empty();
        SHORT_FILM_SECS
            - segs.iter().map(|s| s.2).sum::<f64>()
            - closing_secs(has_route, !places.is_empty(), opts.map_secs)
    } else {
        f64::INFINITY
    };
    // Group consecutive same-chapter stills into collages of up to four,
    // so a day shows several photos at once instead of a single pick.
    // Clips and Live Photo motion always stay solo.
    let mut groups: Vec<Vec<&ResolvedEntry>> = vec![];
    let mut collage_members: HashMap<String, Vec<String>> = HashMap::new();
    let selected: Vec<&ResolvedEntry> = if opts.short_mode {
        let slots = (budget / still_secs).floor().max(1.0) as usize;
        let on_disk: Vec<&ResolvedEntry> = resolved
            .iter()
            .filter(|r| {
                let ok =
                    r.still_path.exists() || r.motion_path.as_ref().is_some_and(|p| p.exists());
                if !ok {
                    fallbacks.push(format!("{} missing on disk; skipped", r.entry.asset_id));
                }
                ok
            })
            .collect();
        let chosen = spread_across_chapters(&on_disk, slots, chapter_of);
        if chosen.len() < on_disk.len() {
            fallbacks.push(format!(
                "short variant: {} of {} picks spread across the trip to fit ~30 s",
                chosen.len(),
                on_disk.len()
            ));
        }
        chosen
    } else {
        resolved.iter().collect()
    };
    for r in selected {
        let is_still = |e: &ResolvedEntry| matches!(e.entry.rendition, Rendition::Still);
        match groups.last_mut() {
            Some(g)
                if !opts.short_mode
                    && g.len() < MAX_COLLAGE
                    && is_still(r)
                    && is_still(g[0])
                    && chapter_of(&g[0].entry.asset_id) == chapter_of(&r.entry.asset_id) =>
            {
                g.push(r);
            }
            _ => groups.push(vec![r]),
        }
    }
    for (idx, group) in groups.iter().enumerate() {
        let present: Vec<&ResolvedEntry> = group
            .iter()
            .copied()
            .filter(|r| {
                let ok =
                    r.still_path.exists() || r.motion_path.as_ref().is_some_and(|p| p.exists());
                if !ok {
                    fallbacks.push(format!("{} missing on disk; skipped", r.entry.asset_id));
                }
                ok
            })
            .collect();
        let Some(&r) = present.first() else { continue };
        // Below the 0.5 s clip floor nothing more fits.
        if budget < 0.5 {
            fallbacks.push(format!(
                "short variant: {} and later dropped to fit ~30 s",
                r.entry.asset_id
            ));
            break;
        }
        let rendered = if present.len() == 1 {
            let cap = fair_share(budget, groups.len() - idx);
            render_entry_segment(caps, r, idx, still_secs, cap, work_dir, &mut fallbacks)
        } else {
            render_collage_segment(caps, &present, idx, still_secs, work_dir)
        };
        let (seg_path, dur, kind) = match rendered {
            Ok(v) => v,
            Err(e) => {
                fallbacks.push(format!("{} skipped: {e}", r.entry.asset_id));
                continue;
            }
        };
        // Record the actual encoded duration: Live Photo trims and short
        // sources routinely encode shorter than planned, and an xfade offset
        // past the accumulated length truncates the whole chain (the film's
        // video ended at the title while music played on). Budget, overlays
        // and timeline math all read this value.
        let probed = ffmpeg::ffprobe_file(&caps.ffprobe_bin, &seg_path)
            .ok()
            .map(|p| p.duration_secs)
            .filter(|d| *d > 0.0)
            .unwrap_or(dur);
        // Flash frames are unwatchable: a sub-2.5s clip (most Live Photo
        // motions) is freeze-padded on its last frame to a minimum hold.
        let dur = match clip_pad_target(probed, budget).filter(|_| kind == "clip") {
            Some(target) => match pad_segment_freeze(caps, &seg_path, target) {
                Ok(()) => target,
                Err(e) => {
                    fallbacks.push(format!("{} freeze-pad failed ({e})", r.entry.asset_id));
                    probed
                }
            },
            None => probed,
        };
        budget -= dur;
        // A collage segment carries every member's caption and id, so the
        // sidecar and EDL account for all the photos on screen.
        let mut seg_entry = r.clone();
        if present.len() > 1 {
            seg_entry.caption = collage_caption(&present);
            collage_members.insert(
                r.entry.asset_id.clone(),
                present.iter().map(|m| m.entry.asset_id.clone()).collect(),
            );
        }
        segs.push((seg_entry, seg_path, dur, kind));
    }
    if segs.len() <= 1 {
        return Err(crate::MediaError::Ffmpeg(
            "no usable hero segments (all sources missing)".into(),
        ));
    }

    // Closing: pull back over the route card for a map recap, then a
    // places card, so the film ends deliberately instead of stopping on
    // the last photo. Both are fixed cards, like the title.
    if let Some(card) = map_card {
        if !plan.map_polyline.is_empty() {
            let close_mp4 = work_dir.join("seg_closing.mp4");
            let dur = opts.map_secs + CLOSING_MAP_EXTRA_SECS;
            match crate::map::render_map_push_in(caps, card, &close_mp4, dur, 1) {
                Ok(()) => segs.push((card_entry("closing", card), close_mp4, dur, "still".into())),
                Err(e) => fallbacks.push(format!("closing map skipped: {e}")),
            }
        }
    }
    if !places.is_empty() {
        let png = work_dir.join("places.png");
        render_card_png(
            opts,
            typst_ok,
            &crate::titlecard::places_card_doc(&opts.trip_name, &places, date_range.as_deref()),
            &png,
            "places card",
            &mut fallbacks,
        )?;
        let places_mp4 = work_dir.join("seg_places.mp4");
        render_hold_segment(caps, &png, CREDITS_SECS, segs.len(), &places_mp4)?;
        segs.push((
            card_entry("credits", &png),
            places_mp4,
            CREDITS_SECS,
            "still".into(),
        ));
    }

    // 2. Chapter starts: cut into segment i when its chapter differs.
    let mut chapter_break = vec![false; segs.len()];
    for i in 1..segs.len() {
        let (a, b) = (&segs[i - 1].0.entry.asset_id, &segs[i].0.entry.asset_id);
        let cut_card = |s: &str| is_card(s) && s != "title";
        chapter_break[i] =
            cut_card(a) || cut_card(b) || (a != "title" && chapter_of(a) != chapter_of(b));
    }

    // A segment opens its chapter when the previous content segment (title
    // and map cards skipped) is in another chapter or there is none. Kept
    // apart from `chapter_break`, which is false for the cut out of the
    // title card, so the first chapter still gets its title overlay.
    let mut chapter_start = vec![false; segs.len()];
    let mut prev_chapter: Option<usize> = None;
    for (i, seg) in segs.iter().enumerate() {
        let id = seg.0.entry.asset_id.as_str();
        if is_card(id) {
            continue;
        }
        let ch = chapter_of(id);
        chapter_start[i] = prev_chapter != Some(ch);
        prev_chapter = Some(ch);
    }

    // 3. Context overlays (Typst lower-thirds baked per segment so they
    // ride through the xfade chain): the day + place stated ONCE at each
    // chapter's first segment, and a label on the route card so it reads
    // as a map. Segments inside a chapter stay clean.
    if typst_ok {
        let chapter_of_entry: HashMap<&str, &triptape_core::types::Chapter> = plan
            .chapters
            .iter()
            .flat_map(|c| c.asset_ids.iter().map(move |id| (id.as_str(), c)))
            .collect();
        for (i, (r, seg_mp4, dur, _)) in segs.iter().enumerate() {
            let id = r.entry.asset_id.as_str();
            if id == "title" || id == "credits" {
                continue;
            }
            let lines = if id == "map" || id == "closing" {
                Some((
                    crate::titlecard::ROUTE_LABEL.to_string(),
                    date_range.clone(),
                ))
            } else if chapter_start[i] {
                chapter_of_entry
                    .get(id)
                    .and_then(|ch| crate::titlecard::chapter_lines(ch))
            } else {
                None
            };
            let Some((top, bottom)) = lines else { continue };
            let doc = crate::titlecard::lower_third_doc(&top, bottom.as_deref());
            let png = work_dir.join(format!("ov_{i:03}.png"));
            if let Err(e) = crate::titlecard::render_text_png(&opts.typst_bin, &doc, &png, 72)
                .and_then(|()| apply_lower_third(caps, seg_mp4, &png, *dur))
            {
                fallbacks.push(format!("overlay skipped for {id}: {e}"));
            }
        }
    }

    // 4. Transitions + per-cut overlaps.
    let kinds: Vec<SegKind> = segs
        .iter()
        .map(|s| match s.3.as_str() {
            "title" => SegKind::Title,
            "map" => SegKind::Map,
            "clip" => SegKind::Clip,
            _ => SegKind::Still,
        })
        .collect();
    let transitions = assign_transitions(segs.len(), &chapter_break, &kinds);
    let durations: Vec<f64> = segs.iter().map(|s| s.2).collect();
    let cut_overlaps: Vec<f64> = transitions
        .iter()
        .skip(1)
        .map(|t| transition_overlap(&t.unwrap_or(Transition::Fade)))
        .collect();
    let offsets = xfade_offsets_var(&durations, &cut_overlaps);

    // 5. Single-pass assembly: every xfade/acrossfade in one filter
    // graph, encoded once. The old pairwise chain re-encoded the whole
    // accumulated film at every cut, so the first segments went through
    // dozens of lossy generations and came out soft. The intermediate
    // encode is near-lossless (CRF 12); the final music pass is the only
    // other generation.
    let acc = work_dir.join("chain_final.mp4");
    {
        let mut args: Vec<String> = vec!["-y".into()];
        for (_, p, _, _) in &segs {
            args.extend(["-i".into(), s(p)]);
        }
        let mut graph = String::new();
        let (mut vl, mut al) = ("[0:v]".to_string(), "[0:a]".to_string());
        for i in 1..segs.len() {
            let t = transitions[i].unwrap_or(Transition::Fade);
            let ov = cut_overlaps[i - 1];
            let offset = offsets[i - 1].max(0.0);
            let (nv, na) = if i == segs.len() - 1 {
                ("[v]".to_string(), "[a]".to_string())
            } else {
                (format!("[v{i}]"), format!("[a{i}]"))
            };
            graph.push_str(&format!(
                "{vl}[{i}:v]xfade=transition={}:duration={ov:.3}:offset={offset:.3}{nv};{al}[{i}:a]acrossfade=d={ov:.3}:c1=tri:c2=tri{na};",
                t.xfade_name(),
            ));
            vl = nv;
            al = na;
        }
        graph.pop();
        args.extend([
            "-filter_complex".into(),
            graph,
            "-map".into(),
            "[v]".into(),
            "-map".into(),
            "[a]".into(),
        ]);
        args.extend(ffmpeg::x264_aac("veryfast", 12));
        args.push(s(&acc));
        ffmpeg::run_ffmpeg(&caps.ffmpeg_bin, &args, &acc)?;
    }

    // 6. Final pass: the music bed is the PRIMARY audio (looped, trimmed to
    // the film, faded in/out); the original film audio is muted, or kept at
    // 0.12 ducked under the music with --keep-ambience. Loudnorm lands on
    // the mix, never on raw clip noise.
    let total = durations.iter().sum::<f64>() - cut_overlaps.iter().sum::<f64>();
    let norm = if caps.loudnorm {
        ffmpeg::LOUDNORM
    } else {
        fallbacks.push("loudnorm absent: flat resample, no loudness pass".into());
        "anull"
    };
    // Resolve the music file: an existing --music wins; otherwise the
    // bundled theme track is materialized into the work dir.
    let music_path = match opts.music_path.as_deref().filter(|m| m.exists()) {
        Some(m) => m.to_path_buf(),
        None => {
            if let Some(m) = &opts.music_path {
                fallbacks.push(format!(
                    "music file {} missing; bundled theme instead",
                    m.display()
                ));
            }
            let bundled = work_dir.join("triptape-theme.mp3");
            std::fs::write(&bundled, DEFAULT_MUSIC_BYTES)
                .map_err(|e| crate::MediaError::Io(e.to_string()))?;
            fallbacks.push("bundled triptape-theme music bed".into());
            bundled
        }
    };
    let fade_out_start = (total - 1.5).max(0.0);
    let music_chain = format!(
        "[1:a]atrim=0:{total:.3},asetpts=PTS-STARTPTS,aresample=48000,aformat=channel_layouts=stereo,afade=t=in:st=0:d=1,afade=t=out:st={fade_out_start:.3}:d=1.5"
    );
    let audio_graph = if opts.keep_ambience {
        format!(
            "[0:a]volume=0.12,aresample=48000,aformat=channel_layouts=stereo[film];{music_chain}[m0];[m0][film]sidechaincompress=threshold=0.02:ratio=8:attack=200:release=800[duck];[film][duck]amix=inputs=2:duration=first:dropout_transition=0,{norm}[aout]"
        )
    } else {
        // Muted film audio: the music bed alone is the soundtrack.
        format!("{music_chain},{norm}[aout]")
    };
    let mut final_args: Vec<String> = vec![
        "-y".into(),
        "-i".into(),
        s(&acc),
        "-stream_loop".into(),
        "-1".into(),
        "-i".into(),
        s(&music_path),
        "-filter_complex".into(),
        audio_graph,
        "-map".into(),
        "0:v".into(),
        "-map".into(),
        "[aout]".into(),
    ];
    final_args.extend(ffmpeg::x264_aac("medium", 19));
    final_args.extend(["-r".into(), "30".into(), s(out_mp4)]);
    if let Err(e) = ffmpeg::run_ffmpeg(&caps.ffmpeg_bin, &final_args, out_mp4) {
        // Music bed is mandatory: a failed mix fails the film (the old
        // film-audio-only path is exactly the raw-clip-noise bug).
        return Err(crate::MediaError::Ffmpeg(format!("music mix failed: {e}")));
    }

    // 7. Verify + EDL + sidecar SRT.
    let probed = ffmpeg::verify_video(&caps.ffprobe_bin, out_mp4, 1920, 1080)?;
    // Segment 0 starts at 0; every later segment starts at its xfade cut.
    let starts: Vec<f64> = std::iter::once(0.0)
        .chain(offsets.iter().copied())
        .collect();
    let srt_path = out_mp4.with_extension("srt");
    let srt_count = write_captions_srt(&segs, &starts, &srt_path)?;
    let srt_opt = if srt_count > 0 {
        Some(srt_path.to_string_lossy().into_owned())
    } else {
        let _ = std::fs::remove_file(&srt_path);
        None
    };
    let edl = Edl {
        version: 1,
        trip_name: opts.trip_name.clone(),
        overlap_secs: opts.overlap_secs,
        cut_overlaps_secs: cut_overlaps
            .iter()
            .map(|o| (o * 1000.0).round() / 1000.0)
            .collect(),
        segments: segs
            .iter()
            .enumerate()
            .map(|(i, (r, _, d, kind))| EdlSegment {
                index: i,
                asset_id: r.entry.asset_id.clone(),
                kind: kind.clone(),
                segment_secs: (*d * 1000.0).round() / 1000.0,
                collage_asset_ids: collage_members
                    .get(&r.entry.asset_id)
                    .cloned()
                    .unwrap_or_default(),
                transition_in: transitions[i],
                offset_secs: if i == 0 {
                    None
                } else {
                    Some((offsets[i - 1] * 1000.0).round() / 1000.0)
                },
            })
            .collect(),
        total_secs: (total * 1000.0).round() / 1000.0,
        captions_sidecar: srt_opt.clone(),
        fallbacks: fallbacks.clone(),
    };
    let edl_path = out_mp4.with_extension("edl.json");
    std::fs::write(&edl_path, serde_json::to_string_pretty(&edl)?)
        .map_err(|e| crate::MediaError::Io(e.to_string()))?;

    Ok(VideoOutput {
        path: out_mp4.to_path_buf(),
        edl_path,
        srt_path: srt_opt.map(PathBuf::from),
        duration_secs: probed.duration_secs,
        fallbacks,
    })
}

/// Bake a 1200x200 lower-third PNG onto `seg` in place (`overlay=48:H-160`,
/// visible `between(t,0.3,segdur)`). `run_ffmpeg` writes a partial sibling
/// and renames, so a failure never leaves a torn segment.
fn apply_lower_third(
    caps: &Capabilities,
    seg: &Path,
    png: &Path,
    seg_dur: f64,
) -> Result<(), crate::MediaError> {
    let graph =
        format!("[0:v][1:v]overlay=48:H-160:format=yuv420:enable='between(t,0.3,{seg_dur:.3})'[v]");
    burn_png_overlay(caps, seg, png, &graph, 16)
}

/// Re-encode `video` in place with `png` composited by `filter_graph`
/// (inputs `[0:v]` and `[1:v]`, output `[v]`); audio is carried over.
pub(crate) fn burn_png_overlay(
    caps: &Capabilities,
    video: &Path,
    png: &Path,
    filter_graph: &str,
    crf: u32,
) -> Result<(), crate::MediaError> {
    let args: Vec<String> = vec![
        "-y".into(),
        "-i".into(),
        s(video),
        "-loop".into(),
        "1".into(),
        "-i".into(),
        s(png),
        "-filter_complex".into(),
        filter_graph.into(),
        "-map".into(),
        "[v]".into(),
        "-map".into(),
        "0:a".into(),
    ]
    .into_iter()
    .chain(ffmpeg::x264_aac("veryfast", crf))
    .chain(["-shortest".into(), s(video)])
    .collect();
    ffmpeg::run_ffmpeg(&caps.ffmpeg_bin, &args, video)
}

fn write_captions_srt(
    segs: &[(ResolvedEntry, PathBuf, f64, String)],
    starts: &[f64],
    out: &Path,
) -> Result<usize, crate::MediaError> {
    let mut n = 0;
    let mut body = String::new();
    for (i, (r, _, d, _)) in segs.iter().enumerate() {
        if let Some(c) = &r.caption {
            if c.trim().is_empty() {
                continue;
            }
            n += 1;
            let st = starts[i] + 0.3;
            // Multi-line (collage) cues get reading time per caption.
            let read_secs = 3.5 * c.trim().lines().count() as f64;
            let en = (starts[i] + (d - 0.6).min(read_secs)).max(st + 0.5);
            body.push_str(&format!(
                "{n}\n{} --> {}\n{}\n\n",
                srt_ts(st),
                srt_ts(en),
                c.trim()
            ));
        }
    }
    if n == 0 {
        return Ok(0);
    }
    std::fs::write(out, body).map_err(|e| crate::MediaError::Io(e.to_string()))?;
    Ok(n)
}

fn srt_ts(t: f64) -> String {
    let ms = (t.max(0.0) * 1000.0).round() as u64;
    format!(
        "{:02}:{:02}:{:02},{:03}",
        ms / 3_600_000,
        (ms / 60_000) % 60,
        (ms / 1000) % 60,
        ms % 1000
    )
}

/// Seconds to freeze-pad a clip to: the minimum hold, limited by what is left
/// of the short-mode budget. `None` when the clip already reaches that.
fn clip_pad_target(probed: f64, budget: f64) -> Option<f64> {
    let target = MIN_SEGMENT_SECS.min(budget);
    (probed < target).then_some(target)
}

/// Freeze-pad `seg` (video + audio) to `target` seconds on its last frame.
fn pad_segment_freeze(
    caps: &Capabilities,
    seg: &Path,
    target: f64,
) -> Result<(), crate::MediaError> {
    let args: Vec<String> = vec![
        "-y".into(),
        "-i".into(),
        s(seg),
        "-vf".into(),
        format!("tpad=stop_mode=clone:stop_duration={target:.3}"),
        "-af".into(),
        format!("apad=whole_dur={target:.3}"),
        "-t".into(),
        format!("{target:.3}"),
    ]
    .into_iter()
    .chain(ffmpeg::x264_aac("veryfast", 16))
    .chain([s(seg)])
    .collect();
    ffmpeg::run_ffmpeg(&caps.ffmpeg_bin, &args, seg)
}

/// Normalize one plan entry to a uniform segment. Returns (path, secs, kind).
fn render_entry_segment(
    caps: &Capabilities,
    r: &ResolvedEntry,
    idx: usize,
    still_secs: f64,
    max_secs: f64,
    work_dir: &Path,
    fallbacks: &mut Vec<String>,
) -> Result<(PathBuf, f64, String), crate::MediaError> {
    match &r.entry.rendition {
        Rendition::Still => {
            let out = work_dir.join(format!("seg_{idx:03}.mp4"));
            // Decode first: looping a raw HEIC fails on tmap/Dolby Vision items.
            let png = ffmpeg::decode_still_to_png(&caps.ffmpeg_bin, &r.still_path)?;
            render_hold_segment(caps, &png.path, still_secs, idx, &out)?;
            Ok((out, still_secs, "still".into()))
        }
        Rendition::Motion {
            trim_start,
            trim_end,
        }
        | Rendition::Clip {
            trim_start,
            trim_end,
        } => {
            let src = r.motion_path.as_ref().unwrap_or(&r.still_path);
            let len = (trim_end - trim_start)
                .clamp(0.5, MAX_CLIP_SECS)
                .min(max_secs.max(0.5));
            // Live motion (≤3 s class) may run short; reels/video allow that.
            let out = work_dir.join(format!("seg_{idx:03}.mp4"));
            // HDR check on the real source.
            let probe = ffmpeg::ffprobe_file(&caps.ffprobe_bin, src).ok();
            // Muted/camera-silent clips get a matching silent track so the
            // acrossfade assembly always has `[n:a]` to consume.
            let has_audio = probe.as_ref().is_none_or(|i| i.has_audio);
            // Upscaled first so the push-in samples, not smears.
            let mut vf =
                "scale=2560:1440:force_original_aspect_ratio=increase,crop=2560:1440,setsar=1"
                    .to_string();
            if let Some(info) = &probe {
                if let Some(chain) = ffmpeg::tonemap_chain(caps, &info.color_transfer) {
                    vf.push(',');
                    vf.push_str(&chain);
                    fallbacks.push(format!(
                        "{} HDR tone-mapped ({})",
                        r.entry.asset_id, info.color_transfer
                    ));
                }
            }
            // Slow glide push-in on clips (1.0 -> 1.05), matching the
            // stills' Ken Burns drift so cuts don't sit static. Normalized
            // to 30 fps first: the glide's progress counts frames.
            let frames = (len * 30.0).round().max(1.0) as u32;
            vf.push_str(&format!(
                ",fps=30,{}",
                ffmpeg::glide(caps, frames, 1920, 1080, 0)
            ));
            vf.push_str(",format=yuv420p");
            let mut args: Vec<String> = vec![
                "-y".into(),
                "-ss".into(),
                format!("{trim_start:.3}"),
                "-t".into(),
                format!("{len:.3}"),
                "-i".into(),
                s(src),
            ];
            let audio_in = if has_audio {
                "0:a"
            } else {
                fallbacks.push(format!(
                    "{} has no audio; silent track added",
                    r.entry.asset_id
                ));
                args.extend(silent_input(len));
                "1:a"
            };
            args.extend([
                "-map".into(),
                "0:v".into(),
                "-map".into(),
                audio_in.into(),
                "-vf".into(),
                vf,
                "-af".into(),
                "aresample=48000,aformat=channel_layouts=stereo".into(),
            ]);
            args.extend(ffmpeg::x264_aac("veryfast", 16));
            args.extend(["-shortest".into(), s(&out)]);
            ffmpeg::run_ffmpeg(&caps.ffmpeg_bin, &args, &out)?;
            Ok((out, len, "clip".into()))
        }
    }
}

/// Extra lavfi input producing `secs` of silent 48 kHz stereo, for sources
/// that carry no audio stream.
pub(crate) fn silent_input(secs: f64) -> Vec<String> {
    vec![
        "-f".into(),
        "lavfi".into(),
        "-t".into(),
        format!("{secs:.3}"),
        "-i".into(),
        "anullsrc=r=48000:cl=stereo".into(),
    ]
}

/// Filter graph for a still hold: the full photo fitted over its own
/// blurred fill at 16:9, then a gentle Ken Burns move (≤1.05×) on the
/// finished composite, varied by index. Zooming the raw photo instead
/// both crops the subject and distorts non-16:9 sources.
fn hold_graph(caps: &Capabilities, idx: usize, frames: u32) -> String {
    // Supersample the composite once, hold it, then glide over it.
    let composite = ffmpeg::fit_over_blur(caps, "[0:v]", 2880, 1620, "");
    let hold = ffmpeg::hold_still(frames);
    let glide = ffmpeg::glide(caps, frames, 1920, 1080, idx);
    format!("{composite},{hold},{glide},format=yuv420p[v]")
}

/// Collage segment: up to four same-chapter stills composed into one
/// 2880x1620 canvas (cover-fitted cells, dark gaps), then held with the
/// usual glide. One segment shows the day's spread at once.
fn render_collage_segment(
    caps: &Capabilities,
    members: &[&ResolvedEntry],
    idx: usize,
    still_secs: f64,
    work_dir: &Path,
) -> Result<(PathBuf, f64, String), crate::MediaError> {
    let canvas = work_dir.join(format!("collage_{idx:03}.png"));
    compose_collage(&caps.ffmpeg_bin, members, &canvas)?;
    let out = work_dir.join(format!("seg_{idx:03}.mp4"));
    let dur = still_secs * 1.5;
    render_hold_segment(caps, &canvas, dur, idx, &out)?;
    Ok((out, dur, "still".into()))
}

/// Cells (x, y, w, h) of a 2-4 photo collage: 2 side by side, 3 as one
/// tall + two stacked, 4 as a 2x2 grid.
fn collage_cells(n: usize, w: u32, h: u32) -> Vec<(u32, u32, u32, u32)> {
    let (hw, hh) = (w / 2, h / 2);
    match n {
        2 => vec![(0, 0, hw, h), (hw, 0, hw, h)],
        3 => vec![(0, 0, hw, h), (hw, 0, hw, hh), (hw, hh, hw, hh)],
        _ => vec![
            (0, 0, hw, hh),
            (hw, 0, hw, hh),
            (0, hh, hw, hh),
            (hw, hh, hw, hh),
        ],
    }
}

/// Compose 2-4 photos into a 2880x1620 collage canvas. Each cell cover-fits
/// its photo like the book grids, with dark gaps between. Photos decode in
/// parallel: each is an independent ffmpeg spawn plus a resize.
fn compose_collage(
    ffmpeg_bin: &str,
    members: &[&ResolvedEntry],
    out: &Path,
) -> Result<(), crate::MediaError> {
    const W: u32 = 2880;
    const H: u32 = 1620;
    const GAP: u32 = 18;
    let cells = collage_cells(members.len(), W, H);
    let tiles = std::thread::scope(|scope| {
        let workers: Vec<_> = members
            .iter()
            .zip(&cells)
            .map(|(m, &(_, _, cw, ch))| {
                scope.spawn(move || -> Result<image::RgbImage, crate::MediaError> {
                    let png = ffmpeg::decode_still_to_png(ffmpeg_bin, &m.still_path)?;
                    let img = image::open(&png.path)
                        .map_err(|e| crate::MediaError::Io(format!("collage open: {e}")))?;
                    Ok(crate::render_book::cover_fit(
                        &img.to_rgb8(),
                        cw - GAP,
                        ch - GAP,
                    ))
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().expect("collage worker panicked"))
            .collect::<Result<Vec<_>, _>>()
    })?;
    let mut canvas = image::RgbImage::from_pixel(W, H, image::Rgb([13, 20, 32]));
    for (tile, &(x, y, _, _)) in tiles.iter().zip(&cells) {
        image::imageops::replace(
            &mut canvas,
            tile,
            i64::from(x + GAP / 2),
            i64::from(y + GAP / 2),
        );
    }
    canvas
        .save(out)
        .map_err(|e| crate::MediaError::Io(format!("collage save: {e}")))
}

/// Still Ken Burns hold.
fn render_hold_segment(
    caps: &Capabilities,
    img: &Path,
    dur: f64,
    idx: usize,
    out: &Path,
) -> Result<(), crate::MediaError> {
    let frames = (dur * 30.0).round() as u32;
    let graph = hold_graph(caps, idx, frames);
    let args: Vec<String> = [
        vec![
            "-y".into(),
            "-loop".into(),
            "1".into(),
            "-i".into(),
            s(img),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            format!("anullsrc=r=48000:cl=stereo:d={dur}"),
            "-filter_complex".into(),
            graph,
            "-map".into(),
            "[v]".into(),
            "-map".into(),
            "1:a".into(),
            "-frames:v".into(),
            frames.to_string(),
        ],
        ffmpeg::x264_aac("veryfast", 16),
        vec!["-shortest".into(), s(out)],
    ]
    .concat();
    ffmpeg::run_ffmpeg(&caps.ffmpeg_bin, &args, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hold_graph_shows_full_photo_with_gentle_zoom() {
        let caps = Capabilities {
            gblur: true,
            perspective: true,
            ..Default::default()
        };
        for idx in 0..4 {
            let g = hold_graph(&caps, idx, 120);
            // Full photo over blurred fill, motion on the composite only,
            // never past 1.05x (window floor is 20/21 of the frame).
            assert!(g.contains("[bg][fg]overlay"), "{g}");
            assert!(
                g.find("overlay").unwrap() < g.find("perspective=").unwrap(),
                "{g}"
            );
            assert!(!g.contains("0.12"), "zoom capped at 1.05: {g}");
            assert!(!g.contains("scale=2400"), "no raw-photo zoom: {g}");
        }
    }

    #[test]
    fn hold_graph_glides_without_zoompan() {
        // The composite must be built once and held, then glided at
        // sub-pixel steps and downscaled with lanczos.
        let caps = Capabilities {
            gblur: true,
            perspective: true,
            ..Default::default()
        };
        for idx in 0..4 {
            let g = hold_graph(&caps, idx, 120);
            assert!(!g.contains("zoompan"), "no zoompan jitter: {g}");
            assert!(
                g.find("overlay").unwrap() < g.find("loop=loop=119").unwrap(),
                "composite held once: {g}"
            );
            assert!(g.contains("perspective="), "animated window: {g}");
            assert!(g.contains("lanczos"), "lanczos downscale: {g}");
            assert!(g.contains("2880"), "supersampled composite: {g}");
        }
    }

    #[test]
    fn collage_cells_tile_the_canvas() {
        for n in 2..=MAX_COLLAGE {
            let cells = collage_cells(n, 2880, 1620);
            assert_eq!(cells.len(), n);
            let area: u32 = cells.iter().map(|c| c.2 * c.3).sum();
            assert_eq!(area, 2880 * 1620, "{n} cells cover the canvas");
        }
    }

    #[test]
    fn offsets_account_for_overlap() {
        // 3 x 4 s segments, 0.6 overlap: cuts at 3.4 and 6.8; total 10.8.
        let o = xfade_offsets(&[4.0, 4.0, 4.0], 0.6);
        assert_eq!(o.len(), 2);
        assert!((o[0] - 3.4).abs() < 1e-9);
        assert!((o[1] - 6.8).abs() < 1e-9);
        assert!(xfade_offsets(&[5.0], 0.6).is_empty());
        assert!(xfade_offsets(&[], 0.6).is_empty());
    }

    #[test]
    fn transition_discipline() {
        use SegKind::*;
        // 8 segments, chapter breaks into #3 and #5.
        let breaks = vec![false, false, false, true, false, true, false, false];
        let kinds = vec![Title, Still, Still, Still, Clip, Still, Still, Still];
        let t = assign_transitions(8, &breaks, &kinds);
        assert_eq!(t[0], None);
        assert_eq!(t[1], Some(Transition::CircleOpen));
        assert_eq!(t[7], Some(Transition::CircleClose));
        // First chapter break takes the fadeblack slot.
        assert_eq!(t[3], Some(Transition::FadeBlack));
        // Kind change (still -> clip) dissolves.
        assert_eq!(t[4], Some(Transition::Dissolve));
        // Blur-dissolve signature capped at 3 and mapped to dissolve.
        let plain = assign_transitions(30, &[false; 30], &[Still; 30]);
        let sig = plain
            .iter()
            .filter(|t| **t == Some(Transition::BlurDissolve))
            .count();
        assert!(sig <= 3, "signature capped, got {sig}");
        assert_eq!(Transition::BlurDissolve.xfade_name(), "dissolve");
    }

    #[test]
    fn transition_variety_no_consecutive_equal() {
        use SegKind::*;
        // Micro-chapter soup: every cut a chapter break (the 104-chapter bug).
        let n = 40;
        let breaks = vec![true; n];
        let kinds = vec![Still; n];
        let t = assign_transitions(n, &breaks, &kinds);
        let names: Vec<&str> = t.iter().skip(1).map(|t| t.unwrap().xfade_name()).collect();
        for w in names.windows(2) {
            assert_ne!(w[0], w[1], "consecutive equal xfades");
        }
        // fadeblack stays rare: at most 1 in 3 cuts.
        let fb = names.iter().filter(|n| ***n == *"fadeblack").count();
        assert!(fb * 3 <= n, "fadeblack capped, got {fb}/{n}");
        // Mixed story: rotation still varies.
        let kinds2: Vec<SegKind> = (0..24)
            .map(|i| if i % 3 == 2 { Clip } else { Still })
            .collect();
        let t2 = assign_transitions(24, &[false; 24], &kinds2);
        let names2: Vec<&str> = t2.iter().skip(1).map(|t| t.unwrap().xfade_name()).collect();
        for w in names2.windows(2) {
            assert_ne!(w[0], w[1], "consecutive equal xfades (mixed)");
        }
        let distinct: std::collections::HashSet<&str> = names2.iter().copied().collect();
        assert!(distinct.len() >= 4, "rotation varies, got {distinct:?}");
    }

    #[test]
    fn offsets_varied_overlaps() {
        // 3 x 4 s segments, overlaps [0.8, 0.5]: cuts at 3.2 and 6.7.
        let o = xfade_offsets_var(&[4.0, 4.0, 4.0], &[0.8, 0.5]);
        assert_eq!(o.len(), 2);
        assert!((o[0] - 3.2).abs() < 1e-9, "got {}", o[0]);
        assert!((o[1] - 6.7).abs() < 1e-9, "got {}", o[1]);
        // Uniform helper agrees with the legacy math.
        let u = xfade_offsets(&[4.0, 4.0, 4.0], 0.6);
        assert!((u[0] - 3.4).abs() < 1e-9);
        assert!((u[1] - 6.8).abs() < 1e-9);
        // Per-transition overlaps per spec.
        assert_eq!(transition_overlap(&Transition::FadeBlack), 0.8);
        assert_eq!(transition_overlap(&Transition::CircleOpen), 0.7);
        assert_eq!(transition_overlap(&Transition::CircleClose), 0.7);
        assert_eq!(transition_overlap(&Transition::Fade), 0.5);
        assert_eq!(transition_overlap(&Transition::BlurDissolve), 0.5);
    }

    #[test]
    fn legacy_chapter_falls_back_to_title() {
        use crate::titlecard::{chapter_book_title, chapter_lines};
        let legacy: triptape_core::types::Chapter =
            serde_json::from_str(r#"{"title":"Day 2 · Coast","asset_ids":["a1"]}"#).unwrap();
        assert_eq!(
            chapter_lines(&legacy),
            Some(("Day 2 · Coast".to_string(), None))
        );
        assert_eq!(chapter_book_title(&legacy), "Day 2 · Coast");
        let mut current = legacy.clone();
        current.day_index = Some(2);
        current.place = Some("Kyoto, Japan".into());
        assert_eq!(
            chapter_lines(&current),
            Some(("Day 2".to_string(), Some("Kyoto, Japan".to_string())))
        );
        current.date_label = Some("Wed, Oct 8".into());
        assert_eq!(
            chapter_book_title(&current),
            "Day 2 · Wed, Oct 8 \u{2014} Kyoto, Japan"
        );
        let mut blank = legacy;
        blank.title = "  ".into();
        assert_eq!(chapter_lines(&blank), None);
    }

    #[test]
    fn chapter_overlay_and_date_range() {
        use crate::titlecard::{chapter_overlay, format_date_range};
        // 2026-10-06 12:00 UTC.
        let day = 1791288000i64;
        // Oct 4 22:50 UTC is already Oct 5 in Japan (+09:00).
        assert_eq!(
            format_date_range(&[(1791154200, Some(9 * 3600))]).as_deref(),
            Some("Oct 5, 2026")
        );
        assert_eq!(
            format_date_range(&[(day, None), (day + 3 * 86400, None)]).as_deref(),
            Some("Oct 6, 2026 – Oct 9, 2026")
        );
        assert_eq!(
            format_date_range(&[(day, None)]).as_deref(),
            Some("Oct 6, 2026")
        );
        assert_eq!(format_date_range(&[]), None);
        // Date-line crossing: the earlier instant (Oct 6 12:00 UTC at
        // +13:00 → Oct 7) has a later local date than the next one
        // (Oct 6 13:00 UTC at -10:00 → Oct 6 03:00).
        assert_eq!(
            format_date_range(&[(day, Some(13 * 3600)), (day + 3600, Some(-10 * 3600))]).as_deref(),
            Some("Oct 6, 2026 – Oct 7, 2026")
        );
        // Chapter overlay: day once, place underneath.
        assert_eq!(
            chapter_overlay(Some(3), Some("Wed, Oct 8"), Some("Kyoto, Japan")),
            Some((
                "Day 3 · Wed, Oct 8".to_string(),
                Some("Kyoto, Japan".to_string())
            ))
        );
        assert_eq!(
            chapter_overlay(Some(3), Some("Wed, Oct 8"), None),
            Some(("Day 3 · Wed, Oct 8".to_string(), None))
        );
        assert_eq!(
            chapter_overlay(None, None, Some("Kyoto, Japan")),
            Some(("Kyoto, Japan".to_string(), None))
        );
        assert_eq!(chapter_overlay(None, None, None), None);
    }

    /// Full hero pipeline on synthetic stills: Typst title + lower-thirds,
    /// per-cut transitions, and the bundled music bed (no --music passed).
    /// Writes to /tmp/hero-smoke so RMS can be measured after the run.
    /// Skips quietly when ffmpeg/typst are unavailable.
    #[test]
    fn hero_smoke_bundled_music_and_overlays() {
        use triptape_core::types::Plan;

        let caps = crate::ffmpeg::probe_capabilities("ffmpeg", "ffprobe");
        if !caps.libx264 || !caps.xfade || !caps.loudnorm || !caps.perspective {
            eprintln!("ffmpeg caps insufficient; skipping hero smoke");
            return;
        }
        if !crate::titlecard::typst_available("typst") {
            eprintln!("typst missing; skipping hero smoke");
            return;
        }
        let dir = PathBuf::from("/tmp/hero-smoke");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("work")).unwrap();
        // Three solid stills (red/green/blue) as PNG sources.
        let mut entries = vec![];
        for (i, px) in [[255u8, 0, 0], [0, 255, 0], [0, 0, 255]].iter().enumerate() {
            let img: image::RgbImage = image::ImageBuffer::from_pixel(640, 480, image::Rgb(*px));
            let p = dir.join(format!("still_{i}.png"));
            img.save(&p).unwrap();
            entries.push(ResolvedEntry {
                entry: PlanEntry {
                    asset_id: format!("a{i}"),
                    content_hash: None,
                    size_bytes: 0,
                    mtime: 0,
                    rendition: Rendition::Still,
                },
                still_path: p,
                motion_path: None,
                caption: if i == 1 {
                    Some("Golden Gate at sunset".into())
                } else {
                    None
                },
                // Oct 6/7/8 2026 12:00 UTC.
                taken_at: Some(1791288000 + i as i64 * 86400),
                tz_offset_secs: None,
            });
        }
        let mut captions = HashMap::new();
        captions.insert("a1".to_string(), "Golden Gate at sunset".to_string());
        let plan = Plan {
            plan_version: 1,
            schema_version: 1,
            hero: vec![],
            reel_selects: vec![],
            chapters: vec![
                triptape_core::types::Chapter {
                    title: "Day 1 · Arrival".into(),
                    asset_ids: vec!["a0".into(), "a1".into()],
                    ..Default::default()
                },
                triptape_core::types::Chapter {
                    title: "Day 2 · Coast".into(),
                    asset_ids: vec!["a2".into()],
                    ..Default::default()
                },
            ],
            captions,
            map_polyline: vec![],
            music: triptape_core::types::MusicPick {
                track: "bundled".into(),
                rationale: "default".into(),
            },
        };
        let out = dir.join("hero.mp4");
        let v = render_hero_video(
            &caps,
            &plan,
            &entries,
            None,
            &dir.join("work"),
            &out,
            &VideoOptions {
                trip_name: "Smoke Trip".into(),
                still_secs: 1.5,
                title_secs: 1.0,
                music_path: None,
                keep_ambience: false,
                typst_bin: "typst".into(),
                ..Default::default()
            },
        )
        .expect("hero smoke render");
        assert!(out.is_file());
        assert!(
            v.fallbacks
                .iter()
                .any(|f| f.contains("bundled triptape-theme")),
            "music bed fallback recorded: {:?}",
            v.fallbacks
        );
        // EDL: per-cut overlaps recorded, transitions vary, offsets match.
        let edl_json = std::fs::read_to_string(&v.edl_path).unwrap();
        let edl: serde_json::Value = serde_json::from_str(&edl_json).unwrap();
        let cuts = edl["cut_overlaps_secs"].as_array().unwrap();
        assert_eq!(
            cuts.len(),
            2,
            "title + same-chapter collage + solo still = 2 cuts"
        );
        let names: Vec<String> = edl["segments"]
            .as_array()
            .unwrap()
            .iter()
            .skip(1)
            .map(|s| s["transition_in"].as_str().unwrap().to_string())
            .collect();
        for w in names.windows(2) {
            assert_ne!(w[0], w[1], "no consecutive equal transitions");
        }
        assert_eq!(names[0], "circle_open");
        assert_eq!(names[names.len() - 1], "circle_close");
        // The caption on the collage's second photo survives grouping, and
        // the EDL names both photos on screen.
        let srt = std::fs::read_to_string(v.srt_path.as_ref().expect("sidecar")).unwrap();
        assert!(srt.contains("Golden Gate at sunset"));
        assert_eq!(
            edl["segments"][1]["collage_asset_ids"],
            serde_json::json!(["a0", "a1"])
        );
    }

    #[test]
    fn collage_caption_keeps_every_member() {
        let member = |caption: Option<&str>| ResolvedEntry {
            caption: caption.map(String::from),
            ..card_entry("a", Path::new("a.png"))
        };
        let (a, b, c) = (member(Some(" One ")), member(None), member(Some("Two")));
        assert_eq!(collage_caption(&[&a, &b, &c]).as_deref(), Some("One\nTwo"));
        let blank = member(Some("  "));
        assert_eq!(collage_caption(&[&b, &blank]), None);
    }

    #[test]
    fn closing_secs_counts_only_cards_that_render() {
        assert_eq!(closing_secs(true, true, 3.0), 9.0);
        assert_eq!(closing_secs(true, false, 3.0), 4.5);
        assert_eq!(closing_secs(false, true, 3.0), CREDITS_SECS);
        assert_eq!(closing_secs(false, false, 3.0), 0.0);
    }

    #[test]
    fn long_clips_leave_time_for_later_segments() {
        const CLIP_SECS: f64 = 8.0;
        let segments = 6;
        let mut budget = 24.0;
        for done in 0..segments {
            assert!(budget >= 0.5, "segment {done} would be dropped");
            budget -= CLIP_SECS.min(fair_share(budget, segments - done));
        }
        assert!(budget.abs() < 1e-9, "the film uses its budget: {budget}");
        // Full-length films have no budget to share.
        assert!(fair_share(f64::INFINITY, 3).is_infinite());
    }

    #[test]
    fn short_film_picks_reach_every_chapter_in_order() {
        let entries: Vec<ResolvedEntry> = (0..39)
            .map(|i| ResolvedEntry {
                entry: PlanEntry {
                    asset_id: format!("a{i}"),
                    content_hash: None,
                    size_bytes: 0,
                    mtime: 0,
                    rendition: Rendition::Still,
                },
                still_path: PathBuf::new(),
                motion_path: None,
                caption: None,
                taken_at: None,
                tz_offset_secs: None,
            })
            .collect();
        let refs: Vec<&ResolvedEntry> = entries.iter().collect();
        let index = |id: &str| id[1..].parse::<usize>().unwrap();
        // Days of 12, 13 and 14 photos.
        let chapter = |id: &str| match index(id) {
            0..=11 => 0,
            12..=24 => 1,
            _ => 2,
        };

        let picks: Vec<usize> = spread_across_chapters(&refs, 7, chapter)
            .iter()
            .map(|r| index(&r.entry.asset_id))
            .collect();
        assert_eq!(picks.len(), 7);
        assert!(
            picks.windows(2).all(|w| w[0] < w[1]),
            "keeps order: {picks:?}"
        );
        let per_day = |day| {
            picks
                .iter()
                .filter(|&&p| chapter(&format!("a{p}")) == day)
                .count()
        };
        assert_eq!((per_day(0), per_day(1), per_day(2)), (3, 2, 2));

        // Everything fits: nothing is dropped.
        assert_eq!(spread_across_chapters(&refs, 39, chapter).len(), 39);
        // A chapter smaller than its share gives its slots to the others.
        let two = |id: &str| usize::from(index(id) > 0);
        assert_eq!(spread_across_chapters(&refs[..4], 3, two).len(), 3);
    }

    /// Regression: GPS + named places in short mode. The closing cards used
    /// to be appended after the budget was spent, overrunning 30 s.
    #[test]
    fn short_film_with_closing_cards_stays_within_limit() {
        use triptape_core::types::Plan;

        let caps = crate::ffmpeg::probe_capabilities("ffmpeg", "ffprobe");
        if !caps.libx264 || !caps.xfade || !caps.loudnorm {
            eprintln!("ffmpeg caps insufficient; skipping short-film limit smoke");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let img: image::RgbImage = image::ImageBuffer::from_pixel(64, 36, image::Rgb([9, 90, 160]));
        let still = dir.join("still.png");
        img.save(&still).unwrap();
        let ids: Vec<String> = (0..10).map(|i| format!("a{i}")).collect();
        let entries: Vec<ResolvedEntry> = ids
            .iter()
            .map(|id| ResolvedEntry {
                entry: PlanEntry {
                    asset_id: id.clone(),
                    content_hash: None,
                    size_bytes: 0,
                    mtime: 0,
                    rendition: Rendition::Still,
                },
                still_path: still.clone(),
                motion_path: None,
                caption: None,
                taken_at: Some(1791288000),
                tz_offset_secs: None,
            })
            .collect();
        let plan = Plan {
            plan_version: 1,
            schema_version: 1,
            hero: vec![],
            reel_selects: vec![],
            chapters: vec![triptape_core::types::Chapter {
                title: "Day 1".into(),
                asset_ids: ids,
                place: Some("Kyoto, Japan".into()),
                ..Default::default()
            }],
            captions: HashMap::new(),
            map_polyline: vec![(35.0, 135.0), (35.1, 135.1)],
            music: triptape_core::types::MusicPick {
                track: "bundled".into(),
                rationale: "default".into(),
            },
        };
        let out = dir.join("hero.mp4");
        let v = render_hero_video(
            &caps,
            &plan,
            &entries,
            Some(&still),
            &dir.join("work"),
            &out,
            &VideoOptions {
                short_mode: true,
                ..Default::default()
            },
        )
        .expect("short film render");
        assert!(
            v.duration_secs <= SHORT_FILM_SECS,
            "short film ran {:.2}s",
            v.duration_secs
        );
        let edl: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&v.edl_path).unwrap()).unwrap();
        let ids: Vec<&str> = edl["segments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["asset_id"].as_str().unwrap())
            .collect();
        assert!(
            ids.contains(&"closing") && ids.contains(&"credits"),
            "{ids:?}"
        );
    }

    /// Regression: a clip whose planned trim (4 s) exceeds its real length
    /// (0.5 s) must not truncate the film. Timeline math uses probed
    /// segment durations; with planned durations the second xfade offset
    /// lands past the accumulated video and the chain collapses.
    #[test]
    fn hero_smoke_short_clip_does_not_truncate() {
        use triptape_core::types::Plan;

        let caps = crate::ffmpeg::probe_capabilities("ffmpeg", "ffprobe");
        if !caps.libx264 || !caps.xfade || !caps.loudnorm || !caps.perspective {
            eprintln!("ffmpeg caps insufficient; skipping short-clip smoke");
            return;
        }
        if !crate::titlecard::typst_available("typst") {
            eprintln!("typst missing; skipping short-clip smoke");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        std::fs::create_dir_all(dir.join("work")).unwrap();
        // 0.5 s clip source, planned as a 4 s trim.
        let clip = dir.join("short.mp4");
        let st = std::process::Command::new(&caps.ffmpeg_bin)
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=0.5:size=640x480:rate=30",
                "-pix_fmt",
                "yuv420p",
                &clip.to_string_lossy(),
            ])
            .output()
            .unwrap();
        assert!(
            st.status.success(),
            "make short clip: {}",
            String::from_utf8_lossy(&st.stderr)
        );
        let img: image::RgbImage =
            image::ImageBuffer::from_pixel(640, 480, image::Rgb([0, 200, 0]));
        let still = dir.join("still.png");
        img.save(&still).unwrap();
        let entries = vec![
            ResolvedEntry {
                entry: PlanEntry {
                    asset_id: "clip0".into(),
                    content_hash: None,
                    size_bytes: 0,
                    mtime: 0,
                    rendition: Rendition::Clip {
                        trim_start: 0.0,
                        trim_end: 4.0,
                    },
                },
                still_path: clip.clone(),
                motion_path: Some(clip.clone()),
                caption: None,
                taken_at: Some(1791288000),
                tz_offset_secs: None,
            },
            ResolvedEntry {
                entry: PlanEntry {
                    asset_id: "still1".into(),
                    content_hash: None,
                    size_bytes: 0,
                    mtime: 0,
                    rendition: Rendition::Still,
                },
                still_path: still,
                motion_path: None,
                caption: None,
                taken_at: Some(1791374400),
                tz_offset_secs: None,
            },
        ];
        let plan = Plan {
            plan_version: 1,
            schema_version: 1,
            hero: vec![],
            reel_selects: vec![],
            chapters: vec![triptape_core::types::Chapter {
                title: "Day 1".into(),
                asset_ids: vec!["clip0".into(), "still1".into()],
                ..Default::default()
            }],
            captions: HashMap::new(),
            map_polyline: vec![],
            music: triptape_core::types::MusicPick {
                track: "bundled".into(),
                rationale: "default".into(),
            },
        };
        let out = dir.join("hero.mp4");
        render_hero_video(
            &caps,
            &plan,
            &entries,
            None,
            &dir.join("work"),
            &out,
            &VideoOptions {
                trip_name: "Short Clip Trip".into(),
                still_secs: 1.5,
                title_secs: 1.0,
                music_path: None,
                keep_ambience: false,
                typst_bin: "typst".into(),
                ..Default::default()
            },
        )
        .expect("short-clip hero render");
        // Video stream duration (not format, which audio can inflate):
        // title 1.0 + clip freeze-padded to 2.5 + still 1.5 - overlaps
        // (~1.2) ~= 3.8 s. Without the pad it would be ~1.8 s.
        let probe = std::process::Command::new(&caps.ffprobe_bin)
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=duration",
                "-of",
                "csv=p=0",
                &out.to_string_lossy(),
            ])
            .output()
            .unwrap();
        let vdur: f64 = String::from_utf8_lossy(&probe.stdout)
            .trim()
            .parse()
            .unwrap_or(0.0);
        assert!(
            vdur > 3.2,
            "video stream must span all segments with the clip padded, got {vdur:.2}s"
        );
    }

    #[test]
    fn clip_pad_target_respects_budget() {
        assert_eq!(clip_pad_target(1.0, f64::INFINITY), Some(MIN_SEGMENT_SECS));
        assert_eq!(clip_pad_target(1.0, 10.0), Some(MIN_SEGMENT_SECS));
        assert_eq!(clip_pad_target(0.8, 1.5), Some(1.5));
        // Already capped to the remaining budget: nothing to pad.
        assert_eq!(clip_pad_target(0.6, 0.6), None);
        assert_eq!(clip_pad_target(3.0, 10.0), None);
    }

    #[test]
    fn srt_timestamps() {
        assert_eq!(srt_ts(0.0), "00:00:00,000");
        assert_eq!(srt_ts(61.5), "00:01:01,500");
        assert_eq!(srt_ts(3723.25), "01:02:03,250");
    }
}
