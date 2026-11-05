//! Stage 5 — Score: per-asset quality and story signals.
//!
//! Heuristics on decoded thumbnail pixels (sharpness/blur/exposure) plus a
//! highlight score with per-signal reasons. Live Photo motion is scored on
//! audio energy/movement/faces only when a motion rendition exists — a
//! lively 3 s clip can outrank its still for video, never for print.

use crate::types::Score;

/// Luma statistics for one frame.
#[derive(Debug, Clone)]
pub struct FrameStats {
    /// Mean luma 0..1.
    pub mean: f32,
    /// Luma variance 0..1.
    pub variance: f32,
    /// Mean absolute neighbor difference (edge energy) 0..1.
    pub edge_energy: f32,
}

/// Compute luma stats from an 8-bit grayscale buffer.
pub fn frame_stats(gray: &[u8], width: u32, height: u32) -> Option<FrameStats> {
    let w = width as usize;
    let h = height as usize;
    let len = w.checked_mul(h)?;
    if len == 0 || gray.len() < len {
        return None;
    }
    // Only the declared frame area counts; trailing padding is ignored.
    let gray = &gray[..len];
    let n = len as f32;
    let mean = gray.iter().map(|&b| b as f32).sum::<f32>() / n / 255.0;
    let variance = gray
        .iter()
        .map(|&b| {
            let v = b as f32 / 255.0 - mean;
            v * v
        })
        .sum::<f32>()
        / n;
    // Edge energy: mean |right - pixel| + |down - pixel| over interior.
    let mut edge = 0.0f32;
    let mut count = 0u32;
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            let v = gray[i] as f32 / 255.0;
            if x + 1 < w {
                edge += (gray[i + 1] as f32 / 255.0 - v).abs();
                count += 1;
            }
            if y + 1 < h {
                edge += (gray[i + w] as f32 / 255.0 - v).abs();
                count += 1;
            }
        }
    }
    Some(FrameStats {
        mean,
        variance,
        edge_energy: if count == 0 { 0.0 } else { edge / count as f32 },
    })
}

/// Score one still frame. Pure math over stats — unit-testable.
pub fn score_still(stats: &FrameStats) -> Score {
    let mut reasons = vec![];
    // Sharpness from edge energy (calibrated so a crisp photo lands ~0.6+).
    let sharpness = (stats.edge_energy * 12.0).clamp(0.0, 1.0);
    if sharpness < 0.25 {
        reasons.push("soft/blurry".to_string());
    }
    let blur = (1.0 - sharpness).clamp(0.0, 1.0);
    // Exposure: punish clipped blacks/whites and very low contrast.
    let mut exposure = 1.0;
    if stats.mean < 0.08 {
        exposure = 0.3;
        reasons.push("severely underexposed".to_string());
    } else if stats.mean < 0.18 {
        exposure = 0.6;
        reasons.push("underexposed".to_string());
    } else if stats.mean > 0.92 {
        exposure = 0.3;
        reasons.push("severely overexposed".to_string());
    } else if stats.mean > 0.82 {
        exposure = 0.6;
        reasons.push("overexposed".to_string());
    }
    if stats.variance < 0.002 {
        exposure *= 0.5;
        reasons.push("flat/low-contrast".to_string());
    }
    if reasons.is_empty() {
        reasons.push("clean exposure, good detail".to_string());
    }
    let total = (sharpness * 0.55 + exposure * 0.45).clamp(0.0, 1.0);
    Score {
        sharpness,
        exposure,
        blur,
        total,
        highlight: false,
        reasons,
    }
}

/// Story signals that can promote an asset to a highlight.
#[derive(Debug, Clone, Default)]
pub struct StorySignals {
    pub face_count: u32,
    /// 0..1 speech/laughter/ambience energy (from motion audio).
    pub audio_energy: f32,
    /// 0..1 movement in the motion clip.
    pub movement: f32,
    /// 0..1 GPS novelty (rare place for this trip).
    pub gps_novelty: f32,
}

