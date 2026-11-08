//! `export` orchestration: one plan in, three keepsakes out.
//!
//! Honors Sec 5.4 eligibility — disabled keepsakes are skipped with the
//! plain-language reason printed (and recorded in the report), never padded
//! and never failed silently.

use crate::ffmpeg::Capabilities;
use crate::progress::Progress;
use crate::render_book::{self, BookOptions, BookPhoto};
use crate::render_reels::{self, ReelOptions};
use crate::render_video::{self, ResolvedEntry, VideoOptions};
use crate::resolution::Resolution;
use crate::segment_cache::{SegmentCache, SegmentCacheConfig};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use triptape_core::plan::{check_source_integrity, plan_eligibility, Integrity};
use triptape_core::types::{distinct_assets, AssetKind, Plan, PlanEntry, Rendition};

#[derive(Debug, Clone)]
pub struct ExportOptions {
    pub preset: render_book::BookPreset,
    pub trip_name: String,
    pub typst_bin: String,
    /// Music playlist in order (crossfaded); empty uses the bundled theme.
    pub music_paths: Vec<PathBuf>,
    /// Keep the original film audio at 0.12 under the music bed (default
    /// mutes it; the music is the primary audio).
    pub keep_ambience: bool,
    /// "all" (default), or one keepsake: "video" | "reels" | "book".
    pub only: String,
    /// Hero film frame size (default 4K). Reels stay 1080x1920.
    pub resolution: Resolution,
    /// Hero segments rendered at once; `None` picks the machine default.
    pub jobs: Option<usize>,
    /// Progress sink and cancel flag (default: silent, never cancelled).
    pub progress: Progress,
    /// Reuse rendered hero segments across exports; `None` renders all.
    pub segment_cache: Option<SegmentCacheConfig>,
}

/// How the segment cache served one export.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SegmentCacheReport {
    pub dir: String,
    /// Segments taken from the cache / segments the cache was asked for.
    pub reused: usize,
    pub total: usize,
    /// Fresh segments written to the cache.
    pub stored: usize,
    /// Fresh segments left out because the volume was low on space.
    pub skipped_low_disk: usize,
    /// Old entries removed to stay within the size cap.
    pub evicted: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Skipped {
    pub keepsake: String,
    pub reason: String,
}

#[derive(Debug, serde::Serialize)]
pub struct ExportReport {
    pub trip_name: String,
    pub video: Option<String>,
    pub reels: Vec<String>,
    pub book_pages: usize,
    pub skipped: Vec<Skipped>,
    pub fallbacks: Vec<String>,
    pub attribution: Vec<String>,
    /// Present when a segment cache was in use.
    pub segment_cache: Option<SegmentCacheReport>,
}

/// The place of the chapter holding `asset_id`, for keepsakes that print or
/// overlay it (reels, book). The hero film never uses it: its lower third
/// already names the place.
fn chapter_place<'a>(plan: &'a Plan, asset_id: &str) -> Option<&'a str> {
    plan.chapters
        .iter()
        .find(|c| c.asset_ids.iter().any(|id| id == asset_id))
        .and_then(|c| c.place.as_deref())
}

/// Resolve a plan entry to files from the asset map.
fn resolve_entry(
    plan: &Plan,
    assets: &HashMap<String, triptape_core::types::Asset>,
    entry: &PlanEntry,
) -> Option<ResolvedEntry> {
    let asset_id = entry.asset_id.as_str();
    let a = assets.get(asset_id)?;
    let (still_path, motion_path) = match a.kind {
        AssetKind::Photo => (a.path.clone(), None),
        AssetKind::Video => (a.path.clone(), Some(a.path.clone())),
        AssetKind::LivePhoto => (a.path.clone(), a.companion_path.clone()),
    };
    Some(ResolvedEntry {
        // The caller's entry, not a lookup by asset id: sibling windows of
        // one clip share the id but not the trim.
        entry: entry.clone(),
        still_path,
        motion_path,
        caption: plan.captions.get(asset_id).cloned(),
        taken_at: a.taken_at,
        tz_offset_secs: a.tz_offset_secs,
        aspect: a.display_aspect(),
    })
}

