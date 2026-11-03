//! Keepsake 1 — Trip Video (hero film).
//!
//! Pipeline: normalize every plan entry to a uniform 1080p30/s48kHz-stereo
//! segment (Ken Burns stills, scored clip trims, title card, map push-in),
//! then assemble with a stepwise pairwise crossfade chain (never 5+ inputs
//! in one graph; each offset accounts for cumulative overlap), a final
//! loudnorm single pass, 1080p libx264 CRF19 faststart, an EDL JSON, and a
//! sidecar SRT (burned-in captions need libass, absent on this build).

use crate::ffmpeg::{self, path_arg as s, Capabilities};
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
            Transition::CircleOpen => "circleopen",
            Transition::CircleClose => "circleclose",
        }
    }
}

/// Pure xfade timeline math: cut `i` (into segment `i+1`) sits at
/// `sum(durations[0..=i]) - overlap*(i+1)`. Stepwise assembly must use
/// these offsets so overlap never compounds.
pub fn xfade_offsets(durations: &[f64], overlap: f64) -> Vec<f64> {
    let mut offsets = vec![];
    let mut acc = 0.0;
    for (i, d) in durations.iter().enumerate() {
        acc += d;
        if i + 1 < durations.len() {
            offsets.push(acc - overlap * (i as f64 + 1.0));
        }
    }
    offsets
}