/// Highlight score: quality base plus story-signal bonuses, with reasons.
/// `for_print` disables motion-only signals (audio/movement) — a lively
/// clip never outranks its still for print.
pub fn highlight_score(base: &Score, signals: &StorySignals, for_print: bool) -> Score {
    let mut out = base.clone();
    let mut bonus = 0.0f32;
    if signals.face_count > 0 {
        bonus += 0.15 + 0.05 * (signals.face_count.min(4) as f32 - 1.0);
        out.reasons.push(format!("{} face(s)", signals.face_count));
    }
    if !for_print {
        if signals.audio_energy > 0.5 {
            bonus += 0.15;
            out.reasons.push("lively audio".to_string());
        }
        if signals.movement > 0.5 {
            bonus += 0.10;
            out.reasons.push("good movement".to_string());
        }
    }
    if signals.gps_novelty > 0.5 {
        bonus += 0.10;
        out.reasons.push("unique place".to_string());
    }
    out.total = (base.total + bonus).clamp(0.0, 1.0);
    out.highlight = out.total >= 0.65;
    out
}

/// CLI fallback for assets nothing could be scored from: a neutral total
/// that never highlights on its own.
pub fn fallback_score() -> Score {
    Score {
        total: 0.5,
        ..Default::default()
    }
}

/// Fixed CLI proxy score for videos: a neutral base plus a GPS-novelty
/// bonus, passed through [`highlight_score`] exactly like the CLI scan.
pub fn video_proxy_score(has_gps: bool) -> Score {
    let sig = StorySignals {
        gps_novelty: if has_gps { 0.6 } else { 0.0 },
        ..Default::default()
    };
    let base = Score {
        total: 0.55,
        sharpness: 0.5,
        exposure: 0.6,
        ..Default::default()
    };
    highlight_score(&base, &sig, false)
}

/// Score an already-decoded still: luma stats → [`score_still`] → default
/// story signals. Returns `None` when the pixels carry no frame stats.
pub fn score_still_image(img: &image::DynamicImage) -> Option<Score> {
    let gray = img.to_luma8();
    let stats = frame_stats(gray.as_raw(), gray.width(), gray.height())?;
    let base = score_still(&stats);
    Some(highlight_score(&base, &StorySignals::default(), false))
}

