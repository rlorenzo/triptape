//! Stage 1 — Enumerate: batched folder walk.
//!
//! Skips hidden/system files. Apple `.AAE` edit recipes and `.XMP`
//! sidecars are counted separately and never become assets.

use std::path::{Path, PathBuf};
use walkdir::{DirEntry, WalkDir};

pub const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "heic", "heif", "png", "webp", "tif", "tiff", "dng", "arw", "cr2", "nef",
];
pub const VIDEO_EXTS: &[&str] = &["mp4", "mov", "m4v", "avi", "mkv"];
/// Sidecar extensions: counted, never ingested.
pub const SIDECAR_EXTS: &[&str] = &["aae", "xmp", "thm"];

fn is_system_file(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        ".ds_store" | "thumbs.db" | "desktop.ini" | ".localized"
    )
}

/// True when any component of `path` (judged relative to the scan root, so a
/// scan root that itself lives under a dot-dir does not hide everything) is
/// hidden. Known system files (`.DS_Store`, …) are exempt here so they are
/// still counted as system files by `classify`, not as hidden.
fn is_hidden(path: &Path) -> bool {
    path.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        s.starts_with('.') && s.len() > 1 && !is_system_file(&s)
    })
}

/// One enumerated file that may become an asset.
#[derive(Debug, Clone)]
pub struct ScannedFile {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub mtime_secs: i64,
    pub ext: String,
}

/// Aggregate result of a scan.
#[derive(Debug, Default)]
pub struct ScanResult {
    pub files: Vec<ScannedFile>,
    /// `.AAE`/`.XMP`/`.THM` sidecars seen (never assets).
    pub sidecars_skipped: usize,
    pub hidden_skipped: usize,
    pub system_skipped: usize,
    pub unsupported_skipped: usize,
    /// Traversal or per-file stat failures (e.g. unreadable subdirectories,
    /// media whose metadata/mtime cannot be read). Non-empty means
    /// the scan is partial and the UI should warn.
    pub errors: Vec<ScanError>,
}

/// A path the walker could not read or stat.
#[derive(Debug, Clone)]
pub struct ScanError {
    pub path: Option<PathBuf>,
    pub message: String,
}

impl ScanResult {
    pub fn is_partial(&self) -> bool {
        !self.errors.is_empty()
    }
}

fn classify(entry: &DirEntry, result: &mut ScanResult) -> Option<ScannedFile> {
    let path = entry.path();
    let name = path.file_name()?.to_string_lossy();
    if is_system_file(&name) {
        result.system_skipped += 1;
        return None;
    }
    let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
    if SIDECAR_EXTS.contains(&ext.as_str()) {
        result.sidecars_skipped += 1;
        return None;
    }
    if !(IMAGE_EXTS.contains(&ext.as_str()) || VIDEO_EXTS.contains(&ext.as_str())) {
        result.unsupported_skipped += 1;
        return None;
    }
    // A supported media file we cannot stat is a partial-scan error, not a
    // silent skip. walkdir already holds this entry's metadata; avoid a
    // second stat.
    let stat = entry
        .metadata()
        .map_err(|e| e.to_string())
        .and_then(|md| md.modified().map(|t| (md, t)).map_err(|e| e.to_string()));
    let (md, modified) = match stat {
        Ok(v) => v,
        Err(message) => {
            result.errors.push(ScanError {
                path: Some(path.to_path_buf()),
                message,
            });
            return None;
        }
    };
    // Pre-1970 mtimes (bad camera clocks, restored archives) are valid
    // timestamps; represent them as negative seconds rather than dropping.
    let mtime = match modified.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    };
    Some(ScannedFile {
        path: path.to_path_buf(),
        size_bytes: md.len(),
        mtime_secs: mtime,
        ext,
    })
}

/// Hand a non-empty batch to the caller, then move it into the aggregate.
fn flush(
    batch: &mut Vec<ScannedFile>,
    result: &mut ScanResult,
    on_batch: &mut impl FnMut(&[ScannedFile]),
) {
    if !batch.is_empty() {
        on_batch(batch);
        result.files.append(batch);
    }
}

