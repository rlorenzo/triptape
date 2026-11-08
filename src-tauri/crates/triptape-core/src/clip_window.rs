//! Best-window picker for video clips: pure scoring math over low-res
//! grayscale samples and per-sample audio energy. No decoding happens here
//! (the media crate feeds samples in), so everything is deterministic and
//! unit-testable. No ML and no face detection, per PLAN.md privacy rules.

/// Seconds between analysis samples. Matches the ~1 s keyframe spacing of
/// phone video, so the media crate can decode keyframes only (~10x faster
/// than decoding every 4K frame).
pub const SAMPLE_SECS: f64 = 1.0;
/// Default length of the chosen window.
pub const TARGET_WINDOW_SECS: f64 = 6.0;
/// Hard ceiling on any clip trim (the film's per-clip clamp).
pub const MAX_WINDOW_SECS: f64 = 8.0;
const _: () = assert!(TARGET_WINDOW_SECS <= MAX_WINDOW_SECS);
/// Clips at or under this length are kept whole.
pub const MIN_ANALYZED_SECS: f64 = 2.0;
/// Opening and closing margin a window never touches (camera raise/lower).
pub const EDGE_SKIP_SECS: f64 = 0.7;

/// Weight of sharpness (Laplacian variance, normalised per clip).
const W_SHARP: f32 = 0.30;
/// Weight of exposure (penalises very dark or blown-out frames).
const W_EXPOSURE: f32 = 0.15;
/// Weight of steadiness (penalises whip pans and shake).
const W_STEADY: f32 = 0.25;
/// Weight of mild subject-motion reward.
const W_MOTION: f32 = 0.10;
/// Weight of audio energy (laughter, cheering); redistributed when absent.
const W_AUDIO: f32 = 0.20;
/// Frame-to-frame mean abs difference (0..1) below which a shot is steady.
const STEADY_DIFF: f32 = 0.12;
/// Extra difference over `STEADY_DIFF` at which steadiness reaches zero.
const STEADY_RANGE: f32 = 0.20;
/// Mean luma deviation from mid-grey that is still considered well exposed.
const EXPOSURE_TOLERANCE: f32 = 0.30;
/// Exposure score lost at full black or full white.
const EXPOSURE_MAX_PENALTY: f32 = 0.7;
/// Steady-shot difference at which the subject-motion reward saturates.
const MOTION_FULL_DIFF: f32 = 0.08;

/// Raw per-sample measurements fed to [`score_samples`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RawSample {
    /// Variance of the Laplacian (0..~1, higher is sharper).
    pub sharpness: f32,
    /// Mean luma 0..1.
    pub mean: f32,
    /// Mean abs difference from the previous sample 0..1 (`None` for the first).
    pub diff: Option<f32>,
    /// Audio RMS 0..1 for this sample's bucket (`None` when there is no audio).
    pub audio: Option<f32>,
}

/// Variance of the 4-neighbour Laplacian over the interior of an 8-bit
/// grayscale frame, with pixels scaled to 0..1. `None` for frames smaller
/// than 3x3 or with a short buffer.
pub fn laplacian_variance(gray: &[u8], width: usize, height: usize) -> Option<f32> {
    if width < 3 || height < 3 || gray.len() < width.checked_mul(height)? {
        return None;
    }
    let px = |x: usize, y: usize| f32::from(gray[y * width + x]) / 255.0;
    let (mut sum, mut sum_sq, mut n) = (0.0f64, 0.0f64, 0u32);
    for y in 1..height - 1 {
        for x in 1..width - 1 {
            let l = px(x - 1, y) + px(x + 1, y) + px(x, y - 1) + px(x, y + 1) - 4.0 * px(x, y);
            sum += f64::from(l);
            sum_sq += f64::from(l * l);
            n += 1;
        }
    }
    let n = f64::from(n);
    let mean = sum / n;
    Some((sum_sq / n - mean * mean).max(0.0) as f32)
}

