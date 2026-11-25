//! The closing map's journey: what the recap travels and how long it takes.
//!
//! Built from the owner's [`Itinerary`]: nodes (the stops, with labels),
//! legs (flight arcs, ground hops, ship legs routed around land, a loop at
//! sea for a round-trip cruise) and the schedule. The renderer plays this
//! schedule exactly, and the plan's film-length budget reads
//! [`Journey::duration_secs`], so the two cannot drift apart.

use crate::itinerary::{asset_day, ItinStop, Itinerary, LegMode, StopKind};
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
pub const FLIGHT_SECS: f64 = 2.0;
pub const GROUND_SECS: f64 = 0.35;
pub const SHIP_LEG_SECS: f64 = 0.6;
/// A round-trip cruise's loop, and the hold on it once drawn.
pub const LOOP_SECS: f64 = 2.4;
pub const LOOP_HOLD_SECS: f64 = 0.8;

/// A stop on the journey.
#[derive(Debug, Clone, PartialEq)]
pub struct JourneyNode {
    /// `lat, lon` in the journey's continuous (unwrapped) longitudes.
    pub pos: (f64, f64),
    /// City label shown at the stop; `None` for waypoints (an embarkation
    /// terminal, a loop's return).
    pub label: Option<String>,
    /// The stop's itinerary name, even when its map label is hidden.
    pub name: String,
    pub pin: bool,
    /// The day the owner arrives here, when the itinerary says.
    pub arrive: Option<jiff::civil::Date>,
    /// The stop is home (where the trip starts or ends).
    pub home: bool,
    /// Artwork for the stop on the parchment travel maps.
    pub sticker: Option<std::path::PathBuf>,
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
    /// First and last day of the leg, when the itinerary dates it (a
    /// round-trip cruise).
    pub dates: Option<(jiff::civil::Date, jiff::civil::Date)>,
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

/// Length of one travel-map hop in the film, by what it shows. Each
/// includes [`HOP_LEAD_SECS`] before the line starts (the cut into the map
/// is a crossfade) and [`HOP_HOLD_SECS`] on the arrival.
pub const HOP_FLIGHT_SECS: f64 = 5.4;
pub const HOP_LOOP_SECS: f64 = 5.0;
pub const HOP_SECS: f64 = 4.2;
pub const HOP_LEAD_SECS: f64 = 0.4;
pub const HOP_HOLD_SECS: f64 = 1.7;
/// Hold on the whole-route map that closes a film told in hops.
pub const HOP_OVERVIEW_SECS: f64 = 5.0;

/// One travel-map interlude: the legs from the last named stop to the next
/// (a flight, a sailing between two ports, a round-trip cruise).
#[derive(Debug, Clone, PartialEq)]
pub struct Hop {
    /// The journey legs it travels, in order.
    pub legs: std::ops::Range<usize>,
    /// The node it arrives at.
    pub node: usize,
    pub secs: f64,
    /// The day the film reaches it: the arrival, or a round trip's first day.
    pub day: Option<jiff::civil::Date>,
    /// It arrives home.
    pub home: bool,
    /// Where it arrives (`lat, lon`, longitude in -180..180).
    pub pos: (f64, f64),
    /// Where it leaves from, in the same form.
    pub from: (f64, f64),
    /// When the trip actually moves: the time (unix seconds) of the shot the
    /// hop plays in front of, set by [`with_moves`]. `i64::MAX` when no shot
    /// follows it; `None` until set, when the hop's day decides.
    pub moved_at: Option<i64>,
}

/// `p` with its longitude wrapped into -180..180 (journey paths unwrap it
/// across the antimeridian).
fn wrapped(p: (f64, f64)) -> (f64, f64) {
    (p.0, (p.1 + 180.0).rem_euclid(360.0) - 180.0)
}

/// How near home a shot must be for the hop home to play before it: the
/// arrival airport is rarely the house (LAX to Ontario is 50 km).
pub const HOME_REACH_M: f64 = 150_000.0;

/// Whether `gps` is within [`HOME_REACH_M`] of the home stop at `home`.
fn within_home_reach(gps: (f64, f64), home: (f64, f64)) -> bool {
    crate::geo::haversine_m(gps, home) <= HOME_REACH_M
}

/// Whether a shot at `gps` was taken back home, at `hop`'s destination.
fn at_home(hop: &Hop, gps: Option<(f64, f64)>) -> bool {
    gps.is_some_and(|g| within_home_reach(g, hop.pos))
}

/// Where each hop plays: for each, the index of the shot it goes in front
/// of and that shot's time (unix seconds), or `shots.len()` and `i64::MAX`
/// to follow them all, and `None` for an undated hop that plays wherever the
/// previous one did. Hops keep their order.
///
/// A hop plays at the actual move, the first shot on or after its day taken
/// nearer its destination than its origin, so the morning's shots in the
/// departure city stay before it. Shots without GPS never trigger the cut;
/// when none on or after the day has GPS nearer the destination, the first
/// shot on or after the day does. The last hop home plays before the first
/// shot taken back home (within [`HOME_REACH_M`], on or after its day), or
/// after every shot when none was or it has no day.
fn moves(
    hops: &[Hop],
    shots: &[Option<(i64, Option<i32>)>],
    gps: &[Option<(f64, f64)>],
) -> Vec<(usize, Option<i64>)> {
    let day_of = |s: usize| shots[s].and_then(|(t, tz)| crate::metadata::local_date(t, tz));
    let time_of = |s: usize| shots.get(s).copied().flatten().map(|(t, _)| t);
    let gps_of = |s: usize| gps.get(s).copied().flatten();
    let mut at = 0;
    let mut floor = i64::MIN;
    let mut out = vec![];
    for (i, hop) in hops.iter().enumerate() {
        let mut when = None;
        if hop.home && i + 1 == hops.len() {
            at = hop
                .day
                .and_then(|day| {
                    (at..shots.len())
                        .find(|&s| at_home(hop, gps_of(s)) && day_of(s).is_some_and(|sd| sd >= day))
                })
                .unwrap_or(shots.len());
            when = hop.day.map(|_| time_of(at).unwrap_or(i64::MAX));
        } else if let Some(day) = hop.day {
            let on_day = |s: &usize| day_of(*s).is_some_and(|d| d >= day);
            at = (at..shots.len())
                .find(|s| {
                    on_day(s)
                        && gps_of(*s).is_some_and(|g| {
                            crate::geo::haversine_m(g, hop.pos)
                                < crate::geo::haversine_m(g, hop.from)
                        })
                })
                .or_else(|| (at..shots.len()).find(on_day))
                .unwrap_or(shots.len());
            when = Some(time_of(at).unwrap_or(i64::MAX));
        }
        // Hops never play out of order.
        let when = when.map(|w| w.max(floor));
        if let Some(w) = when {
            floor = w;
        }
        out.push((at, when));
    }
    out
}

/// `hops` with each one's [`Hop::moved_at`] set from the film's shots (unix
/// seconds and UTC offset, in film order; `None` for an undated shot) and
/// `gps`, for [`hops_reached`]. [`hop_cuts`] places the same hops.
pub fn with_moves(
    mut hops: Vec<Hop>,
    shots: &[Option<(i64, Option<i32>)>],
    gps: &[Option<(f64, f64)>],
) -> Vec<Hop> {
    let placed = moves(&hops, shots, gps);
    for (hop, (_, when)) in hops.iter_mut().zip(placed) {
        hop.moved_at = when;
    }
    hops
}

/// Where each hop plays in a film whose shots were taken at `shots` and
/// `gps`: the index of the shot it goes in front of, or `shots.len()` to
/// follow them all (see [`moves`] for the rule).
pub fn hop_cuts(
    hops: &[Hop],
    shots: &[Option<(i64, Option<i32>)>],
    gps: &[Option<(f64, f64)>],
) -> Vec<usize> {
    moves(hops, shots, gps).into_iter().map(|m| m.0).collect()
}

/// How many hops the trip has made by the time a shot was taken (unix
/// seconds and UTC offset) at `gps`: the hops it plays at or after (their
/// [`Hop::moved_at`]), or, for hops not yet placed by [`with_moves`], the
/// hops whose day is on or before the shot's. Shots either side of a hop get
/// different counts, so a film can keep them in separate segments and play
/// the hop between them. An undated shot counts none; the last hop home
/// counts only for a shot taken back home (see [`hop_cuts`]).
pub fn hops_reached(
    hops: &[Hop],
    shot: Option<(i64, Option<i32>)>,
    gps: Option<(f64, f64)>,
) -> usize {
    let Some((t, tz)) = shot else {
        return 0;
    };
    let Some(day) = crate::metadata::local_date(t, tz) else {
        return 0;
    };
    hops.iter()
        .enumerate()
        .filter(|(i, h)| !(h.home && i + 1 == hops.len()) || at_home(h, gps))
        .filter(|(_, h)| match h.moved_at {
            Some(m) => t >= m,
            None => h.day.is_some_and(|d| d <= day),
        })
        .count()
}

fn short_date(d: jiff::civil::Date) -> String {
    d.strftime("%b %-d").to_string()
}

/// `Oct 26–29`, or `Oct 30 – Nov 2` across a month end.
fn date_span(a: jiff::civil::Date, b: jiff::civil::Date) -> String {
    if a == b {
        short_date(a)
    } else if (a.year(), a.month()) == (b.year(), b.month()) {
        format!("{}\u{2013}{}", short_date(a), b.day())
    } else {
        format!("{} \u{2013} {}", short_date(a), short_date(b))
    }
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
        if i > 0 && self.legs[i - 1].is_loop {
            LOOP_HOLD_SECS
        } else if self.nodes[i].label.is_some() {
            NODE_PAUSE_SECS
        } else {
            0.0
        }
    }

