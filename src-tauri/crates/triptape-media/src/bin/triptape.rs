//! Headless TripTape CLI: `scan` a folder into SQLite, `plan` a story,
//! `export` all eligible keepsakes (hero film, reels pack, photo book).
//!
//! ```sh
//! triptape scan --folder /path/to/trip --db trip.db --cache /tmp/cache [--length auto|SECS|10m]
//! triptape plan --db trip.db --out plan.json [--home LAT,LON] [--itinerary itinerary.json]
//! triptape edit --db trip.db [--project ID] [--include REF...] [--exclude REF...] [--uninclude REF...] [--unexclude REF...] [--list]
//! triptape learn --db trip.db [--project ID] [--show | --backfill | --reset | --apply [--length auto|SECS|10m] [--place-override SPEC]]
//! triptape export --db trip.db --out /path/to/out [--preset P1|P2] [--home LAT,LON] [--music F]... (repeat or ';'-join for a crossfaded playlist) [--keep-ambience] [--itinerary itinerary.json] [--resolution 4k|1080p] [--jobs N] [--quiet]
//!                 [--segment-cache DIR | --no-segment-cache] [--segment-cache-max SIZE]
//! triptape cache stats|clear [--segment-cache DIR]
//! ```

use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::path::{Path, PathBuf};
use triptape_core::plan::PlanEdits;
use triptape_core::{
    asset_ref, cluster,
    db::{Db, EditedPlan},
    hash, jobs, learn, livephoto, metadata, plan as planmod, privacy, proxies, scan, score,
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

fn scan_cmd(flags: &HashMap<String, String>) {
    let folder = need(flags, "folder");
    let db_path = need(flags, "db");
    let cache = need(flags, "cache");
    let ffmpeg = flag_or(flags, "ffmpeg", "ffmpeg");
    let ffprobe = flag_or(flags, "ffprobe", "ffprobe");
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

    let db = Db::open(&db_path).unwrap_or_else(|e| die("open db", e));
    let name = folder
        .file_name()
        .map_or_else(|| "trip".into(), |s| s.to_string_lossy());
    let project = db
        .create_project(&name, &folder.to_string_lossy())
        .unwrap_or_else(|e| die("create project", e));

    let result = scan::scan_folder(&folder).unwrap_or_else(|e| die("scan", e));

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
    for mut a in assets {
        if motion_halves.contains(a.path.as_path()) {
            continue;
        }
        let img = match a.kind {
            AssetKind::Video => {
                let probe = probes.get(&a.path);
                fill_video(&mut a, probe);
                None
            }
            _ => {
                let img = image::open(&a.path).ok();
                fill_photo(&mut a, img.as_ref());
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
        let (score, phash) = score_asset(&a, img.as_ref(), &cache, &ffmpeg, &mut thumbs);
        a.phash = a.phash.or(phash);
        scores.insert(a.id.clone(), score);
        final_assets.push(a);
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
    println!("thumbs: {thumbs} proxies: {proxies_n}");
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

/// Fill video metadata from its probe; unprobeable videos fall back to mtime.
fn fill_video(a: &mut Asset, probe: Option<&metadata::FfprobeOutput>) {
    let Some(p) = probe else {
        a.taken_at = Some(a.mtime);
        a.time_guessed = true;
        (a.user_caption, a.people_count) = metadata::story_fields(&a.path, None);
        return;
    };
    a.duration_secs = p.duration_secs();
    if let Some((w, h)) = p.video_dims() {
        a.width = Some(w);
        a.height = Some(h);
    }
    a.rotation_deg = p.rotation_deg();
    a.gps = p.gps();
    let ct = metadata::resolve_capture_time(p.creation_time(), a.mtime);
    a.taken_at = Some(ct.unix_secs);
    a.time_guessed = ct.time_guessed;
    (a.user_caption, a.people_count) = metadata::story_fields(&a.path, None);
}

/// Fill photo metadata from EXIF; dimensions + pHash from the decoded
/// pixels when decodable (non-HEIC; HEIC proxies cover the rest).
fn fill_photo(a: &mut Asset, img: Option<&image::DynamicImage>) {
    let exif = metadata::read_image_exif(&a.path);
    let ct = metadata::resolve_capture_time(exif.taken_at, a.mtime);
    a.taken_at = Some(ct.unix_secs);
    a.time_guessed = ct.time_guessed;
    a.tz_offset_secs = exif.tz_offset_secs;
    (a.user_caption, a.people_count) = metadata::story_fields(&a.path, Some(&exif));
    a.gps = exif.gps;
    a.rotation_deg = exif
        .orientation
        .map(metadata::orientation_to_degrees)
        .unwrap_or(0);
    if let Some(img) = img {
        a.width = Some(img.width());
        a.height = Some(img.height());
        a.phash = Some(hash::average_hash_from_image(img));
    } else if let (Some(w), Some(h)) = (exif.width, exif.height) {
        // HEIC and other undecodable stills: EXIF carries the true size.
        a.width = Some(w);
        a.height = Some(h);
    }
}

/// Score one asset and fingerprint it. Stills score on `img` (already
/// decoded) or, failing that, a cached FFmpeg thumbnail (counted in
/// `thumbs`); videos get a poster-frame thumbnail the same way and a
/// provisional score (their real one follows clip analysis). The returned
/// pHash comes from the pixels used for scoring, so HEIC stills, Live
/// Photos and videos get one too; `None` when nothing could be decoded.
fn score_asset(
    a: &Asset,
    img: Option<&image::DynamicImage>,
    cache: &Path,
    ffmpeg: &str,
    thumbs: &mut usize,
) -> (Score, Option<u64>) {
    let signals = score::StorySignals::for_asset(a);
    let mut thumb = || {
        let h = a.content_hash.as_deref()?;
        let dst = proxies::cache_path(cache, "thumb", h, ".jpg");
        proxies::run_ffmpeg(ffmpeg, &proxies::thumb_args(&a.path, &dst), &dst).ok()?;
        *thumbs += 1;
        image::open(&dst).ok()
    };
    let owned;
    let pixels = match img {
        Some(img) => Some(img),
        None => {
            owned = thumb();
            owned.as_ref()
        }
    };
    let phash = pixels.map(hash::average_hash_from_image);
    if a.kind == AssetKind::Video {
        return (score::score_video(None, &signals), phash);
    }
    (score::score_still_or_fallback(pixels, &signals), phash)
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
    if let Some(h) = flags.get("home") {
        let home = parse_lat_lon(h).unwrap_or_else(|| die("--home", "want LAT,LON"));
        p.map_polyline = privacy::fuzz_home_route(&p.map_polyline, home);
    }
    let json = p.to_json().unwrap_or_else(|e| die("plan json", e));
    std::fs::write(&out, &json).unwrap_or_else(|e| die(&format!("write {}", out.display()), e));
    println!(
        "wrote {} (hero={} reels={})",
        out.display(),
        p.hero.len(),
        p.reel_selects.len()
    );
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
    let ffmpeg_bin = flag_or(flags, "ffmpeg", "ffmpeg");
    let ffprobe_bin = flag_or(flags, "ffprobe", "ffprobe");
    let typst_bin = flag_or(flags, "typst", "typst");
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
    if let Some(h) = flags.get("home") {
        let home = parse_lat_lon(h).unwrap_or_else(|| die("--home", "want LAT,LON"));
        plan.map_polyline = privacy::fuzz_home_route(&plan.map_polyline, home);
    }
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
            trip_name,
            typst_bin,
            music_paths: music,
            keep_ambience,
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
    dir.map(|dir| SegmentCacheConfig { dir, max_bytes })
}

/// `--segment-cache DIR`, else the platform default.
fn cache_dir_flag(flags: &HashMap<String, String>) -> Option<PathBuf> {
    flags
        .get("segment-cache")
        .map(PathBuf::from)
        .or_else(segment_cache::default_dir)
}

/// `--segment-cache-max SIZE`, when given.
fn cache_max_flag(flags: &HashMap<String, String>) -> Option<u64> {
    flags
        .get("segment-cache-max")
        .map(|v| segment_cache::parse_size(v).unwrap_or_else(|e| die("segment-cache-max", e)))
}

/// `cache stats` / `cache clear`.
fn cache_cmd(sub: &str, flags: &HashMap<String, String>) {
    let dir = cache_dir_flag(flags)
        .unwrap_or_else(|| die("cache", "no cache directory on this platform"));
    match sub {
        "stats" => {
            let st = segment_cache::disk_stats(&dir);
            let cap = segment_cache::resolve_cap(cache_max_flag(flags), &dir);
            println!("segment cache: {}", dir.display());
            println!(
                "entries: {}  size: {}  cap: {}  exports running: {}",
                st.entries,
                segment_cache::fmt_bytes(st.bytes),
                segment_cache::fmt_bytes(cap),
                st.live_leases
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

/// Parsed `edit` arguments. `--include`, `--exclude`, `--uninclude` and
/// `--unexclude` each take one or more asset references (id, file name or
/// path) and may repeat.
#[derive(Debug, Default, PartialEq)]
struct EditArgs {
    db: Option<PathBuf>,
    project: Option<String>,
    list: bool,
    include: Vec<String>,
    exclude: Vec<String>,
    uninclude: Vec<String>,
    unexclude: Vec<String>,
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
            "include" | "exclude" | "uninclude" | "unexclude" => {
                if values.is_empty() {
                    return Err(format!("--{flag} wants at least one asset"));
                }
                match flag {
                    "include" => out.include.extend(values),
                    "exclude" => out.exclude.extend(values),
                    "uninclude" => out.uninclude.extend(values),
                    _ => out.unexclude.extend(values),
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
    let resolve = |refs: &[String]| -> Vec<&Asset> {
        refs.iter()
            .map(|q| asset_ref::resolve_asset(&ep.assets, q).unwrap_or_else(|e| die("edit", e)))
            .collect()
    };
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
    // Un-dos first, then removals, then pins.
    let ops: [(Vec<&Asset>, &str, Setter, bool); 4] = [
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
    ];
    let changed = ops.iter().any(|(assets, ..)| !assets.is_empty());
    if !changed && !a.list {
        die(
            "edit",
            "nothing to do: pass --include/--exclude/--uninclude/--unexclude or --list",
        );
    }
    let mut edits = ep.edits.clone();
    for (assets, verb, set, on) in &ops {
        for asset in assets {
            set(&mut edits, asset.id.clone(), *on);
            println!("{verb}: {}", asset_label(asset));
        }
    }
    if changed {
        db.save_plan_edits(&ep.project_id, &edits)
            .unwrap_or_else(|e| die("save edits", e));
    }
    print_edits(&edits, &by_id);
    if !changed {
        return;
    }
    // Show where each included shot landed in the film.
    let hero = planmod::apply_edits(&ep.base, &ep.assets, &edits).hero;
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

fn main() {
    let (cmd, flags) = parse_args();
    match cmd.as_str() {
        "scan" => scan_cmd(&flags),
        "plan" => plan_cmd(&flags),
        "export" => export_cmd(&flags),
        "learn" => learn_cmd(&flags),
        "cache" => cache_cmd(std::env::args().nth(2).as_deref().unwrap_or(""), &flags),
        "edit" => edit_cmd(&std::env::args().skip(2).collect::<Vec<_>>()),
        _ => {
            eprintln!("unknown command {cmd}; want scan|plan|edit|learn|export|cache");
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
    }
}
