//! The owner's real itinerary: stops (cities and ports with dates), legs
//! (flight, ship, ground) and the cruises between them.
//!
//! When given, it replaces GPS-clustered place naming and the photo-derived
//! route: chapters are labelled with the itinerary's city, or with the ship
//! name when the shot was taken on board, and the closing map travels the
//! itinerary (see [`crate::journey`]). Without one nothing here is used.

use crate::geo::{haversine_m, land_fraction};
use crate::types::{Asset, Chapter, Plan};
use chrono::NaiveDate;
use serde::Deserialize;

/// A shot this close to a docked ship's terminal counts as on board.
pub const DOCK_RADIUS_M: f64 = 1_500.0;

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
}

#[derive(Debug, Clone, Deserialize)]
pub struct Itinerary {
    pub stops: Vec<ItinStop>,
    pub legs: Vec<ItinLeg>,
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
    NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
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

    /// Stop by id.
    pub fn stop(&self, id: &str) -> Option<&ItinStop> {
        self.stops.iter().find(|s| s.id == id)
    }

    fn validate(&self) -> Result<(), String> {
        if self.stops.len() < 2 || self.legs.is_empty() {
            return Err("itinerary needs at least two stops and one leg".into());
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
    /// On a cruise day the shot is on board when it was taken at sea, within
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
        let on_board = match gps {
            None => true,
            Some(p) => {
                land_fraction(p) < 0.5
                    || self
                        .docks()
                        .iter()
                        .any(|d| haversine_m(p, d.pos()) <= DOCK_RADIUS_M)
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

/// Relabel a plan under the itinerary: each chapter takes the label of its
/// first day and medoid fix (city, or the ship when on board), and the
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
    let fixes: Vec<(f64, f64)> = members.iter().filter_map(|a| a.gps).collect();
    let gps = crate::plan::medoid(&fixes);
    match itin.label_for(day, gps) {
        Some(l) => {
            ch.title.clone_from(&l.place);
            ch.place = Some(l.place);
        }
        None => ch.place = None,
    }
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
        a.taken_at = Some(d.and_hms_opt(12, 0, 0).unwrap().and_utc().timestamp());
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
}
