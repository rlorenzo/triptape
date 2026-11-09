//! Chapter stops for the animated closing route recap, and the recap's
//! duration model. The renderer (`triptape-media` `routeanim`) and the plan's
//! film-length estimate both read the constants and functions here, so the
//! budget cannot drift from what is rendered.

use crate::geo::haversine_m;
use crate::plan::medoid;
use crate::types::{Asset, Chapter};
use std::collections::HashMap;

/// Hold of the static closing map card, used when the animation has fewer
/// than two stops to travel between (or fails): the opening map plus 1.5 s.
pub const STATIC_CLOSING_MAP_SECS: f64 = 4.5;
/// Most stops the animation visits; extra ones are thinned out (smallest
/// detour first) so a long trip does not run past [`ROUTE_ANIM_MAX_SECS`].
pub const ROUTE_ANIM_MAX_STOPS: usize = 16;
/// Shortest and longest animated recap.
pub const ROUTE_ANIM_MIN_SECS: f64 = 6.0;
pub const ROUTE_ANIM_MAX_SECS: f64 = 14.0;
/// Screen time budgeted per stop: its pause plus an average leg.
pub const ROUTE_ANIM_SECS_PER_STOP: f64 = 0.9;
/// Fixed lead-in, so two stops still get room to travel.
pub const ROUTE_ANIM_BASE_SECS: f64 = 3.0;

/// One stop of the recap: a chapter's position, as an index into the plan's
/// `map_polyline`, and its place label.
///
/// The position is a polyline point rather than raw GPS on purpose: the
/// polyline is already outlier-filtered and home-fuzzed for export, so a
/// stop can never reveal more than the drawn route does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteStop {
    pub route_index: usize,
    /// The chapter's place (including user place overrides), if it has one.
    pub label: Option<String>,
    /// The chapter has a place name only from a user override (a ship's
    /// name for days at sea): GPS could not name where it is. The recap
    /// draws these as a dashed sea leg, not a stop.
    pub at_sea: bool,
}

/// Number of real stops (not sea days) the recap pauses at; the duration
/// budget is built from this.
pub fn animated_stop_count(stops: &[RouteStop]) -> usize {
    stops.iter().filter(|s| !s.at_sea).count()
}

/// Length of the animated closing map for `stop_count` chapter stops.
/// Fewer than two stops cannot travel anywhere and keep the static card.
pub fn closing_map_secs(stop_count: usize) -> f64 {
    if stop_count < 2 {
        return STATIC_CLOSING_MAP_SECS;
    }
    let n = stop_count.min(ROUTE_ANIM_MAX_STOPS) as f64;
    (ROUTE_ANIM_BASE_SECS + ROUTE_ANIM_SECS_PER_STOP * n)
        .clamp(ROUTE_ANIM_MIN_SECS, ROUTE_ANIM_MAX_SECS)
}

/// Stops in chapter order: each chapter with GPS contributes its medoid fix,
/// snapped to the nearest `route` point. Consecutive chapters landing on the
/// same route point collapse into the first (a later label fills a missing
/// one).
pub fn chapter_route_stops(
    chapters: &[Chapter],
    assets: &[Asset],
    route: &[(f64, f64)],
) -> Vec<RouteStop> {
    if route.is_empty() {
        return vec![];
    }
    let gps_of: HashMap<&str, (f64, f64)> = assets
        .iter()
        .filter_map(|a| Some((a.id.as_str(), a.gps?)))
        .collect();
    let mut out: Vec<RouteStop> = vec![];
    for ch in chapters {
        let fixes: Vec<(f64, f64)> = ch
            .asset_ids
            .iter()
            .filter_map(|id| gps_of.get(id.as_str()).copied())
            .collect();
        let Some(center) = medoid(&fixes) else {
            continue;
        };
        let route_index = nearest_index(route, center);
        let label = ch.place.clone();
        let at_sea = label.is_some() && crate::geo::place_name(center).is_none();
        match out.last_mut() {
            // Same point, or the same named place again (days in one port).
            Some(prev)
                if prev.route_index == route_index || (label.is_some() && prev.label == label) =>
            {
                if prev.label.is_none() {
                    prev.label = label;
                }
            }
            _ => out.push(RouteStop {
                route_index,
                label,
                at_sea,
            }),
        }
    }
    out
}

