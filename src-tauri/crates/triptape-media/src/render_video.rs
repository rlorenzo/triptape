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
            .filter_map(|r| r.taken_at)
            .collect::<Vec<_>>(),
    );
    let title_is_text = if typst_ok {
        let doc = crate::titlecard::title_card_doc(&opts.trip_name, date_range.as_deref());
        match crate::titlecard::render_text_png(&opts.typst_bin, &doc, &title_png, 72) {
            Ok(()) => true,
            Err(e) => {
                fallbacks.push(format!("title text failed ({e}); solid card"));
                crate::titlecard::save_title_card(&opts.trip_name, 1920, 1080, &title_png)?;
                false
            }
        }
    } else {
        crate::titlecard::save_title_card(&opts.trip_name, 1920, 1080, &title_png)?;
        false
    };
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
        ResolvedEntry {
            entry: PlanEntry {
                asset_id: "title".into(),
                content_hash: None,
                size_bytes: 0,
                mtime: 0,
                rendition: Rendition::Still,
            },
            still_path: title_png,
            motion_path: None,
            caption: None,
            taken_at: None,
        },
        title_mp4,
        opts.title_secs,
        "title".into(),
    ));

    // Map push-in when GPS exists and a card was rendered.
    if let Some(card) = map_card {
        if !plan.map_polyline.is_empty() {
            let map_mp4 = work_dir.join("seg_map.mp4");
            match crate::map::render_map_push_in(caps, card, &map_mp4, opts.map_secs) {
                Ok(()) => segs.push((
                    ResolvedEntry {
                        entry: PlanEntry {
                            asset_id: "map".into(),
                            content_hash: None,
                            size_bytes: 0,
                            mtime: 0,
                            rendition: Rendition::Still,
                        },
                        still_path: card.to_path_buf(),
                        motion_path: None,
                        caption: None,
                        taken_at: None,
                    },
                    map_mp4,
                    opts.map_secs,
                    "map".into(),
                )),
                Err(e) => fallbacks.push(format!("map segment skipped: {e}")),
            }
        }
    }

    // Hero entries (short mode: cap at ~30 s of content).
    // Fixed segments (title + optional map) come out of the budget first;
    // each entry is checked and clip-capped before it is encoded.
    let mut budget = if opts.short_mode {
        30.0 - segs.iter().map(|s| s.2).sum::<f64>()
    } else {
        f64::INFINITY
    };
    for (idx, r) in resolved.iter().enumerate() {
        if !r.still_path.exists() && r.motion_path.as_ref().is_none_or(|p| !p.exists()) {
            fallbacks.push(format!("{} missing on disk; skipped", r.entry.asset_id));
            continue;
        }
        // Below the 0.5 s clip floor nothing more fits.
        if budget < 0.5 {
            fallbacks.push(format!(
                "short variant: {} and later dropped to fit ~30 s",
                r.entry.asset_id
            ));
            break;
        }
        let (seg_path, dur, kind) = match render_entry_segment(
            caps,
            r,
            idx,
            still_secs,
            budget,
            work_dir,
            &mut fallbacks,
        ) {
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
        let dur = ffmpeg::ffprobe_file(&caps.ffprobe_bin, &seg_path)
            .ok()
            .map(|p| p.duration_secs)
            .filter(|d| *d > 0.0)
            .unwrap_or(dur);
        budget -= dur;
        segs.push((r.clone(), seg_path, dur, kind));
    }
    if segs.len() <= 1 {
        return Err(crate::MediaError::Ffmpeg(
            "no usable hero segments (all sources missing)".into(),
        ));
    }

    // 2. Chapter starts: cut into segment i when its chapter differs.
    let mut chapter_break = vec![false; segs.len()];
    for i in 1..segs.len() {
        let (a, b) = (&segs[i - 1].0.entry.asset_id, &segs[i].0.entry.asset_id);
        if a == "title" || a == "map" || b == "map" {
            chapter_break[i] = a == "map" || b == "map";
        } else {
            chapter_break[i] = chapter_of(a) != chapter_of(b);
        }
    }

    // A segment opens its chapter when the previous content segment (title
    // and map cards skipped) is in another chapter or there is none. Kept
    // apart from `chapter_break`, which is false for the cut out of the
    // title card, so the first chapter still gets its title overlay.
    let mut chapter_start = vec![false; segs.len()];
    let mut prev_chapter: Option<usize> = None;
    for (i, seg) in segs.iter().enumerate() {
        let id = seg.0.entry.asset_id.as_str();
        if id == "title" || id == "map" {
            continue;
        }
        let ch = chapter_of(id);
        chapter_start[i] = prev_chapter != Some(ch);
        prev_chapter = Some(ch);
    }

    // 3. Per-segment context overlays (Typst lower-thirds baked per segment
    // so they ride through the xfade chain): chapter title on the chapter's
    // first segment, else date + caption. Title/map cards carry no overlay.
    if typst_ok {
        let chapter_title: HashMap<&str, &str> = plan
            .chapters
            .iter()
            .flat_map(|c| c.asset_ids.iter().map(|id| (id.as_str(), c.title.as_str())))
            .collect();
        for (i, (r, seg_mp4, dur, _)) in segs.iter().enumerate() {
            let id = r.entry.asset_id.as_str();
            if id == "title" || id == "map" {
                continue;
            }
            let ch_title = chapter_start[i]
                .then(|| chapter_title.get(id).copied())
                .flatten();
            let Some((top, bottom)) =
                crate::titlecard::overlay_lines(ch_title, r.taken_at, r.caption.as_deref())
            else {
                continue;
            };
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

    // 5. Stepwise pairwise assembly.
    let mut acc = segs[0].1.clone();
    for i in 1..segs.len() {
        let t = transitions[i].unwrap_or(Transition::Fade);
        let ov = cut_overlaps[i - 1];
        let next = &segs[i].1;
        let chained = work_dir.join(format!("chain_{i:03}.mp4"));
        let offset = offsets[i - 1];
        let mut args: Vec<String> = vec![
            "-y".into(),
            "-i".into(),
            s(&acc),
            "-i".into(),
            s(next),
            "-filter_complex".into(),
            format!(
                "[0:v][1:v]xfade=transition={}:duration={:.3}:offset={:.3}[v];[0:a][1:a]acrossfade=d={:.3}:c1=tri:c2=tri[a]",
                t.xfade_name(),
                ov,
                offset.max(0.0),
                ov
            ),
            "-map".into(),
            "[v]".into(),
            "-map".into(),
            "[a]".into(),
        ];
        args.extend(ffmpeg::x264_aac("veryfast", 16));
        args.push(s(&chained));
        ffmpeg::run_ffmpeg(&caps.ffmpeg_bin, &args, &chained)?;
        acc = chained;
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
    let args: Vec<String> = vec![
        "-y".into(),
        "-i".into(),
        s(seg),
        "-loop".into(),
        "1".into(),
        "-i".into(),
        s(png),
        "-filter_complex".into(),
        format!(
            "[0:v][1:v]overlay=48:H-160:format=yuv420:enable='between(t,0.3,{:.3})'[v]",
            seg_dur
        ),
        "-map".into(),
        "[v]".into(),
        "-map".into(),
        "0:a".into(),
    ]
    .into_iter()
    .chain(ffmpeg::x264_aac("veryfast", 16))
    .chain(["-shortest".into(), s(seg)])
    .collect();
    ffmpeg::run_ffmpeg(&caps.ffmpeg_bin, &args, seg)
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
            let en = (starts[i] + (d - 0.6).min(3.5)).max(st + 0.5);
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
            render_hold_segment(caps, &r.still_path, still_secs, idx, &out)?;
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
                .clamp(0.5, 30.0)
                .min(max_secs.max(0.5));
            // Live motion (≤3 s class) may run short; reels/video allow that.
            let out = work_dir.join(format!("seg_{idx:03}.mp4"));
            // HDR check on the real source.
            let probe = ffmpeg::ffprobe_file(&caps.ffprobe_bin, src).ok();
            // Muted/camera-silent clips get a matching silent track so the
            // acrossfade assembly always has `[n:a]` to consume.
            let has_audio = probe.as_ref().is_none_or(|i| i.has_audio);
            // Upscaled first so the zoompan push-in samples, not smears.
            let mut vf = if caps.zoompan {
                "scale=2560:1440:force_original_aspect_ratio=increase,crop=2560:1440,setsar=1"
                    .to_string()
            } else {
                "scale=1920:1080:force_original_aspect_ratio=increase,crop=1920:1080,setsar=1"
                    .to_string()
            };
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
            // Slow push-in on clips (1.0 -> 1.07), matching the stills' Ken
            // Burns drift so cuts don't sit static.
            let frames = (len * 30.0).round().max(1.0) as u32;
            if caps.zoompan {
                vf.push_str(&format!(",zoompan=z='1+0.07*on/{frames}':x='iw/2-(iw/zoom/2)':y='ih/2-(ih/zoom/2)':d=1:s=1920x1080:fps=30"));
            } else {
                vf.push_str(",fps=30");
            }
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
    let composite = ffmpeg::fit_over_blur(caps, "[0:v]", 1920, 1080, "");
    if caps.zoompan {
        let zoompan = match idx % 4 {
            0 => ffmpeg::push_in(0.05, frames, 1920, 1080),
            1 => format!("zoompan=z='1.05-0.05*on/{frames}':x='iw/2-(iw/zoom/2)':y='ih/2-(ih/zoom/2)':d={frames}:s=1920x1080:fps=30"),
            2 => format!("zoompan=z=1.05:x='(iw-iw/zoom)*on/{frames}':y='ih/2-(ih/zoom/2)':d={frames}:s=1920x1080:fps=30"),
            _ => format!("zoompan=z=1.05:x='(iw-iw/zoom)*(1-on/{frames})':y='ih/2-(ih/zoom/2)':d={frames}:s=1920x1080:fps=30"),
        };
        format!("{composite},{zoompan},format=yuv420p[v]")
    } else {
        format!("{composite},fps=30,format=yuv420p[v]")
    }
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
            zoompan: true,
            ..Default::default()
        };
        for idx in 0..4 {
            let g = hold_graph(&caps, idx, 120);
            // Full photo over blurred fill, zoom on the composite only,
            // never past 1.05x.
            assert!(g.contains("[bg][fg]overlay"), "{g}");
            assert!(
                g.find("overlay").unwrap() < g.find("zoompan").unwrap(),
                "{g}"
            );
            assert!(!g.contains("0.12"), "zoom capped at 1.05: {g}");
            assert!(!g.contains("scale=2400"), "no raw-photo zoom: {g}");
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
    fn overlay_lines_builder() {
        use crate::titlecard::{format_date_range, format_day, overlay_lines};
        // 2026-10-06 12:00 UTC.
        let day = 1791288000i64;
        assert_eq!(format_day(day).as_deref(), Some("Oct 6, 2026"));
        assert_eq!(
            format_date_range(&[day, day + 3 * 86400]).as_deref(),
            Some("Oct 6, 2026 – Oct 9, 2026")
        );
        assert_eq!(format_date_range(&[day]).as_deref(), Some("Oct 6, 2026"));
        assert_eq!(format_date_range(&[]), None);
        // Chapter start leads with the chapter title.
        assert_eq!(
            overlay_lines(Some("Day 2 · Coast"), Some(day), Some("Big surf")),
            Some((
                "Day 2 · Coast".to_string(),
                Some("Oct 6, 2026 · Big surf".to_string())
            ))
        );
        // Plain segment: date + caption.
        assert_eq!(
            overlay_lines(None, Some(day), Some("Big surf")),
            Some(("Oct 6, 2026".to_string(), Some("Big surf".to_string())))
        );
        // Nothing to show.
        assert_eq!(overlay_lines(None, None, Some("  ")), None);
        assert_eq!(overlay_lines(None, None, None), None);
    }

    /// Full hero pipeline on synthetic stills: Typst title + lower-thirds,
    /// per-cut transitions, and the bundled music bed (no --music passed).
    /// Writes to /tmp/hero-smoke so RMS can be measured after the run.
    /// Skips quietly when ffmpeg/typst are unavailable.
    #[test]
    fn hero_smoke_bundled_music_and_overlays() {
        use triptape_core::types::Plan;

        let caps = crate::ffmpeg::probe_capabilities("ffmpeg", "ffprobe");
        if !caps.libx264 || !caps.xfade || !caps.loudnorm || !caps.zoompan {
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
                },
                triptape_core::types::Chapter {
                    title: "Day 2 · Coast".into(),
                    asset_ids: vec!["a2".into()],
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
        assert_eq!(cuts.len(), 3, "title + 3 stills = 3 cuts");
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
    }

    /// Regression: a clip whose planned trim (4 s) exceeds its real length
    /// (0.5 s) must not truncate the film. Timeline math uses probed
    /// segment durations; with planned durations the second xfade offset
    /// lands past the accumulated video and the chain collapses.
    #[test]
    fn hero_smoke_short_clip_does_not_truncate() {
        use triptape_core::types::Plan;

        let caps = crate::ffmpeg::probe_capabilities("ffmpeg", "ffprobe");
        if !caps.libx264 || !caps.xfade || !caps.loudnorm || !caps.zoompan {
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
        // title 1.0 + clip 0.5 + still 1.5 - two overlaps (~1.2) ~= 1.8 s.
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
            vdur > 1.4,
            "video stream must span all segments, got {vdur:.2}s"
        );
    }

    #[test]
    fn srt_timestamps() {
        assert_eq!(srt_ts(0.0), "00:00:00,000");
        assert_eq!(srt_ts(61.5), "00:01:01,500");
        assert_eq!(srt_ts(3723.25), "01:02:03,250");
    }
}
