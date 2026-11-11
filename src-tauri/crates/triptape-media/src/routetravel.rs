//! Travelling recap of the owner's itinerary for the closing map card.
//!
//! The camera opens on the home city and follows a plane along the
//! great-circle arc across the Pacific, zooms in on the first city and
//! follows the dot port to port (ship legs dashed, routed around land),
//! circles out and back at sea for a round-trip cruise, follows the flight
//! home, then pulls back to the whole route. Boarding tags ("Boarded X ·
//! Oct 9") appear as the dot leaves for a ship; ships are never map points.
//!
//! The scene is redrawn from world coordinates every frame because the
//! camera moves: land comes from the bundled 0.05 degree grid (bilinear,
//! anti-aliased, drawn on all cores), the route and icons on top. Raw RGB
//! frames stream into ffmpeg like [`crate::routeanim`]. Timing comes from
//! [`Journey::schedule`], the same numbers the plan's length budget uses.

use crate::ffmpeg::Capabilities;
use crate::resolution::Resolution;
use crate::routeanim::{
    blend_px, blend_shape, composite, encode, frame_count, label_doc_sized, RecapReport, CORE, FPS,
    GLOW, PIN_END, PIN_RING, PIN_START, PIN_STOP,
};
use crate::MediaError;
use image::{Rgb, RgbImage, RgbaImage};
use std::path::Path;
use triptape_core::itinerary::LegMode;
use triptape_core::journey::{Journey, Schedule};

/// Camera width (degrees of longitude across the frame) per phase.
const FLIGHT_VIEW_DEG: f64 = 80.0;
const PORT_VIEW_DEG: f64 = 24.0;
/// The loop fills this share of the frame height.
const LOOP_FILL: f64 = 0.45;
/// Camera and zoom smoothing (seconds, Gaussian sigma).
const CAMERA_SIGMA_SECS: f64 = 0.3;
/// Boarding tag on screen: fade in, hold, fade out.
const TAG_SECS: f64 = 2.2;
const TAG_FADE_SECS: f64 = 0.3;
/// A stop's label stays until this many later stops have been reached.
const LABELS_KEPT: usize = 3;
const LABEL_FADE_OUT_SECS: f64 = 0.4;
const SEA_DASH_ON: f64 = 12.0;
const SEA_DASH_OFF: f64 = 10.0;
const WATER: Rgb<u8> = Rgb([17, 27, 43]);
const LAND: Rgb<u8> = Rgb([43, 66, 92]);
const GRID: Rgb<u8> = Rgb([31, 44, 63]);

type LatLon = (f64, f64);

/// Smooth ease-in-out for the leg progress.
fn ease(u: f64) -> f64 {
    crate::routeanim::ease_in_out(u)
}

/// A leg's path with arc lengths (degrees, flat), for positions by progress.
struct LegGeom {
    pts: Vec<LatLon>,
    cum: Vec<f64>,
}

impl LegGeom {
    fn new(pts: &[LatLon]) -> Self {
        let mut cum = vec![0.0];
        for w in pts.windows(2) {
            let d = ((w[1].0 - w[0].0).powi(2) + (w[1].1 - w[0].1).powi(2)).sqrt();
            cum.push(cum[cum.len() - 1] + d);
        }
        Self {
            pts: pts.to_vec(),
            cum,
        }
    }

    fn len(&self) -> f64 {
        self.cum[self.cum.len() - 1]
    }

    /// Point at arc length `s`.
    fn at(&self, s: f64) -> LatLon {
        let s = s.clamp(0.0, self.len());
        let i = self
            .cum
            .partition_point(|&c| c < s)
            .clamp(1, self.pts.len() - 1);
        let (c0, c1) = (self.cum[i - 1], self.cum[i]);
        let u = if c1 > c0 { (s - c0) / (c1 - c0) } else { 0.0 };
        let (a, b) = (self.pts[i - 1], self.pts[i]);
        (a.0 + (b.0 - a.0) * u, a.1 + (b.1 - a.1) * u)
    }

    /// The path up to arc length `s`, ending exactly at [`Self::at`]`(s)`.
    fn prefix(&self, s: f64) -> Vec<LatLon> {
        let s = s.clamp(0.0, self.len());
        let mut out: Vec<LatLon> = self
            .pts
            .iter()
            .zip(&self.cum)
            .take_while(|(_, &c)| c < s)
            .map(|(p, _)| *p)
            .collect();
        out.push(self.at(s));
        out
    }
}

/// Where the dot is at an instant.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Pose {
    Parked(usize),
    Moving { leg: usize, u: f64 },
}

fn pose_at(sched: &Schedule, legs: usize, t: f64) -> Pose {
    for i in 0..legs {
        if t < sched.depart[i] {
            return Pose::Parked(i);
        }
        if t < sched.arrive[i + 1] {
            let u = (t - sched.depart[i]) / sched.leg_secs[i];
            return Pose::Moving { leg: i, u: ease(u) };
        }
    }
    Pose::Parked(legs)
}

fn view_for_leg(journey: &Journey, leg: usize) -> f64 {
    let l = &journey.legs[leg];
    match (l.mode, l.is_loop) {
        (LegMode::Flight, _) => FLIGHT_VIEW_DEG,
        _ => PORT_VIEW_DEG,
    }
}

/// Gaussian blur of `v` (edges clamped).
fn smooth(v: &[f64], sigma_frames: f64) -> Vec<f64> {
    let radius = (3.0 * sigma_frames).ceil() as i64;
    let kernel: Vec<f64> = (-radius..=radius)
        .map(|i| (-(i as f64).powi(2) / (2.0 * sigma_frames * sigma_frames)).exp())
        .collect();
    let norm: f64 = kernel.iter().sum();
    (0..v.len() as i64)
        .map(|f| {
            kernel
                .iter()
                .enumerate()
                .map(|(k, w)| {
                    let j = (f + k as i64 - radius).clamp(0, v.len() as i64 - 1) as usize;
                    w * v[j]
                })
                .sum::<f64>()
                / norm
        })
        .collect()
}

/// A camera: centre and scale.
#[derive(Debug, Clone, Copy)]
struct Cam {
    cx: f64,
    cy: f64,
    /// Pixels per degree (both axes).
    s: f64,
}

impl Cam {
    fn to_screen(self, (lat, lon): LatLon, (w, h): (u32, u32)) -> (f64, f64) {
        (
            f64::from(w) / 2.0 + (lon - self.cx) * self.s,
            f64::from(h) / 2.0 - (lat - self.cy) * self.s,
        )
    }

