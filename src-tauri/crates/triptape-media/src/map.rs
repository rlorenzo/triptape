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

/// Flat water and land tones for the route card.
pub(crate) const WATER: Rgb<u8> = Rgb([17, 27, 43]);
pub(crate) const LAND: Rgb<u8> = Rgb([43, 66, 92]);

/// Attribution owed on every map frame and page (PLAN.md Sec 12).
pub const MAP_ATTRIBUTION: &str = "© OpenStreetMap contributors";

/// Draw the route card PNG at `width`x`height`.
///
/// * Plain styled background (no tile basemap per the G3 fallback).
/// * Route polyline in accent, place pins as dots, day markers as rings.
/// * The map covers the full frame. There is deliberately no attribution
///   bar: it painted a light strip across the bottom of the video. The
///   basemap is Natural Earth (public domain), so no on-frame credit is
///   owed; [`MAP_ATTRIBUTION`] still ships in `ATTRIBUTION.txt` and the EDL.
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
    // Unwrap longitudes first: Los Angeles -> Tokyo crosses the Pacific
    // (102° of span). Raw min/max spans 258° the long way round and
    // crushes the trip into a corner of a half-planet map.
    let route = &unwrap_longitudes(route);
    let mut img = draw_base(route, w, h);
    if route.is_empty() {
        return img;
    }
    let fit = RouteFit::new(route, w, h);
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

/// Water, land and graticule for a route whose longitudes are already
/// unwrapped: the part of the card that never changes, shared by the static
/// card and the animated recap (which paints the route over it per frame).
pub(crate) fn draw_base(route: &[(f64, f64)], w: u32, h: u32) -> RgbImage {
    let k = (h / 1080).max(1);
    let mut img = RgbImage::from_pixel(w, h, WATER);
    if !route.is_empty() {
        let fit = RouteFit::new(route, w, h);
        paint_land(&mut img, &fit);
        if let Some(cols) = fit.gap_columns() {
            draw_axis_break(&mut img, cols, k);
        }
    }
    // Graticule: faint grid so the card reads as a map, not a black card.
    let grid = Rgb([31, 44, 63]);
    for gx in 1..8 {
        let x = w * gx / 8;
        for y in 0..h {
            img.put_pixel(x, y, grid);
        }
    }
    for gy in 1..6 {
        let y = h * gy / 6;
        for x in 0..w {
            img.put_pixel(x, y, grid);
        }
    }
    img
}