/// Score a still from a file on disk (a photo, or a cached thumbnail of
/// one). Returns `None` when the file cannot be decoded or carries no
/// frame stats; callers apply [`fallback_score`].
pub fn score_still_file(path: &std::path::Path) -> Option<Score> {
    score_still_image(&image::open(path).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(mean: f32, var: f32, edge: f32) -> FrameStats {
        FrameStats {
            mean,
            variance: var,
            edge_energy: edge,
        }
    }

    #[test]
    fn dark_frame_scores_poor_exposure() {
        let s = score_still(&stats(0.05, 0.01, 0.05));
        assert!(s.exposure < 0.5);
        assert!(s.reasons.iter().any(|r| r.contains("underexposed")));
    }

    #[test]
    fn blown_frame_scores_poor_exposure() {
        let s = score_still(&stats(0.95, 0.01, 0.05));
        assert!(s.exposure < 0.5);
    }

    #[test]
    fn sharp_clean_frame_scores_high() {
        let s = score_still(&stats(0.5, 0.02, 0.08));
        assert!(s.total > 0.7, "total={}", s.total);
        assert!(s.blur < 0.5);
    }

    #[test]
    fn blurry_frame_flagged() {
        let s = score_still(&stats(0.5, 0.02, 0.005));
        assert!(s.reasons.iter().any(|r| r.contains("blurry")));
    }

    #[test]
    fn faces_promote_highlight() {
        let base = score_still(&stats(0.5, 0.02, 0.06));
        let sig = StorySignals {
            face_count: 2,
            ..Default::default()
        };
        let h = highlight_score(&base, &sig, false);
        assert!(h.total > base.total);
        assert!(h.reasons.iter().any(|r| r.contains("face")));
    }

    #[test]
    fn motion_signals_ignored_for_print() {
        let base = score_still(&stats(0.5, 0.02, 0.06));
        let sig = StorySignals {
            audio_energy: 0.9,
            movement: 0.9,
            ..Default::default()
        };
        let print = highlight_score(&base, &sig, true);
        let video = highlight_score(&base, &sig, false);
        assert!(video.total > print.total);
        assert!((print.total - base.total).abs() < 1e-6);
    }

    #[test]
    fn frame_stats_on_real_buffer() {
        // 4x4 checkerboard: high edge energy, mid mean.
        let mut gray = vec![0u8; 16];
        for y in 0..4 {
            for x in 0..4 {
                gray[y * 4 + x] = if (x + y) % 2 == 0 { 0 } else { 255 };
            }
        }
        let st = frame_stats(&gray, 4, 4).unwrap();
        assert!((st.mean - 0.5).abs() < 1e-6);
        assert!(st.edge_energy > 0.4);
        // Trailing padding must not change the stats.
        let mut padded = gray.clone();
        padded.extend([255u8; 8]);
        let sp = frame_stats(&padded, 4, 4).unwrap();
        assert_eq!(sp.mean, st.mean);
        assert_eq!(sp.variance, st.variance);
        assert!(frame_stats(&gray, u32::MAX, u32::MAX).is_none());
    }

    fn checkerboard(size: u32) -> image::DynamicImage {
        let mut img = image::RgbImage::new(size, size);
        for y in 0..size {
            for x in 0..size {
                let v = if (x + y) % 2 == 0 { 200 } else { 80 };
                img.put_pixel(x, y, image::Rgb([v, v, v]));
            }
        }
        image::DynamicImage::ImageRgb8(img)
    }

    fn flat_dark(size: u32) -> image::DynamicImage {
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            size,
            size,
            image::Rgb([8, 8, 8]),
        ))
    }

    #[test]
    fn sharp_clean_still_beats_flat_dark_still() {
        let sharp = score_still_image(&checkerboard(32)).unwrap();
        let dark = score_still_image(&flat_dark(32)).unwrap();
        assert!(sharp.total > dark.total, "{sharp:?} vs {dark:?}");
        assert!(sharp.highlight, "{sharp:?}");
        assert!(!dark.highlight, "{dark:?}");
    }

    #[test]
    fn still_file_scores_saved_image_and_rejects_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let sharp_path = dir.path().join("sharp.png");
        checkerboard(32).save(&sharp_path).unwrap();
        let sharp = score_still_file(&sharp_path).unwrap();
        assert!(sharp.total > 0.65, "{sharp:?}");

        let dark_path = dir.path().join("dark.png");
        flat_dark(32).save(&dark_path).unwrap();
        let dark = score_still_file(&dark_path).unwrap();
        assert!(dark.total < sharp.total, "{dark:?} vs {sharp:?}");

        assert!(score_still_file(&dir.path().join("missing.png")).is_none());
        let junk = dir.path().join("junk.png");
        std::fs::write(&junk, b"not an image").unwrap();
        assert!(score_still_file(&junk).is_none());
    }

    #[test]
    fn video_proxy_matches_cli_constants() {
        // CLI: base total 0.55, GPS novelty 0.6 bonus 0.10, highlight >= 0.65.
        let plain = video_proxy_score(false);
        assert_eq!(plain.total, 0.55);
        assert!(!plain.highlight);
        let gps = video_proxy_score(true);
        assert!((gps.total - 0.65).abs() < 1e-6, "{gps:?}");
        assert!(gps.highlight);
        assert!(gps.reasons.iter().any(|r| r.contains("unique place")));
        assert_eq!(fallback_score().total, 0.5);
        assert!(!fallback_score().highlight);
    }
}