/// Current (size, mtime) of `path`, in the same form the scanner records.
fn current_size_mtime(path: &Path) -> (Option<u64>, Option<i64>) {
    let Ok(md) = std::fs::metadata(path) else {
        return (None, None);
    };
    let mtime = md
        .modified()
        .ok()
        .map(|m| match m.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_secs() as i64,
            Err(e) => -(e.duration().as_secs() as i64),
        });
    (Some(md.len()), mtime)
}

/// Sec 8 pre-render check: compare the original on disk against the
/// planned snapshot (size + mtime, hashing only on mismatch). Changed or
/// missing sources are skipped with a reason rather than silently rendered.
fn verify_source(entry: &PlanEntry, path: &Path) -> Result<(), String> {
    let (size, mtime) = current_size_mtime(path);
    match check_source_integrity(entry, path, size, mtime, || {
        triptape_core::hash::blake3_file(path).ok()
    }) {
        Integrity::Ok => Ok(()),
        Integrity::Missing { reason } | Integrity::Changed { reason } => Err(reason),
    }
}

/// Snapshot entry for an asset row (used for book photos, which are not
/// plan entries).
fn asset_snapshot(a: &triptape_core::types::Asset) -> PlanEntry {
    PlanEntry {
        asset_id: a.id.clone(),
        window: 0,
        content_hash: a.content_hash.clone(),
        size_bytes: a.size_bytes,
        mtime: a.mtime,
        rendition: triptape_core::types::Rendition::Still,
    }
}

/// Resolve a plan entry and verify its original is unchanged; failures
/// are recorded in `fallbacks` and the entry is dropped.
fn resolve_verified(
    plan: &Plan,
    assets: &HashMap<String, triptape_core::types::Asset>,
    entry: &PlanEntry,
    label: &str,
    fallbacks: &mut Vec<String>,
) -> Option<ResolvedEntry> {
    let r = resolve_entry(plan, assets, entry)?;
    match verify_source(entry, &r.still_path) {
        Ok(()) => Some(r),
        Err(reason) => {
            fallbacks.push(format!("{label}: {reason}; skipped"));
            None
        }
    }
}

/// Run all eligible keepsake renderers. Layout under `out_dir`:
/// `hero.mp4`, `reels/`, `book/`, plus `export.json`.
pub fn export_all(
    caps: &Capabilities,
    plan: &Plan,
    assets: &[triptape_core::types::Asset],
    out_dir: &Path,
    opts: &ExportOptions,
) -> Result<ExportReport, crate::MediaError> {
    // Every renderer polls the one cancel flag through its capabilities.
    let mut caps = crate::ffmpeg::Capabilities {
        progress: opts.progress.clone(),
        ..caps.clone()
    };
    // The cache takes its lease before any segment is used and gives it back
    // however the export ends (the handle also releases on drop).
    let work = out_dir.join(".work");
    let (cache, cache_note) = match &opts.segment_cache {
        None => (None, None),
        Some(cfg) => {
            let version = crate::segment_cache::ffmpeg_version(&caps.ffmpeg_bin);
            match SegmentCache::open(cfg, version, &work) {
                Ok(c) => (Some(c), None),
                Err(e) => (
                    None,
                    Some(format!("segment cache off ({}): {e}", cfg.dir.display())),
                ),
            }
        }
    };
    caps.segment_cache = cache.clone();
    let mut out = export_inner(&caps, plan, assets, out_dir, opts);
    // Leave no intermediates behind however the export ends (on cancel,
    // finished outputs and `.partial` siblings are already gone). This must
    // precede `cache.finish()`: `.work` holds hard links to cache entries, so
    // while it exists eviction and `cache clear` cannot reclaim their bytes.
    if let Err(e) = std::fs::remove_dir_all(&work) {
        if e.kind() != std::io::ErrorKind::NotFound {
            if let Ok(report) = out.as_mut() {
                report.fallbacks.push(format!(
                    "could not remove intermediates ({}): {e}; cached segments they share stay allocated",
                    work.display()
                ));
            }
        }
    }
    if let Some(cache) = cache {
        // Eviction runs only now: the lease is released first, so nothing
        // this export used is protected any longer than it is needed.
        let evicted = cache.finish();
        if let Ok(report) = out.as_mut() {
            record_cache_use(&cache, evicted.removed, report);
        }
    }
    if let (Ok(report), Some(note)) = (out.as_mut(), cache_note) {
        report.fallbacks.push(note);
    }
    // Written last so the durable record includes the cache diagnostics.
    if let Ok(report) = &out {
        std::fs::write(
            out_dir.join("export.json"),
            serde_json::to_string_pretty(report)?,
        )
        .map_err(|e| crate::MediaError::Io(e.to_string()))?;
    }
    out
}