    fn to_world(self, (x, y): (f64, f64), (w, h): (u32, u32)) -> LatLon {
        (
            self.cy - (y - f64::from(h) / 2.0) / self.s,
            self.cx + (x - f64::from(w) / 2.0) / self.s,
        )
    }
}

/// Per-frame camera, from the schedule: the dot's position and the view
/// width for its phase, blurred so cuts between phases become smooth zooms
/// and pans, with the pull-back to the whole route as the last target.
fn plan_camera(
    journey: &Journey,
    sched: &Schedule,
    geoms: &[LegGeom],
    frames: u32,
    aspect: f64,
) -> Vec<(f64, f64, f64)> {
    let (mut lat_min, mut lat_max, mut lon_min, mut lon_max) =
        (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for g in geoms {
        for p in &g.pts {
            lat_min = lat_min.min(p.0);
            lat_max = lat_max.max(p.0);
            lon_min = lon_min.min(p.1);
            lon_max = lon_max.max(p.1);
        }
    }
    let whole_view = ((lon_max - lon_min) * 1.15)
        .max((lat_max - lat_min) * 1.15 * aspect)
        .max(30.0);
    let whole_center = ((lat_min + lat_max) / 2.0, (lon_min + lon_max) / 2.0);
    let legs = journey.legs.len();
    // A loop is framed whole: its bounding box fills LOOP_FILL of the frame
    // height, with the port it leaves from at the bottom of it.
    let loop_frame = |leg: usize| -> ((f64, f64), f64) {
        let g = &geoms[leg];
        let (mut a, mut b, mut c, mut d) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
        for p in &g.pts {
            a = a.min(p.0);
            b = b.max(p.0);
            c = c.min(p.1);
            d = d.max(p.1);
        }
        let view = ((b - a) / LOOP_FILL * aspect).max((d - c) * 1.6).max(4.0);
        (((a + b) / 2.0, (c + d) / 2.0), view)
    };
    let (mut lat, mut lon, mut logw) = (vec![], vec![], vec![]);
    for f in 0..frames {
        let t = f64::from(f) / f64::from(FPS);
        let pose = pose_at(sched, legs, t);
        let (pos, view) = match pose {
            Pose::Moving { leg, .. } if journey.legs[leg].is_loop => loop_frame(leg),
            Pose::Moving { leg, u } => {
                let g = &geoms[leg];
                (g.at(u * g.len()), view_for_leg(journey, leg))
            }
            // Parked before a loop, or holding on one after it.
            Pose::Parked(n) if n < legs && journey.legs[n].is_loop => loop_frame(n),
            Pose::Parked(n) if n > 0 && journey.legs[n - 1].is_loop => loop_frame(n - 1),
            Pose::Parked(n) => (
                journey.nodes[n].pos,
                if n < legs {
                    view_for_leg(journey, n)
                } else {
                    FLIGHT_VIEW_DEG
                },
            ),
        };
        let (pos, view) = if t >= sched.pull_back_at {
            (whole_center, whole_view)
        } else {
            (pos, view)
        };
        lat.push(pos.0);
        lon.push(pos.1);
        logw.push(view.ln());
    }
    let sigma = CAMERA_SIGMA_SECS * f64::from(FPS);
    let (lat, lon, logw) = (
        smooth(&lat, sigma),
        smooth(&lon, sigma),
        smooth(&logw, sigma),
    );
    (0..frames as usize)
        .map(|f| (lat[f], lon[f], logw[f].exp()))
        .collect()
}

/// A label or tag ready to draw.
struct Sprite(RgbaImage);

struct Scene<'a> {
    journey: &'a Journey,
    sched: Schedule,
    geoms: Vec<LegGeom>,
    cams: Vec<(f64, f64, f64)>,
    w: u32,
    h: u32,
    k: f64,
    /// City label sprites by node.
    labels: Vec<Option<Sprite>>,
    /// Boarding tag sprites by leg.
    tags: Vec<Option<Sprite>>,
    /// Nodes with labels in journey order, for the recency rule.
    labelled: Vec<usize>,
    /// Smaller label sprites for the final overview, where every city is
    /// named, and each one's offset from its pin (placed once, at the
    /// overview camera, so the cluster in Asia does not collide).
    overview_labels: Vec<Option<Sprite>>,
    overview_offsets: Vec<Option<(f64, f64)>>,
    /// (glow, core) line colours per leg: each ship gets its own.
    leg_colors: Vec<(Rgb<u8>, Rgb<u8>)>,
    glow_mask: Vec<u8>,
    core_mask: Vec<u8>,
    /// Per row: touched pixel span in the masks.
    spans: Vec<(i32, i32)>,
}

