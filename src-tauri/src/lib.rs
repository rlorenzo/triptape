//! TripTape Tauri v2 glue: commands, events, app state only.
//!
//! IPC contract (PLAN.md Section 7):
//! - Coarse commands only: `scan_folder`, `get_assets`, `start_job`,
//!   `cancel_job`, `export_video`, `export_reels`, `export_book`.
//! - Commands take scoped identifiers (project id, asset id, job id),
//!   never raw filesystem paths — except the folder-picker result returned
//!   by the OS dialog, which enters only through `scan_folder`.
//! - Progress goes out over `job://progress` events (~5 Hz), never one
//!   invoke per file. Thumbnails are served over the `asset://` protocol
//!   scoped to the cache dir; originals are never exposed to the webview.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::Serialize;
use triptape_core::db::Db;
use triptape_media::export::Skipped;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// App state: the open store. `Db` is a single-writer handle shared by
/// every command and background worker, so it sits behind a `Mutex`.
pub struct AppState {
    pub db: Mutex<Db>,
}

impl AppState {
    pub fn open(db_path: &std::path::Path) -> Result<Self, String> {
        Ok(Self {
            db: Mutex::new(Db::open(db_path)?),
        })
    }

    pub fn open_in_memory() -> Result<Self, String> {
        Ok(Self {
            db: Mutex::new(Db::open_in_memory()?),
        })
    }

    /// Reject malformed ids (`../` traversal, empty, control chars) before
    /// any backend lookup. See PLAN.md Section 7 IPC hardening.
    pub fn check_id(id: &str) -> Result<(), String> {
        if id.is_empty() || id.len() > 128 {
            return Err("malformed id".to_string());
        }
        if id.contains('/') || id.contains('\\') || id.contains("..") {
            return Err("malformed id: path separator rejected".to_string());
        }
        if !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err("malformed id".to_string());
        }
        Ok(())
    }

    /// The project must exist in the store; cross-project asset ids are
    /// rejected by scoping every lookup through the project row.
    pub fn require_project(&self, project_id: &str) -> Result<(), String> {
        Self::check_id(project_id)?;
        let db = self.db.lock().map_err(|e| e.to_string())?;
        match db.get_project(project_id)? {
            Some(_) => Ok(()),
            None => Err(format!("unknown project {project_id}")),
        }
    }
}

// ---------------------------------------------------------------------------
// DTOs (no raw filesystem paths cross IPC)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetView {
    pub id: String,
    pub kind: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_secs: Option<f64>,
    pub taken_at: Option<i64>,
    pub time_guessed: bool,
    pub has_gps: bool,
    pub is_live_photo: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanSummary {
    pub project_id: String,
    pub project_name: String,
    pub files_seen: usize,
    pub job_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobView {
    pub id: String,
    pub kind: String,
    pub state: String,
    pub progress: f32,
    pub current_step: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSummary {
    pub job_id: String,
    pub out_dir: String,
    pub video: Option<String>,
    pub reels: Vec<String>,
    pub book_pages: usize,
    pub skipped: Vec<Skipped>,
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested without a Tauri runtime)
// ---------------------------------------------------------------------------

/// Borrow one page of `items`; page is 0-based, `per_page` clamped to 1..=500.
pub fn paginate<T>(items: &[T], page: usize, per_page: usize) -> &[T] {
    let per = per_page.clamp(1, 500);
    items.chunks(per).nth(page).unwrap_or(&[])
}

/// Default export destination for a trip: `~/Movies/TripTape/<trip> (<project id>)/`
/// (Windows: `%USERPROFILE%\\Videos\\TripTape\\...`). The project id keeps two
/// trips with the same folder name from overwriting each other's output.
/// The frontend never sends a path; the backend derives it.
pub fn default_export_dir(trip_name: &str, project_id: &str) -> PathBuf {
    let safe: String = trip_name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe = match safe.trim() {
        "" => "trip",
        s => s,
    };
    // `project_id` is validated by `AppState::check_id` (no separators).
    dirs_base()
        .join("TripTape")
        .join(format!("{safe} ({project_id})"))
}

#[cfg(target_os = "windows")]
fn dirs_base() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\"))
        .join("Videos")
}

#[cfg(not(target_os = "windows"))]
fn dirs_base() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("Movies")
}

/// Supported job kinds for `start_job`.
pub fn valid_job_kind(kind: &str) -> bool {
    matches!(
        kind,
        "ingest" | "render-video" | "render-reels" | "render-book" | "export-all"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_traversal_ids() {
        assert!(AppState::check_id("../etc").is_err());
        assert!(AppState::check_id("a/b").is_err());
        assert!(AppState::check_id("a\\b").is_err());
        assert!(AppState::check_id("").is_err());
        assert!(AppState::check_id("ok-id_123").is_ok());
        assert!(AppState::check_id("prj_abc123").is_ok());
    }

    #[test]
    fn cross_project_unknown_project_rejected() {
        let s = AppState::open_in_memory().unwrap();
        assert!(s.require_project("prj_nope").is_err());
    }

    #[test]
    fn malformed_ids_rejected_before_lookup() {
        let s = AppState::open_in_memory().unwrap();
        assert!(s.require_project("../../x").is_err());
    }

    #[test]
    fn paginate_clamps() {
        let v: Vec<u32> = (0..10).collect();
        assert_eq!(paginate(&v, 0, 4), [0, 1, 2, 3]);
        assert_eq!(paginate(&v, 2, 4), [8, 9]);
        assert_eq!(paginate(&v, 9, 4).len(), 0);
        assert_eq!(paginate(&v, 0, 9999).len(), 10);
        assert_eq!(paginate(&v, usize::MAX, 4).len(), 0);
        // per_page 0 is clamped to 1 rather than panicking in `chunks`.
        assert_eq!(paginate(&v, 3, 0), [3]);
    }

    #[test]
    fn export_dir_falls_back_for_blank_trip_name() {
        assert!(default_export_dir("   ", "prj_1").ends_with("TripTape/trip (prj_1)"));
        assert!(
            default_export_dir("Côte d'Azur", "prj_1").ends_with("TripTape/Côte d_Azur (prj_1)")
        );
    }

    #[test]
    fn export_dir_unique_per_project() {
        assert_ne!(
            default_export_dir("Rome", "prj_a"),
            default_export_dir("Rome", "prj_b")
        );
    }

    #[test]
    fn export_dir_sanitizes_trip_name() {
        let d = default_export_dir("../../evil", "prj_1");
        let s = d.to_string_lossy();
        assert!(!s.contains(".."));
        assert!(s.contains("TripTape"));
    }

    #[test]
    fn only_known_job_kinds() {
        assert!(valid_job_kind("export-all"));
        assert!(!valid_job_kind("rm -rf"));
        assert!(!valid_job_kind(""));
    }
}
