//! Privacy: EXIF strip on export, home fuzzing, pairing-UUID hygiene.
//!
//! - Exports strip EXIF (`-map_metadata -1`): GPS, camera serial, and the
//!   Live Photo pairing UUID go away together, so a shared still cannot be
//!   quietly re-linked to its motion file.
//! - Map routes fuzz the first/last ~500 m near home (toggleable).
//!
//! What an export file may carry (PLAN.md Sec 14), per kind:
//!
//! - Film and reel MP4s, reel cover JPEGs: nothing copied from a source
//!   file. Every FFmpeg command that writes one ends with
//!   [`exif_strip_args`], so container and stream tags (GPS `©xyz` and
//!   ISO 6709 location, make/model/lens, serials, `creation_time`, Apple
//!   `com.apple.quicktime.*` keys including `content.identifier`) and
//!   chapters are dropped. Rotation is not metadata here: the filters
//!   apply it to the pixels, so the file plays upright with no matrix.
//! - Book page JPEGs and `cover.jpg`: re-encoded from decoded pixels by the
//!   `image` crate, which writes no EXIF, XMP or MakerNote at all.
//! - `proof.pdf` / `press.pdf`: Typst embeds those clean JPEGs; the PDF
//!   info carries only Typst's own producer and the export date.
//! - Dates: the only capture dates in an export are the ones the owner sees
//!   on screen or on the page (title-card range, chapter datelines). No
//!   file carries a source's original capture timestamp as metadata.

use crate::geo::{bearing_deg, haversine_m, project};

/// Output args that strip all container/stream metadata and chapters from
/// an FFmpeg output. FFmpeg otherwise copies the first input's tags, and
/// that includes a phone video's `©xyz` GPS location.
pub fn exif_strip_args() -> Vec<String> {
    ["-map_metadata", "-1", "-map_chapters", "-1"]
        .map(String::from)
        .to_vec()
}

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
        assert!(a.contains(&"-map_chapters".to_string()));
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