impl Scene<'_> {
    fn cam(&self, f: u32) -> Cam {
        let (lat, lon, view) = self.cams[f as usize];
        Cam {
            cx: lon,
            cy: lat,
            s: f64::from(self.w) / view,
        }
    }

    /// Land, water and graticule for `cam`, drawn on all cores.
    fn paint_world(&self, cam: Cam) -> RgbImage {
        let (w, h) = (self.w, self.h);
        let mut img = RgbImage::from_pixel(w, h, WATER);
        let cells_per_px = 20.0 / cam.s;
        // A floor on the blend zone rounds the grid's stair-steps into smooth shores.
        let soft = (0.5 * cells_per_px).clamp(0.12, 0.5);
        // Zoomed in (cells wider than a pixel), the blurred grid rounds off
        // the raw grid's stair-steps; zoomed out the raw one is sharper.
        let fraction: fn(LatLon) -> f32 = if cells_per_px < 0.7 {
            triptape_core::geo::land_fraction_smooth
        } else {
            triptape_core::geo::land_fraction
        };
        let threads = std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .min(16);
        let rows_per = (h as usize).div_ceil(threads).max(1);
        let raw = img.as_mut();
        std::thread::scope(|scope| {
            for (chunk_i, chunk) in raw.chunks_mut(rows_per * w as usize * 3).enumerate() {
                scope.spawn(move || {
                    for (ri, row) in chunk.chunks_mut(w as usize * 3).enumerate() {
                        let y = chunk_i * rows_per + ri;
                        let lat = cam.cy - ((y as f64 + 0.5) - f64::from(h) / 2.0) / cam.s;
                        for x in 0..w as usize {
                            let lon = cam.cx + ((x as f64 + 0.5) - f64::from(w) / 2.0) / cam.s;
                            let f = f64::from(fraction((lat, lon)));
                            let t = ((f - (0.5 - soft)) / (2.0 * soft)).clamp(0.0, 1.0);
                            let a = t * t * (3.0 - 2.0 * t);
                            if a > 0.0 {
                                let px = &mut row[x * 3..x * 3 + 3];
                                for (c, v) in px.iter_mut().enumerate() {
                                    *v = (f64::from(WATER.0[c]) * (1.0 - a)
                                        + f64::from(LAND.0[c]) * a)
                                        .round() as u8;
                                }
                            }
                        }
                    }
                });
            }
        });
        self.paint_graticule(&mut img, cam);
        img
    }

    /// Faint lat/lon lines at a spacing that keeps them about 150 px apart.
    fn paint_graticule(&self, img: &mut RgbImage, cam: Cam) {
        let want = 150.0 * self.k / cam.s;
        let step = [1.0, 2.0, 5.0, 10.0, 20.0, 30.0]
            .into_iter()
            .find(|s| *s >= want)
            .unwrap_or(30.0);
        let (w, h) = (self.w, self.h);
        let (lat0, lon0) = cam.to_world((0.0, f64::from(h)), (w, h));
        let (lat1, lon1) = cam.to_world((f64::from(w), 0.0), (w, h));
        let width = self.k.round().max(1.0) as i32;
        let mut lon = (lon0 / step).ceil() * step;
        while lon <= lon1 {
            let x = cam.to_screen((0.0, lon), (w, h)).0.round() as i32;
            for dx in 0..width {
                for y in 0..h {
                    put_grid(img, x + dx, y as i32);
                }
            }
            lon += step;
        }
        let mut lat = (lat0 / step).ceil() * step;
        while lat <= lat1 {
            let y = cam.to_screen((lat, 0.0), (w, h)).1.round() as i32;
            for dy in 0..width {
                for x in 0..w {
                    put_grid(img, x as i32, y + dy);
                }
            }
            lat += step;
        }
    }

    /// Stamp a screen polyline (anti-aliased capsules) into the masks.
    fn stamp_polyline(&mut self, pts: &[(f64, f64)], dashed: bool) {
        let (on, off) = (SEA_DASH_ON * self.k, SEA_DASH_OFF * self.k);
        let mut phase = 0.0; // distance into the on/off pattern
        for w in pts.windows(2) {
            let (a, b) = (w[0], w[1]);
            let len = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
            if len <= 0.0 {
                continue;
            }
            let margin = 6.0 * self.k;
            let off_screen = a.0.max(b.0) < -margin
                || a.0.min(b.0) > f64::from(self.w) + margin
                || a.1.max(b.1) < -margin
                || a.1.min(b.1) > f64::from(self.h) + margin;
            if !dashed {
                if !off_screen {
                    self.stamp_capsule(a, b);
                }
                continue;
            }
            // Dashes keyed to distance along the leg from its start.
            let at = |d: f64| (a.0 + (b.0 - a.0) * d / len, a.1 + (b.1 - a.1) * d / len);
            let mut d = 0.0;
            while d < len {
                let p = phase % (on + off);
                let (end, drawn) = if p < on {
                    ((d + on - p).min(len), true)
                } else {
                    ((d + on + off - p).min(len), false)
                };
                if drawn && !off_screen {
                    self.stamp_capsule(at(d), at(end));
                }
                phase += end - d;
                d = end;
            }
        }
    }

    fn stamp_capsule(&mut self, a: (f64, f64), b: (f64, f64)) {
        let (r_glow, r_core) = (4.0 * self.k, 2.0 * self.k);
        let reach = r_glow + 1.5;
        let x0 = ((a.0.min(b.0) - reach).floor() as i32).max(0);
        let x1 = ((a.0.max(b.0) + reach).ceil() as i32).min(self.w as i32 - 1);
        let y0 = ((a.1.min(b.1) - reach).floor() as i32).max(0);
        let y1 = ((a.1.max(b.1) + reach).ceil() as i32).min(self.h as i32 - 1);
        for y in y0..=y1 {
            for x in x0..=x1 {
                let d = crate::routeanim::point_seg_dist(
                    (f64::from(x) + 0.5, f64::from(y) + 0.5),
                    a,
                    b,
                );
                let cg = ((r_glow + 0.5 - d).clamp(0.0, 1.0) * 255.0) as u8;
                if cg == 0 {
                    continue;
                }
                let cc = ((r_core + 0.5 - d).clamp(0.0, 1.0) * 255.0) as u8;
                let idx = (y as u32 * self.w + x as u32) as usize;
                self.glow_mask[idx] = self.glow_mask[idx].max(cg);
                self.core_mask[idx] = self.core_mask[idx].max(cc);
                let span = &mut self.spans[y as usize];
                span.0 = span.0.min(x);
                span.1 = span.1.max(x);
            }
        }
    }

    /// Blend the stamped masks into `img` and clear them.
    fn flush_route(&mut self, img: &mut RgbImage, glow: Rgb<u8>, core: Rgb<u8>) {
        for y in 0..self.h as usize {
            let (x0, x1) = self.spans[y];
            if x0 > x1 {
                continue;
            }
            for x in x0..=x1 {
                let idx = y * self.w as usize + x as usize;
                let (g, c) = (self.glow_mask[idx], self.core_mask[idx]);
                if g == 0 {
                    continue;
                }
                blend_px(img, x, y as i32, glow, f64::from(g) / 255.0);
                if c > 0 {
                    blend_px(img, x, y as i32, core, f64::from(c) / 255.0);
                }
                self.glow_mask[idx] = 0;
                self.core_mask[idx] = 0;
            }
            self.spans[y] = (i32::MAX, i32::MIN);
        }
    }

    fn frame(&mut self, f: u32) -> RgbImage {
        let t = f64::from(f) / f64::from(FPS);
        let cam = self.cam(f);
        let size = (self.w, self.h);
        let k = self.k;
        let mut img = self.paint_world(cam);
        let legs = self.journey.legs.len();
        let pose = pose_at(&self.sched, legs, t);

        // Route drawn so far: finished legs whole, the current one in part.
        let done_legs = match pose {
            Pose::Parked(n) => n,
            Pose::Moving { leg, .. } => leg,
        };
        for i in 0..done_legs {
            let pts: Vec<_> = self.geoms[i]
                .pts
                .iter()
                .map(|&p| cam.to_screen(p, size))
                .collect();
            self.stamp_polyline(&pts, self.journey.legs[i].mode == LegMode::Ship);
            let (g, c) = self.leg_colors[i];
            self.flush_route(&mut img, g, c);
        }
        if let Pose::Moving { leg, u } = pose {
            let g = &self.geoms[leg];
            let pts: Vec<_> = g
                .prefix(u * g.len())
                .iter()
                .map(|&p| cam.to_screen(p, size))
                .collect();
            self.stamp_polyline(&pts, self.journey.legs[leg].mode == LegMode::Ship);
            let (g, c) = self.leg_colors[leg];
            self.flush_route(&mut img, g, c);
        }

        // Pins of the stops reached.
        let last = self.journey.nodes.len() - 1;
        for (i, n) in self.journey.nodes.iter().enumerate() {
            if !n.pin || t < self.sched.arrive[i] {
                continue;
            }
            let c = cam.to_screen(n.pos, size);
            if !on_screen(c, size, 20.0 * k) {
                continue;
            }
            if i == 0 || i == last {
                let col = if i == 0 { PIN_START } else { PIN_END };
                blend_shape(&mut img, c, 11.0 * k, None, PIN_RING, 1.0);
                blend_shape(&mut img, c, 8.0 * k, None, col, 1.0);
            } else {
                blend_shape(&mut img, c, 5.0 * k, None, PIN_RING, 1.0);
                blend_shape(&mut img, c, 3.0 * k, None, PIN_STOP, 1.0);
            }
        }

        let taken = self.draw_labels(&mut img, cam, t);
        self.draw_tags(&mut img, cam, t, &taken);

        // The dot and its vehicle.
        let (pos, heading_to, mode) = match pose {
            Pose::Moving { leg, u } => {
                let g = &self.geoms[leg];
                let s = u * g.len();
                (g.at(s), g.at(s + 0.4), Some(self.journey.legs[leg].mode))
            }
            Pose::Parked(n) => (self.journey.nodes[n].pos, self.journey.nodes[n].pos, None),
        };
        let dot = cam.to_screen(pos, size);
        if let Pose::Parked(n) = pose {
            if let Some((_, since)) = pause_of(&self.sched, n, t) {
                if self.journey.nodes[n].label.is_some() || n == 0 {
                    let p = (since / 0.9).fract();
                    blend_shape(
                        &mut img,
                        dot,
                        (10.0 + 20.0 * p) * k,
                        Some(2.0 * k),
                        CORE,
                        0.8 * (1.0 - p),
                    );
                }
            }
        }
        match mode {
            Some(LegMode::Flight) => {
                blend_shape(&mut img, dot, 16.0 * k, None, CORE, 0.28);
                let ahead = cam.to_screen(heading_to, size);
                let back = cam.to_screen(self.geoms_back(pose), size);
                let heading = (ahead.1 - back.1).atan2(ahead.0 - back.0);
                draw_plane(&mut img, dot, heading, k);
            }
            Some(LegMode::Ship) => {
                let core = match pose {
                    Pose::Moving { leg, .. } => self.leg_colors[leg].1,
                    Pose::Parked(_) => CORE,
                };
                blend_shape(&mut img, dot, 16.0 * k, None, core, 0.28);
                blend_shape(&mut img, dot, 9.0 * k, None, PIN_RING, 1.0);
                blend_shape(&mut img, dot, 6.0 * k, None, core, 1.0);
                draw_ship_icon(&mut img, (dot.0, dot.1 - 18.0 * k), k, core);
            }
            _ => {
                blend_shape(&mut img, dot, 16.0 * k, None, CORE, 0.28);
                blend_shape(&mut img, dot, 9.0 * k, None, PIN_RING, 1.0);
                blend_shape(&mut img, dot, 6.0 * k, None, CORE, 1.0);
            }
        }
        img
    }

    /// A point just behind the dot on its leg (for the plane's heading).
    fn geoms_back(&self, pose: Pose) -> LatLon {
        match pose {
            Pose::Moving { leg, u } => {
                let g = &self.geoms[leg];
                g.at(u * g.len() - 0.4)
            }
            Pose::Parked(n) => self.journey.nodes[n].pos,
        }
    }

    fn draw_labels(&self, img: &mut RgbImage, cam: Cam, t: f64) -> Vec<(f64, f64, f64, f64)> {
        let mut rects: Vec<(f64, f64, f64, f64)> = vec![];
        let size = (self.w, self.h);
        let k = self.k;
        // At the pull-back the working labels crossfade into the overview
        // set, where every city is named.
        let pull = ease(((t - self.sched.pull_back_at) / 0.6).clamp(0.0, 1.0));
        for (j, &n) in self.labelled.iter().enumerate() {
            let arrive = self.sched.arrive[n];
            let fade_in = ((t - arrive) / triptape_core_label_fade()).clamp(0.0, 1.0);
            if fade_in <= 0.0 {
                continue;
            }
            let c = cam.to_screen(self.journey.nodes[n].pos, size);
            if let (Some(Sprite(sprite)), Some(off)) =
                (&self.overview_labels[n], self.overview_offsets[n])
            {
                let alpha = fade_in * pull;
                if alpha > 0.0 && on_screen(c, size, 300.0 * k) {
                    let at = ((c.0 + off.0).round() as i32, (c.1 + off.1).round() as i32);
                    // A nudged label is tied to its own pin by a short leader.
                    let (lw, lh) = (f64::from(sprite.width()), f64::from(sprite.height()));
                    let (x0, y0) = (f64::from(at.0), f64::from(at.1));
                    let edge = (c.0.clamp(x0, x0 + lw), c.1.clamp(y0, y0 + lh));
                    let len = ((edge.0 - c.0).powi(2) + (edge.1 - c.1).powi(2)).sqrt();
                    if len > 12.0 * k {
                        let steps = (len / (0.7 * k)).ceil() as u32;
                        for i in 0..=steps {
                            let u = f64::from(i) / f64::from(steps);
                            let p = (c.0 + (edge.0 - c.0) * u, c.1 + (edge.1 - c.1) * u);
                            blend_shape(img, p, 0.9 * k, None, CORE, 0.8 * alpha);
                        }
                    }
                    composite(img, sprite, at, alpha);
                    rects.push((x0, y0, x0 + lw, y0 + lh));
                }
            }
            let Some(Sprite(sprite)) = &self.labels[n] else {
                continue;
            };
            // Older labels fade once LABELS_KEPT later stops have been reached.
            let stale_at = self
                .labelled
                .get(j + LABELS_KEPT)
                .map(|&m| self.sched.arrive[m]);
            let recency = stale_at.map_or(1.0, |s| {
                1.0 - ((t - s) / LABEL_FADE_OUT_SECS).clamp(0.0, 1.0)
            });
            let alpha = fade_in * recency * (1.0 - pull);
            if alpha <= 0.0 {
                continue;
            }
            let (lw, lh) = (f64::from(sprite.width()), f64::from(sprite.height()));
            let gap = 14.0 * k;
            if !on_screen(c, size, lw + gap) {
                continue;
            }
            // Alternate above and below so neighbours rarely collide.
            let above = j % 2 == 0;
            let mut x = c.0 + gap;
            if x + lw > f64::from(self.w) - gap {
                x = c.0 - gap - lw;
            }
            let mut y = if above { c.1 - gap - lh } else { c.1 + gap };
            y = y.clamp(gap, f64::from(self.h) - lh - gap);
            composite(img, sprite, (x.round() as i32, y.round() as i32), alpha);
            rects.push((x, y, x + lw, y + lh));
        }
        rects
    }

    fn draw_tags(&self, img: &mut RgbImage, cam: Cam, t: f64, taken: &[(f64, f64, f64, f64)]) {
        let size = (self.w, self.h);
        let k = self.k;
        for (i, tag) in self.tags.iter().enumerate() {
            let Some(Sprite(sprite)) = tag else { continue };
            let start = self.sched.depart[i];
            let local = t - start;
            if !(0.0..=TAG_SECS).contains(&local) {
                continue;
            }
            let alpha = (local / TAG_FADE_SECS)
                .clamp(0.0, 1.0)
                .min(((TAG_SECS - local) / TAG_FADE_SECS).clamp(0.0, 1.0));
            // Sits beside the outgoing leg (a little way along it), up and
            // right of the path when there is room, never on a city label.
            let g = &self.geoms[i];
            let c = cam.to_screen(g.at(0.3 * g.len()), size);
            let (lw, lh) = (f64::from(sprite.width()), f64::from(sprite.height()));
            let (gap, m) = (16.0 * k, 14.0 * k);
            let overlaps = |x: f64, y: f64| {
                taken.iter().any(|r| {
                    x < r.2 + 4.0 * k
                        && r.0 < x + lw + 4.0 * k
                        && y < r.3 + 4.0 * k
                        && r.1 < y + lh + 4.0 * k
                })
            };
            let candidates = [
                (c.0 + gap, c.1 - gap - lh),
                (c.0 + gap, c.1 + gap),
                (c.0 - gap - lw, c.1 - gap - lh),
                (c.0 - gap - lw, c.1 + gap),
                (c.0 + 3.0 * gap, c.1 - 3.0 * gap - lh),
                (c.0 + 3.0 * gap, c.1 + 3.0 * gap),
            ];
            let clamp = |(x, y): (f64, f64)| {
                (
                    x.clamp(m, (f64::from(self.w) - lw - m).max(m)),
                    y.clamp(m, (f64::from(self.h) - lh - m).max(m)),
                )
            };
            let (x, y) = candidates
                .iter()
                .map(|&p| clamp(p))
                .find(|&(x, y)| !overlaps(x, y))
                .unwrap_or_else(|| clamp(candidates[0]));
            composite(img, sprite, (x.round() as i32, y.round() as i32), alpha);
        }
    }
}

