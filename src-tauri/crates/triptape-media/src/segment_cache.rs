//! Content-addressed cache of rendered hero-film segments.
//!
//! A re-export only needs to render what changed, so each segment is stored
//! under a key hashing everything that decides its pixels and audio (see
//! [`KeyBuilder`]). A key never mentions the work directory, so it is stable
//! across exports.
//!
//! # Layout
//!
//! ```text
//! <dir>/<kk>/<key>.mp4      the segment (kk = first two hex digits)
//! <dir>/<kk>/<key>.json     sidecar: size, planned seconds
//! <dir>/leases/<id>.lease   one per running export (pid + start time)
//! <dir>/.tmp-*              in-flight writes; never read
//! ```
//!
//! # Atomic writes
//!
//! An entry is visible only once both its `.mp4` and its `.json` exist and
//! the sidecar's recorded size matches the file. A store builds the `.mp4`
//! under a `.tmp-*` name, renames it into place, and renames the sidecar
//! last. A render that is killed or cancelled never reaches [`store`], and a
//! crash between the two renames leaves an mp4 without sidecar, which is
//! invisible and swept as an orphan.
//!
//! # Protection while an export runs
//!
//! The cache lives where the OS and cleaners (macOS storage management,
//! CleanMyMac, the user) are free to delete it, but an export must not lose a
//! segment it is about to assemble. Two layers:
//!
//! 1. **Materialization.** Every entry an export uses (hit or fresh write) is
//!    placed in the export's own `<out>/.work/` by hard link (APFS clone, then
//!    copy, when the volumes differ). The export then holds its own reference
//!    to the data: deleting the cache file leaves the `.work` file intact. When
//!    the export ends `.work` is removed, so the cache file is the only copy
//!    again and is freely evictable or purgeable.
//! 2. **Lease.** While an export runs, a lease file in the cache dir records
//!    its pid and start time. Eviction and `cache clear` (from any process)
//!    skip every entry used since the oldest live lease started. Stale leases
//!    (dead pid, or older than [`LEASE_MAX_AGE`]) are ignored and removed.
//!
//! Work files are never modified in place (every rewrite goes through
//! [`Capabilities::run`]'s partial-then-rename), so sharing an inode with the
//! cache is safe.
//!
//! [`store`]: SegmentCache::store
//! [`Capabilities::run`]: crate::ffmpeg::Capabilities::run

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Bump when segment pixels change in a way the generated ffmpeg arguments do
/// not show: the collage layout, cover-crop and blur code, or the Ken Burns
/// parameter maths that lives outside the filter strings. Argument changes
/// are already in every key.
pub const SEGMENT_CACHE_VERSION: u32 = 2;

/// Skip new entries when the cache volume has less than this free.
pub const LOW_DISK_BYTES: u64 = 5 << 30;
/// Ceiling of the default size cap.
pub const DEFAULT_CAP_BYTES: u64 = 20 << 30;
/// A lease this old is dead whatever its pid says (pid reuse guard).
const LEASE_MAX_AGE: Duration = Duration::from_secs(3 * 24 * 3600);
/// In-flight and orphaned files younger than this may belong to a live write.
const ORPHAN_GRACE: Duration = Duration::from_secs(3600);

/// Where the cache goes when nothing is configured: the platform cache dir
/// (`~/Library/Caches` on macOS) under the app identifier.
pub fn default_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("app.triptape.desktop").join("segments"))
}

/// Cache location and size cap for one export.
#[derive(Debug, Clone)]
pub struct SegmentCacheConfig {
    pub dir: PathBuf,
    /// `None`: min([`DEFAULT_CAP_BYTES`], 10% of the volume's free space).
    pub max_bytes: Option<u64>,
}

/// Parse `500MB`, `20GB`, `1.5G`, `1024` (bytes). Decimal-free suffix units
/// are binary (1 GB = 2^30).
pub fn parse_size(text: &str) -> Result<u64, String> {
    let t = text.trim().to_ascii_lowercase();
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: f64 = num.parse().map_err(|_| format!("bad size {text:?}"))?;
    let mult: f64 = match unit.trim() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => (1u64 << 20) as f64,
        "g" | "gb" | "gib" => (1u64 << 30) as f64,
        "t" | "tb" | "tib" => (1u64 << 40) as f64,
        _ => return Err(format!("bad size unit in {text:?}")),
    };
    if !n.is_finite() || n < 0.0 {
        return Err(format!("bad size {text:?}"));
    }
    Ok((n * mult) as u64)
}

