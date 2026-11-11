//! Keepsake 2 — Reels Pack (vertical 1080x1920 social clips).
//!
//! Up to 5 reels from distinct moments, 15–30 s each (shorter allowed,
//! never looped to pad). Reframing is center-weighted: landscape sources
//! fill the frame with a center crop, portrait sources fit over a blurred
//! background fill (no per-frame tracking in MVP — that is the later pack).
//! Reel captions ride a sidecar SRT; the place name is burned in via a Typst overlay.

use crate::ffmpeg::{self, path_arg as s, Capabilities};
use crate::render_video::ResolvedEntry;
use std::path::{Path, PathBuf};
use triptape_core::types::Rendition;

/// Non-blocking share disclosure, PLAN.md Sec 14 verbatim.
pub const SHARE_DISCLOSURE: &str = "Reels are made for sharing. Watch them first and post only what you're comfortable sharing, especially videos with kids or other people in them.";

#[derive(Debug, Clone)]
pub struct ReelOptions {
    pub music_path: Option<PathBuf>,
    pub still_secs: f64,
    pub max_secs: f64,
    /// Per-reel slideshow stills (index-aligned with the resolved entries
    /// passed to [`render_reels`]): the selected still plus neighboring
    /// moments from the same chapter. Empty/short lists fall back to a
    /// single-still reel.
    pub slideshows: Vec<Vec<PathBuf>>,
    /// Typst binary for the burned-in place overlay.
    pub typst_bin: String,
    /// Seconds each slideshow photo stays on screen.
    pub slide_secs: f64,
    /// Crossfade length between slideshow photos.
    pub fade_secs: f64,
}

