//! Sea paths that stay off the land: a coarse A* over the bundled land
//! grid, string-pulled to a few waypoints. Ships follow these, not the
//! straight line between ports (Tokyo to Osaka would cross Honshu).

use crate::geo::land_fraction;
use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// Open water: the raw grid says under half a cell of land here.
pub fn is_water(p: (f64, f64)) -> bool {
    land_fraction(p) < 0.5
}

/// Whether the straight segment `a`-`b` stays on water, sampled every
/// 0.05 degrees (the land grid's cell size).
pub fn segment_clear(a: (f64, f64), b: (f64, f64)) -> bool {
    let steps = ((b.0 - a.0).abs().max((b.1 - a.1).abs()) / 0.05)
        .ceil()
        .max(1.0) as u32;
    (0..=steps).all(|i| {
        let t = f64::from(i) / f64::from(steps);
        is_water((a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t))
    })
}

#[derive(PartialEq)]
struct Node(f64, usize);
impl Eq for Node {}
impl Ord for Node {
    fn cmp(&self, o: &Self) -> Ordering {
        o.0.total_cmp(&self.0) // min-heap
    }
}
impl PartialOrd for Node {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

/// A sea path from `a` to `b` (both included): straight when that is all
/// water, else the A* route around the land, string-pulled. The ends are
/// kept as given even when a port sits on a coastal land cell.
pub fn route(a: (f64, f64), b: (f64, f64)) -> Vec<(f64, f64)> {
    if segment_clear(a, b) || (b.1 - a.1).abs() > 170.0 {
        return vec![a, b];
    }
    let extent = (b.0 - a.0).abs().max((b.1 - a.1).abs());
    let margin = (0.5 * extent).max(3.0);
    let (lat0, lat1) = (a.0.min(b.0) - margin, a.0.max(b.0) + margin);
    let (lon0, lon1) = (a.1.min(b.1) - margin, a.1.max(b.1) + margin);
    let cell = (extent + 2.0 * margin).max(1.0) / 300.0;
    let cell = cell.max(0.1);
    let nx = ((lon1 - lon0) / cell).ceil() as i64 + 1;
    let ny = ((lat1 - lat0) / cell).ceil() as i64 + 1;
    let center = |i: usize| {
        let (x, y) = ((i as i64 % nx) as f64, (i as i64 / nx) as f64);
        (lat0 + (y + 0.5) * cell, lon0 + (x + 0.5) * cell)
    };
    let index = |p: (f64, f64)| {
        let x = (((p.1 - lon0) / cell) as i64).clamp(0, nx - 1);
        let y = (((p.0 - lat0) / cell) as i64).clamp(0, ny - 1);
        (y * nx + x) as usize
    };
    let water: Vec<bool> = (0..(nx * ny) as usize)
        .map(|i| is_water(center(i)))
        .collect();
    // Nearest water cell to an endpoint on (or beside) land.
    let snap = |p: (f64, f64)| -> Option<usize> {
        let start = index(p);
        if water[start] {
            return Some(start);
        }
        let (sx, sy) = (start as i64 % nx, start as i64 / nx);
        (1..=40_i64).find_map(|r| {
            (-r..=r)
                .flat_map(|dy| (-r..=r).map(move |dx| (dx, dy)))
                .filter(|(dx, dy)| dx.abs().max(dy.abs()) == r)
                .map(|(dx, dy)| (sx + dx, sy + dy))
                .filter(|&(x, y)| (0..nx).contains(&x) && (0..ny).contains(&y))
                .map(|(x, y)| (y * nx + x) as usize)
                .filter(|&i| water[i])
                .min_by(|&p, &q| {
                    let d = |i: usize| {
                        let c = center(i);
                        (c.0 - a.0).powi(2) + (c.1 - a.1).powi(2)
                    };
                    d(p).total_cmp(&d(q))
                })
        })
    };
    let (Some(s), Some(g)) = (snap(a), snap(b)) else {
        return vec![a, b];
    };
    let octile = |i: usize| {
        let (dx, dy) = (
            ((i as i64 % nx) - (g as i64 % nx)).abs(),
            ((i as i64 / nx) - (g as i64 / nx)).abs(),
        );
        dx.max(dy) as f64 + (std::f64::consts::SQRT_2 - 1.0) * dx.min(dy) as f64
    };
    let mut cost = vec![f64::INFINITY; water.len()];
    let mut from = vec![usize::MAX; water.len()];
    let mut heap = BinaryHeap::new();
    cost[s] = 0.0;
    heap.push(Node(octile(s), s));
    while let Some(Node(_, i)) = heap.pop() {
        if i == g {
            break;
        }
        let (x, y) = (i as i64 % nx, i as i64 / nx);
        for (dx, dy) in [
            (1, 0),
            (-1, 0),
            (0, 1),
            (0, -1),
            (1, 1),
            (1, -1),
            (-1, 1),
            (-1, -1),
        ] {
            let (nxp, nyp) = (x + dx, y + dy);
            if !(0..nx).contains(&nxp) || !(0..ny).contains(&nyp) {
                continue;
            }
            let j = (nyp * nx + nxp) as usize;
            if !water[j] {
                continue;
            }
            let step = if dx != 0 && dy != 0 {
                std::f64::consts::SQRT_2
            } else {
                1.0
            };
            if cost[i] + step < cost[j] {
                cost[j] = cost[i] + step;
                from[j] = i;
                heap.push(Node(cost[j] + octile(j), j));
            }
        }
    }
    if from[g] == usize::MAX && s != g {
        return vec![a, b];
    }
    let mut cells = vec![g];
    while let Some(&last) = cells.last() {
        if last == s {
            break;
        }
        cells.push(from[last]);
    }
    cells.reverse();
    let mut pts: Vec<(f64, f64)> = std::iter::once(a)
        .chain(cells.iter().map(|&i| center(i)))
        .chain(std::iter::once(b))
        .collect();
    pts.dedup();
    string_pull(&pts)
}

/// Greedy line-of-sight smoothing: from each kept point jump to the
/// farthest later point still reachable over water.
fn string_pull(pts: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut out = vec![pts[0]];
    let mut i = 0;
    while i < pts.len() - 1 {
        let next = (i + 1..pts.len())
            .rev()
            .find(|&j| segment_clear(pts[i], pts[j]))
            .unwrap_or(i + 1);
        out.push(pts[next]);
        i = next;
    }
    out
}

/// Ramer-Douglas-Peucker in degree space.
pub fn simplify(pts: &[(f64, f64)], tol: f64) -> Vec<(f64, f64)> {
    if pts.len() < 3 {
        return pts.to_vec();
    }
    let (a, b) = (pts[0], pts[pts.len() - 1]);
    let (idx, far) = pts[1..pts.len() - 1]
        .iter()
        .enumerate()
        .map(|(i, &p)| (i + 1, seg_dist(p, a, b)))
        .max_by(|x, y| x.1.total_cmp(&y.1))
        .unwrap_or((0, 0.0));
    if far <= tol {
        return vec![a, b];
    }
    let mut left = simplify(&pts[..=idx], tol);
    left.pop();
    left.extend(simplify(&pts[idx..], tol));
    left
}

/// Distance in degrees from `p` to segment `a`-`b`.
pub fn seg_dist(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 <= f64::EPSILON {
        0.0
    } else {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0)
    };
    ((p.0 - a.0 - t * dx).powi(2) + (p.1 - a.1 - t * dy).powi(2)).sqrt()
}

