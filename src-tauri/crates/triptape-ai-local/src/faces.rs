//! Face and smile signals: YuNet detection plus FER+ expression, decoded
//! into the three numbers core stores per photo ([`FaceSignals`]).
//!
//! Spike findings applied (`docs/spikes/face-embedding-models.md`):
//! - YuNet also finds posters, statues and dolls, so the face count is a
//!   "plausible faces" count for scoring, never a people count; tagged
//!   people stay the people signal.
//! - Expression is unreliable on small faces: FER+ only runs on faces at
//!   least [`MIN_SMILE_FACE_PX`] wide in the analysed thumbnail, and each
//!   smile is weighted by the face's size ([`FULL_WEIGHT_FACE_PX`]).
//!
//! Nothing here keeps boxes or crops: the analysis returns counts and
//! scores only, which is all scoring needs.

use image::{imageops::FilterType, DynamicImage, RgbImage};
use triptape_core::types::FaceSignals;

/// YuNet confidence floor (the spike's 0.7).
pub const FACE_SCORE_MIN: f32 = 0.7;
/// Overlap above which two detections are one face.
const NMS_IOU: f32 = 0.3;
/// YuNet runs with the long edge at this size (padded to /32), as measured
/// in the spike; a 320 px thumbnail is upscaled 2x so small faces are found.
pub const YUNET_LONG_EDGE: u32 = 640;
/// Smallest face (box side, in analysed-thumbnail pixels) whose expression
/// is read. FER+ was trained on 48 px faces; below about 40 px the spike
/// saw unreliable expressions.
pub const MIN_SMILE_FACE_PX: f32 = 40.0;
/// Face side (thumbnail pixels) at which a smile counts in full; smaller
/// gated faces count in proportion.
pub const FULL_WEIGHT_FACE_PX: f32 = 80.0;
/// Smallest box side (thumbnail pixels) counted as a plausible face at all.
pub const MIN_FACE_PX: f32 = 8.0;
/// FER+ class order; index 1 is happiness.
const FER_HAPPY: usize = 1;
const FER_CLASSES: usize = 8;
/// Context around the face box for the FER+ crop (the spike's 25 %).
const CROP_PAD: f32 = 0.25;

/// A detected face box in thumbnail pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Face {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
    pub score: f32,
}

impl Face {
    /// Longer side of the box.
    pub fn side(&self) -> f32 {
        (self.x2 - self.x1).max(self.y2 - self.y1)
    }
}

fn iou(a: &Face, b: &Face) -> f32 {
    let iw = (a.x2.min(b.x2) - a.x1.max(b.x1)).max(0.0);
    let ih = (a.y2.min(b.y2) - a.y1.max(b.y1)).max(0.0);
    let i = iw * ih;
    let u = (a.x2 - a.x1) * (a.y2 - a.y1) + (b.x2 - b.x1) * (b.y2 - b.y1) - i;
    if u <= 0.0 {
        0.0
    } else {
        i / u
    }
}

/// YuNet input for an RGB image: (BGR 0..255 planar tensor, shape
/// `[1, 3, h, w]`, scale from thumbnail to tensor pixels).
pub fn yunet_input(img: &RgbImage) -> (Vec<f32>, [i64; 4], f32) {
    yunet_input_at(img, YUNET_LONG_EDGE)
}

/// [`yunet_input`] at a chosen long edge: larger finds smaller faces (a
/// toddler in a stroller at the back of a group) at more compute.
pub fn yunet_input_at(img: &RgbImage, long_edge: u32) -> (Vec<f32>, [i64; 4], f32) {
    let (w, h) = (img.width().max(1) as f32, img.height().max(1) as f32);
    let sc = long_edge as f32 / w.max(h);
    let rw = ((w * sc).round() as u32).max(1);
    let rh = ((h * sc).round() as u32).max(1);
    let (pw, ph) = (rw.div_ceil(32) * 32, rh.div_ceil(32) * 32);
    let r = image::imageops::resize(img, rw, rh, FilterType::Triangle);
    let plane = (pw * ph) as usize;
    let mut data = vec![0f32; 3 * plane];
    for (x, y, p) in r.enumerate_pixels() {
        let i = (y * pw + x) as usize;
        data[i] = f32::from(p[2]);
        data[plane + i] = f32::from(p[1]);
        data[2 * plane + i] = f32::from(p[0]);
    }
    (data, [1, 3, i64::from(ph), i64::from(pw)], sc)
}