/// Walk `root` and invoke `on_batch` with slices of at most `batch_size` files.
/// Returns the aggregate counts plus all files (callers that only need counts
/// can drop `files`).
pub fn scan_folder_batched(
    root: &Path,
    batch_size: usize,
    mut on_batch: impl FnMut(&[ScannedFile]),
) -> Result<ScanResult, String> {
    if !root.is_dir() {
        return Err(format!("not a directory: {}", root.display()));
    }
    let batch_size = batch_size.max(1);
    let mut result = ScanResult::default();
    let mut batch: Vec<ScannedFile> = Vec::with_capacity(batch_size);

    for entry in WalkDir::new(root).follow_links(false) {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                result.errors.push(ScanError {
                    path: e.path().map(Path::to_path_buf),
                    message: e.to_string(),
                });
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        // Judge hidden-ness relative to the scan root: ancestors above the
        // root (e.g. tempfile's `.tmpXXXXXX` staging dir) must not hide the
        // whole tree the user asked us to scan.
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        if is_hidden(rel) {
            result.hidden_skipped += 1;
            continue;
        }
        if let Some(f) = classify(&entry, &mut result) {
            batch.push(f);
            if batch.len() >= batch_size {
                flush(&mut batch, &mut result, &mut on_batch);
            }
        }
    }
    flush(&mut batch, &mut result, &mut on_batch);
    Ok(result)
}

/// Convenience wrapper with the default ~500-file batch size.
pub fn scan_folder(root: &Path) -> Result<ScanResult, String> {
    scan_folder_batched(root, 500, |_| {})
}

/// Determinate progress for the hashing phase of ingest: the first half of
/// the job (0.0..=0.5). `done` counts finished files out of `total`. An
/// empty file set is trivially complete at the phase boundary (0.5).
/// The result never exceeds 0.5 and is monotonic non-decreasing in `done`.
pub fn hash_phase_progress(done: usize, total: usize) -> f32 {
    if total == 0 {
        return 0.5;
    }
    (done as f32 / total as f32).clamp(0.0, 1.0) * 0.5
}

/// Step label for the enumeration phase, where the file total is unknown.
/// Callers keep progress at 0 alongside this label so the UI stays
/// indeterminate-friendly instead of faking a precise percent.
pub fn walk_step_label(found: usize) -> String {
    format!("Finding files… {found} found")
}

/// Step label for the background hashing phase, mirroring the old
/// foreground format so the progress text keeps working.
pub fn hash_step_label(done: usize, total: usize) -> String {
    format!("Hashing files… {done} of {total}")
}

/// Step label for the background scoring phase, same shape as the hashing
/// label so the progress text keeps working.
pub fn score_step_label(done: usize, total: usize) -> String {
    format!("Scoring photos… {done} of {total}")
}

