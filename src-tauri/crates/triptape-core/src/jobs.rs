//! Durable jobs: state machine, checkpoints, preflight, cancel.
//!
//! States: `queued → running → (finalizing) → done | failed | cancelled`,
//! plus `interrupted` set only by startup recovery. DB rows live in
//! [`crate::db`]; this module owns transition rules and the filesystem
//! helpers (`.partial` checkpoint + atomic rename finalization, 1.5×
//! free-space precheck, cooperative cancel flag).

use crate::db::{job_state_to_str, Db};
use crate::types::JobState;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// True when `from → to` is a legal live transition.
///
/// `Interrupted` is never a live target — only startup recovery sets it —
/// and no state leaves a terminal state.
pub fn can_transition(from: JobState, to: JobState) -> bool {
    use JobState::*;
    matches!(
        (from, to),
        (Queued, Running | Cancelled)
            | (Running, Finalizing | Done | Failed | Cancelled)
            | (Finalizing, Done | Failed | Cancelled)
    )
}

/// Apply a live transition to the job row, rejecting illegal moves.
pub fn transition(
    db: &Db,
    id: &str,
    to: JobState,
    progress: f32,
    step: Option<&str>,
    error: Option<&str>,
) -> Result<(), String> {
    if to == JobState::Interrupted {
        return Err("interrupted is set only by startup recovery".to_string());
    }
    let cur = db.get_job(id)?.ok_or_else(|| format!("unknown job {id}"))?;
    // Same state is an idempotent progress/step refresh.
    if cur.state != to && !can_transition(cur.state, to) {
        return Err(format!(
            "illegal transition {} -> {}",
            job_state_to_str(cur.state),
            job_state_to_str(to)
        ));
    }
    db.update_job(id, to, progress, step, error)
}

/// Startup recovery: every `running`/`finalizing` row becomes `interrupted`.
/// Returns the ids marked.
pub fn mark_interrupted(db: &Db) -> Result<Vec<String>, String> {
    let stuck = db.jobs_in_state(&[JobState::Running, JobState::Finalizing])?;
    for j in &stuck {
        db.update_job(
            &j.id,
            JobState::Interrupted,
            j.progress,
            j.current_step.as_deref(),
            None,
        )?;
    }
    Ok(stuck.into_iter().map(|j| j.id).collect())
}

/// Write a checkpoint payload to `name.partial` inside `temp_dir`.
///
/// Bytes land in a unique `name.<pid>.<n>.tmp` sibling, are fsynced, then
/// renamed onto `name.partial`, so a crash mid-write never leaves a
/// truncated `.partial` that [`finalize_checkpoint`] would promote.
pub fn checkpoint_write(temp_dir: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    use std::io::Write;
    use std::sync::atomic::AtomicU64;
    static SEQ: AtomicU64 = AtomicU64::new(0);

    std::fs::create_dir_all(temp_dir).map_err(|e| e.to_string())?;
    let partial = temp_dir.join(format!("{name}.partial"));
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = temp_dir.join(format!("{name}.{}.{seq}.tmp", std::process::id()));
    let written = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &partial)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    Ok(partial)
}

/// Atomically promote `name.partial` to `name` (resume skips finished steps
/// whose output exists; final outputs verify before this rename).
pub fn finalize_checkpoint(temp_dir: &Path, name: &str) -> Result<PathBuf, String> {
    let partial = temp_dir.join(format!("{name}.partial"));
    let done = temp_dir.join(name);
    if !partial.exists() {
        return Err(format!("missing checkpoint {}", partial.display()));
    }
    std::fs::rename(&partial, &done).map_err(|e| e.to_string())?;
    Ok(done)
}

/// Required free bytes for an export estimate (estimate × 1.5 headroom).
pub fn required_free_bytes(estimate_bytes: u64) -> u64 {
    estimate_bytes.saturating_mul(3).div_ceil(2)
}

/// Free bytes available on the filesystem containing `path`.
///
/// `None` when unknowable, so the caller can warn instead of block.
pub fn free_bytes(path: &Path) -> Option<u64> {
    disk_space(path).map(|d| d.free)
}

/// Free and total bytes of a filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskSpace {
    /// Bytes available to this user.
    pub free: u64,
    /// Size of the volume; `None` when the platform will not say.
    pub total: Option<u64>,
}