impl Default for ReelOptions {
    fn default() -> Self {
        Self {
            music_path: None,
            still_secs: 8.0,
            max_secs: 30.0,
            slideshows: vec![],
            typst_bin: "typst".to_string(),
            slide_secs: 3.6,
            fade_secs: 0.6,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ReelFile {
    pub index: usize,
    pub asset_id: String,
    pub path: String,
    pub cover: String,
    pub duration_secs: f64,
    pub caption: Option<String>,
}

#[derive(Debug)]
pub struct ReelOutput {
    pub dir: PathBuf,
    pub reels: Vec<ReelFile>,
    pub disclosure_path: PathBuf,
    pub manifest_path: PathBuf,
    pub fallbacks: Vec<String>,
}

/// Filesystem-safe slug from a reel's context text: lowercased, runs of
/// non-alphanumerics collapsed to single dashes, capped at 48 chars.
/// Falls back to the source file stem, then to `reel`.
fn reel_slug(caption: Option<&str>, src: &Path) -> String {
    let raw = caption
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_string)
        .or_else(|| src.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "reel".to_string());
    let lowered: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let joined = lowered
        .split('-')
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let capped: String = joined.chars().take(48).collect();
    match capped.trim_end_matches('-') {
        "" => "reel".to_string(),
        slug => slug.to_string(),
    }
}

/// Burn a place lower-third into `out` for the first ~4 s, in place.
fn burn_place_overlay(
    caps: &Capabilities,
    out: &Path,
    place: &str,
    typst_bin: &str,
) -> Result<(), crate::MediaError> {
    let dir = out.parent().unwrap_or_else(|| Path::new("."));
    let png = dir.join(".place-overlay.png");
    let doc = crate::titlecard::lower_third_doc(place, None);
    let result = crate::titlecard::render_text_png(typst_bin, &doc, &png, 72).and_then(|()| {
        crate::render_video::burn_png_overlay(
            caps,
            out,
            &png,
            "[1:v]scale=972:-2[png];[0:v][png]overlay=54:H-300:format=yuv420:enable='between(t,0.3,4.2)'[v]",
            20,
        )
    });
    let _ = std::fs::remove_file(&png);
    result
}

/// Concatenate two same-format reel parts (1080x1920, 30 fps, AAC).
fn concat_pair(
    caps: &Capabilities,
    first: &Path,
    second: &Path,
    out: &Path,
) -> Result<(), crate::MediaError> {
    let mut args: Vec<String> = vec![
        "-y".into(),
        "-i".into(),
        s(first),
        "-i".into(),
        s(second),
        "-filter_complex".into(),
        "[0:v][0:a][1:v][1:a]concat=n=2:v=1:a=1[v][a]".into(),
        "-map".into(),
        "[v]".into(),
        "-map".into(),
        "[a]".into(),
    ];
    args.extend(quality_tail(out));
    caps.run(&args, out)
}

/// A motion highlight shorter than this is extended with its chapter
/// slideshow — a 2 s Live Photo alone is not a reel.
const SHORT_MOTION_SECS: f64 = 8.0;

/// Extend a just-rendered short motion reel at `out` with the chapter
/// slideshow; returns the added duration (0 when not extended).
fn extend_with_slideshow(
    caps: &Capabilities,
    out: &Path,
    motion_len: f64,
    slides: &[PathBuf],
    opts: &ReelOptions,
    fallbacks: &mut Vec<String>,
) -> Result<f64, crate::MediaError> {
    if motion_len >= SHORT_MOTION_SECS || slides.is_empty() {
        return Ok(0.0);
    }
    let stem = out
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "reel".to_string());
    let dir = out.parent().unwrap_or_else(|| Path::new("."));
    let clip_part = dir.join(format!(".{stem}.clip.mp4"));
    let show_part = dir.join(format!(".{stem}.slides.mp4"));
    let joined = dir.join(format!(".{stem}.joined.mp4"));
    let result: Result<f64, crate::MediaError> = (|| {
        std::fs::rename(out, &clip_part).map_err(|e| crate::MediaError::Io(e.to_string()))?;
        render_slideshow_reel(caps, slides, opts, &show_part, fallbacks)?;
        concat_pair(caps, &clip_part, &show_part, &joined)?;
        std::fs::rename(&joined, out).map_err(|e| crate::MediaError::Io(e.to_string()))?;
        Ok(slideshow_duration(
            slides.len(),
            opts.slide_secs,
            opts.fade_secs,
            caps.xfade,
        ))
    })();
    let added = result.unwrap_or_else(|e| {
        // Extension is an enhancement: keep the motion-only reel.
        if !out.exists() && clip_part.exists() {
            let _ = std::fs::rename(&clip_part, out);
        }
        fallbacks.push(format!(
            "reel {}: slideshow extension failed ({e}); motion only",
            out.file_name().unwrap_or_default().to_string_lossy()
        ));
        0.0
    });
    let _ = std::fs::remove_file(&clip_part);
    let _ = std::fs::remove_file(&show_part);
    let _ = std::fs::remove_file(&joined);
    Ok(added)
}

/// Actual duration of a just-encoded reel part, so a source shorter than its
/// requested trim is still extended; `planned` when probing fails.
fn encoded_len(caps: &Capabilities, out: &Path, planned: f64) -> f64 {
    ffmpeg::ffprobe_file(&caps.ffprobe_bin, out)
        .ok()
        .map(|p| p.duration_secs)
        .filter(|d| *d > 0.0)
        .unwrap_or(planned)
}

/// Encode one reel to `out`; returns its planned duration in seconds.
fn render_one_reel(
    caps: &Capabilities,
    r: &ResolvedEntry,
    src: &Path,
    out: &Path,
    opts: &ReelOptions,
    slides: &[PathBuf],
    fallbacks: &mut Vec<String>,
) -> Result<f64, crate::MediaError> {
    let dur = match &r.entry.rendition {
        Rendition::Still if r.motion_path.is_none() => {
            if slides.len() >= 2 {
                // A still highlight becomes a slideshow of its chapter
                // neighbors, never one squashed photo held for seconds.
                render_slideshow_reel(caps, slides, opts, out, fallbacks)?;
                slideshow_duration(slides.len(), opts.slide_secs, opts.fade_secs, caps.xfade)
            } else {
                render_still_reel(caps, &r.still_path, opts.still_secs, out)?;
                opts.still_secs
            }
        }
        Rendition::Motion {
            trim_start,
            trim_end,
        }
        | Rendition::Clip {
            trim_start,
            trim_end,
        } => {
            let len = (trim_end - trim_start).clamp(0.5, opts.max_secs);
            render_clip_reel(
                caps,
                src,
                *trim_start,
                len,
                out,
                opts.music_path.as_deref(),
                fallbacks,
            )?;
            let len = encoded_len(caps, out, len);
            len + extend_with_slideshow(caps, out, len, slides, opts, fallbacks)?
        }
        // Still rendition but a motion file exists (Live fallback): use it.
        _ => {
            let info = ffmpeg::ffprobe_file(&caps.ffprobe_bin, src).ok();
            let len = info
                .and_then(|p| {
                    if p.duration_secs > 0.5 {
                        Some(p.duration_secs)
                    } else {
                        None
                    }
                })
                .unwrap_or(opts.still_secs)
                .clamp(0.5, opts.max_secs);
            render_clip_reel(
                caps,
                src,
                0.0,
                len,
                out,
                opts.music_path.as_deref(),
                fallbacks,
            )?;
            let len = encoded_len(caps, out, len);
            len + extend_with_slideshow(caps, out, len, slides, opts, fallbacks)?
        }
    };
    Ok(dur)
}

/// Render the reels pack (at most 5, in plan order = story order).
pub fn render_reels(
    caps: &Capabilities,
    resolved: &[ResolvedEntry],
    out_dir: &Path,
    opts: &ReelOptions,
) -> Result<ReelOutput, crate::MediaError> {
    if !caps.libx264 {
        return Err(crate::MediaError::Ffmpeg(
            "libx264 missing; no reel encode".into(),
        ));
    }
    std::fs::create_dir_all(out_dir).map_err(|e| crate::MediaError::Io(e.to_string()))?;
    let mut fallbacks: Vec<String> = vec![];
    if !caps.gblur {
        fallbacks.push("gblur absent: portrait reels use plain dark bars".into());
    }
    if !caps.perspective {
        fallbacks.push("perspective absent: slideshow stills hold without motion".into());
    }

    let mut reels: Vec<ReelFile> = vec![];
    let total = resolved.len().min(5);
    for (i, r) in resolved.iter().take(5).enumerate() {
        caps.progress.check_cancelled()?;
        caps.progress.counted("reels", "reel", i, total);
        let src = r.motion_path.as_ref().unwrap_or(&r.still_path);
        if !src.exists() {
            fallbacks.push(format!(
                "reel {} missing on disk; skipped",
                r.entry.asset_id
            ));
            continue;
        }
        // Human-readable name from the reel's own context (its dateline
        // caption, else the source file name) — never the internal asset id.
        let fname = format!("{:02}_{}.mp4", i + 1, reel_slug(r.caption.as_deref(), src));
        let out = out_dir.join(&fname);
        // One bad source (e.g. a Dolby Vision HEIC this FFmpeg cannot decode)
        // skips that reel, never the whole pack.
        let slides: &[PathBuf] = opts.slideshows.get(i).map_or(&[], Vec::as_slice);
        let dur = match render_one_reel(caps, r, src, &out, opts, slides, &mut fallbacks) {
            Ok(d) => d,
            // `caps.run` already discarded its `.partial`; `out` is either a
            // previous export's reel or a fully encoded new one — keep it.
            Err(crate::MediaError::Cancelled) => return Err(crate::MediaError::Cancelled),
            Err(e) => {
                fallbacks.push(format!("reel {} failed ({e}); skipped", r.entry.asset_id));
                let _ = std::fs::remove_file(&out);
                continue;
            }
        };
        let probed = match ffmpeg::verify_video(&caps.ffprobe_bin, &out, 1080, 1920) {
            Ok(p) => p,
            Err(e) => {
                fallbacks.push(format!(
                    "reel {} verify failed ({e}); skipped",
                    r.entry.asset_id
                ));
                let _ = std::fs::remove_file(&out);
                continue;
            }
        };
        if (probed.duration_secs - dur).abs() > 1.0 + 1.0 / 30.0 {
            // 1 s + 1 frame of container rounding slack; informational only.
            fallbacks.push(format!(
                "reel {} duration {:.2}s vs {:.2}s planned",
                r.entry.asset_id, probed.duration_secs, dur
            ));
        }
        // Cover frame ~1 s in (no earlier than 0.2 s for very short reels).
        let cover = out.with_extension("jpg");
        let at = probed.duration_secs.clamp(0.2, 1.0);
        let cover_args: Vec<String> = vec![
            "-y".into(),
            "-ss".into(),
            format!("{at:.2}"),
            "-i".into(),
            s(&out),
            "-frames:v".into(),
            "1".into(),
            "-q:v".into(),
            "3".into(),
            s(&cover),
        ];
        caps.run(&cover_args, &cover)?;
        if let Some(c) = &r.caption {
            if !c.trim().is_empty() {
                std::fs::write(
                    out.with_extension("srt"),
                    format!("1\n00:00:00,300 --> 00:00:03,800\n{}\n", c.trim()),
                )
                .map_err(|e| crate::MediaError::Io(e.to_string()))?;
            }
        }
        // Mention where we are, in the video itself: burned-in place
        // overlay over the first seconds (the SRT only helps players
        // that show subtitles).
        if let Some(place) = r
            .caption
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
        {
            if let Err(e) = burn_place_overlay(caps, &out, place, &opts.typst_bin) {
                fallbacks.push(format!(
                    "reel {} place overlay skipped ({e})",
                    r.entry.asset_id
                ));
            }
        }
        reels.push(ReelFile {
            index: i + 1,
            asset_id: r.entry.asset_id.clone(),
            path: fname,
            cover: cover.file_name().unwrap().to_string_lossy().into_owned(),
            duration_secs: (probed.duration_secs * 100.0).round() / 100.0,
            caption: r.caption.clone(),
        });
    }

    caps.progress.check_cancelled()?;
    caps.progress.counted("reels", "reel", total, total);

    // Per-reel failures are tolerated, but a pack with zero reels from a
    // nonempty selection is a render failure, not a success.
    if reels.is_empty() && !resolved.is_empty() {
        return Err(crate::MediaError::Ffmpeg(format!(
            "no reels produced: {}",
            fallbacks.join("; ")
        )));
    }

    // Non-blocking disclosure on the pack (PLAN.md Sec 14).
    let disclosure_path = out_dir.join("SHARE_DISCLOSURE.txt");
    std::fs::write(&disclosure_path, format!("{SHARE_DISCLOSURE}\n"))
        .map_err(|e| crate::MediaError::Io(e.to_string()))?;
    let manifest_path = out_dir.join("reels.json");
    std::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "version": 1,
            // File names, not paths: the pack may be moved after rendering.
            "reels": reels.iter().map(|r| {
                let mut r = r.clone();
                let file_name = |p: &str| p.rsplit(['/', '\\']).next().unwrap_or_default().to_string();
                r.path = file_name(&r.path);
                r.cover = file_name(&r.cover);
                r
            }).collect::<Vec<_>>(),
            "share_disclosure": SHARE_DISCLOSURE,
            "fallbacks": fallbacks,
        }))?,
    )
    .map_err(|e| crate::MediaError::Io(e.to_string()))?;

    Ok(ReelOutput {
        dir: out_dir.to_path_buf(),
        reels,
        disclosure_path,
        manifest_path,
        fallbacks,
    })
}

