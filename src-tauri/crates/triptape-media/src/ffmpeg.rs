//! FFmpeg capability probe, process runner, and ffprobe verifier.
//!
//! The bundled/static FFmpeg varies per machine (system FFmpeg 9.0.2 here:
//! `libx264`, `xfade`, `zoompan`, `tonemap`, `loudnorm` present;
//! `zscale`/`drawtext`/`vidstab`/`libass` absent). Every renderer probes
//! once at startup and picks fallback filters accordingly, so exports
//! degrade instead of failing.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Single-pass EBU R128 loudness target shared by every renderer.
pub const LOUDNORM: &str = "loudnorm=I=-16:TP=-1.5:LRA=11";

/// Path as an owned FFmpeg CLI argument.
pub fn path_arg(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Shared libx264 + AAC 48 kHz stereo encode args (yuv420p, faststart).
/// Callers append frame-rate/`-shortest` flags and the output path.
pub fn x264_aac(preset: &str, crf: u32) -> Vec<String> {
    [
        "-c:v",
        "libx264",
        "-preset",
        preset,
        "-crf",
        &crf.to_string(),
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
        "-ar",
        "48000",
        "-ac",
        "2",
        "-movflags",
        "+faststart",
    ]
    .map(String::from)
    .to_vec()
}

/// Encoder/filter capabilities relevant to the three renderers.
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    pub ffmpeg_bin: String,
    pub ffprobe_bin: String,
    pub libx264: bool,
    pub aac: bool,
    pub xfade: bool,
    pub zoompan: bool,
    pub tonemap: bool,
    pub zscale: bool,
    pub loudnorm: bool,
    pub scale: bool,
    pub crop: bool,
    pub gblur: bool,
    pub drawtext: bool,
    pub libass: bool,
    pub vidstabdetect: bool,
    pub vidstabtransform: bool,
    pub subtitles: bool,
}

impl Capabilities {
    /// Captions can only be burned in when libass (or the subtitles filter
    /// chain) exists; otherwise renderers emit a sidecar SRT plus a note.
    pub fn can_burn_captions(&self) -> bool {
        self.libass && self.subtitles
    }

    /// Full HDR->SDR chain needs zscale; otherwise tonemap alone is the
    /// fallback (see [`tonemap_chain`]).
    pub fn full_tonemap(&self) -> bool {
        self.tonemap && self.zscale
    }
}

fn flag_present(output: &str, name: &str) -> bool {
    // Match whole filter/encoder names: filter lines look like
    // " TSC xfade  VV->V  Cross fade ..." and encoder lines like
    // " V..... libx264  libx264 H.264 ...".
    output.lines().any(|l| {
        let t = l.trim_start();
        // Flag columns are uppercase letters/dots/spaces (" .S ", " V....D ");
        // names themselves start lowercase, so stop there.
        let rest = t
            .trim_start_matches(|c: char| c == '.' || c == ' ' || c.is_ascii_uppercase())
            .trim_start();
        rest.split_whitespace().next() == Some(name)
    })
}

/// Probe `ffmpeg_bin` (`-encoders`, `-filters`) for the capabilities above.
/// Missing binaries or unparseable output yield all-false capabilities
/// (callers then fail with a clear error instead of misprobing).
pub fn probe_capabilities(ffmpeg_bin: &str, ffprobe_bin: &str) -> Capabilities {
    let encoders = run_capture(ffmpeg_bin, &["-hide_banner", "-encoders"]).unwrap_or_default();
    let filters = run_capture(ffmpeg_bin, &["-hide_banner", "-filters"]).unwrap_or_default();
    let filter = |name| flag_present(&filters, name);
    Capabilities {
        ffmpeg_bin: ffmpeg_bin.to_string(),
        ffprobe_bin: ffprobe_bin.to_string(),
        libx264: flag_present(&encoders, "libx264"),
        aac: flag_present(&encoders, "aac"),
        xfade: filter("xfade"),
        zoompan: filter("zoompan"),
        tonemap: filter("tonemap"),
        zscale: filter("zscale"),
        loudnorm: filter("loudnorm"),
        scale: filter("scale"),
        crop: filter("crop"),
        gblur: filter("gblur"),
        drawtext: filter("drawtext"),
        libass: filter("libass"),
        vidstabdetect: filter("vidstabdetect"),
        vidstabtransform: filter("vidstabtransform"),
        subtitles: filter("subtitles"),
    }
}

