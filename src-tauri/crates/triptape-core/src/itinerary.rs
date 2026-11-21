//! The owner's real itinerary: stops (cities and ports with dates), legs
//! (flight, ship, ground) and the cruises between them.
//!
//! When given, it replaces GPS-clustered place naming and the photo-derived
//! route: chapters are labelled with the itinerary's city, or with the ship
//! name when the shot was taken on board, and the closing map travels the
//! itinerary (see [`crate::journey`]). Without one nothing here is used.

use crate::geo::{haversine_m, land_fraction};
use crate::types::{Asset, Chapter, Plan};
use jiff::civil::Date as NaiveDate;
use serde::Deserialize;

/// A shot this close to a docked ship's terminal counts as on board.
pub const DOCK_RADIUS_M: f64 = 1_500.0;

/// On a port day, fixes this close to the port (and not at the dock) are
/// ashore even when the coarse land grid calls them water: Jeju's seaside
/// Seogwipo sits 3-14 km from the Gangjeong cruise terminal.
pub const PORT_SHORE_RADIUS_M: f64 = 30_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LegMode {
    Flight,
    Ship,
    Ground,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StopKind {
    Home,
    #[default]
    City,
    Port,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ItinStop {
    pub id: String,
    pub label: String,
    pub lat: f64,
    pub lon: f64,
    #[serde(default)]
    pub arrive: Option<String>,
    #[serde(default)]
    pub depart: Option<String>,
    #[serde(default)]
    pub kind: StopKind,
    /// The stop is a waypoint (an embarkation terminal, the arrival home):
    /// no pin label on the map.
    #[serde(default)]
    pub hide_label: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ItinLeg {
    pub from: String,
    pub to: String,
    pub mode: LegMode,
    #[serde(default)]
    pub ship: Option<String>,
    /// A round trip out and back to the same stop (a cruise from and to
    /// one port).
    #[serde(default, rename = "loop")]
    pub is_loop: bool,
    /// `YYYY-MM-DD..YYYY-MM-DD`: when the ship leg runs, if the stops'
    /// dates do not say.
    #[serde(default)]
    pub dates: Option<String>,
    /// An explicit `[lat, lon]` path for the leg (e.g. a cruise traced from
    /// the line's own itinerary map); drawn as given instead of a routed or
    /// generated path. Must have two or more points.
    #[serde(default)]
    pub path: Option<Vec<[f64; 2]>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Itinerary {
    pub stops: Vec<ItinStop>,
    pub legs: Vec<ItinLeg>,
    /// Places the owner names themselves (Taipei 101, a night market, a
    /// ramen hall): photos taken inside one are labelled with it, ahead of
    /// the built-in attraction list, which misses many of them.
    #[serde(default)]
    pub places: Vec<NamedPlace>,
    /// Built-in attraction names never to use: places the owner was near
    /// but did not visit (the ArtScience Museum beside their hotel).
    #[serde(default)]
    pub hide_attractions: Vec<String>,
}

/// A place the owner named: a point and the reach that counts as there,
/// or a local time window (`from`/`until`, `YYYY-MM-DD HH:MM`) for places
/// GPS cannot tell apart, such as two glasshouses 130 m apart where indoor
/// fixes drift. A window, when given, decides on its own.
#[derive(Debug, Clone, Deserialize)]
pub struct NamedPlace {
    pub name: String,
    #[serde(default)]
    pub lat: Option<f64>,
    #[serde(default)]
    pub lon: Option<f64>,
    /// Metres from the point that count as at the place (default 300).
    #[serde(default = "default_place_radius")]
    pub radius_m: f64,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub until: Option<String>,
}

impl NamedPlace {
    /// The local time window, when both ends parse.
    fn window(&self) -> Option<(jiff::civil::DateTime, jiff::civil::DateTime)> {
        let parse = |s: &Option<String>| {
            jiff::civil::DateTime::strptime("%Y-%m-%d %H:%M", s.as_deref()?.trim()).ok()
        };
        Some((parse(&self.from)?, parse(&self.until)?))
    }

    /// Metres from `gps` when the place has a point and `gps` is inside it.
    fn reach(&self, gps: Option<(f64, f64)>) -> Option<f64> {
        let d = haversine_m(gps?, (self.lat?, self.lon?));
        (d <= self.radius_m).then_some(d)
    }
}

fn default_place_radius() -> f64 {
    300.0
}

/// One cruise: a ship and the days the owner is on board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cruise {
    pub ship: String,
    pub start: NaiveDate,
    pub end: NaiveDate,
}

/// What a photo's place label should be under the itinerary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DayLabel {
    pub place: String,
    pub on_ship: bool,
}

fn parse_date(s: &str) -> Result<NaiveDate, String> {
    NaiveDate::strptime("%Y-%m-%d", s.trim())
        .map_err(|_| format!("bad date {s:?} (want YYYY-MM-DD)"))
}

impl ItinStop {
    pub fn pos(&self) -> (f64, f64) {
        (self.lat, self.lon)
    }

    fn date(&self, field: &Option<String>) -> Option<NaiveDate> {
        field.as_deref().and_then(|s| parse_date(s).ok())
    }

    pub fn arrive_date(&self) -> Option<NaiveDate> {
        self.date(&self.arrive)
    }

    pub fn depart_date(&self) -> Option<NaiveDate> {
        self.date(&self.depart)
    }

    /// Days the owner is at this stop (arrive to depart, either alone is one day).
    fn window(&self) -> Option<(NaiveDate, NaiveDate)> {
        match (self.arrive_date(), self.depart_date()) {
            (Some(a), Some(d)) => Some((a, d.max(a))),
            (Some(a), None) => Some((a, a)),
            (None, Some(d)) => Some((d, d)),
            (None, None) => None,
        }
    }

    fn active_on(&self, day: NaiveDate) -> bool {
        self.window().is_some_and(|(a, d)| a <= day && day <= d)
    }
}

impl Itinerary {
    pub fn from_json(s: &str) -> Result<Self, String> {
        let it: Self = serde_json::from_str(s).map_err(|e| format!("itinerary json: {e}"))?;
        it.validate()?;
        Ok(it)
    }

    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("read itinerary {}: {e}", path.display()))?;
        Self::from_json(&text)
    }

    /// The owner-named place a shot was taken in: one whose time window
    /// holds its local time, else the nearest whose radius reaches its GPS.
    pub fn place_of(&self, a: &Asset) -> Option<&str> {
        let when = a
            .taken_at
            .and_then(|t| crate::metadata::local_datetime(t, a.tz_offset_secs));
        let timed = self.places.iter().find(|pl| {
            pl.window()
                .zip(when)
                .is_some_and(|((from, until), w)| from <= w && w <= until)
        });
        timed
            .or_else(|| {
                self.places
                    .iter()
                    .filter(|pl| pl.window().is_none())
                    .filter_map(|pl| pl.reach(a.gps).map(|d| (pl, d)))
                    .min_by(|x, y| x.1.total_cmp(&y.1))
                    .map(|(pl, _)| pl)
            })
            .map(|pl| pl.name.as_str())
    }

    /// Whether the owner asked never to name this attraction.
    pub fn hides_attraction(&self, name: &str) -> bool {
        self.hide_attractions.iter().any(|h| h.trim() == name)
    }

    /// Stop by id.
    pub fn stop(&self, id: &str) -> Option<&ItinStop> {
        self.stops.iter().find(|s| s.id == id)
    }

    fn validate(&self) -> Result<(), String> {
        if self.stops.len() < 2 || self.legs.is_empty() {
            return Err("itinerary needs at least two stops and one leg".into());
        }
        for pl in &self.places {
            let timed = pl.from.is_some() || pl.until.is_some();
            if timed && pl.window().is_none() {
                return Err(format!(
                    "place {:?}: from/until must both be \"YYYY-MM-DD HH:MM\"",
                    pl.name
                ));
            }
            if !timed && (pl.lat.is_none() || pl.lon.is_none()) {
                return Err(format!(
                    "place {:?} needs lat and lon, or from/until",
                    pl.name
                ));
            }
        }
        for (i, s) in self.stops.iter().enumerate() {
            if self.stops[..i].iter().any(|o| o.id == s.id) {
                return Err(format!("duplicate stop id {:?}", s.id));
            }
            if !(-90.0..=90.0).contains(&s.lat) || !(-180.0..=180.0).contains(&s.lon) {
                return Err(format!("stop {:?} has an invalid position", s.id));
            }
            for d in [&s.arrive, &s.depart].into_iter().flatten() {
                parse_date(d).map_err(|e| format!("stop {:?}: {e}", s.id))?;
            }
        }
        let mut at = &self.legs[0].from;
        for leg in &self.legs {
            for id in [&leg.from, &leg.to] {
                if self.stop(id).is_none() {
                    return Err(format!("leg references unknown stop {id:?}"));
                }
            }
            if &leg.from != at {
                return Err(format!(
                    "legs must form one chain: leg from {:?} follows a leg ending at {at:?}",
                    leg.from
                ));
            }
            if leg.is_loop && leg.from != leg.to {
                return Err(format!("loop leg must start and end at {:?}", leg.from));
            }
            if leg.mode == LegMode::Ship && leg.ship.as_deref().is_none_or(str::is_empty) {
                return Err(format!(
                    "ship leg {:?} -> {:?} needs a ship name",
                    leg.from, leg.to
                ));
            }
            if let Some(d) = &leg.dates {
                parse_range(d)?;
            }
            if let Some(p) = &leg.path {
                if p.len() < 2
                    || p.iter().any(|q| {
                        !(-90.0..=90.0).contains(&q[0]) || !(-180.0..=180.0).contains(&q[1])
                    })
                {
                    return Err(format!(
                        "leg {:?} -> {:?} has an invalid path",
                        leg.from, leg.to
                    ));
                }
            }
            at = &leg.to;
        }
        Ok(())
    }

    /// Stop positions along the legs' chain (loops and repeats included), as
    /// the static route card's polyline.
    pub fn polyline(&self) -> Vec<(f64, f64)> {
        let mut out = vec![];
        if let Some(first) = self.legs.first().and_then(|l| self.stop(&l.from)) {
            out.push(first.pos());
        }
        for leg in &self.legs {
            if let Some(s) = self.stop(&leg.to) {
                out.push(s.pos());
            }
        }
        out
    }

    /// The ship legs' date ranges, merged per ship when consecutive.
    pub fn cruises(&self) -> Vec<Cruise> {
        let mut out: Vec<Cruise> = vec![];
        let mut prev_ship_leg = false;
        for leg in &self.legs {
            let ship = match (leg.mode, &leg.ship) {
                (LegMode::Ship, Some(s)) => s,
                _ => {
                    prev_ship_leg = false;
                    continue;
                }
            };
            let range = leg
                .dates
                .as_deref()
                .and_then(|d| parse_range(d).ok())
                .or_else(|| {
                    let start = self
                        .stop(&leg.from)
                        .and_then(|s| s.depart_date().or(s.arrive_date()))?;
                    let end = self
                        .stop(&leg.to)
                        .and_then(|s| s.arrive_date().or(s.depart_date()))?;
                    Some((start, end.max(start)))
                });
            let Some((start, end)) = range else {
                prev_ship_leg = false;
                continue;
            };
            match out.last_mut() {
                Some(c) if prev_ship_leg && c.ship == *ship && leg.dates.is_none() => {
                    c.end = c.end.max(end);
                }
                _ => out.push(Cruise {
                    ship: ship.clone(),
                    start,
                    end,
                }),
            }
            prev_ship_leg = true;
        }
        out
    }

    /// Every stop a ship docks at (the endpoints of ship legs), for the
    /// "on board while docked" test.
    fn docks(&self) -> Vec<&ItinStop> {
        let mut out: Vec<&ItinStop> = vec![];
        for leg in self.legs.iter().filter(|l| l.mode == LegMode::Ship) {
            for id in [&leg.from, &leg.to] {
                if let Some(s) = self.stop(id) {
                    if !out.iter().any(|o| o.id == s.id) {
                        out.push(s);
                    }
                }
            }
        }
        for s in self.stops.iter().filter(|s| s.kind == StopKind::Port) {
            if !out.iter().any(|o| o.id == s.id) {
                out.push(s);
            }
        }
        out
    }

    /// The label for a shot taken on `day` at `gps` (`None`: no fix).
    ///
    /// On a cruise day the shot is on board when it was taken at sea (but
    /// not within [`PORT_SHORE_RADIUS_M`] of the day's port), within
    /// [`DOCK_RADIUS_M`] of a dock, or has no GPS; otherwise it is ashore and
    /// takes the city of the stop the owner is at that day. Off the cruise
    /// the city of the active stop applies. Never a place-of-interest name.
    pub fn label_for(&self, day: NaiveDate, gps: Option<(f64, f64)>) -> Option<DayLabel> {
        // The stop the owner is at today: a city or port first, home last.
        let city = || {
            let at = |s: &&ItinStop| s.active_on(day);
            self.stops
                .iter()
                .filter(at)
                .find(|s| s.kind != StopKind::Home)
                .or_else(|| self.stops.iter().find(at))
                .map(|s| s.label.clone())
        };
        let cruise = self
            .cruises()
            .into_iter()
            .find(|c| c.start <= day && day <= c.end);
        let Some(cruise) = cruise else {
            return Some(DayLabel {
                place: city()?,
                on_ship: false,
            });
        };
        // Today's dock, if the ship ties up somewhere today: a port call,
        // or the city a cruise starts or ends in (Singapore's Marina Bay is
        // reclaimed land the coarse grid reads as water).
        let docks = self.docks();
        let port = docks.iter().find(|s| s.active_on(day));
        let on_board = match gps {
            None => true,
            Some(p) => {
                let at_dock = self
                    .docks()
                    .iter()
                    .any(|d| haversine_m(p, d.pos()) <= DOCK_RADIUS_M);
                // The land grid is ~5 km cells, so a seaside town reads as
                // water. On a port day the ship is tied up at the dock, so
                // a fix near the port but away from the dock is ashore.
                let ashore_near_port =
                    port.is_some_and(|s| haversine_m(p, s.pos()) <= PORT_SHORE_RADIUS_M);
                at_dock || (land_fraction(p) < 0.5 && !ashore_near_port)
            }
        };
        if on_board {
            return Some(DayLabel {
                place: cruise.ship,
                on_ship: true,
            });
        }
        // Ashore on a cruise day: the active stop's city, else the ship
        // (a land fix on a sea day cannot be placed).
        Some(match city() {
            Some(place) => DayLabel {
                place,
                on_ship: false,
            },
            None => DayLabel {
                place: cruise.ship,
                on_ship: true,
            },
        })
    }

    /// `Carnival Luminosa Embarkation` on the first day of a cruise,
    /// `… Disembarkation` on its last: what a day spent boarding the ship
    /// was about, rather than whatever stands near the terminal (Miraikan
    /// and Shiokaze Park face Tokyo's Aomi terminal).
    pub fn boarding(&self, day: NaiveDate) -> Option<String> {
        self.cruises().into_iter().find_map(|c| {
            if c.start == day {
                Some(format!("{} Embarkation", c.ship))
            } else if c.end == day {
                Some(format!("{} Disembarkation", c.ship))
            } else {
                None
            }
        })
    }

    /// The port the ship calls at on `day`, when it does: a port day spent
    /// ashore is about the port even if its first shots were on deck.
    pub fn port_on(&self, day: NaiveDate) -> Option<String> {
        self.stops
            .iter()
            .find(|s| s.kind == StopKind::Port && s.active_on(day) && !s.hide_label)
            .map(|s| s.label.clone())
    }

    /// First and last day of the trip: the earliest and latest dates of
    /// any stop or dated leg (a cruise may give only its leg's `dates`).
    /// `None` when nothing carries a date.
    pub fn span(&self) -> Option<(NaiveDate, NaiveDate)> {
        let stops = self.stops.iter().filter_map(ItinStop::window);
        let legs = self
            .legs
            .iter()
            .filter_map(|l| l.dates.as_deref().and_then(|d| parse_range(d).ok()));
        stops.chain(legs).fold(None, |acc, (a, d)| {
            Some(match acc {
                None => (a, d),
                Some((s, e)) => (s.min(a), e.max(d)),
            })
        })
    }

    /// Ship names on this itinerary (never shown as map points).
    pub fn ships(&self) -> Vec<String> {
        let mut out: Vec<String> = vec![];
        for leg in &self.legs {
            if let (LegMode::Ship, Some(s)) = (leg.mode, &leg.ship) {
                if !out.contains(s) {
                    out.push(s.clone());
                }
            }
        }
        out
    }
}