/// Landscape fills the frame (center crop); portrait fits over blurred fill.
fn vertical_vf(caps: &Capabilities, src_w: u32, src_h: u32) -> String {
    if src_w > src_h {
        // Landscape: full-bleed center crop keeps the middle of the moment.
        return "scale=1080:1920:force_original_aspect_ratio=increase,crop=1080:1920,setsar=1,fps=30,format=yuv420p".to_string();
    }
    let bg = if caps.gblur {
        "gblur=sigma=40"
    } else {
        "setsar=1"
    };
    format!("split[a][b];[a]scale=1080:1920,{bg}[bg];[b]scale=1080:1920:force_original_aspect_ratio=decrease[fg];[bg][fg]overlay=(W-w)/2:(H-h)/2,setsar=1,fps=30,format=yuv420p")
}

fn quality_tail(out: &Path) -> Vec<String> {
    let mut args = ffmpeg::x264_aac("veryfast", 20);
    args.extend(["-r".into(), "30".into(), "-shortest".into(), s(out)]);
    args
}

fn render_clip_reel(
    caps: &Capabilities,
    src: &Path,
    start: f64,
    len: f64,
    out: &Path,
    music: Option<&Path>,
    fallbacks: &mut Vec<String>,
) -> Result<(), crate::MediaError> {
    let probe = ffmpeg::ffprobe_file(&caps.ffprobe_bin, src).ok();
    let (w, h) = probe
        .as_ref()
        .map(|p| (p.width, p.height))
        .unwrap_or((1920, 1080));
    let has_audio = probe.as_ref().is_none_or(|p| p.has_audio);
    let vf = vertical_vf(caps, w, h);
    let mut src_af = "aresample=48000,aformat=channel_layouts=stereo".to_string();
    if caps.loudnorm {
        src_af.push(',');
        src_af.push_str(ffmpeg::LOUDNORM);
    } else {
        fallbacks.push("loudnorm absent: flat resample on reels".into());
    }
    let mut args: Vec<String> = vec![
        "-y".into(),
        "-ss".into(),
        format!("{start:.3}"),
        "-t".into(),
        format!("{len:.3}"),
        "-i".into(),
        s(src),
    ];
    // Silent sources get a synthesized silent track (verify requires audio).
    // Source audio: first usable stream (never iPhone `apple_apac`).
    let (audio_in, music_in) = if has_audio {
        (ffmpeg::audio_spec(probe.as_ref()), 1)
    } else {
        fallbacks.push(format!(
            "{} has no audio; silent track added",
            src.file_name().unwrap_or_default().to_string_lossy()
        ));
        args.extend(crate::render_video::silent_input(len));
        ("1:a".to_string(), 2)
    };
    if let Some(m) = music {
        if m.exists() {
            // One graph: vertical video + ducked music mix.
            args.extend(["-stream_loop".into(), "-1".into(), "-i".into(), s(m)]);
            args.extend([
                "-filter_complex".into(),
                format!(
                    "[0:v]{vf}[v];[{audio_in}]{src_af}[srca];[{music_in}:a]volume=0.2,aresample=48000,aformat=channel_layouts=stereo[mus];[srca][mus]amix=inputs=2:duration=first:dropout_transition=0[aout]"
                ),
                "-map".into(),
                "[v]".into(),
                "-map".into(),
                "[aout]".into(),
            ]);
            args.extend(quality_tail(out));
            return caps.run(&args, out);
        }
        fallbacks.push("reel music missing; source audio only".into());
    }
    args.extend([
        "-filter_complex".into(),
        format!("[0:v]{vf}[v];[{audio_in}]{src_af}[aout]"),
        "-map".into(),
        "[v]".into(),
        "-map".into(),
        "[aout]".into(),
    ]);
    args.extend(quality_tail(out));
    caps.run(&args, out)
}