/// Free and total bytes of the filesystem containing `path`.
///
/// Uses `df -P -k` on unix and `GetDiskFreeSpaceExW` on Windows. `None` when
/// unknowable.
pub fn disk_space(path: &Path) -> Option<DiskSpace> {
    #[cfg(unix)]
    {
        let out = std::process::Command::new("df")
            .args(["-P", "-k"])
            .arg(path)
            .output()
            .ok()
            .filter(|o| o.status.success())?;
        let text = String::from_utf8_lossy(&out.stdout);
        let cols: Vec<&str> = text.lines().nth(1)?.split_whitespace().collect();
        let kb = |i: usize| cols.get(i)?.parse::<u64>().ok().map(|k| k * 1024);
        Some(DiskSpace {
            free: kb(3)?,
            total: kb(1),
        })
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        extern "system" {
            fn GetDiskFreeSpaceExW(
                directory: *const u16,
                free_to_caller: *mut u64,
                total: *mut u64,
                total_free: *mut u64,
            ) -> i32;
        }
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let (mut free, mut total) = (0u64, 0u64);
        // SAFETY: `wide` is NUL-terminated and outlives the call, `free` and
        // `total` are valid out pointers, and the API accepts null for the last.
        let ok = unsafe {
            GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, std::ptr::null_mut())
        };
        (ok != 0).then_some(DiskSpace {
            free,
            total: Some(total),
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        None
    }
}

/// Fail when free space is below estimate × 1.5. Unknown free space is a
/// pass (caller logs a warning) so exports never block on `df` failures.
pub fn free_space_precheck(dir: &Path, estimate_bytes: u64) -> Result<(), String> {
    let need = required_free_bytes(estimate_bytes);
    match free_bytes(dir) {
        Some(have) if have < need => Err(format!(
            "not enough free space in {}: need {need} bytes (1.5x estimate), have {have}",
            dir.display()
        )),
        _ => Ok(()),
    }
}

/// Cooperative cancel flag shared with a running job task.
#[derive(Debug, Clone, Default)]
pub struct CancelFlag {
    inner: Arc<AtomicBool>,
}

impl CancelFlag {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.inner.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.inner.load(Ordering::SeqCst)
    }
    /// Err when cancelled (job tasks call this between steps).
    pub fn check(&self) -> Result<(), String> {
        if self.is_cancelled() {
            Err("cancelled".to_string())
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Db {
        Db::open_in_memory().unwrap()
    }

    #[test]
    fn state_machine_rules() {
        assert!(can_transition(JobState::Queued, JobState::Running));
        assert!(can_transition(JobState::Running, JobState::Finalizing));
        assert!(can_transition(JobState::Finalizing, JobState::Done));
        assert!(can_transition(JobState::Running, JobState::Cancelled));
        assert!(!can_transition(JobState::Queued, JobState::Done));
        assert!(!can_transition(JobState::Done, JobState::Running));
        assert!(!can_transition(JobState::Failed, JobState::Queued));
        assert!(!can_transition(JobState::Running, JobState::Interrupted));
    }

    #[test]
    fn transitions_enforced_and_recovery() {
        let db = mem();
        let p = db.create_project("t", "/x").unwrap();
        let j = db.insert_job(&p.id, "render-video", None).unwrap();
        assert!(transition(&db, &j.id, JobState::Done, 1.0, None, None).is_err());
        transition(&db, &j.id, JobState::Running, 0.1, Some("norm"), None).unwrap();
        assert!(transition(&db, &j.id, JobState::Interrupted, 0.1, None, None).is_err());
        let marked = mark_interrupted(&db).unwrap();
        assert_eq!(marked, vec![j.id.clone()]);
        let back = db.get_job(&j.id).unwrap().unwrap();
        assert_eq!(back.state, JobState::Interrupted);
        // Terminal: no live transition out of interrupted.
        assert!(transition(&db, &j.id, JobState::Running, 0.2, None, None).is_err());
    }

    #[test]
    fn checkpoint_partial_then_rename() {
        let dir = tempfile::tempdir().unwrap();
        let partial = checkpoint_write(dir.path(), "seg001.mp4", b"bytes").unwrap();
        assert!(partial.extension().unwrap() == "partial");
        assert!(!dir.path().join("seg001.mp4").exists());
        let done = finalize_checkpoint(dir.path(), "seg001.mp4").unwrap();
        assert_eq!(std::fs::read(done).unwrap(), b"bytes");
        assert!(finalize_checkpoint(dir.path(), "missing.mp4").is_err());
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn disk_space_reports_free_within_total() {
        let t = tempfile::tempdir().unwrap();
        let d = disk_space(t.path()).expect("disk space of a temp dir");
        let total = d.total.expect("total size");
        assert!(total > 0 && d.free <= total, "{d:?}");
    }

    #[test]
    fn free_space_math() {
        assert_eq!(required_free_bytes(100), 150);
        assert_eq!(required_free_bytes(0), 0);
        let dir = tempfile::tempdir().unwrap();
        // Tiny estimate always passes; huge estimate fails on real disks.
        assert!(free_space_precheck(dir.path(), 1).is_ok());
        assert!(free_space_precheck(dir.path(), u64::MAX).is_err());
    }

    #[test]
    fn cancel_flag() {
        let c = CancelFlag::new();
        assert!(c.check().is_ok());
        c.cancel();
        assert!(c.is_cancelled());
        assert_eq!(c.check().unwrap_err(), "cancelled");
    }

    #[test]
    fn same_state_refreshes_progress() {
        let db = mem();
        let p = db.create_project("t", "/x").unwrap();
        let j = db.insert_job(&p.id, "render-video", None).unwrap();
        transition(&db, &j.id, JobState::Running, 0.1, Some("a"), None).unwrap();
        transition(&db, &j.id, JobState::Running, 0.6, Some("b"), None).unwrap();
        let back = db.get_job(&j.id).unwrap().unwrap();
        assert_eq!(back.progress, 0.6);
        assert_eq!(back.current_step.as_deref(), Some("b"));
    }
}
