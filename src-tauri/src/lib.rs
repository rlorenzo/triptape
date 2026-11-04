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

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

use serde::Serialize;
use triptape_core::db::{Db, EditedPlan};
use triptape_core::types::{Asset, AssetKind, Chapter, PlanEntry};
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

impl From<&Asset> for AssetView {
    fn from(a: &Asset) -> Self {
        Self {
            id: a.id.clone(),
            kind: a.kind.as_str().to_string(),
            width: a.width,
            height: a.height,
            duration_secs: a.duration_secs,
            taken_at: a.taken_at,
            time_guessed: a.time_guessed,
            has_gps: a.gps.is_some(),
            is_live_photo: a.kind == AssetKind::LivePhoto,
        }
    }
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

/// One keepsake pick inside a chapter, for the Story editor.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoryEntryView {
    #[serde(flatten)]
    pub asset: AssetView,
    pub caption: Option<String>,
    pub in_hero: bool,
    pub in_reels: bool,
}

/// One day/place chapter in the Story editor. `key` is the chapter's
/// first asset id in the stored plan and keys place renames.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChapterView {
    pub key: String,
    pub title: String,
    pub place: Option<String>,
    pub day_index: Option<u32>,
    pub date_label: Option<String>,
    pub entries: Vec<StoryEntryView>,
}

/// The whole editable story: chapters plus every excluded asset
/// (restorable), with pick counts for the header.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoryView {
    pub chapters: Vec<ChapterView>,
    pub excluded: Vec<StoryEntryView>,
    pub hero_count: usize,
    pub reel_count: usize,
}

/// Everything the Story editor reads for one project, loaded once.
pub struct StoryData(EditedPlan);

impl StoryData {
    pub fn load(db: &Db, project_id: &str) -> Result<Self, String> {
        db.edited_plan(Some(project_id))?
            .map(Self)
            .ok_or_else(|| "no plan for project yet — scan first".to_string())
    }