/// Decode YuNet's twelve outputs (cls, obj, bbox, kps for strides 8, 16,
/// 32, in that order) for a `width`-pixel-wide padded input into faces in
/// thumbnail pixels (`scale` = tensor / thumbnail), confidence-filtered and
/// non-max suppressed, best first. `None` on unexpected output sizes.
pub fn yunet_decode(outs: &[Vec<f32>], width: u32, height: u32, scale: f32) -> Option<Vec<Face>> {
    if outs.len() < 9 || scale <= 0.0 {
        return None;
    }
    let mut faces = Vec::new();
    for (si, stride) in [8u32, 16, 32].into_iter().enumerate() {
        let (cols, rows) = ((width / stride) as usize, (height / stride) as usize);
        let (cls, obj, bb) = (&outs[si], &outs[3 + si], &outs[6 + si]);
        let n = cols * rows;
        if cls.len() != n || obj.len() != n || bb.len() != 4 * n {
            return None;
        }
        let s = stride as f32;
        for i in 0..n {
            let score = (cls[i].clamp(0.0, 1.0) * obj[i].clamp(0.0, 1.0)).sqrt();
            if score < FACE_SCORE_MIN {
                continue;
            }
            let (r, c) = ((i / cols) as f32, (i % cols) as f32);
            let cx = (c + bb[i * 4]) * s;
            let cy = (r + bb[i * 4 + 1]) * s;
            let bw = bb[i * 4 + 2].exp() * s;
            let bh = bb[i * 4 + 3].exp() * s;
            faces.push(Face {
                x1: (cx - bw / 2.0) / scale,
                y1: (cy - bh / 2.0) / scale,
                x2: (cx + bw / 2.0) / scale,
                y2: (cy + bh / 2.0) / scale,
                score,
            });
        }
    }
    faces.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut keep: Vec<Face> = Vec::new();
    for f in faces {
        if keep.iter().all(|k| iou(k, &f) < NMS_IOU) {
            keep.push(f);
        }
    }
    Some(keep)
}

/// FER+ input for one face: a square crop padded by [`CROP_PAD`], grey,
/// 64x64, 0..255.
pub fn ferplus_input(img: &RgbImage, f: &Face) -> Vec<f32> {
    let (cx, cy) = ((f.x1 + f.x2) / 2.0, (f.y1 + f.y2) / 2.0);
    let side = f.side() * (1.0 + CROP_PAD);
    let x0 = (cx - side / 2.0).max(0.0) as u32;
    let y0 = (cy - side / 2.0).max(0.0) as u32;
    let x1 = ((cx + side / 2.0).max(0.0) as u32).min(img.width());
    let y1 = ((cy + side / 2.0).max(0.0) as u32).min(img.height());
    let crop = image::imageops::crop_imm(
        img,
        x0.min(img.width().saturating_sub(1)),
        y0.min(img.height().saturating_sub(1)),
        x1.saturating_sub(x0).max(1),
        y1.saturating_sub(y0).max(1),
    )
    .to_image();
    DynamicImage::ImageRgb8(crop)
        .grayscale()
        .resize_exact(64, 64, FilterType::Triangle)
        .to_luma8()
        .pixels()
        .map(|p| f32::from(p[0]))
        .collect()
}

/// Happiness probability (0..1) from FER+'s eight logits. `None` on an
/// unexpected output size.
pub fn ferplus_happiness(logits: &[f32]) -> Option<f32> {
    if logits.len() != FER_CLASSES || logits.iter().any(|x| !x.is_finite()) {
        return None;
    }
    let m = logits.iter().copied().fold(f32::MIN, f32::max);
    let e: Vec<f32> = logits.iter().map(|x| (x - m).exp()).collect();
    let sum: f32 = e.iter().sum();
    Some(e[FER_HAPPY] / sum)
}

/// Faces big enough for FER+ to read.
pub fn smile_gated(faces: &[Face]) -> impl Iterator<Item = &Face> {
    faces.iter().filter(|f| f.side() >= MIN_SMILE_FACE_PX)
}

