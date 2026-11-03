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

#[cfg(test)]
mod tests {
    use super::*;

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