/// Add how `cache` served this export to `report`: the reuse summary and,
/// when the volume ran low, a fallback note.
fn record_cache_use(cache: &SegmentCache, evicted: usize, report: &mut ExportReport) {
    let stats = cache.stats();
    let total = stats.hits + stats.misses;
    if total > 0 {
        report.segment_cache = Some(SegmentCacheReport {
            dir: cache.dir().to_string_lossy().into_owned(),
            reused: stats.hits,
            total,
            stored: stats.stored,
            skipped_low_disk: stats.skipped_low_disk,
            evicted,
        });
    }
    if stats.skipped_low_disk > 0 {
        report.fallbacks.push(format!(
            "segment cache: {} segments not cached (under {} free)",
            stats.skipped_low_disk,
            crate::segment_cache::fmt_bytes(crate::segment_cache::LOW_DISK_BYTES)
        ));
    }
}

fn export_inner(
    caps: &Capabilities,
    plan: &Plan,
    assets: &[triptape_core::types::Asset],
    out_dir: &Path,
    opts: &ExportOptions,
) -> Result<ExportReport, crate::MediaError> {
    // Validate the keepsake selector before touching the output dir, so a
    // typo (e.g. "reel") fails loudly instead of exporting nothing.
    if !matches!(opts.only.as_str(), "all" | "video" | "reels" | "book") {
        return Err(crate::MediaError::InvalidArg(format!(
            "--only must be one of all|video|reels|book, got {:?}",
            opts.only
        )));
    }
    let by_id: HashMap<String, triptape_core::types::Asset> =
        assets.iter().map(|a| (a.id.clone(), a.clone())).collect();
    // Eligibility counts assets, not entries: a scene-split clip plays
    // several windows but is still one usable asset.
    let usable = distinct_assets(&plan.hero);
    let photos = assets.iter().filter(|a| a.kind != AssetKind::Video).count();
    let elig = plan_eligibility(usable, photos, plan.chapters.len());

    std::fs::create_dir_all(out_dir).map_err(|e| crate::MediaError::Io(e.to_string()))?;
    let work = out_dir.join(".work");
    std::fs::create_dir_all(&work).map_err(|e| crate::MediaError::Io(e.to_string()))?;

    let mut report = ExportReport {
        trip_name: opts.trip_name.clone(),
        video: None,
        reels: vec![],
        book_pages: 0,
        skipped: vec![],
        fallbacks: vec![],
        attribution: vec![crate::map::MAP_ATTRIBUTION.to_string()],
        segment_cache: None,
    };

    // Map card once, shared by the hero film and the book map page.
    let mut map_card: Option<PathBuf> = None;
    if !plan.map_polyline.is_empty() {
        let card = work.join("route_card.png");
        let (card_w, card_h) = opts.resolution.dims();
        match crate::map::save_route_card(&plan.map_polyline, &[], card_w, card_h, &card) {
            Ok(note) => {
                report.attribution.push(note);
                map_card = Some(card);
            }
            Err(e) => report.fallbacks.push(format!("map card failed: {e}")),
        }
    }
    let attribution_path = out_dir.join("ATTRIBUTION.txt");
    std::fs::write(
        &attribution_path,
        format!(
            "TripTape export — {}\nMap route card: plain styled background (no tile basemap; G3 fallback scope).\nAttribution on every map frame/page: {}\n",
            opts.trip_name,
            crate::map::MAP_ATTRIBUTION
        ),
    )
    .map_err(|e| crate::MediaError::Io(e.to_string()))?;

    // Keepsake 1 — Trip Video.
    if opts.only != "all" && opts.only != "video" {
        report.skipped.push(Skipped {
            keepsake: "video".into(),
            reason: format!("not selected (--only {})", opts.only),
        });
    } else if elig.video == "disabled" {
        report.skipped.push(Skipped {
            keepsake: "video".into(),
            reason: elig.video_reason.clone(),
        });
    } else {
        let resolved: Vec<ResolvedEntry> = plan
            .hero
            .iter()
            .filter_map(|e| resolve_verified(plan, &by_id, e, "video", &mut report.fallbacks))
            .collect();
        let out_mp4 = out_dir.join("hero.mp4");
        match render_video::render_hero_video(
            caps,
            plan,
            &resolved,
            map_card.as_deref(),
            &work.join("video"),
            &out_mp4,
            &VideoOptions {
                trip_name: opts.trip_name.clone(),
                short_mode: elig.video == "short",
                music_paths: opts.music_paths.clone(),
                keep_ambience: opts.keep_ambience,
                typst_bin: opts.typst_bin.clone(),
                resolution: opts.resolution,
                jobs: opts.jobs,
                ..Default::default()
            },
        ) {
            Ok(v) => {
                report.video = Some(out_mp4.to_string_lossy().into_owned());
                report.fallbacks.extend(v.fallbacks);
            }
            Err(crate::MediaError::Cancelled) => return Err(crate::MediaError::Cancelled),
            Err(e) => report.skipped.push(Skipped {
                keepsake: "video".into(),
                reason: format!("render failed: {e}"),
            }),
        }
    }

    // Keepsake 2 — Reels Pack.
    if opts.only != "all" && opts.only != "reels" {
        report.skipped.push(Skipped {
            keepsake: "reels".into(),
            reason: format!("not selected (--only {})", opts.only),
        });
    } else if elig.reels == 0 {
        report.skipped.push(Skipped {
            keepsake: "reels".into(),
            reason: elig.reels_reason.clone(),
        });
    } else {
        let mut seen = std::collections::HashSet::new();
        let resolved: Vec<ResolvedEntry> = plan
            .reel_selects
            .iter()
            .filter(|e| seen.insert(e.asset_id.clone()))
            .take(elig.reels)
            .filter_map(|e| resolve_verified(plan, &by_id, e, "reels", &mut report.fallbacks))
            .map(|mut r| {
                // A reel names where it is; the user's caption wins.
                if r.caption.is_none() {
                    r.caption = chapter_place(plan, &r.entry.asset_id).map(String::from);
                }
                r
            })
            .collect();
        // Still highlights become slideshows of their chapter neighbors,
        // never reusing a photo across reels.
        // Reserve every selected still first so an earlier reel cannot
        // take a later reel's highlight as a neighbor.
        let mut used_slides: HashSet<PathBuf> =
            resolved.iter().map(|r| r.still_path.clone()).collect();
        let mut slideshows: Vec<Vec<PathBuf>> = Vec::with_capacity(resolved.len());
        for r in &resolved {
            let slides =
                chapter_slideshow(plan, &by_id, r, &mut used_slides, &mut report.fallbacks);
            if slides.len() == 1 && r.motion_path.is_none() {
                report.fallbacks.push(format!(
                    "reel {}: no chapter neighbors; single-still reel",
                    r.entry.asset_id
                ));
            }
            slideshows.push(slides);
        }
        let reels_dir = out_dir.join("reels");
        if resolved.is_empty() {
            // Every selected reel was filtered out (missing/changed source);
            // details are already in report.fallbacks.
            report.skipped.push(Skipped {
                keepsake: "reels".into(),
                reason: "no reel sources available (missing or changed); see fallbacks".into(),
            });
        } else {
            match render_reels::render_reels(
                caps,
                &resolved,
                &reels_dir,
                &ReelOptions {
                    // Reels are short: they loop the first playlist track
                    // rather than building the crossfaded bed.
                    music_path: opts.music_paths.first().cloned(),
                    slideshows,
                    typst_bin: opts.typst_bin.clone(),
                    ..Default::default()
                },
            ) {
                Ok(r) => {
                    report.reels = r.reels.iter().map(|f| f.path.clone()).collect();
                    report.fallbacks.extend(r.fallbacks);
                }
                Err(crate::MediaError::Cancelled) => return Err(crate::MediaError::Cancelled),
                Err(e) => report.skipped.push(Skipped {
                    keepsake: "reels".into(),
                    reason: format!("render failed: {e}"),
                }),
            }
        }
    }

    // Keepsake 3 — Photo Book.
    if opts.only != "all" && opts.only != "book" {
        report.skipped.push(Skipped {
            keepsake: "book".into(),
            reason: format!("not selected (--only {})", opts.only),
        });
    } else if !elig.book_enabled {
        report.skipped.push(Skipped {
            keepsake: "book".into(),
            reason: elig.book_reason.clone(),
        });
    } else {
        let chapter_of: HashMap<&str, usize> = plan
            .chapters
            .iter()
            .enumerate()
            .flat_map(|(i, c)| c.asset_ids.iter().map(move |id| (id.as_str(), i)))
            .collect();
        let mut book_photos: Vec<BookPhoto> = vec![];
        for a in assets.iter().filter(|a| a.kind != AssetKind::Video) {
            if let Err(reason) = verify_source(&asset_snapshot(a), &a.path) {
                report.fallbacks.push(format!("book: {reason}; skipped"));
                continue;
            }
            let (w, h) = match (a.width, a.height) {
                (Some(w), Some(h)) => (w, h),
                _ => match image::image_dimensions(&a.path) {
                    Ok(dims) => dims,
                    Err(_) => {
                        report
                            .fallbacks
                            .push(format!("book: {} unreadable; skipped", a.id));
                        continue;
                    }
                },
            };
            let chapter = chapter_of.get(a.id.as_str()).copied();
            book_photos.push(BookPhoto {
                asset_id: a.id.clone(),
                path: a.path.clone(),
                width: w,
                height: h,
                // The user's caption, else the chapter place.
                caption: plan
                    .captions
                    .get(&a.id)
                    .or_else(|| chapter.and_then(|i| plan.chapters[i].place.as_ref()))
                    .cloned(),
                chapter,
            });
        }
        // Story order: keep db order (taken_at, id).
        let book_dir = out_dir.join("book");
        // Day-marker titles: day once, with the location.
        let chapter_titles: Vec<String> = plan
            .chapters
            .iter()
            .map(crate::titlecard::chapter_book_title)
            .collect();
        match render_book::render_book(
            &book_photos,
            map_card.as_deref(),
            &chapter_titles,
            &book_dir,
            &BookOptions {
                preset: opts.preset,
                trip_name: opts.trip_name.clone(),
                typst_bin: opts.typst_bin.clone(),
                ffmpeg_bin: caps.ffmpeg_bin.clone(),
                progress: opts.progress.clone(),
            },
        ) {
            Ok(b) => {
                report.book_pages = b.counted_pages;
                report.fallbacks.extend(b.fallbacks);
            }
            Err(crate::MediaError::Cancelled) => return Err(crate::MediaError::Cancelled),
            Err(e) => report.skipped.push(Skipped {
                keepsake: "book".into(),
                reason: format!("preflight/render blocked: {e}"),
            }),
        }
    }

    Ok(report)
}