/// Fold detections and per-face happiness into the stored signals:
/// plausible faces (at least [`MIN_FACE_PX`]), the largest face's side as
/// a fraction of the thumbnail's shorter side, and the best size-weighted
/// smile among gated faces (`happy` pairs with [`smile_gated`] order;
/// `None` entries are faces FER+ could not read). `None` smile when no face
/// was big enough to read.
pub fn fold_signals(faces: &[Face], happy: &[Option<f32>], width: u32, height: u32) -> FaceSignals {
    let plausible: Vec<&Face> = faces.iter().filter(|f| f.side() >= MIN_FACE_PX).collect();
    let short = width.min(height).max(1) as f32;
    let largest = plausible
        .iter()
        .map(|f| (f.side() / short).clamp(0.0, 1.0))
        .fold(0.0f32, f32::max);
    let smile = smile_gated(faces)
        .zip(happy)
        .filter_map(|(f, h)| {
            let w = (f.side() / FULL_WEIGHT_FACE_PX).clamp(0.0, 1.0);
            h.map(|h| h.clamp(0.0, 1.0) * w)
        })
        .reduce(f32::max);
    FaceSignals {
        count: plausible.len() as u32,
        largest,
        smile,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn face(x: f32, y: f32, side: f32, score: f32) -> Face {
        Face {
            x1: x,
            y1: y,
            x2: x + side,
            y2: y + side,
            score,
        }
    }

    /// Synthetic YuNet outputs for a `w`x`h` input with one strong anchor
    /// at stride 8, cell (row 2, col 3), box 4x4 cells (32 px) centred.
    fn synthetic(w: u32, h: u32) -> Vec<Vec<f32>> {
        let mut outs = vec![];
        let n = |s: u32| ((w / s) * (h / s)) as usize;
        for s in [8, 16, 32] {
            outs.push(vec![0.0; n(s)]); // cls
        }
        for s in [8, 16, 32] {
            outs.push(vec![0.0; n(s)]); // obj
        }
        for s in [8, 16, 32] {
            outs.push(vec![0.0; 4 * n(s)]); // bbox
        }
        for s in [8, 16, 32] {
            outs.push(vec![0.0; 10 * n(s)]); // kps
        }
        let cols = (w / 8) as usize;
        let i = 2 * cols + 3;
        outs[0][i] = 0.9;
        outs[3][i] = 0.9;
        outs[6][i * 4 + 2] = 4f32.ln();
        outs[6][i * 4 + 3] = 4f32.ln();
        // A weaker duplicate one cell over (suppressed) and a sub-floor one.
        outs[0][i + 1] = 0.85;
        outs[3][i + 1] = 0.85;
        outs[6][(i + 1) * 4 + 2] = 4f32.ln();
        outs[6][(i + 1) * 4 + 3] = 4f32.ln();
        outs[0][i + 20] = 0.5;
        outs[3][i + 20] = 0.5;
        outs
    }

    #[test]
    fn yunet_decode_places_scales_and_suppresses() {
        let faces = yunet_decode(&synthetic(64, 64), 64, 64, 2.0).unwrap();
        assert_eq!(faces.len(), 1, "{faces:?}");
        let f = faces[0];
        // Centre (3*8, 2*8) = (24, 16) in tensor px, 32 px box, scale 2.
        assert!(
            (f.x1 - 4.0).abs() < 1e-4 && (f.y1 - 0.0).abs() < 1e-4,
            "{f:?}"
        );
        assert!((f.side() - 16.0).abs() < 1e-4);
        assert!((f.score - 0.9).abs() < 1e-6);
        assert_eq!(
            yunet_decode(&synthetic(64, 64), 64, 32, 2.0),
            None,
            "wrong size"
        );
        assert_eq!(yunet_decode(&[], 64, 64, 1.0), None);
    }

    #[test]
    fn yunet_input_is_bgr_and_padded() {
        let img = RgbImage::from_pixel(320, 200, image::Rgb([10, 20, 30]));
        let (d, shape, sc) = yunet_input(&img);
        assert_eq!(shape, [1, 3, 416, 640]);
        assert!((sc - 2.0).abs() < 1e-6);
        let plane = 416 * 640;
        assert_eq!((d[0], d[plane], d[2 * plane]), (30.0, 20.0, 10.0));
        // Padding rows stay zero.
        assert_eq!(d[plane - 1], 0.0);
    }

    #[test]
    fn ferplus_softmax_reads_happiness() {
        let mut logits = [0.0f32; 8];
        logits[FER_HAPPY] = 10.0;
        assert!(ferplus_happiness(&logits).unwrap() > 0.99);
        let neutral = [5.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        assert!(ferplus_happiness(&neutral).unwrap() < 0.01);
        assert_eq!(ferplus_happiness(&[1.0; 7]), None);
        assert_eq!(ferplus_happiness(&[f32::NAN; 8]), None);
    }

    #[test]
    fn ferplus_input_is_64x64_grey() {
        let img = RgbImage::from_pixel(100, 80, image::Rgb([200, 200, 200]));
        let d = ferplus_input(&img, &face(90.0, 70.0, 40.0, 0.9));
        assert_eq!(d.len(), 64 * 64);
        assert!(d.iter().all(|&v| (v - 200.0).abs() < 1.5));
    }

    #[test]
    fn small_faces_count_but_never_smile() {
        let faces = [face(0.0, 0.0, 20.0, 0.9), face(50.0, 0.0, 6.0, 0.9)];
        assert_eq!(smile_gated(&faces).count(), 0);
        let s = fold_signals(&faces, &[], 320, 240);
        assert_eq!(s.count, 1, "the 6 px blob is not a plausible face");
        assert!((s.largest - 20.0 / 240.0).abs() < 1e-6);
        assert_eq!(s.smile, None);
    }

    #[test]
    fn smiles_weigh_by_face_size() {
        let big = face(0.0, 0.0, 120.0, 0.9);
        let small = face(150.0, 0.0, 40.0, 0.9);
        let faces = [big, small];
        // Small face grinning (1.0 * 40/80 = 0.5), big face mildly (0.6).
        let s = fold_signals(&faces, &[Some(0.6), Some(1.0)], 320, 240);
        assert_eq!(s.count, 2);
        assert!((s.largest - 0.5).abs() < 1e-6);
        assert!((s.smile.unwrap() - 0.6).abs() < 1e-6);
        // Unreadable crops do not count as frowns.
        let s = fold_signals(&faces, &[None, Some(1.0)], 320, 240);
        assert!((s.smile.unwrap() - 0.5).abs() < 1e-6);
        assert_eq!(fold_signals(&[], &[], 320, 240), FaceSignals::default());
    }
}
