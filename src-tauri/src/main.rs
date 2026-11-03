//! TripTape desktop shell entry point (Tauri v2).
//!
//! Thin wiring only: state setup, startup recovery, command registration,
//! and the `asset://` thumbnail scope (limited to the cache dir by
//! `tauri.conf.json`). All ingest/curation/render work lives in
//! `triptape-core` / `triptape-media` and is invoked from the commands in
//! [`triptape_app`] re-exported here via the glue crate.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::path::PathBuf;

use tauri::{Emitter, Manager, State};
use triptape_app::{
    default_export_dir, paginate, valid_job_kind, AppState, AssetView, ExportSummary, JobView,
    ScanSummary,
};
use triptape_core::{cluster, hash, jobs, plan as planmod, scan, types::*};

fn job_view(job: &Job) -> JobView {
    JobView {
        id: job.id.clone(),
        kind: job.kind.clone(),
        state: triptape_core::db::job_state_to_str(job.state).to_string(),
        progress: job.progress,
        current_step: job.current_step.clone(),
        error: job.error.clone(),
    }
}

fn emit(app: &tauri::AppHandle, job: &Job) {
    let _ = app.emit("job://progress", job_view(job));
}

fn asset_view(a: &Asset) -> AssetView {
    AssetView {
        id: a.id.clone(),
        kind: match a.kind {
            AssetKind::Photo => "photo",
            AssetKind::Video => "video",
            AssetKind::LivePhoto => "live-photo",
        }
        .to_string(),
        width: a.width,
        height: a.height,
        duration_secs: a.duration_secs,
        taken_at: a.taken_at,
        time_guessed: a.time_guessed,
        has_gps: a.gps.is_some(),
        is_live_photo: a.kind == AssetKind::LivePhoto,
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Register a folder chosen in the OS folder picker, ingest it into the
/// store, and build the first plan. The picker result is the only raw path
/// that ever crosses IPC; everything downstream is addressed by scoped ids.
#[tauri::command]
async fn scan_folder(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    folder: String,
) -> Result<ScanSummary, String> {
    let root = PathBuf::from(&folder);
    if !root.is_dir() {
        return Err("not a folder".to_string());
    }
    // Canonicalize once so the stored project folder is stable; the raw
    // string never leaves this command.
    let canon = root
        .canonicalize()
        .map_err(|e| format!("cannot open folder: {e}"))?;

    // Create the project row first so progress has a job to attach to.
    let (project, job_id) = {
        let db = state.db.lock().map_err(|e| e.to_string())?;
        let name = canon
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "trip".to_string());
        let project = db.create_project(&name, &canon.to_string_lossy())?;
        let job = db.insert_job(&project.id, "ingest", None)?;
        (project, job.id)
    };

    // Heavy walk runs off the async executor. It shares the app's single
    // `Db` handle, locking only around writes so other commands stay live.
    let worker_app = app.clone();
    let pid = project.id.clone();
    let worker_job = job_id.clone();
    let outcome = tauri::async_runtime::spawn_blocking(move || -> Result<usize, String> {
        let state = worker_app.state::<AppState>();
        let lock = || state.db.lock().map_err(|e| e.to_string());
        lock()?.update_job(&worker_job, JobState::Running, 0.0, Some("scanning"), None)?;
        let work = || -> Result<usize, String> {
            let result = scan::scan_folder(&canon).map_err(|e| e.to_string())?;
            let total = result.files.len();
            let mut assets: Vec<Asset> = Vec::with_capacity(total);
            for (i, f) in result.files.iter().enumerate() {
                let kind = if scan::VIDEO_EXTS.contains(&f.ext.as_str()) {
                    AssetKind::Video
                } else {
                    AssetKind::Photo
                };
                let mut a = Asset::new(format!("{pid}_a{i:06}"), f.path.clone(), kind);
                a.size_bytes = f.size_bytes;
                a.mtime = f.mtime_secs;
                a.taken_at = Some(f.mtime_secs);
                a.time_guessed = true;
                a.content_hash = hash::blake3_file(&f.path).ok();
                assets.push(a);
                if i % 50 == 0 {
                    // Hashing is the first half of the job; persisting the second.
                    let progress = i as f32 / total as f32 * 0.5;
                    let _ = lock()?.update_job(
                        &worker_job,
                        JobState::Running,
                        progress,
                        Some("scanning"),
                        None,
                    );
                }
            }
            let mut clusters = cluster::cluster_assets(&assets);
            for c in &mut clusters {
                c.id = format!("{pid}_{}", c.id);
            }
            let cluster_of: HashMap<String, String> = clusters
                .iter()
                .flat_map(|c| c.asset_ids.iter().map(|id| (id.clone(), c.label.clone())))
                .collect();
            // No scoring in the desktop ingest yet: the plan falls back to its
            // unscored ordering.
            let plan = planmod::build_plan(&assets, &cluster_of, &HashMap::new());
            let db = lock()?;
            db.in_transaction(|db| {
                for a in &assets {
                    db.upsert_asset(&pid, a)?;
                }
                for c in &clusters {
                    db.insert_cluster(&pid, "day", c)?;
                    for id in &c.asset_ids {
                        let hero = c.asset_ids.first().is_some_and(|h| h == id);
                        db.link_asset_cluster(id, &c.id, hero)?;
                    }
                }
                Ok::<(), String>(())
            })?;
            db.insert_plan(&pid, &plan, "local")?;
            db.update_job(&worker_job, JobState::Done, 1.0, Some("done"), None)?;
            Ok(total)
        };
        let result = work();
        // Every exit path leaves the job in a terminal state.
        if let Err(e) = &result {
            let _ = lock()?.update_job(&worker_job, JobState::Failed, 0.0, Some("failed"), Some(e));
        }
        result
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    // Refresh the job row for the response (event stream carries the rest).
    let job = state
        .db
        .lock()
        .map_err(|e| e.to_string())?
        .get_job(&job_id)?
        .ok_or_else(|| "job vanished".to_string())?;
    emit(&app, &job);
    let files_seen = outcome?;
    Ok(ScanSummary {
        project_id: project.id,
        project_name: project.name,
        files_seen,
        job_id,
    })
}

#[tauri::command]
async fn get_assets(
    state: State<'_, AppState>,
    project_id: String,
    page: Option<usize>,
    per_page: Option<usize>,
) -> Result<Vec<AssetView>, String> {
    state.require_project(&project_id)?;
    let db = state.db.lock().map_err(|e| e.to_string())?;
    let assets = db.list_assets(&project_id)?;
    Ok(
        paginate(&assets, page.unwrap_or(0), per_page.unwrap_or(100))
            .iter()
            .map(asset_view)
            .collect(),
    )
}

#[tauri::command]
async fn start_job(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    project_id: String,
    kind: String,
) -> Result<JobView, String> {
    state.require_project(&project_id)?;
    if !valid_job_kind(&kind) {
        return Err(format!("unknown job kind {kind}"));
    }
    let job = state
        .db
        .lock()
        .map_err(|e| e.to_string())?
        .insert_job(&project_id, &kind, None)?;
    emit(&app, &job);
    Ok(job_view(&job))
}

#[tauri::command]
async fn cancel_job(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    job_id: String,
) -> Result<JobView, String> {
    AppState::check_id(&job_id)?;
    let db = state.db.lock().map_err(|e| e.to_string())?;
    jobs::transition(&db, &job_id, JobState::Cancelled, 0.0, None, None)?;
    let job = db
        .get_job(&job_id)?
        .ok_or_else(|| format!("unknown job {job_id}"))?;
    // Temp-dir cleanup: the render worker owns deletion on cancel; the row
    // records the temp dir so a later sweep can reclaim orphans.
    emit(&app, &job);
    Ok(job_view(&job))
}

/// Load the latest plan, its assets, and the trip name under one lock.
fn load_plan_assets(
    state: &AppState,
    project_id: &str,
) -> Result<(Plan, Vec<Asset>, String), String> {
    state.require_project(project_id)?;
    let db = state.db.lock().map_err(|e| e.to_string())?;
    let (pid, plan) = db
        .latest_plan_with_project(Some(project_id))?
        .ok_or_else(|| "no plan for project yet — scan first".to_string())?;
    let assets = db.list_assets(&pid)?;
    let name = db
        .get_project(&pid)?
        .map(|p| p.name)
        .unwrap_or_else(|| "trip".to_string());
    Ok((plan, assets, name))
}

/// Renders run on a blocking worker so ffmpeg/typst never stall the async
/// executor (and with it `cancel_job` / `get_assets`).
async fn run_export(
    app: tauri::AppHandle,
    project_id: String,
    kind: &'static str,
) -> Result<ExportSummary, String> {
    tauri::async_runtime::spawn_blocking(move || export_blocking(&app, &project_id, kind))
        .await
        .map_err(|e| e.to_string())?
}

fn export_blocking(
    app: &tauri::AppHandle,
    project_id: &str,
    kind: &str,
) -> Result<ExportSummary, String> {
    let state = app.state::<AppState>();
    let (plan, assets, trip_name) = load_plan_assets(&state, project_id)?;
    // Insert and mark Running before the render so the progress event
    // carries the job id the UI's Cancel button needs.
    let job = {
        let db = state.db.lock().map_err(|e| e.to_string())?;
        let job = db.insert_job(project_id, kind, None)?;
        jobs::transition(
            &db,
            &job.id,
            JobState::Running,
            0.0,
            Some("rendering"),
            None,
        )?;
        db.get_job(&job.id)?
            .ok_or_else(|| "job vanished".to_string())?
    };
    emit(app, &job);

    let caps = triptape_media::ffmpeg::probe_capabilities("ffmpeg", "ffprobe");
    let out_dir = default_export_dir(&trip_name, project_id);
    let opts = triptape_media::export::ExportOptions {
        preset: triptape_media::render_book::BookPreset::P1,
        trip_name,
        typst_bin: "typst".to_string(),
        music_path: None,
        keep_ambience: false,
        only: "all".to_string(),
    };
    let rendered = triptape_media::export::export_all(&caps, &plan, &assets, &out_dir, &opts)
        .map_err(|e| e.to_string());

    let db = state.db.lock().map_err(|e| e.to_string())?;
    // A cancelled row is terminal: the transition to Done/Failed is rejected
    // and the cancel is reported instead of a success.
    let terminal = match &rendered {
        Ok(_) => jobs::transition(&db, &job.id, JobState::Done, 1.0, Some("done"), None),
        Err(e) => jobs::transition(&db, &job.id, JobState::Failed, 0.0, Some("failed"), Some(e)),
    };
    let done = db
        .get_job(&job.id)?
        .ok_or_else(|| "job vanished".to_string())?;
    drop(db);
    emit(app, &done);
    if done.state == JobState::Cancelled {
        return Err("export cancelled".to_string());
    }
    terminal?;
    let report = rendered?;
    Ok(ExportSummary {
        job_id: job.id,
        out_dir: out_dir.to_string_lossy().into_owned(),
        video: report.video,
        reels: report.reels,
        book_pages: report.book_pages,
        skipped: report.skipped,
    })
}

#[tauri::command]
async fn export_video(app: tauri::AppHandle, project_id: String) -> Result<ExportSummary, String> {
    run_export(app, project_id, "render-video").await
}

#[tauri::command]
async fn export_reels(app: tauri::AppHandle, project_id: String) -> Result<ExportSummary, String> {
    run_export(app, project_id, "render-reels").await
}

#[tauri::command]
async fn export_book(app: tauri::AppHandle, project_id: String) -> Result<ExportSummary, String> {
    run_export(app, project_id, "render-book").await
}

/// One combined render for "Export all keepsakes" (instead of three passes).
#[tauri::command]
async fn export_all(app: tauri::AppHandle, project_id: String) -> Result<ExportSummary, String> {
    run_export(app, project_id, "export-all").await
}

/// The store lives in the per-user app data dir; `TRIPTAPE_DB` remains an
/// explicit development override.
fn db_path(app: &tauri::App) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(p) = std::env::var_os("TRIPTAPE_DB") {
        return Ok(PathBuf::from(p));
    }
    let dir = app.path().app_data_dir()?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("triptape.sqlite3"))
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let state = AppState::open(&db_path(app)?)?;
            // Startup recovery: any `running`/`finalizing` row becomes
            // `interrupted` (jobs::mark_interrupted); the UI offers
            // Resume / Discard per job.
            {
                let db = state.db.lock().map_err(|e| e.to_string())?;
                match jobs::mark_interrupted(&db) {
                    Ok(ids) if !ids.is_empty() => {
                        eprintln!("recovered {} interrupted job(s)", ids.len())
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("startup recovery failed: {e}"),
                }
            }
            app.manage(state);
            Ok(())
        })
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            scan_folder,
            get_assets,
            start_job,
            cancel_job,
            export_video,
            export_reels,
            export_book,
            export_all
        ])
        .run(tauri::generate_context!())
        .expect("error while running TripTape");
}