    /// Length of the whole recap.
    pub fn duration_secs(&self) -> f64 {
        self.schedule().total_secs
    }

    /// Whether `gps` is back home: within [`HOME_REACH_M`] of a stop the
    /// trip starts or ends at.
    pub fn near_home(&self, gps: (f64, f64)) -> bool {
        self.nodes
            .iter()
            .filter(|n| n.home)
            .any(|n| within_home_reach(gps, wrapped(n.pos)))
    }

    /// The names of the stops the trip starts or ends at (`Los Angeles`),
    /// including a home stop whose map label is hidden.
    pub fn home_labels(&self) -> impl Iterator<Item = &str> {
        self.nodes
            .iter()
            .filter(|n| n.home)
            .map(|n| n.name.as_str())
    }

    /// The journey as hops between named stops, for a film that shows each
    /// one where the trip reaches it. A waypoint (an embarkation terminal)
    /// joins the hop that leaves it.
    pub fn hops(&self) -> Vec<Hop> {
        let mut out = vec![];
        let mut start = 0;
        for (i, leg) in self.legs.iter().enumerate() {
            let node = &self.nodes[i + 1];
            let last = i + 1 == self.legs.len();
            if !(leg.is_loop || node.label.is_some() || last) {
                continue;
            }
            let legs = &self.legs[start..=i];
            let secs = if legs.iter().any(|l| l.mode == LegMode::Flight) {
                HOP_FLIGHT_SECS
            } else if leg.is_loop {
                HOP_LOOP_SECS
            } else {
                HOP_SECS
            };
            out.push(Hop {
                legs: start..i + 1,
                node: i + 1,
                secs,
                day: leg
                    .dates
                    .filter(|_| leg.is_loop)
                    .map(|d| d.0)
                    .or(node.arrive),
                home: node.home,
                pos: wrapped(node.pos),
                from: wrapped(self.nodes[start].pos),
                moved_at: None,
            });
            start = i + 1;
        }
        out
    }

