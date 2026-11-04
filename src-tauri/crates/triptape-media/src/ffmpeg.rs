//! FFmpeg capability probe, process runner, and ffprobe verifier.
//!
//! The bundled/static FFmpeg varies per machine (system FFmpeg 9.0.2 here:
//! `libx264`, `xfade`, `perspective`, `tonemap`, `loudnorm` present;
//! `zscale`/`drawtext`/`vidstab`/`libass` absent). Every renderer probes
//! once at startup and picks fallback filters accordingly, so exports
//! degrade instead of failing.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, PoisonError};
use triptape_core::proxies;
use triptape_core::types::AssetKind;

/// Single-pass EBU R128 loudness target shared by every renderer.
pub const LOUDNORM: &str = "loudnorm=I=-16:TP=-1.5:LRA=11";

/// Filter-graph fragment showing the full `input` photo fitted over its own
/// blurred fill at `w`x`h` (no crop of the subject, no aspect distortion).
/// `tag` suffixes the internal labels so several copies can share one graph.
/// Ends unlabeled after `setsar=1`, ready for [`hold_still`] + [`glide`].
pub fn fit_over_blur(caps: &Capabilities, input: &str, w: u32, h: u32, tag: &str) -> String {
    let blur = if caps.gblur { ",gblur=sigma=40" } else { "" };
    format!(
        "{input}split[a{tag}][b{tag}];[a{tag}]scale={w}:{h}:force_original_aspect_ratio=increase,crop={w}:{h}{blur}[bg{tag}];[b{tag}]scale={w}:{h}:force_original_aspect_ratio=decrease[fg{tag}];[bg{tag}][fg{tag}]overlay=(W-w)/2:(H-h)/2,setsar=1"
    )
}

/// Hold a one-frame input for `frames` frames at a true 30 fps. The filters
/// before this run once, so a costly composite is not rebuilt per frame
/// the way an input-side `-loop 1` would.
pub fn hold_still(frames: u32) -> String {
    format!(
        "loop=loop={}:size=1:start=0,settb=1/30,setpts=N,fps=30",
        frames.saturating_sub(1)
    )
}

/// Smooth Ken Burns glide over a supersampled frame: an animated source
/// window resampled at sub-pixel positions, then lanczos downscaled to
/// `w`x`h`. Variants (cycling): 0 push-in, 1 pull-out, 2 pan right, 3 pan
/// left, all within 1.05x (the window never shrinks below 20/21 of the
/// frame). `perspective` does the animating: `crop` evaluates its size
/// only once and snaps its position to whole pixels, and `zoompan`
/// re-crops at coarse integer steps, so stills would freeze or jitter.
/// Without `perspective` the frame holds static at `w`x`h` instead.
pub fn glide(caps: &Capabilities, frames: u32, w: u32, h: u32, variant: usize) -> String {
    if !caps.perspective {
        return format!("scale={w}:{h}:flags=lanczos,setsar=1");
    }
    let denom = frames.saturating_sub(1).max(1);
    // Frame progress 0..=1; perspective's `in` counts from 1.
    let p = format!("(min(in-1\\,{denom})/{denom})");
    // Window size as a fraction of the frame, and where its slack sits.
    let (size, slack) = match variant % 4 {
        0 => (format!("(1-{p}/21)"), "0.5".to_string()),
        1 => (format!("((20+{p})/21)"), "0.5".to_string()),
        2 => ("(20/21)".to_string(), p),
        _ => ("(20/21)".to_string(), format!("(1-{p})")),
    };
    let left = format!("W*(1-{size})*{slack}");
    let right = format!("W*((1-{size})*{slack}+{size})");
    let top = format!("H*(1-{size})/2");
    let bottom = format!("H*(1+{size})/2");
    format!(
        "perspective=x0='{left}':y0='{top}':x1='{right}':y1='{top}':x2='{left}':y2='{bottom}':x3='{right}':y3='{bottom}':interpolation=cubic:eval=frame,scale={w}:{h}:flags=lanczos,setsar=1"
    )
}

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
    pub perspective: bool,
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
        perspective: filter("perspective"),
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

/// Apply a source file's EXIF orientation to a decoded PNG in place.
/// Best effort: any failure leaves the decoded pixels untouched.
fn apply_exif_orientation(png: &Path, src: &Path) {
    let Some(o) = triptape_core::metadata::read_image_exif(src).orientation else {
        return;
    };
    let t = triptape_core::metadata::orientation_transform(o);
    if !t.mirror_h && t.rotate_cw_deg == 0 {
        return;
    }
    let Ok(d) = image::open(png) else { return };
    let d = orient_image(d, t);
    let _ = d.save(png);
}

