//! Where photos were taken, worked out once for every keepsake. Chapter
//! names (the film's lower thirds, the book's day titles) come from
//! [`chapter_attraction`] and the itinerary relabel; each photo's own place
//! (book captions, reel overlays) from [`photo_places`]. The film, the
//! reels and the book read these and never name places themselves.

use crate::geo::attraction_within;
use crate::itinerary::{asset_day, Itinerary};
use crate::types::{Asset, Plan};
use std::collections::{HashMap, HashSet};

/// How close a photo must be to an attraction to be named for it.
/// Measured on the Japan to Singapore trip: shots at places visited sat a
/// median 35-440 m from the attraction's point (Kōtoku-in, DisneySea,
/// Universal Studios), while places merely walked past or near sat 650-
/// 1000 m away (Hamarikyu beside Tsukiji, Hibiya, Sensō-ji from the hotel).
pub const PLACE_RADIUS_M: f64 = 600.0;

/// Photos needed at an attraction before it names anything: one shot on
/// the walk to the station is not a visit.
pub const PLACE_MIN_SHOTS: usize = 3;

/// The attraction a photo was taken at, within [`PLACE_RADIUS_M`].
pub fn photo_attraction(a: &Asset) -> Option<String> {
    attraction_within(a.gps?, PLACE_RADIUS_M)
}

/// Each distinct item with its count, in the order first seen.
pub fn tally<K: PartialEq>(items: impl IntoIterator<Item = K>) -> Vec<(K, usize)> {
    let mut counts: Vec<(K, usize)> = vec![];
    for k in items {
        match counts.iter_mut().find(|(n, _)| *n == k) {
            Some((_, c)) => *c += 1,
            None => counts.push((k, 1)),
        }
    }
    counts
}

/// The most counted entry of a [`tally`]; ties go to the first seen
/// (`max_by_key` keeps the last maximum, so it runs reversed).
pub fn most_counted<K>(counts: Vec<(K, usize)>) -> Option<(K, usize)> {
    counts.into_iter().rev().max_by_key(|(_, c)| *c)
}

/// The first attraction `fixes` (in story order) visited: the earliest
/// reached with at least `min_shots` taken there. A chapter's name opens
/// its day, so it follows the day: Tsurugaoka Hachimangū in the morning
/// names Day 5 even though more shots came later at Kōtoku-in, whose name
/// then captions its own pages. `hidden` names the owner never wants.
pub fn visited_attraction(
    fixes: &[(f64, f64)],
    min_shots: usize,
    hidden: impl Fn(&str) -> bool,
) -> Option<String> {
    let names = fixes
        .iter()
        .filter_map(|&f| attraction_within(f, PLACE_RADIUS_M))
        .filter(|n| !hidden(n));
    tally(names)
        .into_iter()
        .find(|(_, c)| *c >= min_shots.max(1))
        .map(|(n, _)| n)
}

/// Distinct days an attraction must be visited on to count as a home base.
pub const HOME_BASE_DAYS: usize = 3;

/// Attractions the trip keeps coming back to: [`PLACE_MIN_SHOTS`] or more
/// photos within [`PLACE_RADIUS_M`] on [`HOME_BASE_DAYS`] or more days.
/// That is the hotel's neighbourhood (Masjid Omar beside the Singapore
/// hotel, every morning), not a sight, so it never names a chapter or a
/// photo. A sight visited once in depth (Sensō-ji: 22 photos on one day,
/// a stray shot on two others) stays.
pub fn home_bases(assets: &[Asset]) -> HashSet<String> {
    let poi: Vec<Option<String>> = assets.iter().map(photo_attraction).collect();
    home_bases_of(&day_visits(assets.iter().zip(&poi)))
}

/// Photos per (day, attraction), from each asset paired with its
/// [`photo_attraction`].
fn day_visits<'a>(
    shots: impl IntoIterator<Item = (&'a Asset, &'a Option<String>)>,
) -> HashMap<(jiff::civil::Date, &'a str), usize> {
    let mut visits = HashMap::new();
    for (a, p) in shots {
        if let (Some(d), Some(p)) = (asset_day(a), p) {
            *visits.entry((d, p.as_str())).or_default() += 1;
        }
    }
    visits
}