    /// What a stop's plaque says on a travel map: `Tokyo · Oct 4`. `None`
    /// for a waypoint; the unnamed arrival home reads `Home`.
    pub fn node_text(&self, node: usize) -> Option<String> {
        let n = &self.nodes[node];
        let name = match &n.label {
            Some(l) => l.clone(),
            None if n.home => "Home".to_string(),
            None => return None,
        };
        Some(match n.arrive {
            Some(d) => format!("{name} \u{b7} {}", short_date(d)),
            None => name,
        })
    }

    /// What a hop's plaque says on arrival; a round trip names its ship
    /// and days (`Disney Adventure · Oct 26–29`).
    pub fn hop_text(&self, hop: &Hop) -> Option<String> {
        let leg = &self.legs[hop.legs.end - 1];
        if !leg.is_loop {
            return self.node_text(hop.node);
        }
        let name = leg.ship.clone().unwrap_or_else(|| "Round trip".into());
        Some(match leg.dates {
            Some((a, b)) => format!("{name} \u{b7} {}", date_span(a, b)),
            None => name,
        })
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
        // `pin` is settled once every node is in.
        let node = |stop: &ItinStop, label: Option<String>| JourneyNode {
            pos: stop.pos(),
            label,
            name: stop.label.clone(),
            pin: false,
            // A stop with only a departure is there that day (the same
            // reading as the itinerary's own stop window).
            arrive: stop.arrive_date().or(stop.depart_date()),
            home: stop.kind == StopKind::Home,
            sticker: stop.sticker.as_ref().map(Into::into),
        };
        nodes.push(node(first, Some(first.label.clone())));
        let mut prev_ship: Option<&str> = None;
        // Traced paths by leg, so a smoothed leg can borrow the neighbouring
        // leg's adjacent point and leave a stop with the tangent it arrived on.
        let traced: Vec<Option<Vec<(f64, f64)>>> = itin
            .legs
            .iter()
            .map(|l| {
                l.path
                    .as_ref()
                    .filter(|_| l.mode != LegMode::Flight && !l.is_loop)
                    .map(|p| traced_path(p))
            })
            .collect();
        for (li, leg) in itin.legs.iter().enumerate() {
            let (from, to) = (
                itin.stop(&leg.from).expect("validated chain"),
                itin.stop(&leg.to).expect("validated chain"),
            );
            let explicit = leg.path.as_ref().map(|p| traced_path(p));
            let path = match leg.mode {
                // A traced path is smoothed through its points (flights keep
                // their exact arc).
                _ if explicit.is_some() && leg.mode != LegMode::Flight => {
                    let own = explicit.unwrap_or_default();
                    let before = li
                        .checked_sub(1)
                        .and_then(|j| traced[j].as_ref())
                        .filter(|t| t.len() >= 2)
                        .map(|t| t[t.len() - 2]);
                    let after = traced
                        .get(li + 1)
                        .and_then(Option::as_ref)
                        .filter(|t| t.len() >= 2)
                        .map(|t| t[1]);
                    // Neighbours were unwrapped on their own: bring their
                    // tangent points into this leg's frame.
                    let near = |q: (f64, f64), to: (f64, f64)| (q.0, lon_near(q.1, to.1));
                    let (head, tail) = (own[0], own[own.len() - 1]);
                    catmull_rom_through(
                        &own,
                        10,
                        before.filter(|_| !leg.is_loop).map(|q| near(q, head)),
                        after.filter(|_| !leg.is_loop).map(|q| near(q, tail)),
                    )
                }
                _ if explicit.is_some() => explicit.unwrap_or_default(),
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
                    .date_range()
                    .map(|(a, _)| a)
                    .or(from.depart_date())
                    .or(from.arrive_date());
                match day {
                    Some(d) => format!("Boarded {s} \u{b7} {}", short_date(d)),
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
                dates: leg.date_range(),
            });
            nodes.push(node(
                to,
                (!to.hide_label && !leg.is_loop).then(|| to.label.clone()),
            ));
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
            // Within the leg too: a routed or traced sea path can hold
            // points on both sides of the antimeridian (LA to Japan), and
            // must take the short way across the Pacific, not the long way.
            unwrap_path(&mut leg.path);
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

/// Shift longitudes by multiples of 360 so no step along `pts` jumps more
/// than 180 degrees (the short way across the antimeridian).
fn unwrap_path(pts: &mut [(f64, f64)]) {
    for k in 1..pts.len() {
        pts[k].1 = lon_near(pts[k].1, pts[k - 1].1);
    }
}

/// `lon` shifted by a multiple of 360 to lie within 180 degrees of `of`.
fn lon_near(lon: f64, of: f64) -> f64 {
    of + (lon - of + 180.0).rem_euclid(360.0) - 180.0
}

/// An itinerary leg's traced path as `(lat, lon)` points, unwrapped:
/// smoothing runs on a continuous path, not one that jumps 340 degrees at
/// the date line.
fn traced_path(path: &[[f64; 2]]) -> Vec<(f64, f64)> {
    let mut pts: Vec<(f64, f64)> = path.iter().map(|q| (q[0], q[1])).collect();
    unwrap_path(&mut pts);
    pts
}

/// Positions at sea between two dates, in capture order.
fn sea_fixes(
    assets: &[Asset],
    (from, to): (Option<jiff::civil::Date>, Option<jiff::civil::Date>),
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

/// Uniform Catmull-Rom spline through `pts` (`per_seg` points per span),
/// keeping the first and last points exactly.
pub fn catmull_rom(pts: &[(f64, f64)], per_seg: usize) -> Vec<(f64, f64)> {
    catmull_rom_through(pts, per_seg, None, None)
}

/// [`catmull_rom`] where the path continues from `before` into its first
/// point and on to `after` past its last, so the tangents at both ends match
/// the neighbouring legs instead of kinking at the stop.
pub fn catmull_rom_through(
    pts: &[(f64, f64)],
    per_seg: usize,
    before: Option<(f64, f64)>,
    after: Option<(f64, f64)>,
) -> Vec<(f64, f64)> {
    if pts.len() < 3 && before.is_none() && after.is_none() {
        return pts.to_vec();
    }
    let mut out = vec![pts[0]];
    for i in 0..pts.len() - 1 {
        let (p1, p2) = (pts[i], pts[i + 1]);
        let p0 = if i == 0 {
            before.unwrap_or(p1)
        } else {
            pts[i - 1]
        };
        let p3 = pts
            .get(i + 2)
            .copied()
            .unwrap_or_else(|| after.unwrap_or(p2));
        for k in 1..=per_seg {
            let t = k as f64 / per_seg as f64;
            let (t2, t3) = (t * t, t * t * t);
            let f = |a: f64, b: f64, c: f64, d: f64| {
                0.5 * (2.0 * b
                    + (c - a) * t
                    + (2.0 * a - 5.0 * b + 4.0 * c - d) * t2
                    + (3.0 * b - a - 3.0 * c + d) * t3)
            };
            out.push((f(p0.0, p1.0, p2.0, p3.0), f(p0.1, p1.1, p2.1, p3.1)));
        }
    }
    out
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
        lon = lon_near(raw, lon);
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
    fn a_ship_leg_across_the_pacific_takes_the_short_way() {
        let json = r#"{
          "stops": [
            { "id": "la", "label": "Los Angeles", "lat": 33.7364, "lon": -118.2732, "depart": "2026-10-03", "kind": "home" },
            { "id": "muroran", "label": "Muroran", "lat": 42.3474, "lon": 140.9465, "arrive": "2026-10-15", "depart": "2026-10-15", "kind": "port" },
            { "id": "hakodate", "label": "Hakodate", "lat": 41.77, "lon": 140.73, "arrive": "2026-10-16", "depart": "2026-10-16", "kind": "port" },
            { "id": "tokyo", "label": "Tokyo", "lat": 35.6646, "lon": 139.7613, "arrive": "2026-10-18", "depart": "2026-10-20" },
            { "id": "la2", "label": "Los Angeles", "lat": 33.9416, "lon": -118.4085, "arrive": "2026-10-20", "kind": "home" }
          ],
          "legs": [
            { "from": "la", "to": "muroran", "mode": "ship", "ship": "Ship" },
            { "from": "muroran", "to": "hakodate", "mode": "ship", "ship": "Ship" },
            { "from": "hakodate", "to": "tokyo", "mode": "ground" },
            { "from": "tokyo", "to": "la2", "mode": "flight" }
          ]
        }"#;
        let j = Journey::build(&Itinerary::from_json(json).unwrap(), &[]);
        for (i, leg) in j.legs.iter().enumerate() {
            for w in leg.path.windows(2) {
                assert!(
                    (w[1].1 - w[0].1).abs() <= 180.0,
                    "leg {i} jumps: {:?}",
                    leg.path
                );
            }
        }
        // West from California past -180, not east across the Atlantic.
        let ship = &j.legs[0].path;
        assert!(ship.last().unwrap().1 < -180.0, "{ship:?}");
        assert!(ship.iter().all(|p| p.1 < -100.0));
        for w in j.nodes.windows(2) {
            assert!((w[1].pos.1 - w[0].pos.1).abs() <= 180.0);
        }
    }

    #[test]
    fn an_explicit_ship_path_is_unwrapped_too() {
        let json = r#"{
          "stops": [
            { "id": "la", "label": "Los Angeles", "lat": 33.7, "lon": -118.3, "depart": "2026-10-03", "kind": "home" },
            { "id": "muroran", "label": "Muroran", "lat": 42.3, "lon": 140.9, "arrive": "2026-10-15", "kind": "port" }
          ],
          "legs": [
            { "from": "la", "to": "muroran", "mode": "ship", "ship": "Ship",
              "path": [[33.7, -118.3], [40.0, -170.0], [45.0, 170.0], [42.3, 140.9]] }
          ]
        }"#;
        let j = Journey::build(&Itinerary::from_json(json).unwrap(), &[]);
        for w in j.legs[0].path.windows(2) {
            assert!((w[1].1 - w[0].1).abs() <= 180.0, "{:?}", j.legs[0].path);
        }
        assert!(j.legs[0].path.last().unwrap().1 < -180.0);
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
        assert!(lp[1..lp.len() - 1].iter().all(|&p| is_water(wrapped(p))));
    }

    #[test]
    fn hops_run_from_one_named_stop_to_the_next() {
        let j = journey();
        let hops = j.hops();
        let texts: Vec<String> = hops.iter().filter_map(|h| j.hop_text(h)).collect();
        assert_eq!(
            texts,
            [
                "Tokyo \u{b7} Oct 4",
                "Osaka \u{b7} Oct 11",
                "Fukuoka \u{b7} Oct 13",
                "Jeju \u{b7} Oct 14",
                "Taipei \u{b7} Oct 16",
                "Hong Kong \u{b7} Oct 18",
                "Ho Chi Minh City \u{b7} Oct 21",
                "Singapore \u{b7} Oct 23",
                "Disney Adventure \u{b7} Oct 26\u{2013}29",
                "Home \u{b7} Oct 30",
            ]
        );
        // Every leg is in exactly one hop, in order; the ride to the
        // terminal joins the sailing that leaves it.
        assert_eq!(hops[1].legs, 1..3);
        let covered: Vec<usize> = hops.iter().flat_map(|h| h.legs.clone()).collect();
        assert_eq!(covered, (0..j.legs.len()).collect::<Vec<_>>());
        assert_eq!(hops[0].secs, HOP_FLIGHT_SECS);
        assert_eq!(hops[1].secs, HOP_SECS);
        assert_eq!(hops[8].secs, HOP_LOOP_SECS);
        // The round trip plays on its first day, not the day it returns.
        assert_eq!(hops[8].day, Some(jiff::civil::date(2026, 10, 26)));
        assert!(hops[9].home && !hops[0].home);
        assert_eq!(
            j.home_labels().collect::<Vec<_>>(),
            ["Los Angeles", "Los Angeles"],
            "the arrival home counts even with its map label hidden"
        );
        assert_eq!(
            date_span(
                jiff::civil::date(2026, 10, 30),
                jiff::civil::date(2026, 11, 2)
            ),
            "Oct 30 \u{2013} Nov 2"
        );
    }

    #[test]
    fn hops_play_at_the_move_not_the_start_of_the_day() {
        let itin = Itinerary::from_json(
            r#"{"stops": [
                {"id": "a", "label": "Los Angeles", "lat": 34.05, "lon": -118.24, "home": true, "depart": "2026-09-20"},
                {"id": "b", "label": "Tokyo", "lat": 35.68, "lon": 139.69, "arrive": "2026-09-21", "depart": "2026-09-28"},
                {"id": "c", "label": "Osaka", "lat": 34.69, "lon": 135.5, "arrive": "2026-09-28", "depart": "2026-09-30"},
                {"id": "d", "label": "Los Angeles", "lat": 34.05, "lon": -118.24, "home": true, "arrive": "2026-10-01"}],
              "legs": [{"from": "a", "to": "b", "mode": "flight"},
                       {"from": "b", "to": "c", "mode": "ground"},
                       {"from": "c", "to": "d", "mode": "flight"}]}"#,
        )
        .unwrap();
        let hops = Journey::build(&itin, &[]).hops();
        assert_eq!(hops.len(), 3);
        let tokyo = Some((35.68, 139.69));
        let osaka = Some((34.69, 135.5));
        let la = Some((34.05, -118.24));
        // Local hour (UTC+9) of a day in the trip.
        let at = |m: i8, d: i8, h: i8| {
            let t = jiff::civil::date(2026, m, d).at(h, 0, 0, 0);
            let ts = jiff::tz::TimeZone::UTC.to_timestamp(t).unwrap();
            Some((ts.as_second() - 9 * 3600, Some(9 * 3600)))
        };
        // Landing in Tokyo, then the 28th: hotel, station and train still
        // near Tokyo (one without GPS), then Osaka, then the flight home.
        let shots = [
            at(9, 21, 10),
            at(9, 28, 7),
            at(9, 28, 8),
            at(9, 28, 9),
            at(9, 28, 10),
            at(9, 28, 15),
            at(9, 30, 9),
            at(10, 1, 9),
        ];
        let gps = [tokyo, tokyo, None, tokyo, None, osaka, osaka, la];
        let cuts = hop_cuts(&hops, &shots, &gps);
        assert_eq!(cuts, [0, 5, 7], "Osaka plays at the first Osaka shot");
        let placed = with_moves(hops.clone(), &shots, &gps);
        // The hop and the segments agree on every shot.
        let reached: Vec<_> = shots
            .iter()
            .zip(&gps)
            .map(|(s, g)| hops_reached(&placed, *s, *g))
            .collect();
        assert_eq!(reached, [1, 1, 1, 1, 1, 2, 2, 3]);
        // No shot on the 28th is nearer Osaka: the first on the day does.
        let mut far = gps;
        far[5] = tokyo;
        far[6] = tokyo;
        assert_eq!(hop_cuts(&hops, &shots, &far)[1], 1);
        let placed = with_moves(hops.clone(), &shots, &far);
        assert_eq!(hops_reached(&placed, shots[1], far[1]), 2);
        assert_eq!(hops_reached(&placed, shots[0], far[0]), 1);
        // No GPS at all on the day: the first shot on the day.
        let blind = [tokyo, None, None, None, None, None, None, la];
        assert_eq!(hop_cuts(&hops, &shots, &blind), [0, 1, 7]);
        // The hop home still waits for a shot taken at home.
        assert_eq!(hop_cuts(&hops, &shots[..7], &gps[..7])[2], 7);
    }