    fn entry_builder(&self) -> impl Fn(&str) -> Option<StoryEntryView> + '_ {
        fn ids(picks: &[PlanEntry]) -> HashSet<&str> {
            picks.iter().map(|e| e.asset_id.as_str()).collect()
        }
        let by_id: HashMap<&str, &Asset> =
            self.0.assets.iter().map(|a| (a.id.as_str(), a)).collect();
        let hero = ids(&self.0.edited.hero);
        let reels = ids(&self.0.edited.reel_selects);
        move |id| {
            Some(StoryEntryView {
                asset: AssetView::from(*by_id.get(id)?),
                caption: self.0.edited.captions.get(id).cloned(),
                in_hero: hero.contains(id),
                in_reels: reels.contains(id),
            })
        }
    }

    pub fn view(&self) -> StoryView {
        let entry = self.entry_builder();
        // Keys come from the pristine plan so excluding a chapter's first
        // asset doesn't orphan its place rename.
        let mut key_of: HashMap<&str, &str> = HashMap::new();
        for c in &self.0.base.chapters {
            if let Some(key) = c.asset_ids.first() {
                for id in &c.asset_ids {
                    key_of.insert(id, key);
                }
            }
        }
        let chapters = self
            .0
            .edited
            .chapters
            .iter()
            .map(|c| ChapterView {
                key: c
                    .asset_ids
                    .first()
                    .and_then(|id| key_of.get(id.as_str()))
                    .map(|k| k.to_string())
                    .unwrap_or_default(),
                title: c.title.clone(),
                place: c.place.clone(),
                day_index: c.day_index,
                date_label: c.date_label.clone(),
                entries: c.asset_ids.iter().filter_map(|id| entry(id)).collect(),
            })
            .collect();
        StoryView {
            chapters,
            excluded: self
                .0
                .edits
                .excluded
                .iter()
                .filter_map(|id| entry(id))
                .collect(),
            hero_count: self.0.edited.hero.len(),
            reel_count: self.0.edited.reel_selects.len(),
        }
    }

    /// Every asset of `asset_id`'s scan-time chapter, removed ones included
    /// (choosing a removed asset as a replacement restores it).
    pub fn alternatives(&self, asset_id: &str) -> Result<Vec<StoryEntryView>, String> {
        let chapter = self
            .0
            .base
            .chapters
            .iter()
            .find(|c| c.asset_ids.iter().any(|id| id == asset_id))
            .ok_or_else(|| "asset is not in any chapter".to_string())?;
        let entry = self.entry_builder();
        Ok(chapter
            .asset_ids
            .iter()
            .filter_map(|id| entry(id))
            .collect())
    }

    /// A swap replaces a current video/reel pick with a different asset of
    /// the same scan-time chapter, not yet picked in a keepsake `from` is in;
    /// anything else would do nothing, shorten that keepsake, or move a pick
    /// to another day.
    pub fn check_swap(&self, from: &str, to: &str) -> Result<(), String> {
        if from == to {
            return Err("an asset can't replace itself".to_string());
        }
        let plan = &self.0.edited;
        let picked = |picks: &[PlanEntry], id: &str| picks.iter().any(|e| e.asset_id == id);
        let keepsakes = [&plan.hero, &plan.reel_selects];
        if !keepsakes.iter().any(|picks| picked(picks, from)) {
            return Err("only video and reel picks can be replaced".to_string());
        }
        if keepsakes
            .iter()
            .any(|picks| picked(picks, from) && picked(picks, to))
        {
            return Err("the replacement is already in that keepsake".to_string());
        }
        let member = |c: &Chapter, id: &str| c.asset_ids.iter().any(|a| a == id);
        if !self
            .0
            .base
            .chapters
            .iter()
            .any(|c| member(c, from) && member(c, to))
        {
            return Err("the replacement must come from the same chapter".to_string());
        }
        Ok(())
    }
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
    use triptape_core::plan::{build_plan, PlanEdits};

    fn story_db(ids: &[&str]) -> (Db, String) {
        let db = Db::open_in_memory().unwrap();
        let project = db.create_project("trip", "/tmp/trip").unwrap();
        let assets: Vec<Asset> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let path = PathBuf::from(format!("/t/{id}.jpg"));
                let mut a = Asset::new(*id, path, AssetKind::Photo);
                a.taken_at = Some(1_714_557_600 + i as i64 * 300);
                a
            })
            .collect();
        for a in &assets {
            db.upsert_asset(&project.id, a).unwrap();
        }
        let cluster_of = assets
            .iter()
            .map(|a| (a.id.clone(), "2024-05-01".to_string()))
            .collect();
        let plan = build_plan(&assets, &cluster_of, &HashMap::new());
        db.insert_plan(&project.id, &plan, "local").unwrap();
        (db, project.id)
    }

    #[test]
    fn story_chapter_key_is_stable_when_first_asset_is_excluded() {
        let (db, pid) = story_db(&["a", "b", "c"]);
        let edits = PlanEdits {
            excluded: vec!["a".into()],
            ..Default::default()
        };
        db.save_plan_edits(&pid, &edits).unwrap();
        let view = StoryData::load(&db, &pid).unwrap().view();
        assert_eq!(view.chapters[0].key, "a");
        let ids: Vec<&str> = view.chapters[0]
            .entries
            .iter()
            .map(|e| e.asset.id.as_str())
            .collect();
        assert_eq!(ids, ["b", "c"]);
        assert_eq!(view.excluded[0].asset.id, "a");
        assert_eq!(view.excluded[0].asset.kind, "photo");
    }

    #[test]
    fn story_alternatives_include_removed_assets_of_the_chapter() {
        let (db, pid) = story_db(&["a", "b"]);
        let edits = PlanEdits {
            excluded: vec!["a".into()],
            ..Default::default()
        };
        db.save_plan_edits(&pid, &edits).unwrap();
        let data = StoryData::load(&db, &pid).unwrap();
        let ids: Vec<String> = data
            .alternatives("b")
            .unwrap()
            .into_iter()
            .map(|e| e.asset.id)
            .collect();
        assert_eq!(ids, ["a", "b"]);
        assert!(data.alternatives("missing").is_err());
    }

    #[test]
    fn story_entry_serializes_flat_for_the_webview() {
        let (db, pid) = story_db(&["a"]);
        let view = StoryData::load(&db, &pid).unwrap().view();
        let json = serde_json::to_value(&view.chapters[0].entries[0]).unwrap();
        assert_eq!(json["id"], "a");
        assert_eq!(json["kind"], "photo");
        assert_eq!(json["takenAt"], 1_714_557_600);
        assert!(json.get("inHero").is_some());
    }

    #[test]
    fn story_swap_requires_a_current_pick_and_same_chapter_target() {
        let (db, pid) = story_db(&["a", "b", "c", "d"]);
        let (_, mut plan) = db.latest_plan_with_project(Some(&pid)).unwrap().unwrap();
        plan.chapters[0].asset_ids = vec!["a".into(), "b".into(), "c".into()];
        let mut other = plan.chapters[0].clone();
        other.asset_ids = vec!["d".into()];
        plan.chapters.push(other);
        plan.hero.retain(|e| e.asset_id == "a");
        plan.reel_selects.clear();
        db.insert_plan(&pid, &plan, "local").unwrap();

        let data = StoryData::load(&db, &pid).unwrap();
        assert!(data.check_swap("a", "b").is_ok());
        assert!(data.check_swap("a", "a").is_err()); // itself
        assert!(data.check_swap("b", "c").is_err()); // not a pick
        assert!(data.check_swap("a", "d").is_err()); // another chapter
        assert!(data.check_swap("a", "missing").is_err());
    }

    #[test]
    fn story_swap_rejects_a_target_already_picked_in_the_same_keepsake() {
        let (db, pid) = story_db(&["a", "b", "c", "d"]);
        let (_, mut plan) = db.latest_plan_with_project(Some(&pid)).unwrap().unwrap();
        plan.hero.retain(|e| e.asset_id == "a" || e.asset_id == "b");
        plan.reel_selects = plan
            .hero
            .iter()
            .filter(|e| e.asset_id == "b")
            .cloned()
            .collect();
        db.insert_plan(&pid, &plan, "local").unwrap();

        let data = StoryData::load(&db, &pid).unwrap();
        assert!(data.check_swap("a", "b").is_err()); // both in the video
        assert!(data.check_swap("b", "a").is_err()); // both in the video
        assert!(data.check_swap("a", "c").is_ok());

        // A removed pick is no longer selected, so it can come back.
        let mut edits = PlanEdits::default();
        edits.set_excluded("b".into(), true);
        db.save_plan_edits(&pid, &edits).unwrap();
        let data = StoryData::load(&db, &pid).unwrap();
        data.check_swap("a", "b").unwrap();
        edits.swap("a".into(), "b".into());
        db.save_plan_edits(&pid, &edits).unwrap();
        let view = StoryData::load(&db, &pid).unwrap().view();
        assert_eq!(view.hero_count, 1);
        let picked: Vec<&str> = view.chapters[0]
            .entries
            .iter()
            .filter(|e| e.in_hero)
            .map(|e| e.asset.id.as_str())
            .collect();
        assert_eq!(picked, ["b"]);
    }

    #[test]
    fn story_load_requires_a_plan() {
        let db = Db::open_in_memory().unwrap();
        let project = db.create_project("trip", "/tmp/trip").unwrap();
        assert!(StoryData::load(&db, &project.id).is_err());
    }

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
