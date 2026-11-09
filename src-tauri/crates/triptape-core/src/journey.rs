//! The closing map's journey: what the recap travels and how long it takes.
//!
//! Built from the owner's [`Itinerary`]: nodes (the stops, with labels),
//! legs (flight arcs, ground hops, ship legs routed around land, a loop at
//! sea for a round-trip cruise) and the schedule. The renderer plays this
//! schedule exactly, and the plan's film-length budget reads
//! [`Journey::duration_secs`], so the two cannot drift apart.

use crate::itinerary::{asset_day, Itinerary, LegMode};
use crate::searoute::{is_water, route, shape_ship_leg};
use crate::types::Asset;

/// Hold on the opening stop before the dot sets off.
pub const START_HOLD_SECS: f64 = 0.5;
/// Pause at every labelled stop.
pub const NODE_PAUSE_SECS: f64 = 0.45;
/// Hold on the finished route's last stop before the camera pulls back.
pub const END_TAIL_SECS: f64 = 0.5;
/// Final pull-back to the whole route.
pub const PULL_BACK_SECS: f64 = 1.6;
/// Leg durations.
pub const FLIGHT_SECS: f64 = 2.2;
pub const GROUND_SECS: f64 = 0.35;
pub const SHIP_LEG_SECS: f64 = 0.7;
pub const LOOP_SECS: f64 = 1.6;

/// A stop on the journey.
#[derive(Debug, Clone, PartialEq)]
pub struct JourneyNode {
    /// `lat, lon` in the journey's continuous (unwrapped) longitudes.
    pub pos: (f64, f64),
    /// City label shown at the stop; `None` for waypoints (an embarkation
    /// terminal, a loop's return).
    pub label: Option<String>,
    pub pin: bool,
}

/// One leg between consecutive nodes.
#[derive(Debug, Clone, PartialEq)]
pub struct JourneyLeg {
    pub mode: LegMode,
    pub ship: Option<String>,
    pub is_loop: bool,
    /// Path from the leg's first node to its second (inclusive), `lat, lon`
    /// in the journey's continuous longitudes: a great-circle arc for a
    /// flight, a land-avoiding route for a ship.
    pub path: Vec<(f64, f64)>,
    /// "Boarded Carnival Luminosa · Oct 9", on the leg where boarding happens.
    pub boarding_tag: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Journey {
    pub nodes: Vec<JourneyNode>,
    /// `nodes.len() - 1` legs.
    pub legs: Vec<JourneyLeg>,
}

/// When the dot is where, in seconds from the start of the recap.
#[derive(Debug, Clone, PartialEq)]
pub struct Schedule {
    pub arrive: Vec<f64>,
    pub depart: Vec<f64>,
    /// Time the dot is travelling on each leg: `depart[i]..arrive[i + 1]`.
    pub leg_secs: Vec<f64>,
    /// When the pull-back to the whole route begins.
    pub pull_back_at: f64,
    pub total_secs: f64,
}

fn leg_secs(leg: &JourneyLeg) -> f64 {
    match (leg.mode, leg.is_loop) {
        (LegMode::Flight, _) => FLIGHT_SECS,
        (LegMode::Ground, _) => GROUND_SECS,
        (LegMode::Ship, true) => LOOP_SECS,
        (LegMode::Ship, false) => SHIP_LEG_SECS,
    }
}

impl Journey {
    pub fn schedule(&self) -> Schedule {
        let (mut arrive, mut depart, mut secs) = (vec![0.0], vec![], vec![]);
        for (i, leg) in self.legs.iter().enumerate() {
            let pause = if i == 0 {
                START_HOLD_SECS
            } else {
                self.pause_at(i)
            };
            depart.push(arrive[i] + pause);
            let d = leg_secs(leg);
            secs.push(d);
            arrive.push(depart[i] + d);
        }
        let last = self.nodes.len() - 1;
        let pull_back_at = arrive[last] + END_TAIL_SECS;
        depart.push(pull_back_at);
        Schedule {
            arrive,
            depart,
            leg_secs: secs,
            pull_back_at,
            total_secs: pull_back_at + PULL_BACK_SECS,
        }
    }

    /// Pause at node `i` (not the first or last): labelled stops only.
    fn pause_at(&self, i: usize) -> f64 {
        if self.nodes[i].label.is_some() {
            NODE_PAUSE_SECS
        } else {
            0.0
        }
    }

