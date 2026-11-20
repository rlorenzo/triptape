//! Headless TripTape CLI: `scan` a folder into SQLite, `plan` a story,
//! `export` all eligible keepsakes (hero film, reels pack, photo book).
//!
//! ```sh
//! triptape scan --folder /path/to/trip --db trip.db --cache /tmp/cache [--length auto|SECS|10m] [--models DIR]
//! triptape plan --db trip.db --out plan.json [--home LAT,LON] [--itinerary itinerary.json]
//! triptape edit --db trip.db [--project ID] [--include REF...] [--exclude REF...] [--uninclude REF...] [--unexclude REF...]
//!                [--solo REF...] [--unsolo REF...] [--join REF REF]... [--unjoin REF...] [--list]
//! triptape learn --db trip.db [--project ID] [--show | --backfill | --reset | --apply [--length auto|SECS|10m] [--place-override SPEC]]
//! triptape export --db trip.db --out /path/to/out [--preset P1|P2] [--pages N] [--home LAT,LON] [--music F]... (repeat or ';'-join for a crossfaded playlist) [--keep-ambience] [--no-live-collages] [--itinerary itinerary.json] [--resolution 4k|1080p] [--jobs N] [--quiet]
//!                 [--segment-cache DIR | --no-segment-cache] [--segment-cache-max SIZE] [--segment-cache-min-free SIZE]
//! triptape cache stats|clear [--segment-cache DIR]
//! triptape pick --db trip.db --out /path/to/new-folder [--count 800] [--project ID] [--stills-only]
//! ```
//!
//! Without `--home` or `--segment-cache-max`, `plan`, `export` and `cache`
//! use the home and limit saved in the app's Settings (`TRIPTAPE_SETTINGS`
//! names another settings file).
//!
//! `scan` and `export` take `--ffmpeg`, `--ffprobe` (and `export` `--typst`)
//! paths; without them each tool comes from `TRIPTAPE_FFMPEG` and friends,
//! then the bundled sidecar beside this binary, then `PATH` (with a note).

use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::path::{Path, PathBuf};
use triptape_ai_local::local::{LocalModels, ModelPaths};
use triptape_core::plan::PlanEdits;
use triptape_core::{
    asset_ref, cluster,
    db::{Db, EditedPlan},
    hash, jobs, learn, livephoto, metadata,
    osxphotos::OsxPhotosIndex,
    pick, plan as planmod, privacy,
    process::{resolve_tool, Tool},
    proxies, scan, score,
    settings::Settings,
    types::*,
};
use triptape_media::{
    clip_analysis,
    export::export_all,
    export::ExportOptions,
    ffmpeg,
    progress::{ConsoleProgress, Progress},
    render_book::BookPreset,
    resolution::Resolution,
    segment_cache::{self, SegmentCacheConfig},
};

/// Repeatable flags accumulate `;`-joined (`--music A --music B` is
/// `--music "A;B"`); every other flag keeps its last value.
fn insert_flag(flags: &mut HashMap<String, String>, k: String, v: String) {
    if k == "music" {
        if let Some(prev) = flags.get_mut(&k) {
            prev.push(';');
            prev.push_str(&v);
            return;
        }
    }
    flags.insert(k, v);
}

