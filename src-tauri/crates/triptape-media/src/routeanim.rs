//! Animated "travel along the route" recap for the closing map card.
//!
//! A dot leaves the first chapter stop, the route line draws itself behind
//! it, and it pauses at every stop while the place label fades in. The
//! motion is eased per leg, runs at roughly constant screen speed, and is
//! fitted to a duration the plan already budgeted
//! ([`triptape_core::route_stops::closing_map_secs`]).
//!
//! Rendering: the land base is painted once, the growing line is stamped
//! incrementally into a persistent canvas (anti-aliased, coverage kept in
//! masks so overlapping stamps never beat), and each frame copies the canvas
//! and draws pins, labels and the dot on top. Raw RGB frames stream straight
//! into ffmpeg's stdin, so nothing hits the disk but the finished segment.
//! Offline only: land comes from the bundled Natural Earth grid.

use crate::ffmpeg::{partial_sibling, x264_aac, Capabilities};
use crate::map::{draw_base, unwrap_longitudes, RouteFit};
use crate::resolution::Resolution;
use crate::MediaError;
use image::{Rgb, RgbImage, RgbaImage};
use std::io::Write;
use std::path::Path;
use triptape_core::route_stops::{
    animated_stop_count, closing_map_secs, RouteStop, ROUTE_ANIM_MAX_STOPS,
};

/// Output frame rate (matches the other hero segments).
pub const FPS: u32 = 30;
/// Stops closer than this on screen (1080p design pixels) are merged.
pub const MIN_STOP_SEPARATION_PX: f64 = 48.0;
/// Pause at each stop is this share of the recap, split across the stops,
/// clamped to the range below.
const PAUSE_SHARE: f64 = 0.45;
pub const MIN_PAUSE_SECS: f64 = 0.3;
pub const MAX_PAUSE_SECS: f64 = 0.6;
/// Hold on the finished route after the last arrival.
pub const TAIL_SECS: f64 = 0.6;
/// A leg's time share is its length plus this fraction of the mean leg, so
/// short hops do not flash by while long legs stay quick.
const LEG_FLOOR_SHARE: f64 = 0.5;
/// Sea-leg dash lengths (1080p design pixels).
const SEA_DASH_ON: f64 = 12.0;
const SEA_DASH_OFF: f64 = 10.0;
/// Label fade-in after the dot arrives.
pub const LABEL_FADE_SECS: f64 = 0.25;
/// Shortest a leg may take when the budget is very tight.
const MIN_LEG_SECS: f64 = 0.4;

const GLOW: Rgb<u8> = Rgb([146, 102, 30]);
const CORE: Rgb<u8> = Rgb([245, 178, 66]);
const PIN_RING: Rgb<u8> = Rgb([255, 255, 255]);
const PIN_STOP: Rgb<u8> = Rgb([94, 234, 212]);
const PIN_START: Rgb<u8> = Rgb([52, 199, 123]);
const PIN_END: Rgb<u8> = Rgb([235, 87, 87]);

/// Cubic ease-in-out on `0..=1`: slow out of a stop, slow into the next.
pub fn ease_in_out(u: f64) -> f64 {
    let u = u.clamp(0.0, 1.0);
    if u < 0.5 {
        4.0 * u * u * u
    } else {
        1.0 - (-2.0 * u + 2.0).powi(3) / 2.0
    }
}

/// A stop on screen: pixel position and optional place label.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Stop {
    pub pos: (f64, f64),
    pub label: Option<String>,
    /// The leg to the next stop passed sea days: `Some(ship name)`, empty
    /// when the ship has no name. Drawn dashed with a ship marker.
    pub sea_after: Option<String>,
}

fn dist(a: (f64, f64), b: (f64, f64)) -> f64 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

/// Merge stops closer than `min_sep` pixels to the previous kept stop (the
/// first label in a merged group survives) and thin the result to `max`
/// stops by repeatedly dropping the interior stop whose removal shortens the
/// detour least. The first stop and the trip's last stop always remain.
pub(crate) fn merge_and_thin(stops: &[Stop], min_sep: f64, max: usize) -> Vec<Stop> {
    let Some(last) = stops.len().checked_sub(1) else {
        return vec![];
    };
    // Greedy groups around a representative.
    let mut groups: Vec<(usize, Vec<usize>)> = vec![]; // (rep, members)
    for (i, s) in stops.iter().enumerate() {
        match groups.last_mut() {
            Some((rep, members)) if dist(stops[*rep].pos, s.pos) < min_sep => members.push(i),
            _ => groups.push((i, vec![i])),
        }
    }
    // The trip ends where it ends: the last group is represented by the
    // final stop, unless that would pull the start itself away.
    if groups.len() > 1 {
        if let Some((rep, _)) = groups.last_mut() {
            *rep = last;
        }
    }
    let mut out: Vec<Stop> = groups
        .iter()
        .map(|(rep, members)| Stop {
            pos: stops[*rep].pos,
            label: stops[*rep]
                .label
                .clone()
                .or_else(|| members.iter().find_map(|&m| stops[m].label.clone())),
            sea_after: members.iter().find_map(|&m| stops[m].sea_after.clone()),
        })
        .collect();
    while out.len() > max.max(2) {
        let drop = (1..out.len() - 1)
            .map(|i| {
                let detour = dist(out[i - 1].pos, out[i].pos) + dist(out[i].pos, out[i + 1].pos)
                    - dist(out[i - 1].pos, out[i + 1].pos);
                (detour, i)
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, i)| i);
        match drop {
            Some(i) => {
                // The merged leg is a sea leg if either half was.
                let gone = out.remove(i);
                if out[i - 1].sea_after.is_none() {
                    out[i - 1].sea_after = gone.sea_after;
                }
            }
            None => break,
        }
    }
    out
}

/// Time schedule of the recap: when the dot arrives at and leaves each
/// stop, and how far along the path it is at any instant.
#[derive(Debug, Clone)]
pub(crate) struct Timeline {
    /// Path length at each stop.
    cum: Vec<f64>,
    arrive: Vec<f64>,
    depart: Vec<f64>,
    pub total_secs: f64,
}

