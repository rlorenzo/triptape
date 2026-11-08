//! Shared domain types: Asset, Cluster, Plan, Job.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// What kind of media an asset is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    Photo,
    Video,
    LivePhoto,
}

impl AssetKind {
    /// Label the webview receives for this kind.
    pub fn as_str(self) -> &'static str {
        match self {
            AssetKind::Photo => "photo",
            AssetKind::Video => "video",
            AssetKind::LivePhoto => "live-photo",
        }
    }
}

/// Which rendition of an asset a plan entry (or renderer) should use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rendition {
    /// Full-resolution still (always used for print).
    Still,
    /// Live Photo / clip motion with an optional scored trim window (seconds).
    Motion { trim_start: f64, trim_end: f64 },
    /// Trimmed video clip (seconds).
    Clip { trim_start: f64, trim_end: f64 },
}

/// A single ingested media asset (one Live Photo pair = one asset).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Asset {
    pub id: String,
    pub path: PathBuf,
    /// Second path for Live Photos (the MOV motion file).
    pub companion_path: Option<PathBuf>,
    pub kind: AssetKind,
    pub size_bytes: u64,
    /// Source file mtime (unix seconds).
    pub mtime: i64,
    pub content_hash: Option<String>,
    /// 64-bit perceptual hash for near-dupe grouping.
    pub phash: Option<u64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_secs: Option<f64>,
    /// Capture time (unix seconds) after normalization, if known.
    pub taken_at: Option<i64>,
    /// UTC offset at capture in seconds (EXIF `OffsetTimeOriginal`, or
    /// filled from a dated neighbor). Display times/days use local time.
    pub tz_offset_secs: Option<i32>,
    /// True when `taken_at` fell back to file mtime.
    pub time_guessed: bool,
    pub gps: Option<(f64, f64)>,
    /// EXIF orientation-derived rotation in degrees (0/90/180/270).
    pub rotation_deg: u16,
    /// Live Photo pairing confidence, when applicable.
    pub pairing_confidence: Option<PairingConfidence>,
    /// Caption embedded in the file by the user (EXIF/XMP), if any.
    /// Wins over auto-generated place captions in plans.
    pub user_caption: Option<String>,
    /// How many people the photo library tagged in this asset (a count
    /// only, never the names: they are personal data and stay out of logs,
    /// plans and exports). `None` when untagged or unknown.
    pub people_count: Option<u32>,
    /// Local quality/story score, filled by background scoring after ingest.
    /// `None` until scored (or when scoring was skipped).
    pub score: Option<Score>,
}

impl Asset {
    /// Width / height as displayed (EXIF rotation applied); `None` when
    /// the size is unknown.
    pub fn display_aspect(&self) -> Option<f64> {
        let (w, h) = (f64::from(self.width?), f64::from(self.height?));
        if w <= 0.0 || h <= 0.0 {
            return None;
        }
        Some(if matches!(self.rotation_deg, 90 | 270) {
            h / w
        } else {
            w / h
        })
    }

    pub fn new(id: impl Into<String>, path: PathBuf, kind: AssetKind) -> Self {
        Self {
            id: id.into(),
            path,
            companion_path: None,
            kind,
            size_bytes: 0,
            mtime: 0,
            content_hash: None,
            phash: None,
            width: None,
            height: None,
            duration_secs: None,
            taken_at: None,
            tz_offset_secs: None,
            time_guessed: false,
            gps: None,
            rotation_deg: 0,
            pairing_confidence: None,
            user_caption: None,
            people_count: None,
            score: None,
        }
    }
}

/// How a Live Photo still+motion pair was matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingConfidence {
    /// Tier 1: shared content-identifier UUID in both files' metadata.
    ContentId,
    /// Tier 2: same stem + short MOV fallback.
    BasenameOnly,
}

/// A time/GPS cluster (a day/place group).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cluster {
    pub id: String,
    pub asset_ids: Vec<String>,
    pub start_time: Option<i64>,
    pub end_time: Option<i64>,
    pub centroid_gps: Option<(f64, f64)>,
    pub label: String,
}

/// One chapter of the story plan.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Chapter {
    pub title: String,
    pub asset_ids: Vec<String>,
    /// Nearest populated place to the chapter's GPS medoid
    /// (`City, Country`), when the gazetteer can name it.
    #[serde(default)]
    pub place: Option<String>,
    /// 1-based day of the trip (local dates) this chapter starts on.
    #[serde(default)]
    pub day_index: Option<u32>,
    /// Local date label of the chapter's first asset, e.g. `Wed, Oct 8`.
    #[serde(default)]
    pub date_label: Option<String>,
}

/// Music pick with a human-readable rationale ("vibe fit", never fake emotion).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MusicPick {
    pub track: String,
    pub rationale: String,
}

/// One entry in the hero/reel shot lists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanEntry {
    pub asset_id: String,
    pub content_hash: Option<String>,
    pub size_bytes: u64,
    pub mtime: i64,
    pub rendition: Rendition,
}

/// Versioned, reproducible AI plan consumed by all three renderers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub plan_version: u32,
    pub schema_version: u32,
    pub hero: Vec<PlanEntry>,
    pub reel_selects: Vec<PlanEntry>,
    pub chapters: Vec<Chapter>,
    /// asset_id -> caption draft.
    pub captions: std::collections::HashMap<String, String>,
    /// Route polyline (lat, lon) with day markers.
    pub map_polyline: Vec<(f64, f64)>,
    pub music: MusicPick,
}

impl Plan {
    pub const SCHEMA_VERSION: u32 = 1;

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

/// Per-signal quality score with human-readable reasons.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Score {
    /// 0..1 sharpness estimate.
    pub sharpness: f32,
    /// 0..1 exposure quality (1 = well exposed).
    pub exposure: f32,
    /// 0..1 blur estimate (1 = very blurry).
    pub blur: f32,
    /// Combined 0..1 story score: compressed technical quality plus
    /// [`Score::bonus`].
    pub total: f32,
    /// Story-signal part of `total` (people, caption, video, place),
    /// unclamped: `total` saturates at 1 but this keeps the full bonus.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub bonus: f32,
    /// Squeezed technical quality, stored apart from the clamped `total`
    /// so the plan can rank it per chapter even when `total` saturates.
    /// `None` on older scores; see [`Score::tech_quality`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<f32>,
    /// Whether this asset is a story highlight.
    pub highlight: bool,
    /// One short string per contributing signal.
    pub reasons: Vec<String>,
    /// Best `(start, end)` window in seconds for a long video clip, picked
    /// at ingest. `None` keeps the start-of-clip trim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clip_window: Option<(f64, f64)>,
}

impl Score {
    /// Technical quality without story bonuses: the stored `quality`, or
    /// `total - bonus` for scores saved before it existed.
    pub fn tech_quality(&self) -> f32 {
        self.quality.unwrap_or(self.total - self.bonus)
    }
}

fn is_zero(v: &f32) -> bool {
    *v == 0.0
}

/// Durable job state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Finalizing,
    Done,
    Failed,
    Cancelled,
    /// Set only by startup recovery, never by live transitions.
    Interrupted,
}

/// A durable job row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub project_id: String,
    pub kind: String,
    pub state: JobState,
    pub progress: f32,
    pub current_step: Option<String>,
    pub temp_dir: Option<String>,
    pub error: Option<String>,
}