/// Split a `--music` value into playlist paths. A value that is itself an
/// existing file is taken whole (so a `;` in a filename still works).
fn music_list(v: &str) -> Vec<PathBuf> {
    if Path::new(v).exists() {
        return vec![PathBuf::from(v)];
    }
    v.split(';')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn parse_args() -> (String, HashMap<String, String>) {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().unwrap_or_else(|| {
        eprintln!("usage: triptape <scan|plan|export> [--flag value]...");
        std::process::exit(2);
    });
    let mut flags = HashMap::new();
    let mut pending: Option<String> = None;
    for a in it {
        if let Some(k) = a.strip_prefix("--") {
            // A flag directly after a valueless boolean flag must not
            // swallow it as its value: `--keep-ambience --out x`.
            if let Some(p) = pending.take() {
                insert_flag(&mut flags, p, "true".to_string());
            }
            pending = Some(k.to_string());
        } else if let Some(p) = pending.take() {
            insert_flag(&mut flags, p, a);
        }
    }
    // Trailing valueless flag: `--export --keep-ambience`.
    if let Some(p) = pending.take() {
        flags.insert(p, "true".to_string());
    }
    (cmd, flags)
}

/// Print `ctx: e` and exit 1.
fn die(ctx: &str, e: impl Display) -> ! {
    eprintln!("{ctx}: {e}");
    std::process::exit(1);
}

fn need(flags: &HashMap<String, String>, k: &str) -> PathBuf {
    flags
        .get(k)
        .unwrap_or_else(|| {
            eprintln!("missing --{k}");
            std::process::exit(2);
        })
        .into()
}

/// The plan-shaping flags shared by `scan` and `learn --apply`:
/// `--place-override` (user place labels for GPS-unnamed days, e.g. a ship
/// name for days at sea: `YYYY-MM-DD..YYYY-MM-DD=Label` entries split by
/// `;`) and `--length` (`auto`, seconds, or e.g. `10m`; default auto, so the
/// hero film scales with the trip).
fn plan_flags(
    flags: &HashMap<String, String>,
) -> (Vec<planmod::PlaceOverride>, planmod::FilmLength) {
    let overrides = match flags.get("place-override") {
        Some(spec) => {
            planmod::parse_place_overrides(spec).unwrap_or_else(|e| die("place-override", e))
        }
        None => vec![],
    };
    let length = match flags.get("length") {
        Some(spec) => planmod::parse_film_length(spec).unwrap_or_else(|e| die("length", e)),
        None => planmod::FilmLength::Auto,
    };
    (overrides, length)
}

fn flag_or(flags: &HashMap<String, String>, k: &str, default: &str) -> String {
    flags
        .get(k)
        .map_or_else(|| default.to_string(), Clone::clone)
}

/// The program to run for `tool`: its `--ffmpeg`/`--ffprobe`/`--typst` flag,
/// then `TRIPTAPE_*`, the bundled sidecar beside this binary, then `PATH`.
/// A `PATH` or missing tool is noted on stderr.
fn tool_bin(flags: &HashMap<String, String>, tool: Tool) -> String {
    let resolved = resolve_tool(tool, flags.get(tool.name()).map(String::as_str));
    if let Some(note) = resolved.note() {
        eprintln!("note: {note}");
    }
    resolved.bin
}

fn scan_cmd(flags: &HashMap<String, String>) {
    let folder = need(flags, "folder");
    let db_path = need(flags, "db");
    let cache = need(flags, "cache");
    let ffmpeg = tool_bin(flags, Tool::Ffmpeg);
    let ffprobe = tool_bin(flags, Tool::Ffprobe);
    // Validate before any work so a bad value never yields a misleading exit 0.
    let allow_partial = match flag_or(flags, "allow-partial", "false").as_str() {
        "true" => true,
        "false" => false,
        other => {
            eprintln!("invalid --allow-partial {other:?}; expected true or false");
            std::process::exit(2);
        }
    };
    // Video proxies are opt-in (see the proxy pass below); validated up front
    // so a typo fails before any scan work.
    let want_proxies = match flag_or(flags, "proxies", "false").as_str() {
        "true" => true,
        "false" => false,
        other => {
            eprintln!("invalid --proxies {other:?}; expected true or false");
            std::process::exit(2);
        }
    };

    // Optional local models (`scripts/fetch-models`): `--models DIR`, else
    // TRIPTAPE_MODELS_DIR, else the app data dir's `models/`. Missing files
    // print one line each and leave the scan exactly as without models.
    let workers = std::thread::available_parallelism().map_or(2, |n| n.get().min(4));
    let model_paths = ModelPaths::from_env(
        flags.get("models").map(Path::new),
        dirs::data_dir().map(|d| d.join("app.triptape.desktop").join("models")),
    );
    let models = LocalModels::load(model_paths.as_ref(), workers);
    for note in models.notes() {
        eprintln!("{note}");
    }

    let db = Db::open(&db_path).unwrap_or_else(|e| die("open db", e));
    let name = folder
        .file_name()
        .map_or_else(|| "trip".into(), |s| s.to_string_lossy());
    let project = db
        .create_project(&name, &folder.to_string_lossy())
        .unwrap_or_else(|e| die("create project", e));

    let result = scan::scan_folder(&folder).unwrap_or_else(|e| die("scan", e));
    // Captions/people from an `osxphotos export` without sidecars (loaded
    // once; absent or unreadable means no fallback, never a failed scan).
    let library = OsxPhotosIndex::load(&folder);

    // Fingerprint + probe per file (each video probed exactly once).
    let mut stills: Vec<livephoto::StillFile> = vec![];
    let mut motions: Vec<livephoto::MotionFile> = vec![];
    let mut probes: HashMap<PathBuf, metadata::FfprobeOutput> = HashMap::new();
    let mut assets: Vec<Asset> = vec![];

    for (i, f) in result.files.iter().enumerate() {
        let content_hash = hash::blake3_file(&f.path).ok();
        if scan::VIDEO_EXTS.contains(&f.ext.as_str()) {
            let probe = metadata::probe_video(&ffprobe, &f.path).ok();
            motions.push(livephoto::MotionFile {
                path: f.path.clone(),
                content_id: probe.as_ref().and_then(|p| p.content_identifier()),
                duration_secs: probe.as_ref().and_then(|p| p.duration_secs()),
                has_video_stream: probe.as_ref().is_none_or(|p| p.has_video_stream()),
            });
            if let Some(p) = probe {
                probes.insert(f.path.clone(), p);
            }
            assets.push(base_asset(
                &project.id,
                i,
                f,
                AssetKind::Video,
                content_hash,
            ));
        } else {
            stills.push(livephoto::StillFile {
                path: f.path.clone(),
                content_id: None, // still UUIDs need container tags; basename fallback covers CLI
            });
            assets.push(base_asset(
                &project.id,
                i,
                f,
                AssetKind::Photo,
                content_hash,
            ));
        }
    }

    // Live Photo pairing over the enumerated stills/motions.
    let pairing = livephoto::pair_live_photos(stills, motions);
    let paired_motion: HashMap<&Path, (&Path, PairingConfidence)> = pairing
        .pairs
        .iter()
        .map(|p| (p.still.as_path(), (p.motion.as_path(), p.confidence)))
        .collect();
    let motion_halves: HashSet<&Path> = pairing.pairs.iter().map(|p| p.motion.as_path()).collect();

    // Enrich + score. One row per Live Photo pair: the still absorbs its
    // motion file and the motion-only row is dropped before any work.
    // Motion-file hashes key Live Photo proxies (the still hash would go
    // stale when only the MOV changes).
    let motion_hash: HashMap<PathBuf, String> = assets
        .iter()
        .filter(|a| motion_halves.contains(a.path.as_path()))
        .filter_map(|a| Some((a.path.clone(), a.content_hash.clone()?)))
        .collect();
    let mut final_assets: Vec<Asset> = vec![];
    let mut scores: HashMap<String, Score> = HashMap::new();
    let mut thumbs = 0usize;
    // Stills score in a parallel pass below, from these pixel sources.
    let mut still_sources: Vec<(usize, StillPixels)> = vec![];
    for mut a in assets {
        if motion_halves.contains(a.path.as_path()) {
            continue;
        }
        let img = match a.kind {
            AssetKind::Video => {
                let probe = probes.get(&a.path);
                metadata::fill_video(&mut a, probe, library.as_ref(), &project.id);
                None
            }
            _ => {
                let img = image::open(&a.path).ok();
                fill_photo(&mut a, img.as_ref(), library.as_ref(), &project.id);
                img
            }
        };
        if let Some(&(motion, conf)) = paired_motion.get(a.path.as_path()) {
            a.kind = AssetKind::LivePhoto;
            a.companion_path = Some(motion.to_path_buf());
            a.pairing_confidence = Some(conf);
            if let Some(p) = probes.get(motion) {
                a.duration_secs = p.duration_secs();
            }
        }
        // Every asset gets a perceptual hash from the pixels the scan
        // already has (decoded photo) or the thumbnail/poster it makes
        // (HEIC, Live Photos, videos), so near-dupes collapse everywhere.
        if a.kind == AssetKind::Video {
            let poster =
                make_thumb(&a, &cache, &ffmpeg, &mut thumbs).and_then(|p| hash::open_image(&p));
            a.phash = a
                .phash
                .or(poster.as_ref().map(hash::average_hash_from_image));
            let signals = score::StorySignals::for_asset(&a);
            scores.insert(a.id.clone(), score::score_video(None, &signals));
        } else {
            let source = match img {
                Some(_) => StillPixels::Original,
                None => make_thumb(&a, &cache, &ffmpeg, &mut thumbs)
                    .map_or(StillPixels::Nothing, StillPixels::Thumb),
            };
            still_sources.push((final_assets.len(), source));
        }
        final_assets.push(a);
    }

    // Score stills in parallel from the same pixels as before (the decoded
    // photo, or its thumbnail), adding the optional model outputs.
    let started = std::time::Instant::now();
    let mut stills_done: Vec<(usize, Analysis)> = vec![];
    triptape_core::parallel::for_each_parallel(
        &still_sources,
        workers,
        |(idx, source)| analyze_still(&final_assets[*idx], source, &models),
        |i, analysis, _, _| {
            let idx = still_sources[i].0;
            let a = &final_assets[idx];
            scores.insert(a.id.clone(), analysis.score.clone());
            stills_done.push((idx, analysis));
        },
    );
    for (idx, analysis) in stills_done.drain(..) {
        let a = &mut final_assets[idx];
        a.phash = a.phash.or(analysis.phash);
        a.embedding = analysis.embedding;
        a.faces = analysis.faces;
    }
    let embedded = final_assets
        .iter()
        .filter(|a| a.embedding.is_some())
        .count();
    let with_faces = final_assets
        .iter()
        .filter(|a| a.faces.is_some_and(|f| f.count > 0))
        .count();
    let smiling = final_assets
        .iter()
        .filter(|a| {
            a.faces
                .and_then(|f| f.smile)
                .is_some_and(|s| s >= score::SMILE_MIN)
        })
        .count();
    for (name, runs, ms) in models.timings() {
        println!("model {name}: {runs} runs, {ms:.1} ms/run");
    }

    // Best playback window per video, analysed in parallel (ffmpeg-bound).
    // The clip's score follows the quality of the window that will play.
    let videos: Vec<(String, PathBuf, f64, score::StorySignals)> = final_assets
        .iter()
        .filter(|a| a.kind == AssetKind::Video)
        .filter_map(|a| {
            Some((
                a.id.clone(),
                a.path.clone(),
                a.duration_secs?,
                score::StorySignals::for_asset(a),
            ))
        })
        .collect();
    let workers = std::thread::available_parallelism().map_or(2, |n| n.get().min(4));
    triptape_core::parallel::for_each_parallel(
        &videos,
        workers,
        |(_, path, dur, _)| clip_analysis::analyze_clip(&ffmpeg, &ffprobe, path, *dur),
        |idx, analysis, _, _| {
            let (id, _, _, signals) = &videos[idx];
            scores.insert(id.clone(), analysis.score(signals));
        },
    );

    // Capture timezones: assets without an EXIF offset inherit the
    // nearest dated neighbor's, so day grouping and displayed times are
    // in the photo's local time, not UTC.
    metadata::fill_tz_offsets(&mut final_assets);

    // Cluster + near-dupe hero picks.
    // Cluster ids restart per scan; prefix with the project id so they stay
    // unique across projects sharing one database.
    let mut clusters = cluster::cluster_assets(&final_assets);
    for c in &mut clusters {
        c.id = format!("{}_{}", project.id, c.id);
    }
    // The same file in the library twice: its copies share their caption,
    // people tags and score, so the one kept below carries all of them.
    cluster::share_identical_metadata(&mut final_assets, &mut scores);
    let groups = cluster::collapse_near_dupes(&final_assets, &scores);
    let best: HashSet<String> = groups.iter().map(|g| g.best_id.clone()).collect();
    // Non-best near-dupe members stay in the DB but are kept out of the plan.
    let suppressed = cluster::suppressed_ids(&groups);

    // Store scores (including clip windows) on the asset rows so later
    // `Db::edited_plan` reloads see the same windows as this initial plan.
    for a in &mut final_assets {
        a.score = scores.get(&a.id).cloned();
    }

    // Persist assets, clusters, and links in one transaction.
    let persisted = db.in_transaction(|db| {
        for a in &final_assets {
            db.upsert_asset(&project.id, a)
                .map_err(|e| format!("upsert {}: {e}", a.id))?;
        }
        for c in &clusters {
            db.insert_cluster(&project.id, "day", c)
                .map_err(|e| format!("cluster {}: {e}", c.id))?;
            for id in &c.asset_ids {
                db.link_asset_cluster(id, &c.id, best.contains(id))
                    .map_err(|e| format!("link {id} -> {}: {e}", c.id))?;
            }
        }
        Ok(())
    });
    if let Err(e) = persisted {
        die("persist", e);
    }

    // Proxies for videos and Live Photo motion files are generated lazily
    // (PLAN Sec 8 Stage 4): a scan must stay fast on large folders, so video
    // proxies are built only with --proxies true; thumbnails/posters above
    // already cover Library previews. Without --proxies true the pass is
    // skipped and the reported proxy count is 0, even if cached proxies exist.
    let mut proxies_n = 0usize;
    for a in final_assets
        .iter()
        .filter(|a| a.kind != AssetKind::Photo && want_proxies)
    {
        let src = a.companion_path.as_ref().unwrap_or(&a.path);
        let key = match &a.companion_path {
            Some(m) => motion_hash.get(m).map(String::as_str),
            None => a.content_hash.as_deref(),
        };
        if let Some(h) = key {
            let dst = proxies::cache_path(&cache, "proxy", h, ".mp4");
            let audio = ffmpeg::audio_map(&ffprobe, src);
            let args = proxies::proxy_args(src, &dst, audio.as_deref());
            if proxies::run_ffmpeg(&ffmpeg, &args, &dst).is_ok() {
                proxies_n += 1;
            }
        }
    }

    let cluster_of = cluster::chapter_labels(&clusters);
    let plan_assets: Vec<Asset> = final_assets
        .iter()
        .filter(|a| !suppressed.contains(&a.id))
        .cloned()
        .collect();
    let (overrides, length) = plan_flags(flags);
    let p = planmod::build_plan_with_length(&plan_assets, &cluster_of, &scores, &overrides, length);
    let elig = planmod::plan_eligibility(
        plan_assets.len(),
        plan_assets
            .iter()
            .filter(|a| a.kind != AssetKind::Video)
            .count(),
        clusters.len(),
    );
    let plan_id = db
        .insert_plan(&project.id, &p, "local")
        .unwrap_or_else(|e| die("insert plan", e));

    println!("project: {}", project.id);
    println!("files: {}", result.files.len());
    println!("sidecars_skipped: {}", result.sidecars_skipped);
    println!("hidden_skipped: {}", result.hidden_skipped);
    println!("unsupported_skipped: {}", result.unsupported_skipped);
    println!("pairs: {}", pairing.pairs.len());
    println!("assets: {}", final_assets.len());
    println!("clusters: {}", clusters.len());
    println!(
        "near-dupes: {} groups, {} suppressed",
        groups.iter().filter(|g| g.members.len() > 1).count(),
        suppressed.len()
    );
    println!("thumbs: {thumbs} proxies: {proxies_n}");
    println!(
        "stills: {} scored in {:.1}s, embeddings: {embedded}, with faces: {with_faces}, smiling: {smiling}",
        still_sources.len(),
        started.elapsed().as_secs_f64()
    );
    println!("hero: {} reels: {}", p.hero.len(), p.reel_selects.len());
    println!(
        "plan: {plan_id} video={} reels={} book={}",
        elig.video, elig.reels, elig.book_enabled
    );
    if result.is_partial() {
        println!("partial: {} errors", result.errors.len());
        for e in &result.errors {
            let path = e
                .path
                .as_deref()
                .map_or_else(|| "<unknown path>".into(), |p| p.display().to_string());
            eprintln!("scan error: {path}: {}", e.message);
        }
        // The plan is saved, but automation must not mistake it for complete.
        if !allow_partial {
            eprintln!("scan incomplete; pass --allow-partial true to accept");
            std::process::exit(3);
        }
    }
}

fn base_asset(
    project_id: &str,
    i: usize,
    f: &scan::ScannedFile,
    kind: AssetKind,
    content_hash: Option<String>,
) -> Asset {
    // Scan-local index prefixed by project id: `assets.id` is database-wide.
    let mut a = Asset::new(format!("{project_id}_a{i:05}"), f.path.clone(), kind);
    a.size_bytes = f.size_bytes;
    a.mtime = f.mtime_secs;
    a.content_hash = content_hash;
    a
}

/// Fill photo metadata from EXIF; dimensions + pHash from the decoded
/// pixels when decodable (non-HEIC; HEIC proxies cover the rest).
/// `library` and `salt` as in [`metadata::fill_photo`].
fn fill_photo(
    a: &mut Asset,
    img: Option<&image::DynamicImage>,
    library: Option<&OsxPhotosIndex>,
    salt: &str,
) {
    let exif = metadata::read_image_exif(&a.path);
    // EXIF size covers HEIC and other undecodable stills; decoded pixels
    // override it when available.
    metadata::fill_photo(a, &exif, library, salt);
    if let Some(img) = img {
        a.width = Some(img.width());
        a.height = Some(img.height());
        a.phash = Some(hash::average_hash_from_image(img));
    }
}

/// Make (or reuse) the cached FFmpeg thumbnail of `a`, counted in
/// `thumbs`: the scoring pixels of an undecodable still (HEIC, Live Photo)
/// and a video's poster frame. `None` without a content hash or on failure.
fn make_thumb(a: &Asset, cache: &Path, ffmpeg: &str, thumbs: &mut usize) -> Option<PathBuf> {
    let h = a.content_hash.as_deref()?;
    let dst = proxies::cache_path(cache, "thumb", h, ".jpg");
    proxies::run_ffmpeg(ffmpeg, &proxies::thumb_args(&a.path, &dst), &dst).ok()?;
    *thumbs += 1;
    Some(dst)
}

/// Where a still's scoring pixels come from.
enum StillPixels {
    /// The photo itself decodes (JPEG, PNG, ...).
    Original,
    /// Its cached FFmpeg thumbnail (HEIC, Live Photo stills).
    Thumb(PathBuf),
    /// Nothing decodable: neutral fallback score.
    Nothing,
}

/// Score and fingerprint one still from `source`
/// ([`LocalModels::analyze_still`]).
fn analyze_still(a: &Asset, source: &StillPixels, models: &LocalModels) -> Analysis {
    match source {
        StillPixels::Original => models.analyze_still(a, hash::open_image(&a.path).as_ref(), true),
        StillPixels::Thumb(p) => models.analyze_still(a, hash::open_image(p).as_ref(), false),
        StillPixels::Nothing => models.analyze_still(a, None, false),
    }
}

/// `--itinerary <json>`: the owner's itinerary, which relabels chapters
/// (city, or the ship when on board) and drives the closing map.
fn load_itinerary(flags: &HashMap<String, String>) -> Option<triptape_core::itinerary::Itinerary> {
    let path = flags.get("itinerary")?;
    Some(
        triptape_core::itinerary::Itinerary::load(std::path::Path::new(path))
            .unwrap_or_else(|e| die("itinerary", e)),
    )
}

fn plan_cmd(flags: &HashMap<String, String>) {
    let db_path = need(flags, "db");
    let out = PathBuf::from(flag_or(flags, "out", "plan.json"));
    let db = Db::open(&db_path).unwrap_or_else(|e| die("open db", e));
    // Default (no --project): latest plan in the db, for single-project DBs.
    let itinerary = load_itinerary(flags);
    let mut p = db
        .edited_plan_with(flags.get("project").map(String::as_str), itinerary.as_ref())
        .unwrap_or_else(|e| die("plan", e))
        .unwrap_or_else(|| die("plan", "no plan in db (run scan first)"))
        .edited;
    // Exported plans fuzz route points near home (PLAN.md Sec 14 privacy).
    hide_home(flags, &mut p.map_polyline);
    let json = p.to_json().unwrap_or_else(|e| die("plan json", e));
    std::fs::write(&out, &json).unwrap_or_else(|e| die(&format!("write {}", out.display()), e));
    println!(
        "wrote {} (hero={} reels={})",
        out.display(),
        p.hero.len(),
        p.reel_selects.len()
    );
}

/// The app's saved settings (home on maps, segment cache limit), which
/// apply when the matching flag is not given. `TRIPTAPE_SETTINGS` points
/// elsewhere; a missing file means the defaults. Read once per run.
fn saved_settings() -> &'static Settings {
    static SAVED: std::sync::OnceLock<Settings> = std::sync::OnceLock::new();
    SAVED.get_or_init(|| {
        Settings::default_path()
            .map(|p| Settings::load(&p))
            .unwrap_or_default()
    })
}

