//! Title card PNG (text-free fallback).
//!
//! `drawtext` is absent on this build and no font may be assumed, so the
//! card is a styled solid: dark gradient, accent rule, and a soft frame.
//! The trip name is recorded in the EDL/filename, not rendered as pixels.

use image::{Rgb, RgbImage};
use std::path::Path;

pub fn save_title_card(
    _trip_name: &str,
    width: u32,
    height: u32,
    out: &Path,
) -> Result<(), crate::MediaError> {
    let (w, h) = (width.max(64), height.max(64));
    let mut img = RgbImage::new(w, h);
    for y in 0..h {
        let t = y as f32 / h as f32;
        let c = Rgb([
            (10.0 + 14.0 * t) as u8,
            (12.0 + 16.0 * t) as u8,
            (22.0 + 24.0 * t) as u8,
        ]);
        for x in 0..w {
            img.put_pixel(x, y, c);
        }
    }
    // Accent rule across the middle third + soft inner frame.
    let rule_y = h * 6 / 10;
    for y in rule_y..(rule_y + (h / 120).max(3)) {
        for x in w / 6..w * 5 / 6 {
            img.put_pixel(x, y, Rgb([245, 178, 66]));
        }
    }
    let m = (w.min(h) / 24).max(4);
    for x in m..w - m {
        for y in [m, h - m - 1] {
            img.put_pixel(x, y, Rgb([120, 130, 145]));
        }
    }
    for y in m..h - m {
        for x in [m, w - m - 1] {
            img.put_pixel(x, y, Rgb([120, 130, 145]));
        }
    }
    img.save(out)
        .map_err(|e| crate::MediaError::Io(format!("save title card: {e}")))?;
    Ok(())
}