fn parse_range(s: &str) -> Result<(NaiveDate, NaiveDate), String> {
    let (a, b) = s
        .split_once("..")
        .ok_or_else(|| format!("bad date range {s:?} (want YYYY-MM-DD..YYYY-MM-DD)"))?;
    let (a, b) = (parse_date(a)?, parse_date(b)?);
    if b < a {
        return Err(format!("date range {s:?} ends before it starts"));
    }
    Ok((a, b))
}

/// Local calendar day of an asset.
pub fn asset_day(a: &Asset) -> Option<NaiveDate> {
    crate::metadata::local_date(a.taken_at?, a.tz_offset_secs)
}

/// Relabel a plan under the itinerary: each chapter takes the label most
/// of its GPS fixes get on its first day (city, or the ship when on
/// board; see [`chapter_label`]), and the
/// static route polyline follows the itinerary. Place names from the
/// gazetteer (POIs, hamlets) are not used. Story-editor renames still win,
/// because [`crate::plan::apply_edits`] runs after this.
pub fn apply_itinerary(plan: &Plan, assets: &[Asset], itin: &Itinerary) -> Plan {
    let by_id: std::collections::HashMap<&str, &Asset> =
        assets.iter().map(|a| (a.id.as_str(), a)).collect();
    let mut out = plan.clone();
    for ch in &mut out.chapters {
        relabel_chapter(ch, &by_id, itin);
    }
    out.map_polyline = itin.polyline();
    out
}

