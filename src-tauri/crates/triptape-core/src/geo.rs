//! Spherical-earth geodesy helpers shared by clustering and privacy fuzzing.
//! Coordinates are `(lat, lon)` in degrees; distances in meters.

/// Mean earth radius (m).
pub const EARTH_RADIUS_M: f64 = 6_371_000.0;

/// Great-circle (haversine) distance in meters.
pub fn haversine_m(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (a_lat, b_lat) = (a.0.to_radians(), b.0.to_radians());
    let (dlat, dlon) = ((b.0 - a.0).to_radians(), (b.1 - a.1).to_radians());
    let h = (dlat / 2.0).sin().powi(2) + a_lat.cos() * b_lat.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * h.sqrt().asin()
}

/// Initial bearing from `a` to `b` in degrees (0 = north, clockwise).
pub fn bearing_deg(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (a_lat, b_lat, dlon) = (a.0.to_radians(), b.0.to_radians(), (b.1 - a.1).to_radians());
    let y = dlon.sin() * b_lat.cos();
    let x = a_lat.cos() * b_lat.sin() - a_lat.sin() * b_lat.cos() * dlon.cos();
    y.atan2(x).to_degrees()
}

/// Destination point `dist_m` from `origin` along `bearing_deg`.
pub fn project(origin: (f64, f64), bearing_deg: f64, dist_m: f64) -> (f64, f64) {
    let (lat, bearing) = (origin.0.to_radians(), bearing_deg.to_radians());
    let ang = dist_m / EARTH_RADIUS_M;
    let nlat = (lat.sin() * ang.cos() + lat.cos() * ang.sin() * bearing.cos()).asin();
    let nlon = origin.1.to_radians()
        + (bearing.sin() * ang.sin() * lat.cos()).atan2(ang.cos() - lat.sin() * nlat.sin());
    (nlat.to_degrees(), nlon.to_degrees())
}

/// Gazetteer of populated places (GeoNames cities15000, CC BY 4.0; see
/// `resources/gazetteer/ATTRIBUTION.md`), embedded at compile time.
/// Columns: name, country, latitude, longitude.
const GAZETTEER_TSV: &str = include_str!("../../../../resources/gazetteer/cities.tsv");

struct City {
    name: &'static str,
    country: &'static str,
    lat: f64,
    lon: f64,
}

fn gazetteer() -> &'static [City] {
    static CITIES: std::sync::OnceLock<Vec<City>> = std::sync::OnceLock::new();
    CITIES.get_or_init(|| {
        GAZETTEER_TSV
            .lines()
            .filter_map(|line| {
                let mut p = line.split('\t');
                Some(City {
                    name: p.next()?,
                    country: p.next()?,
                    lat: p.next()?.parse().ok()?,
                    lon: p.next()?.parse().ok()?,
                })
            })
            .collect()
    })
}

/// Nearest populated place to `gps` within 100 km, as `City, Country`.
/// `None` when nothing is close enough to name honestly (open ocean,
/// remote areas). This is the only place-name source in the app: no
/// network reverse geocoding, ever.
pub fn place_name(gps: (f64, f64)) -> Option<String> {
    place_name_within(gps, 100_000.0)
}

fn place_name_within(gps: (f64, f64), max_m: f64) -> Option<String> {
    // Cheap bounding-box reject before the haversine. The latitude bound is
    // a fixed degree span; the longitude bound widens with latitude (a
    // degree of longitude shrinks by cos(lat)) and wraps across ±180°.
    let lat_deg = max_m / 111_000.0 + 0.5;
    let edge_lat = (gps.0.abs() + lat_deg).min(90.0).to_radians();
    let lon_deg = (lat_deg / edge_lat.cos().max(1e-6)).min(180.0);
    let mut best: Option<(&City, f64)> = None;
    for c in gazetteer() {
        let dlon = (c.lon - gps.1).rem_euclid(360.0);
        if (c.lat - gps.0).abs() > lat_deg || dlon.min(360.0 - dlon) > lon_deg {
            continue;
        }
        let d = haversine_m(gps, (c.lat, c.lon));
        if d <= max_m && best.is_none_or(|(_, bd)| d < bd) {
            best = Some((c, d));
        }
    }
    best.map(|(c, _)| format!("{}, {}", c.name, c.country))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn place_name_finds_nearest_city() {
        // Fushimi Inari, Kyoto.
        assert_eq!(
            place_name((34.9671, 135.7727)).as_deref(),
            Some("Kyoto, Japan")
        );
        // Marina Bay, Singapore.
        assert_eq!(
            place_name((1.2834, 103.8607)).as_deref(),
            Some("Singapore, Singapore")
        );
        // Middle of the Pacific: honestly unnamed.
        assert_eq!(place_name((0.0, -150.0)), None);
    }

    #[test]
    fn place_name_high_latitude_and_date_line() {
        // ~67 km west of Alta at 70°N: 1.77° of longitude, still in range.
        assert_eq!(place_name((69.97, 21.5)).as_deref(), Some("Alta, Norway"));
        // Across the antimeridian from Labasa (179.36°E), ~89 km away.
        assert_eq!(
            place_name((-16.4332, -179.8)).as_deref(),
            Some("Labasa, Fiji")
        );
    }

    #[test]
    fn haversine_known_distance() {
        // SF -> LA is ~559 km.
        let d = haversine_m((37.7749, -122.4194), (34.0522, -118.2437));
        assert!((d - 559_000.0).abs() < 5_000.0, "got {d}");
        assert_eq!(haversine_m((1.0, 2.0), (1.0, 2.0)), 0.0);
    }

    #[test]
    fn project_round_trips_bearing_and_distance() {
        let origin = (37.7749, -122.4194);
        let p = project(origin, 90.0, 1_000.0);
        assert!((haversine_m(origin, p) - 1_000.0).abs() < 1.0);
        assert!((bearing_deg(origin, p) - 90.0).abs() < 0.1);
    }
}
