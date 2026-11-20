//! `tauri.conf.json` bundles FFmpeg, ffprobe and Typst as `bundle.externalBin`
//! sidecars, which `scripts/fetch-sidecars` downloads and verifies into
//! `binaries/`. tauri-build copies them next to the compiled binary and stops
//! the build when one is missing, which would break every checkout without
//! them (CI's Rust jobs, a fresh clone, `cargo clippy`).
//!
//! So when any sidecar for this target is missing, this build alone drops
//! `externalBin` (through tauri-build's `TAURI_CONFIG` merge patch) and warns.
//! The app then falls back to the tools on `PATH`, with a visible note
//! (`triptape_core::process::resolve_tool`). Release bundling is unaffected:
//! `tauri build` reads `externalBin` from `tauri.conf.json` itself and fails
//! without the sidecars, so no release ships without them.

use std::path::Path;

const SIDECARS: [&str; 3] = ["ffmpeg", "ffprobe", "typst"];

fn main() {
    // The folder is tracked (its README), so it exists in every checkout and
    // its contents changing (a fetch or a delete) reruns this script.
    println!("cargo:rerun-if-changed=binaries");
    let target = std::env::var("TARGET").expect("cargo sets TARGET for build scripts");
    let exe = if target.contains("windows") {
        ".exe"
    } else {
        ""
    };
    let missing: Vec<String> = SIDECARS
        .iter()
        .map(|name| format!("binaries/{name}-{target}{exe}"))
        .filter(|p| !Path::new(p).is_file())
        .collect();
    // An explicit TAURI_CONFIG (the Tauri CLI's `--config`) is left alone: a
    // missing sidecar then fails the build, as a release must.
    if !missing.is_empty() && std::env::var_os("TAURI_CONFIG").is_none() {
        println!(
            "cargo:warning=sidecars missing ({}): this build uses ffmpeg, ffprobe and typst from PATH. Run scripts/fetch-sidecars to bundle the pinned ones.",
            missing.join(", ")
        );
        std::env::set_var("TAURI_CONFIG", r#"{"bundle":{"externalBin":null}}"#);
    }
    tauri_build::build()
}
