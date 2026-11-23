//! TripTape desktop shell entry point (Tauri v2).
//!
//! Thin wiring only: state setup, startup recovery, command registration,
//! and the `asset://` thumbnail scope (limited to the cache dir by
//! `tauri.conf.json`). All ingest/curation/render work lives in
//! `triptape-core` / `triptape-media` and is invoked from the commands in
//! [`triptape_app`] re-exported here via the glue crate.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tauri::{Emitter, Manager, State};
use triptape_ai_local::local::{LocalModels, ModelPaths};
use triptape_app::{
    apply_live_photo_pairing, book_notes, book_presets, check_sources as check_plan_sources,
    export_dir_in, export_eligibility, export_sources, fill_assets_parallel, find_relinks,
    finished_film, keep_known_asset_ids, keepsake_scope, paginate, parse_book_preset, plain_skip,
    relink_plan, relinked_asset, resolve_export_parent, tool_status, tools, valid_job_kind,
    AppState, AssetView, BookPresetView, EligibilityView, ExportSummary, JobView, RelinkView,
    ScanSummary, SourceCheckView, StoryData, StoryEntryView, StoryView, ToolStatusView,
};
use triptape_core::{
    cluster,
    db::{Db, Project},
    hash, jobs, metadata, plan as planmod, proxies, scan, score,
    types::*,
};