/// Most photos in one still-highlight slideshow reel.
const MAX_SLIDES: usize = 5;

/// Slideshow stills for one reel: the selected still, then the next stills
/// of its own chapter in story order (wrapping to earlier ones), skipping
/// clips, photos already in `used` (callers pre-reserve every reel's
/// selected still), and neighbors that fail the source integrity check
/// (recorded in `fallbacks`). Empty for motion reels.
fn chapter_slideshow(
    plan: &Plan,
    by_id: &HashMap<String, triptape_core::types::Asset>,
    r: &ResolvedEntry,
    used: &mut HashSet<PathBuf>,
    fallbacks: &mut Vec<String>,
) -> Vec<PathBuf> {
    // Still highlight: slideshow starts with the select itself. Motion
    // highlight: neighbors only — the reel opens with the motion moment,
    // then extends into the slideshow when the clip is short.
    let is_still = r.motion_path.is_none() && matches!(r.entry.rendition, Rendition::Still);
    let mut slides: Vec<PathBuf> = vec![];
    if is_still {
        slides.push(r.still_path.clone());
        used.insert(r.still_path.clone());
    }
    let id = &r.entry.asset_id;
    let Some((ch, start)) = plan
        .chapters
        .iter()
        .find_map(|c| c.asset_ids.iter().position(|a| a == id).map(|p| (c, p)))
    else {
        return slides;
    };
    let neighbors = ch.asset_ids[start + 1..]
        .iter()
        .chain(&ch.asset_ids[..start]);
    for a in neighbors.filter_map(|nid| by_id.get(nid)) {
        if slides.len() >= MAX_SLIDES {
            break;
        }
        if a.kind == AssetKind::Video || used.contains(&a.path) {
            continue;
        }
        if let Err(reason) = verify_source(&asset_snapshot(a), &a.path) {
            fallbacks.push(format!("reels: {reason}; neighbor skipped"));
            continue;
        }
        used.insert(a.path.clone());
        slides.push(a.path.clone());
    }
    slides
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_plan() -> Plan {
        Plan {
            plan_version: 1,
            schema_version: 1,
            hero: vec![],
            reel_selects: vec![],
            chapters: vec![],
            captions: HashMap::new(),
            map_polyline: vec![],
            music: triptape_core::types::MusicPick {
                track: "t".into(),
                rationale: "r".into(),
            },
        }
    }

    #[test]
    fn finished_export_removes_its_work_dir() {
        // `.work` hard-links cache entries; left behind, it would pin their
        // bytes past eviction and `cache clear`.
        let t = tempfile::tempdir().unwrap();
        let out = t.path().join("out");
        let opts = ExportOptions {
            preset: render_book::BookPreset::P1,
            trip_name: "t".into(),
            typst_bin: "typst".into(),
            music_path: None,
            keep_ambience: false,
            only: "reels".into(),
            resolution: Default::default(),
            jobs: None,
            progress: Default::default(),
            segment_cache: Some(SegmentCacheConfig {
                dir: t.path().join("cache"),
                max_bytes: None,
            }),
        };
        let report = export_all(&Default::default(), &empty_plan(), &[], &out, &opts).unwrap();
        assert!(!out.join(".work").exists(), "{:?}", report.fallbacks);
        assert!(out.join("export.json").exists());
    }

    #[test]
    fn chapter_slideshow_takes_neighbors_once_and_skips_clips() {
        let dir = tempfile::tempdir().unwrap();
        let mut by_id = HashMap::new();
        for (id, kind) in [
            ("p0", AssetKind::Photo),
            ("p1", AssetKind::Photo),
            ("v2", AssetKind::Video),
            ("p3", AssetKind::Photo),
            ("p4", AssetKind::Photo),
        ] {
            let path = dir.path().join(format!("{id}.jpg"));
            std::fs::write(&path, b"x").unwrap();
            let mut asset = triptape_core::types::Asset::new(id, path.clone(), kind);
            let (size, mtime) = current_size_mtime(&path);
            asset.size_bytes = size.unwrap();
            asset.mtime = mtime.unwrap();
            by_id.insert(id.to_string(), asset);
        }
        // p4 changed on disk after planning: must not enter a slideshow.
        std::fs::write(&by_id["p4"].path, b"changed").unwrap();
        let mut plan = empty_plan();
        plan.chapters.push(triptape_core::types::Chapter {
            title: "Day 1".into(),
            asset_ids: ["p0", "p1", "v2", "p3", "p4"].map(String::from).to_vec(),
            ..Default::default()
        });
        let still = |id: &str| ResolvedEntry {
            entry: PlanEntry {
                asset_id: id.into(),
                window: 0,
                content_hash: None,
                size_bytes: 1,
                mtime: 0,
                rendition: Rendition::Still,
            },
            still_path: by_id[id].path.clone(),
            motion_path: None,
            caption: None,
            taken_at: None,
            tz_offset_secs: None,
            aspect: None,
        };
        // Both selected stills are reserved up front, as export_all does.
        let mut used: HashSet<PathBuf> = [&by_id["p1"].path, &by_id["p3"].path]
            .into_iter()
            .cloned()
            .collect();
        let mut fallbacks = vec![];
        let stems = |v: &[PathBuf]| -> Vec<String> {
            v.iter()
                .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
                .collect()
        };
        // From p1: clip skipped, p3 reserved for its own reel, changed p4
        // rejected, then wrap to p0.
        let a = chapter_slideshow(&plan, &by_id, &still("p1"), &mut used, &mut fallbacks);
        assert_eq!(stems(&a), ["p1", "p0"]);
        assert!(
            fallbacks
                .iter()
                .any(|f| f.contains("p4") && f.contains("changed")),
            "{fallbacks:?}"
        );
        // A second reel in the same chapter keeps its still, reuses nothing.
        let b = chapter_slideshow(&plan, &by_id, &still("p3"), &mut used, &mut fallbacks);
        assert_eq!(stems(&b), ["p3"]);
        // Motion highlights take neighbors only, never their own still.
        let mut clip = still("p0");
        clip.motion_path = Some(dir.path().join("p0.mov"));
        assert!(chapter_slideshow(&plan, &by_id, &clip, &mut used, &mut fallbacks).is_empty());
        let mut fresh: HashSet<PathBuf> = HashSet::new();
        let c = chapter_slideshow(&plan, &by_id, &clip, &mut fresh, &mut fallbacks);
        assert_eq!(stems(&c), ["p1", "p3"]);
        assert!(!fresh.contains(&by_id["p0"].path));
    }

    #[test]
    fn resolve_entry_carries_taken_at_and_caption() {
        let mut plan = empty_plan();
        plan.hero.push(PlanEntry {
            asset_id: "a0".into(),
            window: 0,
            content_hash: None,
            size_bytes: 1,
            mtime: 0,
            rendition: triptape_core::types::Rendition::Still,
        });
        plan.captions.insert("a0".into(), "Overlook".into());
        let mut asset =
            triptape_core::types::Asset::new("a0", PathBuf::from("/tmp/a0.jpg"), AssetKind::Photo);
        asset.taken_at = Some(1791288000);
        let assets: HashMap<String, triptape_core::types::Asset> =
            [("a0".to_string(), asset)].into_iter().collect();
        let r = resolve_entry(&plan, &assets, &plan.hero[0]).expect("resolves");
        assert_eq!(r.taken_at, Some(1791288000));
        assert_eq!(r.caption.as_deref(), Some("Overlook"));
    }

    #[test]
    fn chapter_place_names_the_chapter_not_the_hero_caption() {
        let mut plan = empty_plan();
        plan.chapters.push(triptape_core::types::Chapter {
            title: "Sensō-ji".into(),
            asset_ids: vec!["a0".into()],
            place: Some("Sensō-ji".into()),
            day_index: None,
            date_label: None,
        });
        assert_eq!(chapter_place(&plan, "a0"), Some("Sensō-ji"));
        assert_eq!(chapter_place(&plan, "other"), None);
    }
}