/// Mirror-then-rotate a decoded image per an EXIF orientation transform.
pub(crate) fn orient_image(
    d: image::DynamicImage,
    t: triptape_core::metadata::OrientationTransform,
) -> image::DynamicImage {
    let d = if t.mirror_h { d.fliph() } else { d };
    match t.rotate_cw_deg {
        90 => d.rotate90(),
        180 => d.rotate180(),
        270 => d.rotate270(),
        _ => d,
    }
}

fn is_heif(src: &Path) -> bool {
    src.extension()
        .map(|e| matches!(e.to_ascii_lowercase().to_str(), Some("heic" | "heif")))
        .unwrap_or(false)
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
    let (decoded, via_sips) = {
        let heif = is_heif(src);
        // Order matters (side effects): the first decoder to succeed wins.
        if heif && try_sips() {
            (true, true)
        } else if try_ffmpeg() {
            (true, false)
        } else if !heif && try_sips() {
            (true, true)
        } else {
            (false, false)
        }
    };
    #[cfg(not(target_os = "macos"))]
    let (decoded, via_sips) = (try_ffmpeg(), false);
    if decoded {
        // sips writes stored pixels without applying EXIF orientation
        // (FFmpeg auto-rotates); portrait HEICs would render sideways
        // everywhere this decoder feeds (book, hero stills, reels).
        if via_sips {
            apply_exif_orientation(&out, src);
        }
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

/// Only one still at a time may hold a full-size decode in memory.
static FULL_SIZE_THUMB: Mutex<()> = Mutex::new(());

/// Ensure the editor thumbnail exists at `dst`; a cache hit does no work.
/// Videos use a first frame. Stills let FFmpeg scale directly, which needs no
/// intermediate PNG. HEICs and stills FFmpeg rejects go through
/// [`decode_still_to_png`] like renders do, for its `sips` route. Each attempt
/// writes its own temp sibling, so a file at `dst` is always complete even
/// when requests for one asset overlap.
pub fn render_thumb(
    ffmpeg_bin: &str,
    src: &Path,
    kind: AssetKind,
    dst: &Path,
) -> Result<(), super::MediaError> {
    if proxies::cache_hit(dst) {
        return Ok(());
    }
    let scale_directly = || proxies::run_ffmpeg(ffmpeg_bin, &proxies::thumb_args(src, dst), dst);
    if kind == AssetKind::Video {
        return scale_directly().map_err(super::MediaError::Ffmpeg);
    }
    if !is_heif(src) && scale_directly().is_ok() {
        return Ok(());
    }
    let _full_size = FULL_SIZE_THUMB
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    // An overlapping request may have finished while this one waited.
    if proxies::cache_hit(dst) {
        return Ok(());
    }
    let io_err = |e: String| super::MediaError::Io(e);
    let decoded = decode_still_to_png(ffmpeg_bin, src)?;
    let img = image::open(&decoded.path).map_err(|e| io_err(e.to_string()))?;
    if let Some(p) = dst.parent() {
        std::fs::create_dir_all(p).map_err(|e| io_err(e.to_string()))?;
    }
    let tmp = proxies::temp_sibling(dst);
    let edge = proxies::THUMB_LONG_EDGE;
    // JPEG has no alpha channel; decoded PNGs may carry one.
    let written = img
        .thumbnail(edge, edge)
        .to_rgb8()
        .save(&tmp)
        .map_err(|e| io_err(e.to_string()))
        .and_then(|()| std::fs::rename(&tmp, dst).map_err(|e| io_err(e.to_string())));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
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

    /// Run `glide` over one synthetic still; returns its gray8 frames.
    fn glide_frames(caps: &Capabilities, frames: u32, variant: usize) -> Vec<Vec<u8>> {
        const SIZE: (u32, u32) = (96, 54);
        let vf = format!(
            "{},{},format=gray",
            hold_still(frames),
            glide(caps, frames, SIZE.0, SIZE.1, variant)
        );
        let out = Command::new(&caps.ffmpeg_bin)
            .args(["-v", "error", "-f", "lavfi", "-i"])
            .args(["testsrc2=s=288x162:r=1:d=1", "-vf", &vf])
            .args(["-f", "rawvideo", "-"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
            .chunks((SIZE.0 * SIZE.1) as usize)
            .map(<[u8]>::to_vec)
            .collect()
    }

    fn mean_diff(a: &[u8], b: &[u8]) -> f64 {
        let sum: u64 = a
            .iter()
            .zip(b)
            .map(|(x, y)| u64::from(x.abs_diff(*y)))
            .sum();
        sum as f64 / a.len() as f64
    }

    /// Filter-string checks cannot catch a motion that ffmpeg silently
    /// ignores, so run the real filter and compare pixels.
    #[test]
    fn glide_really_moves_every_variant() {
        let caps = probe_capabilities("ffmpeg", "ffprobe");
        if !caps.perspective {
            eprintln!("ffmpeg lacks perspective; skipping");
            return;
        }
        const FRAMES: u32 = 8;
        let runs: Vec<_> = (0..4).map(|v| glide_frames(&caps, FRAMES, v)).collect();
        for (v, frames) in runs.iter().enumerate() {
            assert_eq!(frames.len(), FRAMES as usize, "variant {v} frame count");
            let (first, last) = (&frames[0], &frames[FRAMES as usize - 1]);
            assert!(mean_diff(first, last) > 1.0, "variant {v} did not move");
        }
        // Push-in and pull-out mirror each other, as do the two pans.
        let end = FRAMES as usize - 1;
        for (a, b) in [(0, 1), (2, 3)] {
            assert!(
                mean_diff(&runs[a][0], &runs[b][end]) < 0.5,
                "{a} start vs {b} end"
            );
            assert!(
                mean_diff(&runs[a][end], &runs[b][0]) < 0.5,
                "{a} end vs {b} start"
            );
        }
    }

    #[test]
    fn orient_image_rotates_pixels_not_metadata() {
        // 2x1 red|blue; EXIF 6 (rotate 90 CW) -> 1x2 with red on top.
        let mut img = image::RgbImage::new(2, 1);
        img.put_pixel(0, 0, image::Rgb([255, 0, 0]));
        img.put_pixel(1, 0, image::Rgb([0, 0, 255]));
        let t = triptape_core::metadata::orientation_transform(6);
        let out = orient_image(image::DynamicImage::ImageRgb8(img), t).to_rgb8();
        assert_eq!(out.dimensions(), (1, 2));
        assert_eq!(out.get_pixel(0, 0).0, [255, 0, 0]);
        assert_eq!(out.get_pixel(0, 1).0, [0, 0, 255]);
        // EXIF 2 (mirror only) swaps left and right.
        let mut img3 = image::RgbImage::new(2, 1);
        img3.put_pixel(0, 0, image::Rgb([255, 0, 0]));
        let t2 = triptape_core::metadata::orientation_transform(2);
        let flipped = orient_image(image::DynamicImage::ImageRgb8(img3), t2).to_rgb8();
        assert_eq!(flipped.get_pixel(1, 0).0, [255, 0, 0]);
        // Identity leaves pixels alone.
        let img2 = image::RgbImage::from_pixel(2, 1, image::Rgb([9, 9, 9]));
        let same = orient_image(
            image::DynamicImage::ImageRgb8(img2),
            triptape_core::metadata::orientation_transform(1),
        )
        .to_rgb8();
        assert_eq!(same.dimensions(), (2, 1));
    }

    #[test]
    fn fit_over_blur_tags_labels_and_gates_blur() {
        let mut caps = Capabilities {
            gblur: true,
            ..Default::default()
        };
        let g = fit_over_blur(&caps, "[2:v]", 1080, 1920, "2");
        assert!(g.starts_with("[2:v]split[a2][b2]"), "{g}");
        assert!(g.contains("gblur=sigma=40[bg2]"), "{g}");
        assert!(
            g.ends_with("[bg2][fg2]overlay=(W-w)/2:(H-h)/2,setsar=1"),
            "{g}"
        );
        caps.gblur = false;
        assert!(!fit_over_blur(&caps, "[0:v]", 1920, 1080, "").contains("gblur"));
        // Motion runs per frame, with progress clamped at the end.
        caps.perspective = true;
        let g = glide(&caps, 90, 1920, 1080, 0);
        assert!(g.contains("min(in-1\\,89)/89"), "{g}");
        assert!(g.contains("eval=frame"), "{g}");
        assert!(g.contains("scale=1920:1080:flags=lanczos"), "{g}");
        assert!(!g.contains("zoompan"), "{g}");
        // No perspective filter: a static hold, never an unrunnable graph.
        caps.perspective = false;
        assert_eq!(
            glide(&caps, 90, 1920, 1080, 0),
            "scale=1920:1080:flags=lanczos,setsar=1"
        );
        assert_eq!(
            hold_still(1),
            "loop=loop=0:size=1:start=0,settb=1/30,setpts=N,fps=30"
        );
    }

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

    fn has_ffmpeg() -> bool {
        Command::new("ffmpeg").arg("-version").output().is_ok()
    }

    fn assert_is_thumb(path: &Path, want: (u32, u32)) {
        let img = image::open(path).unwrap();
        assert_eq!((img.width(), img.height()), want);
        assert_eq!(
            image::ImageFormat::from_path(path).unwrap(),
            image::guess_format(&std::fs::read(path).unwrap()).unwrap()
        );
        assert_eq!(
            dir_entries(path.parent().unwrap()),
            1,
            "temp file left behind"
        );
    }

    fn dir_entries(dir: &Path) -> usize {
        std::fs::read_dir(dir).map(|d| d.count()).unwrap_or(0)
    }

    #[test]
    fn render_thumb_cache_hit_does_no_work() {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("a.jpg");
        std::fs::write(&dst, b"cached").unwrap();
        render_thumb("false", Path::new("/missing"), AssetKind::Video, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"cached");
    }

    #[test]
    fn render_thumb_failure_leaves_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("thumb").join("a.jpg");
        for kind in [AssetKind::Video, AssetKind::Photo] {
            assert!(render_thumb("false", Path::new("/missing"), kind, &dst).is_err());
            assert_eq!(dir_entries(dst.parent().unwrap()), 0);
        }
    }

    #[test]
    fn render_thumb_downscales_a_still_with_alpha() {
        if !has_ffmpeg() {
            eprintln!("ffmpeg missing; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("rgba.png");
        image::RgbaImage::new(640, 480).save(&src).unwrap();
        let dst = dir.path().join("thumb").join("a.jpg");
        render_thumb("ffmpeg", &src, AssetKind::Photo, &dst).unwrap();
        assert_is_thumb(&dst, (320, 240));
    }

    #[test]
    fn render_thumb_takes_the_full_size_route_for_heif() {
        if !has_ffmpeg() {
            eprintln!("ffmpeg missing; skipping");
            return;
        }
        // PNG bytes under a HEIC name: both decoders sniff the content.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("photo.heic");
        image::RgbaImage::new(640, 480)
            .save_with_format(&src, image::ImageFormat::Png)
            .unwrap();
        let dst = dir.path().join("thumb").join("a.jpg");
        render_thumb("ffmpeg", &src, AssetKind::Photo, &dst).unwrap();
        assert_is_thumb(&dst, (320, 240));
    }

    #[test]
    fn render_thumb_applies_exif_orientation_when_scaling_directly() {
        if !has_ffmpeg() {
            eprintln!("ffmpeg missing; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain.jpg");
        image::RgbImage::new(640, 480).save(&plain).unwrap();
        // Big-endian APP1 holding a single tag: Orientation = 6 (rotate 90 CW).
        let mut app1 = b"\xFF\xE1\x00\x22Exif\0\0MM\0\x2A\0\0\0\x08\0\x01".to_vec();
        app1.extend_from_slice(b"\x01\x12\0\x03\0\0\0\x01\0\x06\0\0\0\0\0\0");
        let jpeg = std::fs::read(&plain).unwrap();
        let src = dir.path().join("portrait.jpg");
        std::fs::write(&src, [&jpeg[..2], &app1[..], &jpeg[2..]].concat()).unwrap();
        let dst = dir.path().join("thumb").join("a.jpg");
        render_thumb("ffmpeg", &src, AssetKind::Photo, &dst).unwrap();
        assert_is_thumb(&dst, (240, 320));
    }

    #[test]
    fn render_thumb_survives_overlapping_requests() {
        if !has_ffmpeg() {
            eprintln!("ffmpeg missing; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("big.png");
        image::RgbImage::new(1600, 1200).save(&src).unwrap();
        let dst = dir.path().join("thumb").join("a.jpg");
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..4)
                .map(|_| scope.spawn(|| render_thumb("ffmpeg", &src, AssetKind::Photo, &dst)))
                .collect();
            for worker in workers {
                worker.join().unwrap().unwrap();
            }
        });
        assert_is_thumb(&dst, (320, 240));
    }

    #[test]
    fn render_thumb_uses_a_video_first_frame() {
        if !has_ffmpeg() {
            eprintln!("ffmpeg missing; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("clip.mp4");
        let made = Command::new("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i", "testsrc2=s=640x360:d=1"])
            .arg(&src)
            .status()
            .unwrap();
        assert!(made.success());
        let dst = dir.path().join("thumb").join("a.jpg");
        render_thumb("ffmpeg", &src, AssetKind::Video, &dst).unwrap();
        assert_is_thumb(&dst, (320, 180));
    }

    #[test]
    fn partial_sibling_keeps_dir_and_ext() {
        let p = Path::new("/tmp/out/hero.mp4");
        assert_eq!(partial_sibling(p), Path::new("/tmp/out/.hero.partial.mp4"));
    }
}
