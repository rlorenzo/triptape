//! Journey map card: pure-Rust route rasterizer + zoompan push-in segment.
//!
//! PLAN.md Sec 12 hard rules: never scrape public tile servers, attribution
//! on every frame/page. The bundled-PMTiles basemap spike is unverified, so
//! this renderer draws the route + pins over a plain styled background (the
//! G3 fallback scope: no basemap, so no tile attribution is owed). The
//! OpenStreetMap attribution is still recorded in `ATTRIBUTION.txt` and in
//! the EDL/map metadata, and drawn as a solid attribution bar note.

use image::{Rgb, RgbImage};
use std::path::Path;
use triptape_core::geo::haversine_m;

/// Attribution owed on every map frame and page (PLAN.md Sec 12).
pub const MAP_ATTRIBUTION: &str = "© OpenStreetMap contributors";

/// Drop consecutive jumps over 1,000 km (GPS outliers, PLAN.md Sec 12).
/// The shared plan polyline carries no timestamps, so this is distance-only;
/// callers with timestamps should pre-filter time-qualified jumps.
pub fn drop_outliers(route: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut out: Vec<(f64, f64)> = vec![];
    for &p in route {
        if out.last().is_some_and(|&q| haversine_m(q, p) > 1_000_000.0) {
            continue;
        }
        out.push(p);
    }
    out
}

/// Draw the route card PNG at `width`x`height`.
///
/// * Plain styled background (no tile basemap per the G3 fallback).
/// * Route polyline in accent, place pins as dots, day markers as rings.
/// * Solid attribution bar along the bottom (text itself would need a
///   bundled font + drawtext/libass, both absent here; the attribution
///   string ships in `ATTRIBUTION.txt` and the EDL).
pub fn draw_route_card(
    route: &[(f64, f64)],
    day_markers: &[usize],
    width: u32,
    height: u32,
) -> RgbImage {
    let w = width.max(64);
    let h = height.max(64);
    let mut img = RgbImage::new(w, h);
    // Styled background: deep slate with a subtle vertical gradient.
    for y in 0..h {
        let t = y as f32 / h as f32;
        let c = Rgb([
            (16.0 + 10.0 * t) as u8,
            (24.0 + 12.0 * t) as u8,
            (38.0 + 16.0 * t) as u8,
        ]);
        for x in 0..w {
            img.put_pixel(x, y, c);
        }
    }
    // Attribution bar (bottom 4%): light strip so the note is visible.
    let bar_h = (h as f32 * 0.045).ceil() as u32;
    for y in (h - bar_h)..h {
        for x in 0..w {
            img.put_pixel(x, y, Rgb([232, 236, 241]));
        }
    }
    if route.is_empty() {
        return img;
    }
    let pts = project(route, w, h - bar_h);
    // Route line (accent amber), 3-px wide via neighbor splats.
    let accent = Rgb([245, 178, 66]);
    for win in pts.windows(2) {
        draw_seg(&mut img, win[0], win[1], accent);
    }
    // Pins: filled dots; day markers: larger rings.
    for (i, &p) in pts.iter().enumerate() {
        let r = if day_markers.contains(&i) { 7 } else { 4 };
        draw_dot(&mut img, p, r, Rgb([255, 255, 255]));
        draw_dot(
            &mut img,
            p,
            r.saturating_sub(2),
            if day_markers.contains(&i) {
                Rgb([245, 178, 66])
            } else {
                Rgb([94, 234, 212])
            },
        );
    }
    img
}

/// Fit lat/lon to the frame with 8% padding; equirectangular (fine for a
/// trip-scale card). Single-point routes collapse to a pin card.
fn project(route: &[(f64, f64)], w: u32, h: u32) -> Vec<(i32, i32)> {
    let (mut min_lat, mut max_lat) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut min_lon, mut max_lon) = (f64::INFINITY, f64::NEG_INFINITY);
    for &(lat, lon) in route {
        min_lat = min_lat.min(lat);
        max_lat = max_lat.max(lat);
        min_lon = min_lon.min(lon);
        max_lon = max_lon.max(lon);
    }
    let pad = 0.08;
    let span_lat = (max_lat - min_lat).max(0.02);
    let span_lon = (max_lon - min_lon).max(0.02);
    route
        .iter()
        .map(|&(lat, lon)| {
            let fx = (lon - min_lon) / span_lon;
            let fy = 1.0 - (lat - min_lat) / span_lat;
            let x = (pad + fx * (1.0 - 2.0 * pad)) * w as f64;
            let y = (pad + fy * (1.0 - 2.0 * pad)) * h as f64;
            (x as i32, y as i32)
        })
        .collect()
}