/// Planned slideshow duration: crossfades overlap adjacent slides.
fn slideshow_duration(n: usize, slide_secs: f64, fade_secs: f64, xfade: bool) -> f64 {
    if n == 0 {
        0.0
    } else if xfade {
        n as f64 * slide_secs - (n - 1) as f64 * fade_secs
    } else {
        n as f64 * slide_secs
    }
}

/// One slideshow frame: full photo fitted over its own blurred fill, then
/// a gentle glide applied to the finished 9:16 composite. Moving the
/// composite (never the raw photo) keeps the aspect honest — zoompan on a
/// landscape source squashes it into the portrait frame, and its
/// integer-step re-cropping makes stills jitter.
fn slideshow_frame_vf(caps: &Capabilities, i: usize, slide_secs: f64) -> String {
    let base = ffmpeg::fit_over_blur(caps, &format!("[{i}:v]"), 1620, 2880, &i.to_string(), 40);
    let frames = (slide_secs * 30.0).round().max(1.0) as u32;
    format!(
        "{base},{},{},format=yuv420p[v{i}]",
        ffmpeg::hold_still(frames),
        ffmpeg::glide(caps, frames, 1080, 1920, i)
    )
}

const SLIDE_TRANSITIONS: [&str; 4] = ["fade", "dissolve", "slideleft", "slideright"];