fn job_view(job: &Job) -> JobView {
    JobView {
        id: job.id.clone(),
        project_id: job.project_id.clone(),
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

/// Persist one ingest progress step and emit it on `job://progress` together,
/// so the DB row and the event stream never disagree. Emitting from a
/// `spawn_blocking` worker is fine (`AppHandle` is shared); callers throttle
/// to ~5 Hz (one emit per 200 ms) rather than one event per file.
/// Returns `false` (and writes nothing) when the job was already cancelled, so
/// a late progress tick never revives a cancelled row back to `Running`.
/// Callers hold the Db lock, so the check and the update cannot interleave
/// with `cancel_job`.
fn report_ingest(app: &tauri::AppHandle, db: &Db, job_id: &str, progress: f32, step: &str) -> bool {
    if matches!(db.get_job(job_id), Ok(Some(j)) if j.state == JobState::Cancelled) {
        return false;
    }
    let _ = db.update_job(job_id, JobState::Running, progress, Some(step), None);
    if let Ok(Some(job)) = db.get_job(job_id) {
        emit(app, &job);
    }
    true
}

/// Minimum gap between progress emits (~5 Hz ceiling for the event stream).
const PROGRESS_EMIT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// Foreground handoff progress: walk + clustering + initial plan + persist.
/// The command returns at this point so the library is usable ASAP.
const FOREGROUND_DONE_PROGRESS: f32 = 0.15;
/// Background hashing maps byte-weighted completion into
/// `FOREGROUND_DONE_PROGRESS`..=`BACKGROUND_HASH_CEILING_PROGRESS`;
/// scoring maps per-asset completion into
/// `BACKGROUND_HASH_CEILING_PROGRESS`..=`BACKGROUND_CEILING_PROGRESS`;
/// the plan rebuild and Done follow.
/// Hashing streams bytes at GB/s while scoring decodes every still, so
/// hashing owns a narrow band and scoring owns the bulk: the bar tracks
/// wall-clock reality instead of sitting at 80% for the whole slow phase.
const BACKGROUND_HASH_CEILING_PROGRESS: f32 = 0.30;
const BACKGROUND_CEILING_PROGRESS: f32 = 0.95;
/// Hash-persist batching: flush pending content hashes every ~25 files or
/// ~1s (always at the end) so each Db lock hold stays short.
const HASH_FLUSH_FILES: usize = 25;
const HASH_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
/// Score-persist batching: same shape as the hash flush, so each Db lock
/// hold stays short while decode/ffmpeg work stays lock-free.
const SCORE_FLUSH_FILES: usize = 25;

/// A queued hash write: (asset id, path, size, mtime, digest). The source
/// fingerprint travels with every write so a superseding rescan's recycled
/// ids can never receive another job's hash.
type PendingHash = (String, String, u64, i64, Option<String>);
/// A queued score write: (asset id, path, size, mtime, analysis).
/// Same recycled-id fence as [`PendingHash`].
type PendingScore = (String, String, u64, i64, Analysis);

/// Where the optional local models live: `TRIPTAPE_MODELS_DIR`, else the
/// app data dir's `models/` (what `scripts/fetch-models` installs into).
fn model_paths(app: &tauri::AppHandle) -> Option<ModelPaths> {
    ModelPaths::from_env(
        None,
        app.path().app_data_dir().ok().map(|d| d.join("models")),
    )
}

/// Score and fingerprint one asset for the background ingest pass,
/// mirroring the CLI (`triptape-media`'s `triptape` binary) constants.
///
/// Every kind is read through its existing 320 px ffmpeg thumbnail (same
/// `source_cache_key` + `cache_path` convention as `get_thumbnail`, so
/// scoring warms the Library thumbnail cache; videos use their poster
/// frame). Stills and Live Photos score that image and fall back to the
/// source file when the thumbnail cannot be made or decoded; unscorable
/// stills get the CLI neutral fallback. The pHash comes from the same
/// pixels, so HEIC stills, Live Photos and videos get one too. Videos
/// score by the quality of their best playback window (plus people and
/// caption signals) and keep that window. Stills and Live Photos also go
/// through whatever optional local `models` loaded
/// ([`LocalModels::analyze_still`]). Pure per-file work: never fails,
/// never touches the Db.
fn score_asset_with_cache(cache_dir: &Path, a: &Asset, models: &LocalModels) -> Analysis {
    let thumb = proxies::source_cache_key(&a.id, &a.path)
        .ok()
        .map(|key| proxies::cache_path(cache_dir, "thumb", &key, ".jpg"))
        .filter(|dst| {
            triptape_media::ffmpeg::render_thumb(&tools().ffmpeg.bin, &a.path, a.kind, dst).is_ok()
        })
        .and_then(|dst| hash::open_image(&dst));
    if a.kind == AssetKind::Video {
        // Any failure keeps the start-of-clip trim and a neutral quality.
        let analysis = a.duration_secs.map_or_else(Default::default, |d| {
            triptape_media::clip_analysis::analyze_clip(
                &tools().ffmpeg.bin,
                &tools().ffprobe.bin,
                &a.path,
                d,
            )
        });
        return Analysis {
            score: analysis.score(&score::StorySignals::for_asset(a)),
            phash: thumb.as_ref().map(hash::average_hash_from_image),
            embedding: None,
            faces: None,
        };
    }
    match thumb {
        Some(t) => models.analyze_still(a, Some(&t), false),
        None => models.analyze_still(a, hash::open_image(&a.path).as_ref(), true),
    }
}

/// Upper bound on concurrent scoring workers: each one holds a full-size
/// still decode plus an ffmpeg/sips child at a time, so this also bounds
/// peak memory. Full-size fallback decodes (HEIC) additionally serialize on
/// the media crate's decode mutex, so HEIC throughput stays decode-bound;
/// videos, direct-scale stills, and the scoring math outside the decode
/// hold parallelize fully.
const SCORE_WORKERS_MAX: usize = 4;

/// Score every asset with bounded parallelism: workers run the lock-free
/// [`score_asset_with_cache`] while `on_score` runs on the calling thread
/// once per completed asset as `(asset, score, done count, total)`, so Db
/// persists and progress emits need no extra locking. Completion order
/// varies; scores key by asset id downstream, so input order is irrelevant.
fn score_assets_parallel(
    cache_dir: &Path,
    assets: &[Asset],
    models: &LocalModels,
    workers: usize,
    mut on_score: impl FnMut(&Asset, Analysis, usize, usize),
) {
    triptape_core::parallel::for_each_parallel(
        assets,
        workers,
        |a| score_asset_with_cache(cache_dir, a, models),
        |idx, score, done, total| on_score(&assets[idx], score, done, total),
    );
}

/// Detached second half of progressive ingest: reuse saved digests for
/// unchanged files (no disk reads), hash the rest with at most
/// [`hash::INGEST_HASH_WORKERS`] files in flight, persist each digest as it
/// completes, then score every asset missing a persisted score (restored
/// projects skip recomputation), then rebuild the plan from the hashed and
/// scored assets (so the exact-duplicate hero collapse and the score-based
/// hero trim/cap take effect) and mark the job Done.
///
/// `cache` maps `(path, size_bytes, mtime)` to a saved digest; the caller
/// loads it once under a brief lock. Hits flow through the same batched
/// flush and count toward progress up front.
///
/// Per-file hash errors land as `None` and per-asset scoring failures land
/// on the CLI fallback score; neither fails the job. A score/plan
/// rebuild/persist failure marks the job Failed instead (assets stay
/// usable). Every path ends the job row in a terminal state.
fn background_hash_job(
    app: &tauri::AppHandle,
    project_id: &str,
    job_id: &str,
    files: &[(String, PathBuf, u64, i64)],
    cache: &HashMap<(String, u64, i64), String>,
) {
    let state = app.state::<AppState>();
    let lock = || state.db.lock().map_err(|e| e.to_string());
    let total = files.len();
    let total_bytes: u64 = files.iter().map(|f| f.2).sum();
    let mut last_progress = FOREGROUND_DONE_PROGRESS;
    let mut last_emit = std::time::Instant::now() - PROGRESS_EMIT_INTERVAL;

    let mut work = || -> Result<(), String> {
        let mut bytes_done: u64 = 0;
        let mut done: usize = 0;
        // (id, path, size, mtime, digest): the fingerprint travels with
        // every queued write so a superseding rescan's recycled ids can
        // never receive another job's hash (see
        // `update_asset_hash_if_source_matches`).
        let mut pending: Vec<PendingHash> = vec![];
        let mut last_flush = std::time::Instant::now();
        // One short transaction for the pending hashes. On failure the
        // entries stay queued and surface at the final flush; callers keep
        // every lock hold to this batch only. Writes whose row no longer
        // describes the same source file are dropped, not failed.
        let flush = |pending: &mut Vec<PendingHash>| -> Result<(), String> {
            if pending.is_empty() {
                return Ok(());
            }
            let db = lock()?;
            db.in_transaction(|db| {
                for (id, path, size, mtime, digest) in pending.iter() {
                    db.update_asset_hash_if_source_matches(
                        id,
                        path,
                        *size,
                        *mtime,
                        digest.as_deref(),
                    )?;
                }
                Ok::<(), String>(())
            })?;
            pending.clear();
            Ok(())
        };
        // Reuse pass: cache hits never touch disk. Their digests queue into
        // the same batched flush and count up front (bytes + done count),
        // keeping progress monotonic across the reuse + hashing phases.
        let mut misses: Vec<(String, PathBuf, u64, i64)> = Vec::new();
        for (id, path, size, mtime) in files {
            let key = (path.to_string_lossy().into_owned(), *size, *mtime);
            match cache.get(&key) {
                Some(digest) => {
                    bytes_done += *size;
                    done += 1;
                    pending.push((
                        id.clone(),
                        path.to_string_lossy().into_owned(),
                        *size,
                        *mtime,
                        Some(digest.clone()),
                    ));
                    if pending.len() >= HASH_FLUSH_FILES && flush(&mut pending).is_ok() {
                        last_flush = std::time::Instant::now();
                    }
                }
                None => misses.push((id.clone(), path.clone(), *size, *mtime)),
            }
        }
        let reused = done;
        if reused > 0 {
            flush(&mut pending)?;
            last_flush = std::time::Instant::now();
            let candidate = scan::background_hash_progress(
                bytes_done,
                total_bytes,
                FOREGROUND_DONE_PROGRESS,
                BACKGROUND_HASH_CEILING_PROGRESS,
            )
            .max(last_progress);
            last_progress = candidate;
            if let Ok(db) = lock() {
                let step = scan::hash_step_label(done, total);
                report_ingest(app, &db, job_id, candidate, &step);
            }
            last_emit = std::time::Instant::now();
        }
        let paths: Vec<PathBuf> = misses.iter().map(|f| f.1.clone()).collect();
        // The engine invokes this on its coordinator thread only, so the
        // batched persists and throttled emits need no extra locking beyond
        // the brief Db guard each one takes itself.
        hash::blake3_files_parallel(
            &paths,
            hash::INGEST_HASH_WORKERS,
            |idx, digest, miss_done, _| {
                bytes_done += misses[idx].2;
                pending.push((
                    misses[idx].0.clone(),
                    misses[idx].1.to_string_lossy().into_owned(),
                    misses[idx].2,
                    misses[idx].3,
                    digest,
                ));
                done = reused + miss_done;
                let candidate = scan::background_hash_progress(
                    bytes_done,
                    total_bytes,
                    FOREGROUND_DONE_PROGRESS,
                    BACKGROUND_HASH_CEILING_PROGRESS,
                )
                .max(last_progress);
                last_progress = candidate;
                if (pending.len() >= HASH_FLUSH_FILES
                    || last_flush.elapsed() >= HASH_FLUSH_INTERVAL)
                    && flush(&mut pending).is_ok()
                {
                    last_flush = std::time::Instant::now();
                }
                let step = scan::hash_step_label(done, total);
                if last_emit.elapsed() >= PROGRESS_EMIT_INTERVAL || done == total {
                    if let Ok(db) = lock() {
                        report_ingest(app, &db, job_id, candidate, &step);
                    }
                    last_emit = std::time::Instant::now();
                }
            },
        );
        flush(&mut pending)?;
        // --- Scoring ------------------------------------------------------
        // Decode/ffmpeg work runs lock-free; persists flush in short
        // batches like the hashes. Assets that already carry a persisted
        // score (restored projects) are skipped but still feed the plan,
        // unless an optional local model is installed whose output they
        // still lack. Without models this is the pHash-only path.
        let cache_dir = app.path().app_cache_dir().map_err(|e| e.to_string())?;
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(SCORE_WORKERS_MAX)
            .min(SCORE_WORKERS_MAX);
        // Models load only when an asset could use them: a session costs
        // hundreds of MB, and most jobs on a restored project score nothing.
        let paths = model_paths(app);
        let installed = paths
            .as_ref()
            .map(ModelPaths::installed)
            .unwrap_or_default();
        let mut unscored: Vec<Asset> = lock()?
            .list_assets(project_id)?
            .into_iter()
            .filter(|a| a.score.is_none() || installed.owe(a))
            .collect();
        let models = if unscored.is_empty() {
            LocalModels::none()
        } else {
            LocalModels::load(paths.as_ref(), workers)
        };
        for note in models.notes() {
            eprintln!("{note}");
        }
        // Drop the ones owed only by a model that then failed to load.
        unscored.retain(|a| a.score.is_none() || models.wants(a));
        let score_total = unscored.len();
        if score_total > 0 {
            // (id, path, size, mtime, analysis): same recycled-id fence as
            // the hash flush (see `update_asset_scoring_if_source_matches`).
            let flush_scores = |pending: &mut Vec<PendingScore>| -> Result<(), String> {
                if pending.is_empty() {
                    return Ok(());
                }
                let db = lock()?;
                db.in_transaction(|db| {
                    for (id, path, size, mtime, analysis) in pending.iter() {
                        db.update_asset_scoring_if_source_matches(
                            id, path, *size, *mtime, analysis,
                        )?;
                    }
                    Ok::<(), String>(())
                })?;
                pending.clear();
                Ok(())
            };
            let mut pending_scores: Vec<PendingScore> = vec![];
            // Decode/score work fans out over worker threads; persists and
            // progress stay on this thread via the completion callback,
            // which fires once per scored asset in completion order.
            score_assets_parallel(
                &cache_dir,
                &unscored,
                &models,
                workers,
                |a, s, done, total| {
                    pending_scores.push((
                        a.id.clone(),
                        a.path.to_string_lossy().into_owned(),
                        a.size_bytes,
                        a.mtime,
                        s,
                    ));
                    // A failed flush keeps its batch; the final flush retries it.
                    if pending_scores.len() >= SCORE_FLUSH_FILES {
                        let _ = flush_scores(&mut pending_scores);
                    }
                    let candidate = (BACKGROUND_HASH_CEILING_PROGRESS
                        + (done as f32 / total as f32)
                            * (BACKGROUND_CEILING_PROGRESS - BACKGROUND_HASH_CEILING_PROGRESS))
                        .max(last_progress);
                    last_progress = candidate;
                    let step = scan::score_step_label(done, total);
                    if last_emit.elapsed() >= PROGRESS_EMIT_INTERVAL || done == total {
                        if let Ok(db) = lock() {
                            report_ingest(app, &db, job_id, candidate, &step);
                        }
                        last_emit = std::time::Instant::now();
                    }
                },
            );
            flush_scores(&mut pending_scores)?;
        }
        if let Ok(db) = lock() {
            report_ingest(
                app,
                &db,
                job_id,
                BACKGROUND_CEILING_PROGRESS,
                "Refreshing story plan…",
            );
            last_progress = BACKGROUND_CEILING_PROGRESS;
        }
        // Re-read the now-hashed and newly scored assets under a brief
        // lock; the rebuild itself is pure. Inserting a newer plan only
        // adds a row, so Story edits (PlanEdits applied on read) survive
        // untouched.
        let mut assets = lock()?.list_assets(project_id)?;
        // The plan reads the freshly persisted scores (already-scored
        // restores contribute theirs too), so hero trim/cap match CLI use.
        let mut scores: HashMap<String, Score> = assets
            .iter()
            .filter_map(|a| a.score.clone().map(|s| (a.id.clone(), s)))
            .collect();
        // Copies of one file share their caption, people tags and score,
        // and an edit takes what it lacks from its original, so the one that
        // stays below carries all of them (in the plan only: each library
        // row keeps what its own file says). Before clustering, so an edit
        // that lost its date sits in its original's chapter.
        cluster::share_identical_metadata(&mut assets, &mut scores);
        let clusters = cluster::cluster_assets(&assets);
        let cluster_of = cluster::chapter_labels(&clusters);
        // Near-duplicates (same moment, similar pixels) stay in the library
        // but only the best shot of each stack enters the plan, as in the CLI.
        let suppressed = cluster::suppressed_ids(&cluster::collapse_near_dupes(&assets, &scores));
        assets.retain(|a| !suppressed.contains(&a.id));
        let plan = planmod::build_plan(&assets, &cluster_of, &scores);
        let db = lock()?;
        // A superseding rescan owns the progress ticket and the plan now:
        // skip both the insert and the terminal write so a stale plan can
        // never replace the refreshed one and the Library never reloads on
        // the old job's timing. Hashes and scores written above stay valid
        // either way. Checked under this same lock hold so no rescan commit
        // can land between the check and the insert.
        if let Ok(Some(j)) = db.get_job(job_id) {
            if j.state == JobState::Cancelled {
                return Ok(());
            }
        }
        db.insert_plan(project_id, &plan, "local")?;
        db.update_job(job_id, JobState::Done, 1.0, Some("done"), None)?;
        if let Ok(Some(job)) = db.get_job(job_id) {
            emit(app, &job);
        }
        Ok(())
    };
    if let Err(e) = work() {
        if let Ok(db) = lock() {
            let _ = db.update_job(
                job_id,
                JobState::Failed,
                last_progress,
                Some("failed"),
                Some(&e),
            );
            if let Ok(Some(job)) = db.get_job(job_id) {
                emit(app, &job);
            }
        }
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
    let project = {
        let db = state.db.lock().map_err(|e| e.to_string())?;
        let name = canon
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "trip".to_string());
        db.create_project(&name, &canon.to_string_lossy())?
    };
    run_ingest(app, &state, project, canon, false).await
}

/// Re-run the scan for an already-imported trip in place, without
/// re-picking its folder: the stored folder is re-walked into the SAME
/// project id and name (no new project row). The stored folder path never
/// crosses IPC; only scoped ids do.
#[tauri::command]
async fn rescan_project(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    project_id: String,
) -> Result<ScanSummary, String> {
    state.require_project(&project_id)?;
    let project = state
        .db
        .lock()
        .map_err(|e| e.to_string())?
        .get_project(&project_id)?
        .ok_or_else(|| format!("unknown project {project_id}"))?;
    let canon = PathBuf::from(&project.folder);
    if !canon.is_dir() {
        return Err(format!(
            "trip folder for \"{}\" is missing — it may have been moved or deleted",
            project.name
        ));
    }
    run_ingest(app, &state, project, canon, true).await
}

/// Walk outcome shared by the foreground closures below: enumerated file
/// count plus `(asset id, path, size, mtime)` rows for the background task.
type IngestWalk = (usize, Vec<(String, PathBuf, u64, i64)>);

/// Shared ingest pipeline behind `scan_folder` (new project row) and
/// `rescan_project` (in-place refresh of the stored project).
///
/// A fresh scan numbers assets in walk order (`{pid}_a{i:06}`); a refresh
/// keeps each known file's id by path and gives new files never-reused
/// ids ([`keep_known_asset_ids`]), so Story edits (`plan_edits`, keyed by asset
/// ids) keep applying to the right photos even after files are deleted.
/// `files_seen` is the enumerated file count, as in the picker flow.
///
/// `refresh` selects the persist semantics: a fresh scan only adds rows,
/// while a refresh first clears this project's cluster graph and prunes
/// asset rows whose files left the folder. Progress, job, and
/// background-hashing behavior are identical either way: a fresh ingest
/// job, return at foreground-done, detached hashing plus plan rebuild.
async fn run_ingest(
    app: tauri::AppHandle,
    state: &AppState,
    project: Project,
    canon: PathBuf,
    refresh: bool,
) -> Result<ScanSummary, String> {
    // Create the ingest job first so progress has a job to attach to. On
    // refresh, supersede any still-running ingest job for this project: its
    // progress ticks stop owning the ticket (report_ingest skips cancelled
    // rows) while its already-flushed hashes and scores stay valid — the
    // refresh below recomputes metadata for every file, so nothing the old
    // job still writes is lost, and both jobs' final plan rebuilds re-read
    // the store. This is what lets Rescan run while scoring is in flight.
    let job_id = {
        let db = state.db.lock().map_err(|e| e.to_string())?;
        if refresh {
            if let Ok(running) = db.jobs_in_state(&[JobState::Running]) {
                for j in running
                    .iter()
                    .filter(|j| j.project_id == project.id && j.kind == "ingest")
                {
                    let _ =
                        jobs::transition(&db, &j.id, JobState::Cancelled, j.progress, None, None);
                }
            }
        }
        db.insert_job(&project.id, "ingest", None)?.id
    };

    // Heavy walk runs off the async executor. It shares the app's single
    // `Db` handle, locking only around writes so other commands stay live.
    let worker_app = app.clone();
    let pid = project.id.clone();
    let worker_job = job_id.clone();
    let outcome = tauri::async_runtime::spawn_blocking(move || -> Result<IngestWalk, String> {
        let state = worker_app.state::<AppState>();
        let lock = || state.db.lock().map_err(|e| e.to_string());
        lock()?.update_job(
            &worker_job,
            JobState::Running,
            0.0,
            Some("Finding files…"),
            None,
        )?;
        // Elapsed-since markers implement the ~5 Hz emit ceiling; the first
        // emit of each phase is forced so small folders still show movement.
        // `last_progress` is returned on failure so the terminal Failed row
        // keeps the bar monotonic instead of dropping back to 0.
        let mut last_emit = std::time::Instant::now() - PROGRESS_EMIT_INTERVAL;
        let mut last_progress = 0.0f32;
        if let Ok(db) = lock() {
            report_ingest(&worker_app, &db, &worker_job, 0.0, "Finding files…");
            last_emit = std::time::Instant::now();
        }
        let mut work = || -> Result<IngestWalk, String> {
            // Walk phase: the total is unknown, so progress stays 0
            // (indeterminate) and only the found-count step text advances.
            // `scan_folder_batched` invokes the callback per ~500-file batch,
            // which is already coarser than the 5 Hz ceiling.
            let mut found = 0usize;
            let result = scan::scan_folder_batched(&canon, 500, |batch| {
                found += batch.len();
                if last_emit.elapsed() >= PROGRESS_EMIT_INTERVAL {
                    if let Ok(db) = lock() {
                        report_ingest(
                            &worker_app,
                            &db,
                            &worker_job,
                            0.0,
                            &scan::walk_step_label(found),
                        );
                    }
                    last_emit = std::time::Instant::now();
                }
            })
            .map_err(|e| e.to_string())?;
            let total = result.files.len();
            // Progressive ingest: assets persist hash-less so the command
            // returns (and the library becomes browsable) as soon as the
            // walk, clustering, and initial plan are saved. The detached
            // background task fills in content hashes afterwards. Clustering
            // uses only taken_at/gps and the plan tolerates missing hashes,
            // so both are valid pre-hash.
            // EXIF capture time / GPS (photos) or ffprobe truth (videos)
            // when readable; mtime + `time_guessed` otherwise. Never
            // fails the file: clustering and the initial plan below run
            // on real times/GPS. No pixel decoding on this path. Bounded
            // workers fill in parallel while walk order (and index-based
            // ids) is restored, with per-file progress on the bar.
            // An `osxphotos export` folder's own database (loaded once,
            // read-only) fills captions/people its files lack.
            let library = triptape_core::osxphotos::OsxPhotosIndex::load(&canon);
            let mut assets =
                fill_assets_parallel(&pid, &result.files, library.as_ref(), |done, _| {
                    if done % 25 == 0 || done == total {
                        if let Ok(db) = lock() {
                            let progress = 0.05 * (done as f32 / total.max(1) as f32);
                            report_ingest(
                                &worker_app,
                                &db,
                                &worker_job,
                                progress,
                                &scan::fill_step_label(done, total),
                            );
                        }
                    }
                });
            // Same trip-timezone normalization as the CLI: assets whose EXIF
            // lacks an offset inherit the nearest dated neighbor's.
            metadata::fill_tz_offsets(&mut assets);
            // Live Photo pairing before clustering/plan/persist: each still
            // + short same-stem motion pair becomes one LivePhoto row and
            // the motion-only row is dropped, so clusters, the plan, the
            // persisted rows, and the background hashing file list below
            // all see the paired set. The motion file is not hashed as its
            // own asset (the Live Photo row hashes its still).
            assets = apply_live_photo_pairing(assets);
            // Hold the store from here through the save: a rescan reads the
            // project's ids and saves the rows it assigned under one lock,
            // so two overlapping rescans can never give one new file two
            // ids (the later save would orphan edits made against the
            // first). Grouping and the plan below are quick, in-memory work.
            let db = lock()?;
            if refresh {
                let known = db.list_assets(&pid)?;
                keep_known_asset_ids(&pid, &mut assets, known.into_iter().map(|a| (a.path, a.id)));
            }
            // Short foreground phases; the background task continues from
            // `FOREGROUND_DONE_PROGRESS` once the command returns.
            report_ingest(&worker_app, &db, &worker_job, 0.05, "Grouping photos…");
            last_progress = 0.05;
            let mut clusters = cluster::cluster_assets(&assets);
            for c in &mut clusters {
                c.id = format!("{pid}_{}", c.id);
            }
            let cluster_of: HashMap<String, String> = clusters
                .iter()
                .flat_map(|c| c.asset_ids.iter().map(|id| (id.clone(), c.label.clone())))
                .collect();
            // The foreground plan is unscored by design (no pixel decode on
            // this path, so the library opens fast); the background task
            // scores every asset and rebuilds the plan before marking Done.
            let plan = planmod::build_plan(&assets, &cluster_of, &HashMap::new());
            report_ingest(&worker_app, &db, &worker_job, 0.10, "Building story plan…");
            last_progress = 0.10;
            db.in_transaction(|db| {
                if refresh {
                    // In-place refresh: drop the previous cluster graph
                    // before re-inserting (cluster ids are project-scoped,
                    // so the old links/clusters would otherwise linger
                    // next to the rebuilt ones).
                    db.clear_project_clusters(&pid)?;
                }
                for a in &assets {
                    if refresh {
                        // Metadata-only refresh: dates, GPS, dims and
                        // captions overwrite, stored hashes and scores
                        // survive, so unchanged files skip rehash and
                        // rescore afterwards.
                        db.upsert_asset_preserving_analysis(&pid, a)?;
                    } else {
                        db.upsert_asset(&pid, a)?;
                    }
                }
                // Prune rows whose files left the folder, but only after a
                // complete walk: an unreadable subfolder is not a deleted
                // one. Kept ids upsert in place above, so `plan_edits` keep
                // applying.
                if refresh && result.errors.is_empty() {
                    let keep: Vec<String> = assets.iter().map(|a| a.id.clone()).collect();
                    db.delete_stale_assets(&pid, &keep)?;
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
            // Hand off to the background hashing task: the row stays Running
            // so the global indicator keeps driving until Done. The hashed
            // set is the paired assets (motion halves ride along inside
            // their Live Photo row, unhashed); `files_seen` above stays the
            // enumerated total.
            let paired_total = assets.len();
            db.update_job(
                &worker_job,
                JobState::Running,
                FOREGROUND_DONE_PROGRESS,
                Some(&scan::hash_step_label(0, paired_total)),
                None,
            )?;
            if let Ok(Some(job)) = db.get_job(&worker_job) {
                emit(&worker_app, &job);
            }
            last_progress = FOREGROUND_DONE_PROGRESS;
            let files: Vec<(String, PathBuf, u64, i64)> = assets
                .iter()
                .map(|a| (a.id.clone(), a.path.clone(), a.size_bytes, a.mtime))
                .collect();
            Ok((total, files))
        };
        let result = work();
        // A foreground failure ends the job Failed here; success hands the
        // still-Running row to the detached background task, which always
        // ends it in a terminal state. The Failed row keeps the last emitted
        // progress so the bar stays monotonic; the outer emit below re-sends
        // the terminal row as well.
        if let Err(e) = &result {
            if let Ok(db) = lock() {
                let _ = db.update_job(
                    &worker_job,
                    JobState::Failed,
                    last_progress,
                    Some("failed"),
                    Some(e),
                );
                if let Ok(Some(job)) = db.get_job(&worker_job) {
                    emit(&worker_app, &job);
                }
            }
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
    let (files_seen, files) = outcome?;
    // Snapshot the reuse cache AFTER the foreground persist: refresh
    // upserts preserve stored hashes, so the snapshot carries them (plus
    // any hashes a superseded job flushed while the foreground ran) and
    // unchanged files skip disk reads. The background task never holds
    // the Db mutex while hashing.
    let cache = {
        let db = state.db.lock().map_err(|e| e.to_string())?;
        db.hash_reuse_cache()?
    };
    // Detached background completion: hashing + plan rebuild. Never awaited —
    // the command returns now so the library is usable ASAP. The join handle
    // is dropped on purpose; the task runs to a terminal job state on its own.
    let bg_app = app.clone();
    let bg_pid = project.id.clone();
    let bg_job = job_id.clone();
    tauri::async_runtime::spawn_blocking(move || {
        background_hash_job(&bg_app, &bg_pid, &bg_job, &files, &cache);
    });
    Ok(ScanSummary {
        project_id: project.id,
        project_name: project.name,
        files_seen,
        job_id,
    })
}

/// Reopen the last trip after a relaunch: returns its summary with
/// `files_seen` set to its asset count, or `None` when the store is empty.
///
/// When the project still has assets missing hashes (interrupted or
/// unfinished ingest) or missing scores, this creates a fresh `ingest` job
/// and spawns the SAME detached [`background_hash_job`] over just the
/// missing files — with the reuse cache, so a file hashed under another
/// project still counts — then returns at once with the job Running. The
/// job re-lists the project and scores whatever lacks a persisted score,
/// so an empty missing-hash list with unscored assets still scores, plans,
/// and reaches Done. With nothing missing, no job is created (`job_id` is
/// empty) and no events fire. Never blocks on hashing. Interrupted rows
/// stay interrupted; the resume is a new job (startup recovery already ran
/// before the frontend calls this).
#[tauri::command]
async fn restore_latest_project(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<Option<ScanSummary>, String> {
    // One brief lock for the project row, the missing set, the asset count,
    // the unscored check, and the reuse cache. The project id comes from
    // the store, not IPC, so there is no id to validate here.
    let (project_id, project_name, files_seen, missing, needs_scoring, cache) = {
        let db = state.db.lock().map_err(|e| e.to_string())?;
        let Some(project) = db.latest_project()? else {
            return Ok(None);
        };
        let assets = db.list_assets(&project.id)?;
        let missing: Vec<(String, PathBuf, u64, i64)> = assets
            .iter()
            .filter(|a| a.content_hash.is_none())
            .map(|a| (a.id.clone(), a.path.clone(), a.size_bytes, a.mtime))
            .collect();
        // Stills missing a model output need another pass only when that
        // model is installed (otherwise there is nothing to add).
        let installed = model_paths(&app)
            .as_ref()
            .map(ModelPaths::installed)
            .unwrap_or_default();
        let needs_scoring = assets.iter().any(|a| a.score.is_none() || installed.owe(a));
        let files_seen = assets.len();
        let cache = db.hash_reuse_cache()?;
        (
            project.id,
            project.name,
            files_seen,
            missing,
            needs_scoring,
            cache,
        )
    };
    if missing.is_empty() && !needs_scoring {
        return Ok(Some(ScanSummary {
            project_id,
            project_name,
            files_seen,
            job_id: String::new(),
        }));
    }
    let job_id = {
        let db = state.db.lock().map_err(|e| e.to_string())?;
        let job = db.insert_job(&project_id, "ingest", None)?;
        db.update_job(
            &job.id,
            JobState::Running,
            FOREGROUND_DONE_PROGRESS,
            Some(&scan::hash_step_label(0, missing.len())),
            None,
        )?;
        job.id.clone()
    };
    if let Ok(Some(job)) = state.db.lock().map_err(|e| e.to_string())?.get_job(&job_id) {
        emit(&app, &job);
    }
    let bg_app = app.clone();
    let bg_pid = project_id.clone();
    let bg_job = job_id.clone();
    tauri::async_runtime::spawn_blocking(move || {
        background_hash_job(&bg_app, &bg_pid, &bg_job, &missing, &cache);
    });
    Ok(Some(ScanSummary {
        project_id,
        project_name,
        files_seen,
        job_id,
    }))
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
            .map(AssetView::from)
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
    let progress = db.get_job(&job_id)?.map_or(0.0, |j| j.progress);
    jobs::transition(&db, &job_id, JobState::Cancelled, progress, None, None)?;
    let job = db
        .get_job(&job_id)?
        .ok_or_else(|| format!("unknown job {job_id}"))?;
    // Kills the running child at once. The render then returns, and
    // `export_all` removes its working folder.
    if let Some(flag) = state
        .running_exports
        .lock()
        .map_err(|e| e.to_string())?
        .get(&job_id)
    {
        flag.cancel();
    }
    emit(&app, &job);
    Ok(job_view(&job))
}

/// The newest export of this trip that a quit or crash stopped, if the user
/// hasn't dealt with it yet.
#[tauri::command]
async fn get_interrupted_export(
    state: State<'_, AppState>,
    project_id: String,
) -> Result<Option<JobView>, String> {
    state.require_project(&project_id)?;
    let db = state.db.lock().map_err(|e| e.to_string())?;
    Ok(jobs::interrupted_exports(&db, &project_id)?
        .last()
        .map(job_view))
}

#[tauri::command]
async fn dismiss_interrupted_export(
    state: State<'_, AppState>,
    project_id: String,
) -> Result<(), String> {
    state.require_project(&project_id)?;
    let db = state.db.lock().map_err(|e| e.to_string())?;
    jobs::clear_interrupted_exports(&db, &project_id)
}

/// Load the latest plan, its assets, and the trip name under one lock.
fn load_plan_assets(
    state: &AppState,
    project_id: &str,
) -> Result<(Plan, Vec<Asset>, String), String> {
    state.require_project(project_id)?;
    let db = state.db.lock().map_err(|e| e.to_string())?;
    let plan = db
        .edited_plan(Some(project_id))?
        .ok_or_else(|| "no plan for project yet — scan first".to_string())?;
    let name = db
        .get_project(&plan.project_id)?
        .map(|p| p.name)
        .unwrap_or_else(|| "trip".to_string());
    Ok((plan.edited, plan.assets, name))
}

/// Export progress for the app: the same `job://progress` stream and job row
/// ingest uses, fed by the renderers' [`ProgressEvent`]s, plus the cancel
/// signal (the job row going `cancelled`) the renderers poll.
struct ExportProgress {
    app: tauri::AppHandle,
    job_id: String,
    /// `(keepsake, start, end)` slices of the job's 0..1 bar.
    bands: Vec<(&'static str, f32, f32)>,
    last_emit: std::sync::Mutex<(std::time::Instant, f32)>,
    last_cancel_poll: std::sync::Mutex<std::time::Instant>,
    cancelled: std::sync::atomic::AtomicBool,
}

impl ExportProgress {
    fn new(app: &tauri::AppHandle, job_id: &str, only: &str) -> Self {
        // Rough share of an export's time per keepsake.
        let weights = [("video", 0.6f32), ("reels", 0.25), ("book", 0.15)];
        let chosen: Vec<_> = weights
            .into_iter()
            .filter(|(k, _)| only == "all" || only == *k)
            .collect();
        let sum: f32 = chosen.iter().map(|(_, w)| w).sum();
        let mut at = 0.0;
        let bands = chosen
            .into_iter()
            .map(|(k, w)| {
                let band = (k, at, at + w / sum);
                at = band.2;
                band
            })
            .collect();
        let long_ago = std::time::Instant::now() - std::time::Duration::from_secs(60);
        Self {
            app: app.clone(),
            job_id: job_id.to_string(),
            bands,
            last_emit: std::sync::Mutex::new((long_ago, 0.0)),
            last_cancel_poll: std::sync::Mutex::new(long_ago),
            cancelled: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl triptape_media::progress::ProgressSink for ExportProgress {
    fn report(&self, event: &triptape_media::progress::ProgressEvent) {
        let Some(&(_, lo, hi)) = self.bands.iter().find(|b| b.0 == event.keepsake()) else {
            return;
        };
        let overall = event.keepsake_fraction().map(|f| lo + (hi - lo) * f as f32);
        let mut last = self.last_emit.lock().unwrap_or_else(|e| e.into_inner());
        // Never move the bar backwards between phases or on a step with no
        // fraction; throttle like ingest (the final tick always goes out).
        let progress = overall.unwrap_or(last.1).max(last.1);
        let finished = matches!(event, triptape_media::progress::ProgressEvent::Counted { done, total, .. } if done == total);
        if last.0.elapsed() < PROGRESS_EMIT_INTERVAL && !finished {
            return;
        }
        *last = (std::time::Instant::now(), progress);
        let state = self.app.state::<AppState>();
        let db = state.db.lock();
        if let Ok(db) = db {
            if !report_ingest(&self.app, &db, &self.job_id, progress, &event.text()) {
                self.cancelled
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    fn cancelled(&self) -> bool {
        use std::sync::atomic::Ordering;
        if self.cancelled.load(Ordering::Relaxed) {
            return true;
        }
        let mut polled = self
            .last_cancel_poll
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if polled.elapsed() < std::time::Duration::from_millis(250) {
            return false;
        }
        *polled = std::time::Instant::now();
        let state = self.app.state::<AppState>();
        let gone = state.db.lock().is_ok_and(
            |db| matches!(db.get_job(&self.job_id), Ok(Some(j)) if j.state == JobState::Cancelled),
        );
        self.cancelled.store(gone, Ordering::Relaxed);
        gone
    }
}

/// The owner's export choices as they arrive over IPC. Every field is
/// optional so an older frontend keeps today's defaults.
#[derive(Debug, Default)]
struct ExportChoices {
    book_pages: Option<usize>,
    /// `P1` or `P2`; `None` is P1.
    book_preset: Option<String>,
    live_collages: Option<bool>,
    /// Folder picked in the OS dialog; `None` uses the default folder.
    destination: Option<String>,
}

/// Choices checked and resolved, ready for a render.
#[derive(Debug)]
struct ExportPlan {
    book_pages: Option<usize>,
    book_preset: triptape_media::render_book::BookPreset,
    live_collages: bool,
    /// Folder the trip's own export folder goes inside.
    parent: PathBuf,
}

impl ExportChoices {
    /// Fail on a bad choice before a job row exists or anything renders.
    fn validate(self) -> Result<ExportPlan, String> {
        let book_preset = parse_book_preset(self.book_preset.as_deref())?;
        if let Some(n) = self.book_pages {
            book_preset.page_cap(Some(n))?;
        }
        Ok(ExportPlan {
            book_pages: self.book_pages,
            book_preset,
            live_collages: live_collages_or_default(self.live_collages),
            parent: resolve_export_parent(self.destination.as_deref())?,
        })
    }
}

/// Renders run on a blocking worker so ffmpeg/typst never stall the async
/// executor (and with it `cancel_job` / `get_assets`).
async fn run_export(
    app: tauri::AppHandle,
    project_id: String,
    kind: &'static str,
    choices: ExportChoices,
) -> Result<ExportSummary, String> {
    let plan = choices.validate()?;
    tauri::async_runtime::spawn_blocking(move || export_blocking(&app, &project_id, kind, &plan))
        .await
        .map_err(|e| e.to_string())?
}

/// Live Photo motion in film collages: an omitted value (older frontends)
/// means on.
fn live_collages_or_default(live_collages: Option<bool>) -> bool {
    live_collages.unwrap_or(true)
}

/// Which keepsakes a job kind renders.
fn keepsakes_of(kind: &str) -> &'static str {
    match kind {
        "render-video" => "video",
        "render-reels" => "reels",
        "render-book" => "book",
        _ => "all",
    }
}

/// Keeps a running export's cancel flag reachable for exactly as long as it runs.
struct RunningExport<'a> {
    state: &'a AppState,
    job_id: String,
}

impl<'a> RunningExport<'a> {
    fn register(state: &'a AppState, job_id: &str) -> Result<(Self, jobs::CancelFlag), String> {
        let flag = jobs::CancelFlag::new();
        state
            .running_exports
            .lock()
            .map_err(|e| e.to_string())?
            .insert(job_id.to_string(), flag.clone());
        Ok((
            Self {
                state,
                job_id: job_id.to_string(),
            },
            flag,
        ))
    }
}

impl Drop for RunningExport<'_> {
    fn drop(&mut self) {
        if let Ok(mut running) = self.state.running_exports.lock() {
            running.remove(&self.job_id);
        }
    }
}

fn export_blocking(
    app: &tauri::AppHandle,
    project_id: &str,
    kind: &str,
    choices: &ExportPlan,
) -> Result<ExportSummary, String> {
    let state = app.state::<AppState>();
    let (mut plan, assets, trip_name) = load_plan_assets(&state, project_id)?;
    // Settings: hide home on the film's map and the book's map page, and
    // the owner's segment cache limit.
    let settings = triptape_app::settings::export_settings(app);
    settings.hide_home_on(&mut plan.map_polyline);
    let out_dir = export_dir_in(&choices.parent, &trip_name, project_id);
    // Insert and mark Running before the render so the progress event
    // carries the job id the UI's Cancel button needs.
    let job = {
        let db = state.db.lock().map_err(|e| e.to_string())?;
        jobs::clear_interrupted_exports(&db, project_id)?;
        let work_dir = out_dir.join(".work");
        let job = db.insert_job(project_id, kind, Some(&work_dir.to_string_lossy()))?;
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
    // Registered before the first emit, so Cancel always finds the flag.
    let (_running, cancel) = RunningExport::register(&state, &job.id)?;
    emit(app, &job);
    triptape_app::settings::log_export_start(&job.id, kind, &trip_name, &plan);

    let caps =
        triptape_media::ffmpeg::probe_capabilities(&tools().ffmpeg.bin, &tools().ffprobe.bin);
    let opts = triptape_media::export::ExportOptions {
        preset: choices.book_preset,
        book_max_pages: choices.book_pages,
        trip_name,
        typst_bin: tools().typst.bin.clone(),
        music_paths: Vec::new(),
        keep_ambience: false,
        live_collages: choices.live_collages,
        // The default film: no map card after the title.
        opening_map: false,
        only: keepsakes_of(kind).to_string(),
        resolution: triptape_media::resolution::Resolution::default(),
        jobs: None,
        itinerary: None,
        // Face-aware book crops are CLI-only for now (see `book_faces` in
        // the triptape bin); the app's book crops around the centre.
        faces: None,
        known_faces: Default::default(),
        map_style: Default::default(),
        hop_clips: vec![],
        back_cover: None,
        work_dir: None,
        // The same dir the CLI uses, so `triptape cache` sees the app's
        // segments: macOS ~/Library/Caches/app.triptape.desktop/segments.
        segment_cache: triptape_media::segment_cache::default_dir().map(|dir| {
            triptape_media::segment_cache::SegmentCacheConfig {
                max_bytes: settings.segment_cache_max_bytes,
                ..triptape_media::segment_cache::SegmentCacheConfig::new(dir)
            }
        }),
        // The registered flag: `cancel_job` sets it, so the render stops at
        // once instead of at the sink's next database poll.
        progress: triptape_media::progress::Progress::with_cancel(
            std::sync::Arc::new(ExportProgress::new(app, &job.id, keepsakes_of(kind))),
            cancel,
        ),
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
    triptape_app::settings::log_export_end(&done, rendered.as_ref().err().map(String::as_str));
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
        book_notes: if report.book_pages > 0 {
            book_notes(&report.fallbacks)
        } else {
            vec![]
        },
        skipped: report.skipped.into_iter().map(plain_skip).collect(),
    })
}

// ---------------------------------------------------------------------------
// Story editor: view the plan by chapter, rename places, edit captions,
// drop or restore picks, and swap a pick for a same-chapter alternative.
// Every change persists as PlanEdits, applied on read, so exports and any
// newer plan of the project honor the edits.
// ---------------------------------------------------------------------------

/// Mutate the stored edits and return the resulting story.
fn with_edits(
    state: &AppState,
    project_id: &str,
    f: impl FnOnce(&mut planmod::PlanEdits),
) -> Result<StoryView, String> {
    state.require_project(project_id)?;
    let db = state.db.lock().map_err(|e| e.to_string())?;
    save_edits(&db, project_id, f)
}

fn save_edits(
    db: &Db,
    project_id: &str,
    f: impl FnOnce(&mut planmod::PlanEdits),
) -> Result<StoryView, String> {
    let mut edits = db.get_plan_edits(project_id)?;
    f(&mut edits);
    db.save_plan_edits(project_id, &edits)?;
    Ok(StoryData::load(db, project_id)?.view())
}

#[tauri::command]
async fn get_story(state: State<'_, AppState>, project_id: String) -> Result<StoryView, String> {
    state.require_project(&project_id)?;
    let db = state.db.lock().map_err(|e| e.to_string())?;
    Ok(StoryData::load(&db, &project_id)?.view())
}

#[tauri::command]
async fn get_alternatives(
    state: State<'_, AppState>,
    project_id: String,
    asset_id: String,
) -> Result<Vec<StoryEntryView>, String> {
    state.require_project(&project_id)?;
    AppState::check_id(&asset_id)?;
    let db = state.db.lock().map_err(|e| e.to_string())?;
    StoryData::load(&db, &project_id)?.alternatives(&asset_id)
}

#[tauri::command]
async fn set_chapter_place(
    state: State<'_, AppState>,
    project_id: String,
    chapter_key: String,
    place: Option<String>,
) -> Result<StoryView, String> {
    AppState::check_id(&chapter_key)?;
    with_edits(&state, &project_id, |edits| {
        edits.set_place(chapter_key, place)
    })
}

#[tauri::command]
async fn set_asset_caption(
    state: State<'_, AppState>,
    project_id: String,
    asset_id: String,
    caption: Option<String>,
) -> Result<StoryView, String> {
    AppState::check_id(&asset_id)?;
    with_edits(&state, &project_id, |edits| {
        edits.set_caption(asset_id, caption)
    })
}

#[tauri::command]
async fn set_asset_excluded(
    state: State<'_, AppState>,
    project_id: String,
    asset_id: String,
    excluded: bool,
) -> Result<StoryView, String> {
    AppState::check_id(&asset_id)?;
    with_edits(&state, &project_id, |edits| {
        edits.set_excluded(asset_id, excluded)
    })
}

/// Pin a photo into the film ("Add to film"), or un-pin it. Film only; the
/// plan puts it at its capture-time position and the film grows by that
/// shot; nothing is taken out.
#[tauri::command]
async fn set_asset_included(
    state: State<'_, AppState>,
    project_id: String,
    asset_id: String,
    included: bool,
) -> Result<StoryView, String> {
    AppState::check_id(&asset_id)?;
    state.require_project(&project_id)?;
    let db = state.db.lock().map_err(|e| e.to_string())?;
    // The Library lists both copies of a file in it twice; pinning the one
    // the plan left out pins the copy it kept.
    let asset_id = if included {
        let data = StoryData::load(&db, &project_id)?;
        let id = data.planned_twin_id(&asset_id);
        data.check_include(&id)?;
        id
    } else {
        asset_id
    };
    save_edits(&db, &project_id, |edits| {
        edits.set_included(asset_id, included)
    })
}

#[tauri::command]
async fn swap_asset(
    state: State<'_, AppState>,
    project_id: String,
    from_asset_id: String,
    to_asset_id: String,
) -> Result<StoryView, String> {
    AppState::check_id(&from_asset_id)?;
    AppState::check_id(&to_asset_id)?;
    state.require_project(&project_id)?;
    let db = state.db.lock().map_err(|e| e.to_string())?;
    StoryData::load(&db, &project_id)?.check_swap(&from_asset_id, &to_asset_id)?;
    save_edits(&db, &project_id, |edits| {
        edits.swap(from_asset_id, to_asset_id)
    })
}

/// Play a film photo on its own ("Show on its own"), or put it back in the
/// collages ("Put back in the collage"). Setting it solo drops its joins.
#[tauri::command]
async fn set_asset_solo(
    state: State<'_, AppState>,
    project_id: String,
    asset_id: String,
    solo: bool,
) -> Result<StoryView, String> {
    AppState::check_id(&asset_id)?;
    state.require_project(&project_id)?;
    let db = state.db.lock().map_err(|e| e.to_string())?;
    if solo {
        StoryData::load(&db, &project_id)?.check_solo(&asset_id)?;
    }
    save_edits(&db, &project_id, |edits| {
        edits.collage.set_solo(asset_id, solo)
    })
}

/// Join a film photo playing on its own with the next collage photo of its
/// day ("Join with next"; "Join with previous" passes the earlier photo
/// first), even past the automatic reach.
#[tauri::command]
async fn join_assets(
    state: State<'_, AppState>,
    project_id: String,
    first_asset_id: String,
    second_asset_id: String,
) -> Result<StoryView, String> {
    AppState::check_id(&first_asset_id)?;
    AppState::check_id(&second_asset_id)?;
    state.require_project(&project_id)?;
    let db = state.db.lock().map_err(|e| e.to_string())?;
    StoryData::load(&db, &project_id)?.check_join(&first_asset_id, &second_asset_id)?;
    save_edits(&db, &project_id, |edits| {
        edits.collage.join(first_asset_id, second_asset_id)
    })
}

/// Undo the user's joins of a photo ("Separate").
#[tauri::command]
async fn unjoin_asset(
    state: State<'_, AppState>,
    project_id: String,
    asset_id: String,
) -> Result<StoryView, String> {
    AppState::check_id(&asset_id)?;
    with_edits(&state, &project_id, |edits| edits.collage.unjoin(&asset_id))
}

/// Editor thumbnail: generated once per source version into the app cache dir,
/// returned as a path the webview loads over `asset://` (scoped to that dir).
/// Originals are never exposed. Returns None when no frame can be made,
/// including when the source file is gone.
#[tauri::command]
async fn get_thumbnail(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    project_id: String,
    asset_id: String,
) -> Result<Option<String>, String> {
    state.require_project(&project_id)?;
    AppState::check_id(&asset_id)?;
    let (src, kind) = state
        .db
        .lock()
        .map_err(|e| e.to_string())?
        .asset_source(&project_id, &asset_id)?
        .ok_or_else(|| "unknown asset".to_string())?;
    let cache = app.path().app_cache_dir().map_err(|e| e.to_string())?;
    tauri::async_runtime::spawn_blocking(move || {
        let key = triptape_core::proxies::source_cache_key(&asset_id, &src).ok()?;
        let dst = triptape_core::proxies::cache_path(&cache, "thumb", &key, ".jpg");
        triptape_media::ffmpeg::render_thumb(&tools().ffmpeg.bin, &src, kind, &dst)
            .ok()
            .map(|()| dst.to_string_lossy().into_owned())
    })
    .await
    .map_err(|e| e.to_string())
}

/// Which keepsakes this trip can make, with the reason for any that can't,
/// so the Export screen disables them before a run instead of after.
#[tauri::command]
async fn get_export_eligibility(
    state: State<'_, AppState>,
    project_id: String,
) -> Result<EligibilityView, String> {
    let (plan, assets, _) = load_plan_assets(&state, &project_id)?;
    Ok(export_eligibility(&plan, &assets))
}

/// The pre-render source check for one export (`keepsake`: all, video,
/// reels or book): which originals are missing or changed since the scan.
#[tauri::command]
async fn check_sources(
    state: State<'_, AppState>,
    project_id: String,
    keepsake: String,
) -> Result<SourceCheckView, String> {
    let only = keepsake_scope(&keepsake)?;
    let (plan, assets, _) = load_plan_assets(&state, &project_id)?;
    // Changed files are hashed: keep that off the async executor.
    tauri::async_runtime::spawn_blocking(move || check_plan_sources(&plan, &assets, only))
        .await
        .map_err(|e| e.to_string())
}

/// Relink: look for this trip's missing originals in `folder` (picked in the
/// OS dialog) and point their asset rows at the files found there.
#[tauri::command]
async fn relink_sources(
    app: tauri::AppHandle,
    project_id: String,
    folder: String,
) -> Result<RelinkView, String> {
    let new_root = PathBuf::from(&folder);
    if !new_root.is_absolute() || !new_root.is_dir() {
        return Err("That folder can't be opened. Choose another folder.".to_string());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let (plan, assets, _) = load_plan_assets(&state, &project_id)?;
        let old_root = {
            let db = state.db.lock().map_err(|e| e.to_string())?;
            db.get_project(&project_id)?
                .map(|p| PathBuf::from(p.folder))
                .unwrap_or_default()
        };
        // Relink needs only the missing ones: a stat each, no hashing.
        let missing: Vec<Asset> = export_sources(&plan, &assets, "all")
            .into_iter()
            .filter(|(a, _)| !a.path.exists())
            .map(|(a, _)| a.clone())
            .collect();
        let files: Vec<PathBuf> = scan::scan_folder(&new_root)?
            .files
            .into_iter()
            .map(|f| f.path)
            .collect();
        let found = find_relinks(&missing, &old_root, &new_root, &files);
        let db = state.db.lock().map_err(|e| e.to_string())?;
        db.in_transaction(|db| {
            let mut updated = Vec::with_capacity(found.len());
            for r in &found {
                if let Some(a) = missing.iter().find(|a| a.id == r.asset_id) {
                    let next = relinked_asset(a.clone(), r);
                    db.upsert_asset(&project_id, &next)?;
                    updated.push(next);
                }
            }
            // Film and reel picks are checked against the stored plan's
            // snapshots, so those follow the relinked rows too.
            if let Some(mut base) = db.latest_plan(Some(&project_id))? {
                if relink_plan(&mut base, &updated) {
                    db.insert_plan(&project_id, &base, "local")?;
                }
            }
            Ok(())
        })?;
        Ok(RelinkView {
            found: found.len(),
            not_found: missing.len() - found.len(),
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The folder exports go into: `folder` checked as the owner's pick, or the
/// default folder when `None`. The Export screen shows the result and
/// rejects an unusable pick before any export starts.
#[tauri::command]
async fn check_export_folder(folder: Option<String>) -> Result<String, String> {
    resolve_export_parent(folder.as_deref()).map(|p| p.to_string_lossy().into_owned())
}

/// Play a finished film in the default player, wherever it was saved.
#[tauri::command]
async fn open_film(app: tauri::AppHandle, path: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let film = finished_film(&path)?;
    app.opener()
        .open_path(film.to_string_lossy(), None::<&str>)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn export_video(
    app: tauri::AppHandle,
    project_id: String,
    live_collages: Option<bool>,
    destination: Option<String>,
) -> Result<ExportSummary, String> {
    let choices = ExportChoices {
        live_collages,
        destination,
        ..Default::default()
    };
    run_export(app, project_id, "render-video", choices).await
}

#[tauri::command]
async fn export_reels(
    app: tauri::AppHandle,
    project_id: String,
    destination: Option<String>,
) -> Result<ExportSummary, String> {
    let choices = ExportChoices {
        destination,
        ..Default::default()
    };
    run_export(app, project_id, "render-reels", choices).await
}

/// The book shapes the Export screen offers, with their page limits.
#[tauri::command]
async fn get_book_presets() -> Vec<BookPresetView> {
    book_presets()
}

/// Notes for the Export screen when the media tools will make worse
/// keepsakes: a tool taken from PATH, or an FFmpeg without libass or vidstab.
#[tauri::command]
async fn get_tool_status() -> Result<ToolStatusView, String> {
    // The first call probes FFmpeg: keep that off the async executor.
    tauri::async_runtime::spawn_blocking(tool_status)
        .await
        .map_err(|e| e.to_string())
}

/// `book_pages` caps the book's counted pages; `None` keeps the preset
/// default. `book_preset` is `P1` (the default) or `P2`.
#[tauri::command]
async fn export_book(
    app: tauri::AppHandle,
    project_id: String,
    book_pages: Option<usize>,
    book_preset: Option<String>,
    destination: Option<String>,
) -> Result<ExportSummary, String> {
    let choices = ExportChoices {
        book_pages,
        book_preset,
        destination,
        ..Default::default()
    };
    run_export(app, project_id, "render-book", choices).await
}

/// One combined render for "Export all keepsakes" (instead of three passes).
#[tauri::command]
async fn export_all(
    app: tauri::AppHandle,
    project_id: String,
    book_pages: Option<usize>,
    book_preset: Option<String>,
    live_collages: Option<bool>,
    destination: Option<String>,
) -> Result<ExportSummary, String> {
    let choices = ExportChoices {
        book_pages,
        book_preset,
        live_collages,
        destination,
    };
    run_export(app, project_id, "export-all", choices).await
}

/// Quitting must not leave FFmpeg rendering with no one to read the result.
/// Cancels every export and briefly waits for their children to die.
fn stop_running_exports(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let is_empty = || state.running_exports.lock().map_or(true, |r| r.is_empty());
    if let Ok(running) = state.running_exports.lock() {
        for flag in running.values() {
            flag.cancel();
        }
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !is_empty() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// The store lives in the per-user app data dir. `TRIPTAPE_DB` overrides it in
/// development builds only, so a release never takes this path from outside.
fn db_path(app: &tauri::App) -> Result<PathBuf, Box<dyn std::error::Error>> {
    #[cfg(debug_assertions)]
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
            // `interrupted`. Its working files are reclaimed now, and the
            // Export screen offers to make the keepsake again.
            {
                let db = state.db.lock().map_err(|e| e.to_string())?;
                match jobs::mark_interrupted(&db) {
                    Ok(ids) if !ids.is_empty() => {
                        eprintln!("recovered {} interrupted job(s)", ids.len());
                        for id in ids {
                            if let Ok(Some(job)) = db.get_job(&id) {
                                jobs::remove_leftover_work(&job);
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("startup recovery failed: {e}"),
                }
            }
            app.manage(state);
            // Settings, and the redacted log (R4).
            triptape_app::settings::setup(app)?;
            Ok(())
        })
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            scan_folder,
            rescan_project,
            restore_latest_project,
            get_assets,
            start_job,
            cancel_job,
            get_interrupted_export,
            dismiss_interrupted_export,
            export_video,
            export_reels,
            export_book,
            export_all,
            check_export_folder,
            get_export_eligibility,
            get_book_presets,
            get_tool_status,
            check_sources,
            relink_sources,
            open_film,
            get_story,
            get_alternatives,
            set_chapter_place,
            set_asset_caption,
            set_asset_excluded,
            set_asset_included,
            swap_asset,
            set_asset_solo,
            join_assets,
            unjoin_asset,
            get_thumbnail,
            // Settings screen (R4).
            triptape_app::settings::get_privacy_settings,
            triptape_app::settings::set_home,
            triptape_app::settings::set_hide_home,
            triptape_app::settings::trip_end_as_home,
            triptape_app::settings::get_cache_info,
            triptape_app::settings::set_segment_cache_cap,
            triptape_app::settings::clear_thumbnail_cache,
            triptape_app::settings::clear_segment_cache,
            triptape_app::settings::get_licenses,
            triptape_app::settings::get_diagnostic_report,
            triptape_app::settings::save_diagnostic_report
        ])
        .build(tauri::generate_context!())
        .expect("error while building TripTape")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                stop_running_exports(app);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_collages_default_on_and_honor_an_explicit_off() {
        assert!(live_collages_or_default(None));
        assert!(live_collages_or_default(Some(true)));
        assert!(!live_collages_or_default(Some(false)));
    }

    #[test]
    fn export_choices_default_to_today_and_reject_bad_input() {
        let plan = ExportChoices::default().validate().unwrap();
        assert_eq!(plan.book_pages, None);
        assert_eq!(
            plan.book_preset,
            triptape_media::render_book::BookPreset::P1
        );
        assert!(plan.live_collages);
        assert_eq!(plan.parent, triptape_app::default_export_parent());

        let dir = tempfile::tempdir().unwrap();
        let picked = ExportChoices {
            destination: Some(dir.path().to_string_lossy().into_owned()),
            book_pages: Some(111),
            book_preset: Some("P2".into()),
            live_collages: Some(false),
        }
        .validate()
        .unwrap();
        assert_eq!(picked.parent, dir.path());
        assert_eq!(picked.book_pages, Some(111));
        assert_eq!(
            picked.book_preset,
            triptape_media::render_book::BookPreset::P2
        );
        let bad_preset = ExportChoices {
            book_preset: Some("P9".into()),
            ..Default::default()
        };
        assert!(bad_preset.validate().is_err());
        assert!(!picked.live_collages);

        let bad_pages = ExportChoices {
            book_pages: Some(5),
            ..Default::default()
        };
        assert!(bad_pages.validate().is_err());
        let bad_folder = ExportChoices {
            destination: Some("not/absolute".into()),
            ..Default::default()
        };
        assert!(bad_folder.validate().is_err());
    }

    #[test]
    fn each_export_kind_renders_only_its_keepsake() {
        assert_eq!(keepsakes_of("render-video"), "video");
        assert_eq!(keepsakes_of("render-reels"), "reels");
        assert_eq!(keepsakes_of("render-book"), "book");
        assert_eq!(keepsakes_of("export-all"), "all");
    }

    /// The parallel driver must cover every asset exactly once and agree
    /// with serial scoring. Videos score without touching disk or decoders;
    /// photos run against a warmed cache holding undecodable bytes, so they
    /// deterministically take the neutral fallback — no ffmpeg needed.
    #[test]
    fn parallel_scoring_agrees_with_serial() {
        let scratch = tempfile::tempdir().unwrap();
        let dir = scratch.path();
        let cache = dir.join("cache");
        let mut assets = vec![];
        for i in 0..12 {
            let path = dir.join(format!("clip{i:02}.mov"));
            let mut a = Asset::new(format!("v{i:02}"), path, AssetKind::Video);
            if i % 3 == 0 {
                a.gps = Some((35.0, 139.0));
            }
            assets.push(a);
        }
        for i in 0..4 {
            let path = dir.join(format!("photo{i:02}.heic"));
            std::fs::write(&path, b"fixture").unwrap();
            let key = triptape_core::proxies::source_cache_key(&format!("p{i:02}"), &path).unwrap();
            let dst = triptape_core::proxies::cache_path(&cache, "thumb", &key, ".jpg");
            std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
            std::fs::write(&dst, b"not an image").unwrap();
            assets.push(Asset::new(format!("p{i:02}"), path, AssetKind::Photo));
        }
        let run = |workers: usize| {
            let mut out = vec![];
            let mut dones = vec![];
            score_assets_parallel(
                &cache,
                &assets,
                &LocalModels::none(),
                workers,
                |a, s, done, total| {
                    assert_eq!(total, assets.len());
                    out.push((a.id.clone(), s));
                    dones.push(done);
                },
            );
            assert_eq!(dones, (1..=assets.len()).collect::<Vec<_>>());
            out.sort_by(|a, b| a.0.cmp(&b.0));
            out
        };
        assert_eq!(run(1), run(4));
        // Full coverage, no duplicates: every asset exactly once.
        let covered: Vec<String> = run(4).into_iter().map(|(id, _)| id).collect();
        let mut expected: Vec<String> = assets.iter().map(|a| a.id.clone()).collect();
        expected.sort_unstable();
        assert_eq!(covered, expected);
    }
}
