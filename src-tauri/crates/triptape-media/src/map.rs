//! Journey map card: pure-Rust route rasterizer + push-in segment.
//!
//! PLAN.md Sec 12 hard rules: never scrape public tile servers, attribution
//! on every frame/page. The bundled-PMTiles basemap spike is unverified, so
//! this renderer draws the route + pins over landmasses rasterized from
//! the bundled Natural Earth land grid (public domain; the same grid that
//! suppresses place names on open water). No tile basemap, so no tile
//! attribution is owed. The OpenStreetMap attribution is still recorded
//! in `ATTRIBUTION.txt` and in the EDL/map metadata, and drawn as a solid
//! attribution bar note.

use crate::resolution::Resolution;
use image::{Rgb, RgbImage};
use std::path::Path;
use triptape_core::geo::is_land;

/// Flat water and land tones for the route card.
pub(crate) const WATER: Rgb<u8> = Rgb([17, 27, 43]);
pub(crate) const LAND: Rgb<u8> = Rgb([43, 66, 92]);

/// Attribution owed on every map frame and page (PLAN.md Sec 12).
pub const MAP_ATTRIBUTION: &str = "© OpenStreetMap contributors";

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
    // Strokes, pins and land blocks are drawn in 1080p pixels; larger cards
    // (4K) multiply them so the card looks the same, just sharper.
    let k = (h / 1080).max(1) as i32;
    let mut img = RgbImage::from_pixel(w, h, WATER);
    let bar_h = (h as f32 * 0.045).ceil() as u32;
    // Unwrap longitudes first: Los Angeles -> Tokyo crosses the Pacific
    // (102° of span). Raw min/max spans 258° the long way round and
    // crushes the trip into a corner of a half-planet map.
    let route = &unwrap_longitudes(route);
    if !route.is_empty() {
        paint_land(&mut img, &RouteFit::new(route, w, h - bar_h), k as u32);
    }
    // Attribution bar (bottom 4%): light strip so the note is visible.
    for y in (h - bar_h)..h {
        for x in 0..w {
            img.put_pixel(x, y, Rgb([232, 236, 241]));
        }
    }
    // Graticule: faint grid so the card reads as a map, not a black card.
    let grid = Rgb([31, 44, 63]);
    for gx in 1..8 {
        let x = w * gx / 8;
        for y in 0..(h - bar_h) {
            img.put_pixel(x, y, grid);
        }
    }
    for gy in 1..6 {
        let y = (h - bar_h) * gy / 6;
        for x in 0..w {
            img.put_pixel(x, y, grid);
        }
    }
    if route.is_empty() {
        return img;
    }
    let fit = RouteFit::new(route, w, h - bar_h);
    let pts: Vec<(i32, i32)> = route.iter().map(|&gps| fit.project(gps)).collect();
    // Route line: dark amber glow under the bright accent core.
    for (col, r) in [(Rgb([146, 102, 30]), 4 * k), (Rgb([245, 178, 66]), 2 * k)] {
        for win in pts.windows(2) {
            draw_seg(&mut img, win[0], win[1], col, r);
        }
    }
    // Pins: filled dots; day markers: larger rings.
    for (i, &p) in pts.iter().enumerate() {
        let r = if day_markers.contains(&i) {
            7 * k
        } else {
            4 * k
        };
        draw_dot(&mut img, p, r, Rgb([255, 255, 255]));
        draw_dot(
            &mut img,
            p,
            r.saturating_sub(2 * k),
            if day_markers.contains(&i) {
                Rgb([245, 178, 66])
            } else {
                Rgb([94, 234, 212])
            },
        );
    }
    // Start (green) and end (red) pins, larger than day markers.
    if let (Some(&first), Some(&last)) = (pts.first(), pts.last()) {
        for (p, col) in [(first, Rgb([52, 199, 123])), (last, Rgb([235, 87, 87]))] {
            draw_dot(&mut img, p, 11 * k, Rgb([255, 255, 255]));
            draw_dot(&mut img, p, 8 * k, col);
        }
    }
    img
}

/// Shift longitudes by ±360° so consecutive points never jump more
/// than 180°, keeping antimeridian-crossing routes continuous.
fn unwrap_longitudes(route: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut out: Vec<(f64, f64)> = Vec::with_capacity(route.len());
    for &(lat, lon) in route {
        let lon = match out.last() {
            Some(&(_, prev)) => prev + (lon - prev + 180.0).rem_euclid(360.0) - 180.0,
            None => lon,
        };
        out.push((lat, lon));
    }
    out
}

/// Equirectangular fit of a route's bounding box into a `w`x`h` plot with
/// 8% padding (fine for a trip-scale card). Single-point routes collapse
/// to a pin card. Route, pins, and land all share this one mapping, so
/// they cannot drift apart.
struct RouteFit {
    min_lat: f64,
    min_lon: f64,
    span_lat: f64,
    span_lon: f64,
    w: f64,
    h: f64,
}