/// Fuzz `route` near home: `--home LAT,LON`, else the home saved in the
/// app's Settings while "Hide home on maps" is on.
fn hide_home(flags: &HashMap<String, String>, route: &mut Vec<(f64, f64)>) {
    if let Some(h) = flags.get("home") {
        let home = parse_lat_lon(h).unwrap_or_else(|| die("--home", "want LAT,LON"));
        *route = privacy::fuzz_home_route(route, home);
    } else if saved_settings().hide_home_on(route) {
        eprintln!("note: home hidden on maps (from the app's settings)");
    }
}

/// Parse `"lat,lon"` into a coordinate pair.
fn parse_lat_lon(s: &str) -> Option<(f64, f64)> {
    let (lat, lon) = s.split_once(',')?;
    let lat: f64 = lat.trim().parse().ok()?;
    let lon: f64 = lon.trim().parse().ok()?;
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}

fn export_cmd(flags: &HashMap<String, String>) {
    let db_path = need(flags, "db");
    let out = need(flags, "out");
    let preset = BookPreset::from_flag(&flag_or(flags, "preset", "P1"))
        .unwrap_or_else(|| die("preset", "want P1|P2"));
    // Book page cap (vendors differ: Shutterfly 111, Snapfish 150, Mixbook
    // 400); omitted keeps the preset default. Checked before any rendering.
    let book_max_pages = flags.get("pages").map(|v| {
        let n: usize = v
            .trim()
            .parse()
            .unwrap_or_else(|_| die("--pages", "want a whole number of pages"));
        preset
            .page_cap(Some(n))
            .unwrap_or_else(|e| die("--pages", e))
    });
    let ffmpeg_bin = tool_bin(flags, Tool::Ffmpeg);
    let ffprobe_bin = tool_bin(flags, Tool::Ffprobe);
    let typst_bin = tool_bin(flags, Tool::Typst);
    let music = flags.get("music").map_or_else(Vec::new, |v| music_list(v));
    for m in &music {
        if !m.exists() {
            die("music", format!("not found: {}", m.display()));
        }
    }

    let db = Db::open(&db_path).unwrap_or_else(|e| die("open db", e));
    let itinerary = load_itinerary(flags);
    let EditedPlan {
        project_id,
        edited: mut plan,
        assets,
        ..
    } = db
        .edited_plan_with(flags.get("project").map(String::as_str), itinerary.as_ref())
        .unwrap_or_else(|e| die("plan", e))
        .unwrap_or_else(|| die("plan", "no plan in db (run scan first)"));
    // Rendered maps get the same home fuzzing as `plan` JSON (Sec 14).
    hide_home(flags, &mut plan.map_polyline);
    let trip_name = db
        .get_project(&project_id)
        .unwrap_or_else(|e| die("project", e))
        .map(|p| p.name.clone())
        .unwrap_or_else(|| "trip".to_string());

    let caps = ffmpeg::probe_capabilities(&ffmpeg_bin, &ffprobe_bin);
    println!(
        "ffmpeg caps: libx264={} xfade={} perspective={} tonemap={} zscale={} loudnorm={} drawtext={} libass={} vidstab={}",
        caps.libx264,
        caps.xfade,
        caps.perspective,
        caps.tonemap,
        caps.zscale,
        caps.loudnorm,
        caps.drawtext,
        caps.libass,
        caps.vidstabdetect && caps.vidstabtransform,
    );
    println!("ffmpeg: {ffmpeg_bin}");
    for note in caps.degradations() {
        eprintln!("note: {note}");
    }

    // Hero film frame size: 4K unless asked otherwise (reels stay 1080x1920).
    let resolution = Resolution::from_flag(&flag_or(flags, "resolution", "4k"))
        .unwrap_or_else(|e| die("resolution", e));
    let only = flag_or(flags, "only", "all");
    if !matches!(only.as_str(), "all" | "video" | "reels" | "book") {
        die(
            "only",
            format!("unknown keepsake {only:?} (want all|video|reels|book)"),
        );
    }
    // Free-space precheck (the precheck applies its own 1.5x headroom). Only
    // the hero film scales with --resolution; reels and the book keep the
    // 1080p-era estimate.
    let estimate = if matches!(only.as_str(), "all" | "video") {
        resolution.export_disk_estimate()
    } else {
        Resolution::Hd1080.export_disk_estimate()
    };
    if let Err(e) = jobs::free_space_precheck(&out, estimate) {
        die("free space", e);
    }

    let keep_ambience = flags.get("keep-ambience").is_some_and(|v| v != "false");
    // Live Photos move inside hero collages unless switched off.
    let live_collages = !flags.get("no-live-collages").is_some_and(|v| v != "false");
    // Hero segments rendered at once (default: cores / 4, memory permitting).
    let jobs = flags.get("jobs").map(|v| match v.parse::<usize>() {
        Ok(n) if n >= 1 => n,
        _ => die("jobs", format!("want a whole number >= 1, got {v:?}")),
    });
    // Progress goes to stderr so stdout stays the plain result listing.
    let console = std::sync::Arc::new(ConsoleProgress::stderr());
    let progress = if flags.get("quiet").is_some_and(|v| v != "false") {
        Progress::default()
    } else {
        Progress::new(console.clone())
    };
    let segment_cache = segment_cache_config(flags);
    let exported = export_all(
        &caps,
        &plan,
        &assets,
        &out,
        &ExportOptions {
            preset,
            book_max_pages,
            trip_name,
            typst_bin,
            music_paths: music,
            keep_ambience,
            live_collages,
            only,
            resolution,
            jobs,
            progress,
            segment_cache,
            itinerary,
        },
    );
    console.finish();
    let report = exported.unwrap_or_else(|e| die("export", e));

    if let Some(v) = &report.video {
        println!("video: {v}");
    }
    if let Some(c) = &report.segment_cache {
        println!(
            "video: segments reused {}/{} from cache, overlays {}/{} ({} new, {} evicted; {})",
            c.reused, c.total, c.overlays_reused, c.overlays_total, c.stored, c.evicted, c.dir
        );
    }
    for r in &report.reels {
        println!("reel: {r}");
    }
    if report.book_pages > 0 {
        println!("book: {} pages", report.book_pages);
    }
    for s in &report.skipped {
        println!("skipped {}: {}", s.keepsake, s.reason);
    }
    for f in &report.fallbacks {
        println!("fallback: {f}");
    }
    println!("wrote {}", out.join("export.json").display());
}

