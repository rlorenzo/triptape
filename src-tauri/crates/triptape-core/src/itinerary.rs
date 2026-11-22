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

/// [`PORT_SHORE_RADIUS_M`] for a city a cruise starts or ends in: enough
/// for Singapore's Marina Bay (reclaimed land the land grid reads as
/// water, 2-3 km from the terminal), not for deck shots of the strait.
pub const CITY_SHORE_RADIUS_M: f64 = 5_000.0;

/// What a cruise's first and last days are named for
/// (`Carnival Luminosa Embarkation`).
const BOARDING_SUFFIXES: [&str; 2] = [" Embarkation", " Disembarkation"];

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
    /// Artwork for the stop on the parchment travel maps (a PNG or JPEG;
    /// a path relative to the itinerary file is resolved on load).
    #[serde(default)]
    pub sticker: Option<String>,
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

impl ItinLeg {
    /// The leg's `dates`, when given and well-formed.
    pub fn date_range(&self) -> Option<(NaiveDate, NaiveDate)> {
        self.dates.as_deref().and_then(|d| parse_range(d).ok())
    }
}

/// How far from a stop a day's photos may be and still be in its city.
pub const STOP_NEAR_M: f64 = 30_000.0;

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
    #[serde(default, deserialize_with = "local_minute")]
    pub from: Option<jiff::civil::DateTime>,
    #[serde(default, deserialize_with = "local_minute")]
    pub until: Option<jiff::civil::DateTime>,
}

/// A `YYYY-MM-DD HH:MM` local time, parsed once when the itinerary loads.
fn local_minute<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<jiff::civil::DateTime>, D::Error> {
    let Some(s) = Option::<String>::deserialize(d)? else {
        return Ok(None);
    };
    jiff::civil::DateTime::strptime("%Y-%m-%d %H:%M", s.trim())
        .map(Some)
        .map_err(|_| serde::de::Error::custom(format!("{s:?} is not \"YYYY-MM-DD HH:MM\"")))
}

impl NamedPlace {
    /// The local time window, when both ends are given.
    fn window(&self) -> Option<(jiff::civil::DateTime, jiff::civil::DateTime)> {
        self.from.zip(self.until)
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
        let mut it = Self::from_json(&text)?;
        // Sticker paths are written relative to the itinerary file.
        if let Some(dir) = path.parent() {
            for stop in &mut it.stops {
                if let Some(s) = &mut stop.sticker {
                    *s = dir.join(&*s).to_string_lossy().into_owned();
                }
            }
        }
        Ok(it)
    }

