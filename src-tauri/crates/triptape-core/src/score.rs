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

/// Colourfulness above this adds nothing in the chapter ranking, so
/// garish or oversaturated frames tie with merely vivid ones. "Extremely
/// colourful" on the Hasler-Susstrunk scale.
pub const COLOUR_CAP: f32 = 80.0;

/// Hasler-Susstrunk colourfulness of interleaved 8-bit RGB pixels:
/// `sqrt(σrg² + σyb²) + 0.3·sqrt(μrg² + μyb²)` with `rg = R - G` and
/// `yb = 0.5(R + G) - B`. About 0 for grey, 15 slightly, 33 averagely,
/// 59 highly colourful. `None` for an empty buffer.
pub fn colourfulness(rgb: &[u8]) -> Option<f32> {
    let n = rgb.len() / 3;
    if n == 0 {
        return None;
    }
    let (mut srg, mut syb, mut srg2, mut syb2) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for px in rgb.as_chunks::<3>().0 {
        let (r, g, b) = (px[0] as f64, px[1] as f64, px[2] as f64);
        let rg = r - g;
        let yb = 0.5 * (r + g) - b;
        srg += rg;
        syb += yb;
        srg2 += rg * rg;
        syb2 += yb * yb;
    }
    let n = n as f64;
    let (mrg, myb) = (srg / n, syb / n);
    let var_rg = (srg2 / n - mrg * mrg).max(0.0);
    let var_yb = (syb2 / n - myb * myb).max(0.0);
    Some(((var_rg + var_yb).sqrt() + 0.3 * (mrg * mrg + myb * myb).sqrt()) as f32)
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
        ..Default::default()
    }
}

/// Technical quality (sharpness/exposure, 0..1) is squeezed into
/// `TECH_FLOOR..TECH_FLOOR + TECH_SPAN` before story signals are added. On
/// real trips ~80% of stills land at 0.6..0.9 technically, so left at full
/// scale the sharpest-but-dullest frame outranked every photo of people.
/// Squeezed, the whole technical spread (~0.13) is smaller than the
/// smallest people bonus, so who and what is in the shot decides first and
/// sharpness breaks ties.
pub const TECH_FLOOR: f32 = 0.30;
pub const TECH_SPAN: f32 = 0.45;
/// `total` at or above this marks a story highlight.
pub const HIGHLIGHT_THRESHOLD: f32 = 0.65;
/// Score bonus by people tagged in the photo: a lone person is a portrait,
/// a pair is a moment shared, three or more is the whole group together.
/// Counts come from the photo library's own tags (count only).
pub const PEOPLE_BONUS: [f32; 3] = [0.10, 0.18, 0.28];
/// Bonus for a caption the user wrote themselves: they cared about it.
pub const CAPTION_BONUS: f32 = 0.10;
/// Bonus for a real video clip, so footage competes with stills for film
/// time (a clip also costs more screen time, which the plan accounts for).
pub const CLIP_BONUS: f32 = 0.08;
/// Technical quality assumed for a clip nothing could be analysed from.
pub const NEUTRAL_CLIP_QUALITY: f32 = 0.5;

/// Story signals that can promote an asset to a highlight.
#[derive(Debug, Clone, Default)]
pub struct StorySignals {
    /// People tagged in the photo (0 = none or unknown).
    pub people_count: u32,
    /// The user wrote a caption for this asset.
    pub has_caption: bool,
    /// The asset is a real video clip, not a still or Live Photo.
    pub is_clip: bool,
    /// 0..1 speech/laughter/ambience energy (from motion audio).
    pub audio_energy: f32,
    /// 0..1 movement in the motion clip.
    pub movement: f32,
    /// 0..1 GPS novelty (rare place for this trip).
    pub gps_novelty: f32,
}

impl StorySignals {
    /// Signals an ingested asset carries before any pixel analysis: its
    /// people tag count, caption, kind and (for clips) GPS.
    pub fn for_asset(a: &crate::types::Asset) -> Self {
        let is_clip = a.kind == crate::types::AssetKind::Video;
        Self {
            people_count: a.people_count.unwrap_or(0),
            has_caption: a.user_caption.is_some(),
            is_clip,
            gps_novelty: if is_clip && a.gps.is_some() { 0.6 } else { 0.0 },
            ..Default::default()
        }
    }
}

/// Bonus for `n` tagged people: [`PEOPLE_BONUS`], saturating at three.
pub fn people_bonus(n: u32) -> f32 {
    match n {
        0 => 0.0,
        n => PEOPLE_BONUS[(n as usize - 1).min(PEOPLE_BONUS.len() - 1)],
    }
}