impl RouteFit {
    const PAD: f64 = 0.08;
    const MIN_SPAN_DEG: f64 = 0.02;

    fn new(route: &[(f64, f64)], w: u32, h: u32) -> Self {
        let (mut min_lat, mut max_lat) = (f64::INFINITY, f64::NEG_INFINITY);
        let (mut min_lon, mut max_lon) = (f64::INFINITY, f64::NEG_INFINITY);
        for &(lat, lon) in route {
            min_lat = min_lat.min(lat);
            max_lat = max_lat.max(lat);
            min_lon = min_lon.min(lon);
            max_lon = max_lon.max(lon);
        }
        Self {
            min_lat,
            min_lon,
            span_lat: (max_lat - min_lat).max(Self::MIN_SPAN_DEG),
            span_lon: (max_lon - min_lon).max(Self::MIN_SPAN_DEG),
            w: f64::from(w),
            h: f64::from(h),
        }
    }

    fn project(&self, (lat, lon): (f64, f64)) -> (i32, i32) {
        let fx = (lon - self.min_lon) / self.span_lon;
        let fy = 1.0 - (lat - self.min_lat) / self.span_lat;
        let inner = 1.0 - 2.0 * Self::PAD;
        (
            ((Self::PAD + fx * inner) * self.w) as i32,
            ((Self::PAD + fy * inner) * self.h) as i32,
        )
    }

    /// Inverse of [`Self::project`] for a pixel position.
    fn unproject(&self, x: f64, y: f64) -> (f64, f64) {
        let inner = 1.0 - 2.0 * Self::PAD;
        let fx = (x / self.w - Self::PAD) / inner;
        let fy = (y / self.h - Self::PAD) / inner;
        (
            self.min_lat + (1.0 - fy) * self.span_lat,
            self.min_lon + fx * self.span_lon,
        )
    }
}

/// Paint landmasses from the bundled land grid in small blocks, so the
/// card reads as a map of the region.
fn paint_land(img: &mut RgbImage, fit: &RouteFit, k: u32) {
    let step = 4 * k;
    let (w, plot_h) = (fit.w as u32, fit.h as u32);
    for by in (0..plot_h).step_by(step as usize) {
        for bx in (0..w).step_by(step as usize) {
            let center = fit.unproject(f64::from(bx + step / 2), f64::from(by + step / 2));
            if !is_land(center) {
                continue;
            }
            for y in by..(by + step).min(plot_h) {
                for x in bx..(bx + step).min(w) {
                    img.put_pixel(x, y, LAND);
                }
            }
        }
    }
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

fn draw_seg(img: &mut RgbImage, a: (i32, i32), b: (i32, i32), col: Rgb<u8>, r: i32) {
    let steps = ((b.0 - a.0).abs().max((b.1 - a.1).abs())).max(1);
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let x = (a.0 as f32 + (b.0 - a.0) as f32 * t) as i32;
        let y = (a.1 as f32 + (b.1 - a.1) as f32 * t) as i32;
        draw_dot(img, (x, y), r, col);
    }
}

/// Save the route card PNG; returns the attribution note for logs/EDL.
/// `route` is drawn as given: GPS outliers need capture times to tell them
/// from real long-haul legs, so the plan filters them when it builds the
/// polyline.
pub fn save_route_card(
    route: &[(f64, f64)],
    day_markers: &[usize],
    width: u32,
    height: u32,
    out: &Path,
) -> Result<String, super::MediaError> {
    let img = draw_route_card(route, day_markers, width, height);
    img.save(out)
        .map_err(|e| super::MediaError::Io(format!("save map card: {e}")))?;
    Ok(MAP_ATTRIBUTION.to_string())
}