/// [`home_bases`] from a [`day_visits`] tally.
fn home_bases_of(visits: &HashMap<(jiff::civil::Date, &str), usize>) -> HashSet<String> {
    let mut days: HashMap<&str, usize> = HashMap::new();
    for (&(_, poi), &n) in visits {
        if n >= PLACE_MIN_SHOTS {
            *days.entry(poi).or_default() += 1;
        }
    }
    days.into_iter()
        .filter(|&(_, d)| d >= HOME_BASE_DAYS)
        .map(|(poi, _)| poi.to_string())
        .collect()
}

/// Where the itinerary alone puts a photo, before any attraction.
#[derive(Debug, Clone, PartialEq)]
pub enum Spot<'a> {
    /// A place the owner named.
    Named(&'a str),
    /// On board this ship (even at a dock beside Miraikan).
    Aboard(String),
    /// Ashore, or no itinerary.
    Ashore,
}

/// A photo's [`Spot`]: a named place wins over the ship, as the owner was there.
pub fn spot<'a>(itin: Option<&'a Itinerary>, a: &Asset) -> Spot<'a> {
    let Some(itin) = itin else {
        return Spot::Ashore;
    };
    if let Some(name) = itin.place_of(a) {
        return Spot::Named(name);
    }
    asset_day(a)
        .and_then(|d| itin.label_for(d, a.gps))
        .filter(|l| l.on_ship)
        .map_or(Spot::Ashore, |l| Spot::Aboard(l.place))
}

/// A chapter's attraction for its name when the plan has no itinerary:
/// the one its photos visited (a single-photo chapter needs only that
/// shot there), never one of the trip's [`home_bases`].
pub fn chapter_attraction(fixes: &[(f64, f64)], home: &HashSet<String>) -> Option<String> {
    visited_attraction(fixes, PLACE_MIN_SHOTS.min(fixes.len()), |n| {
        home.contains(n)
    })
}

