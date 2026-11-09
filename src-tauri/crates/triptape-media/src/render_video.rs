//! Keepsake 1 — Trip Video (hero film).
//!
//! Pipeline: normalize every plan entry to a uniform 4K30 (or 1080p30, see
//! [`Resolution`]) / s48kHz-stereo
//! segment (Ken Burns stills, scored clip trims, title card, map push-in),
//! then assemble with a stepwise pairwise crossfade chain (never 5+ inputs
//! in one graph; each offset accounts for cumulative overlap), a final
//! loudnorm single pass, libx264 CRF19 faststart, an EDL JSON, and a
//! sidecar SRT (burned in too when ffmpeg has libass).

use crate::ffmpeg::{self, path_arg as s, Capabilities};
use crate::resolution::Resolution;
use crate::segment_cache::SegmentCache;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use triptape_core::plan::{
    collage_hold_secs, has_caption, is_panorama, pano_coverage, pano_motion, MAX_CLIP_SECS,
    MAX_COLLAGE, MIN_SEGMENT_SECS,
};
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

/// The single-encode filter graph for the hero film: segment inputs
/// `0..n` chained through xfade (and acrossfade when `keep_ambience`),
/// then the optional caption burn and the audio mix ending in `[aout]`.
/// `bed_graph` must emit `[mbed]`. Returns the graph and the `-map`
/// target for the video stream.
fn film_graph(
    transitions: &[Option<Transition>],
    cut_overlaps: &[f64],
    offsets: &[f64],
    keep_ambience: bool,
    burn_srt: Option<&str>,
    bed_graph: &str,
    norm: &str,
) -> (String, String) {
    let n = transitions.len();
    let mut graph = String::new();
    let (mut vl, mut al) = ("[0:v]".to_string(), "[0:a]".to_string());
    for i in 1..n {
        let t = transitions[i].unwrap_or(Transition::Fade);
        let ov = cut_overlaps[i - 1];
        let offset = offsets[i - 1].max(0.0);
        let tag = |p: &str| {
            if i == n - 1 {
                format!("[{p}]")
            } else {
                format!("[{p}{i}]")
            }
        };
        let nv = tag("v");
        graph.push_str(&format!(
            "{vl}[{i}:v]xfade=transition={}:duration={ov:.3}:offset={offset:.3}{nv};",
            t.xfade_name(),
        ));
        vl = nv;
        if keep_ambience {
            let na = tag("a");
            graph.push_str(&format!(
                "{al}[{i}:a]acrossfade=d={ov:.3}:c1=tri:c2=tri{na};"
            ));
            al = na;
        }
    }
    // `vl`/`al` now name the finished film streams ([v]/[a], or the lone
    // input's [0:v]/[0:a] when there is no cut).
    let video_map = match burn_srt {
        Some(sub) => {
            graph.push_str(&format!("{vl}{sub}[vsub];"));
            "[vsub]".to_string()
        }
        // A filter-output label maps as-is; a raw input stream is `0:v`.
        None if n > 1 => vl,
        None => "0:v".to_string(),
    };
    if keep_ambience {
        graph.push_str(&format!(
            "{al}volume=0.12,aresample=48000,aformat=channel_layouts=stereo[film];{bed_graph};[mbed][film]sidechaincompress=threshold=0.02:ratio=8:attack=200:release=800[duck];[film][duck]amix=inputs=2:duration=first:dropout_transition=0,{norm}[aout]"
        ));
    } else {
        // Muted film audio: the music bed alone is the soundtrack.
        graph.push_str(&format!("{bed_graph};[mbed]{norm}[aout]"));
    }
    (graph, video_map)
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
    /// Display width / height (EXIF rotation applied) when known; lets
    /// panoramas be kept out of collages before anything is decoded.
    pub aspect: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct VideoOptions {
    pub trip_name: String,
    pub short_mode: bool,
    pub still_secs: f64,
    pub overlap_secs: f64,
    pub title_secs: f64,
    pub map_secs: f64,
    /// Music playlist in order; crossfaded, cycled only if shorter than the
    /// film. Empty (or all unusable) falls back to the bundled theme.
    pub music_paths: Vec<PathBuf>,
    /// Keep the original film audio at 0.12 under a ducked music bed.
    /// Default (false) mutes it: the music is the primary audio.
    pub keep_ambience: bool,
    /// Typst binary for title/lower-third text PNGs.
    pub typst_bin: String,
    /// Output frame size (default 4K).
    pub resolution: Resolution,
    /// Segments rendered at once; `None` picks [`default_jobs`]. Short mode
    /// always renders sequentially (its budget couples the segments).
    pub jobs: Option<usize>,
    /// Chapter stops the closing map travels between (indices into the
    /// plan's `map_polyline`). Fewer than two keep the static closing card.
    pub route_stops: Vec<triptape_core::route_stops::RouteStop>,
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
            music_paths: Vec::new(),
            keep_ambience: false,
            typst_bin: "typst".into(),
            resolution: Resolution::default(),
            jobs: None,
            route_stops: vec![],
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
    pub music: crate::music::EdlMusic,
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

/// Seconds the closing map runs: the animated recap's budgeted length when
/// there are stops to travel between, else the static card's hold.
fn closing_map_secs(route_stops: usize, map_secs: f64) -> f64 {
    if route_stops >= 2 {
        triptape_core::route_stops::closing_map_secs(route_stops)
    } else {
        map_secs + CLOSING_MAP_EXTRA_SECS
    }
}

/// Seconds the closing cards add, reserved up front in short mode.
fn closing_secs(has_route: bool, route_stops: usize, has_places: bool, map_secs: f64) -> f64 {
    let map = if has_route {
        closing_map_secs(route_stops, map_secs)
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

/// Sidecar text for a collage: each member's caption on its own line,
/// identical lines once.
fn collage_caption(members: &[&ResolvedEntry]) -> Option<String> {
    let mut lines: Vec<&str> = vec![];
    for c in members
        .iter()
        .filter_map(|m| m.caption.as_deref())
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        if !lines.contains(&c) {
            lines.push(c);
        }
    }
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
            window: 0,
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
        aspect: None,
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
        match crate::titlecard::render_text_png(&opts.typst_bin, doc, png, opts.resolution.ppi()) {
            Ok(()) => return Ok(true),
            Err(e) => fallbacks.push(format!("{label} text failed ({e}); solid card")),
        }
    }
    let (w, h) = opts.resolution.dims();
    crate::titlecard::save_title_card(&opts.trip_name, w, h, png)?;
    Ok(false)
}

/// Render the hero film. `work_dir` holds intermediates (deleted by the
/// caller when the export ends); outputs land directly in their final paths.
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
    render_hold_segment(
        caps,
        &title_png,
        opts.title_secs,
        0,
        opts.resolution,
        &title_mp4,
    )?;
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
            match crate::map::render_map_push_in(
                caps,
                card,
                &map_mp4,
                opts.map_secs,
                0,
                opts.resolution,
            ) {
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
            - closing_secs(
                has_route,
                triptape_core::route_stops::animated_stop_count(&opts.route_stops),
                !places.is_empty(),
                opts.map_secs,
            )
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
        match groups.last_mut() {
            Some(g)
                if !opts.short_mode
                    && g.len() < MAX_COLLAGE
                    && collage_eligible(r)
                    && collage_eligible(g[0])
                    && chapter_of(&g[0].entry.asset_id) == chapter_of(&r.entry.asset_id) =>
            {
                g.push(r);
            }
            _ => groups.push(vec![r]),
        }
    }
    // Segment renders are independent unless a short-mode budget couples
    // them (each one's cap and freeze-pad depend on what earlier ones left),
    // so full mode fans out over `jobs` workers and short mode stays
    // sequential. Both paths go through `render_group`, and results are
    // folded back in group order, so the segment list, timeline and
    // fallbacks do not depend on the worker count.
    let variants = glide_variants(&groups);
    let progress = caps.progress.clone();
    let jobs = if opts.short_mode {
        1
    } else {
        opts.jobs
            .unwrap_or_else(|| default_jobs(opts.resolution))
            .max(1)
    };
    let job_caps = Capabilities {
        threads: thread_share(jobs),
        ..caps.clone()
    };
    let mut absorb = |segs: &mut Vec<(ResolvedEntry, PathBuf, f64, String)>,
                      fallbacks: &mut Vec<String>,
                      out: GroupOutcome| {
        fallbacks.extend(out.fallbacks);
        if let Some(seg) = out.seg {
            if !out.collage_members.is_empty() {
                collage_members.insert(seg.0.entry.asset_id.clone(), out.collage_members);
            }
            segs.push(seg);
        }
    };
    if opts.short_mode {
        progress.counted("video", "segment", 0, groups.len());
        for (idx, group) in groups.iter().enumerate() {
            progress.check_cancelled()?;
            let (present, missing) = split_present(group);
            fallbacks.extend(missing);
            let Some(&r) = present.first() else { continue };
            // Below the 0.5 s clip floor nothing more fits.
            if budget < 0.5 {
                fallbacks.push(format!(
                    "short variant: {} and later dropped to fit ~30 s",
                    r.entry.asset_id
                ));
                break;
            }
            let limits = SegmentLimits {
                cap: fair_share(budget, groups.len() - idx),
                budget,
            };
            let out = render_group(
                &job_caps,
                &present,
                idx,
                variants[idx],
                still_secs,
                limits,
                opts,
                work_dir,
            )?;
            if let Some(seg) = &out.seg {
                budget -= seg.2;
            }
            absorb(&mut segs, &mut fallbacks, out);
            progress.counted("video", "segment", idx + 1, groups.len());
        }
    } else {
        let plans: Vec<(Vec<&ResolvedEntry>, Vec<String>)> =
            groups.iter().map(|g| split_present(g)).collect();
        let work: Vec<usize> = (0..plans.len())
            .filter(|&i| !plans[i].0.is_empty())
            .collect();
        progress.counted("video", "segment", 0, work.len());
        let mut results: Vec<Option<Result<GroupOutcome, crate::MediaError>>> =
            (0..plans.len()).map(|_| None).collect();
        triptape_core::parallel::for_each_parallel(
            &work,
            jobs,
            |&idx| {
                progress.check_cancelled()?;
                render_group(
                    &job_caps,
                    &plans[idx].0,
                    idx,
                    variants[idx],
                    still_secs,
                    SegmentLimits::NONE,
                    opts,
                    work_dir,
                )
            },
            |wi, res, done, total| {
                results[work[wi]] = Some(res);
                progress.counted("video", "segment", done, total);
            },
        );
        progress.check_cancelled()?;
        for (idx, (_, missing)) in plans.into_iter().enumerate() {
            fallbacks.extend(missing);
            if let Some(res) = results[idx].take() {
                absorb(&mut segs, &mut fallbacks, res?);
            }
        }
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
            // Animated travel recap; the static pull-out card is the fallback.
            let animated = crate::routeanim::render_route_recap(
                caps,
                &plan.map_polyline,
                &opts.route_stops,
                opts.resolution,
                &opts.typst_bin,
                work_dir,
                &close_mp4,
            );
            let animated = match animated {
                Ok(rep) => {
                    fallbacks.extend(rep.notes);
                    Some(rep.secs)
                }
                Err(crate::MediaError::Cancelled) => return Err(crate::MediaError::Cancelled),
                Err(e) => {
                    if triptape_core::route_stops::animated_stop_count(&opts.route_stops) >= 2 {
                        fallbacks.push(format!("route animation failed, static closing map: {e}"));
                    }
                    None
                }
            };
            let dur = animated.unwrap_or(opts.map_secs + CLOSING_MAP_EXTRA_SECS);
            let rendered = match animated {
                Some(_) => Ok(()),
                None => {
                    crate::map::render_map_push_in(caps, card, &close_mp4, dur, 1, opts.resolution)
                }
            };
            match rendered {
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
        render_hold_segment(
            caps,
            &png,
            CREDITS_SECS,
            segs.len(),
            opts.resolution,
            &places_mp4,
        )?;
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
        // Each overlay rewrites its own segment file, so they run alongside
        // one another; notes are folded back in segment order.
        let mut todo: Vec<(usize, String)> = vec![];
        for (i, (r, _, _, _)) in segs.iter().enumerate() {
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
            todo.push((
                i,
                crate::titlecard::lower_third_doc(&top, bottom.as_deref()),
            ));
        }
        progress.counted("video", "overlay", 0, todo.len());
        let mut notes: Vec<Option<String>> = vec![None; todo.len()];
        triptape_core::parallel::for_each_parallel(
            &todo,
            jobs,
            |(i, doc)| -> Result<Option<String>, crate::MediaError> {
                progress.check_cancelled()?;
                let (r, seg_mp4, dur, _) = &segs[*i];
                // A cached overlay replaces the segment outright and skips
                // the Typst render and the re-encode.
                let ov_key = job_caps.segment_cache.as_deref().and_then(|c| {
                    let base = c.key_of(seg_mp4)?;
                    Some((c, overlay_key(c, &base, doc, *dur, opts)))
                });
                if let Some((c, key)) = &ov_key {
                    if c.fetch_overlay(key, seg_mp4) {
                        return Ok(None);
                    }
                }
                let png = work_dir.join(format!("ov_{i:03}.png"));
                let result = crate::titlecard::render_text_png(
                    &opts.typst_bin,
                    doc,
                    &png,
                    opts.resolution.ppi(),
                )
                .and_then(|()| apply_lower_third(&job_caps, seg_mp4, &png, *dur, opts.resolution));
                if let (Ok(()), Some((c, key))) = (&result, &ov_key) {
                    c.store_overlay(key, seg_mp4, *dur);
                }
                Ok(result
                    .err()
                    .filter(|e| !matches!(e, crate::MediaError::Cancelled))
                    .map(|e| format!("overlay skipped for {}: {e}", r.entry.asset_id)))
            },
            |ti, res, done, total| {
                if let Ok(note) = res {
                    notes[ti] = note;
                }
                progress.counted("video", "overlay", done, total);
            },
        );
        progress.check_cancelled()?;
        fallbacks.extend(notes.into_iter().flatten());
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

    // 5. One encode: every xfade (and, with --keep-ambience, acrossfade) in
    // a single filter graph feeds the caption burn and the music mix, and
    // x264 runs exactly once at final quality. The old pairwise chain
    // re-encoded the whole accumulated film at every cut (soft output); the
    // later two-pass layout (near-lossless intermediate, then final encode)
    // paid for a full extra decode+encode of the film. The music bed is the
    // PRIMARY audio (looped, trimmed, faded in/out); film audio is muted, or
    // kept at 0.12 ducked under the music with --keep-ambience. Loudnorm
    // lands on the mix, never on raw clip noise.
    let total = durations.iter().sum::<f64>() - cut_overlaps.iter().sum::<f64>();
    let norm = if caps.loudnorm {
        ffmpeg::LOUDNORM
    } else {
        fallbacks.push("loudnorm absent: flat resample, no loudness pass".into());
        "anull"
    };
    // Resolve the playlist: existing --music files win; with none usable
    // the bundled theme track is materialized into the work dir.
    let playlist = crate::music::load_playlist(
        &caps.ffmpeg_bin,
        &caps.ffprobe_bin,
        &opts.music_paths,
        work_dir,
        &mut fallbacks,
    )?;
    let bed_plan = crate::music::plan_bed(
        &playlist.iter().map(|t| t.duration_secs).collect::<Vec<_>>(),
        total,
    );
    // Captions: the sidecar SRT is always written; with libass the encode
    // also burns it into the picture. Segment 0 starts at 0; every later
    // segment starts at its xfade cut.
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
    let burn_srt =
        (srt_count > 0 && caps.can_burn_captions()).then(|| ffmpeg::subtitles_filter(&srt_path));
    progress.check_cancelled()?;
    let label = "assembling film (transitions, music, captions, encode)";
    progress.step("video", label, Some(0.0));
    let mut args: Vec<String> = vec!["-y".into()];
    for (_, p, _, _) in &segs {
        args.extend(["-i".into(), s(p)]);
    }
    for m in crate::music::bed_inputs(&bed_plan, &playlist) {
        args.extend(["-i".into(), s(m)]);
    }
    // Segments are inputs 0..n; the bed placements follow.
    let bed_graph = crate::music::bed_filter(&bed_plan, &playlist, segs.len(), total);
    let (graph, video_map) = film_graph(
        &transitions,
        &cut_overlaps,
        &offsets,
        opts.keep_ambience,
        burn_srt.as_deref(),
        &bed_graph,
        norm,
    );
    args.extend([
        "-filter_complex".into(),
        graph,
        "-map".into(),
        video_map,
        "-map".into(),
        "[aout]".into(),
    ]);
    args.extend(ffmpeg::x264_aac("medium", 19));
    args.extend(opts.resolution.x264_level_args());
    args.extend(["-r".into(), "30".into(), s(out_mp4)]);
    let on_progress = |f: f64| progress.step("video", label, Some(f));
    // Music bed is mandatory: a failed mix fails the film (the old
    // film-audio-only path is exactly the raw-clip-noise bug).
    caps.run_progress(&args, out_mp4, total, &on_progress)
        .map_err(|e| match e {
            crate::MediaError::Cancelled => e,
            e => crate::MediaError::Ffmpeg(format!("film assembly failed: {e}")),
        })?;

    // 6. Verify + EDL + sidecar SRT.
    let (out_w, out_h) = opts.resolution.dims();
    let probed = ffmpeg::verify_video(&caps.ffprobe_bin, out_mp4, out_w, out_h)?;
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
        music: bed_plan.edl(&playlist),
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

/// Bake the lower-third PNG (1200x200 at 1080p, 2x that at 4K) onto `seg` in
/// place (`overlay=48:H-160` in 1080p units, visible
/// `between(t,0.3,segdur)`). `caps.run` writes a partial sibling and
/// renames, so a failure never leaves a torn segment.
fn apply_lower_third(
    caps: &Capabilities,
    seg: &Path,
    png: &Path,
    seg_dur: f64,
    res: Resolution,
) -> Result<(), crate::MediaError> {
    let graph = lower_third_graph(seg_dur, res);
    burn_png_overlay(caps, seg, png, &graph, LOWER_THIRD_CRF)
}

/// Quality of the re-encode that bakes a lower third into its segment.
const LOWER_THIRD_CRF: u32 = 16;

fn lower_third_graph(seg_dur: f64, res: Resolution) -> String {
    let k = res.scale();
    format!(
        "[0:v][1:v]overlay={}:H-{}:format=yuv420:enable='between(t,0.3,{seg_dur:.3})'[v]",
        48 * k,
        160 * k
    )
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
    caps.run(&overlay_args(video, png, filter_graph, crf), video)
}

fn overlay_args(video: &Path, png: &Path, filter_graph: &str, crf: u32) -> Vec<String> {
    vec![
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
        "0:a:0".into(),
    ]
    .into_iter()
    .chain(ffmpeg::x264_aac("veryfast", crf))
    .chain(["-shortest".into(), s(video)])
    .collect()
}

/// Cache key of the lower third baked onto a segment: the base segment's key
/// (its pixels), the overlay text, the burn recipe and the final length. The
/// text PNG's look also rests on the Typst binary and fonts, which the key
/// cannot see; bump `SEGMENT_CACHE_VERSION` if the card templates change.
fn overlay_key(
    cache: &SegmentCache,
    base_key: &str,
    doc: &str,
    seg_dur: f64,
    opts: &VideoOptions,
) -> String {
    let mut k = cache.key("overlay");
    k.part("base", base_key)
        .part("doc", doc)
        .num("dur", seg_dur)
        .part("ppi", &opts.resolution.ppi().to_string())
        .part("typst", &opts.typst_bin)
        .args(&overlay_args(
            Path::new(KEY_IN),
            Path::new(KEY_PNG),
            &lower_third_graph(seg_dur, opts.resolution),
            LOWER_THIRD_CRF,
        ));
    k.finish()
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
    caps.run(&args, seg)
}

/// Split a collage group into the members whose sources still exist and the
/// fallback notes for those that vanished.
fn split_present<'a>(group: &[&'a ResolvedEntry]) -> (Vec<&'a ResolvedEntry>, Vec<String>) {
    let mut missing = vec![];
    let present = group
        .iter()
        .copied()
        .filter(|r| {
            let ok = r.still_path.exists() || r.motion_path.as_ref().is_some_and(|p| p.exists());
            if !ok {
                missing.push(format!("{} missing on disk; skipped", r.entry.asset_id));
            }
            ok
        })
        .collect();
    (present, missing)
}

/// One rendered hero segment: what to push on the timeline plus the
/// fallback notes it produced, in the order the sequential loop wrote them.
struct GroupOutcome {
    /// Entry shown (a collage carries all members' captions), segment file,
    /// probed seconds, kind. `None` when the group failed and was skipped.
    seg: Option<(ResolvedEntry, PathBuf, f64, String)>,
    /// Every member id when the segment is a collage (keyed by the lead,
    /// `seg.0.entry.asset_id`); empty otherwise.
    collage_members: Vec<String>,
    fallbacks: Vec<String>,
}

/// Short-mode time limits for one segment: `cap` bounds a single entry's
/// length, `budget` what is left of the film for the freeze-pad. Full mode
/// has no budget.
#[derive(Clone, Copy)]
struct SegmentLimits {
    cap: f64,
    budget: f64,
}

impl SegmentLimits {
    const NONE: Self = Self {
        cap: f64::INFINITY,
        budget: f64::INFINITY,
    };
}

/// Motion variant (glide direction, pan direction) of each group, from a
/// stable hash of its lead asset id rather than its position, so adding or
/// removing a shot leaves every other segment's picture, and so its cache
/// key, unchanged.
///
/// Neighbours should differ, so a group whose hashed variant equals the
/// previous group's hashed variant takes the next one. The comparison is
/// against the previous group's *hashed* value, not its final one: a
/// comparison with the final value would chain through the whole film and
/// make every later segment depend on position again. The cost: a group's
/// variant depends on its immediate neighbour, so inserting or removing a
/// shot can change the shot after it, and a rotated group can occasionally
/// land on the same variant as a neighbour that was itself rotated (about
/// 1 in 16 neighbours instead of 1 in 4, versus never with position).
fn glide_variants(groups: &[Vec<&ResolvedEntry>]) -> Vec<usize> {
    let hashed: Vec<usize> = groups
        .iter()
        .map(|g| g.first().map_or(0, |r| stable_variant(&r.entry)))
        .collect();
    hashed
        .iter()
        .enumerate()
        .map(|(i, &h)| {
            if i > 0 && hashed[i - 1] == h {
                (h + 1) % ffmpeg::GLIDE_VARIANTS
            } else {
                h
            }
        })
        .collect()
}

/// Hashed over the asset id and window index, so sibling windows of one clip
/// are not forced to the same variant.
fn stable_variant(entry: &PlanEntry) -> usize {
    let h = blake3::hash(format!("{}#{}", entry.asset_id, entry.window).as_bytes());
    usize::from(h.as_bytes()[0]) % ffmpeg::GLIDE_VARIANTS
}

/// Render, probe and (for short clips) freeze-pad one group. A failed render
/// is a fallback note, not an error: only a cancel comes back as `Err`.
#[allow(clippy::too_many_arguments)]
fn render_group(
    caps: &Capabilities,
    present: &[&ResolvedEntry],
    idx: usize,
    variant: usize,
    still_secs: f64,
    limits: SegmentLimits,
    opts: &VideoOptions,
    work_dir: &Path,
) -> Result<GroupOutcome, crate::MediaError> {
    let r = present[0];
    let mut fallbacks = vec![];
    let rendered = if present.len() == 1 {
        render_entry_segment(
            caps,
            r,
            idx,
            variant,
            still_secs,
            limits.cap,
            opts.resolution,
            work_dir,
            &mut fallbacks,
        )
    } else {
        render_collage_segment(
            caps,
            present,
            idx,
            variant,
            still_secs,
            opts.resolution,
            work_dir,
        )
    };
    let (seg_path, dur, kind) = match rendered {
        Ok(v) => v,
        Err(crate::MediaError::Cancelled) => return Err(crate::MediaError::Cancelled),
        Err(e) => {
            fallbacks.push(format!("{} skipped: {e}", r.entry.asset_id));
            return Ok(GroupOutcome {
                seg: None,
                collage_members: vec![],
                fallbacks,
            });
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
    let dur = match clip_pad_target(probed, limits.budget).filter(|_| kind == "clip") {
        Some(target) => match pad_segment_freeze(caps, &seg_path, target) {
            Ok(()) => target,
            Err(crate::MediaError::Cancelled) => return Err(crate::MediaError::Cancelled),
            Err(e) => {
                fallbacks.push(format!("{} freeze-pad failed ({e})", r.entry.asset_id));
                probed
            }
        },
        None => probed,
    };
    // A collage segment carries every member's caption and id, so the
    // sidecar and EDL account for all the photos on screen.
    let mut seg_entry = r.clone();
    let mut collage_members = vec![];
    if present.len() > 1 {
        seg_entry.caption = collage_caption(present);
        collage_members = present.iter().map(|m| m.entry.asset_id.clone()).collect();
    }
    Ok(GroupOutcome {
        seg: Some((seg_entry, seg_path, dur, kind)),
        collage_members,
        fallbacks,
    })
}

/// Hero segments rendered at once by default: a quarter of the cores (the
/// single-threaded stages between encodes leave the rest idle for the other
/// workers), cut down so the workers' peak memory stays within half of RAM.
pub fn default_jobs(res: Resolution) -> usize {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    jobs_for(cores, total_memory_bytes(), res)
}

/// [`default_jobs`] for explicit machine facts. Unknown memory assumes 8 GiB.
pub fn jobs_for(cores: usize, mem_bytes: Option<u64>, res: Resolution) -> usize {
    let by_cpu = (cores / 4).max(1);
    let mem = mem_bytes.unwrap_or(8 << 30);
    let by_mem = (mem / 2 / res.segment_peak_bytes()).max(1) as usize;
    by_cpu.min(by_mem)
}

/// ffmpeg threads per concurrent worker: ffmpeg's own default is 1.5x the
/// cores for one process, so split that between the workers. `None` for a
/// lone worker keeps the unrestricted default.
fn thread_share(jobs: usize) -> Option<usize> {
    if jobs <= 1 {
        return None;
    }
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    Some((cores * 3 / 2 / jobs).max(2))
}

/// Physical RAM in bytes, when the OS says.
pub fn total_memory_bytes() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        let kb: u64 = text
            .lines()
            .find_map(|l| l.strip_prefix("MemTotal:"))?
            .split_whitespace()
            .next()?
            .parse()
            .ok()?;
        Some(kb * 1024)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Normalize one plan entry to a uniform segment. Returns (path, secs, kind).
#[allow(clippy::too_many_arguments)]
fn render_entry_segment(
    caps: &Capabilities,
    r: &ResolvedEntry,
    idx: usize,
    variant: usize,
    still_secs: f64,
    max_secs: f64,
    res: Resolution,
    work_dir: &Path,
    fallbacks: &mut Vec<String>,
) -> Result<(PathBuf, f64, String), crate::MediaError> {
    match &r.entry.rendition {
        Rendition::Still => {
            let out = work_dir.join(format!("seg_{idx:03}.mp4"));
            let key = |c: &SegmentCache| still_key(c, caps, r, variant, still_secs, max_secs, res);
            cached_segment(caps, key, out, "still", |out| {
                // Decode first: looping a raw HEIC fails on tmap/Dolby Vision items.
                let png = ffmpeg::decode_still_to_png(&caps.ffmpeg_bin, &r.still_path)?;
                // The decoded (oriented) pixels are the truth about aspect.
                let aspect = image::image_dimensions(&png.path)
                    .ok()
                    .filter(|&(w, h)| w > 0 && h > 0)
                    .map(|(w, h)| f64::from(w) / f64::from(h))
                    .or(r.aspect);
                let dur = if let Some(aspect) = aspect.filter(|&a| is_panorama(a)) {
                    let motion = pano_motion(aspect);
                    // A short-mode budget can shorten the pan, never below a still.
                    let dur = motion.hold_secs.min(max_secs.max(still_secs));
                    // Travel less when shortened so the pan keeps its speed bound.
                    let coverage = pano_coverage(aspect, dur);
                    render_pano_segment(caps, &png.path, dur, aspect, coverage, variant, res, out)?;
                    dur
                } else {
                    render_hold_segment(caps, &png.path, still_secs, variant, res, out)?;
                    still_secs
                };
                Ok(dur)
            })
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
            let (pw, ph) = res.clip_prescale();
            // Framing follows the displayed (autorotated) aspect.
            let display = probe.as_ref().map(ffmpeg::ProbeInfo::display_dims);
            let framing = display.map_or(ClipFraming::Cover, |(w, h)| {
                clip_framing(f64::from(w) / f64::from(h.max(1)))
            });
            // Lanczos where the source is smaller than the pre-scale (the
            // upscale must not smear); the default filter for downscales,
            // where lanczos only costs time.
            let upscaling = display.is_some_and(|(w, h)| match framing {
                ClipFraming::Cover => w < pw || h < ph,
                ClipFraming::Contain => w < pw && h < ph,
            });
            let tonemap = probe.as_ref().and_then(|i| {
                let chain = ffmpeg::tonemap_chain(caps, &i.color_transfer)?;
                fallbacks.push(format!(
                    "{} HDR tone-mapped ({})",
                    r.entry.asset_id, i.color_transfer
                ));
                Some(chain)
            });
            // Normalized to 30 fps before any per-frame work: 60 fps
            // sources would otherwise composite twice the frames, and the
            // glide's progress counts frames.
            let mut vf = String::from("fps=30,");
            match framing {
                ClipFraming::Cover => {
                    let flags = if upscaling { ":flags=lanczos" } else { "" };
                    vf.push_str(&format!(
                        "scale={pw}:{ph}:force_original_aspect_ratio=increase{flags},crop={pw}:{ph},setsar=1"
                    ));
                    if let Some(chain) = &tonemap {
                        vf.push(',');
                        vf.push_str(chain);
                    }
                }
                ClipFraming::Contain => vf.push_str(&ffmpeg::video_contain_over_blur(
                    caps,
                    pw,
                    ph,
                    upscaling,
                    tonemap.as_deref(),
                )),
            }
            // Slow glide push-in on clips (1.0 -> 1.05, gentler when the
            // clip is shown whole), matching the stills' Ken Burns drift
            // so cuts don't sit static.
            let frames = (len * 30.0).round().max(1.0) as u32;
            vf.push(',');
            vf.push_str(&ffmpeg::glide_depth(
                caps,
                frames,
                res.width(),
                res.height(),
                0,
                framing.glide_depth(),
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
                // Only the first usable stream: iPhone spatial `apple_apac`
                // tracks cannot be muxed to MP4 and sank the whole clip.
                ffmpeg::audio_spec(probe.as_ref())
            } else {
                fallbacks.push(format!(
                    "{} has no audio; silent track added",
                    r.entry.asset_id
                ));
                args.extend(silent_input(len));
                "1:a".to_string()
            };
            args.extend([
                "-map".into(),
                "0:v".into(),
                "-map".into(),
                audio_in,
                "-vf".into(),
                vf,
                "-af".into(),
                "aresample=48000,aformat=channel_layouts=stereo".into(),
            ]);
            args.extend(ffmpeg::x264_aac("veryfast", 16));
            args.extend(["-shortest".into(), s(&out)]);
            // The finished argument list (trim, filters, audio choice) is the
            // whole recipe; the output name, last, only carries the position.
            let recipe = &args[..args.len() - 1];
            let key = |c: &SegmentCache| {
                let mut k = c.key("clip");
                k.file("src", src).args(recipe);
                k.finish()
            };
            cached_segment(caps, key, out, "clip", |out| {
                caps.run(&args, out)?;
                Ok(len)
            })
        }
    }
}

/// Most of a clip's area a 16:9 cover crop may cut away before the clip is
/// shown whole over a blurred fill instead. 16:10 (0.10) and 16:9 itself
/// cover; 3:2 (0.16), 4:3 (0.25), squares and portraits (0.44 and up) do
/// not.
const MAX_CLIP_CROP: f64 = 0.15;

/// How a video clip fills the 16:9 frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipFraming {
    /// Scale to cover and crop the overflow (clips close to 16:9).
    Cover,
    /// Whole frame, centred over a blurred copy of itself.
    Contain,
}

impl ClipFraming {
    /// Glide window divisor: contained clips keep the whole subject on
    /// screen, so they push in more gently.
    fn glide_depth(self) -> u32 {
        match self {
            Self::Cover => ffmpeg::GLIDE_DEPTH,
            Self::Contain => ffmpeg::GLIDE_DEPTH_GENTLE,
        }
    }
}

/// Framing for a clip of displayed aspect `aspect` (width / height, after
/// rotation metadata is applied).
fn clip_framing(aspect: f64) -> ClipFraming {
    if crop_loss(aspect, 16.0 / 9.0) <= MAX_CLIP_CROP {
        ClipFraming::Cover
    } else {
        ClipFraming::Contain
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
fn hold_graph(caps: &Capabilities, variant: usize, frames: u32, res: Resolution) -> String {
    // Supersample the composite once, hold it, then glide over it. The fill
    // blur radius is in canvas pixels, so it scales with the resolution.
    let (sw, sh) = res.supersample();
    let (w, h) = res.dims();
    let composite = ffmpeg::fit_over_blur(caps, "[0:v]", sw, sh, "", 40 * res.scale());
    let hold = ffmpeg::hold_still(frames);
    let glide = ffmpeg::glide(caps, frames, w, h, variant);
    format!("{composite},{hold},{glide},format=yuv420p[v]")
}

/// Collage segment: up to four same-chapter stills composed into one
/// supersample canvas (1.5x the frame; cover-fitted cells, dark gaps), then held with the
/// usual glide. One segment shows the day's spread at once.
fn render_collage_segment(
    caps: &Capabilities,
    members: &[&ResolvedEntry],
    idx: usize,
    variant: usize,
    still_secs: f64,
    res: Resolution,
    work_dir: &Path,
) -> Result<(PathBuf, f64, String), crate::MediaError> {
    let out = work_dir.join(format!("seg_{idx:03}.mp4"));
    let dur = collage_hold_secs(members.len(), still_secs);
    // Members in order, plus the hold recipe; the layout and crop code is
    // pixel logic outside ffmpeg's arguments, covered by the cache version.
    let key = |c: &SegmentCache| {
        let mut k = c.key("collage");
        for (i, m) in members.iter().enumerate() {
            k.file(&format!("member{i}"), &m.still_path);
        }
        k.part("res", &format!("{:?}", res.dims()));
        k.args(&hold_args(
            caps,
            Path::new(KEY_IN),
            dur,
            variant,
            res,
            Path::new(KEY_OUT),
        ));
        k.finish()
    };
    cached_segment(caps, key, out, "still", |out| {
        let canvas = work_dir.join(format!("collage_{idx:03}.png"));
        compose_collage(&caps.ffmpeg_bin, members, res, &canvas)?;
        render_hold_segment(caps, &canvas, dur, variant, res, out)?;
        Ok(dur)
    })
}

/// Serve the segment at `out` from the export's segment cache when it holds
/// `key`; otherwise `render` it (returning its seconds) and store the result.
/// The key is only built when a cache is in use.
fn cached_segment(
    caps: &Capabilities,
    key: impl FnOnce(&SegmentCache) -> String,
    out: PathBuf,
    kind: &str,
    render: impl FnOnce(&Path) -> Result<f64, crate::MediaError>,
) -> Result<(PathBuf, f64, String), crate::MediaError> {
    let cached = caps.segment_cache.as_deref().map(|c| (c, key(c)));
    if let Some((c, key)) = &cached {
        if let Some(m) = c.fetch(key, &out) {
            return Ok((out, m.secs, kind.into()));
        }
    }
    let secs = render(&out)?;
    if let Some((c, key)) = &cached {
        c.store(key, &out, secs);
    }
    Ok((out, secs, kind.into()))
}

/// Whether an entry may join a collage: a still that is not a panorama
/// (panoramas pan solo) and has no caption (a caption describes one photo,
/// so a captioned still plays solo; same rule as the plan's timing model).
fn collage_eligible(e: &ResolvedEntry) -> bool {
    matches!(e.entry.rendition, Rendition::Still)
        && !e.aspect.is_some_and(is_panorama)
        && !has_caption(e.caption.as_deref())
}

/// Under this width / height a photo counts as a portrait, which the
/// collage layout never puts in a stacked (half-height) cell.
const PORTRAIT_BELOW: f64 = 0.95;
/// Most of a photo a collage cell may crop away before the photo is
/// shown whole over its own blurred fill instead. Faces survive because
/// the crop keeps the top ([`CROP_FOCUS_Y`]); beyond this the layout and
/// the renderer both treat the cell as a poor fit.
const MAX_CELL_CROP: f64 = 0.35;
/// Focal height for cover crops that must cut height: people sit in the
/// upper part of a portrait, so keep the top (0 = top, 0.5 = centre).
const CROP_FOCUS_Y: f64 = 0.35;
/// Weight of fill that [`fill_cell`] contains rather than covers, against
/// one that crops the same amount: shown whole beats cut off, but blur
/// still wastes the cell.
const CONTAIN_PENALTY: f64 = 0.6;
/// Layout tie-break: cost per pair of photos out of capture order.
const REORDER_PENALTY: f64 = 0.01;

/// One collage cell: which member it holds and its rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CollageCell {
    member: usize,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

/// Column structures tried per photo count: each entry is how many cells
/// (1 whole, 2 stacked) its column holds, left to right.
fn column_shapes(n: usize) -> &'static [&'static [usize]] {
    match n {
        1 => &[&[1]],
        2 => &[&[1, 1]],
        3 => &[&[1, 1, 1], &[1, 2], &[2, 1]],
        4 => &[&[1, 1, 1, 1], &[2, 2], &[1, 1, 2], &[2, 1, 1], &[1, 2, 1]],
        _ => &[],
    }
}

fn permutations(n: usize) -> Vec<Vec<usize>> {
    fn go(cur: &mut Vec<usize>, left: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
        if left.is_empty() {
            out.push(cur.clone());
            return;
        }
        for i in 0..left.len() {
            let m = left.remove(i);
            cur.push(m);
            go(cur, left, out);
            cur.pop();
            left.insert(i, m);
        }
    }
    let mut out = vec![];
    go(&mut vec![], &mut (0..n).collect(), &mut out);
    out
}

/// Badness of showing a `sa`-aspect photo in a `ca`-aspect cell: the share
/// cropped away while it still covers, then a discounted rate for the
/// share of the cell wasted on blur once it is shown whole.
fn cell_penalty(sa: f64, ca: f64) -> f64 {
    let loss = crop_loss(sa, ca);
    if loss <= MAX_CELL_CROP {
        loss
    } else {
        MAX_CELL_CROP + CONTAIN_PENALTY * (loss - MAX_CELL_CROP)
    }
}

/// Cells for `order` (members, left to right and top to bottom) arranged
/// into columns of `shape`, with column widths following `weights`.
fn build_columns(
    order: &[usize],
    shape: &[usize],
    weights: &[f64],
    w: u32,
    h: u32,
) -> Vec<CollageCell> {
    let total: f64 = weights.iter().sum();
    let (mut cells, mut x, mut acc, mut next) = (vec![], 0u32, 0.0, 0usize);
    for (k, (&count, wt)) in shape.iter().zip(weights).enumerate() {
        acc += wt;
        let right = if k + 1 == shape.len() {
            w
        } else {
            (f64::from(w) * acc / total).round() as u32
        };
        let (cw, half) = (right - x, h / 2);
        for j in 0..count {
            let (y, ch) = if count == 1 {
                (0, h)
            } else if j == 0 {
                (0, half)
            } else {
                (half, h - half)
            };
            cells.push(CollageCell {
                member: order[next],
                x,
                y,
                w: cw,
                h: ch,
            });
            next += 1;
        }
        x = right;
    }
    cells
}

/// Ideal width of each column of `shape` filled by `order`: the width at
/// which its cells match their photos' mean aspect. `None` when a portrait
/// would land in a stacked (short) cell.
fn ideal_column_widths(
    order: &[usize],
    shape: &[usize],
    aspects: &[f64],
    h: u32,
) -> Option<Vec<f64>> {
    let mut at = 0;
    shape
        .iter()
        .map(|&count| {
            let members = &order[at..at + count];
            at += count;
            if count > 1 && members.iter().any(|&m| aspects[m] < PORTRAIT_BELOW) {
                return None;
            }
            let mean = members.iter().map(|&m| aspects[m]).sum::<f64>() / count as f64;
            Some(mean * f64::from(h) / count as f64)
        })
        .collect()
}

/// Lay out a collage by photo shape (`aspects` are width / height in
/// member order). Every column arrangement (a row of full-height cells,
/// some columns split in two) of every ordering is scored by how much of
/// each photo its cell crops or wastes; the cheapest wins, with a small
/// preference for capture order. Portraits never go in a stacked (short)
/// cell, so selfies get tall columns and landscapes share the rest.
/// Cells tile the canvas exactly.
fn collage_layout(aspects: &[f64], w: u32, h: u32) -> Vec<CollageCell> {
    let n = aspects.len().min(MAX_COLLAGE);
    let total_area = f64::from(w) * f64::from(h);
    let fit = |cells: &[CollageCell]| -> f64 {
        cells
            .iter()
            .map(|c| {
                let ca = f64::from(c.w) / f64::from(c.h);
                cell_penalty(aspects[c.member], ca) * f64::from(c.w) * f64::from(c.h) / total_area
            })
            .sum()
    };
    let mut best: Option<(f64, Vec<CollageCell>)> = None;
    for order in permutations(n) {
        let inversions = (0..n)
            .flat_map(|i| (i + 1..n).map(move |j| (i, j)))
            .filter(|&(i, j)| order[i] > order[j])
            .count();
        let reorder = REORDER_PENALTY * inversions as f64;
        for shape in column_shapes(n) {
            let Some(ideal) = ideal_column_widths(&order, shape, aspects, h) else {
                continue;
            };
            // Candidate widths: each column's ideal (floored so no column
            // collapses below half the mean), a sqrt-damped blend, and
            // equal columns.
            let mean_ideal = ideal.iter().sum::<f64>() / ideal.len() as f64;
            let floored: Vec<f64> = ideal.iter().map(|&v| v.max(0.5 * mean_ideal)).collect();
            let damped = floored.iter().map(|v| v.sqrt()).collect();
            let sets = [floored, damped, vec![1.0; ideal.len()]];
            for weights in &sets {
                let cells = build_columns(&order, shape, weights, w, h);
                let sc = fit(&cells) + reorder;
                // Strictly better only, so earlier (closer to capture
                // order) candidates win ties despite float noise.
                if best.as_ref().is_none_or(|(b, _)| sc < *b - 1e-9) {
                    best = Some((sc, cells));
                }
            }
        }
    }
    best.map(|(_, cells)| cells).unwrap_or_default()
}

/// Fraction of a `sa`-aspect photo cut away to cover a `ca`-aspect cell.
fn crop_loss(sa: f64, ca: f64) -> f64 {
    1.0 - (sa / ca).min(ca / sa)
}

/// Cover-fit `src` into `tw`x`th`, cutting height around `focus_y`
/// (0 top .. 1 bottom) and width around the centre.
fn cover_fit_focus(src: &image::RgbImage, tw: u32, th: u32, focus_y: f64) -> image::RgbImage {
    let (sw, sh) = (
        f64::from(src.width().max(1)),
        f64::from(src.height().max(1)),
    );
    let scale = (f64::from(tw) / sw).max(f64::from(th) / sh);
    let (nw, nh) = (
        ((sw * scale).round() as u32).max(tw),
        ((sh * scale).round() as u32).max(th),
    );
    let big = image::imageops::resize(src, nw, nh, image::imageops::FilterType::Triangle);
    let x = (nw - tw) / 2;
    let y = (f64::from(nh - th) * focus_y).round() as u32;
    image::imageops::crop_imm(&big, x, y.min(nh - th), tw, th).to_image()
}

/// The whole photo centred over a blurred, cover-fitted copy of itself
/// (the film's usual fit-over-blur, per cell).
fn contain_over_blur(src: &image::RgbImage, tw: u32, th: u32) -> image::RgbImage {
    // Blur a small copy and scale it up: same look, a fraction of the cost.
    let small = cover_fit_focus(src, (tw / 8).max(1), (th / 8).max(1), 0.5);
    let blurred = image::imageops::blur(&small, 3.0);
    let mut cell = image::imageops::resize(&blurred, tw, th, image::imageops::FilterType::Triangle);
    let (sw, sh) = (
        f64::from(src.width().max(1)),
        f64::from(src.height().max(1)),
    );
    let scale = (f64::from(tw) / sw).min(f64::from(th) / sh);
    let (fw, fh) = (
        ((sw * scale).round() as u32).clamp(1, tw),
        ((sh * scale).round() as u32).clamp(1, th),
    );
    let fg = image::imageops::resize(src, fw, fh, image::imageops::FilterType::Triangle);
    image::imageops::replace(
        &mut cell,
        &fg,
        i64::from((tw - fw) / 2),
        i64::from((th - fh) / 2),
    );
    cell
}

/// Fill a cell: cover-crop (top-biased) when little is lost, else the
/// whole photo over its own blur so faces and subjects are never cut off.
fn fill_cell(img: &image::RgbImage, cw: u32, ch: u32) -> image::RgbImage {
    let sa = f64::from(img.width().max(1)) / f64::from(img.height().max(1));
    let ca = f64::from(cw) / f64::from(ch.max(1));
    if crop_loss(sa, ca) <= MAX_CELL_CROP {
        cover_fit_focus(img, cw, ch, CROP_FOCUS_Y)
    } else {
        contain_over_blur(img, cw, ch)
    }
}

/// Compose 2-4 photos into a supersample-size collage canvas with an
/// orientation-aware layout ([`collage_layout`]) and thin dark gaps.
/// Photos decode, then fill their cells, in parallel: each is an
/// independent ffmpeg spawn plus a resize.
fn compose_collage(
    ffmpeg_bin: &str,
    members: &[&ResolvedEntry],
    res: Resolution,
    out: &Path,
) -> Result<(), crate::MediaError> {
    let (w_px, h_px) = res.supersample();
    let gap = 18 * res.scale();
    let images = std::thread::scope(|scope| {
        let workers: Vec<_> = members
            .iter()
            .map(|m| {
                scope.spawn(move || -> Result<image::RgbImage, crate::MediaError> {
                    let png = ffmpeg::decode_still_to_png(ffmpeg_bin, &m.still_path)?;
                    let img = image::open(&png.path)
                        .map_err(|e| crate::MediaError::Io(format!("collage open: {e}")))?;
                    Ok(img.to_rgb8())
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().expect("collage worker panicked"))
            .collect::<Result<Vec<_>, _>>()
    })?;
    let aspects: Vec<f64> = images
        .iter()
        .map(|i| f64::from(i.width().max(1)) / f64::from(i.height().max(1)))
        .collect();
    let cells = collage_layout(&aspects, w_px, h_px);
    let tiles: Vec<image::RgbImage> = std::thread::scope(|scope| {
        let workers: Vec<_> = cells
            .iter()
            .map(|c| {
                let img = &images[c.member];
                scope.spawn(move || fill_cell(img, c.w - gap, c.h - gap))
            })
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().expect("collage worker panicked"))
            .collect()
    });
    let mut canvas = image::RgbImage::from_pixel(w_px, h_px, image::Rgb([13, 20, 32]));
    for (tile, c) in tiles.iter().zip(&cells) {
        image::imageops::replace(
            &mut canvas,
            tile,
            i64::from(c.x + gap / 2),
            i64::from(c.y + gap / 2),
        );
    }
    canvas
        .save(out)
        .map_err(|e| crate::MediaError::Io(format!("collage save: {e}")))
}

/// Filter graph of a panorama pan.
fn pano_graph(dur: f64, aspect: f64, coverage: f64, variant: usize, res: Resolution) -> String {
    let frames = (dur * 30.0).round() as u32;
    format!(
        "[0:v]{},format=yuv420p[v]",
        ffmpeg::pano_pan(
            frames,
            res.width(),
            res.height(),
            aspect,
            coverage,
            variant % 2 == 1
        )
    )
}

/// Panorama segment: pans across the whole image ([`ffmpeg::pano_pan`]);
/// direction alternates with the variant.
#[allow(clippy::too_many_arguments)]
fn render_pano_segment(
    caps: &Capabilities,
    img: &Path,
    dur: f64,
    aspect: f64,
    coverage: f64,
    variant: usize,
    res: Resolution,
    out: &Path,
) -> Result<(), crate::MediaError> {
    let graph = pano_graph(dur, aspect, coverage, variant, res);
    render_graph_segment(caps, img, dur, graph, out)
}

/// Arguments of a still Ken Burns hold.
fn hold_args(
    caps: &Capabilities,
    img: &Path,
    dur: f64,
    variant: usize,
    res: Resolution,
    out: &Path,
) -> Vec<String> {
    let frames = (dur * 30.0).round() as u32;
    graph_segment_args(img, dur, hold_graph(caps, variant, frames, res), out)
}

/// Stand-ins for the input and output paths when a segment's ffmpeg
/// arguments are hashed into its key: the recipe, not where it runs.
const KEY_IN: &str = "<IN>";
const KEY_OUT: &str = "<OUT>";
/// Stand-in for the per-segment overlay PNG, whose name carries its index.
const KEY_PNG: &str = "<PNG>";

/// Cache key of a still entry, before anything is decoded. The source's
/// identity stands in for its pixels (and so for the aspect that picks hold
/// or pan); the generated hold arguments, and the pan arguments for a
/// reference panorama, stand in for the renderer code.
fn still_key(
    cache: &SegmentCache,
    caps: &Capabilities,
    r: &ResolvedEntry,
    variant: usize,
    still_secs: f64,
    max_secs: f64,
    res: Resolution,
) -> String {
    let mut k = cache.key("still");
    k.file("src", &r.still_path)
        .part("res", &format!("{:?}", res.dims()))
        .num("still_secs", still_secs)
        .num("max_secs", max_secs)
        .part("aspect", &format!("{:?}", r.aspect));
    k.args(&hold_args(
        caps,
        Path::new(KEY_IN),
        still_secs,
        variant,
        res,
        Path::new(KEY_OUT),
    ));
    // Any panorama aspect will do: its pan arguments fingerprint the pan code.
    let reference = 3.0;
    let dur = pano_motion(reference).hold_secs;
    k.args(&graph_segment_args(
        Path::new(KEY_IN),
        dur,
        pano_graph(dur, reference, pano_coverage(reference, dur), variant, res),
        Path::new(KEY_OUT),
    ));
    k.finish()
}

/// Still Ken Burns hold.
fn render_hold_segment(
    caps: &Capabilities,
    img: &Path,
    dur: f64,
    variant: usize,
    res: Resolution,
    out: &Path,
) -> Result<(), crate::MediaError> {
    caps.run(&hold_args(caps, img, dur, variant, res, out), out)
}

/// Encode a one-image segment of `dur` seconds through `graph` (a
/// `[0:v]` ... `[v]` filter chain) with silent stereo audio.
fn render_graph_segment(
    caps: &Capabilities,
    img: &Path,
    dur: f64,
    graph: String,
    out: &Path,
) -> Result<(), crate::MediaError> {
    caps.run(&graph_segment_args(img, dur, graph, out), out)
}

fn graph_segment_args(img: &Path, dur: f64, graph: String, out: &Path) -> Vec<String> {
    let frames = (dur * 30.0).round() as u32;
    [
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
    .concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn film_graph_chains_cuts_into_one_graph() {
        let tr = [None, Some(Transition::Fade), Some(Transition::FadeBlack)];
        let (g, map) = film_graph(&tr, &[0.5, 0.8], &[4.5, 8.7], false, None, "BED", "NORM");
        assert!(
            g.starts_with("[0:v][1:v]xfade=transition=fade:duration=0.500:offset=4.500[v1];"),
            "{g}"
        );
        assert!(
            g.contains("[v1][2:v]xfade=transition=fadeblack:duration=0.800:offset=8.700[v];"),
            "{g}"
        );
        // Muted film audio: no acrossfade, the bed is the soundtrack.
        assert!(!g.contains("acrossfade"), "{g}");
        assert!(g.ends_with("BED;[mbed]NORM[aout]"), "{g}");
        assert_eq!(map, "[v]");
    }

    #[test]
    fn film_graph_keeps_ambience_and_burns_captions() {
        let tr = [None, Some(Transition::Fade)];
        let (g, map) = film_graph(
            &tr,
            &[0.5],
            &[4.5],
            true,
            Some("subtitles=x.srt"),
            "BED",
            "NORM",
        );
        assert!(
            g.contains("[0:a][1:a]acrossfade=d=0.500:c1=tri:c2=tri[a];"),
            "{g}"
        );
        assert!(g.contains("[v]subtitles=x.srt[vsub];"), "{g}");
        assert!(g.contains("[a]volume=0.12"), "{g}");
        assert!(g.ends_with("NORM[aout]"), "{g}");
        assert_eq!(map, "[vsub]");
    }

    #[test]
    fn film_graph_single_segment_passes_input_through() {
        let (g, map) = film_graph(&[None], &[], &[], true, None, "BED", "NORM");
        assert!(!g.contains("xfade"), "{g}");
        assert!(g.starts_with("[0:a]volume=0.12"), "{g}");
        assert_eq!(map, "0:v");
        let (g, map) = film_graph(&[None], &[], &[], false, Some("SUB"), "BED", "NORM");
        assert!(g.starts_with("[0:v]SUB[vsub];BED;"), "{g}");
        assert_eq!(map, "[vsub]");
    }

    #[test]
    fn hold_graph_shows_full_photo_with_gentle_zoom() {
        let caps = Capabilities {
            gblur: true,
            perspective: true,
            ..Default::default()
        };
        for idx in 0..4 {
            let g = hold_graph(&caps, idx, 120, Resolution::Hd1080);
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
            let g = hold_graph(&caps, idx, 120, Resolution::Hd1080);
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

    /// Cells tile the canvas exactly, never overlap, use each member once.
    fn assert_tiles(cells: &[CollageCell], n: usize) {
        assert_eq!(cells.len(), n);
        let area: u64 = cells.iter().map(|c| u64::from(c.w) * u64::from(c.h)).sum();
        assert_eq!(area, 2880 * 1620, "{cells:?}");
        let mut members: Vec<usize> = cells.iter().map(|c| c.member).collect();
        members.sort_unstable();
        assert_eq!(members, (0..n).collect::<Vec<_>>());
        for (i, a) in cells.iter().enumerate() {
            assert!(a.x + a.w <= 2880 && a.y + a.h <= 1620, "{a:?}");
            for b in &cells[i + 1..] {
                let apart =
                    a.x + a.w <= b.x || b.x + b.w <= a.x || a.y + a.h <= b.y || b.y + b.h <= a.y;
                assert!(apart, "overlap {a:?} {b:?}");
            }
        }
    }

    /// The plan's film-length estimate must track what the renderer builds:
    /// replay the renderer's own grouping, card durations and transition
    /// assignment over synthetic films and compare totals.
    #[test]
    fn plan_estimate_tracks_renderer_timeline() {
        use triptape_core::plan::{estimate_hero_secs, HeroCards};

        let opts = VideoOptions::default();
        // Deterministic pseudo-random films: (chapter, rendition) per entry.
        let mut seed = 12_345u64;
        let mut next = move |n: u64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 33) % n
        };
        for film in 0..12 {
            let chapters = 3 + film * 2;
            let mut entries: Vec<PlanEntry> = vec![];
            // Some stills carry a caption (some only whitespace, which
            // does not count): captioned ones must play solo.
            let mut captions: HashMap<String, String> = HashMap::new();
            for ch in 0..chapters {
                for k in 0..(1 + next(6) as usize) {
                    let still = next(5) < 3;
                    let id = format!("c{ch:02}e{k}");
                    let secs = 1.0 + next(8) as f64;
                    let caption = match next(5) {
                        0 => Some("Mia on the ferry"),
                        1 => Some("  "),
                        _ => None,
                    };
                    if let Some(c) = caption {
                        captions.insert(id.clone(), c.into());
                    }
                    entries.push(PlanEntry {
                        asset_id: id,
                        window: 0,
                        content_hash: None,
                        size_bytes: 0,
                        mtime: 0,
                        rendition: if still {
                            Rendition::Still
                        } else {
                            Rendition::Clip {
                                trim_start: 0.0,
                                trim_end: secs,
                            }
                        },
                    });
                }
            }
            let chapter_of = |id: &str| id[..3].to_string();
            for cards in [
                HeroCards {
                    route: true,
                    route_stops: 0,
                    places: true,
                },
                HeroCards {
                    route: false,
                    route_stops: 0,
                    places: false,
                },
                HeroCards {
                    route: true,
                    route_stops: 0,
                    places: false,
                },
                HeroCards {
                    route: true,
                    route_stops: 6,
                    places: true,
                },
            ] {
                // -- renderer replay: groups of consecutive same-chapter stills.
                let mut groups: Vec<Vec<&PlanEntry>> = vec![];
                for e in &entries {
                    let is_still = |e: &PlanEntry| {
                        matches!(e.rendition, Rendition::Still)
                            && !has_caption(captions.get(&e.asset_id).map(String::as_str))
                    };
                    match groups.last_mut() {
                        Some(g)
                            if g.len() < MAX_COLLAGE
                                && is_still(e)
                                && is_still(g[0])
                                && chapter_of(&g[0].asset_id) == chapter_of(&e.asset_id) =>
                        {
                            g.push(e)
                        }
                        _ => groups.push(vec![e]),
                    }
                }
                // (asset id, duration, kind) as in the renderer's `segs`.
                let mut segs: Vec<(String, f64, &str)> =
                    vec![("title".into(), opts.title_secs, "title")];
                if cards.route {
                    segs.push(("map".into(), opts.map_secs, "map"));
                }
                for g in &groups {
                    let e = g[0];
                    let (dur, kind) = match &e.rendition {
                        Rendition::Still if g.len() > 1 => {
                            (collage_hold_secs(g.len(), opts.still_secs), "still")
                        }
                        Rendition::Still => (opts.still_secs, "still"),
                        Rendition::Motion {
                            trim_start,
                            trim_end,
                        }
                        | Rendition::Clip {
                            trim_start,
                            trim_end,
                        } => (
                            (trim_end - trim_start)
                                .clamp(0.5, MAX_CLIP_SECS)
                                .max(MIN_SEGMENT_SECS),
                            "clip",
                        ),
                    };
                    segs.push((e.asset_id.clone(), dur, kind));
                }
                if cards.route {
                    segs.push((
                        "closing".into(),
                        closing_map_secs(cards.route_stops, opts.map_secs),
                        "still",
                    ));
                }
                if cards.places {
                    segs.push(("credits".into(), CREDITS_SECS, "still"));
                }
                let mut chapter_break = vec![false; segs.len()];
                for i in 1..segs.len() {
                    let (a, b) = (&segs[i - 1].0, &segs[i].0);
                    let cut_card = |s: &str| is_card(s) && s != "title";
                    chapter_break[i] = cut_card(a)
                        || cut_card(b)
                        || (a != "title" && chapter_of(a) != chapter_of(b));
                }
                let kinds: Vec<SegKind> = segs
                    .iter()
                    .map(|s| match s.2 {
                        "title" => SegKind::Title,
                        "map" => SegKind::Map,
                        "clip" => SegKind::Clip,
                        _ => SegKind::Still,
                    })
                    .collect();
                let transitions = assign_transitions(segs.len(), &chapter_break, &kinds);
                let overlaps: f64 = transitions
                    .iter()
                    .skip(1)
                    .map(|t| transition_overlap(&t.unwrap_or(Transition::Fade)))
                    .sum();
                let actual = segs.iter().map(|s| s.1).sum::<f64>() - overlaps;

                let est = estimate_hero_secs(
                    &entries,
                    |id| CHAPTER_LABELS[id[1..3].parse::<usize>().unwrap()],
                    |_| None,
                    |id| has_caption(captions.get(id).map(String::as_str)),
                    cards,
                );
                let err = (est - actual).abs() / actual;
                assert!(
                    err < 0.015,
                    "film {film} {cards:?}: estimate {est:.1} vs renderer {actual:.1} ({:.2}%)",
                    err * 100.0
                );
            }
        }
    }

    /// Static chapter names for the estimator test (`c00`..`c40`).
    const CHAPTER_LABELS: [&str; 41] = [
        "c00", "c01", "c02", "c03", "c04", "c05", "c06", "c07", "c08", "c09", "c10", "c11", "c12",
        "c13", "c14", "c15", "c16", "c17", "c18", "c19", "c20", "c21", "c22", "c23", "c24", "c25",
        "c26", "c27", "c28", "c29", "c30", "c31", "c32", "c33", "c34", "c35", "c36", "c37", "c38",
        "c39", "c40",
    ];

    #[test]
    fn collage_layouts_tile_the_canvas() {
        const L: f64 = 1.5;
        const P: f64 = 0.75;
        for aspects in [
            vec![L, L],
            vec![P, P],
            vec![P, L],
            vec![L, L, L],
            vec![P, P, P],
            vec![L, P, L],
            vec![P, L, P],
            vec![L, L, L, L],
            vec![P, P, P, L],
            vec![P, P, L, L],
            vec![P, P, P, P],
        ] {
            assert_tiles(&collage_layout(&aspects, 2880, 1620), aspects.len());
        }
    }

    #[test]
    fn two_portraits_sit_side_by_side_at_full_height() {
        let cells = collage_layout(&[0.75, 0.75], 2880, 1620);
        assert!(cells.iter().all(|c| c.h == 1620 && c.y == 0));
        assert_eq!((cells[0].w, cells[1].w), (1440, 1440));
        // A portrait + landscape pair: full-height columns, the landscape
        // wider, each cell near its photo's shape.
        let mixed = collage_layout(&[1.5, 0.75], 2880, 1620);
        assert!(mixed.iter().all(|c| c.h == 1620));
        assert!(mixed[0].w > mixed[1].w);
        let pa = f64::from(mixed[1].w) / f64::from(mixed[1].h);
        assert!(crop_loss(0.75, pa) < 0.1, "portrait cell aspect {pa}");
    }

    #[test]
    fn portraits_get_tall_columns_never_stacked_cells() {
        const L: f64 = 1.5;
        const P: f64 = 0.75;
        // The reviewed case: one landscape + two portrait selfies. Every
        // portrait owns a full-height column that crops it by <= the cap.
        for aspects in [[L, P, P], [P, L, P], [P, P, L]] {
            let cells = collage_layout(&aspects, 3840, 2160);
            for c in &cells {
                if aspects[c.member] < PORTRAIT_BELOW {
                    assert_eq!(c.h, 2160, "portrait in a short cell: {c:?}");
                    let ca = f64::from(c.w) / f64::from(c.h);
                    assert!(crop_loss(P, ca) <= MAX_CELL_CROP, "{c:?} ca={ca}");
                }
            }
        }
        // Mixed fours keep portraits tall too, with landscapes stacked.
        let mixed = [L, P, P, L];
        let cells = collage_layout(&mixed, 3840, 2160);
        assert!(cells
            .iter()
            .filter(|c| mixed[c.member] == P)
            .all(|c| c.h == 2160));
        assert!(cells.iter().any(|c| c.h == 1080));
        // Three portraits: three equal columns.
        let cols = collage_layout(&[0.7, 0.75, 0.8], 2880, 1620);
        assert!(cols.iter().all(|c| c.h == 1620 && c.w == 960));
    }

    #[test]
    fn degenerate_collages_fill_the_canvas() {
        assert!(collage_layout(&[], 2880, 1620).is_empty());
        let one = collage_layout(&[0.75], 2880, 1620);
        assert_eq!(
            one,
            vec![CollageCell {
                member: 0,
                x: 0,
                y: 0,
                w: 2880,
                h: 1620
            }]
        );
    }

    #[test]
    fn landscapes_stack_beside_a_lone_portrait_or_share_a_big_cell() {
        // Lone portrait: tall column; landscapes stack opposite it.
        let cells = collage_layout(&[1.5, 0.75, 1.5], 2880, 1620);
        let tall = cells.iter().find(|c| c.member == 1).unwrap();
        assert_eq!(tall.h, 1620);
        assert!(cells.iter().filter(|c| c.member != 1).all(|c| c.h == 810));
        // All landscape: one big cell beside two stacked.
        let cells = collage_layout(&[1.5, 1.5, 1.5], 2880, 1620);
        assert_eq!(cells.iter().filter(|c| c.h == 1620).count(), 1);
        assert_eq!(cells.iter().filter(|c| c.h == 810).count(), 2);
    }

    #[test]
    fn four_photos_grid_unless_mostly_portrait() {
        let grid = collage_layout(&[1.5, 1.5, 1.5, 1.5], 2880, 1620);
        assert!(grid.iter().all(|c| c.w == 1440 && c.h == 810));
        let cols = collage_layout(&[0.75; 4], 2880, 1620);
        assert!(cols.iter().all(|c| c.h == 1620 && c.w == 720));
    }

    #[test]
    fn clip_framing_follows_displayed_aspect() {
        // 16:9 and 16:10 landscapes keep the cover crop.
        assert_eq!(clip_framing(16.0 / 9.0), ClipFraming::Cover);
        assert_eq!(clip_framing(1.6), ClipFraming::Cover);
        assert_eq!(clip_framing(2.0), ClipFraming::Cover);
        // 4:3, 3:2 Live Photos and 2.39:1 would lose too much.
        assert_eq!(clip_framing(4.0 / 3.0), ClipFraming::Contain);
        assert_eq!(clip_framing(1.5), ClipFraming::Contain);
        assert_eq!(clip_framing(2.39), ClipFraming::Contain);
        // Squares and portraits (a 3840x2160 stream rotated 90) are whole.
        assert_eq!(clip_framing(1.0), ClipFraming::Contain);
        assert_eq!(clip_framing(9.0 / 16.0), ClipFraming::Contain);
        assert_eq!(clip_framing(3.0 / 4.0), ClipFraming::Contain);
    }

    #[test]
    fn rotated_streams_report_displayed_dimensions() {
        let v: serde_json::Value = serde_json::json!({"streams": [{
            "codec_type": "video", "width": 3840, "height": 2160,
            "side_data_list": [{"side_data_type": "Display Matrix", "rotation": -90}]
        }]});
        let info = ffmpeg::parse_probe(&v, Path::new("x.mov")).unwrap();
        assert_eq!(info.display_dims(), (2160, 3840));
        let v = serde_json::json!({"streams": [{
            "codec_type": "video", "width": 1920, "height": 1080,
            "tags": {"rotate": "270"}
        }]});
        let info = ffmpeg::parse_probe(&v, Path::new("x.mov")).unwrap();
        assert_eq!(info.display_dims(), (1080, 1920));
        let v = serde_json::json!({"streams": [{
            "codec_type": "video", "width": 1920, "height": 1080
        }]});
        let info = ffmpeg::parse_probe(&v, Path::new("x.mov")).unwrap();
        assert_eq!(info.display_dims(), (1920, 1080));
    }

    #[test]
    fn contained_clip_graph_blurs_a_small_copy_and_glides_gently() {
        let caps = ffmpeg::Capabilities {
            gblur: true,
            perspective: true,
            ..Default::default()
        };
        let g = ffmpeg::video_contain_over_blur(&caps, 3840, 2160, false, None);
        assert!(
            g.contains("scale=480:270:force_original_aspect_ratio=increase"),
            "{g}"
        );
        assert!(
            g.contains("scale=3840:2160:force_original_aspect_ratio=decrease"),
            "{g}"
        );
        assert!(g.contains("overlay=(W-w)/2:(H-h)/2"), "{g}");
        let gentle = ffmpeg::glide_depth(&caps, 90, 3840, 2160, 0, ffmpeg::GLIDE_DEPTH_GENTLE);
        assert!(gentle.contains("/64"), "{gentle}");
        assert_eq!(
            ffmpeg::glide(&caps, 90, 3840, 2160, 0),
            ffmpeg::glide_depth(&caps, 90, 3840, 2160, 0, 21)
        );
    }

    #[test]
    fn cells_cover_when_close_and_contain_when_far() {
        // 3:2 landscape into a 144x162 cell would lose ~40%: shown whole,
        // so even the photo's top-left corner marker (green) survives.
        let mut land = image::RgbImage::from_pixel(300, 200, image::Rgb([200, 30, 30]));
        for y in 0..20 {
            for x in 0..20 {
                land.put_pixel(x, y, image::Rgb([30, 200, 30]));
            }
        }
        let cell = fill_cell(&land, 144, 162);
        assert_eq!((cell.width(), cell.height()), (144, 162));
        // Whole photo is 144x96 at y 33..129.
        assert_eq!(cell.get_pixel(2, 35), &image::Rgb([30, 200, 30]));
        assert_eq!(cell.get_pixel(100, 81), &image::Rgb([200, 30, 30]));
        // Near-matching portrait covers the cell edge to edge.
        let port = image::RgbImage::from_pixel(300, 400, image::Rgb([30, 30, 200]));
        let cell = fill_cell(&port, 144, 162);
        assert_eq!(cell.get_pixel(0, 0), &image::Rgb([30, 30, 200]));
        assert_eq!(cell.get_pixel(143, 161), &image::Rgb([30, 30, 200]));
    }

    #[test]
    fn panoramas_never_join_collages() {
        let pano = ResolvedEntry {
            aspect: Some(4.4),
            ..card_entry("p", Path::new("p.jpg"))
        };
        let plain = ResolvedEntry {
            aspect: Some(1.5),
            ..card_entry("s", Path::new("s.jpg"))
        };
        let unknown = card_entry("u", Path::new("u.jpg"));
        assert!(!collage_eligible(&pano));
        assert!(collage_eligible(&plain) && collage_eligible(&unknown));
    }

    #[test]
    fn captioned_stills_never_join_collages() {
        let with = |c: Option<&str>| ResolvedEntry {
            caption: c.map(String::from),
            ..card_entry("s", Path::new("s.jpg"))
        };
        assert!(!collage_eligible(&with(Some("Mia on the ferry"))));
        assert!(!collage_eligible(&with(Some(" x "))));
        // Blank captions are no captions.
        assert!(collage_eligible(&with(Some("  "))));
        assert!(collage_eligible(&with(None)));
    }

    #[test]
    fn hold_graph_derives_every_size_from_the_resolution() {
        let caps = Capabilities {
            gblur: true,
            perspective: true,
            ..Default::default()
        };
        let hd = hold_graph(&caps, 0, 120, Resolution::Hd1080);
        assert!(
            hd.contains("scale=2880:1620") && hd.contains("scale=1920:1080"),
            "{hd}"
        );
        assert!(hd.contains("gblur=sigma=40"), "{hd}");
        let uhd = hold_graph(&caps, 0, 120, Resolution::Uhd4k);
        assert!(
            uhd.contains("scale=5760:3240") && uhd.contains("scale=3840:2160"),
            "{uhd}"
        );
        assert!(uhd.contains("gblur=sigma=80"), "{uhd}");
        for leaked in ["2880", "1620", "1920", "1080"] {
            assert!(
                !uhd.contains(leaked),
                "{leaked} leaked into 4K graph: {uhd}"
            );
        }
    }

    #[test]
    fn lower_third_overlay_scales_with_resolution() {
        let hd = lower_third_graph(3.0, Resolution::Hd1080);
        assert!(hd.contains("overlay=48:H-160"), "{hd}");
        let uhd = lower_third_graph(3.0, Resolution::Uhd4k);
        assert!(uhd.contains("overlay=96:H-320"), "{uhd}");
    }

    #[test]
    fn pano_graph_pans_with_crop_window_not_zoompan() {
        let g = ffmpeg::pano_pan(240, 1920, 1080, 4.4, 1.0, false);
        // Strip short side = 2x the frame's, window = the frame at 2x.
        assert!(g.starts_with("scale=-2:2160"), "fills 2x height: {g}");
        assert!(g.contains("crop=w=3840:h=2160"), "{g}");
        assert!(g.ends_with("scale=1920:1080:flags=lanczos,setsar=1"), "{g}");
        assert!(!g.contains("zoompan") && !g.contains("blur"), "{g}");
        assert!(!g.contains("perspective"), "no per-frame resample: {g}");
        assert!(
            g.find("loop=loop=239").unwrap() < g.find("crop=").unwrap(),
            "strip scaled and held once: {g}"
        );
        // Direction flips with `reverse`; tall images scan vertically.
        assert_ne!(g, ffmpeg::pano_pan(240, 1920, 1080, 4.4, 1.0, true));
        let tall = ffmpeg::pano_pan(240, 1920, 1080, 0.25, 1.0, false);
        assert!(tall.starts_with("scale=3840:-2"), "{tall}");
        assert!(
            tall.contains("x='0'") && tall.contains("y='floor((ih-oh)"),
            "{tall}"
        );
        // 4K: 2160 output short side -> 4320 strip, no 1080p numbers.
        let uhd = ffmpeg::pano_pan(240, 3840, 2160, 4.4, 1.0, false);
        assert!(uhd.starts_with("scale=-2:4320"), "{uhd}");
        assert!(uhd.contains("crop=w=7680:h=4320"), "{uhd}");
        assert!(
            uhd.ends_with("scale=3840:2160:flags=lanczos,setsar=1"),
            "{uhd}"
        );
        for leaked in ["1920", "1080"] {
            assert!(!uhd.contains(leaked), "{leaked} leaked: {uhd}");
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

    fn clip_entry(id: &str, src: PathBuf, trim_end: f64) -> ResolvedEntry {
        ResolvedEntry {
            entry: PlanEntry {
                asset_id: id.into(),
                window: 0,
                content_hash: None,
                size_bytes: 0,
                mtime: 0,
                rendition: Rendition::Clip {
                    trim_start: 0.0,
                    trim_end,
                },
            },
            still_path: src.clone(),
            motion_path: Some(src),
            caption: None,
            taken_at: None,
            tz_offset_secs: None,
            aspect: None,
        }
    }

    /// A landscape-coded stream with a 90 degree display matrix (how iPhones
    /// store portrait clips) plus a native odd-sized portrait clip both render
    /// whole over a blurred fill into a clean 1080p segment (odd fitted sizes
    /// once broke zscale's subsampling check).
    #[test]
    fn portrait_clips_render_whole_into_a_landscape_frame() {
        let caps = crate::ffmpeg::probe_capabilities("ffmpeg", "ffprobe");
        if !caps.libx264 || !caps.perspective || !caps.gblur {
            eprintln!("ffmpeg caps insufficient; skipping portrait clip test");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let st = std::process::Command::new(&caps.ffmpeg_bin)
                .args(["-v", "error", "-y"])
                .args(args)
                .status()
                .unwrap();
            assert!(st.success());
        };
        let path = |name: &str| temp.path().join(name).to_string_lossy().into_owned();
        let testsrc = |size: &str, name: &str| {
            let out = path(name);
            let src = format!("testsrc2=s={size}:r=30:d=2");
            run(&["-f", "lavfi", "-i", &src, "-pix_fmt", "yuv420p", &out]);
            out
        };
        let flat = testsrc("640x360", "flat.mp4");
        let rotated = path("rotated.mp4");
        run(&[
            "-display_rotation",
            "90",
            "-i",
            &flat,
            "-c",
            "copy",
            &rotated,
        ]);
        let native = testsrc("358x638", "native.mp4");
        for (name, src) in [("rotated", rotated), ("native", native)] {
            let src = PathBuf::from(src);
            let probe = ffmpeg::ffprobe_file(&caps.ffprobe_bin, &src).unwrap();
            let (dw, dh) = probe.display_dims();
            assert!(dh > dw, "{name}: displayed {dw}x{dh} should be portrait");
            let r = clip_entry(name, src, 1.5);
            let mut fb = vec![];
            let (out, _, kind) = render_entry_segment(
                &caps,
                &r,
                0,
                0,
                3.0,
                10.0,
                Resolution::Hd1080,
                temp.path(),
                &mut fb,
            )
            .unwrap();
            assert_eq!(kind, "clip");
            let info = ffmpeg::ffprobe_file(&caps.ffprobe_bin, &out).unwrap();
            assert_eq!((info.width, info.height), (1920, 1080), "{name}");
        }
    }

    /// Regression: iPhone clips carry a second, spatial `apple_apac` audio
    /// track that the MP4 muxer cannot take; `-map 0:a` mapped it and sank
    /// the clip (ffmpeg exit 234). ffmpeg cannot synthesize apac, so a second
    /// PCM track stands in: the segment must carry exactly one audio stream.
    #[test]
    fn clip_with_second_audio_stream_encodes_one_track() {
        let caps = crate::ffmpeg::probe_capabilities("ffmpeg", "ffprobe");
        if !caps.libx264 || !caps.perspective {
            eprintln!("ffmpeg caps insufficient; skipping multi-audio segment test");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("multi.mkv");
        let made = std::process::Command::new(&caps.ffmpeg_bin)
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=320x180:r=30:d=3",
            ])
            .args([
                "-f",
                "lavfi",
                "-i",
                "sine=f=440:d=3",
                "-f",
                "lavfi",
                "-i",
                "sine=f=880:d=3",
            ])
            .args(["-map", "0", "-map", "1", "-map", "2", "-c:v", "libx264"])
            .args(["-c:a:0", "aac", "-c:a:1", "pcm_s16le", "-ac:a:1", "24"])
            .arg(&src)
            .status()
            .unwrap();
        assert!(made.success());
        let r = clip_entry("c1", src, 2.0);
        let mut fallbacks = vec![];
        let (out, secs, kind) = render_entry_segment(
            &caps,
            &r,
            0,
            0,
            3.0,
            10.0,
            Resolution::Hd1080,
            temp.path(),
            &mut fallbacks,
        )
        .unwrap();
        assert_eq!(kind, "clip");
        assert!((secs - 2.0).abs() < 0.01);
        let info = ffmpeg::ffprobe_file(&caps.ffprobe_bin, &out).unwrap();
        assert!(info.has_audio && info.has_video);
        assert!(fallbacks.is_empty(), "{fallbacks:?}");
        let streams = std::process::Command::new(&caps.ffprobe_bin)
            .args(["-v", "error", "-select_streams", "a", "-show_entries"])
            .args(["stream=index", "-of", "csv=p=0"])
            .arg(&out)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&streams.stdout).lines().count(), 1);
    }

    /// Real iPhone clip with an `apple_apac` track: `TRIPTAPE_APAC_CLIP=<copy of
    /// a .MOV> cargo test -p triptape-media real_apac -- --ignored`.
    #[test]
    #[ignore = "needs a real iPhone clip via TRIPTAPE_APAC_CLIP"]
    fn real_apac_clip_encodes() {
        let Ok(src) = std::env::var("TRIPTAPE_APAC_CLIP") else {
            return;
        };
        let caps = crate::ffmpeg::probe_capabilities("ffmpeg", "ffprobe");
        let temp = tempfile::tempdir().unwrap();
        let src = PathBuf::from(src);
        let r = clip_entry("apac", src, 3.0);
        let mut fallbacks = vec![];
        let (out, _, _) = render_entry_segment(
            &caps,
            &r,
            0,
            0,
            3.0,
            10.0,
            Resolution::Hd1080,
            temp.path(),
            &mut fallbacks,
        )
        .unwrap();
        let info = ffmpeg::ffprobe_file(&caps.ffprobe_bin, &out).unwrap();
        assert!(info.has_audio && info.has_video);
        assert!(
            !fallbacks.iter().any(|f| f.contains("no audio")),
            "{fallbacks:?}"
        );
    }

    /// Full hero pipeline on synthetic stills: Typst title + lower-thirds,
    /// per-cut transitions, and the bundled music bed (no --music passed).
    /// Works in a per-test temp dir (set `KEEP_HERO_SMOKE=1` to keep it and
    /// print its path, to measure RMS after the run).
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
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        if std::env::var_os("KEEP_HERO_SMOKE").is_some() {
            eprintln!("hero smoke kept in {}", temp.keep().display());
        }
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
                    window: 0,
                    content_hash: None,
                    size_bytes: 0,
                    mtime: 0,
                    rendition: Rendition::Still,
                },
                still_path: p,
                motion_path: None,
                caption: if i == 2 {
                    Some("Golden Gate at sunset".into())
                } else {
                    None
                },
                // Oct 6/7/8 2026 12:00 UTC.
                taken_at: Some(1791288000 + i as i64 * 86400),
                tz_offset_secs: None,
                aspect: None,
            });
        }
        let mut captions = HashMap::new();
        captions.insert("a2".to_string(), "Golden Gate at sunset".to_string());
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
                music_paths: Vec::new(),
                keep_ambience: false,
                typst_bin: "typst".into(),
                resolution: Resolution::Hd1080,
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
        // The captioned photo plays solo (keeping its caption), and the EDL
        // names the two uncaptioned photos collaged before it.
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
        // The same words on every member show once.
        let (x, y, z) = (
            member(Some("Tower")),
            member(Some(" Tower ")),
            member(Some("Tower")),
        );
        assert_eq!(collage_caption(&[&x, &y, &z]).as_deref(), Some("Tower"));
    }

    #[test]
    fn closing_secs_counts_only_cards_that_render() {
        assert_eq!(closing_secs(true, 0, true, 3.0), 9.0);
        assert_eq!(closing_secs(true, 1, false, 3.0), 4.5);
        assert_eq!(closing_secs(false, 5, true, 3.0), CREDITS_SECS);
        assert_eq!(closing_secs(false, 0, false, 3.0), 0.0);
        // Two or more stops: the animated recap's budget replaces the card.
        assert_eq!(closing_secs(true, 6, false, 3.0), 8.4);
        assert_eq!(closing_secs(true, 40, true, 3.0), 14.0 + CREDITS_SECS);
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
                    window: 0,
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
                aspect: None,
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
                    window: 0,
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
                aspect: None,
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
                resolution: Resolution::Hd1080,
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
                    window: 0,
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
                aspect: None,
            },
            ResolvedEntry {
                entry: PlanEntry {
                    asset_id: "still1".into(),
                    window: 0,
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
                aspect: None,
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
                music_paths: Vec::new(),
                keep_ambience: false,
                typst_bin: "typst".into(),
                resolution: Resolution::Hd1080,
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

    /// Default (4K) hero render end to end: Typst title, caption burn-in
    /// capable build, level 5.1 H.264, 3840x2160 output.
    #[test]
    fn hero_smoke_4k_default() {
        use triptape_core::types::Plan;

        let caps = crate::ffmpeg::probe_capabilities("ffmpeg", "ffprobe");
        if !caps.libx264 || !caps.xfade || !caps.loudnorm || !caps.perspective {
            eprintln!("ffmpeg caps insufficient; skipping 4K hero smoke");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let img: image::RgbImage =
            image::ImageBuffer::from_pixel(160, 90, image::Rgb([20, 120, 90]));
        let still = dir.join("still.png");
        img.save(&still).unwrap();
        let entry = card_entry("a0", &still);
        let plan = Plan {
            plan_version: 1,
            schema_version: 1,
            hero: vec![],
            reel_selects: vec![],
            chapters: vec![triptape_core::types::Chapter {
                title: "Day 1".into(),
                asset_ids: vec!["a0".into()],
                ..Default::default()
            }],
            captions: HashMap::new(),
            map_polyline: vec![],
            music: triptape_core::types::MusicPick {
                track: "bundled".into(),
                rationale: "default".into(),
            },
        };
        let out = dir.join("hero4k.mp4");
        // No `resolution` override: the default must be 4K.
        render_hero_video(
            &caps,
            &plan,
            &[entry],
            None,
            &dir.join("work"),
            &out,
            &VideoOptions {
                trip_name: "4K Smoke".into(),
                still_secs: 1.0,
                title_secs: 1.0,
                ..Default::default()
            },
        )
        .expect("4k hero render");
        let info = ffmpeg::ffprobe_file(&caps.ffprobe_bin, &out).unwrap();
        assert_eq!((info.width, info.height), (3840, 2160));
        assert!(info.has_audio);
    }

    /// Bench capabilities and resolution: `TRIPTAPE_FFMPEG` (ffprobe beside
    /// it) and `TRIPTAPE_BENCH_RES` (default 4k).
    fn bench_setup() -> (Capabilities, Resolution) {
        let ffmpeg_bin = std::env::var("TRIPTAPE_FFMPEG").unwrap_or_else(|_| "ffmpeg".into());
        let ffprobe_bin = Path::new(&ffmpeg_bin)
            .with_file_name("ffprobe")
            .to_string_lossy()
            .into_owned();
        let caps = crate::ffmpeg::probe_capabilities(&ffmpeg_bin, &ffprobe_bin);
        let res = Resolution::from_flag(
            &std::env::var("TRIPTAPE_BENCH_RES").unwrap_or_else(|_| "4k".into()),
        )
        .unwrap();
        (caps, res)
    }

    /// Visual check: full hero render of real copies (`TRIPTAPE_BENCH_DIR`,
    /// `TRIPTAPE_BENCH_RES`, optional `TRIPTAPE_FFMPEG` for a libass build).
    #[test]
    #[ignore = "needs real media via TRIPTAPE_BENCH_DIR"]
    fn bench_hero() {
        use triptape_core::types::{Chapter, MusicPick, Plan};
        let Ok(dir) = std::env::var("TRIPTAPE_BENCH_DIR") else {
            return;
        };
        let dir = PathBuf::from(dir);
        let (caps, res) = bench_setup();
        let out_dir = dir.join(format!("hero_{}", res.label()));
        let _ = std::fs::remove_dir_all(&out_dir);
        std::fs::create_dir_all(&out_dir).unwrap();
        let mk = |n: &str, cap: Option<&str>, aspect: Option<f64>, clip: bool| {
            let mut e = clip_entry(n, dir.join(n), 4.0);
            if !clip {
                e.entry.rendition = Rendition::Still;
                e.motion_path = None;
            }
            e.caption = cap.map(String::from);
            e.aspect = aspect;
            e.taken_at = Some(1791288000);
            e
        };
        let entries = vec![
            mk(
                "s1.HEIC",
                Some("Golden hour at the temple"),
                Some(1.333),
                false,
            ),
            mk("s2.HEIC", None, Some(1.333), false),
            mk("s3.HEIC", None, Some(1.333), false),
            mk("s4.HEIC", None, Some(1.333), false),
            mk(
                "pano.HEIC",
                Some("The whole bay, left to right"),
                Some(4.436),
                false,
            ),
            mk(
                "portrait.jpg",
                Some("Portrait with a caption"),
                Some(0.5625),
                false,
            ),
            mk("clip4k.mov", Some("A 4K clip"), None, true),
        ];
        let ch = |title: &str, day: u32, place: &str, ids: &[&str]| Chapter {
            title: title.into(),
            asset_ids: ids.iter().map(|s| s.to_string()).collect(),
            place: Some(place.into()),
            day_index: Some(day),
            date_label: Some("Wed, Oct 8".into()),
        };
        let plan = Plan {
            plan_version: 1,
            schema_version: 1,
            hero: vec![],
            reel_selects: vec![],
            chapters: vec![
                ch(
                    "a",
                    1,
                    "Kyoto, Japan",
                    &["s1.HEIC", "s2.HEIC", "s3.HEIC", "s4.HEIC"],
                ),
                ch(
                    "b",
                    2,
                    "Osaka, Japan",
                    &["pano.HEIC", "portrait.jpg", "clip4k.mov"],
                ),
            ],
            captions: HashMap::new(),
            map_polyline: vec![(35.0, 135.7), (34.7, 135.5), (35.6, 139.7), (1.35, 103.8)],
            music: MusicPick {
                track: "bundled".into(),
                rationale: "default".into(),
            },
        };
        let (cw, ch_) = res.dims();
        let card = out_dir.join("card.png");
        crate::map::save_route_card(&plan.map_polyline, &[], cw, ch_, &card).unwrap();
        let t = std::time::Instant::now();
        let out = out_dir.join("hero.mp4");
        let v = render_hero_video(
            &caps,
            &plan,
            &entries,
            Some(&card),
            &out_dir.join("work"),
            &out,
            &VideoOptions {
                trip_name: "Japan to Singapore".into(),
                resolution: res,
                ..Default::default()
            },
        )
        .unwrap();
        eprintln!(
            "BENCH hero {}: {:.1}s wall for {:.1}s film; fallbacks {:?}",
            res.label(),
            t.elapsed().as_secs_f64(),
            v.duration_secs,
            v.fallbacks
        );
    }

    /// Timing bench: `TRIPTAPE_BENCH_DIR=<dir with s1..s4.HEIC pano.HEIC clip4k.mov ...>`
    /// (plus the [`bench_setup`] variables).
    #[test]
    #[ignore = "needs real media via TRIPTAPE_BENCH_DIR"]
    fn bench_segments() {
        let Ok(dir) = std::env::var("TRIPTAPE_BENCH_DIR") else {
            return;
        };
        let dir = PathBuf::from(dir);
        let (caps, res) = bench_setup();
        let work = dir.join(format!("work_{}", res.label()));
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).unwrap();
        let still = |n: &str| card_entry(n, &dir.join(n));
        let only = std::env::var("TRIPTAPE_BENCH_ONLY").unwrap_or_default();
        let time = |label: &str, f: &mut dyn FnMut()| {
            if !only.is_empty() && !only.split(',').any(|o| o == label) {
                return;
            }
            let t = std::time::Instant::now();
            f();
            eprintln!("BENCH {label}: {:.2}s", t.elapsed().as_secs_f64());
        };
        let mut fb = vec![];
        for (label, name) in [("still12mp", "s1.HEIC"), ("pano", "pano.HEIC")] {
            let r = still(name);
            time(label, &mut || {
                let (o, d, _) =
                    render_entry_segment(&caps, &r, 0, 0, 4.0, 99.0, res, &work, &mut fb).unwrap();
                eprintln!("  {label} dur {d:.2}s -> {}", o.display());
                std::fs::copy(&o, work.join(format!("{label}.mp4"))).unwrap();
            });
        }
        let members: Vec<ResolvedEntry> = ["s1.HEIC", "s2.HEIC", "s3.HEIC", "s4.HEIC"]
            .map(still)
            .to_vec();
        let refs: Vec<&ResolvedEntry> = members.iter().collect();
        time("collage4", &mut || {
            let (o, _, _) = render_collage_segment(&caps, &refs, 1, 1, 4.0, res, &work).unwrap();
            std::fs::copy(&o, work.join("collage.mp4")).unwrap();
        });
        for (label, name, end) in [
            ("clip4k", "clip4k.mov", 5.0),
            ("clip1080", "clip1080.mov", 5.0),
            ("live", "live.mov", 3.0),
        ] {
            let r = clip_entry(label, dir.join(name), end);
            time(label, &mut || {
                let (o, d, _) =
                    render_entry_segment(&caps, &r, 2, 2, 4.0, 99.0, res, &work, &mut fb).unwrap();
                eprintln!("  {label} dur {d:.2}s");
                std::fs::copy(&o, work.join(format!("{label}.mp4"))).unwrap();
            });
        }
    }

    /// Five solid stills across two chapters (a trio and a pair collage).
    fn parallel_fixture(dir: &Path) -> (Plan, Vec<ResolvedEntry>) {
        let mut entries = vec![];
        let mut chapters: Vec<triptape_core::types::Chapter> = vec![];
        for i in 0..5usize {
            let img: image::RgbImage = image::ImageBuffer::from_pixel(
                320 + 16 * i as u32,
                240,
                image::Rgb([(i * 20) as u8, 90, 200 - (i * 10) as u8]),
            );
            let p = dir.join(format!("still_{i}.png"));
            img.save(&p).unwrap();
            let id = format!("a{i}");
            if i % 3 == 0 {
                chapters.push(triptape_core::types::Chapter {
                    title: format!("Day {}", chapters.len() + 1),
                    ..Default::default()
                });
            }
            chapters.last_mut().unwrap().asset_ids.push(id.clone());
            entries.push(ResolvedEntry {
                entry: PlanEntry {
                    asset_id: id,
                    window: 0,
                    content_hash: None,
                    size_bytes: 0,
                    mtime: 0,
                    rendition: Rendition::Still,
                },
                still_path: p,
                motion_path: None,
                caption: (i % 3 == 0).then(|| format!("Caption {i}")),
                taken_at: None,
                tz_offset_secs: None,
                aspect: Some(f64::from(320 + 16 * i as u32) / 240.0),
            });
        }
        let plan = Plan {
            plan_version: 1,
            schema_version: 1,
            hero: vec![],
            reel_selects: vec![],
            chapters,
            captions: HashMap::new(),
            map_polyline: vec![],
            music: triptape_core::types::MusicPick {
                track: "bundled".into(),
                rationale: "default".into(),
            },
        };
        (plan, entries)
    }

    fn parallel_caps() -> Option<Capabilities> {
        let caps = crate::ffmpeg::probe_capabilities("ffmpeg", "ffprobe");
        (caps.libx264 && caps.xfade && caps.loudnorm && caps.perspective).then_some(caps)
    }

    fn parallel_opts(jobs: usize) -> VideoOptions {
        VideoOptions {
            trip_name: "Parallel".into(),
            still_secs: 1.0,
            title_secs: 0.5,
            resolution: Resolution::Hd1080,
            jobs: Some(jobs),
            // No Typst: keeps the test independent of installed fonts.
            typst_bin: "typst-not-installed".into(),
            ..Default::default()
        }
    }

    /// A cached re-export reuses every unchanged segment (rendered to a
    /// different work dir), and changing one shot re-renders only its group.
    #[test]
    fn segment_cache_reuses_unchanged_and_rerenders_changed() {
        use crate::segment_cache::{SegmentCache, SegmentCacheConfig};
        let Some(caps) = parallel_caps() else {
            eprintln!("ffmpeg caps insufficient; skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (plan, entries) = parallel_fixture(dir.path());
        let cfg = SegmentCacheConfig {
            dir: dir.path().join("cache"),
            max_bytes: None,
        };
        let run = |name: &str, entries: &[ResolvedEntry]| {
            let work = dir.path().join(name).join(".work");
            let cache = SegmentCache::open(&cfg, "ffmpeg test".into(), &work).unwrap();
            let caps = Capabilities {
                segment_cache: Some(cache.clone()),
                ..caps.clone()
            };
            let out = dir.path().join(format!("{name}.mp4"));
            let v = render_hero_video(&caps, &plan, entries, None, &work, &out, &parallel_opts(2))
                .expect("render");
            let edl: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&v.edl_path).unwrap()).unwrap();
            (cache.stats(), edl["segments"].clone(), v.duration_secs)
        };
        let (first, edl1, dur1) = run("a", &entries);
        assert_eq!(
            (first.hits, first.misses, first.stored),
            (0, 4, 4),
            "{first:?}"
        );
        let (second, edl2, dur2) = run("b", &entries);
        assert_eq!((second.hits, second.misses), (4, 0), "{second:?}");
        assert_eq!(edl1, edl2, "cached film has the same timeline");
        assert!((dur1 - dur2).abs() < 0.05, "{dur1} vs {dur2}");

        // Groups: a0 solo (captioned), a1+a2 collage, a3 solo (captioned), a4
        // solo. Replace a collage member: that group only re-renders.
        let changed = &entries[2].still_path;
        image::ImageBuffer::from_pixel(400, 240, image::Rgb([250u8, 10, 10]))
            .save(changed)
            .unwrap();
        let (third, _, _) = run("c", &entries);
        assert_eq!((third.hits, third.misses), (3, 1), "{third:?}");
    }

    /// Inserting a shot must not change the recipe (so the cache key) of
    /// any still segment except possibly the one right after it.
    #[test]
    fn inserting_a_shot_leaves_other_still_keys_unchanged() {
        use crate::segment_cache::{SegmentCache, SegmentCacheConfig};
        let caps = Capabilities {
            gblur: true,
            perspective: true,
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let cfg = SegmentCacheConfig {
            dir: dir.path().join("c"),
            max_bytes: None,
        };
        let cache = SegmentCache::open(&cfg, "ffmpeg test".into(), &dir.path().join("w")).unwrap();
        let mk = |id: &str| {
            let p = dir.path().join(format!("{id}.png"));
            std::fs::write(&p, id).unwrap();
            let mut e = card_entry(id, &p);
            e.entry.asset_id = id.into();
            e.aspect = Some(1.5);
            e
        };
        let ids: Vec<String> = (0..40).map(|i| format!("asset-{i}")).collect();
        let keys = |entries: &[ResolvedEntry]| -> Vec<(String, String)> {
            let groups: Vec<Vec<&ResolvedEntry>> = entries.iter().map(|e| vec![e]).collect();
            let variants = glide_variants(&groups);
            entries
                .iter()
                .zip(variants)
                .map(|(e, v)| {
                    let k = still_key(&cache, &caps, e, v, 4.0, f64::INFINITY, Resolution::Hd1080);
                    (e.entry.asset_id.clone(), k)
                })
                .collect()
        };
        let base: Vec<ResolvedEntry> = ids.iter().map(|i| mk(i)).collect();
        let before: HashMap<String, String> = keys(&base).into_iter().collect();
        for k in [0, 1, 7, 20, 39, 40] {
            let mut with = base.clone();
            with.insert(k, mk("inserted"));
            let after = keys(&with);
            let changed: Vec<&str> = after
                .iter()
                .filter(|(id, key)| before.get(id).is_some_and(|b| b != key))
                .map(|(id, _)| id.as_str())
                .collect();
            assert!(changed.len() <= 1, "insert at {k} changed {changed:?}");
            if let Some(c) = changed.first() {
                let pos = with.iter().position(|e| e.entry.asset_id == *c).unwrap();
                assert_eq!(pos, k + 1, "only the shot after the insert may change");
            }
        }
    }

    /// Neighbouring groups with the same hash take different variants.
    #[test]
    fn glide_variants_separate_equal_neighbours() {
        let mk = |id: &str| {
            let mut e = card_entry(id, Path::new("x.png"));
            e.entry.asset_id = id.into();
            e
        };
        let a = mk("same");
        let b = mk("same");
        let groups = vec![vec![&a], vec![&b]];
        let v = glide_variants(&groups);
        assert_ne!(v[0], v[1]);
        assert_eq!(v[0], stable_variant(&a.entry));
    }

    /// Lower thirds are cached on the base segment plus their text: a repeat
    /// export skips them all, and changing one chapter-opening shot redoes
    /// only that chapter's overlay.
    #[test]
    fn overlay_cache_reuses_unchanged_lower_thirds() {
        use crate::segment_cache::{SegmentCache, SegmentCacheConfig};
        let Some(caps) = parallel_caps() else {
            eprintln!("ffmpeg caps insufficient; skipping");
            return;
        };
        if !crate::titlecard::typst_available("typst") {
            eprintln!("typst missing; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (plan, entries) = parallel_fixture(dir.path());
        let cfg = SegmentCacheConfig {
            dir: dir.path().join("cache"),
            max_bytes: None,
        };
        let run = |name: &str| {
            let work = dir.path().join(name).join(".work");
            let cache = SegmentCache::open(&cfg, "ffmpeg test".into(), &work).unwrap();
            let caps = Capabilities {
                segment_cache: Some(cache.clone()),
                ..caps.clone()
            };
            let opts = VideoOptions {
                typst_bin: "typst".into(),
                ..parallel_opts(2)
            };
            let out = dir.path().join(format!("{name}.mp4"));
            render_hero_video(&caps, &plan, &entries, None, &work, &out, &opts).expect("render");
            cache.stats()
        };
        // Two chapters open with a0 and a3, so two lower thirds.
        let first = run("a");
        assert_eq!(
            (first.overlay_hits, first.overlay_misses),
            (0, 2),
            "{first:?}"
        );
        let second = run("b");
        assert_eq!(
            (second.overlay_hits, second.overlay_misses),
            (2, 0),
            "{second:?}"
        );
        // Segment counts stay about segments only.
        assert_eq!((second.hits, second.misses), (4, 0), "{second:?}");
        // Replace the shot that opens chapter 1.
        image::ImageBuffer::from_pixel(400, 240, image::Rgb([250u8, 10, 10]))
            .save(&entries[0].still_path)
            .unwrap();
        let third = run("c");
        assert_eq!(
            (third.overlay_hits, third.overlay_misses),
            (1, 1),
            "{third:?}"
        );
    }

    /// Sibling windows of one clip differ in trim, so they get distinct keys
    /// and (hashed with the window index) are free to differ in variant.
    #[test]
    fn sibling_windows_get_distinct_keys_and_variants() {
        use crate::segment_cache::{SegmentCache, SegmentCacheConfig};
        let Some(caps) = parallel_caps() else {
            eprintln!("ffmpeg caps insufficient; skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("long.mp4");
        let made = std::process::Command::new(&caps.ffmpeg_bin)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc=size=320x180:rate=30:duration=4")
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=4"])
            .args(["-shortest", "-c:v", "libx264", "-c:a", "aac"])
            .arg(&src)
            .status()
            .unwrap();
        assert!(made.success());
        let cfg = SegmentCacheConfig {
            dir: dir.path().join("cache"),
            max_bytes: None,
        };
        let work = dir.path().join("w");
        std::fs::create_dir_all(&work).unwrap();
        let cache = SegmentCache::open(&cfg, "ffmpeg test".into(), &work).unwrap();
        let caps = Capabilities {
            segment_cache: Some(cache.clone()),
            ..caps
        };
        let mut windows: Vec<ResolvedEntry> = vec![];
        for (w, (a, b)) in [(0.0, 1.5), (2.0, 3.5)].into_iter().enumerate() {
            let mut r = clip_entry("long", src.clone(), b);
            r.entry.window = w as u32;
            r.entry.rendition = Rendition::Clip {
                trim_start: a,
                trim_end: b,
            };
            windows.push(r);
        }
        for (i, r) in windows.iter().enumerate() {
            let mut fb = vec![];
            render_entry_segment(
                &caps,
                r,
                i,
                0,
                1.0,
                f64::INFINITY,
                Resolution::Hd1080,
                &work,
                &mut fb,
            )
            .unwrap();
        }
        let st = cache.stats();
        assert_eq!((st.hits, st.misses, st.stored), (0, 2, 2), "{st:?}");
        // The window index feeds the variant hash.
        let h = |w: u32| {
            let mut e = windows[0].entry.clone();
            e.window = w;
            stable_variant(&e)
        };
        assert!((1..64).any(|w| h(w) != h(0)), "window ignored by variant");
    }

    /// A trimmed clip is cached on its full recipe: the same trim hits from
    /// another work dir, a different trim misses.
    #[test]
    fn segment_cache_keys_clips_by_trim() {
        use crate::segment_cache::{SegmentCache, SegmentCacheConfig};
        let Some(caps) = parallel_caps() else {
            eprintln!("ffmpeg caps insufficient; skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("clip.mp4");
        let made = std::process::Command::new(&caps.ffmpeg_bin)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc=size=320x180:rate=30:duration=3")
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=3"])
            .args(["-shortest", "-c:v", "libx264", "-c:a", "aac"])
            .arg(&src)
            .status()
            .unwrap();
        assert!(made.success());
        let cfg = SegmentCacheConfig {
            dir: dir.path().join("cache"),
            max_bytes: None,
        };
        let render = |work_name: &str, end: f64| {
            let work = dir.path().join(work_name);
            std::fs::create_dir_all(&work).unwrap();
            let cache = SegmentCache::open(&cfg, "ffmpeg test".into(), &work).unwrap();
            let caps = Capabilities {
                segment_cache: Some(cache.clone()),
                ..caps.clone()
            };
            let r = clip_entry("clip", src.clone(), end);
            let mut fb = vec![];
            let (out, secs, kind) = render_entry_segment(
                &caps,
                &r,
                1,
                1,
                1.0,
                f64::INFINITY,
                Resolution::Hd1080,
                &work,
                &mut fb,
            )
            .unwrap();
            assert_eq!(kind, "clip");
            assert!(out.metadata().unwrap().len() > 0);
            (cache.stats(), secs)
        };
        let (a, secs_a) = render("w1", 2.0);
        assert_eq!((a.hits, a.misses), (0, 1));
        let (b, secs_b) = render("w2", 2.0);
        assert_eq!((b.hits, b.misses), (1, 0), "{b:?}");
        assert_eq!(secs_a, secs_b);
        let (c, _) = render("w3", 1.5);
        assert_eq!((c.hits, c.misses), (0, 1), "{c:?}");
    }

    #[test]
    fn parallel_render_matches_sequential_order_and_timeline() {
        let Some(caps) = parallel_caps() else {
            eprintln!("ffmpeg caps insufficient; skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (plan, entries) = parallel_fixture(dir.path());
        let run = |jobs: usize| {
            let work = dir.path().join(format!("work{jobs}"));
            let out = dir.path().join(format!("hero{jobs}.mp4"));
            let seen = std::sync::Arc::new(Collect::default());
            let caps = Capabilities {
                progress: crate::progress::Progress::new(seen.clone()),
                ..caps.clone()
            };
            let v = render_hero_video(
                &caps,
                &plan,
                &entries,
                None,
                &work,
                &out,
                &parallel_opts(jobs),
            )
            .expect("render");
            let edl: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&v.edl_path).unwrap()).unwrap();
            (edl, v.fallbacks, seen)
        };
        let (edl1, fb1, seen1) = run(1);
        let (edl3, fb3, seen3) = run(3);
        // Same segments, order, probed durations, offsets and notes.
        assert_eq!(edl1["segments"], edl3["segments"]);
        assert_eq!(edl1["total_secs"], edl3["total_secs"]);
        assert_eq!(edl1["cut_overlaps_secs"], edl3["cut_overlaps_secs"]);
        assert_eq!(fb1, fb3);
        assert!(edl1["segments"].as_array().unwrap().len() >= 3);
        // Collages happened, so the plan exercised both group kinds.
        assert!(edl1["segments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s.get("collage_asset_ids").is_some()));
        // Progress counted every segment once, ending complete, both ways.
        for seen in [seen1, seen3] {
            let counts = seen.counts("segment");
            let total = counts.last().unwrap().1;
            assert_eq!(counts.last().unwrap().0, total);
            let dones: Vec<usize> = counts.iter().map(|c| c.0).collect();
            assert!(dones.windows(2).all(|w| w[0] <= w[1]), "{dones:?}");
        }
    }

    #[test]
    fn cancel_mid_parallel_render_leaves_no_partials() {
        let Some(caps) = parallel_caps() else {
            eprintln!("ffmpeg caps insufficient; skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (plan, entries) = parallel_fixture(dir.path());
        // Cancels as soon as the first segment reports done, with the
        // other workers mid-encode.
        let seen = std::sync::Arc::new(Collect::cancel_after_segments(1));
        let caps = Capabilities {
            progress: crate::progress::Progress::new(seen.clone()),
            ..caps
        };
        let work = dir.path().join("work");
        let out = dir.path().join("hero.mp4");
        let res = render_hero_video(&caps, &plan, &entries, None, &work, &out, &parallel_opts(3));
        assert!(matches!(res, Err(crate::MediaError::Cancelled)), "{res:?}");
        assert!(!out.exists());
        let mut leftovers = vec![];
        let mut stack = vec![dir.path().to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(d).unwrap().flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if e.path().is_dir() {
                    stack.push(e.path());
                } else if name.contains(".partial") {
                    leftovers.push(name);
                }
            }
        }
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// Records counted events; optionally asks for cancel after N segments.
    #[derive(Default)]
    struct Collect {
        events: std::sync::Mutex<Vec<crate::progress::ProgressEvent>>,
        cancel_after: Option<usize>,
        stop: std::sync::atomic::AtomicBool,
    }

    impl Collect {
        fn cancel_after_segments(n: usize) -> Self {
            Self {
                cancel_after: Some(n),
                ..Default::default()
            }
        }

        fn counts(&self, unit: &str) -> Vec<(usize, usize)> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|e| match e {
                    crate::progress::ProgressEvent::Counted {
                        unit: u,
                        done,
                        total,
                        ..
                    } if *u == unit => Some((*done, *total)),
                    _ => None,
                })
                .collect()
        }
    }

    impl crate::progress::ProgressSink for Collect {
        fn report(&self, e: &crate::progress::ProgressEvent) {
            if let (
                Some(n),
                crate::progress::ProgressEvent::Counted {
                    unit: "segment",
                    done,
                    ..
                },
            ) = (self.cancel_after, e)
            {
                if *done >= n {
                    self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            self.events.lock().unwrap().push(e.clone());
        }

        fn cancelled(&self) -> bool {
            self.stop.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[test]
    fn job_count_follows_cores_and_memory() {
        // A quarter of the cores, at least one.
        assert_eq!(jobs_for(10, Some(64 << 30), Resolution::Hd1080), 2);
        assert_eq!(jobs_for(16, Some(64 << 30), Resolution::Hd1080), 4);
        assert_eq!(jobs_for(2, Some(64 << 30), Resolution::Uhd4k), 1);
        // Memory caps 4K: half of 16 GiB over the 4K estimate.
        let mem = 16u64 << 30;
        let by_mem = (mem / 2 / Resolution::Uhd4k.segment_peak_bytes()) as usize;
        assert_eq!(jobs_for(32, Some(mem), Resolution::Uhd4k), by_mem.max(1));
        assert_eq!(jobs_for(32, Some(1 << 30), Resolution::Uhd4k), 1);
        // A lone worker keeps ffmpeg's default threading.
        assert_eq!(thread_share(1), None);
        assert!(thread_share(2).unwrap() >= 2);
    }

    /// Visual check of clip framing, a portrait collage and the map card on
    /// real copies in `TRIPTAPE_BENCH_DIR` (portrait.mov, portrait2.mov,
    /// live.mov, land.mov, c1..c3.HEIC); writes `vis_*.mp4` there.
    #[test]
    #[ignore = "needs real media via TRIPTAPE_BENCH_DIR"]
    fn visual_check_framing() {
        let Ok(dir) = std::env::var("TRIPTAPE_BENCH_DIR") else {
            return;
        };
        let dir = PathBuf::from(dir);
        let (caps, res) = bench_setup();
        let work = dir.join(format!("work_{}", res.label()));
        std::fs::create_dir_all(&work).unwrap();
        let mut fb = vec![];
        for (i, name) in ["portrait.mov", "portrait2.mov", "live.mov", "land.mov"]
            .iter()
            .enumerate()
        {
            let r = clip_entry(name, dir.join(name), 3.0);
            let t = std::time::Instant::now();
            let (o, _, _) =
                render_entry_segment(&caps, &r, i, i, 4.0, 99.0, res, &work, &mut fb).unwrap();
            eprintln!("VIS {name}: {:.2}s", t.elapsed().as_secs_f64());
            std::fs::copy(&o, dir.join(format!("vis_{}_{name}.mp4", res.label()))).unwrap();
        }
        let members: Vec<ResolvedEntry> = ["c1.HEIC", "c2.HEIC", "c3.HEIC"]
            .map(|n| card_entry(n, &dir.join(n)))
            .to_vec();
        let refs: Vec<&ResolvedEntry> = members.iter().collect();
        let (o, _, _) = render_collage_segment(&caps, &refs, 9, 1, 4.0, res, &work).unwrap();
        std::fs::copy(&o, dir.join(format!("vis_{}_collage.mp4", res.label()))).unwrap();
        let (w, h) = res.dims();
        let card = dir.join(format!("map_{}.png", res.label()));
        let route = [(35.7, 139.8), (34.7, 135.5), (1.35, 103.8)];
        crate::map::save_route_card(&route, &[0], w, h, &card).unwrap();
        let out = dir.join(format!("vis_{}_map.mp4", res.label()));
        crate::map::render_map_push_in(&caps, &card, &out, 3.0, 0, res).unwrap();
        eprintln!("fallbacks: {fb:?}");
    }

    #[test]
    fn srt_timestamps() {
        assert_eq!(srt_ts(0.0), "00:00:00,000");
        assert_eq!(srt_ts(61.5), "00:01:01,500");
        assert_eq!(srt_ts(3723.25), "01:02:03,250");
    }
}