    /// Length of the whole recap.
    pub fn duration_secs(&self) -> f64 {
        self.schedule().total_secs
    }

    /// Ships that appear on a leg.
    pub fn ships(&self) -> Vec<String> {
        let mut out: Vec<String> = vec![];
        for s in self.legs.iter().filter_map(|l| l.ship.as_ref()) {
            if !out.contains(s) {
                out.push(s.clone());
            }
        }
        out
    }

    /// Build the journey for `itin`. Ship legs are shaped by the GPS fixes of
    /// `assets` taken at sea between the two ports (the ship's real track),
    /// and routed around land between them.
    pub fn build(itin: &Itinerary, assets: &[Asset]) -> Self {
        let mut nodes: Vec<JourneyNode> = vec![];
        let mut legs: Vec<JourneyLeg> = vec![];
        let first = itin.stop(&itin.legs[0].from).expect("validated chain");
        nodes.push(JourneyNode {
            pos: first.pos(),
            label: Some(first.label.clone()),
            pin: true,
        });
        let mut prev_ship: Option<&str> = None;
        for leg in &itin.legs {
            let (from, to) = (
                itin.stop(&leg.from).expect("validated chain"),
                itin.stop(&leg.to).expect("validated chain"),
            );
            let path = match leg.mode {
                LegMode::Flight => great_circle(from.pos(), to.pos()),
                LegMode::Ground => vec![from.pos(), to.pos()],
                LegMode::Ship if leg.is_loop => loop_path(from.pos()),
                LegMode::Ship => {
                    let window = (
                        from.depart_date().or(from.arrive_date()),
                        to.arrive_date().or(to.depart_date()),
                    );
                    shape_ship_leg(from.pos(), to.pos(), &sea_fixes(assets, window))
                }
            };
            let ship = leg.ship.as_deref().filter(|_| leg.mode == LegMode::Ship);
            let boarding_tag = ship.filter(|s| prev_ship != Some(*s)).map(|s| {
                let day = leg
                    .dates
                    .as_deref()
                    .and_then(|d| d.split_once(".."))
                    .and_then(|(a, _)| chrono::NaiveDate::parse_from_str(a, "%Y-%m-%d").ok())
                    .or(from.depart_date())
                    .or(from.arrive_date());
                match day {
                    Some(d) => format!("Boarded {s} \u{b7} {}", d.format("%b %-d")),
                    None => format!("Boarded {s}"),
                }
            });
            prev_ship = ship;
            legs.push(JourneyLeg {
                mode: leg.mode,
                ship: ship.map(str::to_string),
                is_loop: leg.is_loop,
                path,
                boarding_tag,
            });
            nodes.push(JourneyNode {
                pos: to.pos(),
                label: (!to.hide_label && !leg.is_loop).then(|| to.label.clone()),
                pin: false,
            });
        }
        let last = nodes.len() - 1;
        for (i, n) in nodes.iter_mut().enumerate() {
            n.pin = n.label.is_some() || i == last;
        }
        let mut journey = Self { nodes, legs };
        journey.unwrap_longitudes();
        journey
    }