/// Full slideshow filter graph: framed slides joined by rotating
/// crossfades (concat when xfade is unavailable), plus the audio lane.
fn slideshow_graph(
    n: usize,
    caps: &Capabilities,
    slide_secs: f64,
    fade_secs: f64,
    total: f64,
    music: bool,
) -> String {
    let mut parts: Vec<String> = (0..n)
        .map(|i| slideshow_frame_vf(caps, i, slide_secs))
        .collect();
    if n == 1 {
        parts.push("[v0]null[vout]".to_string());
    } else if caps.xfade {
        let mut prev = "v0".to_string();
        for k in 1..n {
            let out_label = if k == n - 1 {
                "vout".to_string()
            } else {
                format!("x{k}")
            };
            let offset = k as f64 * slide_secs - k as f64 * fade_secs;
            parts.push(format!(
                "[{prev}][v{k}]xfade=transition={}:duration={fade_secs:.3}:offset={offset:.3}[{out_label}]",
                SLIDE_TRANSITIONS[(k - 1) % SLIDE_TRANSITIONS.len()]
            ));
            prev = out_label;
        }
    } else {
        let inputs: String = (0..n).map(|i| format!("[v{i}]")).collect();
        parts.push(format!("{inputs}concat=n={n}:v=1:a=0[vout]"));
    }
    if music {
        parts.push(format!(
            "[{n}:a]aresample=48000,volume=0.85,atrim=0:{total:.3},afade=t=out:st={:.3}:d=1[aout]",
            (total - 1.0).max(0.0)
        ));
    } else {
        parts.push(format!("[{n}:a]atrim=0:{total:.3}[aout]"));
    }
    parts.join(";")
}