/// The label most of a chapter's GPS fixes get on `day` (ties: the label
/// reached first in story order). One medoid fix is not enough on a port
/// day: a few deck or tender shots near the dock could pull it on board
/// and title a whole day ashore in Jeju with the ship's name. Shots
/// without GPS cast no vote; a chapter with none takes the no-fix label.
fn chapter_label(itin: &Itinerary, day: NaiveDate, members: &[&Asset]) -> Option<DayLabel> {
    let mut votes: Vec<(DayLabel, usize)> = vec![];
    for gps in members.iter().filter_map(|a| a.gps) {
        let Some(label) = itin.label_for(day, Some(gps)) else {
            continue;
        };
        match votes.iter_mut().find(|(l, _)| *l == label) {
            Some((_, n)) => *n += 1,
            None => votes.push((label, 1)),
        }
    }
    if votes.is_empty() {
        return itin.label_for(day, None);
    }
    // `max_by_key` keeps the last maximum; reversing keeps the first.
    votes
        .into_iter()
        .rev()
        .max_by_key(|(_, n)| *n)
        .map(|(l, _)| l)
}

fn relabel_chapter(
    ch: &mut Chapter,
    by_id: &std::collections::HashMap<&str, &Asset>,
    itin: &Itinerary,
) {
    let members: Vec<&Asset> = ch
        .asset_ids
        .iter()
        .filter_map(|id| by_id.get(id.as_str()).copied())
        .collect();
    let Some(day) = members.iter().filter_map(|a| asset_day(a)).min() else {
        return;
    };
    let Some(label) = chapter_label(itin, day, &members) else {
        ch.place = None;
        return;
    };
    let place = chapter_name(itin, day, &members, label);
    ch.title.clone_from(&place);
    ch.place = Some(place);
}