fn nearest_index(route: &[(f64, f64)], p: (f64, f64)) -> usize {
    route
        .iter()
        .enumerate()
        .map(|(i, &q)| (haversine_m(p, q), i))
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map_or(0, |(_, i)| i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AssetKind;

    #[test]
    fn duration_grows_with_stops_and_is_capped() {
        assert_eq!(closing_map_secs(0), STATIC_CLOSING_MAP_SECS);
        assert_eq!(closing_map_secs(1), STATIC_CLOSING_MAP_SECS);
        assert_eq!(closing_map_secs(2), ROUTE_ANIM_MIN_SECS);
        assert!((closing_map_secs(6) - 8.4).abs() < 1e-9);
        assert_eq!(closing_map_secs(12), 13.8);
        for n in [13, 16, 40, 1000] {
            assert_eq!(closing_map_secs(n), ROUTE_ANIM_MAX_SECS, "{n} stops");
        }
        let mut last = 0.0;
        for n in 2..30 {
            let s = closing_map_secs(n);
            assert!(s >= last && s <= ROUTE_ANIM_MAX_SECS);
            last = s;
        }
    }

    fn chapter(place: Option<&str>, ids: &[&str]) -> Chapter {
        Chapter {
            title: place.unwrap_or("day").into(),
            place: place.map(Into::into),
            asset_ids: ids.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        }
    }

    fn asset(id: &str, gps: Option<(f64, f64)>) -> Asset {
        let mut a = Asset::new(id, std::path::PathBuf::from(id), AssetKind::Photo);
        a.gps = gps;
        a
    }

    #[test]
    fn stops_follow_chapters_snap_to_route_and_merge_repeats() {
        let route = vec![(35.0, 135.0), (35.01, 135.01), (1.35, 103.8)];
        let assets = vec![
            asset("a", Some((35.0, 135.0))),
            asset("b", Some((35.011, 135.011))),
            asset("c", None),
            asset("d", Some((1.36, 103.79))),
        ];
        let chapters = vec![
            chapter(Some("Kyoto"), &["a"]),
            chapter(None, &["c"]),
            chapter(None, &["b"]),
            chapter(Some("Singapore"), &["d"]),
        ];
        let stops = chapter_route_stops(&chapters, &assets, &route);
        assert_eq!(
            stops,
            vec![
                RouteStop {
                    route_index: 0,
                    label: Some("Kyoto".into()),
                    at_sea: false
                },
                RouteStop {
                    route_index: 1,
                    label: None,
                    at_sea: false
                },
                RouteStop {
                    route_index: 2,
                    label: Some("Singapore".into()),
                    at_sea: false
                },
            ]
        );
        // Same route point twice in a row: one stop, first label kept.
        let again = vec![
            chapter(Some("Kyoto"), &["a"]),
            chapter(Some("Gion"), &["a"]),
        ];
        let stops = chapter_route_stops(&again, &assets, &route);
        assert_eq!(stops.len(), 1);
        assert_eq!(stops[0].label.as_deref(), Some("Kyoto"));
        // The same place on consecutive days (a port, a hotel) is one stop.
        let port = vec![
            chapter(Some("Kyoto"), &["a"]),
            chapter(Some("Kyoto"), &["b"]),
            chapter(Some("Singapore"), &["d"]),
        ];
        assert_eq!(chapter_route_stops(&port, &assets, &route).len(), 2);
        assert!(chapter_route_stops(&chapters, &assets, &[]).is_empty());
    }

    #[test]
    fn override_named_chapters_in_open_water_are_sea_days() {
        // A ship's name over open water: GPS cannot name it, so it is not a stop.
        let route = vec![(35.6, 139.7), (20.0, -150.0), (1.35, 103.8)];
        let assets = vec![
            asset("a", Some((35.6, 139.7))),
            asset("b", Some((20.0, -150.0))),
            asset("c", Some((1.35, 103.8))),
        ];
        let chapters = vec![
            chapter(Some("Tokyo"), &["a"]),
            chapter(Some("Carnival Luminosa"), &["b"]),
            chapter(Some("Singapore"), &["c"]),
        ];
        let stops = chapter_route_stops(&chapters, &assets, &route);
        assert_eq!(
            stops.iter().map(|s| s.at_sea).collect::<Vec<_>>(),
            [false, true, false]
        );
        assert_eq!(animated_stop_count(&stops), 2);
    }
}