fn render_slideshow_reel(
    caps: &Capabilities,
    slides: &[PathBuf],
    opts: &ReelOptions,
    out: &Path,
    fallbacks: &mut Vec<String>,
) -> Result<(), crate::MediaError> {
    // Decode first: raw HEIC inputs fail or mis-rotate in filter graphs.
    let mut decoded = vec![];
    for s in slides {
        decoded.push(ffmpeg::decode_still_to_png(&caps.ffmpeg_bin, s)?);
    }
    let n = decoded.len();
    let music = opts.music_path.as_deref().filter(|m| m.exists());
    if opts.music_path.is_some() && music.is_none() {
        fallbacks.push("reel music missing; slideshow silent".into());
    }
    let total = slideshow_duration(n, opts.slide_secs, opts.fade_secs, caps.xfade);
    let mut args: Vec<String> = vec!["-y".into()];
    for d in &decoded {
        args.extend(["-i".into(), s(&d.path)]);
    }
    if let Some(m) = music {
        args.extend(["-stream_loop".into(), "-1".into(), "-i".into(), s(m)]);
    } else {
        args.extend([
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            "anullsrc=r=48000:cl=stereo".into(),
        ]);
    }
    args.extend([
        "-filter_complex".into(),
        slideshow_graph(
            n,
            caps,
            opts.slide_secs,
            opts.fade_secs,
            total,
            music.is_some(),
        ),
        "-map".into(),
        "[vout]".into(),
        "-map".into(),
        "[aout]".into(),
    ]);
    args.extend(quality_tail(out));
    caps.run(&args, out)
}

fn render_still_reel(
    caps: &Capabilities,
    img: &Path,
    dur: f64,
    out: &Path,
) -> Result<(), crate::MediaError> {
    // Decode first: looping a raw HEIC fails on tmap/Dolby Vision items.
    let png = ffmpeg::decode_still_to_png(&caps.ffmpeg_bin, img)?;
    render_still_reel_png(caps, &png.path, dur, out)
}