/// Human size for messages.
pub fn fmt_bytes(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// Hashes the inputs of one segment. Every part is length-prefixed so
/// `("a","bc")` and `("ab","c")` differ.
pub struct KeyBuilder {
    hasher: blake3::Hasher,
    work_dir: String,
}

impl KeyBuilder {
    fn new(ffmpeg_version: &str, work_dir: &Path, kind: &str) -> Self {
        let mut k = Self {
            hasher: blake3::Hasher::new(),
            work_dir: work_dir.to_string_lossy().into_owned(),
        };
        k.part("cache-version", &SEGMENT_CACHE_VERSION.to_string());
        k.part("ffmpeg", ffmpeg_version);
        k.part("kind", kind);
        k
    }

    /// A labelled text value.
    pub fn part(&mut self, label: &str, value: &str) -> &mut Self {
        for s in [label, value] {
            self.hasher.update(&(s.len() as u64).to_le_bytes());
            self.hasher.update(s.as_bytes());
        }
        self
    }

    /// A labelled number, in a form that survives float formatting.
    pub fn num(&mut self, label: &str, v: f64) -> &mut Self {
        self.part(label, &format!("{:016x}", v.to_bits()))
    }

    /// Source identity: canonical path, size and mtime (nanoseconds). A file
    /// that cannot be stat'ed contributes its absence, so it never matches a
    /// present one.
    pub fn file(&mut self, label: &str, path: &Path) -> &mut Self {
        let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let ident = match std::fs::metadata(&canon) {
            Ok(md) => {
                let mtime = md
                    .modified()
                    .ok()
                    .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos());
                format!("{}|{}|{mtime}", canon.display(), md.len())
            }
            Err(_) => format!("{}|missing", canon.display()),
        };
        self.part(label, &ident)
    }

    /// The full generated ffmpeg argument list, with the work directory
    /// normalized out.
    pub fn args(&mut self, args: &[String]) -> &mut Self {
        let norm: Vec<std::borrow::Cow<str>> = args
            .iter()
            .map(|a| {
                if !self.work_dir.is_empty() && a.contains(&self.work_dir) {
                    a.replace(&self.work_dir, "<WORK>").into()
                } else {
                    a.as_str().into()
                }
            })
            .collect();
        self.part("args", &norm.join("\u{1f}"))
    }

    pub fn finish(&self) -> String {
        self.hasher.finalize().to_hex().to_string()
    }
}

// ---------------------------------------------------------------------------
// Entries
// ---------------------------------------------------------------------------

/// Sidecar of one entry.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SegmentMeta {
    /// Seconds the renderer reported for the segment.
    pub secs: f64,
    /// Size of the `.mp4`, to reject a torn or swapped file cheaply.
    pub size: u64,
}

/// Counters for one export.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UseStats {
    pub hits: usize,
    pub misses: usize,
    pub stored: usize,
    /// Fresh segments not cached because the volume was low on space.
    pub skipped_low_disk: usize,
}

/// A live export, as recorded in its lease file.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Lease {
    pid: u32,
    started_ms: u64,
}

/// What [`SegmentCache::evict`] and [`clear`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EvictReport {
    pub removed: usize,
    pub freed: u64,
    /// Entries kept because a live export used them.
    pub protected: usize,
}

/// Totals of what is on disk.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiskStats {
    pub entries: usize,
    pub bytes: u64,
    pub live_leases: usize,
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn io_err(e: std::io::Error) -> crate::MediaError {
    crate::MediaError::Io(e.to_string())
}

/// One export's handle on the cache. Dropping it releases the lease.
pub struct SegmentCache {
    dir: PathBuf,
    max_bytes: Option<u64>,
    ffmpeg_version: String,
    work_dir: PathBuf,
    lease_path: Mutex<Option<PathBuf>>,
    stats: Mutex<UseStats>,
}

impl std::fmt::Debug for SegmentCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SegmentCache")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