/// Total story bonus for `signals`, with one reason string per signal.
/// `for_print` drops motion-only signals (audio/movement) and the clip
/// bonus: a lively clip never outranks its still for print.
fn signal_bonus(signals: &StorySignals, for_print: bool) -> (f32, Vec<String>) {
    let mut bonus = 0.0f32;
    let mut reasons = vec![];
    if signals.people_count > 0 {
        bonus += people_bonus(signals.people_count);
        reasons.push(match signals.people_count {
            1 => "1 person".to_string(),
            n => format!("{n} people"),
        });
    }
    if signals.has_caption {
        bonus += CAPTION_BONUS;
        reasons.push("your caption".to_string());
    }
    if !for_print {
        if signals.is_clip {
            bonus += CLIP_BONUS;
            reasons.push("video clip".to_string());
        }
        if signals.audio_energy > 0.5 {
            bonus += 0.15;
            reasons.push("lively audio".to_string());
        }
        if signals.movement > 0.5 {
            bonus += 0.10;
            reasons.push("good movement".to_string());
        }
    }
    if signals.gps_novelty > 0.5 {
        bonus += 0.10;
        reasons.push("unique place".to_string());
    }
    (bonus, reasons)
}

/// Map raw technical quality (0..1) onto the squeezed
/// `TECH_FLOOR..TECH_FLOOR + TECH_SPAN` band; see [`TECH_FLOOR`].
pub fn squeeze_quality(quality: f32) -> f32 {
    TECH_FLOOR + TECH_SPAN * quality.clamp(0.0, 1.0)
}