/// Determinate progress for the background hashing phase of progressive
/// ingest: byte-weighted completion mapped into the remaining job range
/// (`base`..=`ceiling`, e.g. 0.15..=0.95 after the foreground steps). An
/// empty file set is trivially complete at the ceiling. The result stays
/// within the range and is monotonic non-decreasing in `bytes_done`.
pub fn background_hash_progress(bytes_done: u64, total_bytes: u64, base: f32, ceiling: f32) -> f32 {
    if total_bytes == 0 {
        return ceiling;
    }
    let frac = (bytes_done as f32 / total_bytes as f32).clamp(0.0, 1.0);
    base + frac * (ceiling - base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(p: &Path, bytes: &[u8]) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, bytes).unwrap();
    }

    #[test]
    fn skips_hidden_and_counts_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("IMG_001.jpg"), b"fake-jpg");
        write(&root.join("IMG_001.AAE"), b"sidecar");
        write(&root.join("edit.XMP"), b"sidecar");
        write(&root.join(".hidden.jpg"), b"nope");
        write(&root.join(".DS_Store"), b"nope");
        write(&root.join("notes.txt"), b"nope");

        let r = scan_folder(root).unwrap();
        assert_eq!(r.files.len(), 1, "only the jpg becomes a file");
        assert_eq!(r.sidecars_skipped, 2);
        assert_eq!(r.hidden_skipped, 1);
        assert_eq!(r.system_skipped, 1);
        assert_eq!(r.unsupported_skipped, 1);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_dir_is_reported() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("ok.jpg"), b"x");
        let locked = root.join("locked");
        write(&locked.join("IMG.jpg"), b"x");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let readable_anyway = fs::read_dir(&locked).is_ok(); // e.g. running as root
        let r = scan_folder(root);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let r = r.unwrap();
        if !readable_anyway {
            assert!(r.is_partial());
            assert_eq!(r.errors[0].path.as_deref(), Some(locked.as_path()));
            assert_eq!(r.files.len(), 1);
        }
    }

    #[test]
    fn batches_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..7 {
            write(&dir.path().join(format!("P{i:03}.jpg")), b"fake-jpg-bytes");
        }
        let mut sizes = vec![];
        let r = scan_folder_batched(dir.path(), 3, |b| sizes.push(b.len())).unwrap();
        assert_eq!(r.files.len(), 7);
        assert_eq!(sizes, vec![3, 3, 1]);
    }

    #[test]
    fn hash_phase_progress_maps_first_half_monotonically() {
        assert_eq!(hash_phase_progress(0, 100), 0.0);
        assert_eq!(hash_phase_progress(50, 100), 0.25);
        assert_eq!(hash_phase_progress(100, 100), 0.5);
        // Empty set is trivially complete at the phase boundary.
        assert_eq!(hash_phase_progress(0, 0), 0.5);
        // Over-counts clamp instead of overshooting into later phases.
        assert_eq!(hash_phase_progress(200, 100), 0.5);
        let mut last = 0.0f32;
        for done in 0..=100 {
            let p = hash_phase_progress(done, 100);
            assert!(p >= last, "must be non-decreasing at done={done}");
            assert!(p <= 0.5);
            last = p;
        }
    }

    #[test]
    fn background_hash_progress_stays_in_range_and_monotonic() {
        let near =
            |got: f32, want: f32| assert!((got - want).abs() < 1e-6, "expected {want}, got {got}");
        near(background_hash_progress(0, 1000, 0.15, 0.95), 0.15);
        near(background_hash_progress(1000, 1000, 0.15, 0.95), 0.95);
        // Empty set is trivially complete at the ceiling.
        near(background_hash_progress(0, 0, 0.15, 0.95), 0.95);
        // Over-counts clamp instead of overshooting the ceiling.
        near(background_hash_progress(9999, 1000, 0.15, 0.95), 0.95);
        let mut last = 0.15f32;
        for done in (0..=1000).step_by(7) {
            let p = background_hash_progress(done, 1000, 0.15, 0.95);
            assert!(p >= last, "must be non-decreasing at done={done}");
            assert!((0.15..=0.95).contains(&p), "out of range: {p}");
            last = p;
        }
        assert_eq!(hash_step_label(3, 12), "Hashing files… 3 of 12");
        assert_eq!(score_step_label(3, 12), "Scoring photos… 3 of 12");
    }

    #[test]
    fn walk_step_label_reports_count_without_faking_percent() {
        assert_eq!(walk_step_label(0), "Finding files… 0 found");
        assert_eq!(walk_step_label(1200), "Finding files… 1200 found");
    }

    #[test]
    fn hidden_dirs_hide_contents_and_empty_batch_not_emitted() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join(".cache/IMG_1.jpg"), b"x");
        write(&dir.path().join("sub/.DS_Store"), b"x");
        let mut calls = 0;
        let r = scan_folder_batched(dir.path(), 10, |_| calls += 1).unwrap();
        assert_eq!(r.files.len(), 0);
        assert_eq!(r.hidden_skipped, 1);
        assert_eq!(r.system_skipped, 1);
        assert_eq!(calls, 0, "no files means no batch callback");
        assert!(scan_folder(&dir.path().join("nope")).is_err());
    }
}
