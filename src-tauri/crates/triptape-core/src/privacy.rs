//! Privacy: EXIF strip on export, home fuzzing, pairing-UUID hygiene.
//!
//! - Exports strip EXIF (`-map_metadata -1`): GPS, camera serial, and the
//!   Live Photo pairing UUID go away together, so a shared still cannot be
//!   quietly re-linked to its motion file.
//! - Map routes fuzz the first/last ~500 m near home (toggleable).

use crate::geo::{bearing_deg, haversine_m, project};

/// Extra output args that strip all container/stream metadata on export.
pub fn exif_strip_args() -> Vec<String> {
    vec!["-map_metadata".into(), "-1".into()]
}

/// Suffix appended to an export filename to mark the pairing as stripped.
pub const PAIRING_STRIPPED_MARKER: &str = "pairing-stripped";

/// Radius (m) around home inside which route points are fuzzed.
pub const HOME_FUZZ_METERS: f64 = 500.0;

/// Push every route point within [`HOME_FUZZ_METERS`] of `home` out along
/// its bearing from home to exactly that radius. Deterministic and stable:
/// points already outside the radius are untouched.
pub fn fuzz_home_route(route: &[(f64, f64)], home: (f64, f64)) -> Vec<(f64, f64)> {
    route.iter().map(|&p| fuzz_point(p, home)).collect()
}

fn fuzz_point(p: (f64, f64), home: (f64, f64)) -> (f64, f64) {
    let d = haversine_m(p, home);
    if d >= HOME_FUZZ_METERS {
        return p;
    }
    // Exactly at home has no bearing: step due north to the ring.
    let bearing = if d < 1e-9 { 0.0 } else { bearing_deg(home, p) };
    project(home, bearing, HOME_FUZZ_METERS)
}

/// True when the point needs fuzzing (inside the home radius).
pub fn needs_fuzz(p: (f64, f64), home: (f64, f64)) -> bool {
    haversine_m(p, home) < HOME_FUZZ_METERS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_args_cover_metadata() {
        let a = exif_strip_args();
        assert!(a.contains(&"-map_metadata".to_string()));
        assert!(a.contains(&"-1".to_string()));
    }

    #[test]
    fn home_fuzz_pushes_to_ring() {
        let home = (37.7749, -122.4194);
        // ~111 m north of home.
        let near = (37.7759, -122.4194);
        assert!(needs_fuzz(near, home));
        let out = fuzz_home_route(&[near], home);
        let d = haversine_m(out[0], home);
        assert!(
            (d - HOME_FUZZ_METERS).abs() < 5.0,
            "fuzzed point must sit ~500 m out, got {d}"
        );
        // Far points untouched.
        let far = (34.0522, -118.2437);
        assert!(!needs_fuzz(far, home));
        assert_eq!(fuzz_home_route(&[far], home), vec![far]);
        // At home: deterministic push north, also ~500 m.
        let out = fuzz_home_route(&[home], home);
        let d = haversine_m(out[0], home);
        assert!((d - HOME_FUZZ_METERS).abs() < 5.0);
    }
}
