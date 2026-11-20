//! The Settings screen's backend (PLAN.md Sec 14, R4): home on maps, the
//! caches, the licences, and the redacted log with its diagnostic report.
//!
//! Self-contained: `main.rs` calls [`setup`] once, registers the commands
//! below, and asks [`export_settings`] what an export should honour.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;
use tauri::Manager;
use triptape_core::applog;
use triptape_core::privacy::Redactor;
use triptape_core::process::ToolSource;
use triptape_core::settings::{check_segment_cache_cap, Home, Settings};
use triptape_media::segment_cache;

use crate::{tool_status, tools, AppState};

/// Loaded settings and where they live.
pub struct SettingsState {
    path: PathBuf,
    log_dir: PathBuf,
    current: Mutex<Settings>,
}

impl SettingsState {
    fn get(&self) -> Result<Settings, String> {
        self.current
            .lock()
            .map(|s| s.clone())
            .map_err(|e| e.to_string())
    }

    /// Change the settings and save them; nothing changes if saving fails.
    fn update(&self, f: impl FnOnce(&mut Settings)) -> Result<Settings, String> {
        let mut current = self.current.lock().map_err(|e| e.to_string())?;
        let mut next = current.clone();
        f(&mut next);
        next.save(&self.path)?;
        *current = next.clone();
        Ok(next)
    }
}