/// Mean absolute luma difference (0..1) between two equally sized frames.
pub fn mean_abs_diff(a: &[u8], b: &[u8]) -> Option<f32> {
    if a.is_empty() || a.len() != b.len() {
        return None;
    }
    let total: u64 = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| u64::from(x.abs_diff(y)))
        .sum();
    Some(total as f32 / a.len() as f32 / 255.0)
}

/// Mean luma 0..1 of an 8-bit grayscale frame.
pub fn mean_luma(gray: &[u8]) -> Option<f32> {
    if gray.is_empty() {
        return None;
    }
    let total: u64 = gray.iter().map(|&b| u64::from(b)).sum();
    Some(total as f32 / gray.len() as f32 / 255.0)
}

/// RMS (0..1) of mono signed 16-bit audio per `bucket_secs` bucket.
pub fn audio_rms_buckets(samples: &[i16], sample_rate: u32, bucket_secs: f64) -> Vec<f32> {
    let per = ((f64::from(sample_rate) * bucket_secs) as usize).max(1);
    samples
        .chunks(per)
        .map(|c| {
            let sq: f64 = c.iter().map(|&s| (f64::from(s) / 32768.0).powi(2)).sum();
            (sq / c.len() as f64).sqrt() as f32
        })
        .collect()
}

/// Min-max normalise to 0..1; a flat series maps to a neutral 0.5.
fn normalize(values: &[f32]) -> Vec<f32> {
    let lo = values.iter().copied().fold(f32::INFINITY, f32::min);
    let hi = values.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    if (hi - lo).is_nan() || hi - lo <= 1e-6 {
        return vec![0.5; values.len()];
    }
    values.iter().map(|v| (v - lo) / (hi - lo)).collect()
}

/// Combine raw measurements into one 0..1 score per sample. Sharpness and
/// audio are normalised per clip; exposure, steadiness and motion use
/// absolute thresholds. Without audio its weight is spread over the rest.
pub fn score_samples(raw: &[RawSample]) -> Vec<f32> {
    // Sqrt compresses the heavy tail of Laplacian variance.
    let sharp = normalize(&raw.iter().map(|r| r.sharpness.sqrt()).collect::<Vec<_>>());
    let has_audio = raw.iter().any(|r| r.audio.is_some());
    let audio = normalize(
        &raw.iter()
            .map(|r| r.audio.unwrap_or(0.0))
            .collect::<Vec<_>>(),
    );
    let weight_sum = if has_audio { 1.0 } else { 1.0 - W_AUDIO };
    raw.iter()
        .enumerate()
        .map(|(i, r)| {
            let exposure = 1.0
                - EXPOSURE_MAX_PENALTY * ((r.mean - 0.5).abs() - EXPOSURE_TOLERANCE).max(0.0)
                    / (0.5 - EXPOSURE_TOLERANCE);
            // The first sample has no predecessor: borrow the next one's diff.
            let diff = r.diff.or_else(|| raw.get(i + 1).and_then(|n| n.diff));
            let (steady, motion) = match diff {
                Some(d) => (
                    1.0 - ((d - STEADY_DIFF) / STEADY_RANGE).clamp(0.0, 1.0),
                    if d <= STEADY_DIFF {
                        (d / MOTION_FULL_DIFF).min(1.0)
                    } else {
                        0.0
                    },
                ),
                None => (0.5, 0.0),
            };
            let mut s = W_SHARP * sharp[i]
                + W_EXPOSURE * exposure.clamp(0.0, 1.0)
                + W_STEADY * steady
                + W_MOTION * motion;
            if has_audio {
                s += W_AUDIO * audio[i];
            }
            s / weight_sum
        })
        .collect()
}

/// Window length for a clip: the target, never past the clip.
pub fn window_len(duration: f64) -> f64 {
    duration.min(TARGET_WINDOW_SECS)
}

/// Whether a clip is long enough for a window to differ from playing it
/// whole. Callers check this before paying for any decoding.
pub fn wants_window(duration: f64) -> bool {
    duration.is_finite() && duration > MIN_ANALYZED_SECS && duration - window_len(duration) >= 0.1
}