/// Segment cache for `export`: on by default in the platform cache dir,
/// moved by `--segment-cache DIR`, bounded by `--segment-cache-max SIZE`,
/// off with `--no-segment-cache`.
fn segment_cache_config(flags: &HashMap<String, String>) -> Option<SegmentCacheConfig> {
    let off = flags.get("no-segment-cache").is_some_and(|v| v != "false");
    if off && flags.contains_key("segment-cache") {
        die(
            "segment-cache",
            "--segment-cache and --no-segment-cache conflict",
        );
    }
    let max_bytes = cache_max_flag(flags);
    if off {
        return None;
    }
    let dir = cache_dir_flag(flags);
    if dir.is_none() {
        eprintln!("segment cache off: no cache directory on this platform");
    }
    let min_free_bytes = cache_min_free_flag(flags);
    dir.map(|dir| SegmentCacheConfig {
        max_bytes,
        min_free_bytes,
        ..SegmentCacheConfig::new(dir)
    })
}

/// `--segment-cache DIR`, else the platform default.
fn cache_dir_flag(flags: &HashMap<String, String>) -> Option<PathBuf> {
    flags
        .get("segment-cache")
        .map(PathBuf::from)
        .or_else(segment_cache::default_dir)
}

/// `--segment-cache-max SIZE`, else the limit saved in the app's Settings.
fn cache_max_flag(flags: &HashMap<String, String>) -> Option<u64> {
    flags
        .get("segment-cache-max")
        .map(|v| segment_cache::parse_size(v).unwrap_or_else(|e| die("segment-cache-max", e)))
        .or_else(|| saved_settings().segment_cache_max_bytes)
}

