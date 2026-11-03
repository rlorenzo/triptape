//! `export` orchestration: one plan in, three keepsakes out.
//!
//! Honors Sec 5.4 eligibility — disabled keepsakes are skipped with the
//! plain-language reason printed (and recorded in the report), never padded
//! and never failed silently.

use crate::ffmpeg::Capabilities;
use crate::render_book::{self, BookOptions, BookPhoto};
use crate::render_reels::{self, ReelOptions};
use crate::render_video::{self, ResolvedEntry, VideoOptions};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use triptape_core::plan::{check_source_integrity, plan_eligibility, Integrity};
use triptape_core::types::{AssetKind, Plan, PlanEntry};

#[derive(Debug, Clone)]
pub struct ExportOptions {
    pub preset: render_book::BookPreset,
    pub trip_name: String,
    pub typst_bin: String,
    pub music_path: Option<PathBuf>,
    /// "all" (default), or one keepsake: "video" | "reels" | "book".
    pub only: String,
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
}

/// Resolve a plan entry to files from the asset map.
fn resolve_entry(
    plan: &Plan,
    assets: &HashMap<String, triptape_core::types::Asset>,
    asset_id: &str,
) -> Option<ResolvedEntry> {
    let a = assets.get(asset_id)?;
    let entry = plan
        .hero
        .iter()
        .chain(plan.reel_selects.iter())
        .find(|e| e.asset_id == asset_id)?
        .clone();
    let (still_path, motion_path) = match a.kind {
        AssetKind::Photo => (a.path.clone(), None),
        AssetKind::Video => (a.path.clone(), Some(a.path.clone())),
        AssetKind::LivePhoto => (a.path.clone(), a.companion_path.clone()),
    };
    Some(ResolvedEntry {
        entry,
        still_path,
        motion_path,
        caption: plan.captions.get(asset_id).cloned(),
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
    let r = resolve_entry(plan, assets, &entry.asset_id)?;
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
    let usable = plan.hero.len();
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
    };

    // Map card once, shared by the hero film and the book map page.
    let mut map_card: Option<PathBuf> = None;
    if !plan.map_polyline.is_empty() {
        let card = work.join("route_card.png");
        match crate::map::save_route_card(&plan.map_polyline, &[], 1920, 1080, &card) {
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
                music_path: opts.music_path.clone(),
                ..Default::default()
            },
        ) {
            Ok(v) => {
                report.video = Some(out_mp4.to_string_lossy().into_owned());
                report.fallbacks.extend(v.fallbacks);
            }
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
            .collect();
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
                    music_path: opts.music_path.clone(),
                    ..Default::default()
                },
            ) {
                Ok(r) => {
                    report.reels = r.reels.iter().map(|f| f.path.clone()).collect();
                    report.fallbacks.extend(r.fallbacks);
                }
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
            book_photos.push(BookPhoto {
                asset_id: a.id.clone(),
                path: a.path.clone(),
                width: w,
                height: h,
                caption: plan.captions.get(&a.id).cloned(),
            });
        }
        // Story order: keep db order (taken_at, id).
        let book_dir = out_dir.join("book");
        let chapter_titles: Vec<String> = plan.chapters.iter().map(|c| c.title.clone()).collect();
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
            },
        ) {
            Ok(b) => {
                report.book_pages = b.counted_pages;
                report.fallbacks.extend(b.fallbacks);
            }
            Err(e) => report.skipped.push(Skipped {
                keepsake: "book".into(),
                reason: format!("preflight/render blocked: {e}"),
            }),
        }
    }

    let report_path = out_dir.join("export.json");
    std::fs::write(&report_path, serde_json::to_string_pretty(&report)?)
        .map_err(|e| crate::MediaError::Io(e.to_string()))?;
    Ok(report)
}