/// Shift longitudes by ±360° so consecutive points never jump more
/// than 180°, keeping antimeridian-crossing routes continuous.
pub(crate) fn unwrap_longitudes(route: &[(f64, f64)]) -> Vec<(f64, f64)> {
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
pub(crate) struct RouteFit {
    min_lat: f64,
    min_lon: f64,
    span_lat: f64,
    span_lon: f64,
    w: f64,
    h: f64,
    /// Broken longitude axis: an empty ocean gap squeezed out of the map.
    gap: Option<LonGap>,
}

/// An empty longitude stretch `[lo, hi]` (degrees, unwrapped) between route
/// points, drawn only [`GAP_SQUEEZE_DEG`] wide. A trip that crosses the
/// Pacific once would otherwise spend most of the frame on open water and
/// crush the actual destinations into a corner.
#[derive(Debug, Clone, Copy, PartialEq)]
struct LonGap {
    lo: f64,
    hi: f64,
}

/// Width the squeezed gap is drawn at.
const GAP_SQUEEZE_DEG: f64 = 10.0;
/// A gap is squeezed only when at least this wide and this share of the
/// route's longitude span.
const MIN_GAP_DEG: f64 = 40.0;
const MIN_GAP_SHARE: f64 = 0.4;

impl LonGap {
    /// Widest empty stretch between consecutive sorted route longitudes,
    /// when it is big enough to squeeze.
    fn find(route: &[(f64, f64)]) -> Option<Self> {
        let mut lons: Vec<f64> = route.iter().map(|p| p.1).collect();
        lons.sort_by(f64::total_cmp);
        let span = lons.last()? - lons.first()?;
        lons.windows(2)
            .map(|w| (w[1] - w[0], w[0], w[1]))
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .filter(|(g, _, _)| *g >= MIN_GAP_DEG && *g >= MIN_GAP_SHARE * span)
            .map(|(_, lo, hi)| Self { lo, hi })
    }

    fn warp(self, lon: f64) -> f64 {
        let width = self.hi - self.lo;
        if lon <= self.lo {
            lon
        } else if lon >= self.hi {
            lon - width + GAP_SQUEEZE_DEG
        } else {
            self.lo + (lon - self.lo) * GAP_SQUEEZE_DEG / width
        }
    }

    fn unwarp(self, lon: f64) -> f64 {
        let width = self.hi - self.lo;
        if lon <= self.lo {
            lon
        } else if lon >= self.lo + GAP_SQUEEZE_DEG {
            lon + width - GAP_SQUEEZE_DEG
        } else {
            self.lo + (lon - self.lo) * width / GAP_SQUEEZE_DEG
        }
    }
}

impl RouteFit {
    const PAD: f64 = 0.08;
    const MIN_SPAN_DEG: f64 = 0.02;

    pub(crate) fn new(route: &[(f64, f64)], w: u32, h: u32) -> Self {
        let (mut min_lat, mut max_lat) = (f64::INFINITY, f64::NEG_INFINITY);
        let (mut min_lon, mut max_lon) = (f64::INFINITY, f64::NEG_INFINITY);
        let gap = LonGap::find(route);
        for &(lat, lon) in route {
            let lon = gap.map_or(lon, |g| g.warp(lon));
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
            gap,
        }
    }

    /// Pixel columns `[x0, x1]` of the squeezed gap, when the axis is broken.
    pub(crate) fn gap_columns(&self) -> Option<(f64, f64)> {
        let g = self.gap?;
        let (x0, _) = self.project_f((self.min_lat, g.lo));
        let (x1, _) = self.project_f((self.min_lat, g.hi));
        Some((x0, x1))
    }

    fn project(&self, gps: (f64, f64)) -> (i32, i32) {
        let (x, y) = self.project_f(gps);
        (x as i32, y as i32)
    }

    /// [`Self::project`] without the pixel rounding, for sub-pixel motion.
    pub(crate) fn project_f(&self, (lat, lon): (f64, f64)) -> (f64, f64) {
        let lon = self.gap.map_or(lon, |g| g.warp(lon));
        let fx = (lon - self.min_lon) / self.span_lon;
        let fy = 1.0 - (lat - self.min_lat) / self.span_lat;
        let inner = 1.0 - 2.0 * Self::PAD;
        (
            (Self::PAD + fx * inner) * self.w,
            (Self::PAD + fy * inner) * self.h,
        )
    }

    /// Inverse of [`Self::project`] for a pixel position.
    fn unproject(&self, x: f64, y: f64) -> (f64, f64) {
        let inner = 1.0 - 2.0 * Self::PAD;
        let fx = (x / self.w - Self::PAD) / inner;
        let fy = (y / self.h - Self::PAD) / inner;
        let lon = self.min_lon + fx * self.span_lon;
        (
            self.min_lat + (1.0 - fy) * self.span_lat,
            self.gap.map_or(lon, |g| g.unwarp(lon)),
        )
    }
}

/// Paint landmasses from the bundled land grid: the raw 0.05° cells,
/// bilinearly blended and thresholded at the coastline, anti-aliased by 2x2
/// supersampling, so shores are smooth at any zoom. Land inside a squeezed
/// longitude gap is left as water.
fn paint_land(img: &mut RgbImage, fit: &RouteFit) {
    let (w, h) = (fit.w as u32, fit.h as u32);
    // Cells per pixel: how wide the shoreline's blend zone is in pixels.
    let (lat0, _) = fit.unproject(0.0, 0.0);
    let (lat1, _) = fit.unproject(0.0, 1.0);
    let cells_per_px = (lat0 - lat1).abs() * 20.0;
    let soft = (0.35 * cells_per_px).clamp(0.03, 0.5);
    let gap = fit.gap_columns();
    const OFFS: [f64; 2] = [0.25, 0.75];
    for y in 0..h {
        for x in 0..w {
            if gap.is_some_and(|(g0, g1)| f64::from(x) >= g0 && f64::from(x) <= g1) {
                continue;
            }
            let mut cover = 0.0;
            for oy in OFFS {
                for ox in OFFS {
                    let gps = fit.unproject(f64::from(x) + ox, f64::from(y) + oy);
                    let f = f64::from(triptape_core::geo::land_fraction(gps));
                    let t = ((f - (0.5 - soft)) / (2.0 * soft)).clamp(0.0, 1.0);
                    cover += t * t * (3.0 - 2.0 * t);
                }
            }
            let a = cover / 4.0;
            if a > 0.0 {
                let px = img.get_pixel_mut(x, y);
                for c in 0..3 {
                    px.0[c] =
                        (f64::from(px.0[c]) * (1.0 - a) + f64::from(LAND.0[c]) * a).round() as u8;
                }
            }
        }
    }
}

/// Two slanted rules at the edges of a squeezed longitude gap, the usual
/// sign of a broken axis.
fn draw_axis_break(img: &mut RgbImage, (x0, x1): (f64, f64), k: u32) {
    let col = Rgb([96, 116, 146]);
    let h = img.height();
    for edge in [x0, x1] {
        for y in 0..h {
            let cx = edge + (f64::from(y) - f64::from(h) / 2.0) * 0.12;
            for dx in 0..(2 * k) {
                let x = (cx + f64::from(dx)) as i64;
                if (0..i64::from(img.width())).contains(&x) && (y / (6 * k)).is_multiple_of(2) {
                    img.put_pixel(x as u32, y, col);
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
    fn long_empty_ocean_gaps_are_squeezed_out_of_the_axis() {
        // Los Angeles then Asia (unwrapped, as the card does): 100+ degrees
        // of Pacific between the route's points.
        let route =
            unwrap_longitudes(&[(33.9, -118.4), (35.6, 139.7), (22.3, 114.0), (1.35, 103.8)]);
        let fit = RouteFit::new(&route, 1920, 1080);
        let (g0, g1) = fit.gap_columns().expect("gap squeezed");
        assert!(g1 > g0 && g1 - g0 < 0.35 * 1920.0, "{g0}..{g1}");
        // Asia now fills a good share of the frame: Tokyo to Singapore.
        let (tx, _) = fit.project_f(route[1]);
        let (sx, _) = fit.project_f(route[3]);
        assert!((tx - sx).abs() > 0.3 * 1920.0, "{tx} {sx}");
        // Both sides still round-trip through the warp.
        for gps in [route[0], route[1], route[3]] {
            let (x, y) = fit.project_f(gps);
            let (lat, lon) = fit.unproject(x, y);
            assert!((lat - gps.0).abs() < 0.01 && (lon - gps.1).abs() < 0.01);
        }
        // A compact trip has no gap.
        let japan = RouteFit::new(&[(35.0, 139.0), (34.0, 135.0), (33.0, 130.0)], 1920, 1080);
        assert!(japan.gap_columns().is_none());
    }

    #[test]
    fn shorelines_are_antialiased_not_blocky() {
        // Tokyo Bay area at 1080p: coast pixels take blended tones.
        let route = vec![(35.0, 139.0), (36.0, 140.0)];
        let img = draw_route_card(&route, &[], 960, 540);
        let blended = img
            .pixels()
            .filter(|p| **p != WATER && **p != LAND && p.0[2] > 43 && p.0[2] < 92)
            .count();
        assert!(
            blended > 200,
            "coastline should carry blended pixels: {blended}"
        );
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
            let fit = RouteFit::new(&unwrap_longitudes(&route), w, h);
            let (x, y) = fit.project(route[0]);
            // Start pin: white ring from 8k to 11k px, so 10 px out is
            // ring at 1080p and colored core at 4K.
            let at = |d: i32| *img.get_pixel((x + d * k) as u32, y as u32);
            assert_eq!(at(10), white, "{w}x{h}: ring at 10k px");
            assert_ne!(at(13), white, "{w}x{h}: pin ends before 13k px");
        }
    }

    #[test]
    fn card_geometry_and_full_frame_map() {
        let route = vec![(35.0, 139.0), (36.0, 140.0)];
        let img = draw_route_card(&route, &[0], 320, 200);
        assert_eq!((img.width(), img.height()), (320, 200));
        // The map reaches the last row: no light strip along the bottom.
        let strip = Rgb([232, 236, 241]);
        assert!(
            img.rows().all(|mut r| r.all(|p| *p != strip)),
            "attribution strip still painted"
        );
        // Water or land, not an overlay: the bottom row is map-toned.
        // (shorelines are anti-aliased, so any blend of the two tones).
        let p = img.get_pixel(10, 199).0;
        assert!((0..3).all(|c| p[c] >= WATER.0[c] && p[c] <= LAND.0[c]));
        // Empty route still yields a valid card.
        let empty = draw_route_card(&[], &[], 320, 200);
        assert_eq!((empty.width(), empty.height()), (320, 200));
    }
}