/// `--segment-cache-min-free SIZE`, when given.
fn cache_min_free_flag(flags: &HashMap<String, String>) -> Option<u64> {
    flags
        .get("segment-cache-min-free")
        .map(|v| segment_cache::parse_size(v).unwrap_or_else(|e| die("segment-cache-min-free", e)))
}

/// `cache stats` / `cache clear`.
fn cache_cmd(sub: &str, flags: &HashMap<String, String>) {
    let dir = cache_dir_flag(flags)
        .unwrap_or_else(|| die("cache", "no cache directory on this platform"));
    match sub {
        "stats" => {
            let st = segment_cache::disk_stats(&dir);
            let disk = segment_cache::disk_space(&dir);
            let policy = segment_cache::Policy::resolve(
                cache_max_flag(flags),
                cache_min_free_flag(flags),
                disk,
            );
            let free = disk.map(|d| d.free);
            let size = segment_cache::fmt_bytes;
            println!("segment cache: {}", dir.display());
            println!(
                "entries: {}  size: {}  exports running: {}",
                st.entries,
                size(st.bytes),
                st.live_leases
            );
            println!(
                "limit: {}  free-space floor: {}",
                size(policy.max_bytes),
                size(policy.floor_bytes)
            );
            println!(
                "disk free: {}  headroom: {} (room to grow before the limit or the floor)",
                free.map_or_else(|| "unknown".into(), size),
                size(policy.headroom(st.bytes, free))
            );
        }
        "clear" => {
            let r = segment_cache::clear(&dir);
            println!(
                "removed {} entries ({})",
                r.removed,
                segment_cache::fmt_bytes(r.freed)
            );
            if r.protected > 0 {
                println!(
                    "kept {} entries in use by a running export; clear again when it finishes",
                    r.protected
                );
            }
        }
        other => {
            eprintln!("unknown cache command {other:?}; want stats|clear");
            std::process::exit(2);
        }
    }
}

/// Parsed `edit` arguments. `--include`, `--exclude`, `--uninclude`,
/// `--unexclude`, `--solo`, `--unsolo` and `--unjoin` each take one or more
/// asset references (id, file name or path) and may repeat; `--join` takes
/// exactly two and may repeat.
#[derive(Debug, Default, PartialEq)]
struct EditArgs {
    db: Option<PathBuf>,
    project: Option<String>,
    list: bool,
    include: Vec<String>,
    exclude: Vec<String>,
    uninclude: Vec<String>,
    unexclude: Vec<String>,
    solo: Vec<String>,
    unsolo: Vec<String>,
    join: Vec<(String, String)>,
    unjoin: Vec<String>,
}

fn parse_edit_args(args: &[String]) -> Result<EditArgs, String> {
    let mut out = EditArgs::default();
    let mut i = 0;
    while i < args.len() {
        let flag = args[i]
            .strip_prefix("--")
            .ok_or_else(|| format!("unexpected argument {:?}", args[i]))?;
        i += 1;
        let mut values = vec![];
        while i < args.len() && !args[i].starts_with("--") {
            values.push(args[i].clone());
            i += 1;
        }
        let single = |values: Vec<String>| match <[String; 1]>::try_from(values) {
            Ok([v]) => Ok(v),
            Err(_) => Err(format!("--{flag} wants exactly one value")),
        };
        match flag {
            "db" => out.db = Some(single(values)?.into()),
            "project" => out.project = Some(single(values)?),
            "list" if values.is_empty() => out.list = true,
            "list" => return Err("--list takes no value".to_string()),
            "join" => match <[String; 2]>::try_from(values) {
                Ok([a, b]) => out.join.push((a, b)),
                Err(_) => return Err("--join wants exactly two assets".to_string()),
            },
            "include" | "exclude" | "uninclude" | "unexclude" | "solo" | "unsolo" | "unjoin" => {
                if values.is_empty() {
                    return Err(format!("--{flag} wants at least one asset"));
                }
                match flag {
                    "include" => out.include.extend(values),
                    "exclude" => out.exclude.extend(values),
                    "uninclude" => out.uninclude.extend(values),
                    "unexclude" => out.unexclude.extend(values),
                    "solo" => out.solo.extend(values),
                    "unsolo" => out.unsolo.extend(values),
                    _ => out.unjoin.extend(values),
                }
            }
            other => return Err(format!("unknown flag --{other}")),
        }
    }
    Ok(out)
}

