//! Stage 4 — Proxies: 320 px JPEG thumbs + 720p H.264 proxies via FFmpeg.
//!
//! Generated lazily and cached by content hash. HEIC stills are decoded
//! through the bundled FFmpeg sidecar (the `image` crate cannot decode HEIC,
//! and OS codecs must never be required).

use std::path::{Path, PathBuf};

/// Thumbnail long-edge target (px).
pub const THUMB_LONG_EDGE: u32 = 320;
/// Proxy height target (720p).
pub const PROXY_HEIGHT: u32 = 720;

/// Content-addressed cache path: `<cache>/<kind>/<hash><suffix>`.
pub fn cache_path(cache_dir: &Path, kind: &str, content_hash: &str, suffix: &str) -> PathBuf {
    cache_dir.join(kind).join(format!("{content_hash}{suffix}"))
}

/// True when a cached rendition already exists and is non-empty.
pub fn cache_hit(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.len() > 0)
        .unwrap_or(false)
}

fn path_arg(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// `ffmpeg -y [pre_input...] -i <src> [opts...] <dst>`.
fn ffmpeg_args(pre_input: &[String], src: &Path, opts: &[String], dst: &Path) -> Vec<String> {
    let mut args = vec!["-y".to_string()];
    args.extend_from_slice(pre_input);
    args.extend(["-i".to_string(), path_arg(src)]);
    args.extend_from_slice(opts);
    args.push(path_arg(dst));
    args
}

fn strs(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| s.to_string()).collect()
}

/// FFmpeg args to decode any still (incl. HEIC) to a JPEG thumbnail.
/// Keeps aspect ratio; only downsizes (`min()` guard on the long edge).
pub fn thumb_args(src: &Path, dst: &Path) -> Vec<String> {
    // HEIC inputs reach the encoder through FFmpeg's internal tile-merge
    // filtergraph, which rejects a simple -vf chain ("Simple and complex
    // filtering cannot be used together"); -filter_complex works for both
    // stills and video first frames.
    let fc = format!(
        "scale='if(gt(iw,ih),min(iw,{e}),-2)':'if(gt(iw,ih),-2,min(ih,{e}))'",
        e = THUMB_LONG_EDGE
    );
    ffmpeg_args(
        &[],
        src,
        &strs(&["-filter_complex", &fc, "-frames:v", "1", "-q:v", "4"]),
        dst,
    )
}

/// FFmpeg args for a 720p H.264/AAC proxy (poster frames: see `poster_args`).
pub fn proxy_args(src: &Path, dst: &Path) -> Vec<String> {
    let vf = format!("scale=-2:{PROXY_HEIGHT}");
    let opts = strs(&[
        "-vf",
        &vf,
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-crf",
        "23",
        "-c:a",
        "aac",
        "-movflags",
        "+faststart",
    ]);
    ffmpeg_args(&[], src, &opts, dst)
}

/// FFmpeg args to extract a single poster frame at `at_secs` (e.g. ~1.5 s
/// into a Live Photo motion file whose still is missing).
pub fn poster_args(src: &Path, dst: &Path, at_secs: f64) -> Vec<String> {
    let seek = strs(&["-ss", &at_secs.to_string()]);
    ffmpeg_args(&seek, src, &strs(&["-frames:v", "1", "-q:v", "4"]), dst)
}

/// Unique sibling temp path that keeps `out`'s extension (FFmpeg picks the
/// muxer from it): `<dir>/.<stem>.tmp-<pid>-<n>.<ext>`.
fn temp_sibling(out: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let stem = out
        .file_stem()
        .map(|s| s.to_string_lossy())
        .unwrap_or_default();
    let ext = out
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    out.with_file_name(format!(".{stem}.tmp-{}-{n}{ext}", std::process::id()))
}

/// Run FFmpeg with the given args (whose last element must be `out`).
/// Skips work on cache hit. Renders to a temp sibling and atomically renames
/// it into place only after FFmpeg succeeds, so a cache file at `out` is
/// always a complete rendition; partial output is removed on failure.
pub fn run_ffmpeg(ffmpeg_bin: &str, args: &[String], out: &Path) -> Result<(), String> {
    if cache_hit(out) {
        return Ok(());
    }
    let Some((last, head)) = args.split_last() else {
        return Err("ffmpeg args are empty".to_string());
    };
    if *last != path_arg(out) {
        return Err("ffmpeg output arg does not match cache path".to_string());
    }
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = temp_sibling(out);
    let result = std::process::Command::new(ffmpeg_bin)
        .args(head)
        .arg(&tmp)
        .status()
        .map_err(|e| format!("ffmpeg spawn failed: {e}"))
        .and_then(|status| {
            if !status.success() {
                Err(format!("ffmpeg exited with {status}"))
            } else if !cache_hit(&tmp) {
                Err("ffmpeg produced empty output".to_string())
            } else {
                std::fs::rename(&tmp, out).map_err(|e| e.to_string())
            }
        });
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_paths_are_content_addressed() {
        let dir = Path::new("/cache");
        let a = cache_path(dir, "thumb", "abc123", ".jpg");
        assert_eq!(a, PathBuf::from("/cache/thumb/abc123.jpg"));
    }

    #[test]
    fn thumb_args_downscale_only() {
        let args = thumb_args(Path::new("in.HEIC"), Path::new("out.jpg"));
        let vf = args
            .iter()
            .skip_while(|a| *a != "-filter_complex")
            .nth(1)
            .unwrap();
        assert!(vf.contains("min(iw,320)"), "long edge capped at 320: {vf}");
        assert!(vf.contains("min(ih,320)"), "long edge capped at 320: {vf}");
        assert_eq!(args.first().map(String::as_str), Some("-y"));
        assert_eq!(args.last().map(String::as_str), Some("out.jpg"));
    }

    #[test]
    fn poster_args_seek_before_input() {
        let args = poster_args(Path::new("in.MOV"), Path::new("p.jpg"), 1.5);
        let pos = |s: &str| args.iter().position(|a| a == s).unwrap();
        assert!(pos("-ss") < pos("-i"), "input seek: {args:?}");
        assert_eq!(args[pos("-ss") + 1], "1.5");
        assert_eq!(args[pos("-i") + 1], "in.MOV");
        assert_eq!(args.last().map(String::as_str), Some("p.jpg"));
    }

    #[test]
    fn cache_hit_requires_nonempty_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.jpg");
        assert!(!cache_hit(&p));
        std::fs::write(&p, b"").unwrap();
        assert!(!cache_hit(&p));
        std::fs::write(&p, b"jpg").unwrap();
        assert!(cache_hit(&p));
    }

    #[test]
    fn temp_sibling_keeps_dir_and_extension() {
        let out = Path::new("/cache/thumb/abc.jpg");
        let a = temp_sibling(out);
        assert_eq!(a.parent(), out.parent());
        assert_eq!(a.extension().unwrap(), "jpg");
        assert_ne!(a, temp_sibling(out));
    }

    #[cfg(unix)]
    #[test]
    fn failed_ffmpeg_leaves_no_cache_entry() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("thumb").join("h.jpg");
        let args = thumb_args(Path::new("in.jpg"), &out);
        assert!(run_ffmpeg("false", &args, &out).is_err());
        assert!(!out.exists());
        let leftovers = std::fs::read_dir(out.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 0, "temp file must be cleaned up");
    }

    #[test]
    fn proxy_args_are_720p_h264() {
        let args = proxy_args(Path::new("in.MOV"), Path::new("out.mp4"));
        assert!(args.iter().any(|a| a == "libx264"));
        assert!(args.iter().any(|a| a.contains("720")));
    }
}