fn draw_dot(img: &mut RgbImage, c: (i32, i32), r: i32, col: Rgb<u8>) {
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy > r * r {
                continue;
            }
            let (x, y) = (c.0 + dx, c.1 + dy);
            if x >= 0 && y >= 0 && (x as u32) < img.width() && (y as u32) < img.height() {
                img.put_pixel(x as u32, y as u32, col);
            }
        }
    }
}

fn draw_seg(img: &mut RgbImage, a: (i32, i32), b: (i32, i32), col: Rgb<u8>) {
    let steps = ((b.0 - a.0).abs().max((b.1 - a.1).abs())).max(1);
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let x = (a.0 as f32 + (b.0 - a.0) as f32 * t) as i32;
        let y = (a.1 as f32 + (b.1 - a.1) as f32 * t) as i32;
        draw_dot(img, (x, y), 2, col);
    }
}

/// Save the route card PNG; returns the attribution note for logs/EDL.
pub fn save_route_card(
    route: &[(f64, f64)],
    day_markers: &[usize],
    width: u32,
    height: u32,
    out: &Path,
) -> Result<String, super::MediaError> {
    let clean = drop_outliers(route);
    let img = draw_route_card(&clean, day_markers, width, height);
    img.save(out)
        .map_err(|e| super::MediaError::Io(format!("save map card: {e}")))?;
    Ok(MAP_ATTRIBUTION.to_string())
}

/// Render the MVP map segment: slow push-in over the card via zoompan
/// (3 s class, 30 fps, 1080p with stereo silence for chain compatibility).
/// Falls back to a static hold when `zoompan` is absent.
pub fn render_map_push_in(
    caps: &super::ffmpeg::Capabilities,
    card_png: &Path,
    out_mp4: &Path,
    duration_secs: f64,
) -> Result<(), super::MediaError> {
    let dur = duration_secs.clamp(2.0, 5.0);
    let frames = (dur * 30.0).round() as u32;
    // Gentle push-in: zoom 1.00 -> 1.10 over the segment.
    let vf_video = if caps.zoompan {
        format!(
            "scale=1920:1080:force_original_aspect_ratio=increase,crop=1920:1080,zoompan=z='1+0.10*on/{frames}':d={frames}:s=1920x1080:fps=30,format=yuv420p"
        )
    } else {
        "scale=1920:1080:force_original_aspect_ratio=increase,crop=1920:1080,fps=30,format=yuv420p"
            .to_string()
    };
    let args: Vec<String> = [
        vec![
            "-y".into(),
            "-loop".into(),
            "1".into(),
            "-i".into(),
            card_png.to_string_lossy().into_owned(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            format!("anullsrc=r=48000:cl=stereo:d={dur}"),
            "-vf".into(),
            vf_video,
            "-frames:v".into(),
            frames.to_string(),
        ],
        super::ffmpeg::x264_aac("veryfast", 19),
        vec!["-shortest".into(), out_mp4.to_string_lossy().into_owned()],
    ]
    .concat();
    super::ffmpeg::run_ffmpeg(&caps.ffmpeg_bin, &args, out_mp4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outliers_dropped_single_pin_kept() {
        let route = vec![(35.0, 139.0), (35.1, 139.1), (-33.0, 151.0)];
        let clean = drop_outliers(&route);
        assert_eq!(clean.len(), 2);
        let single = vec![(35.0, 139.0)];
        assert_eq!(drop_outliers(&single), single, "pin card keeps its point");
        assert!(drop_outliers(&[]).is_empty());
    }

    #[test]
    fn card_geometry_and_bar() {
        let route = vec![(35.0, 139.0), (36.0, 140.0)];
        let img = draw_route_card(&route, &[0], 320, 200);
        assert_eq!((img.width(), img.height()), (320, 200));
        // Attribution bar: bottom row is the light strip.
        assert_eq!(*img.get_pixel(10, 199), Rgb([232, 236, 241]));
        // Empty route still yields a valid card.
        let empty = draw_route_card(&[], &[], 320, 200);
        assert_eq!((empty.width(), empty.height()), (320, 200));
    }
}