/// File name and id of an asset, for messages.
fn asset_label(a: &Asset) -> String {
    let name = a
        .path
        .file_name()
        .map_or_else(String::new, |f| f.to_string_lossy().into_owned());
    format!("{name} [{}]", a.id)
}

/// Print the Story-editor edits with file names.
fn print_edits(edits: &PlanEdits, by_id: &HashMap<&str, &Asset>) {
    let show = |id: &String| {
        by_id
            .get(id.as_str())
            .map_or_else(|| format!("{id} (not in project)"), |a| asset_label(a))
    };
    for (title, ids) in [("included", &edits.included), ("excluded", &edits.excluded)] {
        println!("{title}: {}", ids.len());
        for id in ids {
            println!("  {}", show(id));
        }
    }
    println!("swaps: {}", edits.swaps.len());
    for (from, to) in &edits.swaps {
        println!("  {} -> {}", show(from), show(to));
    }
    println!(
        "place overrides: {}, caption overrides: {}",
        edits.place_overrides.len(),
        edits.caption_overrides.len()
    );
    let collage = &edits.collage;
    println!("on their own: {}", collage.solo.len());
    for id in &collage.solo {
        println!("  {}", show(id));
    }
    println!("joins: {}", collage.joins.len());
    for (a, b) in &collage.joins {
        println!("  {} + {}", show(a), show(b));
    }
}

/// How the edited film groups, as the renderer will (missing files left
/// out), as hero indices.
fn film_groups(plan: &Plan, assets: &[Asset]) -> Vec<Vec<usize>> {
    planmod::HeroGrouping::from_assets(plan, assets).groups(&plan.collage)
}

/// Print the film's collages and estimated length.
fn print_film(plan: &Plan, assets: &[Asset], by_id: &HashMap<&str, &Asset>, was_secs: Option<f64>) {
    let groups = film_groups(plan, assets);
    let secs = planmod::estimate_plan_secs(plan, assets);
    let collages: Vec<&Vec<usize>> = groups.iter().filter(|g| g.len() > 1).collect();
    let was = was_secs.map_or_else(String::new, |w| format!(" (was {w:.1} s)"));
    println!(
        "film: {} segments, {} collages, about {secs:.1} s{was}",
        groups.len(),
        collages.len()
    );
    for g in collages {
        let names: Vec<String> = g
            .iter()
            .map(|&i| {
                let id = &plan.hero[i].asset_id;
                by_id
                    .get(id.as_str())
                    .map_or_else(|| id.clone(), |a| asset_label(a))
            })
            .collect();
        println!("  collage of {}: {}", g.len(), names.join(", "));
    }
}

fn edit_cmd(args: &[String]) {
    let a = parse_edit_args(args).unwrap_or_else(|e| die("edit", e));
    let db_path = a.db.as_ref().unwrap_or_else(|| die("edit", "missing --db"));
    let db = Db::open(db_path).unwrap_or_else(|e| die("open db", e));
    let ep = db
        .edited_plan(a.project.as_deref())
        .unwrap_or_else(|e| die("plan", e))
        .unwrap_or_else(|| die("plan", "no plan in db (run scan first)"));
    let by_id: HashMap<&str, &Asset> = ep.assets.iter().map(|x| (x.id.as_str(), x)).collect();
    let resolve_one = |q: &str| -> &Asset {
        asset_ref::resolve_asset(&ep.assets, q).unwrap_or_else(|e| die("edit", e))
    };
    let resolve =
        |refs: &[String]| -> Vec<&Asset> { refs.iter().map(|q| resolve_one(q)).collect() };
    // Resolve everything first so a bad reference changes nothing.
    let includes = resolve(&a.include);
    if let Some(asset) = includes
        .iter()
        .find(|x| ep.base.chapter_of(&x.id).is_none())
    {
        die(
            "edit",
            format!("{} is not in any chapter of the plan", asset_label(asset)),
        );
    }
    type Setter = fn(&mut PlanEdits, String, bool);
    let set_solo: Setter = |e, id, on| e.collage.set_solo(id, on);
    let solos = resolve(&a.solo);
    // Un-dos first, then removals, then pins.
    let ops: [(Vec<&Asset>, &str, Setter, bool); 6] = [
        (
            resolve(&a.uninclude),
            "uninclude",
            PlanEdits::set_included,
            false,
        ),
        (
            resolve(&a.unexclude),
            "unexclude",
            PlanEdits::set_excluded,
            false,
        ),
        (
            resolve(&a.exclude),
            "exclude",
            PlanEdits::set_excluded,
            true,
        ),
        (includes, "include", PlanEdits::set_included, true),
        // Collage edits after the picks they apply to.
        (resolve(&a.unsolo), "unsolo", set_solo, false),
        (solos.clone(), "solo", set_solo, true),
    ];
    let unjoins = resolve(&a.unjoin);
    let joins: Vec<(&Asset, &Asset)> = a
        .join
        .iter()
        .map(|(x, y)| (resolve_one(x), resolve_one(y)))
        .collect();
    let changed = ops.iter().any(|(assets, ..)| !assets.is_empty())
        || !unjoins.is_empty()
        || !joins.is_empty();
    if !changed && !a.list {
        die(
            "edit",
            "nothing to do: pass --include/--exclude/--uninclude/--unexclude/--solo/--unsolo/--join/--unjoin or --list",
        );
    }
    let mut edits = ep.edits.clone();
    for (assets, verb, set, on) in &ops {
        for asset in assets {
            set(&mut edits, asset.id.clone(), *on);
            println!("{verb}: {}", asset_label(asset));
        }
    }
    for asset in &unjoins {
        edits.collage.unjoin(&asset.id);
        println!("unjoin: {}", asset_label(asset));
    }
    // Each solo photo must be in the film; each join must leave the two in
    // one collage of the film (neighbouring collage photos of one day).
    // Collage edits never change which photos are in the film.
    let edited = |edits: &PlanEdits| planmod::apply_edits(&ep.base, &ep.assets, edits);
    let mut plan = edited(&edits);
    for asset in &solos {
        if !plan.hero.iter().any(|e| e.asset_id == asset.id) {
            die("edit", format!("{} is not in the film", asset_label(asset)));
        }
    }
    for (x, y) in &joins {
        let at = |a: &Asset| {
            plan.hero
                .iter()
                .position(|e| e.asset_id == a.id)
                .unwrap_or_else(|| die("edit", format!("{} is not in the film", asset_label(a))))
        };
        let (first, second) = if at(x) <= at(y) { (x, y) } else { (y, x) };
        let (i, j) = (at(first), at(second));
        edits.collage.join(first.id.clone(), second.id.clone());
        plan.collage.join(first.id.clone(), second.id.clone());
        let together = film_groups(&plan, &ep.assets)
            .iter()
            .any(|g| g.contains(&i) && g.contains(&j));
        if !together {
            die(
                "edit",
                format!(
                    "{} and {} can't share a collage: they must be neighbouring film photos of one day, \
                     without a caption, not panoramas or videos, in a collage of at most {}",
                    asset_label(first),
                    asset_label(second),
                    planmod::MAX_COLLAGE
                ),
            );
        }
        println!("join: {} + {}", asset_label(first), asset_label(second));
    }
    let was_secs = changed.then(|| planmod::estimate_plan_secs(&ep.edited, &ep.assets));
    if changed {
        db.save_plan_edits(&ep.project_id, &edits)
            .unwrap_or_else(|e| die("save edits", e));
    }
    print_edits(&edits, &by_id);
    print_film(&plan, &ep.assets, &by_id, was_secs);
    if !changed {
        return;
    }
    // Show where each included shot landed in the film.
    let hero = plan.hero;
    let label_at = |j: Option<usize>| {
        j.and_then(|j| hero.get(j))
            .and_then(|e| by_id.get(e.asset_id.as_str()))
            .map_or_else(|| "(start/end)".to_string(), |x| asset_label(x))
    };
    for asset in &ops[3].0 {
        match hero.iter().position(|e| e.asset_id == asset.id) {
            Some(i) => println!(
                "film: {} after {} before {} ({:?})",
                asset_label(asset),
                label_at(i.checked_sub(1)),
                label_at(Some(i + 1)),
                hero[i].rendition
            ),
            None => println!("film: {} is not in the film", asset_label(asset)),
        }
    }
    println!("film now has {} shots", hero.len());
}

