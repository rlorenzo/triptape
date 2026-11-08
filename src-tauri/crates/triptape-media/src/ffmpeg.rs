//! FFmpeg capability probe, process runner, and ffprobe verifier.
//!
//! The bundled/static FFmpeg varies per machine (system FFmpeg 9.0.2 here:
//! `libx264`, `xfade`, `perspective`, `tonemap`, `loudnorm` present;
//! `zscale`/`drawtext`/`vidstab`/libass absent). Every renderer probes
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
/// `sigma` is the fill blur radius in canvas pixels, so larger canvases
/// scale it to keep the same look.
pub fn fit_over_blur(
    caps: &Capabilities,
    input: &str,
    w: u32,
    h: u32,
    tag: &str,
    sigma: u32,
) -> String {
    let blur = if caps.gblur {
        format!(",gblur=sigma={sigma}")
    } else {
        String::new()
    };
    format!(
        "{input}split[a{tag}][b{tag}];[a{tag}]scale={w}:{h}:force_original_aspect_ratio=increase,crop={w}:{h}{blur}[bg{tag}];[b{tag}]scale={w}:{h}:force_original_aspect_ratio=decrease[fg{tag}];[bg{tag}][fg{tag}]overlay=(W-w)/2:(H-h)/2,setsar=1"
    )
}

