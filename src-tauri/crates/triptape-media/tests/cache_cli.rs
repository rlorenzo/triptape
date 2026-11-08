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

fn entry(dir: &std::path::Path, key: &str, bytes: usize) {
    let sub = dir.join(&key[..2]);
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join(format!("{key}.mp4")), vec![1u8; bytes]).unwrap();
    std::fs::write(
        sub.join(format!("{key}.json")),
        format!(r#"{{"secs":4.0,"kind":"still","size":{bytes}}}"#),
    )
    .unwrap();
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
    entry(dir, "cc33", 512);
    // A lease owned by this very (live) test process, started before the
    // entry was last used.
    std::fs::create_dir_all(dir.join("leases")).unwrap();
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .saturating_sub(60_000);
    std::fs::write(
        dir.join("leases/live.lease"),
        format!(r#"{{"pid":{},"started_ms":{started}}}"#, std::process::id()),
    )
    .unwrap();

    let (ok, out) = triptape(&["cache", "clear", "--segment-cache", dir.to_str().unwrap()]);
    assert!(ok);
    assert!(out.contains("removed 0 entries"), "{out}");
    assert!(out.contains("kept 1 entries"), "{out}");
    assert!(dir.join("cc/cc33.mp4").exists());
}

#[test]
fn unknown_cache_command_fails() {
    let (ok, _) = triptape(&["cache", "polish"]);
    assert!(!ok);
}
