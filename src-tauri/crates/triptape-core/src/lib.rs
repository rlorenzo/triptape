//! triptape-core: scan, hash, EXIF, proxies, SQLite, job types. Pure Rust,
//! testable headless without the Tauri UI.

pub mod clip_window;
pub mod cluster;
pub mod db;
pub mod geo;
pub mod hash;
pub mod jobs;
pub mod livephoto;
pub mod metadata;
pub mod parallel;
pub mod plan;
pub mod privacy;
pub mod proxies;
pub mod scan;
pub mod score;
pub mod types;
