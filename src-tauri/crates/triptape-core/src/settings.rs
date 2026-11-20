//! The owner's settings, shared by the app and the CLI: one small JSON file
//! in the app's data dir (macOS `~/Library/Application Support/
//! app.triptape.desktop/settings.json`, Windows `%APPDATA%\app.triptape.desktop`).
//!
//! Nothing here leaves the computer. The home position is used only to
//! fuzz the drawn route near home ([`crate::privacy::fuzz_home_route`]).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The Tauri bundle identifier: names the app's data and cache dirs.
pub const APP_IDENTIFIER: &str = "app.triptape.desktop";

/// Smallest and largest segment cache cap the owner may set.
pub const MIN_SEGMENT_CACHE_CAP: u64 = 1 << 30;
pub const MAX_SEGMENT_CACHE_CAP: u64 = 1 << 40;

/// Where home is, in decimal degrees.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Home {
    pub lat: f64,
    pub lon: f64,
}

impl Home {
    /// A home position, if `lat`/`lon` are real coordinates.
    pub fn new(lat: f64, lon: f64) -> Result<Self, String> {
        if !(lat.is_finite() && (-90.0..=90.0).contains(&lat)) {
            return Err("Latitude must be a number from -90 to 90.".into());
        }
        if !(lon.is_finite() && (-180.0..=180.0).contains(&lon)) {
            return Err("Longitude must be a number from -180 to 180.".into());
        }
        Ok(Self { lat, lon })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// Home, when the owner has set it.
    pub home: Option<Home>,
    /// Fuzz the route near home on every map (on by default).
    pub hide_home: bool,
    /// Segment cache size limit; `None` keeps the built-in default.
    pub segment_cache_max_bytes: Option<u64>,
    /// Per-install salt for the log's short file and folder hashes, so a
    /// shared report cannot be matched against a list of likely names.
    pub log_salt: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            home: None,
            hide_home: true,
            segment_cache_max_bytes: None,
            log_salt: None,
        }
    }
}

impl Settings {
    /// The settings file: `TRIPTAPE_SETTINGS` when set (tests, scripted
    /// runs), else `settings.json` in the app's data dir.
    pub fn default_path() -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("TRIPTAPE_SETTINGS") {
            return Some(PathBuf::from(p));
        }
        dirs::data_dir().map(|d| d.join(APP_IDENTIFIER).join("settings.json"))
    }

    /// Read `path`. A missing or unreadable file gives the defaults, and a
    /// home or cap that is out of range is dropped rather than trusted.
    pub fn load(path: &Path) -> Self {
        let mut s: Self = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        s.home = s.home.and_then(|h| Home::new(h.lat, h.lon).ok());
        s.segment_cache_max_bytes = s
            .segment_cache_max_bytes
            .filter(|b| check_segment_cache_cap(*b).is_ok());
        s
    }

    /// Write to `path` so it is never seen half written.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, text)
            .and_then(|()| std::fs::rename(&tmp, path))
            .map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                e.to_string()
            })
    }

    /// Make a log salt if there is none yet; true when one was made.
    pub fn ensure_log_salt(&mut self) -> bool {
        if self.log_salt.is_some() {
            return false;
        }
        self.log_salt = Some(uuid::Uuid::new_v4().simple().to_string());
        true
    }

    /// The home to hide on maps: set, and hiding switched on.
    pub fn home_to_hide(&self) -> Option<(f64, f64)> {
        self.home.filter(|_| self.hide_home).map(|h| (h.lat, h.lon))
    }

    /// Fuzz `route` near home when hiding applies; true when it did.
    pub fn hide_home_on(&self, route: &mut Vec<(f64, f64)>) -> bool {
        match self.home_to_hide() {
            Some(home) => {
                *route = crate::privacy::fuzz_home_route(route, home);
                true
            }
            None => false,
        }
    }
}

/// A cap the owner may choose: from 1 GB to 1 TB.
pub fn check_segment_cache_cap(bytes: u64) -> Result<u64, String> {
    if (MIN_SEGMENT_CACHE_CAP..=MAX_SEGMENT_CACHE_CAP).contains(&bytes) {
        Ok(bytes)
    } else {
        Err("Choose a limit from 1 GB to 1 TB.".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::haversine_m;

    #[test]
    fn defaults_hide_home_once_one_is_set() {
        let mut s = Settings::default();
        assert!(s.hide_home);
        assert_eq!(s.home_to_hide(), None);
        s.home = Some(Home::new(37.7749, -122.4194).unwrap());
        assert_eq!(s.home_to_hide(), Some((37.7749, -122.4194)));
        s.hide_home = false;
        assert_eq!(s.home_to_hide(), None);
    }

    #[test]
    fn home_rejects_impossible_coordinates() {
        assert!(Home::new(91.0, 0.0).is_err());
        assert!(Home::new(0.0, -181.0).is_err());
        assert!(Home::new(f64::NAN, 0.0).is_err());
        assert!(Home::new(-33.86, 151.21).is_ok());
    }

    #[test]
    fn round_trips_and_distrusts_a_hand_edited_file() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("deep").join("settings.json");
        assert_eq!(Settings::load(&path), Settings::default());
        let s = Settings {
            home: Some(Home::new(35.3192, 139.5522).unwrap()),
            hide_home: true,
            segment_cache_max_bytes: Some(5 << 30),
            log_salt: Some("salt".into()),
        };
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path), s);
        std::fs::write(
            &path,
            r#"{"home":{"lat":123,"lon":0},"segmentCacheMaxBytes":5}"#,
        )
        .unwrap();
        let bad = Settings::load(&path);
        assert_eq!(bad.home, None);
        assert_eq!(bad.segment_cache_max_bytes, None);
        assert!(bad.hide_home, "a missing field keeps its default");
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(Settings::load(&path), Settings::default());
    }

    #[test]
    fn hiding_home_moves_nearby_route_points_out() {
        let home = (35.3192, 139.5522);
        let s = Settings {
            home: Some(Home::new(home.0, home.1).unwrap()),
            ..Default::default()
        };
        let mut route = vec![(35.3193, 139.5522), (35.6812, 139.7671)];
        assert!(s.hide_home_on(&mut route));
        assert!(haversine_m(route[0], home) > 495.0);
        assert_eq!(route[1], (35.6812, 139.7671));
    }

    #[test]
    fn cache_cap_bounds() {
        assert!(check_segment_cache_cap(MIN_SEGMENT_CACHE_CAP - 1).is_err());
        assert!(check_segment_cache_cap(20 << 30).is_ok());
        assert!(check_segment_cache_cap(MAX_SEGMENT_CACHE_CAP + 1).is_err());
    }
}