fn run_capture(bin: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(bin).args(args).output().ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        None
    }
}

/// HDR (HLG/PQ) to SDR BT.709 video filter chain, or `None` for SDR.
///
/// * Full chain when `zscale` exists (verified G1 path).
/// * Fallback `tonemap` alone when `zscale` is absent.
/// * Assumes the caller appends `,format=yuv420p` (or bakes it in).
pub fn tonemap_chain(caps: &Capabilities, color_transfer: &str) -> Option<String> {
    match color_transfer {
        t if t.contains("smpte2084") || t.contains("arib-std-b67") || t.contains("smpte-2084") => {
            if !caps.tonemap {
                return None;
            }
            if caps.zscale {
                Some(
                    "zscale=t=linear,tonemap=hable:desat=0,zscale=t=bt709:m=bt709:p=bt709"
                        .to_string(),
                )
            } else {
                // No zscale on this build: tonemap alone (less precise
                // primaries, but avoids washed-out HDR).
                Some("tonemap=hable:desat=0".to_string())
            }
        }
        _ => None,
    }
}

/// A still decoded to PNG inside a private, uniquely named temp directory.
/// The directory (and PNG) is removed when this value is dropped.
pub struct DecodedStill {
    _dir: tempfile::TempDir,
    pub path: PathBuf,
}

/// Decode a still to PNG in a private temp dir. Tries FFmpeg first; some
/// HEIC files (Dolby Vision `tmap` items) defeat FFmpeg entirely, so on
/// macOS fall back to `sips` (ImageIO). HEIC/HEIF on macOS go to `sips`
/// first: ImageIO always composes the full tile grid, while older FFmpeg
/// builds return a single tile.
pub fn decode_still_to_png(
    ffmpeg_bin: &str,
    src: &Path,
) -> Result<DecodedStill, super::MediaError> {
    let dir = tempfile::Builder::new()
        .prefix("triptape-still-")
        .tempdir()
        .map_err(|e| super::MediaError::Io(e.to_string()))?;
    let out = dir.path().join("still.png");
    let ok = |st: std::io::Result<std::process::ExitStatus>| {
        if matches!(st, Ok(s) if s.success()) && out.is_file() {
            true
        } else {
            let _ = std::fs::remove_file(&out);
            false
        }
    };
    let try_ffmpeg = || {
        ok(Command::new(ffmpeg_bin)
            .args(["-nostdin", "-i"])
            .arg(src)
            .args(["-frames:v", "1"])
            .arg(&out)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status())
    };
    #[cfg(target_os = "macos")]
    let try_sips = || {
        ok(Command::new("sips")
            .args(["-s", "format", "png"])
            .arg(src)
            .arg("--out")
            .arg(&out)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status())
    };
    #[cfg(target_os = "macos")]
    let decoded = {
        let heif = src
            .extension()
            .map(|e| matches!(e.to_ascii_lowercase().to_str(), Some("heic" | "heif")))
            .unwrap_or(false);
        // Order matters (side effects): the first decoder to succeed wins.
        let first_sips = heif && try_sips();
        first_sips || try_ffmpeg() || (!heif && try_sips())
    };
    #[cfg(not(target_os = "macos"))]
    let decoded = try_ffmpeg();
    if decoded {
        return Ok(DecodedStill {
            _dir: dir,
            path: out,
        });
    }
    Err(super::MediaError::Ffmpeg(format!(
        "cannot decode still {}",
        src.display()
    )))
}