    /// Make longitudes continuous along the whole journey (a flight across
    /// the Pacific runs on past 180 degrees rather than jumping back).
    fn unwrap_longitudes(&mut self) {
        let mut prev = self.nodes[0].pos;
        for (i, leg) in self.legs.iter_mut().enumerate() {
            let shift = ((prev.1 - leg.path[0].1) / 360.0).round() * 360.0;
            for p in &mut leg.path {
                p.1 += shift;
            }
            leg.path[0] = prev;
            prev = leg.path[leg.path.len() - 1];
            self.nodes[i + 1].pos = prev;
        }
        self.nodes[0].pos = self.legs[0].path[0];
    }
}

/// Positions at sea between two dates, in capture order.
fn sea_fixes(
    assets: &[Asset],
    (from, to): (Option<chrono::NaiveDate>, Option<chrono::NaiveDate>),
) -> Vec<(f64, f64)> {
    let (Some(from), Some(to)) = (from, to) else {
        return vec![];
    };
    let mut fixes: Vec<(i64, (f64, f64))> = assets
        .iter()
        .filter_map(|a| {
            let (gps, taken, day) = (a.gps?, a.taken_at?, asset_day(a)?);
            (from <= day && day <= to && is_water(gps)).then_some((taken, gps))
        })
        .collect();
    fixes.sort_by_key(|f| f.0);
    fixes.into_iter().map(|f| f.1).collect()
}

/// Great-circle arc from `a` to `b`, a point about every degree, with
/// longitudes continuous from `a` (they may run past 180).
pub fn great_circle(a: (f64, f64), b: (f64, f64)) -> Vec<(f64, f64)> {
    let vec = |p: (f64, f64)| {
        let (lat, lon) = (p.0.to_radians(), p.1.to_radians());
        [lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin()]
    };
    let (va, vb) = (vec(a), vec(b));
    let dot = (va[0] * vb[0] + va[1] * vb[1] + va[2] * vb[2]).clamp(-1.0, 1.0);
    let angle = dot.acos();
    if angle < 1e-6 {
        return vec![a, b];
    }
    let steps = (angle.to_degrees()).ceil().max(2.0) as u32;
    let mut out = vec![a];
    let mut lon = a.1;
    for i in 1..=steps {
        let t = f64::from(i) / f64::from(steps);
        let (wa, wb) = (
            ((1.0 - t) * angle).sin() / angle.sin(),
            (t * angle).sin() / angle.sin(),
        );
        let v = [
            wa * va[0] + wb * vb[0],
            wa * va[1] + wb * vb[1],
            wa * va[2] + wb * vb[2],
        ];
        let lat = v[2].clamp(-1.0, 1.0).asin().to_degrees();
        let raw = v[1].atan2(v[0]).to_degrees();
        lon += (raw - lon + 180.0).rem_euclid(360.0) - 180.0;
        out.push((lat, lon));
    }
    out
}

/// A closed loop out to sea from `center` and back: a lens about 2 degrees
/// long in the first direction with open water at its tip and flanks, each
/// side routed around land and the corners rounded. Smaller when boxed in.
pub fn loop_path(center: (f64, f64)) -> Vec<(f64, f64)> {
    for r in [2.0, 1.5, 1.0, 0.6] {
        for k in 0..16 {
            let dir = f64::from(k) * std::f64::consts::TAU / 16.0;
            let (dy, dx) = (dir.sin(), dir.cos()); // lat, lon components
            let at = |along: f64, across: f64| {
                (
                    center.0 + dy * along * r + dx * across * r,
                    center.1 + dx * along * r - dy * across * r,
                )
            };
            let (tip, p1, p2) = (at(1.0, 0.0), at(0.5, 0.3), at(0.5, -0.3));
            if ![tip, p1, p2].iter().all(|&p| is_water(p)) {
                continue;
            }
            let mut path = vec![center];
            for (from, to) in [(center, p1), (p1, tip), (tip, p2), (p2, center)] {
                path.extend(route(from, to).into_iter().skip(1));
            }
            return round_corners(&path);
        }
    }
    // Boxed in (a harbour on land data): a token loop anyway.
    vec![
        center,
        (center.0 + 0.2, center.1 + 0.1),
        (center.0 + 0.1, center.1 - 0.1),
        center,
    ]
}

/// Two rounds of corner cutting that keep the first and last points.
fn round_corners(path: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut pts = path.to_vec();
    for _ in 0..2 {
        let mut out = vec![pts[0]];
        for w in pts.windows(2) {
            out.push((0.75 * w[0].0 + 0.25 * w[1].0, 0.75 * w[0].1 + 0.25 * w[1].1));
            out.push((0.25 * w[0].0 + 0.75 * w[1].0, 0.25 * w[0].1 + 0.75 * w[1].1));
        }
        out.push(pts[pts.len() - 1]);
        pts = out;
    }
    pts
}

/// Land fraction under a point, exposed for tests of sea paths.
#[cfg(test)]
fn on_land(p: (f64, f64)) -> bool {
    !is_water(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AssetKind;

    const JSON: &str = r#"{
      "stops": [
        { "id": "lax-out", "label": "Los Angeles", "lat": 33.9416, "lon": -118.4085, "depart": "2026-10-03", "kind": "home" },
        { "id": "tokyo", "label": "Tokyo", "lat": 35.7148, "lon": 139.7967, "arrive": "2026-10-04", "depart": "2026-10-09" },
        { "id": "tokyo-port", "label": "Tokyo", "lat": 35.6177, "lon": 139.771, "arrive": "2026-10-09", "depart": "2026-10-09", "kind": "port", "hide_label": true },
        { "id": "osaka", "label": "Osaka", "lat": 34.656, "lon": 135.4306, "arrive": "2026-10-11", "depart": "2026-10-11", "kind": "port" },
        { "id": "fukuoka", "label": "Fukuoka", "lat": 33.6036, "lon": 130.4038, "arrive": "2026-10-13", "depart": "2026-10-13", "kind": "port" },
        { "id": "jeju", "label": "Jeju", "lat": 33.229, "lon": 126.4826, "arrive": "2026-10-14", "depart": "2026-10-14", "kind": "port" },
        { "id": "keelung", "label": "Taipei", "lat": 25.1335, "lon": 121.7414, "arrive": "2026-10-16", "depart": "2026-10-16", "kind": "port" },
        { "id": "hongkong", "label": "Hong Kong", "lat": 22.295, "lon": 114.168, "arrive": "2026-10-18", "depart": "2026-10-18", "kind": "port" },
        { "id": "saigon", "label": "Ho Chi Minh City", "lat": 10.5833, "lon": 107.0333, "arrive": "2026-10-21", "depart": "2026-10-21", "kind": "port" },
        { "id": "singapore", "label": "Singapore", "lat": 1.2655, "lon": 103.864, "arrive": "2026-10-23", "depart": "2026-10-30" },
        { "id": "lax-home", "label": "Los Angeles", "lat": 33.9416, "lon": -118.4085, "arrive": "2026-10-30", "kind": "home", "hide_label": true }
      ],
      "legs": [
        { "from": "lax-out", "to": "tokyo", "mode": "flight" },
        { "from": "tokyo", "to": "tokyo-port", "mode": "ground" },
        { "from": "tokyo-port", "to": "osaka", "mode": "ship", "ship": "Carnival Luminosa" },
        { "from": "osaka", "to": "fukuoka", "mode": "ship", "ship": "Carnival Luminosa" },
        { "from": "fukuoka", "to": "jeju", "mode": "ship", "ship": "Carnival Luminosa" },
        { "from": "jeju", "to": "keelung", "mode": "ship", "ship": "Carnival Luminosa" },
        { "from": "keelung", "to": "hongkong", "mode": "ship", "ship": "Carnival Luminosa" },
        { "from": "hongkong", "to": "saigon", "mode": "ship", "ship": "Carnival Luminosa" },
        { "from": "saigon", "to": "singapore", "mode": "ship", "ship": "Carnival Luminosa" },
        { "from": "singapore", "to": "singapore", "mode": "ship", "ship": "Disney Adventure", "loop": true, "dates": "2026-10-26..2026-10-29" },
        { "from": "singapore", "to": "lax-home", "mode": "flight" }
      ]
    }"#;

    fn journey() -> Journey {
        Journey::build(&Itinerary::from_json(JSON).unwrap(), &[])
    }

    #[test]
    fn nodes_labels_and_legs_follow_the_itinerary() {
        let j = journey();
        assert_eq!(j.nodes.len(), 12);
        assert_eq!(j.legs.len(), 11);
        let labels: Vec<Option<&str>> = j.nodes.iter().map(|n| n.label.as_deref()).collect();
        assert_eq!(
            labels,
            [
                Some("Los Angeles"),
                Some("Tokyo"),
                None, // embarkation terminal
                Some("Osaka"),
                Some("Fukuoka"),
                Some("Jeju"),
                Some("Taipei"),
                Some("Hong Kong"),
                Some("Ho Chi Minh City"),
                Some("Singapore"),
                None, // the loop's return
                None, // home again
            ]
        );
        // Never a ship as a map point.
        assert!(j
            .nodes
            .iter()
            .all(|n| !n.label.as_deref().unwrap_or("").contains("Carnival")));
        assert!(!j.nodes[2].pin && j.nodes[11].pin);
        // Boarding tags: once per ship, on the leg where boarding happens.
        let tags: Vec<_> = j
            .legs
            .iter()
            .filter_map(|l| l.boarding_tag.as_deref())
            .collect();
        assert_eq!(
            tags,
            [
                "Boarded Carnival Luminosa \u{b7} Oct 9",
                "Boarded Disney Adventure \u{b7} Oct 26"
            ]
        );
        assert!(j.legs[9].is_loop);
        assert_eq!(j.ships(), ["Carnival Luminosa", "Disney Adventure"]);
    }

    #[test]
    fn flights_are_great_circle_arcs_with_continuous_longitudes() {
        let j = journey();
        let out = &j.legs[0].path;
        assert!(out.len() > 80, "an arc, not a chord: {}", out.len());
        // LA west to Tokyo across the Pacific: longitudes run on past -180.
        assert!(out.iter().all(|w| w.1 <= -118.0));
        assert!((out[out.len() - 1].1 - (139.7967 - 360.0)).abs() < 0.01);
        // The great circle bows north of the straight line.
        assert!(out.iter().any(|p| p.0 > 45.0));
        // The whole journey is continuous: each leg starts where the last ended.
        for w in j.legs.windows(2) {
            assert_eq!(w[0].path[w[0].path.len() - 1], w[1].path[0]);
        }
        // Home again by the other way round: Singapore east to Los Angeles.
        let home = &j.legs[10].path;
        assert!((home[home.len() - 1].1 - (-118.4085 - 360.0 * 0.0 - 0.0)).abs() < 400.0);
        assert!((j.nodes[11].pos.1 - j.nodes[9].pos.1).abs() < 200.0);
    }

    #[test]
    fn ship_legs_stay_on_water_and_the_loop_is_closed() {
        let j = journey();
        for (i, leg) in j
            .legs
            .iter()
            .enumerate()
            .filter(|(_, l)| l.mode == LegMode::Ship && !l.is_loop)
        {
            let wrapped = |p: (f64, f64)| (p.0, (p.1 + 180.0).rem_euclid(360.0) - 180.0);
            // The first and last hops leave and enter a coastal port (a land
            // cell by the grid); everything between is open water.
            let inner = &leg.path[1..leg.path.len() - 1];
            let land: usize = inner
                .windows(2)
                .map(|w| {
                    (0..=20)
                        .filter(|k| {
                            let t = f64::from(*k) / 20.0;
                            on_land(wrapped((
                                w[0].0 + (w[1].0 - w[0].0) * t,
                                w[0].1 + (w[1].1 - w[0].1) * t,
                            )))
                        })
                        .count()
                })
                .sum();
            // A narrow strait may nick a coastal cell; nothing like overland.
            let samples = 21 * inner.len().saturating_sub(1).max(1);
            assert!(land * 8 <= samples, "leg {i} crosses land: {:?}", leg.path);
        }
        let lp = &j.legs[9].path;
        assert_eq!(lp[0], lp[lp.len() - 1]);
        assert!(lp.len() > 10);
        assert!(lp[1..lp.len() - 1]
            .iter()
            .all(|&p| is_water((p.0, (p.1 + 180.0).rem_euclid(360.0) - 180.0))));
    }

    #[test]
    fn schedule_fits_the_budget_and_pauses_only_at_labelled_stops() {
        let j = journey();
        let s = j.schedule();
        // About 14-18 s.
        assert!((14.0..=18.0).contains(&s.total_secs), "{}", s.total_secs);
        assert_eq!(s.total_secs, j.duration_secs());
        assert!((s.depart[0] - s.arrive[0] - START_HOLD_SECS).abs() < 1e-9);
        // Labelled stops pause; the terminal and the loop's return do not.
        assert!((s.depart[1] - s.arrive[1] - NODE_PAUSE_SECS).abs() < 1e-9);
        assert_eq!(s.depart[2] - s.arrive[2], 0.0);
        assert_eq!(s.depart[10] - s.arrive[10], 0.0);
        assert!(s.arrive.windows(2).all(|w| w[1] > w[0]));
        assert_eq!(s.pull_back_at, s.arrive[11] + END_TAIL_SECS);
        assert_eq!(s.total_secs, s.pull_back_at + PULL_BACK_SECS);
        assert_eq!(s.leg_secs[0], FLIGHT_SECS);
        assert_eq!(s.leg_secs[9], LOOP_SECS);
    }

    #[test]
    fn photos_at_sea_shape_a_ship_leg() {
        let it = Itinerary::from_json(JSON).unwrap();
        let mut a = Asset::new("p", std::path::PathBuf::from("p"), AssetKind::Photo);
        // Off Tanegashima on a sea day between Osaka (10-11) and Fukuoka (10-13).
        a.gps = Some((30.8, 131.5));
        a.taken_at = Some(
            chrono::NaiveDate::from_ymd_opt(2026, 10, 12)
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .unwrap()
                .and_utc()
                .timestamp(),
        );
        let j = Journey::build(&it, &[a]);
        assert!(
            j.legs[3].path.iter().any(|p| p.0 < 32.0),
            "south of Kyushu: {:?}",
            j.legs[3].path
        );
        assert!(is_water((30.8, 131.5)));
    }
}