    /// The owner-named place a shot was taken in: one whose time window
    /// holds its local time, else the nearest whose radius reaches its GPS.
    /// A shot without a time zone has no local time to place it by.
    pub fn place_of(&self, a: &Asset) -> Option<&str> {
        let when = a
            .taken_at
            .zip(a.tz_offset_secs)
            .and_then(|(t, tz)| crate::metadata::local_datetime(t, Some(tz)));
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
            if pl.name.trim().is_empty() {
                return Err("a place needs a name".into());
            }
            if pl.lat.is_some_and(|v| !(-90.0..=90.0).contains(&v))
                || pl.lon.is_some_and(|v| !(-180.0..=180.0).contains(&v))
            {
                return Err(format!("place {:?} has lat/lon out of range", pl.name));
            }
            if !(pl.radius_m.is_finite() && pl.radius_m > 0.0) {
                return Err(format!("place {:?} needs a radius_m above 0", pl.name));
            }
            let timed = pl.from.is_some() || pl.until.is_some();
            if timed && pl.window().is_none() {
                return Err(format!("place {:?} needs both from and until", pl.name));
            }
            if pl.window().is_some_and(|(from, until)| until < from) {
                return Err(format!("place {:?} has until before from", pl.name));
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
            let range = leg.date_range().or_else(|| {
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
    /// not within [`PORT_SHORE_RADIUS_M`] of the day's port call, or
    /// [`CITY_SHORE_RADIUS_M`] of a city the cruise starts or ends in),
    /// within [`DOCK_RADIUS_M`] of a dock, or has no GPS on a day without a
    /// port call (on a port day most shots are ashore); otherwise it is ashore and
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
        let port_call = port.is_some_and(|s| s.kind == StopKind::Port);
        let on_board = match gps {
            // No fix on a cruise day: the phone was aboard (in airplane
            // mode at sea, or on the ship's wifi in port). A shot ashore
            // without GPS borrows a nearby shot's fix before it gets here
            // (`places::borrowed_fixes`).
            None => true,
            Some(p) => {
                let at_dock = self
                    .docks()
                    .iter()
                    .any(|d| haversine_m(p, d.pos()) <= DOCK_RADIUS_M);
                // The land grid is ~5 km cells, so a seaside town reads as
                // water. On a port day the ship is tied up at the dock, so
                // a fix near the port but away from the dock is ashore.
                let shore_m = if port_call {
                    PORT_SHORE_RADIUS_M
                } else {
                    CITY_SHORE_RADIUS_M
                };
                let ashore_near_port = port.is_some_and(|s| haversine_m(p, s.pos()) <= shore_m);
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
    /// The stop the trip is at on `day` (a city or a port call, not home),
    /// by its label, unless hidden, or when `at` (where the day's photos
    /// were) lies beyond [`STOP_NEAR_M`] of it: a day trip out of the city.
    pub fn stop_on(&self, day: NaiveDate, at: Option<(f64, f64)>) -> Option<String> {
        self.stops
            .iter()
            .find(|s| s.kind != StopKind::Home && !s.hide_label && s.active_on(day))
            .filter(|s| {
                at.is_none_or(|p| crate::geo::haversine_m(p, (s.lat, s.lon)) <= STOP_NEAR_M)
            })
            .map(|s| s.label.clone())
    }

    /// The port the ship calls at on `day`, by its label, if any (and
    /// not hidden): the book names it beside the ship on a port day.
    pub fn port_call(&self, day: NaiveDate) -> Option<String> {
        self.stops
            .iter()
            .find(|s| s.kind == StopKind::Port && !s.hide_label && s.active_on(day))
            .map(|s| s.label.clone())
    }

    pub fn boarding(&self, day: NaiveDate) -> Option<String> {
        self.cruises().into_iter().find_map(|c| {
            let [on, off] = BOARDING_SUFFIXES;
            if c.start == day {
                Some(format!("{}{on}", c.ship))
            } else if c.end == day {
                Some(format!("{}{off}", c.ship))
            } else {
                None
            }
        })
    }

    /// First and last day of the trip: the earliest and latest dates of
    /// any stop or dated leg (a cruise may give only its leg's `dates`).
    /// `None` when nothing carries a date.
    pub fn span(&self) -> Option<(NaiveDate, NaiveDate)> {
        let stops = self.stops.iter().filter_map(ItinStop::window);
        let legs = self.legs.iter().filter_map(ItinLeg::date_range);
        stops.chain(legs).fold(None, |acc, (a, d)| {
            Some(match acc {
                None => (a, d),
                Some((s, e)) => (s.min(a), e.max(d)),
            })
        })
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

/// Whether `place` is one of `ships` or a cruise's boarding day named
/// for one (`Carnival Luminosa Embarkation`): where the owner slept, not a
/// place they went.
pub fn is_ship_place(ships: &[String], place: &str) -> bool {
    ships.iter().any(|ship| {
        place
            .strip_prefix(ship.as_str())
            .is_some_and(|rest| rest.is_empty() || BOARDING_SUFFIXES.contains(&rest))
    })
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
    let home = crate::places::home_bases(assets);
    let mut out = plan.clone();
    for ch in &mut out.chapters {
        relabel_chapter(ch, &by_id, itin, &home);
    }
    out.map_polyline = itin.polyline();
    out
}

/// Name what the film shows: for each chapter, [`chapter_name`] over only
/// its shots in `plan.hero`, kept in [`Chapter::film_place`] when it
/// differs from the chapter's own place. A chapter often spans more than
/// the film keeps (Osaka Castle in the morning, Dotonbori in the
/// afternoon, with only Dotonbori in the film), and its lower third should
/// name what is on screen. Run after the user's edits, on the final hero;
/// chapters `renamed` by the user keep their name. A visit needs fewer
/// shots here, as the film keeps only a few of each place.
pub fn name_film_chapters(
    plan: &mut Plan,
    assets: &[Asset],
    itin: &Itinerary,
    renamed: impl Fn(&Chapter) -> bool,
) {
    let by_id: std::collections::HashMap<&str, &Asset> =
        assets.iter().map(|a| (a.id.as_str(), a)).collect();
    // Film order, each asset once (a multi-window clip has several
    // entries): the user's moves reorder the hero, not the chapter.
    let mut seen = std::collections::HashSet::new();
    let film_order: Vec<String> = plan
        .hero
        .iter()
        .map(|e| e.asset_id.clone())
        .filter(|id| seen.insert(id.clone()))
        .collect();
    let home = crate::places::home_bases(assets);
    for ch in &mut plan.chapters {
        ch.film_place = None;
        if renamed(ch) {
            continue;
        }
        let members: Vec<&Asset> = ch
            .asset_ids
            .iter()
            .filter_map(|id| by_id.get(id.as_str()).copied())
            .collect();
        let in_chapter: std::collections::HashSet<&str> =
            ch.asset_ids.iter().map(String::as_str).collect();
        let on_screen: Vec<&Asset> = film_order
            .iter()
            .filter(|id| in_chapter.contains(id.as_str()))
            .filter_map(|id| by_id.get(id.as_str()).copied())
            .collect();
        if on_screen.is_empty() {
            continue;
        }
        let Some(day) = members.iter().filter_map(|a| asset_day(a)).min() else {
            continue;
        };
        let Some(label) = chapter_label(itin, day, &on_screen) else {
            continue;
        };
        let min_shots = crate::places::PLACE_MIN_SHOTS.min(on_screen.len().div_ceil(2));
        let name = chapter_name(itin, day, &on_screen, label, &home, min_shots);
        if ch.place.as_deref() != Some(name.as_str()) {
            ch.film_place = Some(name);
        }
    }
}

/// The label most of a chapter's GPS fixes get on `day` (ties: the label
/// reached first in story order). One medoid fix is not enough on a port
/// day: a few deck or tender shots near the dock could pull it on board
/// and title a whole day ashore in Jeju with the ship's name. Shots
/// without GPS cast no vote; a chapter with none takes the no-fix label.
fn chapter_label(itin: &Itinerary, day: NaiveDate, members: &[&Asset]) -> Option<DayLabel> {
    let votes = crate::places::tally(
        members
            .iter()
            .filter_map(|a| itin.label_for(day, Some(a.gps?))),
    );
    if votes.is_empty() {
        return itin.label_for(day, None);
    }
    crate::places::most_counted(votes).map(|(l, _)| l)
}

fn relabel_chapter(
    ch: &mut Chapter,
    by_id: &std::collections::HashMap<&str, &Asset>,
    itin: &Itinerary,
    home: &std::collections::HashSet<String>,
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
    let place = chapter_name(
        itin,
        day,
        &members,
        label,
        home,
        crate::places::PLACE_MIN_SHOTS,
    );
    ch.title.clone_from(&place);
    ch.place = Some(place);
}

/// A relabelled chapter's name, shared by every keepsake: a place the
/// owner named when most of its photos are there; on board, the boarding
/// (`Carnival Luminosa Embarkation`) on a cruise's first or last day, else
/// the ship (on a port day too); ashore, the attraction its
/// photos visited (see [`crate::places`]), else the stop's city.
fn chapter_name(
    itin: &Itinerary,
    day: NaiveDate,
    members: &[&Asset],
    label: DayLabel,
    home: &std::collections::HashSet<String>,
    min_shots: usize,
) -> String {
    let named = crate::places::tally(members.iter().filter_map(|a| itin.place_of(a)));
    // A chapter spanning several named places (Mount Faber, then the
    // Oceanarium) is named for the first one visited, as a day is named
    // for its first sight.
    let mut visited = named
        .iter()
        .filter(|(_, c)| *c >= min_shots.max(1))
        .map(|(n, _)| *n);
    if let (Some(first), Some(_)) = (visited.next(), visited.next()) {
        return first.to_string();
    }
    if let Some((name, _)) =
        crate::places::most_counted(named).filter(|(_, c)| 2 * c >= members.len())
    {
        return name.to_string();
    }
    if label.on_ship {
        // Shots aboard on a port day (the lounge, the cabin, the party
        // that stayed on the ship) are the ship's, not the port's.
        return itin.boarding(day).unwrap_or(label.place);
    }
    // Shots inside a place the owner named are that place, not the
    // built-in landmark next door (the Oceanarium is not Universal
    // Studios).
    let fixes: Vec<(f64, f64)> = members
        .iter()
        .filter(|a| itin.place_of(a).is_none())
        .filter_map(|a| a.gps)
        .collect();
    // Neither a hidden name nor the hotel's neighbourhood (a home base).
    crate::places::visited_attraction(&fixes, min_shots, |n| {
        itin.hides_attraction(n) || home.contains(n)
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
        // No GPS on a cruise day, port day included: the ship (a shot ashore
        // borrows a nearby shot's fix before it gets here).
        let unfixed = it.label_for(day("2026-10-11"), None).unwrap();
        assert_eq!(
            (unfixed.place.as_str(), unfixed.on_ship),
            ("Carnival Luminosa", true)
        );
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
        // Sailing day from Singapore: Marina Bay, read as water, is ashore;
        // the strait 12 km out is on deck, not 30 km of "near the port".
        let bay = it
            .label_for(day("2026-10-26"), Some((1.2834, 103.8607)))
            .unwrap();
        assert!(!bay.on_ship, "{bay:?}");
        assert!(
            it.label_for(day("2026-10-26"), Some((1.22, 103.97)))
                .unwrap()
                .on_ship
        );
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
            cover: Default::default(),
            crop: Default::default(),
            book: Default::default(),
            closing: Default::default(),
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
    fn the_film_names_the_shots_it_shows() {
        let mut it = itin();
        it.places = serde_json::from_str(
            r#"[
              { "name": "Taipei 101", "lat": 25.0340, "lon": 121.5645, "radius_m": 250 },
              { "name": "Raohe Night Market", "lat": 25.0509, "lon": 121.5775 }
            ]"#,
        )
        .unwrap();
        let tower = Some((25.0343, 121.5648));
        let market = Some((25.0510, 121.5770));
        let assets = vec![
            photo("t1", "2026-10-16", tower),
            photo("t2", "2026-10-16", tower),
            photo("t3", "2026-10-16", tower),
            photo("m1", "2026-10-16", market),
            photo("m2", "2026-10-16", market),
        ];
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let chapter = |members: &[&str]| Chapter {
            title: "Taipei 101".into(),
            asset_ids: ids(members),
            place: Some("Taipei 101".into()),
            day_index: Some(14),
            date_label: None,
            film_place: None,
        };
        let entry = |id: &str| {
            serde_json::json!({
                "asset_id": id, "content_hash": null, "size_bytes": 0, "mtime": 0,
                "rendition": "still"
            })
        };
        let mut plan: Plan = serde_json::from_value(serde_json::json!({
            "plan_version": 1, "schema_version": 1, "reel_selects": [], "chapters": [],
            "captions": {}, "map_polyline": [], "music": {"track": "", "rationale": ""},
            "hero": [entry("m1"), entry("m2"), entry("t1")],
        }))
        .unwrap();
        plan.chapters = vec![
            chapter(&["t1", "t2", "t3", "m1", "m2"]),
            chapter(&["t1", "t2", "t3"]),
        ];
        name_film_chapters(&mut plan, &assets, &it, |_| false);
        // The film shows the night market first: that is the lower third.
        assert_eq!(
            plan.chapters[0].film_place.as_deref(),
            Some("Raohe Night Market")
        );
        // Shown where the chapter is named: nothing to add.
        assert_eq!(plan.chapters[1].film_place, None);
        // A chapter the user renamed keeps their name.
        name_film_chapters(&mut plan, &assets, &it, |c| c.asset_ids.len() == 5);
        assert_eq!(plan.chapters[0].film_place, None);
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
        // In the window (noon on Oct 23), whatever the GPS says; a shot
        // with no time zone has no local time to match it by.
        let mut dome = photo("d", "2026-10-23", Some((25.0343, 121.5648)));
        dome.tz_offset_secs = Some(0);
        assert_eq!(it.place_of(&dome), Some("Flower Dome"));
        dome.gps = None;
        dome.tz_offset_secs = None;
        assert_eq!(it.place_of(&dome), None);
        // A malformed window is refused when loading.
        let bad = JSON.replacen(
            "\"legs\"",
            r#""places": [{ "name": "X", "from": "Oct 23", "until": "2026-10-23 12:30" }], "legs""#,
            1,
        );
        assert!(Itinerary::from_json(&bad).is_err());
        // So is a window that ends before it starts.
        let reversed = JSON.replacen(
            "\"legs\"",
            r#""places": [{ "name": "X", "from": "2026-10-23 12:30", "until": "2026-10-23 11:30" }], "legs""#,
            1,
        );
        assert!(Itinerary::from_json(&reversed).is_err());
        // Typos are refused too: a blank name, a latitude of 400, a
        // non-positive radius.
        for bad_place in [
            r#"{ "name": " ", "lat": 1.0, "lon": 1.0 }"#,
            r#"{ "name": "X", "lat": 400.0, "lon": 1.0 }"#,
            r#"{ "name": "X", "lat": 1.0, "lon": 1.0, "radius_m": -5 }"#,
        ] {
            let json = JSON.replacen(
                "\"legs\"",
                &format!(r#""places": [{bad_place}], "legs""#),
                1,
            );
            assert!(Itinerary::from_json(&json).is_err(), "{bad_place}");
        }
        // So is a window with only one end.
        let half = JSON.replacen(
            "\"legs\"",
            r#""places": [{ "name": "X", "from": "2026-10-23 11:30" }], "legs""#,
            1,
        );
        assert!(Itinerary::from_json(&half)
            .unwrap_err()
            .contains("needs both from and until"));
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
            chapter_name(it, day(date), &refs, label, &Default::default(), 3)
        };
        // On board on the cruise's first day; on a port day, shots aboard
        // (the party that stayed on the ship) are the ship's.
        assert_eq!(
            name(&it, "2026-10-09", (35.6190, 139.7766), 3),
            "Carnival Luminosa Embarkation"
        );
        assert_eq!(
            name(&it, "2026-10-11", (34.656, 135.4306), 3),
            "Carnival Luminosa"
        );
        // At sea: the ship.
        assert_eq!(
            name(&it, "2026-10-10", (30.0, 140.0), 3),
            "Carnival Luminosa"
        );
        // Ashore: a named place, then a visited attraction, then the city.
        assert_eq!(name(&it, "2026-10-11", (34.6687, 135.5013), 3), "Dotonbori");
        // Two named places in one chapter: the first one visited names it.
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
        assert_eq!(
            chapter_name(&it, day("2026-10-11"), &refs, label, &Default::default(), 3),
            "Dotonbori"
        );
        assert_eq!(
            name(&it, "2026-10-06", (35.6267, 139.8881), 3),
            "Tokyo DisneySea"
        );
        assert_eq!(name(&it, "2026-10-06", (35.6267, 139.8881), 2), "Tokyo");
        it.hide_attractions = vec!["Tokyo DisneySea".into()];
        assert_eq!(name(&it, "2026-10-06", (35.6267, 139.8881), 3), "Tokyo");
    }

    #[test]
    fn ships_and_their_boarding_days_are_not_places_visited() {
        let ships = ["Carnival Luminosa".to_string()];
        for p in [
            "Carnival Luminosa",
            "Carnival Luminosa Embarkation",
            "Carnival Luminosa Disembarkation",
        ] {
            assert!(is_ship_place(&ships, p), "{p}");
        }
        for p in ["Osaka", "Carnival Luminosa Deck 9", "Carnival"] {
            assert!(!is_ship_place(&ships, p), "{p}");
        }
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
        // No GPS anywhere: the no-fix rule (on board on any cruise day).
        let blind = photo("x", "2026-10-11", None);
        let label = chapter_label(&itin(), day("2026-10-11"), &[&blind]).unwrap();
        assert_eq!(label.place, "Carnival Luminosa");
        let blind = photo("x", "2026-10-10", None);
        let label = chapter_label(&itin(), day("2026-10-10"), &[&blind]).unwrap();
        assert_eq!(label.place, "Carnival Luminosa");
    }
}
