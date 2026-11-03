//! Keepsake 2 — Reels Pack (vertical 1080x1920 social clips).
//!
//! Up to 5 reels from distinct moments, 15–30 s each (shorter allowed,
//! never looped to pad). Reframing is center-weighted: landscape sources
//! fill the frame with a center crop, portrait sources fit over a blurred
//! background fill (no per-frame tracking in MVP — that is the later pack).
//! Captions ride the sidecar SRT per reel when libass is absent.

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
}

impl Default for ReelOptions {
    fn default() -> Self {
        Self {
            music_path: None,
            still_secs: 8.0,
            max_secs: 30.0,
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

    let mut reels: Vec<ReelFile> = vec![];
    for (i, r) in resolved.iter().take(5).enumerate() {
        let src = r.motion_path.as_ref().unwrap_or(&r.still_path);
        if !src.exists() {
            fallbacks.push(format!(
                "reel {} missing on disk; skipped",
                r.entry.asset_id
            ));
            continue;
        }
        let fname = format!("{:02}_{}.mp4", i + 1, r.entry.asset_id);
        let out = out_dir.join(&fname);
        let dur = match &r.entry.rendition {
            Rendition::Still if r.motion_path.is_none() => {
                render_still_reel(caps, &r.still_path, opts.still_secs, &out)?;
                opts.still_secs
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
                    &out,
                    opts.music_path.as_deref(),
                    &mut fallbacks,
                )?;
                len
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
                    &out,
                    opts.music_path.as_deref(),
                    &mut fallbacks,
                )?;
                len
            }
        };
        let probed = ffmpeg::verify_video(&caps.ffprobe_bin, &out, 1080, 1920)?;
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
        ffmpeg::run_ffmpeg(&caps.ffmpeg_bin, &cover_args, &cover)?;
        if let Some(c) = &r.caption {
            if !c.trim().is_empty() {
                std::fs::write(
                    out.with_extension("srt"),
                    format!("1\n00:00:00,300 --> 00:00:03,800\n{}\n", c.trim()),
                )
                .map_err(|e| crate::MediaError::Io(e.to_string()))?;
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

    // Non-blocking disclosure on the pack (PLAN.md Sec 14).
    let disclosure_path = out_dir.join("SHARE_DISCLOSURE.txt");
    std::fs::write(&disclosure_path, format!("{SHARE_DISCLOSURE}\n"))
        .map_err(|e| crate::MediaError::Io(e.to_string()))?;
    let manifest_path = out_dir.join("reels.json");
    std::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "version": 1,
            "reels": reels,
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
    let (audio_in, music_in) = if has_audio {
        (0, 1)
    } else {
        fallbacks.push(format!(
            "{} has no audio; silent track added",
            src.file_name().unwrap_or_default().to_string_lossy()
        ));
        args.extend(crate::render_video::silent_input(len));
        (1, 2)
    };
    if let Some(m) = music {
        if m.exists() {
            // One graph: vertical video + ducked music mix.
            args.extend(["-stream_loop".into(), "-1".into(), "-i".into(), s(m)]);
            args.extend([
                "-filter_complex".into(),
                format!(
                    "[0:v]{vf}[v];[{audio_in}:a]{src_af}[srca];[{music_in}:a]volume=0.2,aresample=48000,aformat=channel_layouts=stereo[mus];[srca][mus]amix=inputs=2:duration=first:dropout_transition=0[aout]"
                ),
                "-map".into(),
                "[v]".into(),
                "-map".into(),
                "[aout]".into(),
            ]);
            args.extend(quality_tail(out));
            return ffmpeg::run_ffmpeg(&caps.ffmpeg_bin, &args, out);
        }
        fallbacks.push("reel music missing; source audio only".into());
    }
    args.extend([
        "-filter_complex".into(),
        format!("[0:v]{vf}[v];[{audio_in}:a]{src_af}[aout]"),
        "-map".into(),
        "[v]".into(),
        "-map".into(),
        "[aout]".into(),
    ]);
    args.extend(quality_tail(out));
    ffmpeg::run_ffmpeg(&caps.ffmpeg_bin, &args, out)
}

fn render_still_reel(
    caps: &Capabilities,
    img: &Path,
    dur: f64,
    out: &Path,
) -> Result<(), crate::MediaError> {
    let frames = (dur * 30.0).round() as u32;
    // Hook-first: gentle push-in so the first frame already fills 9:16.
    let vf = if caps.zoompan {
        format!("scale=2160:-2,zoompan=z='1+0.10*on/{frames}':x='iw/2-(iw/zoom/2)':y='ih/2-(ih/zoom/2)':d={frames}:s=1080x1920:fps=30,format=yuv420p")
    } else {
        "scale=1080:1920:force_original_aspect_ratio=increase,crop=1080:1920,fps=30,format=yuv420p"
            .to_string()
    };
    let mut args: Vec<String> = vec![
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
    ];
    args.extend(quality_tail(out));
    ffmpeg::run_ffmpeg(&caps.ffmpeg_bin, &args, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disclosure_is_verbatim_sec14() {
        assert_eq!(
            SHARE_DISCLOSURE,
            "Reels are made for sharing. Watch them first and post only what you're comfortable sharing, especially videos with kids or other people in them."
        );
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
