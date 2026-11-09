//! `triptape cache stats|clear` against a scratch cache directory.

use std::process::Command;

fn triptape(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_triptape"))
        .args(args)
        .output()
        .expect("run triptape");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// A cache entry for a real-shaped key: `prefix` padded to 64 hex digits.
fn entry(dir: &std::path::Path, prefix: &str, bytes: usize) -> String {
    let key = format!("{prefix:0<64}");
    let sub = dir.join(&key[..2]);
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join(format!("{key}.mp4")), vec![1u8; bytes]).unwrap();
    std::fs::write(
        sub.join(format!("{key}.json")),
        format!(r#"{{"secs":4.0,"size":{bytes}}}"#),
    )
    .unwrap();
    key
}

#[test]
fn stats_then_clear_empties_the_cache() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().to_str().unwrap();
    entry(t.path(), "aa11", 2048);
    entry(t.path(), "bb22", 1024);

    let (ok, out) = triptape(&["cache", "stats", "--segment-cache", dir]);
    assert!(ok);
    assert!(out.contains("entries: 2"), "{out}");

    let (ok, out) = triptape(&["cache", "clear", "--segment-cache", dir]);
    assert!(ok);
    assert!(out.contains("removed 2 entries"), "{out}");

    let (_, out) = triptape(&["cache", "stats", "--segment-cache", dir]);
    assert!(out.contains("entries: 0"), "{out}");
}

#[test]
fn clear_keeps_entries_a_running_export_uses() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path();
    let key = entry(dir, "cc33", 512);
    // A lease owned by this very (live) test process, started before the
    // entry was last used.
    std::fs::create_dir_all(dir.join("leases")).unwrap();
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .saturating_sub(60_000);
    std::fs::write(
        dir.join(format!("leases/{}-{started}-0.lease", std::process::id())),
        format!(r#"{{"pid":{},"started_ms":{started}}}"#, std::process::id()),
    )
    .unwrap();

    let (ok, out) = triptape(&["cache", "clear", "--segment-cache", dir.to_str().unwrap()]);
    assert!(ok);
    assert!(out.contains("removed 0 entries"), "{out}");
    assert!(out.contains("kept 1 entries"), "{out}");
    assert!(dir.join("cc").join(format!("{key}.mp4")).exists());
}

#[test]
fn unknown_cache_command_fails() {
    // Always name a scratch dir: no test may touch the user's real cache.
    let t = tempfile::tempdir().unwrap();
    let (ok, _) = triptape(&[
        "cache",
        "polish",
        "--segment-cache",
        t.path().to_str().unwrap(),
    ]);
    assert!(!ok);
}

#[test]
fn stats_reports_the_given_cap_and_rejects_a_bad_one() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().to_str().unwrap();
    let (ok, out) = triptape(&[
        "cache",
        "stats",
        "--segment-cache",
        dir,
        "--segment-cache-max",
        "1MB",
    ]);
    assert!(ok);
    assert!(out.contains("cap: 1.0 MB"), "{out}");
    let (ok, _) = triptape(&[
        "cache",
        "stats",
        "--segment-cache",
        dir,
        "--segment-cache-max",
        "lots",
    ]);
    assert!(!ok);
}

#[test]
fn stats_shows_limit_floor_free_space_and_headroom() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().to_str().unwrap();
    entry(t.path(), "dd44", 2048);
    let (ok, out) = triptape(&[
        "cache",
        "stats",
        "--segment-cache",
        dir,
        "--segment-cache-max",
        "4GB",
        "--segment-cache-min-free",
        "1GB",
    ]);
    assert!(ok);
    assert!(out.contains("limit: 4.0 GB"), "{out}");
    assert!(out.contains("free-space floor: 1.0 GB"), "{out}");
    assert!(out.contains("disk free:"), "{out}");
    assert!(out.contains("headroom:"), "{out}");
}