/// Load the settings, start the redacted log, and manage both.
pub fn setup(app: &tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let data = app.path().app_data_dir()?;
    let path = data.join("settings.json");
    let mut settings = Settings::load(&path);
    if settings.ensure_log_salt() {
        // A failed save only means a new salt next launch.
        let _ = settings.save(&path);
    }
    let log_dir = app.path().app_log_dir()?;
    let redactor = Redactor::new(
        app.path().home_dir().ok().as_deref(),
        settings.log_salt.as_deref().unwrap_or_default(),
    );
    applog::init(&log_dir, redactor);
    applog::info(&format!(
        "TripTape {} started ({} {})",
        app.package_info().version,
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    app.manage(SettingsState {
        path,
        log_dir,
        current: Mutex::new(settings),
    });
    Ok(())
}

/// What an export honours: the home to hide and the segment cache cap.
pub fn export_settings(app: &tauri::AppHandle) -> Settings {
    app.try_state::<SettingsState>()
        .and_then(|s| s.get().ok())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Home on maps
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PrivacyView {
    pub home: Option<Home>,
    pub hide_home: bool,
}

impl From<&Settings> for PrivacyView {
    fn from(s: &Settings) -> Self {
        Self {
            home: s.home,
            hide_home: s.hide_home,
        }
    }
}

#[tauri::command]
pub async fn get_privacy_settings(
    state: tauri::State<'_, SettingsState>,
) -> Result<PrivacyView, String> {
    Ok(PrivacyView::from(&state.get()?))
}

/// Set home (both numbers) or forget it (neither). Hiding switches on with
/// a new home: the default whenever one is set.
#[tauri::command]
pub async fn set_home(
    state: tauri::State<'_, SettingsState>,
    lat: Option<f64>,
    lon: Option<f64>,
) -> Result<PrivacyView, String> {
    let home = match (lat, lon) {
        (Some(lat), Some(lon)) => Some(Home::new(lat, lon)?),
        (None, None) => None,
        _ => return Err("Enter both a latitude and a longitude.".into()),
    };
    let s = state.update(|s| {
        if home.is_some() && s.home != home {
            s.hide_home = true;
        }
        s.home = home;
    })?;
    applog::info(if home.is_some() {
        "settings: home set"
    } else {
        "settings: home cleared"
    });
    Ok(PrivacyView::from(&s))
}

#[tauri::command]
pub async fn set_hide_home(
    state: tauri::State<'_, SettingsState>,
    hide: bool,
) -> Result<PrivacyView, String> {
    let s = state.update(|s| s.hide_home = hide)?;
    applog::info(&format!("settings: hide home on maps {hide}"));
    Ok(PrivacyView::from(&s))
}

/// Where the trip starts or ends on its route, to fill in as home. Only
/// read here; the owner still saves it. No geocoding, no network.
#[tauri::command]
pub async fn trip_end_as_home(
    state: tauri::State<'_, AppState>,
    project_id: String,
    which: String,
) -> Result<Home, String> {
    state.require_project(&project_id)?;
    let db = state.db.lock().map_err(|e| e.to_string())?;
    let plan = db
        .edited_plan(Some(&project_id))?
        .ok_or_else(|| "This trip has no story yet.".to_string())?;
    route_end(&plan.edited.map_polyline, &which)
}

/// The first or last point of a route.
pub fn route_end(route: &[(f64, f64)], which: &str) -> Result<Home, String> {
    let point = match which {
        "first" => route.first(),
        "last" => route.last(),
        _ => return Err("want first or last".into()),
    };
    let (lat, lon) = point
        .copied()
        .ok_or_else(|| "None of this trip's photos has a location.".to_string())?;
    Home::new(lat, lon)
}

// ---------------------------------------------------------------------------
// Caches
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CacheInfo {
    /// Library thumbnails and preview proxies.
    pub thumbnail_bytes: u64,
    pub thumbnail_files: u64,
    /// Rendered film pieces reused by later exports.
    pub segment_bytes: u64,
    pub segment_entries: usize,
    /// Pieces a running export is using right now.
    pub segment_in_use: usize,
    /// The owner's limit; `None` means the default.
    pub segment_cap_bytes: Option<u64>,
    pub default_segment_cap_bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ClearResult {
    pub removed: u64,
    pub freed_bytes: u64,
    /// Entries kept because an export is using them.
    pub kept: u64,
}

/// The app's cache folders: the root and the thumbnail and proxy folders.
fn cache_root(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    app.path().app_cache_dir().map_err(|e| e.to_string())
}

const THUMB_DIRS: [&str; 2] = ["thumb", "proxy"];

/// The segment cache the exports use (see `export_blocking`).
fn segment_dir() -> Result<PathBuf, String> {
    segment_cache::default_dir().ok_or_else(|| "no cache folder on this computer".to_string())
}

#[tauri::command]
pub async fn get_cache_info(
    app: tauri::AppHandle,
    state: tauri::State<'_, SettingsState>,
) -> Result<CacheInfo, String> {
    let root = cache_root(&app)?;
    let cap = state.get()?.segment_cache_max_bytes;
    tauri::async_runtime::spawn_blocking(move || {
        let (mut thumbnail_bytes, mut thumbnail_files) = (0, 0);
        for d in THUMB_DIRS {
            let (b, f) = dir_size(&root.join(d));
            thumbnail_bytes += b;
            thumbnail_files += f;
        }
        let seg = segment_dir()
            .map(|d| segment_cache::disk_stats(&d))
            .unwrap_or_default();
        CacheInfo {
            thumbnail_bytes,
            thumbnail_files,
            segment_bytes: seg.bytes,
            segment_entries: seg.entries,
            segment_in_use: seg.live_leases,
            segment_cap_bytes: cap,
            default_segment_cap_bytes: segment_cache::DEFAULT_CAP_BYTES,
        }
    })
    .await
    .map_err(|e| e.to_string())
}

/// Set the segment cache limit in bytes, or `None` for the default. Takes
/// effect from the next export, which evicts down to it when it finishes.
#[tauri::command]
pub async fn set_segment_cache_cap(
    state: tauri::State<'_, SettingsState>,
    bytes: Option<u64>,
) -> Result<Option<u64>, String> {
    let cap = bytes.map(check_segment_cache_cap).transpose()?;
    let s = state.update(|s| s.segment_cache_max_bytes = cap)?;
    applog::info(&format!(
        "settings: segment cache limit {}",
        cap.map_or("default".into(), segment_cache::fmt_bytes)
    ));
    Ok(s.segment_cache_max_bytes)
}

#[tauri::command]
pub async fn clear_thumbnail_cache(app: tauri::AppHandle) -> Result<ClearResult, String> {
    let root = cache_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let mut total = ClearResult::default();
        for d in THUMB_DIRS {
            let r = clear_dir_within(&root.join(d), &root)?;
            total.removed += r.removed;
            total.freed_bytes += r.freed_bytes;
        }
        applog::info(&format!(
            "cache: cleared thumbnails, {} files, {}",
            total.removed,
            segment_cache::fmt_bytes(total.freed_bytes)
        ));
        Ok(total)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Clear the segment cache. Entries a running export uses are kept.
#[tauri::command]
pub async fn clear_segment_cache(app: tauri::AppHandle) -> Result<ClearResult, String> {
    let root = cache_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let dir = segment_dir()?;
        if !dir.exists() {
            return Ok(ClearResult::default());
        }
        check_within(&dir, &root)?;
        let r = segment_cache::clear(&dir);
        applog::info(&format!(
            "cache: cleared segments, {} entries, {}, {} kept in use",
            r.removed,
            segment_cache::fmt_bytes(r.freed),
            r.protected
        ));
        Ok(ClearResult {
            removed: r.removed as u64,
            freed_bytes: r.freed,
            kept: r.protected as u64,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Bytes and files under `dir`, never following links.
pub fn dir_size(dir: &Path) -> (u64, u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    let (mut bytes, mut files) = (0, 0);
    for e in entries.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            let (b, f) = dir_size(&e.path());
            bytes += b;
            files += f;
        } else if ft.is_file() {
            bytes += e.metadata().map_or(0, |m| m.len());
            files += 1;
        }
    }
    (bytes, files)
}

/// `dir`, resolved, when it sits strictly inside `root` (resolved too).
/// Anything else is refused: a cache clear never reaches outside the app's
/// cache folder, whatever a link or a `..` says.
pub fn check_within(dir: &Path, root: &Path) -> Result<PathBuf, String> {
    let refuse =
        || "That folder is not inside TripTape's cache folder, so it was left alone.".to_string();
    let root = root.canonicalize().map_err(|_| refuse())?;
    let dir = dir.canonicalize().map_err(|_| refuse())?;
    if dir == root || !dir.starts_with(&root) {
        return Err(refuse());
    }
    Ok(dir)
}

/// Empty `dir` (which must be inside `root`), keeping `dir` itself. Links
/// inside are removed as links; what they point to is never touched.
pub fn clear_dir_within(dir: &Path, root: &Path) -> Result<ClearResult, String> {
    let mut r = ClearResult::default();
    if std::fs::symlink_metadata(dir).is_err() {
        return Ok(r);
    }
    let dir = check_within(dir, root)?;
    remove_children(&dir, &mut r);
    Ok(r)
}

fn remove_children(dir: &Path, r: &mut ClearResult) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        // `file_type` comes from the directory entry: it describes a link
        // itself, never its target.
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_symlink() {
            // A link to a folder is a directory entry on Windows.
            if std::fs::remove_file(&path).is_err() {
                let _ = std::fs::remove_dir(&path);
            }
        } else if ft.is_dir() {
            remove_children(&path, r);
            let _ = std::fs::remove_dir(&path);
        } else {
            let len = e.metadata().map_or(0, |m| m.len());
            if std::fs::remove_file(&path).is_ok() {
                r.removed += 1;
                r.freed_bytes += len;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Licences
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LicenseView {
    pub id: String,
    pub title: String,
    pub summary: String,
    /// Full licence text, as bundled.
    pub text: String,
    /// Where to get the source code, and the project pages.
    pub links: Vec<LinkView>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LinkView {
    pub label: String,
    pub url: String,
}

/// Where the licence files are: the bundled `licenses/` folder (which also
/// holds `TripTape-LICENSE.txt` and `THIRD_PARTY_MODELS.md`, see
/// `tauri.conf.json`), or during development `resources/licenses` and the
/// checkout's root, where those two live as `LICENSE` and the ledger.
fn license_dirs(app: &tauri::AppHandle) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = app
        .path()
        .resource_dir()
        .ok()
        .map(|d| d.join("licenses"))
        .into_iter()
        .collect();
    if cfg!(debug_assertions) {
        dirs.extend(checkout_license_dirs());
    }
    dirs
}

/// `resources/licenses` and the repository root of this checkout.
fn checkout_license_dirs() -> [PathBuf; 2] {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    [root.join("resources").join("licenses"), root]
}

#[tauri::command]
pub async fn get_licenses(app: tauri::AppHandle) -> Result<Vec<LicenseView>, String> {
    read_licenses(&license_dirs(&app)).map_err(|e| {
        format!("Some licence files are missing from this copy of TripTape ({e}). They are in the source repository.")
    })
}

/// The licences TripTape shows, each file read from the first of `dirs`
/// that has it under one of its names. Every file must be found: a build
/// without them is broken, not quietly incomplete.
pub fn read_licenses(dirs: &[PathBuf]) -> Result<Vec<LicenseView>, String> {
    let read = |names: &[&str]| {
        dirs.iter()
            .flat_map(|d| names.iter().map(move |n| d.join(n)))
            .find(|p| p.is_file())
            .ok_or_else(|| names[0].to_string())
            .and_then(|p| std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", names[0])))
    };
    let link = |label: &str, url: &str| LinkView {
        label: label.into(),
        url: url.into(),
    };
    let typst = format!(
        "{}\n\n{}",
        read(&["Typst-LICENSE-Apache-2.0.txt"])?,
        read(&["Typst-NOTICE.txt"])?
    );
    Ok(vec![
        LicenseView {
            id: "triptape".into(),
            title: "TripTape".into(),
            summary: "MIT License. Free to use, change and share.".into(),
            text: read(&["TripTape-LICENSE.txt", "LICENSE"])?,
            links: vec![link(
                "Source code",
                "https://github.com/rlorenzo/triptape",
            )],
        },
        LicenseView {
            id: "ffmpeg".into(),
            title: "FFmpeg and ffprobe 9.0.2".into(),
            summary: "GNU General Public License, version 3. A separate program TripTape runs to make films and reels; it is not part of TripTape's code.".into(),
            text: read(&["GPL-3.0.txt"])?,
            links: vec![
                link("FFmpeg 9.0.2 source code", "https://ffmpeg.org/releases/ffmpeg-9.0.2.tar.xz"),
                link("Windows build commit", "https://github.com/FFmpeg/FFmpeg/commit/946fcce07b"),
                link("macOS build", "https://evermeet.cx/ffmpeg/"),
                link("Windows build", "https://www.gyan.dev/ffmpeg/builds/"),
            ],
        },
        LicenseView {
            id: "typst".into(),
            title: "Typst 0.15.1".into(),
            summary: "Apache License 2.0. A separate program TripTape runs to lay out the photo book and titles.".into(),
            text: typst,
            links: vec![link(
                "Typst 0.15.1 source code",
                "https://github.com/typst/typst/tree/v0.15.1",
            )],
        },
        LicenseView {
            id: "bundled".into(),
            title: "Bundled programs".into(),
            summary: "How the bundled programs are built and where their source is.".into(),
            text: read(&["THIRD_PARTY.md"])?,
            links: vec![],
        },
        LicenseView {
            id: "models".into(),
            title: "Third-party models".into(),
            summary: "TripTape ships no machine-learning models yet. This ledger lists the ones planned and the licence each must have.".into(),
            text: read(&["THIRD_PARTY_MODELS.md"])?,
            links: vec![],
        },
    ])
}

// ---------------------------------------------------------------------------
// Diagnostic report
// ---------------------------------------------------------------------------

/// Log an export's start. The trip's name, captions and place names are
/// registered as secrets first, so no later line (an FFmpeg or Typst error
/// quoting a caption, say) can carry them.
pub fn log_export_start(
    job_id: &str,
    kind: &str,
    trip_name: &str,
    plan: &triptape_core::types::Plan,
) {
    let places = plan
        .chapters
        .iter()
        .flat_map(|c| [Some(c.title.as_str()), c.place.as_deref()])
        .flatten();
    applog::add_secrets(
        std::iter::once(trip_name)
            .chain(plan.captions.values().map(String::as_str))
            .chain(places),
    );
    applog::info(&format!(
        "export {job_id} {kind}: started, {} film picks, {} reel picks, {} chapters",
        plan.hero.len(),
        plan.reel_selects.len(),
        plan.chapters.len()
    ));
}

/// Log how an export ended, with the error text (redacted) when it failed.
pub fn log_export_end(job: &triptape_core::types::Job, error: Option<&str>) {
    match error {
        None => applog::info(&format!("export {} {}: {:?}", job.id, job.kind, job.state)),
        Some(e) => applog::error(&format!(
            "export {} {}: {:?} at {}: {e}",
            job.id,
            job.kind,
            job.state,
            job.current_step.as_deref().unwrap_or("start")
        )),
    }
}

/// How many log lines a report carries.
const REPORT_LOG_LINES: usize = 200;

fn source_word(s: &ToolSource) -> &'static str {
    match s {
        ToolSource::Override => "chosen by an override",
        ToolSource::Sidecar => "bundled",
        ToolSource::Path => "from PATH (development fallback)",
        ToolSource::Missing => "missing",
    }
}

/// A plain-text report the owner can read before sharing it: app and OS,
/// the media tools, cache sizes, and the last log lines. Everything that
/// came from outside the app is redacted again on the way out.
#[tauri::command]
pub async fn get_diagnostic_report(
    app: tauri::AppHandle,
    state: tauri::State<'_, SettingsState>,
) -> Result<String, String> {
    let cache = get_cache_info(app.clone(), state).await?;
    let version = app.package_info().version.to_string();
    tauri::async_runtime::spawn_blocking(move || diagnostic_report(&version, &cache))
        .await
        .map_err(|e| e.to_string())
}

pub fn diagnostic_report(version: &str, cache: &CacheInfo) -> String {
    let t = tools();
    let mut out = String::new();
    let mut line = |s: &str| {
        out.push_str(s);
        out.push('\n');
    };
    line("TripTape diagnostic report");
    line(&format!("Made {}", applog::now()));
    line("Nothing here was sent anywhere. Folder and file names are short codes; places, captions and names are left out.");
    line("");
    line(&format!(
        "App: TripTape {version} ({})",
        if cfg!(debug_assertions) {
            "development build"
        } else {
            "release build"
        }
    ));
    line(&format!(
        "System: {} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    line("Media tools:");
    for (name, tool) in [
        ("FFmpeg", &t.ffmpeg),
        ("ffprobe", &t.ffprobe),
        ("Typst", &t.typst),
    ] {
        line(&format!("  {name}: {}", source_word(&tool.source)));
    }
    let notes = tool_status().notes;
    if !notes.is_empty() {
        line("Tool notes:");
        for n in notes {
            line(&format!("  - {}", applog::redact(&n)));
        }
    }
    let size = segment_cache::fmt_bytes;
    line("Caches:");
    line(&format!(
        "  Thumbnails: {} in {} files",
        size(cache.thumbnail_bytes),
        cache.thumbnail_files
    ));
    line(&format!(
        "  Film pieces: {} in {} entries, limit {}{}",
        size(cache.segment_bytes),
        cache.segment_entries,
        size(
            cache
                .segment_cap_bytes
                .unwrap_or(cache.default_segment_cap_bytes)
        ),
        if cache.segment_in_use > 0 {
            format!(", {} export(s) using it", cache.segment_in_use)
        } else {
            String::new()
        }
    ));
    line(&format!("  Log: {}", size(applog::total_bytes())));
    line("");
    let tail = applog::tail(REPORT_LOG_LINES);
    line(&format!("Last {} log lines:", tail.len()));
    // Already redacted when written; again in case a secret was
    // registered after the line was.
    for l in applog::redact_all(&tail) {
        line(&l);
    }
    out
}

/// Write the report into the log folder and return its path, for when the
/// clipboard is not available. The owner can open it before sharing.
#[tauri::command]
pub async fn save_diagnostic_report(
    app: tauri::AppHandle,
    state: tauri::State<'_, SettingsState>,
) -> Result<String, String> {
    let dir = state.log_dir.clone();
    let report = get_diagnostic_report(app, state).await?;
    let path = dir.join("diagnostic-report.txt");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(&path, report).map_err(|e| e.to_string())?;
    Ok(path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_never_leaves_the_cache_folder() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("cache");
        let thumbs = root.join("thumb");
        std::fs::create_dir_all(thumbs.join("sub")).unwrap();
        std::fs::write(thumbs.join("a.jpg"), b"1234").unwrap();
        std::fs::write(thumbs.join("sub").join("b.jpg"), b"56").unwrap();
        // The owner's things, outside the cache.
        let outside = t.path().join("photos");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.heic"), b"precious").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, thumbs.join("link-to-photos")).unwrap();
            std::os::unix::fs::symlink(outside.join("keep.heic"), thumbs.join("link.heic"))
                .unwrap();
        }

        let r = clear_dir_within(&thumbs, &root).unwrap();
        assert_eq!((r.removed, r.freed_bytes), (2, 6));
        assert!(thumbs.is_dir(), "the folder itself stays");
        assert_eq!(std::fs::read_dir(&thumbs).unwrap().count(), 0);
        assert_eq!(
            std::fs::read(outside.join("keep.heic")).unwrap(),
            b"precious"
        );

        // Refused: the root itself, a sibling, a `..` escape, and a link out.
        assert!(clear_dir_within(&root, &root).is_err());
        assert!(clear_dir_within(&outside, &root).is_err());
        assert!(clear_dir_within(&thumbs.join("..").join("..").join("photos"), &root).is_err());
        #[cfg(unix)]
        {
            let link = root.join("proxy");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            assert!(clear_dir_within(&link, &root).is_err());
        }
        assert!(outside.join("keep.heic").exists());
        // A cache folder that does not exist yet is simply empty.
        let none = clear_dir_within(&root.join("never-made"), &root).unwrap();
        assert_eq!(none.removed, 0);
    }

    #[test]
    fn sizes_count_files_but_not_link_targets() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("c/d")).unwrap();
        std::fs::write(t.path().join("c/a"), vec![0u8; 10]).unwrap();
        std::fs::write(t.path().join("c/d/b"), vec![0u8; 5]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(t.path().join("c/a"), t.path().join("c/d/l")).unwrap();
        assert_eq!(dir_size(&t.path().join("c")), (15, 2));
        assert_eq!(dir_size(&t.path().join("missing")), (0, 0));
    }

    #[test]
    fn licences_read_from_the_checkout() {
        let l = read_licenses(&checkout_license_dirs()).expect("every licence file is present");
        let ffmpeg = l.iter().find(|x| x.id == "ffmpeg").unwrap();
        assert!(ffmpeg.text.contains("GNU GENERAL PUBLIC LICENSE"));
        assert!(ffmpeg.text.contains("Version 3"));
        assert!(ffmpeg
            .links
            .iter()
            .any(|k| k.url.ends_with("ffmpeg-9.0.2.tar.xz")));
        let ours = l.iter().find(|x| x.id == "triptape").unwrap();
        assert!(ours.text.contains("MIT License"));
        assert!(l
            .iter()
            .any(|x| x.id == "typst" && x.text.contains("Apache License")));
        assert!(l
            .iter()
            .any(|x| x.id == "models" && x.text.contains("Third-Party Models")));
        assert!(read_licenses(&[]).is_err());
    }

    #[test]
    fn bundle_ships_every_licence_file() {
        // tauri.conf.json must copy what `read_licenses` looks for.
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let res = &conf["bundle"]["resources"];
        assert_eq!(res["../resources/licenses/*"], "licenses/");
        assert_eq!(res["../LICENSE"], "licenses/TripTape-LICENSE.txt");
        assert_eq!(
            res["../THIRD_PARTY_MODELS.md"],
            "licenses/THIRD_PARTY_MODELS.md"
        );
    }

    #[test]
    fn route_ends_fill_home() {
        let route = [(35.0, 139.0), (34.6, 135.5)];
        assert_eq!(
            route_end(&route, "first").unwrap(),
            Home::new(35.0, 139.0).unwrap()
        );
        assert_eq!(
            route_end(&route, "last").unwrap(),
            Home::new(34.6, 135.5).unwrap()
        );
        assert!(route_end(&[], "first").is_err());
        assert!(route_end(&route, "middle").is_err());
    }
}