/// Each planned photo's place, by asset id: a place the owner named in
/// the itinerary; the ship for shots the itinerary puts on board (even at
/// a dock beside Miraikan or the Osaka Aquarium); else the attraction it
/// was taken at on a day with [`PLACE_MIN_SHOTS`] there; else its
/// chapter's place (its city when that place is a sight elsewhere),
/// unless that is a name the trip hides (a home base or
/// an itinerary `hide_attractions` entry). Photos with none are absent.
pub fn photo_places(
    plan: &Plan,
    assets: &[Asset],
    itin: Option<&Itinerary>,
) -> HashMap<String, String> {
    let chapter_place: HashMap<&str, &str> = plan
        .chapters
        .iter()
        .filter_map(|c| c.place.as_deref().map(|p| (c, p)))
        .flat_map(|(c, p)| c.asset_ids.iter().map(move |id| (id.as_str(), p)))
        .collect();
    let in_plan: HashSet<&str> = plan
        .chapters
        .iter()
        .flat_map(|c| c.asset_ids.iter().map(String::as_str))
        .collect();
    // One gazetteer lookup per asset serves both the trip's home bases and
    // the planned photos' visits.
    let poi: Vec<Option<String>> = assets.iter().map(photo_attraction).collect();
    let home = home_bases_of(&day_visits(assets.iter().zip(&poi)));
    let hidden = |n: &str| home.contains(n) || itin.is_some_and(|i| i.hides_attraction(n));
    // Videos too: a reel can be a clip, and it names its place like a photo.
    let planned: Vec<(&Asset, &Option<String>)> = assets
        .iter()
        .zip(&poi)
        .filter(|(a, _)| in_plan.is_empty() || in_plan.contains(a.id.as_str()))
        .collect();
    let visits = day_visits(planned.iter().copied());
    let sights: HashSet<&str> = poi.iter().flatten().map(String::as_str).collect();
    let city_of = |a: &Asset| {
        let stop = itin.and_then(|i| i.label_for(asset_day(a)?, a.gps));
        stop.filter(|l| !l.on_ship)
            .map(|l| l.place)
            .or_else(|| crate::geo::city_name(a.gps?))
    };
    let mut out = HashMap::new();
    for (a, poi) in planned {
        let own = match spot(itin, a) {
            Spot::Named(name) => Some(name.to_string()),
            Spot::Aboard(ship) => Some(ship),
            Spot::Ashore => poi
                .as_deref()
                .filter(|&p| {
                    !hidden(p)
                        && asset_day(a)
                            .and_then(|d| visits.get(&(d, p)))
                            .is_some_and(|&n| n >= PLACE_MIN_SHOTS)
                })
                .map(String::from),
        };
        // The chapter's place, unless that is a sight this photo was not
        // taken at (the day's first visit, Osaka Castle Park, must not
        // caption the evening in Dōtonbori): then the photo's own city.
        let fallback = || {
            let p = *chapter_place.get(a.id.as_str()).filter(|&&p| !hidden(p))?;
            if !sights.contains(p) || poi.as_deref() == Some(p) {
                return Some(p.to_string());
            }
            Some(city_of(a).unwrap_or_else(|| p.to_string()))
        };
        if let Some(p) = own.or_else(fallback) {
            out.insert(a.id.clone(), p);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AssetKind;
    use crate::types::Chapter;

    fn photo(id: &str, noon_utc: i64, gps: Option<(f64, f64)>) -> Asset {
        let mut a = Asset::new(id, format!("{id}.jpg").into(), AssetKind::Photo);
        a.taken_at = Some(noon_utc);
        a.gps = gps;
        a
    }

    fn plan_of(chapters: Vec<(&str, Vec<&str>)>) -> Plan {
        let mut plan: Plan = serde_json::from_str(
            r#"{"plan_version":1,"schema_version":1,"hero":[],"reel_selects":[],"chapters":[],"captions":{},"map_polyline":[],"music":{"track":"","rationale":""}}"#,
        )
        .unwrap();
        plan.chapters = chapters
            .into_iter()
            .map(|(place, ids)| Chapter {
                title: place.into(),
                asset_ids: ids.into_iter().map(String::from).collect(),
                place: Some(place.into()),
                day_index: None,
                date_label: None,
            })
            .collect();
        plan
    }

    #[test]
    fn the_most_counted_ties_to_the_first_seen() {
        let t = tally(["b", "a", "b", "a", "c"]);
        assert_eq!(t, [("b", 2), ("a", 2), ("c", 1)]);
        assert_eq!(most_counted(t), Some(("b", 2)));
        assert_eq!(most_counted(tally(Vec::<&str>::new())), None);
    }

    #[test]
    fn the_hotel_neighbourhood_is_a_home_base_not_a_sight() {
        let day = |n: i64| 1_791_288_000 + n * 86_400;
        let masjid = (1.2856, 103.8475); // near the Singapore hotel
        let shots = |d: i64, n: usize, gps| {
            (0..n).map(move |i| photo(&format!("{d}-{i}"), day(d) + i as i64, Some(gps)))
        };
        let seen = |days: &[i64]| -> Vec<Asset> {
            days.iter().flat_map(|&d| shots(d, 3, masjid)).collect()
        };
        let name = attraction_within(masjid, PLACE_RADIUS_M).expect("an attraction near the hotel");
        assert!(home_bases(&seen(&[0, 1, 2])).contains(&name));
        assert!(
            !home_bases(&seen(&[0, 1])).contains(&name),
            "two days is a visit"
        );
        let plan = plan_of(vec![]);
        assert!(
            photo_places(&plan, &seen(&[0, 1, 2]), None).is_empty(),
            "a home base captions no photo"
        );
        assert_eq!(photo_places(&plan, &seen(&[0, 1]), None).len(), 6);
        // Nor does a chapter named for it give it back as the fallback.
        let assets = seen(&[0, 1, 2]);
        let ids: Vec<&str> = assets.iter().map(|a| a.id.as_str()).collect();
        let named = plan_of(vec![(name.as_str(), ids)]);
        assert!(photo_places(&named, &assets, None).is_empty());
    }

    #[test]
    fn a_visit_needs_several_shots_within_reach() {
        let disneysea = (35.6267, 139.8881);
        // Day 5: the shrine in the morning, then many more at the Buddha.
        let (shrine, buddha) = ((35.3260, 139.5565), (35.3167, 139.5359));
        let day: Vec<(f64, f64)> = [shrine; 3].into_iter().chain([buddha; 10]).collect();
        assert_eq!(
            visited_attraction(&day, 3, |_| false).as_deref(),
            Some("Tsurugaoka Hachimangū"),
            "the first visit names the chapter"
        );
        assert_eq!(
            visited_attraction(&[disneysea; 3], 3, |_| false).as_deref(),
            Some("Tokyo DisneySea")
        );
        // One shot is passing by; a hidden name never counts.
        assert_eq!(visited_attraction(&[disneysea], 3, |_| false), None);
        assert_eq!(
            visited_attraction(&[disneysea; 3], 3, |n| n == "Tokyo DisneySea"),
            None
        );
        // A Day 5 shot 749 m from the National Museum of Western Art.
        assert_eq!(
            attraction_within((35.71139, 139.78256), PLACE_RADIUS_M),
            None
        );
        // Single-photo chapters name the place that photo was taken at.
        assert_eq!(
            chapter_attraction(&[disneysea], &HashSet::new()).as_deref(),
            Some("Tokyo DisneySea")
        );
    }

    #[test]
    fn a_day_named_for_a_sight_lends_it_only_to_shots_taken_there() {
        let oct11 = 1_791_720_000;
        let castle = (34.6873, 135.5262); // Osaka Castle
        let dotonbori = (34.6687, 135.5013);
        let assets: Vec<Asset> = (0..3)
            .map(|i| photo(&format!("c{i}"), oct11 + i, Some(castle)))
            .chain([photo("d", oct11 + 9, Some(dotonbori))])
            .collect();
        let sight = attraction_within(castle, PLACE_RADIUS_M).expect("a castle sight");
        let plan = plan_of(vec![(sight.as_str(), vec!["c0", "c1", "c2", "d"])]);
        let places = photo_places(&plan, &assets, None);
        assert_eq!(places["c0"], sight);
        assert_ne!(places["d"], sight, "the evening elsewhere takes its city");
        assert!(places["d"].contains("Osaka"), "{}", places["d"]);
    }

    #[test]
    fn photo_places_prefer_visits_then_the_chapter() {
        let oct6 = 1_791_288_000; // noon UTC
        let disneysea = (35.6267, 139.8881);
        let assets: Vec<Asset> = (0..4)
            .map(|i| photo(&format!("d{i}"), oct6 + i, Some(disneysea)))
            .chain([photo("x", oct6, None)])
            .collect();
        let plan = plan_of(vec![("Tokyo", vec!["d0", "d1", "d2", "d3", "x"])]);
        let places = photo_places(&plan, &assets, None);
        assert_eq!(places["d0"], "Tokyo DisneySea");
        assert_eq!(places["x"], "Tokyo");
        // One DisneySea shot that day is not a visit.
        let plan = plan_of(vec![("Tokyo", vec!["d0", "x"])]);
        let places = photo_places(
            &plan,
            &assets[..1]
                .iter()
                .chain([&assets[4]])
                .cloned()
                .collect::<Vec<_>>(),
            None,
        );
        assert_eq!(places["d0"], "Tokyo");
    }

    #[test]
    fn shots_on_board_name_the_ship_not_the_terminal_sights() {
        let itin = Itinerary::from_json(
            r#"{
              "stops": [
                { "id": "tokyo", "label": "Tokyo", "lat": 35.71, "lon": 139.80, "arrive": "2026-10-04", "depart": "2026-10-09" },
                { "id": "port", "label": "Tokyo", "lat": 35.6177, "lon": 139.771, "arrive": "2026-10-09", "depart": "2026-10-09", "kind": "port" },
                { "id": "osaka", "label": "Osaka", "lat": 34.656, "lon": 135.4306, "arrive": "2026-10-11", "depart": "2026-10-11", "kind": "port" }
              ],
              "legs": [
                { "from": "tokyo", "to": "port", "mode": "ground" },
                { "from": "port", "to": "osaka", "mode": "ship", "ship": "Carnival Luminosa" }
              ]
            }"#,
        )
        .unwrap();
        let oct9 = 1_791_547_200;
        // Miraikan faces the Aomi terminal: on board by the dock rule.
        let assets: Vec<Asset> = (0..4)
            .map(|i| photo(&format!("m{i}"), oct9 + i, Some((35.6190, 139.7766))))
            .collect();
        let plan = plan_of(vec![(
            "Carnival Luminosa Embarkation",
            vec!["m0", "m1", "m2", "m3"],
        )]);
        let places = photo_places(&plan, &assets, Some(&itin));
        assert_eq!(places["m0"], "Carnival Luminosa");
        // A reel clip at the dock names its place like a photo.
        let mut clip = photo("v", oct9, Some((35.6190, 139.7766)));
        clip.kind = AssetKind::Video;
        let plan = plan_of(vec![("Carnival Luminosa Embarkation", vec!["v"])]);
        assert_eq!(
            photo_places(&plan, &[clip], Some(&itin))["v"],
            "Carnival Luminosa"
        );
        assert_eq!(
            spot(Some(&itin), &assets[0]),
            Spot::Aboard("Carnival Luminosa".into())
        );
        assert_eq!(spot(None, &assets[0]), Spot::Ashore);
    }
}