    #[test]
    fn hops_play_where_the_film_reaches_their_day() {
        let j = journey();
        let hops = j.hops();
        // Noon local time (UTC+9) on a day of October 2026.
        let shot = |day: i8| {
            let noon = jiff::civil::date(2026, 10, day).at(12, 0, 0, 0);
            let ts = jiff::tz::TimeZone::UTC.to_timestamp(noon).unwrap();
            Some((ts.as_second() - 9 * 3600, Some(9 * 3600)))
        };
        // Airport, Tokyo x2, sea day, Osaka, undated, Jeju, Singapore
        // (through the cruise), the morning of the flight home.
        let shots = [
            shot(3),
            shot(4),
            shot(5),
            shot(10),
            shot(11),
            None,
            shot(14),
            shot(24),
            shot(27),
            shot(30),
        ];
        let no_gps = [None; 10];
        let cuts = hop_cuts(&hops, &shots, &no_gps);
        assert_eq!(cuts[0], 1, "Tokyo after the airport shot");
        assert_eq!(cuts[1], 4, "Osaka after the sea day");
        // No Fukuoka shots: its hop plays back to back with Jeju's.
        assert_eq!((cuts[2], cuts[3]), (6, 6));
        // Ports with no shots at all queue up before the next dated shot.
        assert_eq!(&cuts[4..8], [7, 7, 7, 7]);
        assert_eq!(cuts[8], 8, "the cruise before its first day's shots");
        // Home closes the trip when no shot was taken back home, even
        // though shots share its date.
        assert_eq!(cuts[9], shots.len());
        // A last shot back home (Ontario, 50 km from LAX): the hop home
        // plays before it, after the morning in Singapore.
        let mut gps = no_gps.to_vec();
        gps[8] = Some((1.36, 103.99));
        gps[9] = Some((34.06, -117.6));
        let home_cuts = hop_cuts(&hops, &shots, &gps);
        assert_eq!(home_cuts[9], 9, "home before the shot taken at home");
        assert!(j.near_home((34.06, -117.6)), "Ontario is home");
        assert!(!j.near_home((1.36, 103.99)), "Changi is not");
        // The departure shot at the home airport predates the hop home, so
        // it does not pull the hop home to the start.
        gps[0] = Some((33.94, -118.41));
        assert_eq!(hop_cuts(&hops, &shots, &gps)[9], 9);
        // An undated hop home closes the trip, as no shot counts it.
        let mut undated = hops.clone();
        undated[9].day = None;
        assert_eq!(hop_cuts(&undated, &shots, &gps)[9], shots.len());
        assert_eq!(hops_reached(&undated, shot(30), Some((34.06, -117.6))), 9);
        assert_eq!(hops_reached(&hops, shot(30), Some((34.06, -117.6))), 10);
        assert_eq!(hops_reached(&hops, shot(30), Some((1.36, 103.99))), 9);
        assert!(cuts.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(hop_cuts(&hops, &[], &[]), vec![0; hops.len()]);
        // Shots either side of a hop's day count a different number of
        // hops made, so they are never shown in one collage.
        assert_eq!(hops_reached(&hops, shot(3), None), 0);
        assert_eq!(hops_reached(&hops, shot(10), None), 1);
        assert_eq!(hops_reached(&hops, shot(11), None), 2);
        assert_eq!(hops_reached(&hops, None, None), 0);
        // Away from home the hop home splits nothing.
        assert_eq!(hops_reached(&hops, shot(30), None), 9);
        // A stop with only a departure date still dates its hop.
        let itin = Itinerary::from_json(
            r#"{"stops": [
                {"id": "a", "label": "Osaka", "lat": 34.6, "lon": 135.4, "depart": "2026-10-11"},
                {"id": "b", "label": "Kyoto", "lat": 35.0, "lon": 135.8, "depart": "2026-10-12"}],
              "legs": [{"from": "a", "to": "b", "mode": "ground"}]}"#,
        )
        .unwrap();
        let j = Journey::build(&itin, &[]);
        assert_eq!(j.hops()[0].day, Some(jiff::civil::date(2026, 10, 12)));
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
        // After the loop the camera holds on it for a moment.
        assert!((s.depart[10] - s.arrive[10] - LOOP_HOLD_SECS).abs() < 1e-9);
        assert!(s.arrive.windows(2).all(|w| w[1] > w[0]));
        assert_eq!(s.pull_back_at, s.arrive[11] + END_TAIL_SECS);
        assert_eq!(s.total_secs, s.pull_back_at + PULL_BACK_SECS);
        assert_eq!(s.leg_secs[0], FLIGHT_SECS);
        assert_eq!(s.leg_secs[9], LOOP_SECS);
    }

    #[test]
    fn an_explicit_leg_path_is_used_as_given() {
        let json = JSON.replacen(
            "\"loop\": true,",
            "\"loop\": true, \"path\": [[1.265, 103.864], [2.7, 105.3], [1.265, 103.864]],",
            1,
        );
        let j = Journey::build(&Itinerary::from_json(&json).unwrap(), &[]);
        // Used as given (longitudes continue the journey's unwrapped run).
        let p = &j.legs[9].path;
        // Smoothed through its three points: 2 spans x 10 samples + the start.
        assert_eq!(p.len(), 21);
        assert!((p[10].0 - 2.7).abs() < 1e-9 && (p[10].1 + 360.0 - 105.3).abs() < 1e-6);
        assert!((p[0].0 - p[20].0).abs() < 0.01 && (p[0].1 - p[20].1).abs() < 0.01);
        // A path with one point is rejected.
        let bad = JSON.replacen(
            "\"loop\": true,",
            "\"loop\": true, \"path\": [[1.0, 2.0]],",
            1,
        );
        assert!(Itinerary::from_json(&bad).is_err());
    }

    #[test]
    fn traced_legs_leave_a_stop_on_the_tangent_they_arrived_on() {
        // Jeju: arrive heading west-north-west, leave south-south-west.
        let it = Itinerary::from_json(JSON_TWO_TRACED).unwrap();
        let j = Journey::build(&it, &[]);
        let (a, b) = (&j.legs[1].path, &j.legs[2].path);
        let (p, n) = (a[a.len() - 2], a[a.len() - 1]);
        let (m, q) = (b[0], b[1]);
        assert_eq!(n, m);
        let ang = |u: (f64, f64), v: (f64, f64)| (v.0 - u.0).atan2(v.1 - u.1).to_degrees();
        let turn = (ang(p, n) - ang(m, q) + 540.0).rem_euclid(360.0) - 180.0;
        let turn = turn.abs();
        assert!(turn < 15.0, "tangent kink of {turn} degrees at the stop");
    }

    const JSON_TWO_TRACED: &str = r#"{
      "stops": [
        { "id": "a", "label": "A", "lat": 35.0, "lon": 139.0, "depart": "2026-10-03", "kind": "home" },
        { "id": "b", "label": "B", "lat": 34.0, "lon": 135.0, "arrive": "2026-10-04", "depart": "2026-10-04" },
        { "id": "c", "label": "C", "lat": 33.2, "lon": 126.5, "arrive": "2026-10-05", "depart": "2026-10-05", "kind": "port" },
        { "id": "d", "label": "D", "lat": 25.1, "lon": 121.7, "arrive": "2026-10-06", "depart": "2026-10-06", "kind": "port" }
      ],
      "legs": [
        { "from": "a", "to": "b", "mode": "flight" },
        { "from": "b", "to": "c", "mode": "ship", "ship": "S", "path": [[34.0, 135.0], [33.4, 128.8], [33.17, 126.65], [33.2, 126.5]] },
        { "from": "c", "to": "d", "mode": "ship", "ship": "S", "path": [[33.2, 126.5], [33.1, 126.2], [31.2, 125.1], [25.1, 121.7]] },
        { "from": "d", "to": "d", "mode": "ship", "ship": "T", "loop": true }
      ]
    }"#;

    #[test]
    fn catmull_rom_passes_through_its_points_and_keeps_the_ends() {
        let pts = [(0.0, 0.0), (1.0, 2.0), (0.0, 4.0), (-1.0, 2.0)];
        let s = catmull_rom(&pts, 8);
        assert_eq!(s.len(), 1 + 3 * 8);
        assert_eq!(s[0], pts[0]);
        assert_eq!(*s.last().unwrap(), pts[3]);
        for (i, p) in pts.iter().enumerate().skip(1) {
            let q = s[i * 8];
            assert!((q.0 - p.0).abs() < 1e-9 && (q.1 - p.1).abs() < 1e-9);
        }
        // Rounder than the polyline: the middle sample leaves the chord.
        assert!(catmull_rom(&pts[..2], 4).len() == 2);
    }

    #[test]
    fn photos_at_sea_shape_a_ship_leg() {
        let it = Itinerary::from_json(JSON).unwrap();
        let mut a = Asset::new("p", std::path::PathBuf::from("p"), AssetKind::Photo);
        // Off Tanegashima on a sea day between Osaka (10-11) and Fukuoka (10-13).
        a.gps = Some((30.8, 131.5));
        a.taken_at =
            crate::metadata::wall_to_unix(jiff::civil::datetime(2026, 10, 12, 12, 0, 0, 0));
        let j = Journey::build(&it, &[a]);
        assert!(
            j.legs[3].path.iter().any(|p| p.0 < 32.0),
            "south of Kyushu: {:?}",
            j.legs[3].path
        );
        assert!(is_water((30.8, 131.5)));
    }
}