/// `learn`: show, reset or apply the story-score weights learned from the
/// owner's Story-editor edits. Never changes a plan unless `--apply`.
fn learn_cmd(flags: &HashMap<String, String>) {
    let db_path = need(flags, "db");
    let db = Db::open(&db_path).unwrap_or_else(|e| die("open db", e));
    let project_id = match flags.get("project") {
        Some(id) => id.clone(),
        None => {
            db.latest_project()
                .unwrap_or_else(|e| die("project", e))
                .unwrap_or_else(|| die("learn", "no project in db"))
                .id
        }
    };
    let on = |k: &str| flags.get(k).is_some_and(|v| v == "true");
    if on("reset") {
        let n = db
            .clear_preference_events(&project_id)
            .unwrap_or_else(|e| die("reset", e));
        println!("project: {project_id}");
        println!("reset: forgot {n} preference events; weights are back to the defaults");
        println!("a plan built earlier keeps its weights until you rebuild it with --apply");
        return;
    }
    if on("backfill") {
        learn::backfill(&db, &project_id).unwrap_or_else(|e| die("backfill", e));
    }
    let summary = learn::project_summary(&db, &project_id).unwrap_or_else(|e| die("learn", e));
    let fit = &summary.fit;
    println!("project: {project_id}");
    println!(
        "events: {} ({} informative; weights adapt from {})",
        fit.events,
        fit.usable,
        learn::MIN_USABLE_EVENTS
    );
    if fit.weights.is_identity() {
        if fit.usable < learn::MIN_USABLE_EVENTS {
            println!(
                "weights: defaults ({} more informative edits needed before anything adapts)",
                learn::MIN_USABLE_EVENTS - fit.usable
            );
        } else {
            println!("weights: defaults (your edits agree with them)");
        }
    } else {
        println!("weights:");
        for def in learn::FEATURES
            .iter()
            .filter(|d| fit.weights.multipliers.contains_key(d.name))
        {
            println!("  {:<16} x{:.2}", def.label, fit.weights.get(def.name));
        }
        for line in &summary.explanations {
            println!("  - {line}");
        }
    }
    if !on("apply") {
        return;
    }
    if fit.weights.is_identity() {
        // Still rebuild: after a reset or undo, the stored plan may carry
        // weights learned earlier.
        println!("apply: rebuilding with the default weights");
    }
    let (overrides, length) = plan_flags(flags);
    let hero_of = |db: &Db| -> Vec<String> {
        db.edited_plan(Some(&project_id))
            .unwrap_or_else(|e| die("plan", e))
            .map(|ep| ep.edited.hero.into_iter().map(|e| e.asset_id).collect())
            .unwrap_or_default()
    };
    let before = hero_of(&db);
    let assets = db
        .list_assets(&project_id)
        .unwrap_or_else(|e| die("assets", e));
    let edits = db
        .get_plan_edits(&project_id)
        .unwrap_or_else(|e| die("edits", e));
    let plan = learn::replan(&assets, &fit.weights, &edits, &overrides, length);
    let plan_id = db
        .insert_plan(&project_id, &plan, "learned")
        .unwrap_or_else(|e| die("insert plan", e));
    let after = hero_of(&db);
    let by_id: HashMap<&str, &Asset> = assets.iter().map(|a| (a.id.as_str(), a)).collect();
    let name = |id: &String| {
        by_id
            .get(id.as_str())
            .map_or_else(|| id.clone(), |a| asset_label(a))
    };
    println!("apply: new plan {plan_id} (your edits carry over)");
    let edited = triptape_core::plan::apply_edits(&plan, &assets, &edits);
    let lost = learn::unapplied_swaps(&edited, &edits);
    if !lost.is_empty() {
        println!(
            "note: {} swap(s) no longer apply: the shot each replaced left the film",
            lost.len()
        );
        for (from, to) in lost.iter().take(10) {
            println!("  {} -> {} (pin it to keep it)", name(from), name(to));
        }
    }
    println!("film: {} shots before, {} now", before.len(), after.len());
    if !flags.contains_key("length") {
        println!("note: film length was rebuilt as auto; pass --length to match your scan");
    }
    for (verb, from, to) in [
        ("now in", &before, &after),
        ("dropped from", &after, &before),
    ] {
        let ids: Vec<&String> = to.iter().filter(|id| !from.contains(id)).collect();
        println!("{verb} the film: {}", ids.len());
        for id in ids.iter().take(10) {
            println!("  {}", name(id));
        }
    }
}

/// Photos `pick` copies when `--count` is not given.
const DEFAULT_PICK_COUNT: usize = 800;

/// A file name in `dir` for `src` that no earlier copy took: `IMG_1.HEIC`,
/// then `IMG_1-2.HEIC`, ... (two source folders can hold the same name).
#[cfg(test)]
fn free_name(src: &Path, taken: &mut HashSet<String>) -> String {
    free_names(src, None, taken).0
}

/// `src`'s file name split into stem and extension (with its dot).
fn stem_ext(src: &Path) -> (String, String) {
    let name = src
        .file_name()
        .map_or_else(|| "photo".to_string(), |n| n.to_string_lossy().into_owned());
    match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name, String::new()),
    }
}

/// [`free_name`] for a still and its Live Photo motion file together: both
/// get the still's stem (`IMG_1.HEIC` + `IMG_1.MOV`, or `IMG_1-2.*`) so
/// the pair still matches by name in the folder.
fn free_names(
    still: &Path,
    motion: Option<&Path>,
    taken: &mut HashSet<String>,
) -> (String, Option<String>) {
    let (stem, ext) = stem_ext(still);
    let motion_ext = motion
        .map(|m| stem_ext(m).1)
        .filter(|m| !m.eq_ignore_ascii_case(&ext));
    let mut n = 1;
    loop {
        let base = if n == 1 {
            stem.clone()
        } else {
            format!("{stem}-{n}")
        };
        let s = format!("{base}{ext}");
        let m = motion_ext.as_ref().map(|e| format!("{base}{e}"));
        let free = |c: &String| !taken.contains(&c.to_lowercase());
        if free(&s) && m.as_ref().is_none_or(free) {
            taken.insert(s.to_lowercase());
            if let Some(m) = &m {
                taken.insert(m.to_lowercase());
            }
            // A motion file sharing the still's extension can't share its
            // stem too; it takes the next free name of its own.
            let m = m.or_else(|| motion.map(|mp| free_names(mp, None, taken).0));
            return (s, m);
        }
        n += 1;
    }
}