/// Pick the max-sum window of `window_len(duration)` over per-sample scores
/// (sample `i` starts at `i * SAMPLE_SECS`), keeping `EDGE_SKIP_SECS` clear
/// of both ends. Ties go to the earliest window. Returns `(start, end)` in
/// seconds, or `None` when the clip should play whole (short, or no
/// analysis available).
pub fn pick_window(scores: &[f32], duration: f64) -> Option<(f64, f64)> {
    if !wants_window(duration) {
        return None;
    }
    let len = window_len(duration);
    let n = (len / SAMPLE_SECS).round() as usize;
    if n == 0 || scores.len() < n {
        return None;
    }
    let first = (EDGE_SKIP_SECS / SAMPLE_SECS).ceil() as usize;
    let by_time = ((duration - EDGE_SKIP_SECS - len) / SAMPLE_SECS).floor();
    let last = (by_time.max(0.0) as usize).min(scores.len() - n);
    if by_time < first as f64 || last < first {
        // Too short for margins: centre the window instead.
        let start = ((duration - len) / 2.0).max(0.0);
        return Some((start, start + len));
    }
    let mut best = (first, f32::NEG_INFINITY);
    for s in first..=last {
        let sum: f32 = scores[s..s + n].iter().sum();
        if sum > best.1 {
            best = (s, sum);
        }
    }
    let start = best.0 as f64 * SAMPLE_SECS;
    Some((start, start + len))
}

