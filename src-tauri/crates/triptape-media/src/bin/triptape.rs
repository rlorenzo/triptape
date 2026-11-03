//! Headless TripTape CLI: `scan` a folder into SQLite, `plan` a story,
//! `export` all eligible keepsakes (hero film, reels pack, photo book).
//!
//! ```sh
//! triptape scan --folder /path/to/trip --db trip.db --cache /tmp/cache
//! triptape plan --db trip.db --out plan.json [--home LAT,LON]
//! triptape export --db trip.db --out /path/to/out [--preset P1|P2] [--home LAT,LON] [--music F] [--keep-ambience]
//! ```

use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::path::{Path, PathBuf};
use triptape_core::{
    cluster, db::Db, hash, jobs, livephoto, metadata, plan as planmod, privacy, proxies, scan,
    score, types::*,
};
use triptape_media::{export::export_all, export::ExportOptions, ffmpeg, render_book::BookPreset};

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
                flags.insert(p, "true".to_string());
            }
            pending = Some(k.to_string());
        } else if let Some(p) = pending.take() {
            flags.insert(p, a);
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
        scores.insert(
            a.id.clone(),
            score_asset(&a, img.as_ref(), &cache, &ffmpeg, &mut thumbs),
        );
        final_assets.push(a);
    }

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
    let suppressed: HashSet<&str> = groups
        .iter()
        .flat_map(|g| g.members.iter().filter(|m| **m != g.best_id))
        .map(String::as_str)
        .collect();

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
            let args = proxies::proxy_args(src, &dst);
            if proxies::run_ffmpeg(&ffmpeg, &args, &dst).is_ok() {
                proxies_n += 1;
            }
        }
    }

    let cluster_of: HashMap<String, String> = clusters
        .iter()
        .flat_map(|c| c.asset_ids.iter().map(|id| (id.clone(), c.label.clone())))
        .collect();
    let plan_assets: Vec<Asset> = final_assets
        .iter()
        .filter(|a| !suppressed.contains(a.id.as_str()))
        .cloned()
        .collect();
    let p = planmod::build_plan(&plan_assets, &cluster_of, &scores);
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
}

/// Fill photo metadata from EXIF; dimensions + pHash from the decoded
/// pixels when decodable (non-HEIC; HEIC proxies cover the rest).
fn fill_photo(a: &mut Asset, img: Option<&image::DynamicImage>) {
    let exif = metadata::read_image_exif(&a.path);
    let ct = metadata::resolve_capture_time(exif.taken_at, a.mtime);
    a.taken_at = Some(ct.unix_secs);
    a.time_guessed = ct.time_guessed;
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

/// Score one asset. Stills score on `img` (already decoded) or, failing
/// that, a cached FFmpeg thumbnail (counted in `thumbs`); video uses a
/// duration + GPS-novelty proxy.
fn score_asset(
    a: &Asset,
    img: Option<&image::DynamicImage>,
    cache: &Path,
    ffmpeg: &str,
    thumbs: &mut usize,
) -> Score {
    if a.kind == AssetKind::Video {
        let sig = score::StorySignals {
            gps_novelty: if a.gps.is_some() { 0.6 } else { 0.0 },
            ..Default::default()
        };
        let base = Score {
            total: 0.55,
            sharpness: 0.5,
            exposure: 0.6,
            ..Default::default()
        };
        return score::highlight_score(&base, &sig, false);
    }
    let mut thumb = || {
        let h = a.content_hash.as_deref()?;
        let dst = proxies::cache_path(cache, "thumb", h, ".jpg");
        proxies::run_ffmpeg(ffmpeg, &proxies::thumb_args(&a.path, &dst), &dst).ok()?;
        *thumbs += 1;
        image::open(&dst).ok()
    };
    let gray = match img {
        Some(img) => Some(img.to_luma8()),
        None => thumb().map(|t| t.to_luma8()),
    };
    gray.and_then(|g| score::frame_stats(g.as_raw(), g.width(), g.height()))
        .map(|stats| {
            let base = score::score_still(&stats);
            score::highlight_score(&base, &score::StorySignals::default(), false)
        })
        .unwrap_or_else(|| Score {
            total: 0.5,
            ..Default::default()
        })
}

fn plan_cmd(flags: &HashMap<String, String>) {
    let db_path = need(flags, "db");
    let out = PathBuf::from(flag_or(flags, "out", "plan.json"));
    let db = Db::open(&db_path).unwrap_or_else(|e| die("open db", e));
    // Default (no --project): latest plan in the db, for single-project DBs.
    let p = db
        .latest_plan(flags.get("project").map(String::as_str))
        .unwrap_or_else(|e| die("plan", e))
        .unwrap_or_else(|| die("plan", "no plan in db (run scan first)"));
    // Exported plans fuzz route points near home (PLAN.md Sec 14 privacy).
    let mut p = p;
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
    let music = flags.get("music").map(PathBuf::from);
    if let Some(m) = &music {
        if !m.exists() {
            die("music", format!("not found: {}", m.display()));
        }
    }

    let db = Db::open(&db_path).unwrap_or_else(|e| die("open db", e));
    let (project_id, mut plan) = db
        .latest_plan_with_project(flags.get("project").map(String::as_str))
        .unwrap_or_else(|e| die("plan", e))
        .unwrap_or_else(|| die("plan", "no plan in db (run scan first)"));
    // Rendered maps get the same home fuzzing as `plan` JSON (Sec 14).
    if let Some(h) = flags.get("home") {
        let home = parse_lat_lon(h).unwrap_or_else(|| die("--home", "want LAT,LON"));
        plan.map_polyline = privacy::fuzz_home_route(&plan.map_polyline, home);
    }
    let assets = db
        .list_assets(&project_id)
        .unwrap_or_else(|e| die("assets", e));
    let trip_name = db
        .get_project(&project_id)
        .unwrap_or_else(|e| die("project", e))
        .map(|p| p.name.clone())
        .unwrap_or_else(|| "trip".to_string());

    let caps = ffmpeg::probe_capabilities(&ffmpeg_bin, &ffprobe_bin);
    println!(
        "ffmpeg caps: libx264={} xfade={} zoompan={} tonemap={} zscale={} loudnorm={} drawtext={} libass={} vidstab={}",
        caps.libx264,
        caps.xfade,
        caps.zoompan,
        caps.tonemap,
        caps.zscale,
        caps.loudnorm,
        caps.drawtext,
        caps.libass,
        caps.vidstabdetect && caps.vidstabtransform,
    );

    // Free-space precheck: ~2 GB estimate for the three keepsakes (the
    // precheck applies its own 1.5x headroom).
    if let Err(e) = jobs::free_space_precheck(&out, 2_000_000_000) {
        die("free space", e);
    }

    let keep_ambience = flags.get("keep-ambience").is_some_and(|v| v != "false");
    let report = export_all(
        &caps,
        &plan,
        &assets,
        &out,
        &ExportOptions {
            preset,
            trip_name,
            typst_bin,
            music_path: music,
            keep_ambience,
            only: flag_or(flags, "only", "all"),
        },
    )
    .unwrap_or_else(|e| die("export", e));

    if let Some(v) = &report.video {
        println!("video: {v}");
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

fn main() {
    let (cmd, flags) = parse_args();
    match cmd.as_str() {
        "scan" => scan_cmd(&flags),
        "plan" => plan_cmd(&flags),
        "export" => export_cmd(&flags),
        _ => {
            eprintln!("unknown command {cmd}; want scan|plan|export");
            std::process::exit(2);
        }
    }
}