/// Filter-graph fragment (a complete simple filtergraph on the implicit
/// input) showing each frame whole, centred in `w`x`h` over a blurred
/// cover-fill of itself. The frame is first fitted to `w`x`h` (`lanczos`
/// when upscaling), then `tonemap` (an HDR chain) runs on that fitted
/// frame, converted to 8-bit before the overlay; the fill is built from a
/// 1/8 copy and scaled back up, so the blur costs almost nothing per
/// frame even at 4K.
pub fn video_contain_over_blur(
    caps: &Capabilities,
    w: u32,
    h: u32,
    lanczos: bool,
    tonemap: Option<&str>,
) -> String {
    let (sw, sh) = ((w / 8).max(2) & !1, (h / 8).max(2) & !1);
    let blur = if caps.gblur { ",gblur=sigma=4" } else { "" };
    let flags = if lanczos { ":flags=lanczos" } else { "" };
    let tone = tonemap.map_or(String::new(), |t| format!(",{t},format=yuv420p"));
    format!(
        "scale={w}:{h}:force_original_aspect_ratio=decrease:force_divisible_by=2{flags}{tone},split[a][b];[a]scale={sw}:{sh}:force_original_aspect_ratio=increase,crop={sw}:{sh}{blur},scale={w}:{h}:flags=bicubic[bg];[bg][b]overlay=(W-w)/2:(H-h)/2,setsar=1"
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
    glide_depth(caps, frames, w, h, variant, GLIDE_DEPTH)
}

/// Window divisor of the standard glide: the window shrinks to
/// `(D-1)/D` of the frame, i.e. a 1.05x push at 21.
pub const GLIDE_DEPTH: u32 = 21;
/// Gentler divisor for clips shown whole over a blurred fill (about 1.5x
/// less movement: the full subject is on screen, so it must not crowd the
/// edges).
pub const GLIDE_DEPTH_GENTLE: u32 = 64;

/// [`glide`] with an explicit window divisor (see [`GLIDE_DEPTH`]).
pub fn glide_depth(
    caps: &Capabilities,
    frames: u32,
    w: u32,
    h: u32,
    variant: usize,
    depth: u32,
) -> String {
    if !caps.perspective {
        return format!("scale={w}:{h}:flags=lanczos,setsar=1");
    }
    let denom = frames.saturating_sub(1).max(1);
    // Frame progress 0..=1; perspective's `in` counts from 1.
    let p = format!("(min(in-1\\,{denom})/{denom})");
    // Window size as a fraction of the frame, and where its slack sits.
    let (size, slack) = match variant % 4 {
        0 => (format!("(1-{p}/{depth})"), "0.5".to_string()),
        1 => (format!("(({}+{p})/{depth})", depth - 1), "0.5".to_string()),
        2 => (format!("({}/{depth})", depth - 1), p),
        _ => (format!("({}/{depth})", depth - 1), format!("(1-{p})")),
    };
    let left = format!("W*(1-{size})*{slack}");
    let right = format!("W*((1-{size})*{slack}+{size})");
    let top = format!("H*(1-{size})/2");
    let bottom = format!("H*(1+{size})/2");
    format!(
        "perspective=x0='{left}':y0='{top}':x1='{right}':y1='{top}':x2='{left}':y2='{bottom}':x3='{right}':y3='{bottom}':interpolation=cubic:eval=frame,scale={w}:{h}:flags=lanczos,setsar=1"
    )
}

/// A panorama strip is scaled so its short side is this multiple of the
/// output's short side before panning. The pan then crops a 2x window at an
/// integer strip pixel, so position error is at most half an output pixel
/// after the lanczos downscale, while per-frame work stays one crop plus one
/// scale instead of a resampling `perspective` over the whole strip.
const PANO_SS: u32 = 2;

/// Smooth side-to-side (or top-to-bottom for tall images) pan over a whole
/// panorama, no zoompan. The photo is scaled once so its short side is 2x
/// the frame's ([`PANO_SS`]), covering the frame with no bars or blurred
/// fill; a frame-sized (2x) window then slides across it with smoothstep
/// easing and is lanczos-downscaled to `w`x`h`. `aspect` is the display
/// width / height; `coverage` the fraction of the overflow to travel (see
/// `triptape_core::plan::pano_motion`); `reverse` flips the direction.
/// Needs only `scale`/`crop`/`loop`, so it works without `perspective`.
pub fn pano_pan(frames: u32, w: u32, h: u32, aspect: f64, coverage: f64, reverse: bool) -> String {
    let tall = aspect < 1.0;
    // Window in strip pixels: the frame at 2x.
    let (ww, wh) = (w * PANO_SS, h * PANO_SS);
    let fill = if tall {
        format!("scale={ww}:-2:flags=lanczos")
    } else {
        format!("scale=-2:{wh}:flags=lanczos")
    };
    let denom = frames.saturating_sub(1).max(1);
    let p = format!("(min(n\\,{denom})/{denom})");
    let eased = format!("({p}*{p}*(3-2*{p}))");
    let eased = if reverse {
        format!("(1-{eased})")
    } else {
        eased
    };
    // Where the window sits within the slack, centered on 0.5 and narrowed
    // by `coverage`.
    let slack = format!("(0.5+({eased}-0.5)*{coverage:.4})");
    let (x, y) = if tall {
        ("0".to_string(), format!("floor((ih-oh)*{slack})"))
    } else {
        (format!("floor((iw-ow)*{slack})"), "0".to_string())
    };
    format!(
        "{fill},setsar=1,{hold},crop=w={ww}:h={wh}:x='{x}':y='{y}':exact=1,scale={w}:{h}:flags=lanczos,setsar=1",
        hold = hold_still(frames),
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
    /// Per-process encoder/filter thread cap, set when several renders run
    /// at once so their ffmpegs share the cores. `None` leaves ffmpeg's
    /// own default (all cores).
    pub threads: Option<usize>,
    /// Cancel flag and progress sink; every [`Capabilities::run`] polls it.
    pub progress: crate::progress::Progress,
}

impl Capabilities {
    /// Captions burn in through libass's `subtitles` filter; without it
    /// renderers emit a sidecar SRT plus a note.
    pub fn can_burn_captions(&self) -> bool {
        self.subtitles
    }

    /// Full HDR->SDR chain needs zscale; otherwise tonemap alone is the
    /// fallback (see [`tonemap_chain`]).
    pub fn full_tonemap(&self) -> bool {
        self.tonemap && self.zscale
    }
}

/// Escape `v` as a filter option value, then as a filtergraph element, so it
/// survives both parsing levels (ffmpeg-filters "Notes on filtergraph
/// escaping"): level one escapes backslash, quote and colon; level two
/// backslash, quote, comma, semicolon and brackets.
fn escape_filter_value(v: &str) -> String {
    backslash_escape(
        &backslash_escape(v, &['\\', '\'', ':']),
        &['\\', '\'', ',', ';', '[', ']'],
    )
}

/// Prefix every char of `v` found in `special` with a backslash.
fn backslash_escape(v: &str, special: &[char]) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        if special.contains(&c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `subtitles` filter burning the SRT at `srt` into the frame: white text,
/// dark outline, bottom-centre and lifted clear of the lower-third. Sizes
/// are in libass's 288-line default play resolution, so they scale with the
/// frame height by themselves (1080p: 3.75 px per unit, 4K: 7.5), and the
/// same style serves every output resolution. Paths with spaces, colons,
/// commas or apostrophes are escaped for both parsing levels.
pub fn subtitles_filter(srt: &Path) -> String {
    let style = "FontName=Helvetica,FontSize=11,PrimaryColour=&H00FFFFFF,OutlineColour=&H80000000,BorderStyle=1,Outline=1.2,Shadow=0.5,Alignment=2,MarginV=60";
    format!(
        "subtitles=filename={}:force_style={}",
        escape_filter_value(&path_arg(srt)),
        escape_filter_value(style)
    )
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
        // ffmpeg has no filter called `libass`; the library surfaces as the
        // `subtitles` and `ass` filters.
        libass: filter("subtitles") || filter("ass"),
        vidstabdetect: filter("vidstabdetect"),
        vidstabtransform: filter("vidstabtransform"),
        subtitles: filter("subtitles"),
        ..Default::default()
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

/// Shrink to thumbnail size and apply an EXIF orientation transform (if any),
/// all in memory. The thumbnail path uses this instead of the
/// [`apply_exif_orientation`] file round-trip: re-encoding a 24 MP PNG to
/// disk just to downscale it to 320 px dominated scoring profiles. Shrinking
/// first means the transform never copies the full-size buffer; the long
/// edge is the same either way because `thumbnail` fits a square box.
fn orient_and_downscale(
    img: image::DynamicImage,
    transform: Option<triptape_core::metadata::OrientationTransform>,
    edge: u32,
) -> image::RgbImage {
    let small = img.thumbnail(edge, edge);
    let oriented = match transform {
        Some(t) if t.mirror_h || t.rotate_cw_deg != 0 => orient_image(small, t),
        _ => small,
    };
    // JPEG has no alpha channel; decoded PNGs may carry one.
    oriented.to_rgb8()
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
    /// True when the decoder left EXIF orientation unapplied (sips output);
    /// FFmpeg output arrives already rotated. Renders don't care (they use
    /// [`decode_still_to_png`], which orients the file); the thumbnail path
    /// orients in memory instead so it never re-encodes full-size pixels.
    pub needs_orientation: bool,
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
    let mut decoded = decode_still_raw(ffmpeg_bin, src)?;
    if decoded.needs_orientation {
        apply_exif_orientation(&decoded.path, src);
        decoded.needs_orientation = false;
    }
    Ok(decoded)
}

/// [`decode_still_to_png`] without the orientation file round-trip: the
/// sips/FFmpeg decode selection is shared, but orientation (if any) is left
/// for the caller to apply in memory.
fn decode_still_raw(ffmpeg_bin: &str, src: &Path) -> Result<DecodedStill, super::MediaError> {
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
        // (FFmpeg auto-rotates); the caller orients from here.
        return Ok(DecodedStill {
            _dir: dir,
            path: out,
            needs_orientation: via_sips,
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
    let decoded = decode_still_raw(ffmpeg_bin, src)?;
    let mut transform = None;
    if decoded.needs_orientation {
        transform = triptape_core::metadata::read_image_exif(src)
            .orientation
            .map(triptape_core::metadata::orientation_transform);
    }
    let img = orient_and_downscale(
        image::open(&decoded.path).map_err(|e| io_err(e.to_string()))?,
        transform,
        proxies::THUMB_LONG_EDGE,
    );
    if let Some(p) = dst.parent() {
        std::fs::create_dir_all(p).map_err(|e| io_err(e.to_string()))?;
    }
    let tmp = proxies::temp_sibling(dst);
    let written = img
        .save(&tmp)
        .map_err(|e| io_err(e.to_string()))
        .and_then(|()| std::fs::rename(&tmp, dst).map_err(|e| io_err(e.to_string())));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Extras for [`run_ffmpeg_with`].
#[derive(Default)]
pub struct RunOpts<'a> {
    /// Encoder/filter thread cap for this process.
    pub threads: Option<usize>,
    /// Cancel source: when it fires, ffmpeg is killed, its `.partial` is
    /// removed and `Cancelled` is returned.
    pub progress: Option<&'a crate::progress::Progress>,
    /// Expected output seconds and a callback: ffmpeg's `-progress` stream
    /// (`out_time_us`) is turned into a `0..=1` fraction of that length.
    pub watch: Option<(f64, &'a dyn Fn(f64))>,
}

/// Run FFmpeg with `args` (whose last element must be `final_out`), with an
/// optional thread cap, cancellation and encode progress. Renders to a
/// `.partial` sibling and atomically renames on success, so a file at
/// `final_out` is always a complete render. Returns the stderr tail on
/// failure for the redacted diagnostics bundle.
pub fn run_ffmpeg_with(
    ffmpeg_bin: &str,
    args: &[String],
    final_out: &Path,
    opts: &RunOpts<'_>,
) -> Result<(), super::MediaError> {
    let Some((last, _)) = args.split_last() else {
        return Err(super::MediaError::Ffmpeg("empty ffmpeg args".into()));
    };
    if last != &final_out.to_string_lossy() {
        return Err(super::MediaError::Ffmpeg(
            "ffmpeg output arg does not match destination".into(),
        ));
    }
    if let Some(p) = opts.progress {
        p.check_cancelled()?;
    }
    if let Some(p) = final_out.parent() {
        std::fs::create_dir_all(p).map_err(|e| super::MediaError::Io(e.to_string()))?;
    }
    let partial = partial_sibling(final_out);
    let watch = opts.watch.filter(|(total, _)| *total > 0.0);
    let mut fixed: Vec<String> = vec![];
    if watch.is_some() {
        fixed.extend(["-nostats", "-progress", "pipe:1"].map(String::from));
    }
    fixed.extend_from_slice(&args[..args.len() - 1]);
    if let Some(n) = opts.threads {
        // Output-side `-threads` caps the encoder; the simple and complex
        // filter-graph pools are global. All sit before the output path.
        let n = n.max(1).to_string();
        fixed.extend([
            "-threads".into(),
            n.clone(),
            "-filter_threads".into(),
            n.clone(),
            "-filter_complex_threads".into(),
            n,
        ]);
    }
    fixed.push(partial.to_string_lossy().into_owned());
    let mut child = Command::new(ffmpeg_bin)
        .args(&fixed)
        .stdin(std::process::Stdio::null())
        .stdout(if watch.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| super::MediaError::Ffmpeg(format!("spawn {ffmpeg_bin}: {e}")))?;
    // Drain both pipes on helper threads so a full pipe never stalls ffmpeg.
    let mut stderr = child.stderr.take();
    let err_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(e) = stderr.as_mut() {
            let _ = std::io::Read::read_to_end(e, &mut buf);
        }
        buf
    });
    let out_time_us = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let out_thread = child.stdout.take().map(|stdout| {
        let slot = std::sync::Arc::clone(&out_time_us);
        std::thread::spawn(move || {
            for line in
                std::io::BufRead::lines(std::io::BufReader::new(stdout)).map_while(Result::ok)
            {
                if let Some(v) = line.strip_prefix("out_time_us=") {
                    if let Ok(us) = v.trim().parse::<u64>() {
                        slot.store(us, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
        })
    });
    let join_pipes = |out_thread: Option<std::thread::JoinHandle<()>>| {
        if let Some(t) = out_thread {
            let _ = t.join();
        }
        err_thread.join().unwrap_or_default()
    };
    let mut last_reported = u64::MAX;
    // Short calls (most of them) exit within a few ms, so the poll starts
    // tight and backs off to the cancel-latency ceiling.
    let mut nap = std::time::Duration::from_millis(1);
    let waited = loop {
        if opts.progress.is_some_and(|p| p.is_cancelled()) {
            break Err(super::MediaError::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(st)) => break Ok(st),
            Ok(None) => {}
            Err(e) => break Err(super::MediaError::Ffmpeg(format!("wait {ffmpeg_bin}: {e}"))),
        }
        if let Some((total, cb)) = watch {
            let us = out_time_us.load(std::sync::atomic::Ordering::Relaxed);
            if us != last_reported {
                last_reported = us;
                cb((us as f64 / 1e6 / total).clamp(0.0, 1.0));
            }
        }
        std::thread::sleep(nap);
        nap = (nap * 2).min(std::time::Duration::from_millis(25));
    };
    match waited {
        Ok(status) => {
            let stderr = join_pipes(out_thread);
            finish_ffmpeg(status, &stderr, &partial, final_out)
        }
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            join_pipes(out_thread);
            let _ = std::fs::remove_file(&partial);
            Err(e)
        }
    }
}

/// Promote the `.partial` on success; on failure delete it and report the
/// stderr tail.
fn finish_ffmpeg(
    status: std::process::ExitStatus,
    stderr: &[u8],
    partial: &Path,
    final_out: &Path,
) -> Result<(), super::MediaError> {
    if !status.success() {
        let _ = std::fs::remove_file(partial);
        return Err(super::MediaError::Ffmpeg(format!(
            "ffmpeg exit {}: {}",
            status.code().unwrap_or(-1),
            stderr_tail(stderr),
        )));
    }
    std::fs::rename(partial, final_out)
        .map_err(|e| super::MediaError::Io(format!("rename partial: {e}")))?;
    Ok(())
}

impl Capabilities {
    /// [`run_ffmpeg_with`] using this context's thread cap and cancel flag.
    pub fn run(&self, args: &[String], final_out: &Path) -> Result<(), super::MediaError> {
        self.run_with(args, final_out, None)
    }

    /// [`Capabilities::run`] that also reports encode progress, as a
    /// fraction of `total_secs` of output, to `on_fraction`.
    pub fn run_progress(
        &self,
        args: &[String],
        final_out: &Path,
        total_secs: f64,
        on_fraction: &dyn Fn(f64),
    ) -> Result<(), super::MediaError> {
        self.run_with(args, final_out, Some((total_secs, on_fraction)))
    }

    fn run_with(
        &self,
        args: &[String],
        final_out: &Path,
        watch: Option<(f64, &dyn Fn(f64))>,
    ) -> Result<(), super::MediaError> {
        run_ffmpeg_with(
            &self.ffmpeg_bin,
            args,
            final_out,
            &RunOpts {
                threads: self.threads,
                progress: Some(&self.progress),
                watch,
            },
        )
    }
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
    /// True when at least one *usable* audio stream exists (see
    /// [`is_unusable_audio_codec`]); apac-only files count as silent.
    pub has_audio: bool,
    /// Position among the file's audio streams of the one to use
    /// (`-map 0:a:N`): the first stream we can decode and mux.
    pub audio_index: usize,
    pub audio_rate: u32,
    pub audio_channels: u32,
    pub color_transfer: String,
    /// Display rotation in degrees from the stream's display matrix or
    /// `rotate` tag (iPhone portrait clips are landscape-coded with 90).
    pub rotation: i32,
}

/// Audio codecs ffmpeg cannot decode or the MP4 muxer cannot take. iPhone 16
/// clips carry a spatial `apple_apac` track beside the regular AAC one;
/// mapping it makes the encode fail ("Invalid argument" opening the output).
pub fn is_unusable_audio_codec(codec: &str) -> bool {
    matches!(codec, "" | "none" | "unknown" | "apple_apac")
}

impl ProbeInfo {
    /// Width and height as displayed: ffmpeg autorotates by default, so a
    /// 90/270 degree stream arrives with the two swapped.
    pub fn display_dims(&self) -> (u32, u32) {
        if self.rotation.rem_euclid(180) == 90 {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        }
    }

    /// Input specifier for the usable audio stream of input 0, e.g. `0:a:0`.
    pub fn audio_spec(&self) -> String {
        format!("0:a:{}", self.audio_index)
    }
}

/// Audio specifier for input 0: the probed usable stream, or the first audio
/// stream when the probe failed.
pub fn audio_spec(probe: Option<&ProbeInfo>) -> String {
    probe.map_or_else(|| "0:a:0".to_string(), ProbeInfo::audio_spec)
}

/// `-map` value for a source's audio: the first usable stream, `None` when the
/// probe finds no usable audio (e.g. `apple_apac` only), and the optional
/// first stream when the probe itself fails.
pub fn audio_map(ffprobe_bin: &str, path: &Path) -> Option<String> {
    match ffprobe_file(ffprobe_bin, path) {
        Ok(info) => info.has_audio.then(|| info.audio_spec()),
        Err(_) => Some("0:a:0?".to_string()),
    }
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
    parse_probe(&v, path)
}

/// Fold `ffprobe -show_streams -show_format -of json` output into [`ProbeInfo`].
pub(crate) fn parse_probe(
    v: &serde_json::Value,
    path: &Path,
) -> Result<ProbeInfo, super::MediaError> {
    let mut info = ProbeInfo::default();
    if let Some(d) = v
        .pointer("/format/duration")
        .and_then(|d| d.as_str())
        .and_then(|d| d.parse::<f64>().ok())
    {
        info.duration_secs = d;
    }
    let mut audio_seen = 0usize;
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
                info.rotation = stream_rotation(s);
                info.color_transfer = s
                    .get("color_transfer")
                    .and_then(|c| c.as_str())
                    .unwrap_or("unknown")
                    .to_string();
            }
            Some("audio") => {
                let pos = audio_seen;
                audio_seen += 1;
                let codec = s.get("codec_name").and_then(|c| c.as_str()).unwrap_or("");
                if info.has_audio || is_unusable_audio_codec(codec) {
                    continue;
                }
                info.has_audio = true;
                info.audio_index = pos;
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

/// Rotation of a video stream: display-matrix side data, else the legacy
/// `rotate` tag. 0 when neither is present.
fn stream_rotation(s: &serde_json::Value) -> i32 {
    let side = s
        .get("side_data_list")
        .and_then(|l| l.as_array())
        .into_iter()
        .flatten()
        .find_map(|d| d.get("rotation").and_then(|r| r.as_f64()));
    let tag = || {
        s.pointer("/tags/rotate")
            .and_then(|r| r.as_str())
            .and_then(|r| r.parse::<f64>().ok())
    };
    side.or_else(tag).map_or(0, |r| r.round() as i32)
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
    fn orient_and_downscale_applies_the_transform_to_the_thumb() {
        // 640x480, red left / blue right; EXIF 6 (rotate 90 CW) gives a
        // portrait 320-edge thumb of 240x320 with red on top.
        let mut img = image::RgbImage::new(640, 480);
        for x in 0..640 {
            for y in 0..480 {
                img.put_pixel(
                    x,
                    y,
                    if x < 320 {
                        image::Rgb([255, 0, 0])
                    } else {
                        image::Rgb([0, 0, 255])
                    },
                );
            }
        }
        let t = triptape_core::metadata::orientation_transform(6);
        let thumb = orient_and_downscale(image::DynamicImage::ImageRgb8(img), Some(t), 320);
        assert_eq!(thumb.dimensions(), (240, 320));
        assert_eq!(thumb.get_pixel(120, 10).0, [255, 0, 0]);
        assert_eq!(thumb.get_pixel(120, 310).0, [0, 0, 255]);
        // No transform: plain landscape downscale, colors unmoved.
        let mut flat = image::RgbImage::new(640, 480);
        for x in 0..640 {
            for y in 0..480 {
                flat.put_pixel(
                    x,
                    y,
                    if x < 320 {
                        image::Rgb([255, 0, 0])
                    } else {
                        image::Rgb([0, 0, 255])
                    },
                );
            }
        }
        let plain = orient_and_downscale(image::DynamicImage::ImageRgb8(flat), None, 320);
        assert_eq!(plain.dimensions(), (320, 240));
        assert_eq!(plain.get_pixel(10, 120).0, [255, 0, 0]);
        assert_eq!(plain.get_pixel(310, 120).0, [0, 0, 255]);
    }

    #[test]
    fn fit_over_blur_tags_labels_and_gates_blur() {
        let mut caps = Capabilities {
            gblur: true,
            ..Default::default()
        };
        let g = fit_over_blur(&caps, "[2:v]", 1080, 1920, "2", 40);
        assert!(g.starts_with("[2:v]split[a2][b2]"), "{g}");
        assert!(g.contains("gblur=sigma=40[bg2]"), "{g}");
        assert!(
            g.ends_with("[bg2][fg2]overlay=(W-w)/2:(H-h)/2,setsar=1"),
            "{g}"
        );
        caps.gblur = false;
        assert!(!fit_over_blur(&caps, "[0:v]", 1920, 1080, "", 40).contains("gblur"));
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

    fn probe_json(streams: &str) -> serde_json::Value {
        serde_json::from_str(&format!(
            r#"{{"format":{{"duration":"2.0"}},"streams":[{streams}]}}"#
        ))
        .unwrap()
    }

    const VIDEO: &str = r#"{"codec_type":"video","codec_name":"hevc","width":1920,"height":1080,"avg_frame_rate":"30/1"}"#;
    const AAC: &str =
        r#"{"codec_type":"audio","codec_name":"aac","sample_rate":"48000","channels":1}"#;
    const APAC: &str =
        r#"{"codec_type":"audio","codec_name":"apple_apac","sample_rate":"48000","channels":4}"#;
    const DATA: &str = r#"{"codec_type":"data","codec_name":"none"}"#;

    /// iPhone 16 layout: hevc, aac, apple_apac, data tracks.
    #[test]
    fn probe_picks_first_usable_audio_stream() {
        let v = probe_json(&format!("{VIDEO},{AAC},{APAC},{DATA}"));
        let info = parse_probe(&v, Path::new("x.mov")).unwrap();
        assert!(info.has_audio);
        assert_eq!(info.audio_spec(), "0:a:0");
        assert_eq!(info.audio_channels, 1);
        // apac listed first: skip it, map the aac that follows.
        let v = probe_json(&format!("{VIDEO},{APAC},{AAC}"));
        let info = parse_probe(&v, Path::new("x.mov")).unwrap();
        assert_eq!(info.audio_spec(), "0:a:1");
        assert_eq!(info.audio_rate, 48000);
    }

    #[test]
    fn audio_spec_defaults_to_first_stream_without_probe() {
        assert_eq!(audio_spec(None), "0:a:0");
        let v = probe_json(&format!("{VIDEO},{APAC},{AAC}"));
        let info = parse_probe(&v, Path::new("x.mov")).unwrap();
        assert_eq!(audio_spec(Some(&info)), "0:a:1");
    }

    #[test]
    fn apac_only_clip_counts_as_silent() {
        let v = probe_json(&format!("{VIDEO},{APAC},{DATA}"));
        let info = parse_probe(&v, Path::new("x.mov")).unwrap();
        assert!(!info.has_audio);
    }

    #[test]
    fn libass_is_detected_by_its_filters() {
        let full = " .. ass  V->V  Render ASS subtitles using the libass library.\n .. subtitles V->V Render text subtitles.\n";
        assert!(flag_present(full, "subtitles") && flag_present(full, "ass"));
        let plain = " ..C scale V->V Scale.\n T.. drawtext V->V Draw text.\n";
        assert!(!flag_present(plain, "subtitles") && !flag_present(plain, "ass"));
    }

    #[test]
    fn subtitles_filter_escapes_awkward_paths() {
        let f = subtitles_filter(Path::new(
            "/Movies/Japan to Singapore 2026/it's: a,b[1].srt",
        ));
        assert!(
            f.starts_with(
                r"subtitles=filename=/Movies/Japan to Singapore 2026/it\\\'s\\: a\,b\[1\].srt:force_style="
            ),
            "{f}"
        );
        // Style commas are escaped for the filtergraph level.
        assert!(f.contains(r"FontName=Helvetica\,FontSize=11"), "{f}");
    }

    /// Burn an SRT through an awkward path with a libass ffmpeg and check
    /// pixels change at the bottom of the frame. Skips without libass.
    #[test]
    fn subtitles_burn_in_through_awkward_path() {
        let full = "/opt/homebrew/opt/ffmpeg-full/bin/ffmpeg";
        let bin = if Path::new(full).exists() {
            full
        } else {
            "ffmpeg"
        };
        let caps = probe_capabilities(bin, "ffprobe");
        if !caps.can_burn_captions() {
            eprintln!("no libass ffmpeg; skipping caption burn-in test");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("Japan to Singapore 2026 it's: [a],b");
        std::fs::create_dir_all(&dir).unwrap();
        let srt = dir.join("film.srt");
        std::fs::write(
            &srt,
            "1\n00:00:00,000 --> 00:00:02,000\nGolden Gate at sunset\n",
        )
        .unwrap();
        let run = |vf: &str| {
            let out = Command::new(bin)
                .args([
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=black:s=1920x1080:r=30:d=1",
                ])
                .args([
                    "-vf",
                    vf,
                    "-frames:v",
                    "1",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "gray",
                    "-",
                ])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            out.stdout
        };
        let bright = |px: &[u8]| px.iter().filter(|&&p| p > 128).count();
        let burned = run(&subtitles_filter(&srt));
        assert_eq!(burned.len(), 1920 * 1080);
        assert!(bright(&burned) > 500, "text pixels expected");
        // Text sits in the bottom half, above the lower-third band.
        assert_eq!(bright(&burned[..1920 * 540]), 0);
        assert_eq!(bright(&run("null")), 0);
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

    /// A stand-in `ffmpeg` script: logs its args, emits `-progress` lines,
    /// then creates the output named by its last argument.
    #[cfg(unix)]
    fn fake_ffmpeg(dir: &Path, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("fake-ffmpeg");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\nfor a; do last=\"$a\"; done\necho \"$@\" > '{}'\n{body}\n",
                dir.join("args.txt").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin.to_string_lossy().into_owned()
    }

    #[cfg(unix)]
    #[test]
    fn run_reports_progress_fractions_and_caps_threads() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_ffmpeg(
            dir.path(),
            "echo out_time_us=1000000; sleep 0.3; echo out_time_us=2000000; sleep 0.3; echo out_time_us=4000000; echo progress=end; sleep 0.1; : > \"$last\"",
        );
        let out = dir.path().join("o.mp4");
        let args = vec!["-y".to_string(), out.to_string_lossy().into_owned()];
        let seen = Mutex::new(vec![]);
        run_ffmpeg_with(
            &bin,
            &args,
            &out,
            &RunOpts {
                threads: Some(3),
                watch: Some((4.0, &|f| seen.lock().unwrap().push(f))),
                ..Default::default()
            },
        )
        .unwrap();
        let seen = seen.into_inner().unwrap();
        assert!(seen.len() >= 2, "{seen:?}");
        assert!(seen.windows(2).all(|w| w[0] <= w[1]));
        assert!(seen.iter().all(|f| (0.0..=1.0).contains(f)));
        assert!(*seen.last().unwrap() >= 0.5, "{seen:?}");
        assert!(out.exists() && !partial_sibling(&out).exists());
        let logged = std::fs::read_to_string(dir.path().join("args.txt")).unwrap();
        assert!(
            logged.contains("-threads 3 -filter_threads 3 -filter_complex_threads 3"),
            "{logged}"
        );
        assert!(logged.contains("-progress pipe:1"), "{logged}");
        assert!(
            logged.trim_end().ends_with(".partial.mp4"),
            "output is the partial: {logged}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_run_reports_stderr_and_removes_the_partial() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_ffmpeg(dir.path(), ": > \"$last\"; echo boom >&2; exit 3");
        let out = dir.path().join("o.mp4");
        let args = vec!["-y".to_string(), out.to_string_lossy().into_owned()];
        let res = run_ffmpeg_with(&bin, &args, &out, &RunOpts::default());
        let Err(crate::MediaError::Ffmpeg(msg)) = res else {
            panic!("{res:?}");
        };
        assert!(msg.contains("exit 3") && msg.contains("boom"), "{msg}");
        assert!(!out.exists() && !partial_sibling(&out).exists());
        let logged = std::fs::read_to_string(dir.path().join("args.txt")).unwrap();
        assert!(!logged.contains("-progress") && !logged.contains("-threads"));
    }

    #[cfg(unix)]
    #[test]
    fn cancel_kills_ffmpeg_and_removes_the_partial() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_ffmpeg(dir.path(), ": > \"$last\"; exec sleep 30");
        let out = dir.path().join("o.mp4");
        let args = vec!["-y".to_string(), out.to_string_lossy().into_owned()];
        let progress = crate::progress::Progress::default();
        let handle = progress.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            handle.cancel();
        });
        let started = std::time::Instant::now();
        let res = run_ffmpeg_with(
            &bin,
            &args,
            &out,
            &RunOpts {
                progress: Some(&progress),
                ..Default::default()
            },
        );
        canceller.join().unwrap();
        assert!(matches!(res, Err(crate::MediaError::Cancelled)), "{res:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(!out.exists() && !partial_sibling(&out).exists());
        // Already cancelled: ffmpeg is never started.
        let again = run_ffmpeg_with(
            "/nonexistent/ffmpeg",
            &args,
            &out,
            &RunOpts {
                progress: Some(&progress),
                ..Default::default()
            },
        );
        assert!(matches!(again, Err(crate::MediaError::Cancelled)));
    }
}