impl Timeline {
    /// Fit `pts` (two or more stops) into `target_secs`: a pause at every
    /// stop, a closing hold, and the rest shared among the legs by length
    /// with a soft floor, so screen speed stays roughly constant. Each leg
    /// is eased on its own.
    pub fn new(pts: &[(f64, f64)], target_secs: f64) -> Self {
        let n = pts.len();
        assert!(n >= 2, "a recap needs two stops");
        let legs: Vec<f64> = pts.windows(2).map(|w| dist(w[0], w[1])).collect();
        let pause = (PAUSE_SHARE * target_secs / n as f64).clamp(MIN_PAUSE_SECS, MAX_PAUSE_SECS);
        let legs_total =
            (target_secs - n as f64 * pause - TAIL_SECS).max(MIN_LEG_SECS * legs.len() as f64);
        let mean = legs.iter().sum::<f64>() / legs.len() as f64;
        let weights: Vec<f64> = legs.iter().map(|d| d + LEG_FLOOR_SHARE * mean).collect();
        let wsum: f64 = weights.iter().sum();
        let mut cum = vec![0.0];
        let (mut arrive, mut depart) = (vec![0.0], vec![pause]);
        for (d, w) in legs.iter().zip(&weights) {
            cum.push(cum[cum.len() - 1] + d);
            let arr = depart[depart.len() - 1] + legs_total * w / wsum;
            arrive.push(arr);
            depart.push(arr + pause);
        }
        let total_secs = depart[n - 1] + TAIL_SECS;
        depart[n - 1] = total_secs;
        Self {
            cum,
            arrive,
            depart,
            total_secs,
        }
    }

    pub fn stops(&self) -> usize {
        self.cum.len()
    }

    pub fn path_len(&self) -> f64 {
        self.cum[self.cum.len() - 1]
    }

    pub fn arrival_secs(&self, stop: usize) -> f64 {
        self.arrive[stop]
    }

    #[cfg(test)]
    pub fn departure_secs(&self, stop: usize) -> f64 {
        self.depart[stop]
    }

    /// Distance travelled along the path at time `t`.
    pub fn dist_at(&self, t: f64) -> f64 {
        for i in 0..self.cum.len() - 1 {
            if t < self.depart[i] {
                return self.cum[i];
            }
            if t < self.arrive[i + 1] {
                let u = (t - self.depart[i]) / (self.arrive[i + 1] - self.depart[i]);
                return self.cum[i] + ease_in_out(u) * (self.cum[i + 1] - self.cum[i]);
            }
        }
        self.path_len()
    }

    /// Stop the dot is paused at (or has just left behind) at time `t`,
    /// with seconds since it arrived there, when it is paused.
    pub fn pause_at(&self, t: f64) -> Option<(usize, f64)> {
        (0..self.stops())
            .find(|&i| t >= self.arrive[i] && t < self.depart[i])
            .map(|i| (i, t - self.arrive[i]))
    }

    /// Label opacity of `stop` at `t`: 0 before the dot arrives, fading to 1.
    pub fn label_alpha(&self, stop: usize, t: f64) -> f64 {
        ((t - self.arrive[stop]) / LABEL_FADE_SECS).clamp(0.0, 1.0)
    }
}

/// Frames in a recap of `secs`.
pub fn frame_count(secs: f64) -> u32 {
    ((secs * f64::from(FPS)).round() as u32).max(1)
}

/// Top-left corner for each label so that none overlaps another, hides a
/// pin, or leaves the frame. Candidates run above-right, above-left, below
/// and so on; when all collide the first (clamped) one is used. `None`
/// labels stay `None`.
pub(crate) fn place_labels(
    anchors: &[(f64, f64)],
    sizes: &[Option<(u32, u32)>],
    frame: (u32, u32),
    gap: f64,
) -> Vec<Option<(i32, i32)>> {
    let margin = gap;
    let (fw, fh) = (f64::from(frame.0), f64::from(frame.1));
    let mut placed: Vec<(f64, f64, f64, f64)> = vec![]; // x0, y0, x1, y1
    let mut out = vec![];
    for (i, (&(ax, ay), size)) in anchors.iter().zip(sizes).enumerate() {
        let Some((lw, lh)) = *size else {
            out.push(None);
            continue;
        };
        let (lw, lh) = (f64::from(lw), f64::from(lh));
        // Nearest free spot first: around the pin, then farther rings.
        let candidates: Vec<(f64, f64)> = [1.0, 3.0, 6.0, 10.0]
            .iter()
            .flat_map(|&m| {
                let g = gap * m;
                [
                    (ax + g, ay - g - lh),
                    (ax - g - lw, ay - g - lh),
                    (ax + g, ay + g),
                    (ax - g - lw, ay + g),
                    (ax - lw / 2.0, ay - g - lh),
                    (ax - lw / 2.0, ay + g),
                    (ax + g, ay - lh / 2.0),
                    (ax - g - lw, ay - lh / 2.0),
                ]
            })
            .collect();
        let clamp = |(x, y): (f64, f64)| {
            (
                x.clamp(margin, (fw - lw - margin).max(margin)),
                y.clamp(margin, (fh - lh - margin).max(margin)),
            )
        };
        let free = |(x, y): (f64, f64)| {
            let r = (x, y, x + lw, y + lh);
            let hits = |o: &(f64, f64, f64, f64)| r.0 < o.2 && o.0 < r.2 && r.1 < o.3 && o.1 < r.3;
            // Keep clear of every pin (a pin is ~ gap/2 across).
            let pin_hit = anchors.iter().enumerate().any(|(j, &(px, py))| {
                let pad = gap * 0.6;
                j != i && px > r.0 - pad && px < r.2 + pad && py > r.1 - pad && py < r.3 + pad
            });
            !placed.iter().any(hits) && !pin_hit
        };
        let pick = candidates
            .iter()
            .map(|&c| clamp(c))
            .find(|&c| free(c))
            .unwrap_or_else(|| clamp(candidates[0]));
        placed.push((pick.0, pick.1, pick.0 + lw, pick.1 + lh));
        out.push(Some((pick.0.round() as i32, pick.1.round() as i32)));
    }
    out
}

/// Blend `col` into one pixel with `alpha` coverage.
fn blend_px(img: &mut RgbImage, x: i32, y: i32, col: Rgb<u8>, alpha: f64) {
    if alpha <= 0.0 || x < 0 || y < 0 || x as u32 >= img.width() || y as u32 >= img.height() {
        return;
    }
    let a = alpha.min(1.0);
    let px = img.get_pixel_mut(x as u32, y as u32);
    for c in 0..3 {
        px.0[c] = (f64::from(px.0[c]) * (1.0 - a) + f64::from(col.0[c]) * a).round() as u8;
    }
}

/// Anti-aliased disc (`ring_width` of `None`) or ring centred on `c`.
fn blend_shape(
    img: &mut RgbImage,
    c: (f64, f64),
    r: f64,
    ring: Option<f64>,
    col: Rgb<u8>,
    alpha: f64,
) {
    let reach = r + ring.unwrap_or(0.0) / 2.0 + 1.5;
    let (x0, x1) = ((c.0 - reach).floor() as i32, (c.0 + reach).ceil() as i32);
    let (y0, y1) = ((c.1 - reach).floor() as i32, (c.1 + reach).ceil() as i32);
    for y in y0..=y1 {
        for x in x0..=x1 {
            let d = dist((f64::from(x) + 0.5, f64::from(y) + 0.5), c);
            let cov = match ring {
                None => r + 0.5 - d,
                Some(w) => w / 2.0 + 0.5 - (d - r).abs(),
            };
            blend_px(img, x, y, col, cov.clamp(0.0, 1.0) * alpha);
        }
    }
}