/// `pick`: copy the trip's best photos (no near-repeats unless different
/// people are in them) into a new folder. Sources are only read; the
/// folder must be new or empty so nothing is ever overwritten.
fn pick_cmd(flags: &HashMap<String, String>) {
    let db_path = need(flags, "db");
    let out = need(flags, "out");
    let count = match flags.get("count") {
        None => DEFAULT_PICK_COUNT,
        Some(v) => match v.parse::<usize>() {
            Ok(n) if n > 0 => n,
            _ => die("--count", "want a whole number of photos, 1 or more"),
        },
    };
    let stills_only = flags.get("stills-only").is_some_and(|v| v != "false");
    let db = Db::open(&db_path).unwrap_or_else(|e| die("open db", e));
    let ep = db
        .edited_plan(flags.get("project").map(String::as_str))
        .unwrap_or_else(|e| die("plan", e))
        .unwrap_or_else(|| die("plan", "no plan in db (run scan first)"));
    match out.read_dir() {
        Ok(mut entries) => {
            if entries.next().is_some() {
                die("--out", format!("{} is not empty", out.display()));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => die("--out", format!("{}: {e}", out.display())),
    }
    let excluded: HashSet<&str> = ep.edits.excluded.iter().map(String::as_str).collect();
    let picked = pick::pick_photos(&ep.assets, &excluded, count);
    if picked.is_empty() {
        die("pick", "no photos to pick from");
    }
    std::fs::create_dir_all(&out).unwrap_or_else(|e| die(&format!("create {}", out.display()), e));
    let mut taken: HashSet<String> = HashSet::new();
    let (mut copied, mut failed) = (0usize, 0usize);
    for a in &picked {
        let motion = a.companion_path.as_ref().filter(|_| !stills_only);
        let (still_name, motion_name) =
            free_names(&a.path, motion.map(PathBuf::as_path), &mut taken);
        let copies = std::iter::once((&a.path, still_name)).chain(motion.zip(motion_name));
        for (src, name) in copies {
            let dest = out.join(name);
            match std::fs::copy(src, &dest) {
                Ok(_) if *src == a.path => copied += 1,
                Ok(_) => {}
                Err(e) => {
                    failed += 1;
                    eprintln!("pick: {} not copied: {e}", src.display());
                }
            }
        }
    }
    println!(
        "picked {copied} of {} photos into {}",
        ep.assets
            .iter()
            .filter(|a| a.kind != AssetKind::Video)
            .count(),
        out.display()
    );
    if failed > 0 {
        eprintln!("pick: {failed} files could not be copied");
        std::process::exit(1);
    }
}

fn main() {
    let (cmd, flags) = parse_args();
    match cmd.as_str() {
        "scan" => scan_cmd(&flags),
        "plan" => plan_cmd(&flags),
        "export" => export_cmd(&flags),
        "pick" => pick_cmd(&flags),
        "learn" => learn_cmd(&flags),
        "cache" => cache_cmd(std::env::args().nth(2).as_deref().unwrap_or(""), &flags),
        "edit" => edit_cmd(&std::env::args().skip(2).collect::<Vec<_>>()),
        _ => {
            eprintln!("unknown command {cmd}; want scan|plan|edit|learn|export|pick|cache");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn music_flag_repeats_and_semicolons() {
        let mut f = HashMap::new();
        insert_flag(&mut f, "music".into(), "A.wav".into());
        insert_flag(&mut f, "music".into(), "B.wav;C.wav".into());
        insert_flag(&mut f, "out".into(), "x".into());
        insert_flag(&mut f, "out".into(), "y".into());
        assert_eq!(f["music"], "A.wav;B.wav;C.wav");
        assert_eq!(f["out"], "y", "other flags keep the last value");
        let list = music_list(&f["music"]);
        assert_eq!(list.len(), 3);
        assert_eq!(list[2], PathBuf::from("C.wav"));
        assert_eq!(music_list("single.wav"), vec![PathBuf::from("single.wav")]);
        assert!(music_list("").is_empty());
    }

    #[test]
    fn free_name_never_reuses_a_name() {
        let mut taken = HashSet::new();
        assert_eq!(
            free_name(Path::new("/a/IMG_1.HEIC"), &mut taken),
            "IMG_1.HEIC"
        );
        assert_eq!(
            free_name(Path::new("/b/img_1.heic"), &mut taken),
            "img_1-2.heic"
        );
        assert_eq!(
            free_name(Path::new("/c/IMG_1.HEIC"), &mut taken),
            "IMG_1-3.HEIC"
        );
        assert_eq!(free_name(Path::new("/a/noext"), &mut taken), "noext");
    }

    #[test]
    fn a_live_photo_pair_keeps_one_stem() {
        let mut taken = HashSet::new();
        free_name(Path::new("/a/IMG_1.HEIC"), &mut taken);
        // Only the still collides; the motion file follows it to -2.
        assert_eq!(
            free_names(
                Path::new("/b/IMG_1.HEIC"),
                Some(Path::new("/b/IMG_1.MOV")),
                &mut taken
            ),
            ("IMG_1-2.HEIC".to_string(), Some("IMG_1-2.MOV".to_string()))
        );
        // Only the motion name is taken; the still moves with it.
        free_name(Path::new("/c/IMG_9.MOV"), &mut taken);
        assert_eq!(
            free_names(
                Path::new("/d/IMG_9.JPG"),
                Some(Path::new("/d/x.mov")),
                &mut taken
            ),
            ("IMG_9-2.JPG".to_string(), Some("IMG_9-2.mov".to_string()))
        );
    }

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn edit_args_take_several_values_per_flag() {
        let a = parse_edit_args(&args(
            "--db t.db --include IMG_1060.JPG a2 --exclude x --include b --list",
        ))
        .unwrap();
        assert_eq!(a.db, Some(PathBuf::from("t.db")));
        assert_eq!(a.include, ["IMG_1060.JPG", "a2", "b"]);
        assert_eq!(a.exclude, ["x"]);
        assert!(a.list);
    }

    #[test]
    fn edit_args_reject_bad_input() {
        assert!(parse_edit_args(&args("--include")).is_err());
        assert!(parse_edit_args(&args("--bogus x")).is_err());
        assert!(parse_edit_args(&args("stray")).is_err());
        assert!(parse_edit_args(&args("--db a b")).is_err());
        assert!(parse_edit_args(&args("--list x")).is_err());
        assert!(parse_edit_args(&args("--join a")).is_err());
        assert!(parse_edit_args(&args("--join a b c")).is_err());
        assert!(parse_edit_args(&args("--solo")).is_err());
    }

    #[test]
    fn edit_args_take_collage_edits() {
        let a = parse_edit_args(&args(
            "--db t.db --solo a b --unsolo c --join d e --join f g --unjoin h",
        ))
        .unwrap();
        assert_eq!(a.solo, ["a", "b"]);
        assert_eq!(a.unsolo, ["c"]);
        assert_eq!(a.join, [("d".into(), "e".into()), ("f".into(), "g".into())]);
        assert_eq!(a.unjoin, ["h"]);
    }
}