/// Assign one transition per cut from story position.
///
/// * `chapter_break[i]` — cut into segment `i` starts a new chapter.
/// * First cut opens with `circleopen`, final cut closes with `circleclose`.
/// * Chapter breaks use `fadeblack`; other place/day changes `smoothleft`.
/// * Up to two `dissolve` cuts become the blur-dissolve signature.
/// * Everything else is the `fade` default.
pub fn assign_transitions(n_segments: usize, chapter_break: &[bool]) -> Vec<Option<Transition>> {
    let mut out: Vec<Option<Transition>> = vec![None; n_segments];
    if n_segments == 0 {
        return out;
    }
    let mut signature_used = 0;
    for (i, slot) in out.iter_mut().enumerate().skip(1) {
        let t = if i == n_segments - 1 && n_segments > 2 {
            Transition::CircleClose
        } else if i == 1 {
            Transition::CircleOpen
        } else if chapter_break.get(i).copied().unwrap_or(false) {
            Transition::FadeBlack
        } else if i % 4 == 3 {
            Transition::SmoothLeft
        } else if signature_used < 2 && i % 3 == 2 {
            signature_used += 1;
            Transition::BlurDissolve
        } else if i % 2 == 0 {
            Transition::Dissolve
        } else {
            Transition::Fade
        };
        *slot = Some(t);
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
    if !caps.drawtext {
        fallbacks.push("drawtext absent: solid title card without text".into());
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
    // Title card first.
    let title_png = work_dir.join("title.png");
    crate::titlecard::save_title_card(&opts.trip_name, 1920, 1080, &title_png)?;
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
        budget -= dur;
        segs.push((r.clone(), seg_path, dur, kind));
    }
    if segs.len() <= 1 {
        return Err(crate::MediaError::Ffmpeg(
            "no usable hero segments (all sources missing)".into(),
        ));
    }

    // 2. Transitions + offsets.
    let mut chapter_break = vec![false; segs.len()];
    for i in 1..segs.len() {
        let (a, b) = (&segs[i - 1].0.entry.asset_id, &segs[i].0.entry.asset_id);
        if a == "title" || a == "map" || b == "map" {
            chapter_break[i] = a == "map" || b == "map";
        } else {
            chapter_break[i] = chapter_of(a) != chapter_of(b);
        }
    }
    let transitions = assign_transitions(segs.len(), &chapter_break);
    let durations: Vec<f64> = segs.iter().map(|s| s.2).collect();
    let offsets = xfade_offsets(&durations, opts.overlap_secs);

    // 3. Stepwise pairwise assembly.
    let mut acc = segs[0].1.clone();
    for i in 1..segs.len() {
        let t = transitions[i].unwrap_or(Transition::Fade);
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
                opts.overlap_secs,
                offset.max(0.0),
                opts.overlap_secs
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

    // 4. Final pass: loudnorm single pass (+ optional ducked music mix).
    let total = durations.iter().sum::<f64>() - opts.overlap_secs * offsets.len() as f64;
    let film_af = if caps.loudnorm {
        ffmpeg::LOUDNORM.to_string()
    } else {
        fallbacks.push("loudnorm absent: flat resample, no loudness pass".into());
        "aresample=48000,aformat=channel_layouts=stereo".to_string()
    };
    let final_args = |music: Option<&Path>, audio_filter: String| -> Vec<String> {
        let mut args: Vec<String> = vec!["-y".into(), "-i".into(), s(&acc)];
        if let Some(m) = music {
            // Multi-input graph: needs -filter_complex + explicit maps.
            args.extend(["-stream_loop".into(), "-1".into(), "-i".into(), s(m)]);
            args.extend([
                "-filter_complex".into(),
                audio_filter,
                "-map".into(),
                "0:v".into(),
                "-map".into(),
                "[aout]".into(),
            ]);
        } else {
            args.extend(["-filter:a".into(), audio_filter]);
        }
        args.extend(ffmpeg::x264_aac("medium", 19));
        args.extend(["-r".into(), "30".into(), "-shortest".into(), s(out_mp4)]);
        args
    };
    let music = opts.music_path.as_deref().filter(|m| {
        let ok = m.exists();
        if !ok {
            fallbacks.push("music file missing; film audio only".into());
        }
        ok
    });
    let first = match music {
        // Music ducks under the (loudnormed when available) film audio.
        Some(m) => final_args(
            Some(m),
            format!("[0:a]{film_af},asplit=2[film][key];[1:a]volume=0.25,aresample=48000,aformat=channel_layouts=stereo[m];[m][key]sidechaincompress=threshold=0.02:ratio=8:attack=200:release=800[duck];[film][duck]amix=inputs=2:duration=first:dropout_transition=0[aout]"),
        ),
        None => final_args(None, film_af.clone()),
    };
    if let Err(e) = ffmpeg::run_ffmpeg(&caps.ffmpeg_bin, &first, out_mp4) {
        if music.is_none() {
            return Err(e);
        }
        fallbacks.push(format!("music mix failed ({e}); retrying film audio only"));
        ffmpeg::run_ffmpeg(&caps.ffmpeg_bin, &final_args(None, film_af), out_mp4)?;
    }

    // 5. Verify + EDL + sidecar SRT.
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
            let mut vf =
                "scale=1920:1080:force_original_aspect_ratio=increase,crop=1920:1080,setsar=1,fps=30"
                    .to_string();
            let probe = ffmpeg::ffprobe_file(&caps.ffprobe_bin, src).ok();
            // Muted/camera-silent clips get a matching silent track so the
            // acrossfade assembly always has `[n:a]` to consume.
            let has_audio = probe.as_ref().is_none_or(|i| i.has_audio);
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

/// Still Ken Burns hold: upscale first (jitter guard), vary move by index.
fn render_hold_segment(
    caps: &Capabilities,
    img: &Path,
    dur: f64,
    idx: usize,
    out: &Path,
) -> Result<(), crate::MediaError> {
    let frames = (dur * 30.0).round() as u32;
    let zoompan = match idx % 4 {
        0 => format!("zoompan=z='1+0.12*on/{frames}':x='iw/2-(iw/zoom/2)':y='ih/2-(ih/zoom/2)':d={frames}:s=1920x1080:fps=30"),
        1 => format!("zoompan=z='1.12-0.12*on/{frames}':x='iw/2-(iw/zoom/2)':y='ih/2-(ih/zoom/2)':d={frames}:s=1920x1080:fps=30"),
        2 => format!("zoompan=z=1.1:x='(iw-iw/zoom)*on/{frames}':y='ih/2-(ih/zoom/2)':d={frames}:s=1920x1080:fps=30"),
        _ => format!("zoompan=z=1.1:x='(iw-iw/zoom)*(1-on/{frames})':y='ih/2-(ih/zoom/2)':d={frames}:s=1920x1080:fps=30"),
    };
    let vf = if caps.zoompan {
        format!("scale=2400:-2,{zoompan},format=yuv420p")
    } else {
        "scale=1920:1080:force_original_aspect_ratio=increase,crop=1920:1080,fps=30,format=yuv420p"
            .to_string()
    };
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
            "-vf".into(),
            vf,
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
        // 6 segments, chapter break into #3.
        let breaks = vec![false, false, false, true, false, false];
        let t = assign_transitions(6, &breaks);
        assert_eq!(t[0], None);
        assert_eq!(t[1], Some(Transition::CircleOpen));
        assert_eq!(t[5], Some(Transition::CircleClose));
        assert_eq!(t[3], Some(Transition::FadeBlack));
        // Blur-dissolve signature capped at 2 and mapped to dissolve.
        let plain = assign_transitions(14, &[false; 14]);
        let sig = plain
            .iter()
            .filter(|t| **t == Some(Transition::BlurDissolve))
            .count();
        assert!(sig <= 2, "signature capped, got {sig}");
        assert_eq!(Transition::BlurDissolve.xfade_name(), "dissolve");
    }

    #[test]
    fn srt_timestamps() {
        assert_eq!(srt_ts(0.0), "00:00:00,000");
        assert_eq!(srt_ts(61.5), "00:01:01,500");
        assert_eq!(srt_ts(3723.25), "01:02:03,250");
    }
}