/// A ship's path from `a` to `b` through `via` (positions seen on board
/// between the two ports, in time order): the points far enough off the
/// straight line to matter are kept as waypoints (at most six) and each hop
/// is routed around land.
pub fn shape_ship_leg(a: (f64, f64), b: (f64, f64), via: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let len = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
    // Fixes wildly off the corridor are GPS noise, not the ship's track.
    let kept: Vec<(f64, f64)> = via
        .iter()
        .copied()
        .filter(|&p| is_water(p) && seg_dist(p, a, b) <= 0.5 * len + 1.5)
        .collect();
    let mut chain: Vec<(f64, f64)> = std::iter::once(a)
        .chain(kept)
        .chain(std::iter::once(b))
        .collect();
    chain.dedup();
    let mut chain = simplify(&chain, 0.25);
    if chain.len() > 8 {
        let step = (chain.len() - 1) as f64 / 7.0;
        chain = (0..8)
            .map(|i| chain[(f64::from(i) * step).round() as usize])
            .collect();
    }
    let mut out: Vec<(f64, f64)> = vec![chain[0]];
    for w in chain.windows(2) {
        let hop = route(w[0], w[1]);
        out.extend(hop.into_iter().skip(1));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKYO_PORT: (f64, f64) = (35.6177, 139.771);
    const OSAKA: (f64, f64) = (34.656, 135.4306);

    /// Share of sampled points on land (ends excluded: ports sit on coast).
    fn land_share(path: &[(f64, f64)]) -> f64 {
        let (mut n, mut land) = (0, 0);
        for w in path.windows(2) {
            let steps = ((w[1].0 - w[0].0).abs().max((w[1].1 - w[0].1).abs()) / 0.05)
                .ceil()
                .max(1.0) as u32;
            for i in 1..steps {
                let t = f64::from(i) / f64::from(steps);
                let p = (
                    w[0].0 + (w[1].0 - w[0].0) * t,
                    w[0].1 + (w[1].1 - w[0].1) * t,
                );
                n += 1;
                if !is_water(p) {
                    land += 1;
                }
            }
        }
        f64::from(land) / f64::from(n.max(1))
    }

    #[test]
    fn the_straight_line_tokyo_to_osaka_crosses_honshu_the_route_does_not() {
        assert!(
            land_share(&[TOKYO_PORT, OSAKA]) > 0.1,
            "premise: straight is overland"
        );
        let path = route(TOKYO_PORT, OSAKA);
        assert!(path.len() > 2);
        assert_eq!(path[0], TOKYO_PORT);
        assert_eq!(*path.last().unwrap(), OSAKA);
        // Port-side hops may nick the coast; the passage is open water.
        assert!(land_share(&path) < 0.04, "{}", land_share(&path));
        // Not a wild detour.
        let d = |p: (f64, f64), q: (f64, f64)| ((p.0 - q.0).powi(2) + (p.1 - q.1).powi(2)).sqrt();
        let len: f64 = path.windows(2).map(|w| d(w[0], w[1])).sum();
        assert!(len < 3.0 * d(TOKYO_PORT, OSAKA), "{len}");
    }

    #[test]
    fn open_water_stays_straight() {
        let a = (30.0, 150.0);
        let b = (25.0, 160.0);
        assert_eq!(route(a, b), vec![a, b]);
    }

    #[test]
    fn photo_fixes_steer_the_ship_south_around_kyushu() {
        let fukuoka = (33.6036, 130.4038);
        // Fixes off Tanegashima, south of Kyushu.
        let via = [(31.0, 133.0), (30.6, 131.2), (31.3, 130.6)];
        let path = shape_ship_leg(OSAKA, fukuoka, &via);
        assert!(path.iter().any(|p| p.0 < 32.0), "goes south: {path:?}");
        assert!(land_share(&path) < 0.04, "{}", land_share(&path));
        // Noise far off the corridor is ignored.
        let noisy = shape_ship_leg(OSAKA, fukuoka, &[(60.0, 10.0)]);
        assert!(noisy.iter().all(|p| p.0 < 40.0));
    }

    #[test]
    fn simplify_keeps_corners_and_drops_collinear_points() {
        let pts = [(0.0, 0.0), (0.0, 1.0), (0.0, 2.0), (1.0, 2.0)];
        assert_eq!(
            simplify(&pts, 0.1),
            vec![(0.0, 0.0), (0.0, 2.0), (1.0, 2.0)]
        );
        assert_eq!(simplify(&pts[..2], 0.1).len(), 2);
    }
}
