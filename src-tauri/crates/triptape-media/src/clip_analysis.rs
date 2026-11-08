//! Ingest-time analysis of video clips: decode a low-res grayscale frame
//! and an audio RMS reading every [`SAMPLE_SECS`], score them with the pure
//! math in `triptape_core::clip_window`, and return the best window.
//!
//! Every failure (missing ffmpeg, undecodable clip, no audio track) degrades
//! quietly: no window means the plan keeps the start-of-clip trim.

use std::path::Path;
use std::process::{Command, Stdio};
use triptape_core::clip_window::{
    audio_rms_buckets, laplacian_variance, mean_abs_diff, mean_luma, pick_window, score_samples,
    wants_window, window_quality, RawSample, MIN_ANALYZED_SECS, SAMPLE_SECS,
};

/// Analysis frame width in pixels.
const FRAME_W: usize = 160;
/// Analysis frame height in pixels (aspect is not preserved; the metrics
/// are relative, and a fixed size keeps the raw stream trivially parseable).
const FRAME_H: usize = 90;
/// Audio sample rate for energy analysis.
const AUDIO_RATE: u32 = 8000;
/// Clips are analysed only up to this many seconds (bounds decode time).
const MAX_ANALYZED_SECS: f64 = 1200.0;

fn run_capture(ffmpeg_bin: &str, args: &[String]) -> Option<Vec<u8>> {
    let out = Command::new(ffmpeg_bin)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

/// ffmpeg args: `pre` are input options (before `-i`), `tail` output options.
fn input_args(path: &Path, pre: &[&str], tail: &[&str]) -> Vec<String> {
    let mut a: Vec<String> = ["-nostdin", "-v", "error", "-t"]
        .iter()
        .map(ToString::to_string)
        .collect();
    a.push(format!("{MAX_ANALYZED_SECS}"));
    a.extend(pre.iter().map(ToString::to_string));
    a.push("-i".into());
    a.push(crate::ffmpeg::path_arg(path));
    a.extend(tail.iter().map(ToString::to_string));
    a
}

/// Raw per-sample measurements for a clip, or `None` when no frames decode.
fn measure(ffmpeg_bin: &str, audio_map: Option<&str>, path: &Path) -> Option<Vec<RawSample>> {
    let vf = format!(
        "fps={},scale={FRAME_W}:{FRAME_H},format=gray",
        1.0 / SAMPLE_SECS
    );
    let rate = AUDIO_RATE.to_string();
    let video_args = input_args(
        path,
        // Keyframes only: ~1 s apart on phone video, far cheaper than
        // decoding every frame.
        &["-skip_frame", "nokey"],
        &[
            "-an", "-vf", &vf, "-f", "rawvideo", "-pix_fmt", "gray", "pipe:1",
        ],
    );
    // One explicit, probed stream: ffmpeg's best-stream pick can land on the
    // undecodable spatial `apple_apac` track. No usable audio: video only.
    let audio_args = audio_map.map(|a| {
        input_args(
            path,
            &[],
            &[
                "-vn", "-map", a, "-ac", "1", "-ar", &rate, "-f", "s16le", "pipe:1",
            ],
        )
    });
    // The two decodes are independent: run the audio one alongside.
    let (video, audio) = std::thread::scope(|s| {
        let audio = s.spawn(|| run_capture(ffmpeg_bin, audio_args.as_ref()?));
        let video = run_capture(ffmpeg_bin, &video_args);
        (video, audio.join().ok().flatten())
    });
    let video = video?;
    let frames: Vec<&[u8]> = video
        .as_chunks::<{ FRAME_W * FRAME_H }>()
        .0
        .iter()
        .map(|f| f.as_slice())
        .collect();
    if frames.is_empty() {
        return None;
    }
    let audio = audio
        .map(|pcm| {
            let samples: Vec<i16> = pcm
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes(*b))
                .collect();
            audio_rms_buckets(&samples, AUDIO_RATE, SAMPLE_SECS)
        })
        .filter(|b| !b.is_empty());
    Some(
        frames
            .iter()
            .enumerate()
            .map(|(i, f)| RawSample {
                sharpness: laplacian_variance(f, FRAME_W, FRAME_H).unwrap_or(0.0),
                mean: mean_luma(f).unwrap_or(0.5),
                diff: i.checked_sub(1).and_then(|p| mean_abs_diff(frames[p], f)),
                audio: audio.as_ref().map(|b| b.get(i).copied().unwrap_or(0.0)),
            })
            .collect(),
    )
}

/// What ingest learns about a video clip.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ClipAnalysis {
    /// Best `(start, end)` playback window in seconds; `None` when the clip
    /// plays whole.
    pub window: Option<(f64, f64)>,
    /// Mean sample score (0..1) over the part that will play; `None` when
    /// the clip was too short to analyse or analysis failed.
    pub quality: Option<f32>,
}

/// Analyse a clip once: best playback window plus the quality of what plays.
/// Clips at or under [`MIN_ANALYZED_SECS`] are not decoded and yield the
/// default (no window, no quality).
pub fn analyze_clip(
    ffmpeg_bin: &str,
    ffprobe_bin: &str,
    path: &Path,
    duration_secs: f64,
) -> ClipAnalysis {
    if !duration_secs.is_finite() || duration_secs <= MIN_ANALYZED_SECS {
        return ClipAnalysis::default();
    }
    let audio = crate::ffmpeg::audio_map(ffprobe_bin, path);
    let Some(raw) = measure(ffmpeg_bin, audio.as_deref(), path) else {
        return ClipAnalysis::default();
    };
    let scores = score_samples(&raw);
    let window = pick_window(&scores, duration_secs);
    ClipAnalysis {
        window,
        quality: window_quality(&scores, window),
    }
}

/// Best `(start, end)` playback window of a video clip, in seconds. `None`
/// when the clip is short enough to play whole or analysis failed.
pub fn best_clip_window(
    ffmpeg_bin: &str,
    ffprobe_bin: &str,
    path: &Path,
    duration_secs: f64,
) -> Option<(f64, f64)> {
    if !wants_window(duration_secs) {
        return None;
    }
    analyze_clip(ffmpeg_bin, ffprobe_bin, path, duration_secs).window
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_ffmpeg_or_file_yields_no_window() {
        let p = Path::new("/nonexistent/clip.mov");
        assert_eq!(
            best_clip_window("ffmpeg-does-not-exist", "ffprobe", p, 30.0),
            None
        );
        assert_eq!(best_clip_window("ffmpeg", "ffprobe", p, 30.0), None);
    }

    #[test]
    fn unanalysable_clips_report_no_window_and_no_quality() {
        let p = Path::new("/nonexistent/clip.mov");
        assert_eq!(
            analyze_clip("ffmpeg", "ffprobe", p, 30.0),
            ClipAnalysis::default()
        );
        assert_eq!(
            analyze_clip("ffmpeg", "ffprobe", p, 1.5),
            ClipAnalysis::default()
        );
        assert_eq!(
            analyze_clip("ffmpeg", "ffprobe", p, f64::NAN),
            ClipAnalysis::default()
        );
    }

    #[test]
    fn short_clips_skip_analysis() {
        let p = Path::new("/nonexistent/clip.mov");
        assert_eq!(best_clip_window("ffmpeg", "ffprobe", p, 1.5), None);
        assert_eq!(best_clip_window("ffmpeg", "ffprobe", p, f64::NAN), None);
    }
}