/// Highlight score: compressed technical quality plus story-signal
/// bonuses, with reasons. See [`TECH_FLOOR`] for why quality is squeezed.
pub fn highlight_score(base: &Score, signals: &StorySignals, for_print: bool) -> Score {
    let mut out = base.clone();
    let (bonus, reasons) = signal_bonus(signals, for_print);
    out.reasons.extend(reasons);
    let quality = squeeze_quality(base.total);
    out.bonus = bonus;
    out.quality = Some(quality);
    out.total = (quality + bonus).clamp(0.0, 1.0);
    out.highlight = out.total >= HIGHLIGHT_THRESHOLD;
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

/// [`fallback_score`] plus whatever story signals the asset carries: an
/// unscorable photo of five friends still beats an unscorable street.
pub fn fallback_score_with(signals: &StorySignals) -> Score {
    let (bonus, reasons) = signal_bonus(signals, false);
    let mut s = fallback_score();
    s.reasons = reasons;
    s.bonus = bonus;
    s.quality = Some(s.total);
    s.total = (s.total + bonus).clamp(0.0, 1.0);
    s.highlight = bonus > 0.0 && s.total >= HIGHLIGHT_THRESHOLD;
    s
}

/// Score for a video clip from its best playback window: `window_quality`
/// is the mean per-sample clip score (0..1) over the window the film will
/// play, or `None` when the clip could not be analysed (neutral quality).
/// `signals` carries people/caption/GPS; the clip bonus applies on top.
pub fn score_video(window_quality: Option<f32>, signals: &StorySignals) -> Score {
    let analysed = window_quality.is_some();
    let quality = window_quality.map_or(NEUTRAL_CLIP_QUALITY, |q| q.clamp(0.0, 1.0));
    let base = Score {
        total: quality,
        sharpness: quality,
        exposure: quality,
        reasons: vec![if analysed {
            "best-window clip quality".to_string()
        } else {
            "clip not analysed".to_string()
        }],
        ..Default::default()
    };
    let signals = StorySignals {
        is_clip: true,
        ..signals.clone()
    };
    highlight_score(&base, &signals, false)
}

/// Score an already-decoded still: luma stats → [`score_still`] → story
/// signals. Returns `None` when the pixels carry no frame stats.
pub fn score_still_image(img: &image::DynamicImage, signals: &StorySignals) -> Option<Score> {
    let gray = img.to_luma8();
    let stats = frame_stats(gray.as_raw(), gray.width(), gray.height())?;
    let mut base = score_still(&stats);
    // Borrow the pixels when already RGB8 rather than copying the frame.
    base.colourfulness = match img.as_rgb8() {
        Some(rgb) => colourfulness(rgb.as_raw()),
        None => colourfulness(img.to_rgb8().as_raw()),
    };
    Some(highlight_score(&base, signals, false))
}

/// Score a still from whatever pixels could be decoded for it (the photo
/// or a thumbnail of it): [`score_still_image`] when `pixels` carry frame
/// stats, else [`fallback_score_with`].
pub fn score_still_or_fallback(
    pixels: Option<&image::DynamicImage>,
    signals: &StorySignals,
) -> Score {
    pixels
        .and_then(|p| score_still_image(p, signals))
        .unwrap_or_else(|| fallback_score_with(signals))
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

    fn solid(rgb: [u8; 3]) -> Vec<u8> {
        rgb.iter().copied().cycle().take(3 * 64).collect()
    }

    #[test]
    fn grey_is_not_colourful() {
        assert!(colourfulness(&solid([128, 128, 128])).unwrap() < 1e-3);
        let ramp: Vec<u8> = (0..=255u8).flat_map(|v| [v, v, v]).collect();
        assert!(colourfulness(&ramp).unwrap() < 1e-3);
        assert_eq!(colourfulness(&[]), None);
    }

    #[test]
    fn saturated_mix_is_very_colourful() {
        let mut px = vec![];
        for c in [[255u8, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0]] {
            px.extend(solid(c));
        }
        assert!(colourfulness(&px).unwrap() > COLOUR_CAP);
        // A flat colour cast still counts, through the mean term.
        let cast = colourfulness(&solid([200, 120, 60])).unwrap();
        assert!(cast > 20.0, "cast={cast}");
    }

    #[test]
    fn still_image_records_colourfulness() {
        let grey =
            image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(8, 8, image::Rgb([90; 3])));
        let red = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            8,
            8,
            image::Rgb([220, 30, 30]),
        ));
        let sig = StorySignals::default();
        let g = score_still_image(&grey, &sig).unwrap();
        let r = score_still_image(&red, &sig).unwrap();
        assert!(g.colourfulness.unwrap() < 1e-3);
        assert!(r.colourfulness.unwrap() > 50.0);
        assert!(
            (g.total - r.total).abs() < 1e-6,
            "colour must not change the absolute total"
        );
        // Non-RGB8 frames are converted first and score the same.
        let rgba = image::DynamicImage::ImageRgba8(red.to_rgba8());
        let a = score_still_image(&rgba, &sig).unwrap();
        assert_eq!(a.colourfulness, r.colourfulness);
    }

    #[test]
    fn old_score_json_loads_without_colourfulness() {
        let old = r#"{"sharpness":0.5,"exposure":1.0,"blur":0.5,"total":0.6,"highlight":false,"reasons":[]}"#;
        let s: Score = serde_json::from_str(old).unwrap();
        assert_eq!(s.colourfulness, None);
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("colourfulness"));
        let mut s = s;
        s.colourfulness = Some(42.5);
        let back: Score = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.colourfulness, Some(42.5));
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

    fn people(n: u32) -> StorySignals {
        StorySignals {
            people_count: n,
            ..Default::default()
        }
    }

    #[test]
    fn people_bonus_grows_with_group_size_and_saturates() {
        assert_eq!(people_bonus(0), 0.0);
        let (one, two, three, ten) = (
            people_bonus(1),
            people_bonus(2),
            people_bonus(3),
            people_bonus(10),
        );
        assert!(0.0 < one && one < two && two < three, "{one} {two} {three}");
        assert_eq!(three, ten);
        let base = score_still(&stats(0.5, 0.02, 0.06));
        let plain = highlight_score(&base, &StorySignals::default(), false);
        let group = highlight_score(&base, &people(5), false);
        assert!(group.total > plain.total + 0.25);
        assert!(group.reasons.iter().any(|r| r.contains("5 people")));
        assert!((group.total - plain.total - group.bonus).abs() < 1e-6);
    }

    #[test]
    fn people_outrank_sharpness_but_sharpness_still_breaks_ties() {
        // The sharpest possible frame of nobody vs. a soft frame of a pair.
        let crisp = highlight_score(&score_still(&stats(0.5, 0.02, 0.2)), &people(0), false);
        let soft_pair = highlight_score(&score_still(&stats(0.5, 0.02, 0.03)), &people(2), false);
        assert!(soft_pair.total > crisp.total, "{soft_pair:?} vs {crisp:?}");
        // Same people: the sharper frame wins.
        let sharp_pair = highlight_score(&score_still(&stats(0.5, 0.02, 0.08)), &people(2), false);
        assert!(sharp_pair.total > soft_pair.total);
    }

    #[test]
    fn caption_adds_a_bonus() {
        let base = score_still(&stats(0.5, 0.02, 0.06));
        let plain = highlight_score(&base, &StorySignals::default(), false);
        let sig = StorySignals {
            has_caption: true,
            ..Default::default()
        };
        let cap = highlight_score(&base, &sig, false);
        assert!((cap.total - plain.total - CAPTION_BONUS).abs() < 1e-6);
        assert!(cap.reasons.iter().any(|r| r.contains("caption")));
        // Captions and people still count for print.
        assert!((highlight_score(&base, &sig, true).total - cap.total).abs() < 1e-6);
    }

    #[test]
    fn motion_signals_ignored_for_print() {
        let base = score_still(&stats(0.5, 0.02, 0.06));
        let sig = StorySignals {
            audio_energy: 0.9,
            movement: 0.9,
            is_clip: true,
            ..Default::default()
        };
        let print = highlight_score(&base, &sig, true);
        let video = highlight_score(&base, &sig, false);
        let none = highlight_score(&base, &StorySignals::default(), false);
        assert!(video.total > print.total);
        assert!((print.total - none.total).abs() < 1e-6);
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
        let sharp = score_still_image(&checkerboard(32), &StorySignals::default()).unwrap();
        let dark = score_still_image(&flat_dark(32), &StorySignals::default()).unwrap();
        assert!(sharp.total > dark.total, "{sharp:?} vs {dark:?}");
        assert!(sharp.highlight, "{sharp:?}");
        assert!(!dark.highlight, "{dark:?}");
    }

    #[test]
    fn squeeze_quality_maps_into_the_tech_band() {
        assert_eq!(squeeze_quality(0.0), TECH_FLOOR);
        assert_eq!(squeeze_quality(1.0), TECH_FLOOR + TECH_SPAN);
        assert_eq!(squeeze_quality(-1.0), TECH_FLOOR);
        assert_eq!(squeeze_quality(2.0), TECH_FLOOR + TECH_SPAN);
    }

    #[test]
    fn still_or_fallback_scores_pixels_and_falls_back_without_them() {
        let people = StorySignals {
            people_count: 2,
            ..Default::default()
        };
        let sharp = score_still_or_fallback(Some(&checkerboard(32)), &people);
        assert_eq!(
            sharp,
            score_still_image(&checkerboard(32), &people).unwrap()
        );
        assert_eq!(
            score_still_or_fallback(None, &people),
            fallback_score_with(&people)
        );
    }

    #[test]
    fn video_score_follows_window_quality_and_signals() {
        let none = StorySignals::default();
        let lo = score_video(Some(0.2), &none);
        let hi = score_video(Some(0.9), &none);
        assert!(hi.total > lo.total + 0.2, "{hi:?} vs {lo:?}");
        assert!(hi.reasons.iter().any(|r| r.contains("video clip")));
        // Unanalysed clips get a neutral quality, never a highlight alone.
        let unknown = score_video(None, &none);
        assert!(!unknown.highlight, "{unknown:?}");
        assert!(unknown.total > lo.total && unknown.total < hi.total);
        // People, captions and GPS lift a clip like any other asset.
        let sig = StorySignals {
            people_count: 3,
            has_caption: true,
            gps_novelty: 0.6,
            ..Default::default()
        };
        let rich = score_video(Some(0.5), &sig);
        assert!(rich.highlight, "{rich:?}");
        assert!(rich.total > score_video(Some(0.5), &none).total + 0.3);
        assert!(rich.reasons.iter().any(|r| r.contains("unique place")));
    }

    #[test]
    fn signals_for_asset_read_people_caption_and_kind() {
        use crate::types::{Asset, AssetKind};
        let mut photo = Asset::new("p", "/t/p.heic".into(), AssetKind::Photo);
        photo.people_count = Some(2);
        photo.user_caption = Some("Dinner".into());
        photo.gps = Some((1.0, 2.0));
        let s = StorySignals::for_asset(&photo);
        assert_eq!((s.people_count, s.has_caption, s.is_clip), (2, true, false));
        assert_eq!(s.gps_novelty, 0.0, "stills keep the old no-GPS-bonus rule");
        let mut clip = Asset::new("v", "/t/v.mov".into(), AssetKind::Video);
        clip.gps = Some((1.0, 2.0));
        let s = StorySignals::for_asset(&clip);
        assert!(s.is_clip && s.gps_novelty > 0.5);
    }

    #[test]
    fn fallback_is_neutral_unless_signals_say_otherwise() {
        assert_eq!(fallback_score().total, 0.5);
        assert!(!fallback_score().highlight);
        let plain = fallback_score_with(&StorySignals::default());
        assert_eq!(plain.total, 0.5);
        assert!(!plain.highlight);
        let group = fallback_score_with(&people(4));
        assert!(group.total > 0.75 && group.highlight, "{group:?}");
    }
}