/// Run FFmpeg with `args` (whose last element must be `final_out`).
/// Renders to a `.partial` sibling and atomically renames on success, so a
/// file at `final_out` is always a complete render. Returns the stderr tail
/// on failure for the redacted diagnostics bundle.
pub fn run_ffmpeg(
    ffmpeg_bin: &str,
    args: &[String],
    final_out: &Path,
) -> Result<(), super::MediaError> {
    let Some((last, _)) = args.split_last() else {
        return Err(super::MediaError::Ffmpeg("empty ffmpeg args".into()));
    };
    if last != &final_out.to_string_lossy() {
        return Err(super::MediaError::Ffmpeg(
            "ffmpeg output arg does not match destination".into(),
        ));
    }
    if let Some(p) = final_out.parent() {
        std::fs::create_dir_all(p).map_err(|e| super::MediaError::Io(e.to_string()))?;
    }
    let partial = partial_sibling(final_out);
    let mut fixed: Vec<String> = args[..args.len() - 1].to_vec();
    fixed.push(partial.to_string_lossy().into_owned());
    let out = Command::new(ffmpeg_bin)
        .args(&fixed)
        .output()
        .map_err(|e| super::MediaError::Ffmpeg(format!("spawn {ffmpeg_bin}: {e}")))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&partial);
        return Err(super::MediaError::Ffmpeg(format!(
            "ffmpeg exit {}: {}",
            out.status.code().unwrap_or(-1),
            stderr_tail(&out.stderr),
        )));
    }
    std::fs::rename(&partial, final_out)
        .map_err(|e| super::MediaError::Io(format!("rename partial: {e}")))?;
    Ok(())
}

/// `<dir>/.<stem>.partial.<ext>` next to the final output. The real
/// extension is preserved so FFmpeg still picks the muxer from it.
pub fn partial_sibling(final_out: &Path) -> PathBuf {
    let stem = final_out
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "out".into());
    let ext = final_out
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    final_out.with_file_name(format!(".{stem}.partial{ext}"))
}

fn stderr_tail(stderr: &[u8]) -> String {
    const MAX: usize = 2000;
    let s = String::from_utf8_lossy(stderr);
    let s = s.trim();
    if s.len() <= MAX {
        s.to_string()
    } else {
        format!("...{}", &s[s.len() - MAX..])
    }
}

/// Stream facts from `ffprobe -of json` used to verify renders.
#[derive(Debug, Clone, Default)]
pub struct ProbeInfo {
    pub duration_secs: f64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub has_video: bool,
    pub has_audio: bool,
    pub audio_rate: u32,
    pub audio_channels: u32,
    pub color_transfer: String,
}

/// Probe one media file; errors when ffprobe fails or finds no streams.
pub fn ffprobe_file(ffprobe_bin: &str, path: &Path) -> Result<ProbeInfo, super::MediaError> {
    let out = Command::new(ffprobe_bin)
        .args([
            "-v",
            "error",
            "-show_streams",
            "-show_format",
            "-of",
            "json",
            &path.to_string_lossy(),
        ])
        .output()
        .map_err(|e| super::MediaError::Ffmpeg(format!("spawn {ffprobe_bin}: {e}")))?;
    if !out.status.success() {
        return Err(super::MediaError::Ffmpeg(format!(
            "ffprobe failed: {}",
            stderr_tail(&out.stderr)
        )));
    }
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(super::MediaError::Json)?;
    let mut info = ProbeInfo::default();
    if let Some(d) = v
        .pointer("/format/duration")
        .and_then(|d| d.as_str())
        .and_then(|d| d.parse::<f64>().ok())
    {
        info.duration_secs = d;
    }
    for s in v
        .pointer("/streams")
        .and_then(|s| s.as_array())
        .into_iter()
        .flatten()
    {
        match s.get("codec_type").and_then(|c| c.as_str()) {
            Some("video") => {
                info.has_video = true;
                info.width = s.get("width").and_then(|w| w.as_u64()).unwrap_or(0) as u32;
                info.height = s.get("height").and_then(|h| h.as_u64()).unwrap_or(0) as u32;
                info.fps = s
                    .get("avg_frame_rate")
                    .and_then(|r| r.as_str())
                    .and_then(parse_rate)
                    .unwrap_or(0.0);
                info.color_transfer = s
                    .get("color_transfer")
                    .and_then(|c| c.as_str())
                    .unwrap_or("unknown")
                    .to_string();
            }
            Some("audio") => {
                info.has_audio = true;
                info.audio_rate = s
                    .get("sample_rate")
                    .and_then(|r| r.as_str())
                    .and_then(|r| r.parse().ok())
                    .unwrap_or(0);
                info.audio_channels =
                    s.get("channels").and_then(|c| c.as_u64()).unwrap_or(0) as u32;
            }
            _ => {}
        }
    }
    if !info.has_video {
        return Err(super::MediaError::Ffmpeg(format!(
            "no video stream in {}",
            path.display()
        )));
    }
    Ok(info)
}

