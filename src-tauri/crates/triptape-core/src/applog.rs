//! The local diagnostic log (PLAN.md Sec 14): a size-capped rolling file in
//! the app's log dir, written only through [`log`], which redacts every
//! line with [`crate::privacy::Redactor`] before it touches the disk.
//!
//! Lines hold job ids, stages, FFmpeg exit codes and error text. Paths and
//! file names come out hashed, coordinates as `<gps>`, and registered
//! captions as `<text>`. Nothing is sent anywhere: the owner copies a
//! report from Settings when they choose to.

use crate::privacy::Redactor;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

/// The current file; older ones are `triptape.1.log` (newest) and up.
pub const LOG_FILE: &str = "triptape.log";
/// One file's size before it rolls over.
pub const MAX_FILE_BYTES: u64 = 4 << 20;
/// Files kept, the current one included: 20 MB at most in all.
pub const MAX_FILES: usize = 5;
/// Rolled files older than this are deleted.
pub const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

/// A rolling set of log files in one folder.
#[derive(Debug, Clone)]
pub struct RollingLog {
    dir: PathBuf,
    max_file_bytes: u64,
    max_files: usize,
    max_age: Duration,
}

impl RollingLog {
    pub fn new(dir: &Path) -> Self {
        Self::with_limits(dir, MAX_FILE_BYTES, MAX_FILES, MAX_AGE)
    }

    pub fn with_limits(
        dir: &Path,
        max_file_bytes: u64,
        max_files: usize,
        max_age: Duration,
    ) -> Self {
        Self {
            dir: dir.to_path_buf(),
            max_file_bytes,
            max_files: max_files.max(1),
            max_age,
        }
    }

    fn file(&self, n: usize) -> PathBuf {
        if n == 0 {
            self.dir.join(LOG_FILE)
        } else {
            self.dir.join(format!("triptape.{n}.log"))
        }
    }

    /// Append one line, rolling over first when it would not fit.
    pub fn append(&self, line: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let current = self.file(0);
        let size = std::fs::metadata(&current).map_or(0, |m| m.len());
        if size > 0 && size + line.len() as u64 + 1 > self.max_file_bytes {
            self.roll();
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&current)?;
        writeln!(f, "{line}")
    }

    /// Shift every file up one, dropping the oldest and any past the age.
    fn roll(&self) {
        let _ = std::fs::remove_file(self.file(self.max_files - 1));
        for n in (0..self.max_files - 1).rev() {
            let _ = std::fs::rename(self.file(n), self.file(n + 1));
        }
        self.prune_old();
    }

    /// Delete rolled files last written more than `max_age` ago.
    pub fn prune_old(&self) {
        let now = SystemTime::now();
        for n in 1..self.max_files {
            let path = self.file(n);
            let old = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .is_ok_and(|t| now.duration_since(t).unwrap_or_default() > self.max_age);
            if old {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    /// The last `n` lines, oldest first, across the current and rolled files.
    pub fn tail(&self, n: usize) -> Vec<String> {
        let mut lines: Vec<String> = vec![];
        for k in 0..self.max_files {
            if lines.len() >= n {
                break;
            }
            let Ok(text) = std::fs::read_to_string(self.file(k)) else {
                continue;
            };
            let mut older: Vec<String> = text.lines().map(str::to_string).collect();
            older.append(&mut lines);
            lines = older;
        }
        let skip = lines.len().saturating_sub(n);
        lines.split_off(skip)
    }

    /// Bytes on disk across every file of this log.
    pub fn total_bytes(&self) -> u64 {
        (0..self.max_files)
            .filter_map(|n| std::fs::metadata(self.file(n)).ok())
            .map(|m| m.len())
            .sum()
    }
}

struct Logger {
    log: RollingLog,
    redactor: Mutex<Redactor>,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

/// Start logging into `dir`. Later calls are ignored: there is one log.
pub fn init(dir: &Path, redactor: Redactor) {
    let log = RollingLog::new(dir);
    log.prune_old();
    let _ = LOGGER.set(Logger {
        log,
        redactor: Mutex::new(redactor),
    });
}

/// Write one line: `<UTC time> <LEVEL> <redacted message>`. Does nothing
/// before [`init`] (the CLI never starts the log).
pub fn log(level: &str, message: &str) {
    let Some(logger) = LOGGER.get() else {
        return;
    };
    let text = logger
        .redactor
        .lock()
        .map(|r| r.redact(message))
        .unwrap_or_else(|_| "<unredactable line dropped>".to_string());
    let line = format!("{} {} {text}", now(), level.to_uppercase());
    if logger.log.append(&line).is_err() {
        eprintln!("{line}");
    }
}

/// The time now, in UTC, as the log writes it.
pub fn now() -> String {
    jiff::Timestamp::now()
        .round(jiff::Unit::Second)
        .map_or_else(|_| jiff::Timestamp::now().to_string(), |t| t.to_string())
}

pub fn info(message: &str) {
    log("info", message);
}

pub fn warn(message: &str) {
    log("warn", message);
}

pub fn error(message: &str) {
    log("error", message);
}

/// Never let `texts` (captions, place names typed by the owner) reach the
/// log, from now on.
pub fn add_secrets<'a>(texts: impl IntoIterator<Item = &'a str>) {
    if let Some(logger) = LOGGER.get() {
        if let Ok(mut r) = logger.redactor.lock() {
            for t in texts {
                r.add_secret(t);
            }
        }
    }
}

/// Redact `text` the way the log does (for diagnostic reports).
pub fn redact(text: &str) -> String {
    LOGGER
        .get()
        .and_then(|l| l.redactor.lock().ok().map(|r| r.redact(text)))
        .unwrap_or_else(|| Redactor::default().redact(text))
}

/// The last `n` log lines (already redacted when written).
pub fn tail(n: usize) -> Vec<String> {
    LOGGER.get().map_or_else(Vec::new, |l| l.log.tail(n))
}

/// Bytes the log takes on disk.
pub fn total_bytes() -> u64 {
    LOGGER.get().map_or(0, |l| l.log.total_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolls_over_at_the_size_cap_and_keeps_a_bounded_set() {
        let t = tempfile::tempdir().unwrap();
        let log = RollingLog::with_limits(t.path(), 100, 3, MAX_AGE);
        for n in 0..40 {
            log.append(&format!("line {n:02} ........")).unwrap();
        }
        let files: Vec<_> = std::fs::read_dir(t.path()).unwrap().flatten().collect();
        assert_eq!(files.len(), 3, "current plus two rolled files");
        assert!(log.total_bytes() <= 3 * 100);
        let tail = log.tail(5);
        assert_eq!(tail.len(), 5);
        assert_eq!(tail.last().unwrap(), "line 39 ........");
        assert_eq!(tail[0], "line 35 ........");
        // The tail crosses into the rolled file when the current one is short.
        assert_eq!(log.tail(9).first().unwrap(), "line 31 ........");
    }

    #[test]
    fn prunes_rolled_files_past_the_age_limit() {
        let t = tempfile::tempdir().unwrap();
        let log = RollingLog::with_limits(t.path(), 1 << 20, 3, Duration::ZERO);
        std::fs::write(t.path().join("triptape.1.log"), "old\n").unwrap();
        log.append("new").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        log.prune_old();
        assert!(!t.path().join("triptape.1.log").exists());
        assert!(t.path().join(LOG_FILE).exists(), "the current file stays");
    }
}