/// A relabelled chapter's name, shared by every keepsake: a place the
/// owner named when most of its photos are there; on board, the boarding
/// (`Carnival Luminosa Embarkation`) on a cruise's first or last day, else
/// the port on a port day, else the ship; ashore, the attraction its
/// photos visited (see [`crate::places`]), else the stop's city.
fn chapter_name(itin: &Itinerary, day: NaiveDate, members: &[&Asset], label: DayLabel) -> String {
    let mut named: Vec<(&str, usize)> = vec![];
    for name in members.iter().filter_map(|a| itin.place_of(a)) {
        match named.iter_mut().find(|(n, _)| *n == name) {
            Some((_, c)) => *c += 1,
            None => named.push((name, 1)),
        }
    }
    // A chapter spanning several named places (the Flower Dome, then the
    // Cloud Forest) is named for what holds them, not its biggest part.
    let several = named
        .iter()
        .filter(|(_, c)| *c >= crate::places::PLACE_MIN_SHOTS)
        .count()
        > 1;
    if let Some((name, _)) = named
        .into_iter()
        .rev()
        .max_by_key(|(_, c)| *c)
        .filter(|(_, c)| !several && 2 * c >= members.len())
    {
        return name.to_string();
    }
    if label.on_ship {
        return itin
            .boarding(day)
            .or_else(|| itin.port_on(day))
            .unwrap_or(label.place);
    }
    let fixes: Vec<(f64, f64)> = members.iter().filter_map(|a| a.gps).collect();
    crate::places::visited_attraction(&fixes, crate::places::PLACE_MIN_SHOTS, |n| {
        itin.hides_attraction(n)
    })
    .unwrap_or(label.place)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AssetKind;

    const JSON: &str = r#"{
      "stops": [
        { "id": "lax", "label": "Los Angeles", "lat": 33.94, "lon": -118.41, "depart": "2026-10-03", "kind": "home" },
        { "id": "tokyo", "label": "Tokyo", "lat": 35.71, "lon": 139.80, "arrive": "2026-10-04", "depart": "2026-10-09" },
        { "id": "tokyo-port", "label": "Tokyo", "lat": 35.6177, "lon": 139.771, "arrive": "2026-10-09", "depart": "2026-10-09", "kind": "port", "hide_label": true },
        { "id": "osaka", "label": "Osaka", "lat": 34.656, "lon": 135.4306, "arrive": "2026-10-11", "depart": "2026-10-11", "kind": "port" },
        { "id": "sg", "label": "Singapore", "lat": 1.2655, "lon": 103.864, "arrive": "2026-10-23", "depart": "2026-10-30" },
        { "id": "lax2", "label": "Los Angeles", "lat": 33.94, "lon": -118.41, "arrive": "2026-10-30", "kind": "home", "hide_label": true }
      ],
      "legs": [
        { "from": "lax", "to": "tokyo", "mode": "flight" },
        { "from": "tokyo", "to": "tokyo-port", "mode": "ground" },
        { "from": "tokyo-port", "to": "osaka", "mode": "ship", "ship": "Carnival Luminosa" },
        { "from": "osaka", "to": "sg", "mode": "ship", "ship": "Carnival Luminosa" },
        { "from": "sg", "to": "sg", "mode": "ship", "ship": "Disney Adventure", "loop": true, "dates": "2026-10-26..2026-10-29" },
        { "from": "sg", "to": "lax2", "mode": "flight" }
      ]
    }"#;

    fn itin() -> Itinerary {
        Itinerary::from_json(JSON).unwrap()
    }

    fn day(s: &str) -> NaiveDate {
        parse_date(s).unwrap()
    }

    #[test]
    fn parses_and_merges_cruises_per_ship() {
        let cruises = itin().cruises();
        assert_eq!(
            cruises,
            vec![
                Cruise {
                    ship: "Carnival Luminosa".into(),
                    start: day("2026-10-09"),
                    end: day("2026-10-23")
                },
                Cruise {
                    ship: "Disney Adventure".into(),
                    start: day("2026-10-26"),
                    end: day("2026-10-29")
                },
            ]
        );
        assert_eq!(itin().ships(), ["Carnival Luminosa", "Disney Adventure"]);
        // Chain polyline: every stop in order, loops and repeats included.
        assert_eq!(itin().polyline().len(), 7);
    }

    #[test]
    fn rejects_bad_itineraries() {
        let bad = |from: &str, to: &str| JSON.replacen(from, to, 1);
        for (from, to, why) in [
            ("\"to\": \"tokyo\"", "\"to\": \"nowhere\"", "unknown stop"),
            (
                "\"from\": \"tokyo\", \"to\": \"tokyo-port\"",
                "\"from\": \"osaka\", \"to\": \"tokyo-port\"",
                "chain",
            ),
            (
                "\"ship\": \"Carnival Luminosa\"",
                "\"ship\": \"\"",
                "ship name",
            ),
            ("2026-10-04", "10/04/2026", "date"),
            ("\"id\": \"tokyo-port\"", "\"id\": \"tokyo\"", "duplicate"),
        ] {
            assert!(Itinerary::from_json(&bad(from, to)).is_err(), "{why}");
        }
        assert!(Itinerary::from_json("{}").is_err());
    }

    #[test]
    fn land_days_use_the_itinerary_city_never_a_poi() {
        let it = itin();
        // Asakusa shot on a Tokyo land day: the city, whatever the gazetteer says.
        let l = it
            .label_for(day("2026-10-06"), Some((35.7148, 139.7967)))
            .unwrap();
        assert_eq!((l.place.as_str(), l.on_ship), ("Tokyo", false));
        let l = it
            .label_for(day("2026-10-24"), Some((1.28, 103.86)))
            .unwrap();
        assert_eq!(l.place, "Singapore");
        let l = it.label_for(day("2026-10-03"), None).unwrap();
        assert_eq!(l.place, "Los Angeles");
        // Outside every stop's dates: nothing to say.
        assert_eq!(it.label_for(day("2026-09-01"), None), None);
    }

    #[test]
    fn cruise_days_split_on_board_from_ashore_at_1500_m() {
        let it = itin();
        // Osaka port day 10-11. Terminal coordinate: on board.
        let dock = (34.656, 135.4306);
        assert_eq!(
            it.label_for(day("2026-10-11"), Some(dock)).unwrap().place,
            "Carnival Luminosa"
        );
        // ~1 km away: still on board. ~4 km away in town: ashore.
        assert!(
            it.label_for(day("2026-10-11"), Some((34.656, 135.4306 + 0.011)))
                .unwrap()
                .on_ship
        );
        let ashore = it
            .label_for(day("2026-10-11"), Some((34.6687, 135.4986)))
            .unwrap();
        assert_eq!((ashore.place.as_str(), ashore.on_ship), ("Osaka", false));
        // At sea, or with no GPS, on a sea day: the ship.
        let sea = it
            .label_for(day("2026-10-10"), Some((30.0, 140.0)))
            .unwrap();
        assert!(sea.on_ship && sea.place == "Carnival Luminosa");
        assert!(it.label_for(day("2026-10-10"), None).unwrap().on_ship);
        // Land fix on a sea day with no active stop cannot be placed: the ship.
        assert!(
            it.label_for(day("2026-10-15"), Some((36.0, 138.0)))
                .unwrap()
                .on_ship
        );
        // Embark day: ashore in Asakusa is Tokyo, at the terminal is the ship.
        let town = it
            .label_for(day("2026-10-09"), Some((35.7148, 139.7967)))
            .unwrap();
        assert_eq!(town.place, "Tokyo");
        assert!(
            it.label_for(day("2026-10-09"), Some((35.6177, 139.771)))
                .unwrap()
                .on_ship
        );
        // Disney Adventure leaves from the Singapore terminal.
        let d = it
            .label_for(day("2026-10-27"), Some((1.2655, 103.864)))
            .unwrap();
        assert_eq!(d.place, "Disney Adventure");
        let d = it.label_for(day("2026-10-27"), Some((1.8, 105.6))).unwrap();
        assert_eq!(d.place, "Disney Adventure");
    }

    fn photo(id: &str, day: &str, gps: Option<(f64, f64)>) -> Asset {
        let mut a = Asset::new(id, std::path::PathBuf::from(id), AssetKind::Photo);
        let d = parse_date(day).unwrap();
        a.taken_at = crate::metadata::wall_to_unix(d.at(12, 0, 0, 0));
        a.gps = gps;
        a
    }

    #[test]
    fn apply_relabels_chapters_and_replaces_the_polyline() {
        let assets = vec![
            photo("a", "2026-10-06", Some((35.7148, 139.7967))),
            photo("b", "2026-10-10", Some((30.0, 140.0))),
            photo("c", "2026-10-11", Some((34.6687, 135.4986))),
        ];
        let chapter = |id: &str| Chapter {
            title: "Madame Tussauds".into(),
            place: Some("Madame Tussauds".into()),
            asset_ids: vec![id.into()],
            ..Default::default()
        };
        let plan = Plan {
            plan_version: 1,
            schema_version: Plan::SCHEMA_VERSION,
            hero: vec![],
            reel_selects: vec![],
            chapters: vec![chapter("a"), chapter("b"), chapter("c")],
            captions: Default::default(),
            collage: Default::default(),
            map_polyline: vec![(0.0, 0.0)],
            music: crate::types::MusicPick {
                track: String::new(),
                rationale: String::new(),
            },
        };
        let out = apply_itinerary(&plan, &assets, &itin());
        let places: Vec<_> = out
            .chapters
            .iter()
            .map(|c| c.place.clone().unwrap())
            .collect();
        assert_eq!(places, ["Tokyo", "Carnival Luminosa", "Osaka"]);
        assert_eq!(out.chapters[1].title, "Carnival Luminosa");
        assert_eq!(out.map_polyline.len(), 7);
    }

    #[test]
    fn named_places_label_shots_inside_their_radius_or_window() {
        let mut it = itin();
        assert!(it.places.is_empty());
        it.places = serde_json::from_str(
            r#"[
              { "name": "Taipei 101", "lat": 25.0340, "lon": 121.5645, "radius_m": 250 },
              { "name": "Raohe Night Market", "lat": 25.0509, "lon": 121.5775 },
              { "name": "Flower Dome", "from": "2026-10-23 11:30", "until": "2026-10-23 12:30" }
            ]"#,
        )
        .unwrap();
        let at = |gps| photo("p", "2026-10-16", gps);
        assert_eq!(
            it.place_of(&at(Some((25.0343, 121.5648)))),
            Some("Taipei 101")
        );
        assert_eq!(
            it.place_of(&at(Some((25.0510, 121.5770)))),
            Some("Raohe Night Market")
        );
        // Between the two, outside both radii; or no GPS.
        assert_eq!(it.place_of(&at(Some((25.0420, 121.5700)))), None);
        assert_eq!(it.place_of(&at(None)), None);
        // In the window (noon on Oct 23), whatever the GPS says.
        let dome = photo("d", "2026-10-23", Some((25.0343, 121.5648)));
        assert_eq!(it.place_of(&dome), Some("Flower Dome"));
        // A malformed window is refused when loading.
        let bad = JSON.replacen(
            "\"legs\"",
            r#""places": [{ "name": "X", "from": "Oct 23", "until": "2026-10-23 12:30" }], "legs""#,
            1,
        );
        assert!(Itinerary::from_json(&bad).is_err());
    }

    #[test]
    fn chapter_names_are_shared_place_names() {
        let mut it = itin();
        it.places = serde_json::from_str(
            r#"[{ "name": "Dotonbori", "lat": 34.6687, "lon": 135.5013, "radius_m": 200 }]"#,
        )
        .unwrap();
        let name = |it: &Itinerary, date: &str, gps: (f64, f64), n: usize| {
            let members: Vec<Asset> = (0..n)
                .map(|i| photo(&format!("p{i}"), date, Some(gps)))
                .collect();
            let refs: Vec<&Asset> = members.iter().collect();
            let label = chapter_label(it, day(date), &refs).unwrap();
            chapter_name(it, day(date), &refs, label)
        };
        // On board on the cruise's first day, and on a port day.
        assert_eq!(
            name(&it, "2026-10-09", (35.6190, 139.7766), 3),
            "Carnival Luminosa Embarkation"
        );
        assert_eq!(name(&it, "2026-10-11", (34.656, 135.4306), 3), "Osaka");
        // At sea: the ship.
        assert_eq!(
            name(&it, "2026-10-10", (30.0, 140.0), 3),
            "Carnival Luminosa"
        );
        // Ashore: a named place, then a visited attraction, then the city.
        assert_eq!(name(&it, "2026-10-11", (34.6687, 135.5013), 3), "Dotonbori");
        // Two named places in one chapter: neither names it.
        it.places.push(
            serde_json::from_str(
                r#"{ "name": "Kuromon Market", "lat": 34.6655, "lon": 135.5065, "radius_m": 200 }"#,
            )
            .unwrap(),
        );
        let mixed: Vec<Asset> = (0..6)
            .map(|i| {
                let gps = if i < 3 {
                    (34.6687, 135.5013)
                } else {
                    (34.6655, 135.5065)
                };
                photo(&format!("q{i}"), "2026-10-11", Some(gps))
            })
            .collect();
        let refs: Vec<&Asset> = mixed.iter().collect();
        let label = chapter_label(&it, day("2026-10-11"), &refs).unwrap();
        assert_eq!(chapter_name(&it, day("2026-10-11"), &refs, label), "Osaka");
        assert_eq!(
            name(&it, "2026-10-06", (35.6267, 139.8881), 3),
            "Tokyo DisneySea"
        );
        assert_eq!(name(&it, "2026-10-06", (35.6267, 139.8881), 2), "Tokyo");
        it.hide_attractions = vec!["Tokyo DisneySea".into()];
        assert_eq!(name(&it, "2026-10-06", (35.6267, 139.8881), 3), "Tokyo");
    }

    #[test]
    fn port_on_names_port_days_only() {
        assert_eq!(itin().port_on(day("2026-10-11")).as_deref(), Some("Osaka"));
        assert_eq!(itin().port_on(day("2026-10-10")), None);
        // The embarkation port is hidden on the map and has no port day.
        assert_eq!(itin().port_on(day("2026-10-09")), None);
    }

    #[test]
    fn boarding_names_a_cruise_first_and_last_day() {
        assert_eq!(
            itin().boarding(day("2026-10-09")).as_deref(),
            Some("Carnival Luminosa Embarkation")
        );
        assert_eq!(
            itin().boarding(day("2026-10-23")).as_deref(),
            Some("Carnival Luminosa Disembarkation")
        );
        assert_eq!(itin().boarding(day("2026-10-11")), None);
    }

    #[test]
    fn span_runs_from_departure_to_homecoming() {
        assert_eq!(itin().span(), Some((day("2026-10-03"), day("2026-10-30"))));
        // A cruise dated only on its leg still sets the span.
        let cruise = Itinerary::from_json(
            r#"{
              "stops": [
                { "id": "sg", "label": "Singapore", "lat": 1.2655, "lon": 103.864 },
                { "id": "pk", "label": "Penang", "lat": 5.41, "lon": 100.34 }
              ],
              "legs": [
                { "from": "sg", "to": "pk", "mode": "ship", "ship": "Disney Adventure", "dates": "2026-12-20..2026-12-24" }
              ]
            }"#,
        )
        .unwrap();
        assert_eq!(cruise.span(), Some((day("2026-12-20"), day("2026-12-24"))));
    }

    #[test]
    fn port_day_ashore_majority_beats_dock_shots() {
        // Osaka port day: two shots at the dock (on board by the dock rule)
        // and three ashore in the city. The medoid could sit at the dock;
        // the day was spent in Osaka.
        let dock = (34.656, 135.4306);
        let city = [
            (34.6687, 135.4986),
            (34.6690, 135.5010),
            (34.6650, 135.5020),
        ];
        let members: Vec<Asset> = [dock, dock]
            .iter()
            .chain(city.iter())
            .enumerate()
            .map(|(i, &g)| photo(&format!("p{i}"), "2026-10-11", Some(g)))
            .collect();
        let refs: Vec<&Asset> = members.iter().collect();
        let label = chapter_label(&itin(), day("2026-10-11"), &refs).unwrap();
        assert_eq!(label.place, "Osaka");
        // Mostly on board: the ship.
        let refs: Vec<&Asset> = vec![&members[0], &members[1], &members[2]];
        let label = chapter_label(&itin(), day("2026-10-11"), &refs).unwrap();
        assert_eq!(label.place, "Carnival Luminosa");
        // Seaside fixes the coarse land grid reads as water stay ashore on
        // a port day; the same water fix on a sea day is on board.
        let seaside = (34.58, 135.33);
        assert!(land_fraction(seaside) < 0.5, "fixture must read as water");
        let ashore = itin().label_for(day("2026-10-11"), Some(seaside)).unwrap();
        assert_eq!(ashore.place, "Osaka");
        let at_sea = itin().label_for(day("2026-10-10"), Some(seaside)).unwrap();
        assert_eq!(at_sea.place, "Carnival Luminosa");
        // No GPS anywhere: the no-fix rule (on board on a cruise day).
        let blind = photo("x", "2026-10-11", None);
        let label = chapter_label(&itin(), day("2026-10-11"), &[&blind]).unwrap();
        assert_eq!(label.place, "Carnival Luminosa");
    }
}