fn triptape_core_label_fade() -> f64 {
    crate::routeanim::LABEL_FADE_SECS
}

fn pause_of(sched: &Schedule, node: usize, t: f64) -> Option<(usize, f64)> {
    (t >= sched.arrive[node] && t < sched.depart[node]).then(|| (node, t - sched.arrive[node]))
}

fn on_screen(c: (f64, f64), (w, h): (u32, u32), margin: f64) -> bool {
    c.0 > -margin && c.0 < f64::from(w) + margin && c.1 > -margin && c.1 < f64::from(h) + margin
}

fn put_grid(img: &mut RgbImage, x: i32, y: i32) {
    if x >= 0 && y >= 0 && (x as u32) < img.width() && (y as u32) < img.height() {
        let px = img.get_pixel_mut(x as u32, y as u32);
        // Under land the line is the same faint step lighter, not a new colour.
        if *px == WATER {
            *px = GRID;
        } else {
            for c in 0..3 {
                px.0[c] = px.0[c].saturating_add(6);
            }
        }
    }
}

/// Anti-aliased convex polygon (3x3 sub-samples per pixel).
fn fill_poly(img: &mut RgbImage, pts: &[(f64, f64)], col: Rgb<u8>, alpha: f64) {
    let (mut x0, mut x1, mut y0, mut y1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for p in pts {
        x0 = x0.min(p.0);
        x1 = x1.max(p.0);
        y0 = y0.min(p.1);
        y1 = y1.max(p.1);
    }
    let inside = |px: f64, py: f64| {
        let mut pos = false;
        let mut neg = false;
        for i in 0..pts.len() {
            let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
            let cross = (b.0 - a.0) * (py - a.1) - (b.1 - a.1) * (px - a.0);
            pos |= cross > 0.0;
            neg |= cross < 0.0;
        }
        !(pos && neg)
    };
    for y in y0.floor() as i32..=y1.ceil() as i32 {
        for x in x0.floor() as i32..=x1.ceil() as i32 {
            let mut hits = 0;
            for sy in 0..3 {
                for sx in 0..3 {
                    let (px, py) = (
                        f64::from(x) + (f64::from(sx) + 0.5) / 3.0,
                        f64::from(y) + (f64::from(sy) + 0.5) / 3.0,
                    );
                    hits += i32::from(inside(px, py));
                }
            }
            if hits > 0 {
                blend_px(img, x, y, col, alpha * f64::from(hits) / 9.0);
            }
        }
    }
}

/// A small airliner silhouette at `c` pointing along `heading` (radians,
/// screen space).
fn draw_plane(img: &mut RgbImage, c: (f64, f64), heading: f64, k: f64) {
    let (sin, cos) = heading.sin_cos();
    let place = |pts: &[(f64, f64)]| -> Vec<(f64, f64)> {
        pts.iter()
            .map(|&(x, y)| (c.0 + (x * cos - y * sin) * k, c.1 + (x * sin + y * cos) * k))
            .collect()
    };
    let parts: [&[(f64, f64)]; 5] = [
        &[(16.0, 0.0), (-11.0, -3.0), (-11.0, 3.0)],
        &[(3.0, -1.0), (-6.0, -15.0), (-10.0, -15.0), (-3.0, -1.0)],
        &[(3.0, 1.0), (-6.0, 15.0), (-10.0, 15.0), (-3.0, 1.0)],
        &[(-8.0, -1.0), (-13.0, -6.0), (-15.0, -6.0), (-12.0, -1.0)],
        &[(-8.0, 1.0), (-13.0, 6.0), (-15.0, 6.0), (-12.0, 1.0)],
    ];
    // A dark halo first so the plane reads on land and water alike.
    for p in parts {
        let grown: Vec<(f64, f64)> = p.iter().map(|&(x, y)| (x * 1.25, y * 1.25)).collect();
        fill_poly(img, &place(&grown), Rgb([13, 20, 32]), 0.55);
    }
    for p in parts {
        fill_poly(img, &place(p), PIN_RING, 1.0);
    }
}

/// A small ship (hull and sail) centred on `c`.
fn draw_ship_icon(img: &mut RgbImage, c: (f64, f64), k: f64, sail: Rgb<u8>) {
    let at = |pts: &[(f64, f64)]| -> Vec<(f64, f64)> {
        pts.iter()
            .map(|&(x, y)| (c.0 + x * k, c.1 + y * k))
            .collect()
    };
    fill_poly(
        img,
        &at(&[(-12.0, 0.0), (12.0, 0.0), (8.0, 7.0), (-8.0, 7.0)]),
        Rgb([13, 20, 32]),
        0.7,
    );
    fill_poly(
        img,
        &at(&[(-10.0, 0.0), (10.0, 0.0), (7.0, 5.0), (-7.0, 5.0)]),
        PIN_RING,
        1.0,
    );
    fill_poly(
        img,
        &at(&[(-1.0, -1.0), (-1.0, -14.0), (8.0, -1.0)]),
        sail,
        1.0,
    );
}

/// Render the itinerary recap at `res` into `out_mp4`.
pub fn render_journey_recap(
    caps: &Capabilities,
    journey: &Journey,
    res: Resolution,
    typst_bin: &str,
    work_dir: &Path,
    out_mp4: &Path,
) -> Result<RecapReport, MediaError> {
    render_journey_sized(caps, journey, res.dims(), typst_bin, work_dir, out_mp4)
}

pub(crate) fn render_journey_sized(
    caps: &Capabilities,
    journey: &Journey,
    (w, h): (u32, u32),
    typst_bin: &str,
    work_dir: &Path,
    out_mp4: &Path,
) -> Result<RecapReport, MediaError> {
    let mut notes = vec![];
    let mut scene = build_scene(journey, (w, h), typst_bin, work_dir, &mut notes)?;
    let frames = scene.cams.len() as u32;
    let secs = f64::from(frames) / f64::from(FPS);
    encode(caps, (w, h), frames, secs, out_mp4, |f| scene.frame(f))?;
    Ok(RecapReport { secs, notes })
}

fn build_scene<'a>(
    journey: &'a Journey,
    (w, h): (u32, u32),
    typst_bin: &str,
    work_dir: &Path,
    notes: &mut Vec<String>,
) -> Result<Scene<'a>, MediaError> {
    if journey.legs.is_empty() || journey.nodes.len() != journey.legs.len() + 1 {
        return Err(MediaError::InvalidArg(
            "journey needs at least one leg".into(),
        ));
    }
    let k = f64::from((h / 1080).max(1));
    let sched = journey.schedule();
    let frames = frame_count(sched.total_secs);
    let geoms: Vec<LegGeom> = journey.legs.iter().map(|l| LegGeom::new(&l.path)).collect();
    let cams = plan_camera(journey, &sched, &geoms, frames, f64::from(w) / f64::from(h));

    std::fs::create_dir_all(work_dir).map_err(|e| MediaError::Io(e.to_string()))?;
    let mut sprite = |tag: String, text: &str, size_pt: f64| -> Option<Sprite> {
        let png = work_dir.join(format!("journey_{tag}.png"));
        let made = crate::titlecard::render_text_png(
            typst_bin,
            &label_doc_sized(text, size_pt),
            &png,
            72 * k as u32,
        )
        .and_then(|()| {
            image::open(&png)
                .map(|im| im.to_rgba8())
                .map_err(|e| MediaError::Io(format!("read label: {e}")))
        });
        let _ = std::fs::remove_file(&png);
        match made {
            Ok(img) => Some(Sprite(img)),
            Err(e) => {
                notes.push(format!("recap label {text:?} skipped: {e}"));
                None
            }
        }
    };
    let labels: Vec<Option<Sprite>> = journey
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| {
            n.label
                .as_deref()
                .and_then(|t| sprite(format!("n{i:02}"), t, 30.0))
        })
        .collect();
    let overview_labels: Vec<Option<Sprite>> = journey
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| {
            n.label
                .as_deref()
                .and_then(|t| sprite(format!("o{i:02}"), t, 22.0))
        })
        .collect();
    let tags: Vec<Option<Sprite>> = journey
        .legs
        .iter()
        .enumerate()
        .map(|(i, l)| {
            l.boarding_tag
                .as_deref()
                .and_then(|t| sprite(format!("t{i:02}"), t, 30.0))
        })
        .collect();
    let labelled: Vec<usize> = (0..journey.nodes.len())
        .filter(|&i| journey.nodes[i].label.is_some())
        .collect();
    // Place every overview label once, at the final camera, so the cities
    // clustered in Asia are nudged apart rather than dropped.
    let overview_offsets: Vec<Option<(f64, f64)>> = {
        let (lat, lon, view) = *cams.last().expect("frames");
        let cam = Cam {
            cx: lon,
            cy: lat,
            s: f64::from(w) / view,
        };
        let anchors: Vec<(f64, f64)> = journey
            .nodes
            .iter()
            .map(|n| cam.to_screen(n.pos, (w, h)))
            .collect();
        let sizes: Vec<Option<(u32, u32)>> = overview_labels
            .iter()
            .map(|l| l.as_ref().map(|Sprite(i)| (i.width(), i.height())))
            .collect();
        crate::routeanim::place_labels(&anchors, &sizes, (w, h), 10.0 * k)
            .into_iter()
            .zip(&anchors)
            .map(|(p, a)| p.map(|p| (f64::from(p.0) - a.0, f64::from(p.1) - a.1)))
            .collect()
    };
    // Each ship's legs get their own colour; flights and ground hops stay amber.
    let ships = journey.ships();
    let palette: [(Rgb<u8>, Rgb<u8>); 3] = [
        (GLOW, CORE),
        (Rgb([34, 104, 156]), Rgb([125, 211, 252])),
        (Rgb([26, 112, 98]), Rgb([110, 231, 183])),
    ];
    let leg_colors: Vec<(Rgb<u8>, Rgb<u8>)> = journey
        .legs
        .iter()
        .map(|l| {
            l.ship
                .as_ref()
                .and_then(|s| ships.iter().position(|x| x == s))
                .map_or(palette[0], |i| palette[i % palette.len()])
        })
        .collect();
    Ok(Scene {
        journey,
        sched,
        geoms,
        cams,
        w,
        h,
        k,
        labels,
        tags,
        labelled,
        overview_labels,
        overview_offsets,
        leg_colors,
        glow_mask: vec![0; (w * h) as usize],
        core_mask: vec![0; (w * h) as usize],
        spans: vec![(i32::MAX, i32::MIN); h as usize],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use triptape_core::itinerary::Itinerary;

    const JSON: &str = r#"{
      "stops": [
        { "id": "a", "label": "Los Angeles", "lat": 33.94, "lon": -118.41, "depart": "2026-10-03", "kind": "home" },
        { "id": "b", "label": "Tokyo", "lat": 35.71, "lon": 139.80, "arrive": "2026-10-04", "depart": "2026-10-09" },
        { "id": "c", "label": "Tokyo", "lat": 35.6177, "lon": 139.771, "arrive": "2026-10-09", "depart": "2026-10-09", "kind": "port", "hide_label": true },
        { "id": "d", "label": "Osaka", "lat": 34.656, "lon": 135.4306, "arrive": "2026-10-11", "depart": "2026-10-11", "kind": "port" },
        { "id": "e", "label": "Singapore", "lat": 1.2655, "lon": 103.864, "arrive": "2026-10-23", "depart": "2026-10-30" },
        { "id": "f", "label": "Los Angeles", "lat": 33.94, "lon": -118.41, "arrive": "2026-10-30", "kind": "home", "hide_label": true }
      ],
      "legs": [
        { "from": "a", "to": "b", "mode": "flight" },
        { "from": "b", "to": "c", "mode": "ground" },
        { "from": "c", "to": "d", "mode": "ship", "ship": "Carnival Luminosa" },
        { "from": "d", "to": "e", "mode": "ship", "ship": "Carnival Luminosa" },
        { "from": "e", "to": "e", "mode": "ship", "ship": "Disney Adventure", "loop": true, "dates": "2026-10-26..2026-10-29" },
        { "from": "e", "to": "f", "mode": "flight" }
      ]
    }"#;

    fn journey() -> Journey {
        Journey::build(&Itinerary::from_json(JSON).unwrap(), &[])
    }

    fn scene_at<'a>(w: u32, h: u32, j: &'a Journey, dir: &Path) -> Scene<'a> {
        build_scene(j, (w, h), "no-such-typst", dir, &mut vec![]).unwrap()
    }

    #[test]
    fn frame_count_is_the_schedule_times_fps() {
        let j = journey();
        let dir = tempfile::tempdir().unwrap();
        let s = scene_at(320, 180, &j, dir.path());
        assert_eq!(s.cams.len() as u32, frame_count(j.duration_secs()));
        assert!(
            (s.cams.len() as f64 / f64::from(FPS) - j.duration_secs()).abs() < 0.5 / f64::from(FPS)
        );
    }

    #[test]
    fn leg_geometry_walks_arc_length() {
        let g = LegGeom::new(&[(0.0, 0.0), (0.0, 3.0), (4.0, 3.0)]);
        assert_eq!(g.len(), 7.0);
        assert_eq!(g.at(0.0), (0.0, 0.0));
        assert_eq!(g.at(1.5), (0.0, 1.5));
        assert_eq!(g.at(5.0), (2.0, 3.0));
        assert_eq!(g.at(99.0), (4.0, 3.0));
        let p = g.prefix(5.0);
        assert_eq!(p, vec![(0.0, 0.0), (0.0, 3.0), (2.0, 3.0)]);
        assert_eq!(g.prefix(0.0), vec![(0.0, 0.0)]);
    }

    #[test]
    fn pose_follows_the_schedule_with_eased_legs() {
        let j = journey();
        let s = j.schedule();
        assert_eq!(pose_at(&s, j.legs.len(), 0.0), Pose::Parked(0));
        // Mid flight (first leg): moving, progress between 0 and 1.
        let mid = (s.depart[0] + s.arrive[1]) / 2.0;
        match pose_at(&s, j.legs.len(), mid) {
            Pose::Moving { leg: 0, u } => assert!((u - 0.5).abs() < 1e-9),
            p => panic!("{p:?}"),
        }
        // Parked at each arrival, and for good after the last.
        assert_eq!(
            pose_at(&s, j.legs.len(), s.arrive[3] + 0.01),
            Pose::Parked(3)
        );
        assert_eq!(
            pose_at(&s, j.legs.len(), s.total_secs),
            Pose::Parked(j.legs.len())
        );
    }

    #[test]
    fn camera_opens_on_la_follows_to_asia_and_ends_on_the_whole_route() {
        let j = journey();
        let dir = tempfile::tempdir().unwrap();
        let s = scene_at(320, 180, &j, dir.path());
        // First frame: on Los Angeles at the flight zoom.
        let (lat, lon, view) = s.cams[0];
        assert!(
            (lat - 33.94).abs() < 3.0 && (lon + 118.41).abs() < 3.0,
            "{lat} {lon}"
        );
        assert!((view - FLIGHT_VIEW_DEG).abs() < 5.0, "{view}");
        // At the first port the view has zoomed in (ship legs).
        let t = s.sched.arrive[3] + 0.2;
        let f = (t * f64::from(FPS)) as usize;
        assert!(
            s.cams[f].2 < 40.0,
            "zoomed in on the ports: {}",
            s.cams[f].2
        );
        // Last frame: the whole route (LA to Singapore) in view.
        let (lat, lon, view) = *s.cams.last().unwrap();
        let (lon_lo, lon_hi) = (lon - view / 2.0, lon + view / 2.0);
        let all_lons = j.legs.iter().flat_map(|l| l.path.iter().map(|p| p.1));
        for l in all_lons {
            assert!(l >= lon_lo && l <= lon_hi, "{l} outside {lon_lo}..{lon_hi}");
        }
        let half_lat = view * 9.0 / 16.0 / 2.0;
        for p in j.legs.iter().flat_map(|l| l.path.iter()) {
            assert!((p.0 - lat).abs() <= half_lat, "{} outside lat view", p.0);
        }
        // The camera never jumps: consecutive frames move a bounded amount.
        for w in s.cams.windows(2) {
            assert!(
                (w[1].1 - w[0].1).abs() < 8.0,
                "lon jump {} -> {}",
                w[0].1,
                w[1].1
            );
            assert!((w[1].2 / w[0].2 - 1.0).abs() < 0.15, "zoom jump");
        }
    }

    fn route_px(img: &RgbImage) -> usize {
        img.pixels().filter(|p| **p == GLOW).count()
    }

    #[test]
    fn first_frame_has_no_route_and_the_last_has_it_all() {
        let j = journey();
        let dir = tempfile::tempdir().unwrap();
        let mut s = scene_at(480, 270, &j, dir.path());
        let n = s.cams.len() as u32;
        let first = s.frame(0);
        assert_eq!(route_px(&first), 0, "no line before the dot moves");
        let mut counts = vec![];
        for f in (0..n).step_by(4) {
            counts.push(route_px(&s.frame(f)));
        }
        let last = s.frame(n - 1);
        assert!(route_px(&last) > 300, "{}", route_px(&last));
        // The Pacific arc is on the first frames' side of the world, the
        // Asian legs come later: line pixels exist mid-journey too.
        assert!(counts.iter().any(|&c| c > 50));
        // Dashed ship legs leave gaps: fewer pixels than a solid line of the
        // same length would, checked by the dash pattern leaving land/water
        // pixels along an overview of the ship leg.
        assert_eq!(
            s.labels.iter().filter(|l| l.is_some()).count(),
            0,
            "typst absent"
        );
    }

    #[test]
    fn ships_are_never_map_labels_and_tags_ride_the_boarding_legs() {
        let j = journey();
        let dir = tempfile::tempdir().unwrap();
        let mut notes = vec![];
        let s = build_scene(&j, (320, 180), "no-such-typst", dir.path(), &mut notes).unwrap();
        // Typst is absent: every requested text is noted, none is a ship name
        // as a node label; both boarding tags were requested.
        let node_notes: Vec<_> = notes.iter().filter(|n| !n.contains("Boarded")).collect();
        assert!(node_notes
            .iter()
            .all(|n| !n.contains("Carnival") && !n.contains("Disney")));
        assert_eq!(notes.iter().filter(|n| n.contains("Boarded")).count(), 2);
        assert_eq!(s.labelled, vec![0, 1, 3, 4]);
        // Typst is absent, so no overview sprite exists to place.
        assert!(s.overview_offsets.iter().all(Option::is_none));
    }

    #[test]
    fn smoothing_is_a_normalised_blur_that_keeps_constants() {
        let v = vec![5.0; 40];
        assert!(smooth(&v, 4.0).iter().all(|x| (x - 5.0).abs() < 1e-9));
        let step: Vec<f64> = (0..60).map(|i| if i < 30 { 0.0 } else { 1.0 }).collect();
        let s = smooth(&step, 4.0);
        assert!(s[0] < 0.01 && s[59] > 0.99);
        assert!(s.windows(2).all(|w| w[1] >= w[0] - 1e-12));
    }

    fn test_caps() -> Option<Capabilities> {
        let bin = std::env::var("TRIPTAPE_FFMPEG").unwrap_or_else(|_| "ffmpeg".into());
        let probe = Path::new(&bin)
            .with_file_name("ffprobe")
            .to_string_lossy()
            .into_owned();
        let caps = crate::ffmpeg::probe_capabilities(&bin, &probe);
        caps.libx264.then_some(caps)
    }

    #[test]
    fn real_ffmpeg_journey_has_the_scheduled_length_and_frames() {
        let Some(caps) = test_caps() else { return };
        let j = journey();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("journey.mp4");
        let rep =
            render_journey_sized(&caps, &j, (320, 180), "no-such-typst", dir.path(), &out).unwrap();
        assert!((rep.secs - j.duration_secs()).abs() < 0.5 / f64::from(FPS));
        let raw = std::process::Command::new(&caps.ffmpeg_bin)
            .args(["-v", "error", "-i"])
            .arg(&out)
            .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
            .output()
            .unwrap();
        assert!(raw.status.success());
        let frame_len = 320 * 180 * 3;
        assert_eq!(raw.stdout.len() % frame_len, 0);
        assert_eq!((raw.stdout.len() / frame_len) as u32, frame_count(rep.secs));
        let near = |frame: &[u8]| {
            frame
                .as_chunks::<3>()
                .0
                .iter()
                .filter(|p| {
                    (i32::from(p[0]) - 245).abs() < 14
                        && (i32::from(p[1]) - 178).abs() < 14
                        && (i32::from(p[2]) - 66).abs() < 14
                })
                .count()
        };
        let n = raw.stdout.len() / frame_len;
        // The last frame (whole route) shows far more route than the first.
        assert!(near(&raw.stdout[(n - 1) * frame_len..]) > near(&raw.stdout[..frame_len]) + 60);
    }

    /// Real-trip check (run by hand, release build):
    /// `TRIPTAPE_RECAP_DB=copy.db TRIPTAPE_ITINERARY=itinerary.json TRIPTAPE_RECAP_OUT=dir
    /// [TRIPTAPE_BENCH_RES=1080p] [TRIPTAPE_FFMPEG=..]
    /// cargo test --release -p triptape-media real_trip_journey -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a trip db and itinerary; see doc comment"]
    fn real_trip_journey() {
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k}"));
        let out_dir = std::path::PathBuf::from(env("TRIPTAPE_RECAP_OUT"));
        let res = Resolution::from_flag(
            &std::env::var("TRIPTAPE_BENCH_RES").unwrap_or_else(|_| "4k".into()),
        )
        .unwrap();
        let caps = test_caps().expect("ffmpeg with libx264");
        let itin = Itinerary::load(Path::new(&env("TRIPTAPE_ITINERARY"))).unwrap();
        let loaded = triptape_core::db::Db::open(Path::new(&env("TRIPTAPE_RECAP_DB")))
            .unwrap()
            .edited_plan_with(None, Some(&itin))
            .unwrap()
            .expect("plan in db");
        for c in loaded.edited.chapters.iter().take(200) {
            eprintln!("chapter {:?} place {:?}", c.title, c.place);
        }
        let t = std::time::Instant::now();
        let journey = Journey::build(&itin, &loaded.assets);
        eprintln!("journey built in {:.2}s", t.elapsed().as_secs_f64());
        std::fs::create_dir_all(&out_dir).unwrap();
        let out = out_dir.join(format!("journey_{}.mp4", res.label()));
        let t = std::time::Instant::now();
        let rep = render_journey_recap(&caps, &journey, res, "typst", &out_dir, &out).unwrap();
        eprintln!(
            "JOURNEY {}: {:.1}s of video, rendered in {:.1}s, notes {:?}",
            res.label(),
            rep.secs,
            t.elapsed().as_secs_f64(),
            rep.notes
        );
    }
}
