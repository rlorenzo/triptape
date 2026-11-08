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
use triptape_app::{
    apply_live_photo_pairing, default_export_dir, fill_foreground_metadata, paginate,
    valid_job_kind, AppState, AssetView, ExportSummary, JobView, ScanSummary, StoryData,
    StoryEntryView, StoryView,
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
fn report_ingest(app: &tauri::AppHandle, db: &Db, job_id: &str, progress: f32, step: &str) {
    let _ = db.update_job(job_id, JobState::Running, progress, Some(step), None);
    if let Ok(Some(job)) = db.get_job(job_id) {
        emit(app, &job);
    }
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

/// One asset's background scoring result.
#[derive(Debug, Clone, PartialEq)]
struct Scored {
    score: Score,
    /// Perceptual hash of the pixels scored (thumbnail or poster frame);
    /// `None` when nothing decoded.
    phash: Option<u64>,
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
/// caption signals) and keep that window. Pure per-file work: never fails,
/// never touches the Db.
fn score_asset_with_cache(cache_dir: &Path, a: &Asset) -> Scored {
    let signals = score::StorySignals::for_asset(a);
    let thumb = proxies::source_cache_key(&a.id, &a.path)
        .ok()
        .map(|key| proxies::cache_path(cache_dir, "thumb", &key, ".jpg"))
        .filter(|dst| triptape_media::ffmpeg::render_thumb("ffmpeg", &a.path, a.kind, dst).is_ok())
        .and_then(|dst| hash::open_image(&dst));
    if a.kind == AssetKind::Video {
        // Any failure keeps the start-of-clip trim and a neutral quality.
        let analysis = a.duration_secs.map_or_else(Default::default, |d| {
            triptape_media::clip_analysis::analyze_clip("ffmpeg", "ffprobe", &a.path, d)
        });
        return Scored {
            score: analysis.score(&signals),
            phash: thumb.as_ref().map(hash::average_hash_from_image),
        };
    }
    let pixels = thumb.or_else(|| hash::open_image(&a.path));
    Scored {
        score: score::score_still_or_fallback(pixels.as_ref(), &signals),
        phash: pixels.as_ref().map(hash::average_hash_from_image),
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
    workers: usize,
    mut on_score: impl FnMut(&Asset, Scored, usize, usize),
) {
    triptape_core::parallel::for_each_parallel(
        assets,
        workers,
        |a| score_asset_with_cache(cache_dir, a),
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
        let mut pending: Vec<(String, Option<String>)> = vec![];
        let mut last_flush = std::time::Instant::now();
        // One short transaction for the pending hashes. On failure the
        // entries stay queued and surface at the final flush; callers keep
        // every lock hold to this batch only.
        let flush = |pending: &mut Vec<(String, Option<String>)>| -> Result<(), String> {
            if pending.is_empty() {
                return Ok(());
            }
            let db = lock()?;
            db.in_transaction(|db| {
                for (id, digest) in pending.iter() {
                    db.update_asset_hash(id, digest.as_deref())?;
                }
                Ok::<(), String>(())
            })?;
            pending.clear();
            Ok(())
        };
        // Reuse pass: cache hits never touch disk. Their digests queue into
        // the same batched flush and count up front (bytes + done count),
        // keeping progress monotonic across the reuse + hashing phases.
        let mut misses: Vec<(String, PathBuf, u64)> = Vec::new();
        for (id, path, size, mtime) in files {
            let key = (path.to_string_lossy().into_owned(), *size, *mtime);
            match cache.get(&key) {
                Some(digest) => {
                    bytes_done += *size;
                    done += 1;
                    pending.push((id.clone(), Some(digest.clone())));
                    if pending.len() >= HASH_FLUSH_FILES && flush(&mut pending).is_ok() {
                        last_flush = std::time::Instant::now();
                    }
                }
                None => misses.push((id.clone(), path.clone(), *size)),
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
                pending.push((misses[idx].0.clone(), digest));
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
        // score (restored projects) are skipped but still feed the plan.
        let cache_dir = app.path().app_cache_dir().map_err(|e| e.to_string())?;
        let unscored: Vec<Asset> = lock()?
            .list_assets(project_id)?
            .into_iter()
            .filter(|a| a.score.is_none())
            .collect();
        let score_total = unscored.len();
        if score_total > 0 {
            let flush_scores =
                |pending: &mut Vec<(String, Score, Option<u64>)>| -> Result<(), String> {
                    if pending.is_empty() {
                        return Ok(());
                    }
                    let db = lock()?;
                    db.in_transaction(|db| {
                        for (id, s, phash) in pending.iter() {
                            db.update_asset_scoring(id, s, *phash)?;
                        }
                        Ok::<(), String>(())
                    })?;
                    pending.clear();
                    Ok(())
                };
            let mut pending_scores: Vec<(String, Score, Option<u64>)> = vec![];
            // Decode/score work fans out over worker threads; persists and
            // progress stay on this thread via the completion callback,
            // which fires once per scored asset in completion order.
            let workers = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(SCORE_WORKERS_MAX)
                .min(SCORE_WORKERS_MAX);
            score_assets_parallel(&cache_dir, &unscored, workers, |a, s, done, total| {
                pending_scores.push((a.id.clone(), s.score, s.phash));
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
            });
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
        let clusters = cluster::cluster_assets(&assets);
        let cluster_of: HashMap<String, String> = clusters
            .iter()
            .flat_map(|c| c.asset_ids.iter().map(|id| (id.clone(), c.label.clone())))
            .collect();
        // The plan reads the freshly persisted scores (already-scored
        // restores contribute theirs too), so hero trim/cap match CLI use.
        let scores: HashMap<String, Score> = assets
            .iter()
            .filter_map(|a| a.score.clone().map(|s| (a.id.clone(), s)))
            .collect();
        // Near-duplicates (same moment, similar pixels) stay in the library
        // but only the best shot of each stack enters the plan, as in the CLI.
        let suppressed = cluster::suppressed_ids(&cluster::collapse_near_dupes(&assets, &scores));
        assets.retain(|a| !suppressed.contains(&a.id));
        let plan = planmod::build_plan(&assets, &cluster_of, &scores);
        let db = lock()?;
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
/// Asset ids are enumeration-index based (`{pid}_a{i:06}` in walk order),
/// so an unchanged folder yields identical ids across rescans: Story edits
/// (`plan_edits`, keyed by asset/chapter ids) are never touched here and
/// keep applying to the refreshed assets and plans. `files_seen` is the
/// enumerated file count, as in the picker flow.
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
    // Create the ingest job first so progress has a job to attach to, and
    // snapshot the hash-reuse cache BEFORE the foreground persist below
    // overwrites the old hashes (foreground upserts write content_hash
    // None). On refresh the snapshot still carries the previous scan's
    // digests, so unchanged files skip disk reads in the background task.
    let job_id = {
        let db = state.db.lock().map_err(|e| e.to_string())?;
        db.insert_job(&project.id, "ingest", None)?.id
    };
    let cache = {
        let db = state.db.lock().map_err(|e| e.to_string())?;
        db.hash_reuse_cache()?
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
                // EXIF capture time / GPS (photos) or ffprobe truth (videos)
                // when readable; mtime + `time_guessed` otherwise. Never
                // fails the file: clustering and the initial plan below run
                // on real times/GPS. No pixel decoding on this path.
                fill_foreground_metadata(&mut a);
                assets.push(a);
            }
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
            // Short foreground phases; the background task continues from
            // `FOREGROUND_DONE_PROGRESS` once the command returns.
            if let Ok(db) = lock() {
                report_ingest(&worker_app, &db, &worker_job, 0.05, "Grouping photos…");
                last_progress = 0.05;
            }
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
            if let Ok(db) = lock() {
                report_ingest(&worker_app, &db, &worker_job, 0.10, "Building story plan…");
                last_progress = 0.10;
            }
            let db = lock()?;
            db.in_transaction(|db| {
                if refresh {
                    // In-place refresh: drop the previous cluster graph
                    // before re-inserting (cluster ids are project-scoped,
                    // so the old links/clusters would otherwise linger
                    // next to the rebuilt ones).
                    db.clear_project_clusters(&pid)?;
                }
                for a in &assets {
                    db.upsert_asset(&pid, a)?;
                }
                if refresh {
                    // Prune rows whose files left the folder. Kept ids
                    // upsert in place above (identical ids for unchanged
                    // folders), so `plan_edits` keep applying.
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
    // The reuse cache was snapshotted before the foreground persist (above),
    // so refresh hits for unchanged files skip disk reads. The background
    // task never holds the Db mutex while hashing.
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
        let needs_scoring = assets.iter().any(|a| a.score.is_none());
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
    let plan = db
        .edited_plan(Some(project_id))?
        .ok_or_else(|| "no plan for project yet — scan first".to_string())?;
    let name = db
        .get_project(&plan.project_id)?
        .map(|p| p.name)
        .unwrap_or_else(|| "trip".to_string());
    Ok((plan.edited, plan.assets, name))
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
        triptape_media::ffmpeg::render_thumb("ffmpeg", &src, kind, &dst)
            .ok()
            .map(|()| dst.to_string_lossy().into_owned())
    })
    .await
    .map_err(|e| e.to_string())
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
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            scan_folder,
            rescan_project,
            restore_latest_project,
            get_assets,
            start_job,
            cancel_job,
            export_video,
            export_reels,
            export_book,
            export_all,
            get_story,
            get_alternatives,
            set_chapter_place,
            set_asset_caption,
            set_asset_excluded,
            swap_asset,
            get_thumbnail
        ])
        .run(tauri::generate_context!())
        .expect("error while running TripTape");
}

#[cfg(test)]
mod tests {
    use super::*;

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
            score_assets_parallel(&cache, &assets, workers, |a, s, done, total| {
                assert_eq!(total, assets.len());
                out.push((a.id.clone(), s));
                dones.push(done);
            });
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