/// Mean per-sample score over the part of the clip that will play: the
/// samples overlapping `window`, or every sample when the clip plays whole.
/// This is the clip's quality for story scoring. `None` without samples.
pub fn window_quality(scores: &[f32], window: Option<(f64, f64)>) -> Option<f32> {
    let span = match window {
        Some((start, end)) => {
            let from = (start / SAMPLE_SECS).floor().max(0.0) as usize;
            let to = ((end / SAMPLE_SECS).ceil() as usize).min(scores.len());
            scores.get(from..to)?
        }
        None => scores,
    };
    (!span.is_empty()).then(|| span.iter().sum::<f32>() / span.len() as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn laplacian_flat_is_zero_and_checkerboard_is_high() {
        assert_eq!(laplacian_variance(&[128; 25], 5, 5), Some(0.0));
        let checker: Vec<u8> = (0..25).map(|i| if i % 2 == 0 { 0 } else { 255 }).collect();
        // Interior pixels alternate +4 / -4, so the variance is ~16.
        assert!(laplacian_variance(&checker, 5, 5).unwrap() > 1.0);
        assert_eq!(laplacian_variance(&[0; 4], 2, 2), None);
        assert_eq!(laplacian_variance(&[0; 5], 3, 3), None);
    }

    #[test]
    fn diff_and_mean_helpers() {
        assert_eq!(mean_abs_diff(&[0, 0], &[255, 255]), Some(1.0));
        assert_eq!(mean_abs_diff(&[10, 20], &[10, 20]), Some(0.0));
        assert_eq!(mean_abs_diff(&[1], &[1, 2]), None);
        assert_eq!(mean_luma(&[0, 255]), Some(0.5));
        assert_eq!(mean_luma(&[]), None);
    }

    #[test]
    fn rms_buckets_split_by_time() {
        let mut s = vec![0i16; 8];
        s.extend(vec![16384i16; 8]);
        let b = audio_rms_buckets(&s, 16, 0.5);
        assert_eq!(b.len(), 2);
        assert_eq!(b[0], 0.0);
        assert!((b[1] - 0.5).abs() < 1e-6);
    }

    fn sample(sharp: f32, audio: Option<f32>) -> RawSample {
        RawSample {
            sharpness: sharp,
            mean: 0.5,
            diff: Some(0.05),
            audio,
        }
    }

    #[test]
    fn scoring_prefers_sharp_steady_well_exposed_loud() {
        let good = sample(0.4, Some(0.5));
        let blurry = sample(0.0, Some(0.0));
        let dark = RawSample { mean: 0.02, ..good };
        let shaky = RawSample {
            diff: Some(0.5),
            ..good
        };
        let s = score_samples(&[good, blurry, dark, shaky]);
        assert!(s[0] > s[1] && s[0] > s[2] && s[0] > s[3], "{s:?}");
    }

    #[test]
    fn scoring_without_audio_stays_in_range() {
        let s = score_samples(&[sample(0.1, None), sample(0.3, None)]);
        assert!(s.iter().all(|v| (0.0..=1.0).contains(v)), "{s:?}");
        assert!(s[1] > s[0]);
        assert!(score_samples(&[]).is_empty());
    }

    #[test]
    fn flat_scores_pick_earliest_valid_window() {
        // 30 s clip, 6 s window: earliest start honours the 0.7 s edge skip.
        let w = pick_window(&[0.5; 30], 30.0).unwrap();
        assert_eq!(w, (1.0, 7.0));
    }

    #[test]
    fn best_window_follows_the_peak() {
        let mut s = vec![0.1; 30];
        for v in &mut s[15..21] {
            *v = 0.9;
        }
        assert_eq!(pick_window(&s, 30.0), Some((15.0, 21.0)));
    }

    #[test]
    fn edges_are_never_selected() {
        let mut s = vec![0.1; 30];
        s[0] = 5.0; // great opening, but inside the skipped margin
        for v in &mut s[26..] {
            *v = 1.0; // great ending
        }
        let (start, end) = pick_window(&s, 30.0).unwrap();
        assert!(
            start >= EDGE_SKIP_SECS && end <= 30.0 - EDGE_SKIP_SECS,
            "{start} {end}"
        );
        // Last valid start: 30 - 0.7 - 6 = 23.3 s -> sample 23 -> 23.0 s.
        let tail: Vec<f32> = (0..30).map(|i| i as f32).collect();
        assert_eq!(pick_window(&tail, 30.0), Some((23.0, 29.0)));
    }

    #[test]
    fn short_and_whole_clips_are_untouched() {
        assert_eq!(pick_window(&[1.0; 2], 2.0), None);
        assert_eq!(pick_window(&[1.0; 5], 5.0), None, "shorter than window");
        assert_eq!(pick_window(&[], 30.0), None);
        assert_eq!(pick_window(&[1.0; 3], 30.0), None, "too few samples");
        assert_eq!(pick_window(&[1.0; 30], f64::NAN), None);
    }

    #[test]
    fn wants_window_only_for_clips_longer_than_the_window() {
        assert!(!wants_window(1.5));
        assert!(!wants_window(TARGET_WINDOW_SECS));
        assert!(!wants_window(TARGET_WINDOW_SECS + 0.05));
        assert!(wants_window(TARGET_WINDOW_SECS + 0.2));
        assert!(!wants_window(f64::INFINITY));
    }

    #[test]
    fn marginal_clip_centres_its_window() {
        // 6.4 s clip: no room for the margins around a 6 s window.
        let w = pick_window(&[0.5; 7], 6.4).unwrap();
        assert!(
            (w.0 - 0.2).abs() < 1e-9 && (w.1 - 6.2).abs() < 1e-9,
            "{w:?}"
        );
    }

    #[test]
    fn no_audio_path_still_picks_by_picture() {
        let mut raw: Vec<RawSample> = (0..20).map(|_| sample(0.01, None)).collect();
        for r in &mut raw[10..16] {
            r.sharpness = 0.5;
        }
        assert_eq!(pick_window(&score_samples(&raw), 20.0), Some((10.0, 16.0)));
    }

    #[test]
    fn window_quality_is_the_mean_of_the_played_samples() {
        let scores = [0.1, 0.1, 0.9, 0.9, 0.9, 0.9, 0.9, 0.9, 0.1, 0.1];
        let q = window_quality(&scores, Some((2.0, 8.0))).unwrap();
        assert!((q - 0.9).abs() < 1e-6);
        // Plays whole: every sample counts.
        let whole = window_quality(&scores, None).unwrap();
        assert!((whole - 0.58).abs() < 1e-6, "{whole}");
        // A window running past the samples is clipped, not an error.
        assert!(window_quality(&scores, Some((8.0, 14.0))).is_some());
        assert_eq!(window_quality(&scores, Some((20.0, 26.0))), None);
        assert_eq!(window_quality(&[], None), None);
    }
}
