//! TripTape Tauri v2 glue: commands, events, app state only.
//!
//! IPC contract (PLAN.md Section 7):
//! - Coarse commands only: `scan_folder`, `get_assets`, `start_job`,
//!   `cancel_job`, `export_video`, `export_reels`, `export_book`.
//! - Commands take scoped identifiers (project id, asset id, job id),
//!   never raw filesystem paths — except folder-picker results returned
//!   by the OS dialog (the trip folder through `scan_folder`, the export
//!   folder through `check_export_folder` and the export commands, checked
//!   by [`resolve_export_parent`]) and a finished film's path, which
//!   `open_film` accepts only via [`finished_film`].
//! - Progress goes out over `job://progress` events (~5 Hz), never one
//!   invoke per file. Thumbnails are served over the `asset://` protocol
//!   scoped to the cache dir; originals are never exposed to the webview.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;
use triptape_core::db::{Db, EditedPlan};
use triptape_core::metadata;
use triptape_core::osxphotos::OsxPhotosIndex;
use triptape_core::scan::{ScannedFile, VIDEO_EXTS};
use triptape_core::types::{distinct_assets, Asset, AssetKind, Chapter, Plan, PlanEntry};
use triptape_media::export::Skipped;
use triptape_media::render_book::{BookPreset, MAX_BOOK_PAGES};

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// App state: the open store. `Db` is a single-writer handle shared by
/// every command and background worker, so it sits behind a `Mutex`.
pub struct AppState {
    pub db: Mutex<Db>,
    /// Cancel flags of the exports running now, by job id.
    pub running_exports: Mutex<std::collections::HashMap<String, triptape_core::jobs::CancelFlag>>,
}

impl AppState {
    pub fn open(db_path: &std::path::Path) -> Result<Self, String> {
        Ok(Self {
            db: Mutex::new(Db::open(db_path)?),
            running_exports: Mutex::default(),
        })
    }