fn render_still_reel_png(
    caps: &Capabilities,
    img: &Path,
    dur: f64,
    out: &Path,
) -> Result<(), crate::MediaError> {
    let frames = (dur * 30.0).round() as u32;
    // Same framing as one slideshow slide (input 0, output `[v0]`): full
    // photo over its blurred fill, glide on the finished 9:16 composite.
    let graph = slideshow_frame_vf(caps, 0, dur);
    let mut args: Vec<String> = vec!["-y".into()];
    args.extend([
        "-i".into(),
        s(img),
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        format!("anullsrc=r=48000:cl=stereo:d={dur}"),
        "-filter_complex".into(),
        graph,
        "-map".into(),
        "[v0]".into(),
        "-map".into(),
        "1:a".into(),
        "-frames:v".into(),
        frames.to_string(),
    ]);
    args.extend(quality_tail(out));
    caps.run(&args, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_still_fails_closed_on_missing_file() {
        let r = crate::ffmpeg::decode_still_to_png("ffmpeg", Path::new("/nonexistent/x.HEIC"));
        assert!(r.is_err(), "missing still must fail, not fabricate a frame");
    }

    #[test]
    fn slideshow_graph_frames_full_photo_and_crossfades() {
        let caps = Capabilities {
            gblur: true,
            xfade: true,
            perspective: true,
            ..Default::default()
        };
        let g = slideshow_graph(3, &caps, 3.6, 0.6, 9.6, false);
        // Every slide: full photo over blurred fill (overlay), and the
        // glide moves the composite AFTER the overlay — never the raw
        // landscape photo (that squashes it into 9:16).
        for i in 0..3 {
            assert!(g.contains(&format!("[bg{i}][fg{i}]overlay")), "{g}");
        }
        let overlay_at = g.find("overlay").unwrap();
        let zoom_at = g.find("perspective=").unwrap();
        assert!(overlay_at < zoom_at, "glide applies to composite: {g}");
        assert!(!g.contains("zoompan"), "no zoompan jitter: {g}");
        assert_eq!(g.matches("xfade=transition=").count(), 2, "{g}");
        assert!(g.contains("transition=fade") && g.contains("transition=dissolve"));
        // Duration math: crossfades overlap slides.
        assert!((slideshow_duration(3, 3.6, 0.6, true) - 9.6).abs() < 1e-9);
        assert!((slideshow_duration(3, 3.6, 0.6, false) - 10.8).abs() < 1e-9);
        assert_eq!(slideshow_duration(0, 3.6, 0.6, true), 0.0);
    }

    #[test]
    fn reel_slug_names_context_not_asset_id() {
        let src = Path::new("/trip/IMG_0131.HEIC");
        assert_eq!(
            reel_slug(Some("Day 4 · Sun, Oct 5 · 4:32 PM"), src),
            "day-4-sun-oct-5-4-32-pm"
        );
        // No caption: fall back to the source file name.
        assert_eq!(reel_slug(None, src), "img-0131");
        assert_eq!(reel_slug(Some("  "), src), "img-0131");
        // Never empty, filesystem-safe, capped.
        let long = reel_slug(Some(&"word ".repeat(30)), src);
        assert!(long.len() <= 48 && !long.ends_with('-') && !long.is_empty());
    }

    #[test]
    fn disclosure_is_verbatim_sec14() {
        assert_eq!(
            SHARE_DISCLOSURE,
            "Reels are made for sharing. Watch them first and post only what you're comfortable sharing, especially videos with kids or other people in them."
        );
    }

    /// The overlay rewrites the reel in place: same streams and length, the
    /// text visible early on, and no temp files left beside it.
    #[test]
    fn place_overlay_burns_in_place() {
        let caps = crate::ffmpeg::probe_capabilities("ffmpeg", "ffprobe");
        if !caps.libx264 || !crate::titlecard::typst_available("typst") {
            eprintln!("ffmpeg/typst unavailable; skipping reel overlay");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let reel = temp.path().join("reel.mp4");
        let made = std::process::Command::new(&caps.ffmpeg_bin)
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=1080x1920:r=30:d=2",
            ])
            .args(["-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo", "-t", "2"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"])
            .arg(&reel)
            .output()
            .unwrap();
        assert!(made.status.success());
        let bright_bytes = |path: &Path| {
            let out = std::process::Command::new(&caps.ffmpeg_bin)
                .args(["-v", "error", "-ss", "1", "-i"])
                .arg(path)
                .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "gray", "-"])
                .output()
                .unwrap();
            out.stdout.iter().filter(|&&b| b > 128).count()
        };
        assert_eq!(bright_bytes(&reel), 0);

        burn_place_overlay(&caps, &reel, "Kyoto, Japan", "typst").unwrap();

        assert!(bright_bytes(&reel) > 0, "place text visible at 1 s");
        let probe = ffmpeg::ffprobe_file(&caps.ffprobe_bin, &reel).unwrap();
        assert!((probe.duration_secs - 2.0).abs() < 0.2, "{probe:?}");
        let files = std::fs::read_dir(temp.path()).unwrap().count();
        assert_eq!(files, 1, "no overlay or partial files left behind");
    }

    #[test]
    fn vertical_filter_shapes() {
        let caps = Capabilities {
            gblur: true,
            ..Default::default()
        };
        let land = vertical_vf(&caps, 1920, 1080);
        assert!(land.contains("crop=1080:1920"), "landscape fills frame");
        let port = vertical_vf(&caps, 1080, 1920);
        assert!(
            port.contains("gblur") && port.contains("overlay"),
            "portrait blur-fill"
        );
        let no_blur = vertical_vf(&Capabilities::default(), 1080, 1920);
        assert!(!no_blur.contains("gblur") && no_blur.contains("overlay"));
    }
}