fn parse_rate(s: &str) -> Option<f64> {
    let (n, d) = s.split_once('/')?;
    let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
    if d == 0.0 {
        None
    } else {
        Some(n / d)
    }
}

/// Verify a hero/reel render: video present, expected geometry, CFR 30 fps
/// (±1 frame duration tolerance is checked by callers against the EDL).
pub fn verify_video(
    ffprobe_bin: &str,
    path: &Path,
    expect_w: u32,
    expect_h: u32,
) -> Result<ProbeInfo, super::MediaError> {
    let info = ffprobe_file(ffprobe_bin, path)?;
    if info.width != expect_w || info.height != expect_h {
        return Err(super::MediaError::Ffmpeg(format!(
            "geometry {}x{} != expected {expect_w}x{expect_h}",
            info.width, info.height
        )));
    }
    if (info.fps - 30.0).abs() > 0.5 {
        return Err(super::MediaError::Ffmpeg(format!(
            "fps {:.2} != 30",
            info.fps
        )));
    }
    if !info.has_audio {
        return Err(super::MediaError::Ffmpeg(
            "render has no audio stream".into(),
        ));
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_matching_is_exact() {
        let filters = "Filters:\n .S tonemap V->V Conversion.\n ..C scale V->V Scale.\n";
        assert!(flag_present(filters, "tonemap"));
        assert!(flag_present(filters, "scale"));
        assert!(
            !flag_present(filters, "zscale"),
            "tone* must not match zscale"
        );
        assert!(!flag_present(filters, "drawtext"));
    }

    #[test]
    fn tonemap_only_for_hdr() {
        let caps = Capabilities {
            tonemap: true,
            zscale: true,
            ..Default::default()
        };
        assert!(tonemap_chain(&caps, "bt709").is_none());
        assert!(tonemap_chain(&caps, "unknown").is_none());
        let full = tonemap_chain(&caps, "smpte2084").unwrap();
        assert!(full.contains("zscale") && full.contains("tonemap"));
        let no_z = Capabilities {
            tonemap: true,
            ..Default::default()
        };
        let fb = tonemap_chain(&no_z, "arib-std-b67").unwrap();
        assert!(!fb.contains("zscale"), "fallback must not need zscale");
        assert!(tonemap_chain(&Capabilities::default(), "smpte2084").is_none());
    }

    #[test]
    fn x264_aac_carries_preset_crf_and_fixed_tail() {
        let a = x264_aac("medium", 19);
        let at = |k: &str| a[a.iter().position(|x| x == k).unwrap() + 1].as_str();
        assert_eq!(at("-c:v"), "libx264");
        assert_eq!(at("-preset"), "medium");
        assert_eq!(at("-crf"), "19");
        assert_eq!(at("-pix_fmt"), "yuv420p");
        assert_eq!((at("-ar"), at("-ac")), ("48000", "2"));
        assert_eq!(at("-movflags"), "+faststart");
    }

    #[test]
    fn partial_sibling_keeps_dir_and_ext() {
        let p = Path::new("/tmp/out/hero.mp4");
        assert_eq!(partial_sibling(p), Path::new("/tmp/out/.hero.partial.mp4"));
    }
}