impl SegmentCache {
    /// Open the cache for an export whose intermediates live in `work_dir`,
    /// and take a lease. `ffmpeg_version` is the first line of
    /// `ffmpeg -version` (see [`ffmpeg_version`]).
    pub fn open(
        cfg: &SegmentCacheConfig,
        ffmpeg_version: String,
        work_dir: &Path,
    ) -> Result<Arc<Self>, crate::MediaError> {
        std::fs::create_dir_all(cfg.dir.join("leases")).map_err(io_err)?;
        let cache = Self {
            dir: cfg.dir.clone(),
            max_bytes: cfg.max_bytes,
            ffmpeg_version,
            work_dir: work_dir.to_path_buf(),
            lease_path: Mutex::new(None),
            stats: Mutex::new(UseStats::default()),
        };
        let pid = std::process::id();
        let path = cache.dir.join("leases").join(format!(
            "{pid}-{}-{}.lease",
            now_ms(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let lease = Lease {
            pid,
            started_ms: now_ms(),
        };
        // Written aside and renamed in: a half-written lease would read as
        // stale to another process and be swept.
        let tmp = cache.tmp_name();
        std::fs::write(&tmp, serde_json::to_vec(&lease)?)
            .and_then(|()| std::fs::rename(&tmp, &path))
            .map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                io_err(e)
            })?;
        *cache.lease_path.lock().expect("lease lock") = Some(path);
        Ok(Arc::new(cache))
    }

    /// The cache directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Start a key for a segment of the given kind.
    pub fn key(&self, kind: &str) -> KeyBuilder {
        KeyBuilder::new(&self.ffmpeg_version, &self.work_dir, kind)
    }

    pub fn stats(&self) -> UseStats {
        *self.stats.lock().expect("stats lock")
    }

    fn bump(&self, f: impl FnOnce(&mut UseStats)) {
        f(&mut self.stats.lock().expect("stats lock"));
    }

    /// On a hit, materialize the entry at `dest` (a path in the work dir) and
    /// return its sidecar. A miss, a torn entry, or an entry that vanished
    /// mid-way all return `None`.
    pub fn fetch(&self, key: &str, dest: &Path) -> Option<SegmentMeta> {
        let meta = read_valid(&self.dir, key);
        let Some(meta) = meta else {
            self.bump(|s| s.misses += 1);
            return None;
        };
        // Touch first: the lease rule protects entries used since a lease
        // began, and LRU reads the same timestamp.
        let mp4 = entry_path(&self.dir, key, "mp4");
        touch(&mp4);
        if place(&mp4, dest).is_err() {
            // Deleted between the check and the link: a miss, not an error.
            let _ = std::fs::remove_file(dest);
            self.bump(|s| s.misses += 1);
            return None;
        }
        self.bump(|s| s.hits += 1);
        Some(meta)
    }

    /// Cache the finished segment at `produced`. Best effort: a full disk or
    /// an I/O error is swallowed because the export itself already has its
    /// segment. Returns whether the entry was stored.
    pub fn store(&self, key: &str, produced: &Path, secs: f64) -> bool {
        if read_valid(&self.dir, key).is_some() {
            return true;
        }
        if free_bytes(&self.dir).is_some_and(|f| f < LOW_DISK_BYTES) {
            self.bump(|s| s.skipped_low_disk += 1);
            return false;
        }
        let Ok(size) = std::fs::metadata(produced).map(|m| m.len()) else {
            return false;
        };
        if size == 0 {
            return false;
        }
        let ok = self
            .try_store(key, produced, &SegmentMeta { secs, size })
            .is_ok();
        if ok {
            self.bump(|s| s.stored += 1);
        }
        ok
    }

    fn try_store(&self, key: &str, produced: &Path, meta: &SegmentMeta) -> std::io::Result<()> {
        let mp4 = entry_path(&self.dir, key, "mp4");
        let json = entry_path(&self.dir, key, "json");
        std::fs::create_dir_all(mp4.parent().expect("entry has a parent"))?;
        let tmp_mp4 = self.tmp_name();
        let tmp_json = self.tmp_name();
        let result = (|| {
            place(produced, &tmp_mp4)?;
            // The data must be durable before the entry becomes visible.
            std::fs::File::open(&tmp_mp4)?.sync_all()?;
            let mut f = std::fs::File::create(&tmp_json)?;
            f.write_all(&serde_json::to_vec(meta).map_err(std::io::Error::other)?)?;
            f.sync_all()?;
            std::fs::rename(&tmp_mp4, &mp4)?;
            std::fs::rename(&tmp_json, &json)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp_mp4);
            let _ = std::fs::remove_file(&tmp_json);
        }
        result
    }

    fn tmp_name(&self) -> PathBuf {
        self.dir.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// End the export's claim on the cache: release the lease, then evict
    /// least-recently-used entries down to the size cap (entries of other
    /// live exports stay).
    pub fn finish(&self) -> EvictReport {
        self.release_lease();
        evict_to(&self.dir, resolve_cap(self.max_bytes, &self.dir), now_ms())
    }

    fn release_lease(&self) {
        if let Some(p) = self.lease_path.lock().expect("lease lock").take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

impl Drop for SegmentCache {
    fn drop(&mut self) {
        self.release_lease();
    }
}

/// min([`DEFAULT_CAP_BYTES`], 10% of the volume's free space) unless set.
pub fn resolve_cap(max_bytes: Option<u64>, dir: &Path) -> u64 {
    max_bytes.unwrap_or_else(|| {
        free_bytes(dir).map_or(DEFAULT_CAP_BYTES, |f| (f / 10).min(DEFAULT_CAP_BYTES))
    })
}

/// `ffmpeg -version`'s first line, or the binary name when it will not run.
pub fn ffmpeg_version(ffmpeg_bin: &str) -> String {
    crate::ffmpeg::run_capture(ffmpeg_bin, &["-version"])
        .and_then(|out| out.lines().next().map(str::to_string))
        .unwrap_or_else(|| format!("unknown:{ffmpeg_bin}"))
}

// ---------------------------------------------------------------------------
// Disk operations shared with the CLI
// ---------------------------------------------------------------------------

/// `<dir>/<kk>/<key>.<ext>` (`ext`: "mp4" or "json").
fn entry_path(dir: &Path, key: &str, ext: &str) -> PathBuf {
    dir.join(&key[..2.min(key.len())])
        .join(format!("{key}.{ext}"))
}

/// The sidecar at `json` when it describes a complete `.mp4` of `mp4_len`
/// bytes: parseable, nonzero, size matches.
fn complete_meta(json: &Path, mp4_len: u64) -> Option<SegmentMeta> {
    let meta: SegmentMeta = serde_json::from_slice(&std::fs::read(json).ok()?).ok()?;
    (mp4_len > 0 && mp4_len == meta.size && meta.secs.is_finite()).then_some(meta)
}

/// The sidecar when the entry is complete (see [`complete_meta`]).
fn read_valid(dir: &Path, key: &str) -> Option<SegmentMeta> {
    let len = std::fs::metadata(entry_path(dir, key, "mp4")).ok()?.len();
    complete_meta(&entry_path(dir, key, "json"), len)
}

/// Set mtime to now (the LRU and lease timestamp). Opening for write does not
/// truncate; failure is harmless (the entry just looks older).
fn touch(path: &Path) {
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
        let _ = f.set_modified(SystemTime::now());
    }
}

/// Give `dst` its own reference to `src`'s data: hard link, else APFS clone,
/// else a full copy (different volumes). Replaces `dst`.
fn place(src: &Path, dst: &Path) -> std::io::Result<()> {
    let _ = std::fs::remove_file(dst);
    if std::fs::hard_link(src, dst).is_ok() {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    if clone_file(src, dst).is_ok() {
        return Ok(());
    }
    std::fs::copy(src, dst).map(|_| ())
}

#[cfg(target_os = "macos")]
fn clone_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = |p: &Path| {
        std::ffi::CString::new(p.as_os_str().as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
    };
    let (s, d) = (c(src)?, c(dst)?);
    // SAFETY: both pointers are valid NUL-terminated strings for the call.
    let rc = unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Free bytes on the volume holding `path` (or its nearest existing parent,
/// since the cache dir may not exist yet).
fn free_bytes(path: &Path) -> Option<u64> {
    let existing = path.ancestors().find(|p| p.exists())?;
    triptape_core::jobs::free_bytes(existing)
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: signal 0 only checks existence and permission.
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// `<pid>-<timestamp>-<seq>.lease`, the only lease names [`SegmentCache::open`]
/// makes.
fn is_lease_name(name: &str) -> bool {
    name.strip_suffix(".lease")
        .is_some_and(|stem| is_digit_fields(stem, 3))
}

/// `s` is exactly `n` nonempty runs of ASCII digits joined by `-`.
fn is_digit_fields(s: &str, n: usize) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == n
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit()))
}

/// Leases of exports still running. With `sweep`, stale lease files are
/// deleted. Only regular files named like the cache's own leases are read;
/// anything else under `leases/` is left alone.
fn live_leases(dir: &Path, now: u64, sweep: bool) -> Vec<Lease> {
    let mut live = vec![];
    let Ok(rd) = std::fs::read_dir(dir.join("leases")) else {
        return live;
    };
    for e in rd.flatten() {
        if !is_lease_name(&e.file_name().to_string_lossy())
            || !e.file_type().is_ok_and(|t| t.is_file())
        {
            continue;
        }
        let path = e.path();
        let lease: Option<Lease> = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        match lease {
            Some(l)
                if pid_alive(l.pid)
                    && now.saturating_sub(l.started_ms) < LEASE_MAX_AGE.as_millis() as u64 =>
            {
                live.push(l)
            }
            _ if sweep => {
                let _ = std::fs::remove_file(&path);
            }
            _ => {}
        }
    }
    live
}

struct DiskEntry {
    mp4: PathBuf,
    json: PathBuf,
    size: u64,
    last_used_ms: u64,
}

/// `s` is exactly `n` lowercase hex digits (the form [`KeyBuilder::finish`]
/// produces).
fn is_hex(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `.tmp-<pid>-<seq>`, the only temp names [`SegmentCache::tmp_name`] makes.
fn is_cache_tmp(name: &str) -> bool {
    name.strip_prefix(".tmp-")
        .is_some_and(|rest| is_digit_fields(rest, 2))
}

/// Complete entries. With `sweep`, also delete old temp files and orphans.
/// Only names the cache itself creates are ever considered (`<kk>/` shard
/// dirs holding `<key>.mp4` with a 64-hex key starting with `kk`, and
/// `.tmp-<pid>-<seq>`), so pointing the cache at a directory with other
/// files never touches them.
fn scan(dir: &Path, now: u64, sweep: bool) -> Vec<DiskEntry> {
    let mut out = vec![];
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    let old = |md: &std::fs::Metadata| {
        md.modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age > ORPHAN_GRACE)
    };
    for e in rd.flatten() {
        let path = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(md) = e.metadata() else { continue };
        if md.is_file() && is_cache_tmp(&name) {
            if sweep && old(&md) {
                let _ = std::fs::remove_file(&path);
            }
            continue;
        }
        if !md.is_dir() || !is_hex(&name, 2) {
            continue;
        }
        let Ok(sub) = std::fs::read_dir(&path) else {
            continue;
        };
        for f in sub.flatten() {
            let fp = f.path();
            if fp.extension().and_then(|x| x.to_str()) != Some("mp4") {
                continue;
            }
            let owned = fp
                .file_stem()
                .and_then(|x| x.to_str())
                .is_some_and(|k| is_hex(k, 64) && k.starts_with(name.as_str()));
            if !owned {
                continue;
            }
            let Ok(fmd) = f.metadata() else { continue };
            let json = fp.with_extension("json");
            if complete_meta(&json, fmd.len()).is_none() {
                if sweep && old(&fmd) {
                    let _ = std::fs::remove_file(&fp);
                    let _ = std::fs::remove_file(&json);
                }
                continue;
            }
            let last = fmd
                .modified()
                .ok()
                .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                .map_or(now, |d| d.as_millis() as u64);
            out.push(DiskEntry {
                mp4: fp,
                json,
                size: fmd.len(),
                last_used_ms: last,
            });
        }
    }
    out
}

/// Remove oldest-used unprotected entries until the total is at most `cap`
/// (`0`: remove every unprotected entry).
fn evict_to(dir: &Path, cap: u64, now: u64) -> EvictReport {
    let mut entries = scan(dir, now, true);
    let leases = live_leases(dir, now, true);
    let floor = leases.iter().map(|l| l.started_ms).min();
    let mut total: u64 = entries.iter().map(|e| e.size).sum();
    entries.sort_by_key(|e| e.last_used_ms);
    let mut report = EvictReport::default();
    for e in entries {
        if total <= cap {
            break;
        }
        if floor.is_some_and(|f| e.last_used_ms >= f) {
            report.protected += 1;
            continue;
        }
        // Sidecar first: the entry is invisible the moment it goes.
        let _ = std::fs::remove_file(&e.json);
        if std::fs::remove_file(&e.mp4).is_ok() {
            report.removed += 1;
            report.freed += e.size;
            total = total.saturating_sub(e.size);
        }
    }
    report
}

/// `triptape cache clear`: remove every entry no live export has used.
pub fn clear(dir: &Path) -> EvictReport {
    evict_to(dir, 0, now_ms())
}

/// `triptape cache stats`. Read-only: no orphan or temp-file sweep.
pub fn disk_stats(dir: &Path) -> DiskStats {
    let now = now_ms();
    let entries = scan(dir, now, false);
    DiskStats {
        entries: entries.len(),
        bytes: entries.iter().map(|e| e.size).sum(),
        live_leases: live_leases(dir, now, false).len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(dir: &Path, max: Option<u64>) -> SegmentCacheConfig {
        SegmentCacheConfig {
            dir: dir.to_path_buf(),
            max_bytes: max,
        }
    }

    fn open(dir: &Path, work: &Path, max: Option<u64>) -> Arc<SegmentCache> {
        std::fs::create_dir_all(work).unwrap();
        SegmentCache::open(&cfg(dir, max), "ffmpeg version test".into(), work).unwrap()
    }

    fn produce(work: &Path, name: &str, bytes: usize) -> PathBuf {
        let p = work.join(name);
        std::fs::write(&p, vec![7u8; bytes]).unwrap();
        p
    }

    fn set_age(path: &Path, secs_ago: u64) {
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(secs_ago))
            .unwrap();
    }

    #[test]
    fn key_is_stable_and_sensitive_to_every_input() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("a.jpg");
        std::fs::write(&src, b"x").unwrap();
        let c = open(&t.path().join("c"), &t.path().join("w"), None);
        let base = |f: &dyn Fn(&mut KeyBuilder)| {
            let mut k = c.key("still");
            k.file("src", &src).num("secs", 4.0).args(&["-i".into()]);
            f(&mut k);
            k.finish()
        };
        let a = base(&|_| {});
        assert_eq!(a, base(&|_| {}), "same inputs, same key");
        assert_ne!(
            a,
            base(&|k| {
                k.part("extra", "1");
            })
        );
        let mut other_kind = c.key("clip");
        other_kind
            .file("src", &src)
            .num("secs", 4.0)
            .args(&["-i".into()]);
        assert_ne!(a, other_kind.finish());
        let mut other_num = c.key("still");
        other_num
            .file("src", &src)
            .num("secs", 4.5)
            .args(&["-i".into()]);
        assert_ne!(a, other_num.finish());
        let mut other_args = c.key("still");
        other_args
            .file("src", &src)
            .num("secs", 4.0)
            .args(&["-j".into()]);
        assert_ne!(a, other_args.finish());
        // The source changing (size) changes the key.
        std::fs::write(&src, b"xy").unwrap();
        assert_ne!(a, base(&|_| {}));
        // Another ffmpeg changes it too.
        let c2 = SegmentCache::open(
            &cfg(&t.path().join("c"), None),
            "ffmpeg version other".into(),
            &t.path().join("w"),
        )
        .unwrap();
        let mut k2 = c2.key("still");
        k2.file("src", &src).num("secs", 4.0).args(&["-i".into()]);
        assert_ne!(base(&|_| {}), k2.finish());
    }

    #[test]
    fn key_ignores_the_work_dir() {
        let t = tempfile::tempdir().unwrap();
        let mk = |work: &str| {
            let w = t.path().join(work);
            let c = open(&t.path().join("c"), &w, None);
            let mut k = c.key("clip");
            k.args(&[
                "-i".into(),
                "/media/a.mov".into(),
                w.join("seg_003.mp4").to_string_lossy().into_owned(),
            ]);
            k.finish()
        };
        assert_eq!(mk("exp1/.work"), mk("elsewhere/.work"));
    }

    #[test]
    fn key_parts_are_length_prefixed() {
        let t = tempfile::tempdir().unwrap();
        let c = open(&t.path().join("c"), &t.path().join("w"), None);
        let mut a = c.key("k");
        a.part("a", "bc");
        let mut b = c.key("k");
        b.part("ab", "c");
        assert_ne!(a.finish(), b.finish());
    }

    #[test]
    fn miss_then_store_then_hit_materializes() {
        let t = tempfile::tempdir().unwrap();
        let (cdir, work) = (t.path().join("c"), t.path().join("w"));
        let c = open(&cdir, &work, None);
        let key = c.key("still").finish();
        let dest = work.join("seg_000.mp4");
        assert!(c.fetch(&key, &dest).is_none());
        let made = produce(&work, "made.mp4", 1000);
        assert!(c.store(&key, &made, 4.0));
        let c2 = open(&cdir, &work, None);
        let meta = c2.fetch(&key, &dest).expect("hit");
        assert_eq!(meta.secs, 4.0);
        assert_eq!(std::fs::read(&dest).unwrap().len(), 1000);
        assert_eq!(c2.stats().hits, 1);
        assert_eq!(c.stats().misses, 1);
        assert_eq!(c.stats().stored, 1);
    }

    #[test]
    fn partial_writes_are_never_reused() {
        let t = tempfile::tempdir().unwrap();
        let (cdir, work) = (t.path().join("c"), t.path().join("w"));
        let c = open(&cdir, &work, None);
        let key = c.key("still").finish();
        let dest = work.join("seg.mp4");
        let made = produce(&work, "made.mp4", 500);
        assert!(c.store(&key, &made, 4.0));
        // mp4 without sidecar (crash between renames).
        std::fs::remove_file(entry_path(&cdir, &key, "json")).unwrap();
        assert!(c.fetch(&key, &dest).is_none());
        // Sidecar present but file torn (size mismatch).
        let meta = SegmentMeta {
            secs: 4.0,
            size: 999,
        };
        std::fs::write(
            entry_path(&cdir, &key, "json"),
            serde_json::to_vec(&meta).unwrap(),
        )
        .unwrap();
        assert!(c.fetch(&key, &dest).is_none());
        // Empty file.
        std::fs::write(entry_path(&cdir, &key, "mp4"), b"").unwrap();
        let meta = SegmentMeta { size: 0, ..meta };
        std::fs::write(
            entry_path(&cdir, &key, "json"),
            serde_json::to_vec(&meta).unwrap(),
        )
        .unwrap();
        assert!(c.fetch(&key, &dest).is_none());
        // A store with a missing or empty source writes nothing.
        let key2 = c.key("other").finish();
        assert!(!c.store(&key2, &work.join("nope.mp4"), 1.0));
        assert!(!c.store(&key2, &produce(&work, "empty.mp4", 0), 1.0));
        assert!(!entry_path(&cdir, &key2, "mp4").exists());
        // No temp debris from the failures.
        let debris = std::fs::read_dir(&cdir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp-"))
            .count();
        assert_eq!(debris, 0);
    }

    #[test]
    fn materialized_segment_survives_cache_deletion() {
        let t = tempfile::tempdir().unwrap();
        let (cdir, work) = (t.path().join("c"), t.path().join("w"));
        let c = open(&cdir, &work, None);
        let key = c.key("still").finish();
        assert!(c.store(&key, &produce(&work, "made.mp4", 4096), 4.0));
        let dest = work.join("seg_001.mp4");
        assert!(c.fetch(&key, &dest).is_some());
        // A cleaner wipes the whole cache mid-export.
        std::fs::remove_dir_all(&cdir).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), vec![7u8; 4096]);
        #[cfg(unix)]
        {
            // Same volume: a real shared inode, so no extra space either.
            use std::os::unix::fs::MetadataExt;
            let c = open(&t.path().join("c2"), &work, None);
            let k = c.key("x").finish();
            c.store(&k, &dest, 1.0);
            let d2 = work.join("again.mp4");
            c.fetch(&k, &d2).unwrap();
            assert_eq!(
                std::fs::metadata(&d2).unwrap().ino(),
                std::fs::metadata(&dest).unwrap().ino()
            );
        }
    }

    #[test]
    fn eviction_is_lru_and_honors_the_cap() {
        let t = tempfile::tempdir().unwrap();
        let (cdir, work) = (t.path().join("c"), t.path().join("w"));
        let c = open(&cdir, &work, Some(2500));
        let keys: Vec<String> = (0..4).map(|i| c.key(&format!("k{i}")).finish()).collect();
        for (i, k) in keys.iter().enumerate() {
            assert!(c.store(k, &produce(&work, &format!("m{i}.mp4"), 1000), 1.0));
            // Oldest first: k0 oldest, k3 newest.
            set_age(&entry_path(&cdir, k, "mp4"), 1000 - 100 * i as u64);
        }
        // k0 was used most recently of all.
        touch(&entry_path(&cdir, &keys[0], "mp4"));
        let r = c.finish();
        assert_eq!(r.removed, 2, "{r:?}");
        let alive: Vec<bool> = keys
            .iter()
            .map(|k| read_valid(&cdir, k).is_some())
            .collect();
        assert_eq!(alive, [true, false, false, true]);
    }

    #[test]
    fn eviction_skips_entries_of_a_live_export() {
        let t = tempfile::tempdir().unwrap();
        let (cdir, work) = (t.path().join("c"), t.path().join("w"));
        // Entries from an earlier export, aged.
        let setup = open(&cdir, &work, None);
        let old = setup.key("old").finish();
        let used = setup.key("used").finish();
        assert!(setup.store(&old, &produce(&work, "a.mp4", 1000), 1.0));
        assert!(setup.store(&used, &produce(&work, "b.mp4", 1000), 1.0));
        setup.release_lease();
        set_age(&entry_path(&cdir, &old, "mp4"), 5000);
        set_age(&entry_path(&cdir, &used, "mp4"), 5000);
        // A live export starts, then hits `used`.
        let live = open(&cdir, &work, None);
        assert!(live.fetch(&used, &work.join("seg.mp4")).is_some());
        // Another process clears the cache: only the untouched entry goes.
        let r = clear(&cdir);
        assert_eq!((r.removed, r.protected), (1, 1), "{r:?}");
        assert!(read_valid(&cdir, &used).is_some());
        assert!(read_valid(&cdir, &old).is_none());
        // Our own eviction is held off by the same lease.
        assert_eq!(evict_to(&cdir, 0, now_ms()).removed, 0);
        // Once the export finishes the entry is evictable again.
        live.release_lease();
        assert_eq!(clear(&cdir).removed, 1);
        // And the export's materialized copy was unaffected throughout.
        assert!(work.join("seg.mp4").exists());
    }

    #[test]
    fn stale_leases_do_not_protect() {
        let t = tempfile::tempdir().unwrap();
        let (cdir, work) = (t.path().join("c"), t.path().join("w"));
        let c = open(&cdir, &work, None);
        let k = c.key("a").finish();
        assert!(c.store(&k, &produce(&work, "a.mp4", 1000), 1.0));
        c.release_lease();
        // A lease left by a crashed export (pid that cannot exist).
        let stale = Lease {
            pid: u32::MAX - 1,
            started_ms: now_ms() - 1000,
        };
        let lp = cdir
            .join("leases")
            .join(format!("{}-1-0.lease", u32::MAX - 1));
        std::fs::write(&lp, serde_json::to_vec(&stale).unwrap()).unwrap();
        assert_eq!(disk_stats(&cdir).live_leases, 0);
        assert!(lp.exists(), "stats must not sweep leases");
        let r = clear(&cdir);
        assert_eq!((r.removed, r.protected), (1, 0));
        assert!(!lp.exists(), "stale lease swept");
    }

    #[test]
    fn orphans_and_old_temp_files_are_swept() {
        let t = tempfile::tempdir().unwrap();
        let (cdir, work) = (t.path().join("c"), t.path().join("w"));
        let c = open(&cdir, &work, None);
        let tmp = cdir.join(".tmp-1-1");
        std::fs::write(&tmp, b"partial").unwrap();
        let k = c.key("o").finish();
        std::fs::create_dir_all(entry_path(&cdir, &k, "mp4").parent().unwrap()).unwrap();
        std::fs::write(entry_path(&cdir, &k, "mp4"), b"orphan").unwrap();
        // Fresh ones may belong to a live write: kept.
        c.finish();
        assert!(tmp.exists() && entry_path(&cdir, &k, "mp4").exists());
        set_age(&tmp, 7200);
        set_age(&entry_path(&cdir, &k, "mp4"), 7200);
        evict_to(&cdir, 0, now_ms());
        assert!(!tmp.exists() && !entry_path(&cdir, &k, "mp4").exists());
    }

    #[test]
    fn foreign_files_in_the_cache_dir_are_never_touched() {
        let t = tempfile::tempdir().unwrap();
        let (cdir, work) = (t.path().join("c"), t.path().join("w"));
        let c = open(&cdir, &work, None);
        let k = c.key("o").finish();
        let foreign = [
            cdir.join("holiday").join("original.mp4"),
            cdir.join(&k[..2]).join("original.mp4"),
            cdir.join("zz").join(format!("{}.mp4", "z".repeat(64))),
            cdir.join(if &k[..2] == "00" { "01" } else { "00" })
                .join(format!("{k}.mp4")),
            cdir.join(".tmp-notes.mp4"),
            cdir.join("leases").join("notes.txt"),
            cdir.join("leases").join("dead.lease"),
        ];
        for f in &foreign {
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, b"precious").unwrap();
            set_age(f, 7200);
        }
        // Stats never sweeps, even owned orphans.
        let orphan = entry_path(&cdir, &k, "mp4");
        std::fs::write(&orphan, b"orphan").unwrap();
        set_age(&orphan, 7200);
        assert_eq!(disk_stats(&cdir).entries, 0);
        assert!(orphan.exists());
        c.finish();
        clear(&cdir);
        assert!(!orphan.exists());
        for f in &foreign {
            assert!(f.exists(), "{} was deleted", f.display());
        }
    }

    #[test]
    fn dropping_the_cache_releases_its_lease() {
        let t = tempfile::tempdir().unwrap();
        let (cdir, work) = (t.path().join("c"), t.path().join("w"));
        let c = open(&cdir, &work, None);
        assert_eq!(disk_stats(&cdir).live_leases, 1);
        drop(c);
        assert_eq!(disk_stats(&cdir).live_leases, 0);
    }

    #[test]
    fn only_the_cache_own_names_are_recognized() {
        assert!(is_lease_name("12-1700000000000-0.lease"));
        for bad in [
            "dead.lease",
            "1-2.lease",
            "1--2.lease",
            "1-2-3.txt",
            "a-2-3.lease",
        ] {
            assert!(!is_lease_name(bad), "{bad}");
        }
        assert!(is_cache_tmp(".tmp-12-3"));
        for bad in [
            ".tmp-notes.mp4",
            ".tmp-1",
            ".tmp-1-2-3",
            ".tmp--2",
            "tmp-1-2",
        ] {
            assert!(!is_cache_tmp(bad), "{bad}");
        }
    }

    #[test]
    fn free_bytes_measures_a_cache_dir_that_does_not_exist_yet() {
        let t = tempfile::tempdir().unwrap();
        let missing = t.path().join("not").join("yet");
        assert_eq!(
            free_bytes(&missing).is_some(),
            free_bytes(t.path()).is_some()
        );
    }

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("500MB").unwrap(), 500 << 20);
        assert_eq!(parse_size("20gb").unwrap(), 20 << 30);
        assert_eq!(parse_size("1.5G").unwrap(), 3 << 29);
        assert!(parse_size("fast").is_err());
        assert!(parse_size("5 parsecs").is_err());
    }
}