fn point_seg_dist(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 <= f64::EPSILON {
        0.0
    } else {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0)
    };
    dist(p, (a.0 + t * dx, a.1 + t * dy))
}

/// Everything needed to draw frames; frames must be requested in time order
/// because the line is accumulated in the canvas.
pub(crate) struct Animator {
    w: u32,
    h: u32,
    /// Stroke/pin scale over 1080p design units.
    k: f64,
    pts: Vec<(f64, f64)>,
    timeline: Timeline,
    base: RgbImage,
    canvas: RgbImage,
    glow_mask: Vec<u8>,
    core_mask: Vec<u8>,
    drawn: f64,
    labels: Vec<Option<RgbaImage>>,
    label_pos: Vec<Option<(i32, i32)>>,
    /// Per leg: dashed sea leg, with the ship's label when it is the first
    /// leg for that ship (sprite and its placement near the leg's middle).
    sea: Vec<bool>,
    sea_labels: Vec<Option<RgbaImage>>,
    sea_label_pos: Vec<Option<(i32, i32)>>,
}

impl Animator {
    pub fn new(
        w: u32,
        h: u32,
        base: RgbImage,
        stops: &[Stop],
        timeline: Timeline,
        labels: Vec<Option<RgbaImage>>,
        sea_labels: Vec<Option<RgbaImage>>,
    ) -> Self {
        let k = f64::from((h / 1080).max(1));
        let pts: Vec<(f64, f64)> = stops.iter().map(|s| s.pos).collect();
        let sea: Vec<bool> = stops[..stops.len() - 1]
            .iter()
            .map(|s| s.sea_after.is_some())
            .collect();
        // Ship labels sit at the middle of their leg; stop labels are
        // placed around them so nothing overlaps.
        let mids: Vec<(f64, f64)> = pts
            .windows(2)
            .map(|w| ((w[0].0 + w[1].0) / 2.0, (w[0].1 + w[1].1) / 2.0))
            .collect();
        let anchors: Vec<(f64, f64)> = pts.iter().chain(&mids).copied().collect();
        let sizes: Vec<Option<(u32, u32)>> = labels
            .iter()
            .chain(&sea_labels)
            .map(|l| l.as_ref().map(|l| (l.width(), l.height())))
            .collect();
        let mut placed = place_labels(&anchors, &sizes, (w, h), 14.0 * k);
        let sea_label_pos = placed.split_off(pts.len());
        let label_pos = placed;
        Self {
            w,
            h,
            k,
            pts,
            timeline,
            canvas: base.clone(),
            base,
            glow_mask: vec![0; (w * h) as usize],
            core_mask: vec![0; (w * h) as usize],
            drawn: 0.0,
            labels,
            label_pos,
            sea,
            sea_labels,
            sea_label_pos,
        }
    }

    /// Point at path distance `d`.
    fn point_at(&self, d: f64) -> (f64, f64) {
        let cum = &self.timeline.cum;
        let i = (0..cum.len() - 1)
            .find(|&i| d <= cum[i + 1])
            .unwrap_or(cum.len() - 2);
        let len = cum[i + 1] - cum[i];
        let u = if len <= 0.0 {
            1.0
        } else {
            ((d - cum[i]) / len).clamp(0.0, 1.0)
        };
        let (a, b) = (self.pts[i], self.pts[i + 1]);
        (a.0 + (b.0 - a.0) * u, a.1 + (b.1 - a.1) * u)
    }

    /// Stamp the route from path distance `from` to `to` into the canvas.
    fn extend_line(&mut self, to: f64) {
        let from = self.drawn;
        if to <= from {
            return;
        }
        let cum = self.timeline.cum.clone();
        for i in 0..cum.len() - 1 {
            let (lo, hi) = (from.max(cum[i]), to.min(cum[i + 1]));
            if lo >= hi {
                continue;
            }
            if self.sea[i] {
                // Dashes keyed to path distance, so they stay put as the
                // line grows frame by frame.
                let (on, period) = (SEA_DASH_ON * self.k, (SEA_DASH_ON + SEA_DASH_OFF) * self.k);
                let mut d = lo;
                while d < hi {
                    let phase = d % period;
                    if phase < on {
                        let end = hi.min(d + (on - phase));
                        let (a, b) = (self.point_at(d), self.point_at(end));
                        self.stamp_capsule(a, b);
                        d = end;
                    } else {
                        d = hi.min(d + (period - phase));
                    }
                }
            } else {
                let (a, b) = (self.point_at(lo), self.point_at(hi));
                self.stamp_capsule(a, b);
            }
        }
        self.drawn = to;
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
                let d = point_seg_dist((f64::from(x) + 0.5, f64::from(y) + 0.5), a, b);
                let cg = ((r_glow + 0.5 - d).clamp(0.0, 1.0) * 255.0) as u8;
                if cg == 0 {
                    continue;
                }
                let cc = ((r_core + 0.5 - d).clamp(0.0, 1.0) * 255.0) as u8;
                let idx = (y as u32 * self.w + x as u32) as usize;
                if cg <= self.glow_mask[idx] && cc <= self.core_mask[idx] {
                    continue;
                }
                self.glow_mask[idx] = self.glow_mask[idx].max(cg);
                self.core_mask[idx] = self.core_mask[idx].max(cc);
                let mut px = *self.base.get_pixel(x as u32, y as u32);
                for (mask, col) in [(self.glow_mask[idx], GLOW), (self.core_mask[idx], CORE)] {
                    let a = f64::from(mask) / 255.0;
                    for c in 0..3 {
                        px.0[c] = (f64::from(px.0[c]) * (1.0 - a) + f64::from(col.0[c]) * a).round()
                            as u8;
                    }
                }
                self.canvas.put_pixel(x as u32, y as u32, px);
            }
        }
    }

    /// Frame at time `t` seconds. Call with non-decreasing `t`.
    pub fn frame(&mut self, t: f64) -> RgbImage {
        let d = self.timeline.dist_at(t);
        self.extend_line(d);
        let mut img = self.canvas.clone();
        let k = self.k;
        let n = self.pts.len();
        // Pins appear as the dot reaches them; start is there from the first frame.
        for i in 0..n {
            if t < self.timeline.arrival_secs(i) {
                continue;
            }
            let c = self.pts[i];
            if i == 0 || i == n - 1 {
                let col = if i == 0 { PIN_START } else { PIN_END };
                blend_shape(&mut img, c, 11.0 * k, None, PIN_RING, 1.0);
                blend_shape(&mut img, c, 8.0 * k, None, col, 1.0);
            } else {
                blend_shape(&mut img, c, 5.0 * k, None, PIN_RING, 1.0);
                blend_shape(&mut img, c, 3.0 * k, None, PIN_STOP, 1.0);
            }
        }
        for i in 0..n {
            let alpha = self.timeline.label_alpha(i, t);
            if let (Some(sprite), Some(pos)) = (&self.labels[i], self.label_pos[i]) {
                if alpha > 0.0 {
                    composite(&mut img, sprite, pos, alpha);
                }
            }
        }
        // Ship marker and name at the middle of a sea leg, once the dot
        // has passed it.
        for i in 0..self.sea.len() {
            let Some(sprite) = &self.sea_labels[i] else {
                continue;
            };
            let len = self.timeline.cum[i + 1] - self.timeline.cum[i];
            let mid = self.timeline.cum[i] + len / 2.0;
            let alpha = ((d - mid) / (0.15 * len)).clamp(0.0, 1.0);
            if alpha > 0.0 {
                if let Some(pos) = self.sea_label_pos[i] {
                    composite(&mut img, sprite, pos, alpha);
                }
                draw_ship(&mut img, self.point_at(mid), k, alpha);
            }
        }
        let dot = self.point_at(d);
        if let Some((_, since)) = self.timeline.pause_at(t) {
            // A soft ring pulses outward while the dot rests at a stop.
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
        blend_shape(&mut img, dot, 16.0 * k, None, CORE, 0.28);
        blend_shape(&mut img, dot, 9.0 * k, None, PIN_RING, 1.0);
        blend_shape(&mut img, dot, 6.0 * k, None, CORE, 1.0);
        img
    }
}