/// Render the MVP map segment: slow glide over the card (3 s class,
/// 30 fps, at `res` with stereo silence for chain compatibility). `variant`
/// picks the glide direction (0 push-in, 1 pull-out for the closing
/// recap). Never zoompan: its integer-step re-cropping jitters.
pub fn render_map_push_in(
    caps: &super::ffmpeg::Capabilities,
    card_png: &Path,
    out_mp4: &Path,
    duration_secs: f64,
    variant: usize,
    res: Resolution,
) -> Result<(), super::MediaError> {
    let dur = duration_secs.clamp(2.0, 6.0);
    let frames = (dur * 30.0).round() as u32;
    let (w, h) = res.dims();
    let (sw, sh) = res.supersample();
    let vf_video = format!(
        "scale={sw}:{sh}:flags=lanczos,{},{},format=yuv420p",
        super::ffmpeg::hold_still(frames),
        super::ffmpeg::glide(caps, frames, w, h, variant)
    );
    let args: Vec<String> = [
        vec![
            "-y".into(),
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
    caps.run(&args, out_mp4)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pixels painted in the land and water tones.
    fn land_water_px(img: &RgbImage) -> (usize, usize) {
        let count = |tone: Rgb<u8>| img.pixels().filter(|&&p| p == tone).count();
        (count(LAND), count(WATER))
    }

    #[test]
    fn route_fit_round_trips_pixels_to_gps() {
        let fit = RouteFit::new(&[(35.0, 139.0), (36.0, 141.0)], 1000, 500);
        for gps in [(35.0, 139.0), (35.5, 140.0), (36.0, 141.0)] {
            let (x, y) = fit.project(gps);
            let (lat, lon) = fit.unproject(f64::from(x), f64::from(y));
            assert!((lat - gps.0).abs() < 0.01, "{lat} vs {}", gps.0);
            assert!((lon - gps.1).abs() < 0.01, "{lon} vs {}", gps.1);
        }
    }

    #[test]
    fn card_shows_landmass_not_black_card() {
        // Tokyo -> Singapore: Japan, Taiwan, and SE-Asia landmasses must
        // be visible against the water, not a plain dark card.
        let route = vec![(35.6, 139.7), (25.0, 121.5), (1.35, 103.8)];
        let (land, water) = land_water_px(&draw_route_card(&route, &[], 960, 540));
        assert!(land > 20_000, "landmass missing: {land} land px");
        assert!(water > 20_000, "water missing: {water} water px");
    }

    #[test]
    fn transpacific_route_keeps_pacific_water() {
        // Los Angeles -> Tokyo -> Hong Kong -> Singapore crosses the
        // antimeridian: the frame must be mostly Pacific water with
        // Asia at one side, not a half-planet land belt.
        let route = vec![(33.94, -118.4), (35.6, 139.7), (22.3, 114.0), (1.35, 103.8)];
        let (land, water) = land_water_px(&draw_route_card(&route, &[], 960, 540));
        assert!(land > 5_000, "coasts missing: {land}");
        assert!(
            water > land,
            "Pacific should dominate: water {water} vs land {land}"
        );
    }

    #[test]
    fn saved_card_keeps_long_haul_legs() {
        // The export path: every 1,000+ km leg must reach the card.
        let route = vec![(33.94, -118.4), (35.6, 139.7), (22.3, 114.0), (1.35, 103.8)];
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("card.png");
        save_route_card(&route, &[], 960, 540, &out).unwrap();
        let saved = image::open(&out).unwrap().to_rgb8();
        assert!(saved == draw_route_card(&route, &[], 960, 540));
    }

    #[test]
    fn unwrap_longitudes_shifts_only_across_the_antimeridian() {
        fn lons(route: &[(f64, f64)]) -> Vec<i32> {
            let unwrapped = unwrap_longitudes(route);
            assert_eq!(unwrapped.len(), route.len());
            for (u, r) in unwrapped.iter().zip(route) {
                assert_eq!(u.0, r.0, "latitude must pass through");
            }
            unwrapped.iter().map(|p| p.1.round() as i32).collect()
        }
        // Same-hemisphere hops and empty routes pass through untouched.
        assert_eq!(
            lons(&[(35.0, 139.0), (36.0, 141.0), (1.35, 104.0)]),
            [139, 141, 104]
        );
        assert!(lons(&[]).is_empty());
        // Fiji hops: eastbound lands past +180, westbound past -180, and
        // the return trip comes back to the raw longitude.
        assert_eq!(
            lons(&[(-17.0, 179.0), (-17.0, -179.0), (-17.0, 179.0)]),
            [179, 181, 179]
        );
        assert_eq!(lons(&[(-17.0, -179.0), (-17.0, 179.0)]), [-179, -181]);
        // Los Angeles -> Tokyo takes the 102° Pacific way, not 258°.
        assert_eq!(lons(&[(33.94, -118.0), (35.6, 140.0)]), [-118, -220]);
    }

    #[test]
    fn card_strokes_scale_with_4k_canvas() {
        // A near-vertical route keeps the line clear of the pin's sides.
        let route = vec![(35.0, 139.0), (36.0, 139.05)];
        let white = Rgb([255, 255, 255]);
        for (w, h, k) in [(1920, 1080, 1), (3840, 2160, 2)] {
            let img = draw_route_card(&route, &[], w, h);
            let fit = RouteFit::new(
                &unwrap_longitudes(&route),
                w,
                h - (h as f32 * 0.045).ceil() as u32,
            );
            let (x, y) = fit.project(route[0]);
            // Start pin: white ring from 8k to 11k px, so 10 px out is
            // ring at 1080p and colored core at 4K.
            let at = |d: i32| *img.get_pixel((x + d * k) as u32, y as u32);
            assert_eq!(at(10), white, "{w}x{h}: ring at 10k px");
            assert_ne!(at(13), white, "{w}x{h}: pin ends before 13k px");
        }
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