    pub fn open_in_memory() -> Result<Self, String> {
        Ok(Self {
            db: Mutex::new(Db::open_in_memory()?),
            running_exports: Mutex::default(),
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
    pub place: Option<String>,
    /// Combined 0..1 quality score, once background scoring has run.
    pub score: Option<f32>,
    /// Whether the stored score marks this asset a story highlight.
    pub highlight: bool,
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
            place: a
                .gps
                .and_then(triptape_core::geo::place_name)
                .filter(|s| !s.is_empty()),
            score: a.score.as_ref().map(|s| s.total),
            highlight: a.score.as_ref().is_some_and(|s| s.highlight),
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
    pub project_id: String,
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

const NOT_IN_CHAPTER: &str = "asset is not in any chapter";

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
            .chapter_of(asset_id)
            .ok_or_else(|| NOT_IN_CHAPTER.to_string())?;
        let entry = self.entry_builder();
        Ok(chapter
            .asset_ids
            .iter()
            .filter_map(|id| entry(id))
            .collect())
    }

    /// Only assets of a scan-time chapter can be pinned into the film
    /// (the plan needs the chapter to trade a shot off against).
    pub fn check_include(&self, asset_id: &str) -> Result<(), String> {
        self.0
            .base
            .chapter_of(asset_id)
            .map(|_| ())
            .ok_or_else(|| NOT_IN_CHAPTER.to_string())
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
    /// What the book's print check found, in plain words (empty when the
    /// book wasn't made or nothing needs a look).
    pub book_notes: Vec<String>,
}

/// The book's preflight results from an export's fallback notes, worded for
/// the owner. The renderer reports them only as notes in `export.json`; this
/// groups them so the Export screen can show them. Notes it doesn't know
/// are left to `export.json`.
pub fn book_notes(fallbacks: &[String]) -> Vec<String> {
    use triptape_media::render_book::{preflight, DpiVerdict};
    // The renderer's own wording of a soft-photo warning, so a reworded
    // message still matches.
    let soft = preflight(
        BookPreset::P1,
        MAX_BOOK_PAGES,
        &[],
        &[(1, DpiVerdict::Warn)],
    )
    .into_iter()
    .find(|i| i.level == "warn")
    .map(|i| i.message)
    .unwrap_or_default();
    let mut soft_pages: Vec<usize> = vec![];
    let (mut too_small, mut unavailable) = (0usize, 0usize);
    let mut fit: Option<String> = None;
    for note in fallbacks {
        if let Some((page, message)) = note
            .strip_prefix("page ")
            .and_then(|rest| rest.split_once(": "))
        {
            if message == soft {
                if let Ok(n) = page.parse() {
                    soft_pages.push(n);
                }
            }
            continue;
        }
        let Some(note) = note.strip_prefix("book: ") else {
            continue;
        };
        if note.contains("excluded (below 200 dpi") {
            too_small += 1;
        } else if note.ends_with("; skipped") {
            unavailable += 1;
        } else if let Some(rest) = note.strip_prefix("selected ") {
            fit = fit_note(rest);
        }
    }
    soft_pages.sort_unstable();
    soft_pages.dedup();
    let mut notes = vec![];
    match soft_pages.as_slice() {
        [] => {}
        [page] => notes.push(format!(
            "Page {page} has a photo at 200-299 dpi. It may print a little soft."
        )),
        pages => notes.push(format!(
            "{} pages have a photo at 200-299 dpi and may print a little soft: pages {}.",
            pages.len(),
            pages
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
    if too_small > 0 {
        notes.push(format!(
            "{} left out because {} too small to print sharply.",
            photo_count(too_small),
            if too_small == 1 {
                "it was"
            } else {
                "they were"
            }
        ));
    }
    if unavailable > 0 {
        notes.push(format!(
            "{} left out because {} file was missing, changed or unreadable.",
            photo_count(unavailable),
            if unavailable == 1 { "its" } else { "each" }
        ));
    }
    notes.extend(fit);
    notes
}

fn photo_count(n: usize) -> String {
    if n == 1 {
        "1 photo was".to_string()
    } else {
        format!("{n} photos were")
    }
}

/// "K of N photos ... to fit the C-page cap" as advice.
fn fit_note(rest: &str) -> Option<String> {
    let mut words = rest.split_whitespace();
    let kept = words.next()?;
    let of = words.nth(1)?;
    let cap = rest
        .rsplit_once("to fit the ")?
        .1
        .strip_suffix("-page cap")?;
    Some(format!(
        "The book uses {kept} of {of} photos to stay within {cap} pages. Allow more pages to fit more."
    ))
}

/// A skipped keepsake's reason, with the book's print-check block reworded.
pub fn plain_skip(mut skip: Skipped) -> Skipped {
    if let Some(issue) = skip
        .reason
        .strip_prefix("preflight/render blocked: book preflight blocked: ")
    {
        skip.reason = format!("It didn't pass its print check: {issue}.");
    }
    skip
}

/// Whether one keepsake can be made from the trip, before any run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeepsakeEligibility {
    pub enabled: bool,
    /// Why it can't be made; for a film that will be the short version, why
    /// it is short. Empty when there is nothing to say.
    pub reason: String,
}

/// What the Export screen may offer, keepsake by keepsake (PLAN.md Sec 5.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EligibilityView {
    pub video: KeepsakeEligibility,
    pub reels: KeepsakeEligibility,
    pub book: KeepsakeEligibility,
}

/// The plan's eligibility, counted exactly as `export::export_all` counts it
/// (distinct film assets, every non-video asset, one moment per chapter), so
/// the screen disables exactly what a run would skip, with the same reasons.
pub fn export_eligibility(plan: &Plan, assets: &[Asset]) -> EligibilityView {
    let usable = distinct_assets(&plan.hero);
    let photos = assets.iter().filter(|a| a.kind != AssetKind::Video).count();
    let e = triptape_core::plan::plan_eligibility(usable, photos, plan.chapters.len());
    let card = |enabled: bool, reason: String, say_why: bool| KeepsakeEligibility {
        enabled,
        reason: if say_why { reason } else { String::new() },
    };
    EligibilityView {
        video: card(e.video != "disabled", e.video_reason, e.video != "full"),
        reels: card(e.reels > 0, e.reels_reason, e.reels == 0),
        book: card(e.book_enabled, e.book_reason, !e.book_enabled),
    }
}

/// One book shape the owner can pick, with the page limits the field follows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookPresetView {
    /// `P1` or `P2`, as the export commands take it.
    pub id: &'static str,
    pub name: &'static str,
    pub min_pages: usize,
    /// The page cap used when the owner doesn't set one.
    pub default_pages: usize,
    pub max_pages: usize,
}

/// The book shapes on offer (PLAN.md Sec 11: only two in v1), P1 first.
pub fn book_presets() -> Vec<BookPresetView> {
    [(BookPreset::P1, "P1"), (BookPreset::P2, "P2")]
        .into_iter()
        .map(|(preset, id)| {
            let (min_pages, default_pages) = preset.page_range();
            BookPresetView {
                id,
                name: preset.name(),
                min_pages,
                default_pages,
                max_pages: MAX_BOOK_PAGES,
            }
        })
        .collect()
}

/// The book shape an export asked for; omitted means P1 (Classic Square).
pub fn parse_book_preset(id: Option<&str>) -> Result<BookPreset, String> {
    match id {
        None => Ok(BookPreset::P1),
        Some(id) => BookPreset::from_flag(id).ok_or_else(|| format!("unknown book shape {id:?}")),
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested without a Tauri runtime)
// ---------------------------------------------------------------------------

/// Foreground metadata fill for desktop ingest (the Tauri shell's
/// `scan_folder`): EXIF capture time / TZ / GPS / rotation / dims / caption /
/// people count for photos, ffprobe duration / dims / rotation / GPS / capture time for
/// videos. Mirrors the CLI mapping in `triptape-media`'s `triptape` binary
/// minus pixel-decode work (no phash, no decoded-image dimensions): EXIF
/// reads are header-only and each video costs one ffprobe spawn.
///
/// Per-file fault-tolerant: unreadable EXIF or a failed probe falls back to
/// file mtime with `time_guessed = true` and never fails the scan.
///
/// Story fields (caption, people, library score) come from in-file XMP,
/// then the XMP sidecar, then `library` (the folder's osxphotos export
/// database, when present). `salt` (the project id) salts the people
/// signature.
pub fn fill_foreground_metadata(a: &mut Asset, library: Option<&OsxPhotosIndex>, salt: &str) {
    if a.kind == AssetKind::Video {
        let probe = metadata::probe_video("ffprobe", &a.path).ok();
        metadata::fill_video(a, probe.as_ref(), library, salt);
    } else {
        let exif = metadata::read_image_exif(&a.path);
        metadata::fill_photo(a, &exif, library, salt);
    }
}

/// Upper bound on foreground metadata workers. EXIF reads are header-only
/// and fast, but every video costs one ffprobe spawn, so this stays above
/// the scoring worker count without fork-storming hundreds of probes.
pub const FOREGROUND_FILL_WORKERS_MAX: usize = 8;

/// Fill foreground metadata for every walked file on a bounded worker pool.
/// Pure per-file work: EXIF/ffprobe reads plus struct fill, no Db access,
/// so workers never contend on the store lock. `on_done(done, total)` runs
/// on the calling thread once per completed file for progress.
///
/// Walk order is restored before returning, so index-based asset ids
/// (`{pid}_a{i:06}`) are identical to the sequential fill and Story edits
/// keyed by those ids keep applying across rescans. `library` is the
/// folder's osxphotos export index (see [`fill_foreground_metadata`]).
pub fn fill_assets_parallel(
    pid: &str,
    files: &[ScannedFile],
    library: Option<&OsxPhotosIndex>,
    mut on_done: impl FnMut(usize, usize),
) -> Vec<Asset> {
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(FOREGROUND_FILL_WORKERS_MAX);
    let mut pairs: Vec<(usize, Asset)> = Vec::with_capacity(files.len());
    triptape_core::parallel::for_each_parallel(
        files,
        workers,
        |f| {
            let kind = if VIDEO_EXTS.contains(&f.ext.as_str()) {
                AssetKind::Video
            } else {
                AssetKind::Photo
            };
            let mut a = Asset::new(String::new(), f.path.clone(), kind);
            a.size_bytes = f.size_bytes;
            a.mtime = f.mtime_secs;
            fill_foreground_metadata(&mut a, library, pid);
            a
        },
        |idx, mut asset, done, total| {
            asset.id = format!("{pid}_a{idx:06}");
            pairs.push((idx, asset));
            on_done(done, total);
        },
    );
    pairs.sort_by_key(|(idx, _)| *idx);
    pairs.into_iter().map(|(_, a)| a).collect()
}

/// Live Photo pairing for desktop ingest (`scan_folder`): fold each still +
/// short same-stem motion pair into one [`AssetKind::LivePhoto`] row.
///
/// Mirrors the CLI (`triptape-media`'s `triptape` binary) semantics: the
/// still absorbs its motion file (kind becomes LivePhoto, `companion_path`
/// set, `pairing_confidence` set, `duration_secs` taken from the motion
/// asset when the still lacks one) and the motion-only row is dropped.
/// Capture time and every other still field are preserved, as is the
/// original asset order.
///
/// Inputs come from already-probed assets, not disk: Photo-kind assets are
/// stills, Video-kind assets are motions with `has_video_stream` true only
/// when a duration was probed (an unknown duration must never merge, per
/// the engine rules). Still content ids are unavailable here, so Tier 2
/// (same directory + same stem + MOV at most 3.5 s) is the effective path;
/// the engine's duration/conflict rules are relied on exactly, never
/// loosened.
pub fn apply_live_photo_pairing(assets: Vec<Asset>) -> Vec<Asset> {
    use triptape_core::livephoto::{self, MotionFile, StillFile};

    let stills: Vec<StillFile> = assets
        .iter()
        .filter(|a| a.kind == AssetKind::Photo)
        .map(|a| StillFile {
            path: a.path.clone(),
            content_id: None,
        })
        .collect();
    let motions: Vec<MotionFile> = assets
        .iter()
        .filter(|a| a.kind == AssetKind::Video)
        .map(|a| MotionFile {
            path: a.path.clone(),
            content_id: None,
            duration_secs: a.duration_secs,
            // No stream table on `Asset`: a probed duration is the only
            // motion evidence we have; unknown duration must never merge.
            has_video_stream: a.duration_secs.is_some(),
        })
        .collect();
    if stills.is_empty() || motions.is_empty() {
        return assets;
    }
    let pairing = livephoto::pair_live_photos(stills, motions);
    if pairing.pairs.is_empty() {
        return assets;
    }
    let motion_duration: HashMap<&std::path::Path, Option<f64>> = pairing
        .pairs
        .iter()
        .map(|p| {
            let duration = assets
                .iter()
                .find(|a| a.path == p.motion)
                .and_then(|a| a.duration_secs);
            (p.motion.as_path(), duration)
        })
        .collect();
    let paired_motion: HashMap<PathBuf, (&PathBuf, triptape_core::types::PairingConfidence)> =
        pairing
            .pairs
            .iter()
            .map(|p| (p.still.clone(), (&p.motion, p.confidence)))
            .collect();
    assets
        .into_iter()
        .filter_map(|mut a| {
            if a.kind == AssetKind::Video && motion_duration.contains_key(a.path.as_path()) {
                return None;
            }
            if let Some((motion, confidence)) = paired_motion.get(&a.path) {
                a.kind = AssetKind::LivePhoto;
                a.companion_path = Some((*motion).clone());
                a.pairing_confidence = Some(*confidence);
                if a.duration_secs.is_none() {
                    a.duration_secs = motion_duration.get(motion.as_path()).copied().flatten();
                }
            }
            Some(a)
        })
        .collect()
}

/// Borrow one page of `items`; page is 0-based, `per_page` clamped to 1..=500.
pub fn paginate<T>(items: &[T], page: usize, per_page: usize) -> &[T] {
    let per = per_page.clamp(1, 500);
    items.chunks(per).nth(page).unwrap_or(&[])
}

/// Default export destination for a trip: `~/Movies/TripTape/<trip> (<project id>)/`
/// (Windows: `%USERPROFILE%\\Videos\\TripTape\\...`). The project id keeps two
/// trips with the same folder name from overwriting each other's output.
pub fn default_export_dir(trip_name: &str, project_id: &str) -> PathBuf {
    export_dir_in(&default_export_parent(), trip_name, project_id)
}

/// The folder trips export into unless the owner picks another one.
pub fn default_export_parent() -> PathBuf {
    dirs_base().join("TripTape")
}

/// One trip's export folder inside `parent`. Every trip gets its own
/// subfolder, so a picked folder's own `book/` or `reels/` is never replaced.
pub fn export_dir_in(parent: &Path, trip_name: &str, project_id: &str) -> PathBuf {
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
    parent.join(format!("{safe} ({project_id})"))
}

/// The folder an export goes into: the owner's pick (from the folder dialog),
/// else the default. A pick must be an absolute path to a folder TripTape can
/// write to, or one it can create inside a writable folder. Nothing is
/// created here; the export makes the folders it needs.
pub fn resolve_export_parent(picked: Option<&str>) -> Result<PathBuf, String> {
    let Some(raw) = picked.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(default_export_parent());
    };
    let unusable = "TripTape can't save keepsakes in that folder. Choose another folder.";
    if raw.contains('\0') {
        return Err(unusable.to_string());
    }
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err(unusable.to_string());
    }
    // The nearest part of the path that exists must be a writable folder.
    let existing = path
        .ancestors()
        .find(|p| p.exists())
        .ok_or_else(|| unusable.to_string())?;
    if !existing.is_dir() || !folder_is_writable(existing) {
        return Err(unusable.to_string());
    }
    Ok(path)
}

/// Whether a file can be created (and removed) in `dir`. Probes for real:
/// permission bits miss read-only volumes and sandbox rules.
fn folder_is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".triptape-write-check-{}", std::process::id()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(f) => {
            drop(f);
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Validate a "Play film" request: only a finished film (`hero.mp4` beside its
/// `export.json`) may be opened, wherever the owner saved it. The opener's
/// own scope only covers the default folder.
pub fn finished_film(path: &str) -> Result<PathBuf, String> {
    let p = PathBuf::from(path);
    let named_film = p.file_name().is_some_and(|n| n == "hero.mp4");
    let finished = p
        .parent()
        .is_some_and(|dir| dir.join("export.json").is_file());
    if p.is_absolute() && named_film && p.is_file() && finished {
        Ok(p)
    } else {
        Err("that file is not a finished TripTape film".to_string())
    }
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
    fn story_include_requires_a_chapter_member() {
        let (db, pid) = story_db(&["a", "b"]);
        let data = StoryData::load(&db, &pid).unwrap();
        assert!(data.check_include("b").is_ok());
        assert_eq!(
            data.check_include("missing").unwrap_err(),
            "asset is not in any chapter"
        );
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
        // `place` is an Option like the other nullable fields: absent GPS
        // serializes as null, never as an empty string.
        assert!(json.get("place").is_some());
        assert!(json["place"].is_null());
    }

    #[test]
    fn asset_view_place_names_nearby_gps() {
        let mut a = Asset::new("kyoto", PathBuf::from("/t/kyoto.jpg"), AssetKind::Photo);
        a.gps = Some((34.9671, 135.7727));
        let view = AssetView::from(&a);
        assert!(view.has_gps);
        assert_eq!(view.place.as_deref(), Some("Fushimi Inari-taisha"));

        let mut b = Asset::new("nogps", PathBuf::from("/t/nogps.jpg"), AssetKind::Photo);
        b.gps = None;
        let plain = AssetView::from(&b);
        assert_eq!(plain.place, None);
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
    fn foreground_fill_without_exif_falls_back_to_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nometa.jpg");
        std::fs::write(&path, b"not an image at all").unwrap();
        let mut a = Asset::new("a1", path.clone(), AssetKind::Photo);
        a.size_bytes = 19;
        a.mtime = 1_700_000_000;
        fill_foreground_metadata(&mut a, None, "p");
        assert_eq!(a.taken_at, Some(1_700_000_000));
        assert!(a.time_guessed);
        assert_eq!(a.gps, None);
    }

    #[test]
    fn foreground_fill_unprobeable_video_falls_back_to_mtime() {
        // Missing file: ffprobe (present or not) cannot probe it, exactly
        // like the CLI's unprobeable-video path.
        let dir = tempfile::tempdir().unwrap();
        let mut a = Asset::new("v1", dir.path().join("gone.mp4"), AssetKind::Video);
        a.size_bytes = 10;
        a.mtime = 1_700_000_001;
        fill_foreground_metadata(&mut a, None, "p");
        assert_eq!(a.taken_at, Some(1_700_000_001));
        assert!(a.time_guessed);
        assert_eq!(a.duration_secs, None);
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
    fn export_parent_defaults_and_rejects_unusable_picks() {
        assert_eq!(
            resolve_export_parent(None).unwrap(),
            default_export_parent()
        );
        assert_eq!(
            resolve_export_parent(Some("  ")).unwrap(),
            default_export_parent()
        );
        assert!(resolve_export_parent(Some("relative/folder")).is_err());
        assert!(resolve_export_parent(Some("/tmp/a\0b")).is_err());
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a-file");
        std::fs::write(&file, b"x").unwrap();
        // A file is not a folder, and nothing can be created under it.
        assert!(resolve_export_parent(Some(file.to_str().unwrap())).is_err());
        assert!(resolve_export_parent(Some(file.join("sub").to_str().unwrap())).is_err());
    }

    #[test]
    fn export_parent_accepts_writable_and_creatable_folders() {
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path().to_str().unwrap();
        assert_eq!(resolve_export_parent(Some(here)).unwrap(), dir.path());
        let later = dir.path().join("not yet").join("made");
        assert_eq!(
            resolve_export_parent(Some(later.to_str().unwrap())).unwrap(),
            later
        );
        // The check leaves nothing behind.
        assert!(!later.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn export_parent_rejects_read_only_folder() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let ro = dir.path().join("ro");
        std::fs::create_dir(&ro).unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Root ignores permission bits; the check is only meaningful otherwise.
        if !folder_is_writable(&ro) {
            assert!(resolve_export_parent(Some(ro.to_str().unwrap())).is_err());
        }
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn export_dir_lives_inside_the_chosen_folder() {
        let d = export_dir_in(Path::new("/Volumes/Photos"), "Rome", "prj_1");
        assert_eq!(d, Path::new("/Volumes/Photos/Rome (prj_1)"));
        assert!(default_export_dir("Rome", "prj_1").starts_with(default_export_parent()));
    }

    #[test]
    fn only_a_finished_film_may_be_played() {
        let dir = tempfile::tempdir().unwrap();
        let film = dir.path().join("hero.mp4");
        std::fs::write(&film, b"film").unwrap();
        // Not finished yet: no export.json beside it.
        assert!(finished_film(film.to_str().unwrap()).is_err());
        std::fs::write(dir.path().join("export.json"), b"{}").unwrap();
        assert_eq!(finished_film(film.to_str().unwrap()).unwrap(), film);
        let other = dir.path().join("notes.txt");
        std::fs::write(&other, b"x").unwrap();
        assert!(finished_film(other.to_str().unwrap()).is_err());
        assert!(finished_film("hero.mp4").is_err());
    }

    fn photos(n: usize) -> Vec<Asset> {
        (0..n)
            .map(|i| {
                let mut a = Asset::new(
                    format!("p{i:03}"),
                    PathBuf::from(format!("/t/p{i:03}.jpg")),
                    AssetKind::Photo,
                );
                // One photo an hour over a few days, so chapters form.
                a.taken_at = Some(1_714_557_600 + i as i64 * 3_600);
                a
            })
            .collect()
    }

    fn plan_for(assets: &[Asset]) -> Plan {
        let cluster_of = assets
            .iter()
            .map(|a| (a.id.clone(), format!("day{}", a.taken_at.unwrap() / 86_400)))
            .collect();
        build_plan(assets, &cluster_of, &HashMap::new())
    }

    #[test]
    fn eligibility_disables_the_book_with_its_reason_on_a_thin_trip() {
        let assets = photos(12);
        let e = export_eligibility(&plan_for(&assets), &assets);
        assert!(!e.book.enabled);
        assert_eq!(
            e.book.reason,
            triptape_core::plan::plan_eligibility(0, 12, 0).book_reason
        );
        // A short film is still a film; the reason says why it is short.
        assert!(e.video.enabled);
        assert!(e.video.reason.contains("short"));
        assert!(e.reels.enabled);
        assert!(e.reels.reason.is_empty());
    }

    #[test]
    fn eligibility_disables_everything_on_an_empty_plan() {
        let e = export_eligibility(&plan_for(&[]), &[]);
        assert!(!e.video.enabled && !e.reels.enabled && !e.book.enabled);
        for card in [&e.video, &e.reels, &e.book] {
            assert!(!card.reason.is_empty());
        }
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["book"]["enabled"], false);
    }

    #[test]
    fn eligibility_enables_the_book_for_enough_photos() {
        let assets = photos(40);
        let e = export_eligibility(&plan_for(&assets), &assets);
        assert!(e.book.enabled);
        assert!(e.book.reason.is_empty());
    }

    #[test]
    fn book_presets_follow_the_renderer_and_default_to_p1() {
        let presets = book_presets();
        assert_eq!(
            presets.iter().map(|p| p.id).collect::<Vec<_>>(),
            ["P1", "P2"]
        );
        for p in &presets {
            let preset = parse_book_preset(Some(p.id)).unwrap();
            assert_eq!((p.min_pages, p.default_pages), preset.page_range());
            assert_eq!(preset.page_cap(None).unwrap(), p.default_pages);
            assert!(preset.page_cap(Some(p.max_pages)).is_ok());
            assert!(preset.page_cap(Some(p.max_pages + 1)).is_err());
            assert!(preset.page_cap(Some(p.min_pages - 1)).is_err());
        }
        assert_eq!(presets[1].default_pages, 80);
        assert_eq!(parse_book_preset(None).unwrap(), BookPreset::P1);
        assert_eq!(parse_book_preset(Some("p2")).unwrap(), BookPreset::P2);
        assert!(parse_book_preset(Some("P3")).is_err());
        let json = serde_json::to_value(&presets[0]).unwrap();
        assert_eq!(json["defaultPages"], 60);
    }

    #[test]
    fn book_notes_group_the_print_check_in_plain_words() {
        use triptape_media::render_book::{preflight, DpiVerdict};
        let soft = preflight(BookPreset::P1, 60, &[], &[(3, DpiVerdict::Warn)])
            .into_iter()
            .find(|i| i.level == "warn")
            .unwrap()
            .message;
        let fallbacks: Vec<String> = vec![
            format!("page 12: {soft}"),
            format!("page 3: {soft}"),
            format!("page 3: {soft}"),
            "page 4: something else".into(),
            "book: p001 excluded (below 200 dpi even in the smallest slot)".into(),
            "book: p002 excluded (below 200 dpi at its assigned slot)".into(),
            "book: p003 is missing; offer Relink, Rescan, or Skip; skipped".into(),
            "book: selected 120 of 180 photos (your captioned photos first, then the best-scored of each chapter) to fit the 60-page cap".into(),
            "video: v1 changed on disk; skipped".into(),
            "map card failed: x".into(),
        ];
        assert_eq!(
            book_notes(&fallbacks),
            [
                "2 pages have a photo at 200-299 dpi and may print a little soft: pages 3, 12.",
                "2 photos were left out because they were too small to print sharply.",
                "1 photo was left out because its file was missing, changed or unreadable.",
                "The book uses 120 of 180 photos to stay within 60 pages. Allow more pages to fit more.",
            ]
        );
        assert_eq!(
            book_notes(&[format!("page 7: {soft}")]),
            ["Page 7 has a photo at 200-299 dpi. It may print a little soft."]
        );
        assert!(book_notes(&[]).is_empty());
    }

    #[test]
    fn a_print_check_block_reads_as_plain_words() {
        let blocked = plain_skip(Skipped {
            keepsake: "book".into(),
            reason: "preflight/render blocked: book preflight blocked: 20 pages below the 24-page vendor minimum; add photos (18 photos)".into(),
        });
        assert_eq!(
            blocked.reason,
            "It didn't pass its print check: 20 pages below the 24-page vendor minimum; add photos (18 photos)."
        );
        let other = plain_skip(Skipped {
            keepsake: "video".into(),
            reason: "not selected (--only book)".into(),
        });
        assert_eq!(other.reason, "not selected (--only book)");
    }

    #[test]
    fn only_known_job_kinds() {
        assert!(valid_job_kind("export-all"));
        assert!(!valid_job_kind("rm -rf"));
        assert!(!valid_job_kind(""));
    }

    fn pairing_asset(id: &str, path: &str, kind: AssetKind, duration: Option<f64>) -> Asset {
        let mut a = Asset::new(id, PathBuf::from(path), kind);
        a.taken_at = Some(1_714_557_600);
        a.duration_secs = duration;
        a
    }

    #[test]
    fn pairing_folds_short_mov_into_live_photo() {
        let still = pairing_asset("s", "/t/IMG_101.HEIC", AssetKind::Photo, None);
        let mov = pairing_asset("m", "/t/IMG_101.MOV", AssetKind::Video, Some(2.0));
        let out = apply_live_photo_pairing(vec![still, mov]);
        assert_eq!(out.len(), 1, "motion row must be dropped");
        let lp = &out[0];
        assert_eq!(lp.id, "s", "the surviving row is the still");
        assert_eq!(lp.kind, AssetKind::LivePhoto);
        assert_eq!(
            lp.companion_path.as_deref(),
            Some(std::path::Path::new("/t/IMG_101.MOV"))
        );
        assert!(lp.pairing_confidence.is_some());
        assert_eq!(
            lp.duration_secs,
            Some(2.0),
            "duration comes from the motion"
        );
        assert_eq!(lp.taken_at, Some(1_714_557_600), "still capture time kept");
    }

    #[test]
    fn pairing_keeps_long_mov_as_separate_video() {
        let still = pairing_asset("s", "/t/IMG_102.HEIC", AssetKind::Photo, None);
        let mov = pairing_asset("m", "/t/IMG_102.MOV", AssetKind::Video, Some(30.0));
        let out = apply_live_photo_pairing(vec![still, mov]);
        assert_eq!(out.len(), 2, "a 30s same-stem MOV is a real video");
        assert_eq!(out[0].kind, AssetKind::Photo);
        assert_eq!(out[1].kind, AssetKind::Video);
        assert_eq!(out[1].companion_path, None);
    }

    #[test]
    fn pairing_keeps_different_stem_mov_separate() {
        let still = pairing_asset("s", "/t/IMG_103.HEIC", AssetKind::Photo, None);
        let mov = pairing_asset("m", "/t/IMG_104.MOV", AssetKind::Video, Some(2.0));
        let out = apply_live_photo_pairing(vec![still, mov]);
        assert_eq!(out.len(), 2, "different stems never pair");
        assert!(out.iter().all(|a| a.kind != AssetKind::LivePhoto));
    }

    #[test]
    fn pairing_keeps_unknown_duration_mov_separate() {
        let still = pairing_asset("s", "/t/IMG_105.HEIC", AssetKind::Photo, None);
        let mov = pairing_asset("m", "/t/IMG_105.MOV", AssetKind::Video, None);
        let out = apply_live_photo_pairing(vec![still, mov]);
        assert_eq!(out.len(), 2, "unknown-duration MOV must never merge");
        assert!(out.iter().all(|a| a.kind != AssetKind::LivePhoto));
    }

    fn scanned_file(path: &std::path::Path, ext: &str) -> triptape_core::scan::ScannedFile {
        let md = std::fs::metadata(path).unwrap();
        let mtime = md
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        triptape_core::scan::ScannedFile {
            path: path.to_path_buf(),
            size_bytes: md.len(),
            mtime_secs: mtime,
            ext: ext.to_string(),
        }
    }

    fn sequential_fill(pid: &str, files: &[triptape_core::scan::ScannedFile]) -> Vec<Asset> {
        let mut out = Vec::with_capacity(files.len());
        for (i, f) in files.iter().enumerate() {
            let kind = if triptape_core::scan::VIDEO_EXTS.contains(&f.ext.as_str()) {
                AssetKind::Video
            } else {
                AssetKind::Photo
            };
            let mut a = Asset::new(format!("{pid}_a{i:06}"), f.path.clone(), kind);
            a.size_bytes = f.size_bytes;
            a.mtime = f.mtime_secs;
            fill_foreground_metadata(&mut a, None, "p");
            out.push(a);
        }
        out
    }

    fn asset_signature(a: &Asset) -> (Option<i64>, bool, Option<u32>, Option<f64>, String) {
        (
            a.taken_at,
            a.time_guessed,
            a.width,
            a.duration_secs,
            format!("{:?}", a.kind),
        )
    }

    #[test]
    fn parallel_fill_matches_sequential_fill_in_walk_order() {
        let dir = tempfile::tempdir().unwrap();
        // EXIF-less PNGs (mtime fallback) plus undecodable MOVs (probe
        // fallback): both fallbacks are deterministic, so any divergence
        // between the two fill paths is a parallelization bug, not data.
        let mut files = Vec::new();
        for i in 0..8 {
            let path = dir.path().join(format!("img_{i}.png"));
            image::RgbImage::new(4, 4).save(&path).unwrap();
            files.push(scanned_file(&path, "png"));
        }
        for i in 0..4 {
            let path = dir.path().join(format!("clip_{i}.mov"));
            std::fs::write(&path, b"not a movie").unwrap();
            files.push(scanned_file(&path, "mov"));
        }
        let seq = sequential_fill("p", &files);
        let par = fill_assets_parallel("p", &files, None, |_, _| {});
        assert_eq!(par.len(), seq.len(), "no file lost in the parallel fill");
        assert_eq!(
            par.iter().map(|a| a.id.clone()).collect::<Vec<_>>(),
            seq.iter().map(|a| a.id.clone()).collect::<Vec<_>>(),
            "walk order (and index-based ids) preserved"
        );
        for (s, p) in seq.iter().zip(par.iter()) {
            assert_eq!(
                asset_signature(s),
                asset_signature(p),
                "metadata diverges for {}",
                s.path.display()
            );
        }
    }
}