/// Small ship (hull and sail) centred on `c`.
fn draw_ship(img: &mut RgbImage, c: (f64, f64), k: f64, alpha: f64) {
    let hull = [(-9.0, 0.0), (9.0, 0.0), (6.0, 5.0), (-6.0, 5.0)];
    let sail = [(-1.0, -1.0), (-1.0, -12.0), (7.0, -1.0)];
    for (poly, col) in [(&hull[..], PIN_RING), (&sail[..], CORE)] {
        let pts: Vec<(f64, f64)> = poly
            .iter()
            .map(|&(x, y)| (c.0 + x * k, c.1 + (y - 1.0) * k))
            .collect();
        fill_convex(img, &pts, col, alpha);
    }
}

/// Fill a convex polygon (winding in either direction).
fn fill_convex(img: &mut RgbImage, pts: &[(f64, f64)], col: Rgb<u8>, alpha: f64) {
    let (mut x0, mut x1, mut y0, mut y1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for p in pts {
        x0 = x0.min(p.0);
        x1 = x1.max(p.0);
        y0 = y0.min(p.1);
        y1 = y1.max(p.1);
    }
    for y in y0.floor() as i32..=y1.ceil() as i32 {
        for x in x0.floor() as i32..=x1.ceil() as i32 {
            let (px, py) = (f64::from(x) + 0.5, f64::from(y) + 0.5);
            let cross =
                |a: (f64, f64), b: (f64, f64)| (b.0 - a.0) * (py - a.1) - (b.1 - a.1) * (px - a.0);
            let signs: Vec<bool> = (0..pts.len())
                .map(|i| cross(pts[i], pts[(i + 1) % pts.len()]) >= 0.0)
                .collect();
            if signs.iter().all(|&s| s) || signs.iter().all(|&s| !s) {
                blend_px(img, x, y, col, alpha);
            }
        }
    }
}

/// Alpha-composite `sprite` onto `img` at `pos`, scaled by `opacity`.
fn composite(img: &mut RgbImage, sprite: &RgbaImage, pos: (i32, i32), opacity: f64) {
    for (sx, sy, px) in sprite.enumerate_pixels() {
        let a = f64::from(px.0[3]) / 255.0 * opacity;
        if a > 0.0 {
            blend_px(
                img,
                pos.0 + sx as i32,
                pos.1 + sy as i32,
                Rgb([px.0[0], px.0[1], px.0[2]]),
                a,
            );
        }
    }
}

/// Typst source for a transparent stop-label pill (sized to its text).
fn label_doc(text: &str) -> String {
    format!(
        r##"#set page(width: auto, height: auto, margin: 0pt, fill: none)
#set text(font: {fonts}, fill: white, size: 30pt, weight: "bold")
#box(fill: rgb(13, 20, 32, 225), inset: (x: 16pt, y: 9pt), radius: 9pt, stroke: 1.5pt + rgb("#f5b242"), "{text}")"##,
        fonts = crate::titlecard::FONT_STACK,
        text = crate::titlecard::esc(text),
    )
}

/// Outcome of a recap render.
#[derive(Debug, Clone)]
pub struct RecapReport {
    /// Length of the written segment.
    pub secs: f64,
    /// Non-fatal notes (e.g. labels skipped), for the render's fallbacks.
    pub notes: Vec<String>,
}

/// Render the animated closing recap of `route` (the plan's polyline)
/// through `stops` at `res` into `out_mp4`: H.264 with stereo silence, like
/// the other map segments. Errors when fewer than two stops remain after
/// merging close ones; the caller then keeps the static closing card.
pub fn render_route_recap(
    caps: &Capabilities,
    route: &[(f64, f64)],
    stops: &[RouteStop],
    res: Resolution,
    typst_bin: &str,
    work_dir: &Path,
    out_mp4: &Path,
) -> Result<RecapReport, MediaError> {
    let (w, h) = res.dims();
    render_recap_sized(caps, route, stops, (w, h), typst_bin, work_dir, out_mp4)
}

pub(crate) fn render_recap_sized(
    caps: &Capabilities,
    route: &[(f64, f64)],
    stops: &[RouteStop],
    (w, h): (u32, u32),
    typst_bin: &str,
    work_dir: &Path,
    out_mp4: &Path,
) -> Result<RecapReport, MediaError> {
    let mut notes = vec![];
    let (mut animator, total) =
        build_animator(route, stops, (w, h), typst_bin, work_dir, &mut notes)?;
    let frames = frame_count(total);
    let secs = f64::from(frames) / f64::from(FPS);
    encode(caps, (w, h), frames, secs, out_mp4, |f| {
        animator.frame(f64::from(f) / f64::from(FPS))
    })?;
    Ok(RecapReport { secs, notes })
}

/// Project, merge and schedule the stops and prepare label sprites.
fn build_animator(
    route: &[(f64, f64)],
    stops: &[RouteStop],
    (w, h): (u32, u32),
    typst_bin: &str,
    work_dir: &Path,
    notes: &mut Vec<String>,
) -> Result<(Animator, f64), MediaError> {
    let route = unwrap_longitudes(route);
    if route.is_empty() {
        return Err(MediaError::InvalidArg("recap needs a route".into()));
    }
    let k = f64::from((h / 1080).max(1));
    let fit = RouteFit::new(&route, w, h);
    // Real stops become pins; sea days (a ship's name over open water) fold
    // into the leg after the previous stop, which is drawn dashed.
    let mut screen: Vec<Stop> = vec![];
    for s in stops {
        let Some(&gps) = route.get(s.route_index) else {
            continue;
        };
        if s.at_sea {
            if let Some(prev) = screen.last_mut() {
                if prev.sea_after.is_none() {
                    prev.sea_after = Some(s.label.clone().unwrap_or_default());
                }
            }
            continue;
        }
        screen.push(Stop {
            pos: fit.project_f(gps),
            label: s.label.clone(),
            sea_after: None,
        });
    }
    let merged = merge_and_thin(&screen, MIN_STOP_SEPARATION_PX * k, ROUTE_ANIM_MAX_STOPS);
    if merged.len() < 2 {
        return Err(MediaError::InvalidArg(
            "fewer than two distinct stops for the recap".into(),
        ));
    }
    // The plan budgeted the recap from the chapter count; fill exactly that.
    let target = closing_map_secs(animated_stop_count(stops));
    let pts: Vec<(f64, f64)> = merged.iter().map(|s| s.pos).collect();
    let timeline = Timeline::new(&pts, target);
    let total = timeline.total_secs;

    // Each ship is named once, on its first sea leg.
    let mut seen_ships: Vec<&str> = vec![];
    let sea_names: Vec<Option<&str>> = merged[..merged.len() - 1]
        .iter()
        .map(|s| {
            let name = s.sea_after.as_deref().filter(|n| !n.is_empty())?;
            if seen_ships.contains(&name) {
                return None;
            }
            seen_ships.push(name);
            Some(name)
        })
        .collect();
    std::fs::create_dir_all(work_dir).map_err(|e| MediaError::Io(e.to_string()))?;
    let mut sprite = |tag: String, text: Option<&str>| -> Option<RgbaImage> {
        let text = text?;
        let png = work_dir.join(format!("recap_label_{tag}.png"));
        let made =
            crate::titlecard::render_text_png(typst_bin, &label_doc(text), &png, 72 * k as u32)
                .and_then(|()| {
                    image::open(&png)
                        .map(|im| im.to_rgba8())
                        .map_err(|e| MediaError::Io(format!("read label: {e}")))
                });
        let _ = std::fs::remove_file(&png);
        match made {
            Ok(img) => Some(img),
            Err(e) => {
                notes.push(format!("recap label {text:?} skipped: {e}"));
                None
            }
        }
    };
    let sprites: Vec<Option<RgbaImage>> = merged
        .iter()
        .enumerate()
        .map(|(i, s)| sprite(format!("{i:02}"), s.label.as_deref()))
        .collect();
    let sea_sprites: Vec<Option<RgbaImage>> = sea_names
        .iter()
        .enumerate()
        .map(|(i, n)| sprite(format!("sea{i:02}"), *n))
        .collect();
    let base = draw_base(&route, w, h);
    Ok((
        Animator::new(w, h, base, &merged, timeline, sprites, sea_sprites),
        total,
    ))
}

/// Pipe `frames` raw RGB frames from `frame_at` into ffmpeg and write
/// `out_mp4` atomically (partial file, then rename).
fn encode(
    caps: &Capabilities,
    (w, h): (u32, u32),
    frames: u32,
    secs: f64,
    out_mp4: &Path,
    mut frame_at: impl FnMut(u32) -> RgbImage,
) -> Result<(), MediaError> {
    use std::process::{Command, Stdio};
    caps.progress.check_cancelled()?;
    if let Some(dir) = out_mp4.parent() {
        std::fs::create_dir_all(dir).map_err(|e| MediaError::Io(e.to_string()))?;
    }
    let partial = partial_sibling(out_mp4);
    let mut args: Vec<String> = [
        "-y",
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "rgb24",
    ]
    .map(String::from)
    .to_vec();
    args.extend([
        "-s".into(),
        format!("{w}x{h}"),
        "-r".into(),
        FPS.to_string(),
        "-i".into(),
        "pipe:0".into(),
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        format!("anullsrc=r=48000:cl=stereo:d={secs}"),
    ]);
    if let Some(n) = caps.threads {
        args.extend(["-threads".into(), n.max(1).to_string()]);
    }
    args.extend(x264_aac("veryfast", 19));
    args.extend(["-shortest".into(), partial.to_string_lossy().into_owned()]);
    let mut child = Command::new(&caps.ffmpeg_bin)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| MediaError::Ffmpeg(format!("spawn {}: {e}", caps.ffmpeg_bin)))?;
    let mut stdin = child.stdin.take();
    let mut stderr = child.stderr.take();
    let err_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(e) = stderr.as_mut() {
            let _ = std::io::Read::read_to_end(e, &mut buf);
        }
        buf
    });
    // A writer thread keeps ffmpeg fed while the next frame is drawn.
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(3);
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        let Some(mut stdin) = stdin.take() else {
            return Ok(());
        };
        for buf in rx {
            stdin.write_all(&buf)?;
        }
        Ok(())
    });
    let mut cancelled = false;
    for f in 0..frames {
        if f % 8 == 0 && caps.progress.is_cancelled() {
            cancelled = true;
            break;
        }
        if tx.send(frame_at(f).into_raw()).is_err() {
            break; // writer stopped: ffmpeg died; its stderr says why
        }
    }
    drop(tx);
    if cancelled {
        let _ = child.kill();
    }
    let wrote = writer
        .join()
        .unwrap_or_else(|_| Err(std::io::Error::other("writer panicked")));
    let status = child
        .wait()
        .map_err(|e| MediaError::Ffmpeg(e.to_string()))?;
    let stderr = String::from_utf8_lossy(&err_thread.join().unwrap_or_default())
        .trim()
        .to_string();
    if cancelled {
        let _ = std::fs::remove_file(&partial);
        return Err(MediaError::Cancelled);
    }
    if !status.success() || wrote.is_err() {
        let _ = std::fs::remove_file(&partial);
        return Err(MediaError::Ffmpeg(format!(
            "route recap encode failed ({status}{}): {stderr}",
            wrote.err().map_or(String::new(), |e| format!(", {e}"))
        )));
    }
    std::fs::rename(&partial, out_mp4).map_err(|e| MediaError::Io(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use triptape_core::route_stops::{ROUTE_ANIM_MAX_SECS, ROUTE_ANIM_MIN_SECS};

    fn pts(n: usize) -> Vec<(f64, f64)> {
        (0..n)
            .map(|i| {
                (
                    100.0 + 120.0 * i as f64,
                    300.0 + 60.0 * ((i * 7) % 5) as f64,
                )
            })
            .collect()
    }

    fn stop(x: f64, y: f64, label: Option<&str>) -> Stop {
        Stop {
            pos: (x, y),
            label: label.map(Into::into),
            sea_after: None,
        }
    }

    #[test]
    fn easing_is_symmetric_monotone_and_clamped() {
        assert_eq!(ease_in_out(0.0), 0.0);
        assert_eq!(ease_in_out(1.0), 1.0);
        assert!((ease_in_out(0.5) - 0.5).abs() < 1e-12);
        assert_eq!(ease_in_out(-3.0), 0.0);
        assert_eq!(ease_in_out(9.0), 1.0);
        let mut last = 0.0;
        for i in 0..=100 {
            let v = ease_in_out(f64::from(i) / 100.0);
            assert!(v >= last);
            last = v;
        }
        // Slow at the ends, fast in the middle.
        assert!(ease_in_out(0.1) < 0.1 && ease_in_out(0.9) > 0.9);
    }

    #[test]
    fn timeline_fills_the_budgeted_duration() {
        for n in 2..=ROUTE_ANIM_MAX_STOPS {
            let tl = Timeline::new(&pts(n), closing_map_secs(n));
            assert!(
                (tl.total_secs - closing_map_secs(n)).abs() < 1e-9,
                "{n} stops: {}",
                tl.total_secs
            );
            assert!(
                tl.total_secs >= ROUTE_ANIM_MIN_SECS - 1e-9
                    && tl.total_secs <= ROUTE_ANIM_MAX_SECS + 1e-9
            );
        }
    }

    #[test]
    fn timeline_pauses_at_every_stop_and_progress_is_monotone() {
        let tl = Timeline::new(&pts(5), closing_map_secs(5));
        assert_eq!(tl.dist_at(0.0), 0.0);
        assert_eq!(tl.dist_at(tl.total_secs), tl.path_len());
        for i in 0..5 {
            let (a, d) = (tl.arrival_secs(i), tl.departure_secs(i));
            let pause = d - a;
            if i < 4 {
                assert!(
                    (MIN_PAUSE_SECS - 1e-9..=MAX_PAUSE_SECS + 1e-9).contains(&pause),
                    "{pause}"
                );
            } else {
                assert!(pause >= TAIL_SECS);
            }
            // Parked for the whole pause.
            let at = tl.dist_at(a + 1e-9);
            assert!((tl.dist_at(a + pause * 0.5) - at).abs() < 1e-6);
            assert!((tl.dist_at(d - 1e-9) - at).abs() < 1e-6);
            assert_eq!(tl.pause_at(a + pause / 2.0).map(|p| p.0), Some(i));
        }
        assert!(tl.pause_at(tl.arrival_secs(0) + 0.0).is_some());
        let mut last = 0.0;
        let mut t = 0.0;
        while t <= tl.total_secs {
            let d = tl.dist_at(t);
            assert!(d + 1e-9 >= last);
            last = d;
            t += 1.0 / 60.0;
        }
        // Between pauses the dot is moving.
        let mid = (tl.departure_secs(0) + tl.arrival_secs(1)) / 2.0;
        assert!(tl.dist_at(mid) > 0.0 && tl.dist_at(mid) < tl.dist_at(tl.arrival_secs(1)));
    }

    #[test]
    fn each_leg_eases_in_and_out() {
        let tl = Timeline::new(&pts(3), closing_map_secs(3));
        let (a, b) = (tl.departure_secs(0), tl.arrival_secs(1));
        let step = (b - a) / 20.0;
        let v = |t: f64| (tl.dist_at(t + 1e-3) - tl.dist_at(t)) / 1e-3;
        assert!(v(a + 1e-4) < v(a + 10.0 * step) / 10.0, "slow start");
        assert!(v(b - 2e-3) < v(a + 10.0 * step) / 10.0, "slow arrival");
    }

    #[test]
    fn long_legs_take_longer_but_not_proportionally() {
        // One short hop (60 px) then a long leg (1200 px).
        let tl = Timeline::new(&[(100.0, 100.0), (160.0, 100.0), (1360.0, 100.0)], 8.0);
        let short = tl.arrival_secs(1) - tl.departure_secs(0);
        let long = tl.arrival_secs(2) - tl.departure_secs(1);
        assert!(long > short);
        assert!(long / short < 1200.0 / 60.0 / 2.0, "{long} vs {short}");
    }

    #[test]
    fn tight_budgets_still_leave_room_for_legs() {
        // 40 stops cannot fit any budget; legs keep a floor and total grows.
        let tl = Timeline::new(&pts(40), 4.0);
        assert!(tl.total_secs > 4.0);
        for i in 0..39 {
            assert!(tl.arrival_secs(i + 1) - tl.departure_secs(i) >= MIN_LEG_SECS * 0.2);
        }
    }

    #[test]
    fn close_stops_merge_keeping_the_first_label_and_the_final_stop() {
        let stops = vec![
            stop(100.0, 100.0, Some("Tokyo")),
            stop(110.0, 105.0, Some("Shibuya")),
            stop(500.0, 100.0, None),
            stop(505.0, 100.0, Some("Kyoto")),
            stop(900.0, 400.0, Some("Osaka")),
            stop(910.0, 402.0, Some("Home")),
        ];
        let m = merge_and_thin(&stops, 48.0, 16);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].label.as_deref(), Some("Tokyo"));
        assert_eq!(m[1].label.as_deref(), Some("Kyoto"), "inherits a label");
        // The trip ends at its final stop, not the first of the close pair.
        assert_eq!(m[2].pos, (910.0, 402.0));
        assert_eq!(m[2].label.as_deref(), Some("Home"));
        // Everything on one spot collapses to one stop.
        assert_eq!(merge_and_thin(&stops[..2], 48.0, 16).len(), 1);
        assert!(merge_and_thin(&[], 48.0, 16).is_empty());
    }

    #[test]
    fn many_stops_thin_to_the_cap_dropping_the_least_detour() {
        let mut stops: Vec<Stop> = (0..30)
            .map(|i| stop(100.0 + 60.0 * f64::from(i), 300.0, None))
            .collect();
        // One genuine corner that must survive thinning.
        stops[10].pos.1 = 800.0;
        let m = merge_and_thin(&stops, 48.0, 8);
        assert_eq!(m.len(), 8);
        assert_eq!(m[0].pos, stops[0].pos);
        assert_eq!(m[7].pos, stops[29].pos);
        assert!(m.iter().any(|s| s.pos.1 == 800.0), "the corner stays");
    }

    #[test]
    fn labels_fade_in_when_the_dot_arrives() {
        let tl = Timeline::new(&pts(4), closing_map_secs(4));
        for i in 0..4 {
            let a = tl.arrival_secs(i);
            assert_eq!(tl.label_alpha(i, a - 0.01), 0.0);
            assert!(tl.label_alpha(i, a + LABEL_FADE_SECS / 2.0) > 0.4);
            assert_eq!(tl.label_alpha(i, a + LABEL_FADE_SECS), 1.0);
            assert_eq!(tl.label_alpha(i, tl.total_secs), 1.0);
            if i > 0 {
                assert!(a > tl.arrival_secs(i - 1));
            }
        }
    }

    #[test]
    fn labels_avoid_each_other_pins_and_the_frame_edge() {
        let anchors = [
            (300.0, 200.0),
            (330.0, 210.0),
            (1900.0, 1070.0),
            (40.0, 30.0),
        ];
        let sizes = [Some((220, 50)), Some((220, 50)), Some((240, 50)), None];
        let pos = place_labels(&anchors, &sizes, (1920, 1080), 14.0);
        assert_eq!(pos[3], None);
        let rects: Vec<(i32, i32, i32, i32)> = (0..3)
            .map(|i| {
                let p = pos[i].unwrap();
                let s = sizes[i].unwrap();
                assert!(p.0 >= 0 && p.1 >= 0, "{p:?}");
                assert!(
                    p.0 + s.0 as i32 <= 1920 && p.1 + s.1 as i32 <= 1080,
                    "{p:?}"
                );
                (p.0, p.1, p.0 + s.0 as i32, p.1 + s.1 as i32)
            })
            .collect();
        for (i, a) in rects.iter().enumerate() {
            for b in &rects[i + 1..] {
                assert!(
                    a.2 <= b.0 || b.2 <= a.0 || a.3 <= b.1 || b.3 <= a.1,
                    "{a:?} {b:?}"
                );
            }
        }
        // Label 0 does not sit on stop 1's pin.
        let r0 = rects[0];
        assert!(!(330 > r0.0 && 330 < r0.2 && 210 > r0.1 && 210 < r0.3));
    }

    #[test]
    fn frame_count_is_duration_times_fps() {
        assert_eq!(frame_count(8.4), 252);
        assert_eq!(frame_count(6.0), 180);
        assert_eq!(frame_count(0.0), 1);
        for n in 2..=ROUTE_ANIM_MAX_STOPS {
            let tl = Timeline::new(&pts(n), closing_map_secs(n));
            let f = frame_count(tl.total_secs);
            assert!((f64::from(f) / f64::from(FPS) - tl.total_secs).abs() <= 0.5 / f64::from(FPS));
        }
    }

    fn trip() -> (Vec<(f64, f64)>, Vec<RouteStop>) {
        let route = vec![
            (35.7, 139.8),
            (35.0, 135.7),
            (34.7, 135.5),
            (25.0, 121.5),
            (1.35, 103.8),
        ];
        let stops = [0usize, 3, 4]
            .iter()
            .map(|&i| RouteStop {
                route_index: i,
                label: Some(format!("Stop {i}")),
                at_sea: false,
            })
            .collect();
        (route, stops)
    }

    /// Trail pixels: the glow tone only the route line paints.
    fn trail_px(img: &RgbImage) -> usize {
        img.pixels().filter(|p| **p == GLOW).count()
    }

    #[test]
    fn first_frame_has_no_route_and_last_frame_has_all_of_it() {
        let (route, stops) = trip();
        let dir = tempfile::tempdir().unwrap();
        let (mut an, total) = build_animator(
            &route,
            &stops,
            (480, 270),
            "no-such-typst",
            dir.path(),
            &mut vec![],
        )
        .unwrap();
        let frames = frame_count(total);
        let mut counts = vec![];
        for f in 0..frames {
            counts.push(trail_px(&an.frame(f64::from(f) / f64::from(FPS))));
        }
        assert_eq!(counts[0], 0, "first frame draws no line");
        assert!(counts[frames as usize - 1] > 500);
        // The line only ever grows.
        assert!(
            counts.windows(2).all(|w| w[1] + 8 >= w[0]),
            "monotone growth"
        );
        // Last frame matches a frame held after the end.
        let last = an.frame(f64::from(frames - 1) / f64::from(FPS));
        let after = an.frame(total + 5.0);
        // (the resting pulse ring may tint a few pixels)
        assert!(trail_px(&last).abs_diff(trail_px(&after)) < 20);
    }

    #[test]
    fn single_spot_trips_cannot_animate() {
        let route = vec![(35.7, 139.8), (35.7001, 139.8001)];
        let stops: Vec<RouteStop> = (0..2)
            .map(|i| RouteStop {
                route_index: i,
                label: None,
                at_sea: false,
            })
            .collect();
        let dir = tempfile::tempdir().unwrap();
        let r = build_animator(&route, &stops, (480, 270), "x", dir.path(), &mut vec![]);
        assert!(r.is_err());
    }

    fn sea_stop(index: usize, ship: &str) -> RouteStop {
        RouteStop {
            route_index: index,
            label: Some(ship.into()),
            at_sea: true,
        }
    }

    fn port(index: usize, name: &str) -> RouteStop {
        RouteStop {
            route_index: index,
            label: Some(name.into()),
            at_sea: false,
        }
    }

    #[test]
    fn sea_days_become_a_dashed_leg_and_the_ship_is_named_once() {
        let route = vec![
            (35.7, 139.8),
            (30.0, 130.0),
            (25.0, 121.5),
            (15.0, 112.0),
            (1.35, 103.8),
        ];
        let stops = vec![
            port(0, "Tokyo"),
            sea_stop(1, "Ship One"),
            port(2, "Keelung"),
            sea_stop(3, "Ship One"),
            port(4, "Singapore"),
        ];
        let dir = tempfile::tempdir().unwrap();
        let mut notes = vec![];
        let (an, total) = build_animator(
            &route,
            &stops,
            (960, 540),
            "no-such-typst",
            dir.path(),
            &mut notes,
        )
        .unwrap();
        // Three pins, two legs, both dashed; sea days are not stops.
        assert_eq!(an.pts.len(), 3);
        assert_eq!(an.sea, [true, true]);
        // Budget counts the three real stops.
        assert!((total - closing_map_secs(3)).abs() < 1e-9);
        // The ship is requested once (typst is absent, so it is noted).
        let ship_notes = notes.iter().filter(|n| n.contains("Ship One")).count();
        assert_eq!(ship_notes, 1, "{notes:?}");
    }

    #[test]
    fn dashed_legs_draw_less_line_than_solid_ones() {
        let route = vec![(35.7, 139.8), (25.0, 121.5), (1.35, 103.8)];
        let dir = tempfile::tempdir().unwrap();
        let solid = vec![port(0, "A"), port(1, "B"), port(2, "C")];
        let dashed = vec![port(0, "A"), sea_stop(1, "S"), port(2, "C")];
        let mut counts = vec![];
        for stops in [&solid, &dashed] {
            let (mut an, total) = build_animator(
                &route,
                stops,
                (960, 540),
                "no-such-typst",
                dir.path(),
                &mut vec![],
            )
            .unwrap();
            counts.push(trail_px(&an.frame(total)));
        }
        assert!(counts[1] > 100, "dashes are drawn");
        // Dashed: a 12-on/10-off pattern over one long leg, vs two solid legs.
        assert!(counts[1] < counts[0], "{counts:?}");
    }

    #[test]
    fn merging_keeps_sea_legs() {
        let mut a = stop(100.0, 100.0, Some("A"));
        a.sea_after = Some("Ship".into());
        let stops = vec![
            a,
            stop(110.0, 100.0, None),
            stop(500.0, 100.0, Some("B")),
            stop(900.0, 100.0, Some("C")),
        ];
        let m = merge_and_thin(&stops, 48.0, 16);
        assert_eq!(m[0].sea_after.as_deref(), Some("Ship"));
        // Thinning folds a dropped stop's sea leg into the merged leg.
        let mut b = stop(500.0, 100.0, Some("B"));
        b.sea_after = Some("Ship".into());
        let t = merge_and_thin(
            &[stop(100.0, 100.0, None), b, stop(900.0, 100.0, None)],
            48.0,
            2,
        );
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].sea_after.as_deref(), Some("Ship"));
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
    fn real_ffmpeg_recap_has_the_right_length_first_and_last_frames() {
        let Some(caps) = test_caps() else { return };
        let (route, stops) = trip();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("recap.mp4");
        let (w, h) = (320u32, 180u32);
        let rep = render_recap_sized(
            &caps,
            &route,
            &stops,
            (w, h),
            "no-such-typst",
            dir.path(),
            &out,
        )
        .unwrap();
        // Labels were requested but typst is absent: noted, not fatal.
        assert_eq!(rep.notes.len(), 3, "{:?}", rep.notes);
        assert!((rep.secs - closing_map_secs(3)).abs() < 1e-9);
        // Decode every frame to raw RGB: count is duration x fps.
        let raw = std::process::Command::new(&caps.ffmpeg_bin)
            .args(["-v", "error", "-i"])
            .arg(&out)
            .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
            .output()
            .unwrap();
        assert!(raw.status.success());
        let frame_len = (w * h * 3) as usize;
        let n = raw.stdout.len() / frame_len;
        assert_eq!(raw.stdout.len() % frame_len, 0);
        assert_eq!(n as u32, frame_count(rep.secs));
        // Route pixels ~ glow tone (h264 shifts colours a little).
        let near_glow = |frame: &[u8]| {
            frame
                .as_chunks::<3>()
                .0
                .iter()
                .filter(|p| {
                    (i32::from(p[0]) - 146).abs() < 16
                        && (i32::from(p[1]) - 102).abs() < 16
                        && (i32::from(p[2]) - 30).abs() < 16
                })
                .count()
        };
        let first = &raw.stdout[..frame_len];
        let last = &raw.stdout[(n - 1) * frame_len..];
        // Dot edges blend through similar tones, so compare rather than
        // demand zero: the first frame has no line, the last has all of it.
        let (n_first, n_last) = (near_glow(first), near_glow(last));
        assert!(n_last > 150, "last frame shows the route: {n_last}");
        assert!(n_first * 20 < n_last, "first frame has no route: {n_first}");
        // Audio track present and matching length.
        let probe = std::process::Command::new(&caps.ffprobe_bin)
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_type,duration",
                "-of",
                "csv=p=0",
            ])
            .arg(&out)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&probe.stdout).to_string();
        assert!(text.contains("audio") && text.contains("video"), "{text}");
        // No partial file left behind.
        assert!(!partial_sibling(&out).exists());
    }

    #[test]
    fn cancelled_recap_leaves_no_file() {
        let Some(mut caps) = test_caps() else { return };
        caps.progress = crate::progress::Progress::default();
        caps.progress.cancel();
        let (route, stops) = trip();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("recap.mp4");
        let r = render_recap_sized(&caps, &route, &stops, (320, 180), "x", dir.path(), &out);
        assert!(matches!(r, Err(MediaError::Cancelled)));
        assert!(!out.exists() && !partial_sibling(&out).exists());
    }

    /// Real-trip check (run by hand, release build):
    /// `TRIPTAPE_RECAP_DB=copy.db TRIPTAPE_RECAP_OUT=dir [TRIPTAPE_BENCH_RES=1080p]
    /// [TRIPTAPE_FFMPEG=..] cargo test --release -p triptape-media real_trip_recap -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a trip db; see doc comment"]
    fn real_trip_recap() {
        let db = std::env::var("TRIPTAPE_RECAP_DB").expect("TRIPTAPE_RECAP_DB");
        let out_dir = std::path::PathBuf::from(
            std::env::var("TRIPTAPE_RECAP_OUT").expect("TRIPTAPE_RECAP_OUT"),
        );
        let res = Resolution::from_flag(
            &std::env::var("TRIPTAPE_BENCH_RES").unwrap_or_else(|_| "4k".into()),
        )
        .unwrap();
        let caps = test_caps().expect("ffmpeg with libx264");
        let loaded = triptape_core::db::Db::open(Path::new(&db))
            .unwrap()
            .edited_plan(None)
            .unwrap()
            .expect("plan in db");
        let plan = loaded.edited;
        let stops = triptape_core::route_stops::chapter_route_stops(
            &plan.chapters,
            &loaded.assets,
            &plan.map_polyline,
        );
        eprintln!(
            "chapters={} polyline={} stops={:?}",
            plan.chapters.len(),
            plan.map_polyline.len(),
            stops.iter().map(|s| s.label.clone()).collect::<Vec<_>>()
        );
        std::fs::create_dir_all(&out_dir).unwrap();
        let out = out_dir.join(format!("recap_{}.mp4", res.label()));
        let t = std::time::Instant::now();
        let rep = render_route_recap(
            &caps,
            &plan.map_polyline,
            &stops,
            res,
            "typst",
            &out_dir,
            &out,
        )
        .unwrap();
        eprintln!(
            "RECAP {}: {:.1}s of video, rendered in {:.1}s, notes {:?}",
            res.label(),
            rep.secs,
            t.elapsed().as_secs_f64(),
            rep.notes
        );
    }
}
