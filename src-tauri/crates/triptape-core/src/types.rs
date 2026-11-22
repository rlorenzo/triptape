//! Shared domain types: Asset, Cluster, Plan, Job.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
    /// The only source of plan captions besides Story-editor overrides.
    pub user_caption: Option<String>,
    /// How many people the photo library tagged in this asset (a count
    /// only, never the names: they are personal data and stay out of logs,
    /// plans and exports). `None` when untagged or unknown.
    pub people_count: Option<u32>,
    /// Signature of WHO is tagged (see `metadata::PeopleTags::signature`):
    /// equal for the same people, different for different people at the
    /// same count, salted with the project id. Never the names themselves.
    /// `None` when nobody is tagged.
    #[serde(default)]
    pub people_sig: Option<u64>,
    /// The photo library's own aesthetic score (Apple Photos
    /// `score.overall`, roughly -1..1), stored as-is. `None` when unknown.
    #[serde(default)]
    pub library_score: Option<f32>,
    /// Local quality/story score, filled by background scoring after ingest.
    /// `None` until scored (or when scoring was skipped).
    pub score: Option<Score>,
    /// Image embedding of a still (or Live Photo still) from the optional
    /// local model: an extra "same scene" signal next to `phash`. `None`
    /// when the model is not installed or the pixels could not be read.
    /// Analysis data: never serialized (not in plans, exports or the
    /// webview), only stored in the local database.
    #[serde(skip)]
    pub embedding: Option<crate::embedding::Embedding>,
    /// Face and smile signals from the optional local models (thumbnail
    /// analysis). `None` until analysed with the models installed. Counts
    /// and scores only, never boxes; never serialized.
    #[serde(skip)]
    pub faces: Option<FaceSignals>,
}

/// A named person's region in a photo (never the name itself), from the owner's photo library
/// (Apple Photos' face tags): who matters in the frame. Normalised to the
/// upright photo, top-left origin. `body` marks a region known only from
/// the person's body (no face found), whose top stands in for the head.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct KnownFace {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    #[serde(default)]
    pub body: bool,
}

/// What the optional face models found in one photo: only what scoring
/// needs (no boxes, crops or identities).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FaceSignals {
    /// Plausible faces detected. Not a people count: posters, statues and
    /// dolls are faces to a detector too.
    pub count: u32,
    /// Largest face's side as a fraction of the picture's shorter side.
    pub largest: f32,
    /// Best size-weighted smile (0..1) among faces big enough to read;
    /// `None` when none was.
    pub smile: Option<f32>,
}

impl Asset {
    /// Whether this is a Live Photo with its companion video, so it can
    /// play its motion.
    pub fn has_live_motion(&self) -> bool {
        self.kind == AssetKind::LivePhoto && self.companion_path.is_some()
    }

    /// Width / height as displayed (EXIF rotation applied); `None` when
    /// the size is unknown.
    pub fn display_aspect(&self) -> Option<f64> {
        let (w, h) = (f64::from(self.width?), f64::from(self.height?));
        if w <= 0.0 || h <= 0.0 {
            return None;
        }
        Some(if self.rotation_swaps_axes() {
            h / w
        } else {
            w / h
        })
    }

    /// Whether the EXIF rotation (90/270) displays the stored width as the
    /// height: a portrait shot stored sideways.
    pub fn rotation_swaps_axes(&self) -> bool {
        matches!(self.rotation_deg, 90 | 270)
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
            people_sig: None,
            library_score: None,
            score: None,
            embedding: None,
            faces: None,
        }
    }
}

/// What background scoring derives from one asset's pixels, persisted
/// together so a rescan of an unchanged file reuses all of it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Analysis {
    pub score: Score,
    /// Perceptual hash of the pixels scored; `None` when nothing decoded.
    pub phash: Option<u64>,
    /// See [`Asset::embedding`]; `None` without the model.
    pub embedding: Option<crate::embedding::Embedding>,
    /// See [`Asset::faces`]; `None` without the face models.
    pub faces: Option<FaceSignals>,
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
    /// Where the film's own shots of this chapter were taken, when that
    /// differs from [`Self::place`] (which names the whole chapter, as the
    /// book shows it): the film shows Dotonbori, not the castle it skipped.
    /// Set by [`crate::itinerary::name_film_chapters`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub film_place: Option<String>,
    /// Where the film moves on to another place within the chapter:
    /// `(asset_id, place)` for the first film shot at each new place, in
    /// film order (lunch in town, then the lava cliffs; the Flower Dome,
    /// then the Cloud Forest). Each gets its own lower third. Set by
    /// [`crate::itinerary::name_film_chapters`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub film_place_changes: Vec<(String, String)>,
}

impl Chapter {
    /// The place the film names for this chapter: [`Self::film_place`]
    /// when set, else [`Self::place`].
    pub fn film_heading_place(&self) -> Option<&str> {
        self.film_place.as_deref().or(self.place.as_deref())
    }

    /// Every place the film names in this chapter, in film order: its
    /// heading place, then [`Self::film_place_changes`].
    pub fn film_places(&self) -> impl Iterator<Item = &str> {
        self.film_heading_place()
            .into_iter()
            .chain(self.film_place_changes.iter().map(|(_, p)| p.as_str()))
    }
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
    /// Which playback window of the asset this entry plays: an index into
    /// `Score::clip_windows`. Always 0 except for the sibling entries of a
    /// long multi-scene clip. `(asset_id, window)` identifies an entry.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub window: u32,
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
    /// asset_id -> the user's own caption (embedded or typed in the Story
    /// editor). Never the chapter place: book and reels add that at export.
    pub captions: std::collections::HashMap<String, String>,
    /// Route polyline (lat, lon) with day markers.
    pub map_polyline: Vec<(f64, f64)>,
    pub music: MusicPick,
    /// The user's collage overrides for the film, set only by
    /// `plan::apply_edits` from the Story editor's edits. A stored plan never
    /// carries any (and serializes as it always did).
    #[serde(default, skip_serializing_if = "CollageEdits::is_empty")]
    pub collage: CollageEdits,
    /// Photos the owner pinned to the book cover: asset id -> horizontal
    /// focus in percent (0 left edge .. 100 right edge; 50 centres), for a
    /// panorama whose interesting part is off centre.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub cover: BTreeMap<String, u8>,
    /// Crop focus the owner set per photo: asset id -> (x, y) percent of
    /// the photo (0,0 top left .. 100,100 bottom right) a cropped cell
    /// centres on, for heads the face detector cannot see (a mascot).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub crop: BTreeMap<String, (u8, u8)>,
    /// Photos the owner pinned into the book: always placed, like a
    /// captioned photo.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub book: Vec<String>,
    /// The owner's photo for the book's closing page, if chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closing: Option<String>,
}

/// How the user regrouped the film's collages, by asset id. Read by
/// `plan::collage_slots`, the one place grouping inputs are built for the
/// planner estimate, the renderer and the Story editor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollageEdits {
    /// Photos that play on their own ("Show on its own").
    #[serde(default)]
    pub solo: Vec<String>,
    /// Photos joined into one collage, earlier one first. A link holds only
    /// while the two are neighbouring collage photos of one chapter.
    #[serde(default)]
    pub joins: Vec<(String, String)>,
}

impl CollageEdits {
    pub fn is_empty(&self) -> bool {
        self.solo.is_empty() && self.joins.is_empty()
    }

    /// Whether the user set this photo to play on its own.
    pub fn is_solo(&self, asset_id: &str) -> bool {
        self.solo.iter().any(|id| id == asset_id)
    }

    /// Whether the user joined these two photos, in either order.
    pub fn linked(&self, a: &str, b: &str) -> bool {
        self.joins
            .iter()
            .any(|(x, y)| (x == a && y == b) || (x == b && y == a))
    }

    /// Play a film photo on its own, or put it back in the collages.
    /// Setting it solo also drops its joins: they would contradict it.
    pub fn set_solo(&mut self, asset_id: String, solo: bool) {
        self.solo.retain(|id| *id != asset_id);
        if solo {
            self.unjoin(&asset_id);
            self.solo.push(asset_id);
        }
    }

    /// Join two neighbouring film photos (`first` plays earlier) into one
    /// collage; neither stays solo.
    pub fn join(&mut self, first: String, second: String) {
        self.solo.retain(|id| *id != first && *id != second);
        self.joins
            .retain(|(a, b)| !(*a == first && *b == second || *a == second && *b == first));
        self.joins.push((first, second));
    }

    /// Undo the user's joins of a photo ("Separate").
    pub fn unjoin(&mut self, asset_id: &str) {
        self.joins.retain(|(a, b)| a != asset_id && b != asset_id);
    }

    /// Keep only the overrides whose photos all satisfy `keep`.
    pub fn retain_assets(&mut self, keep: impl Fn(&str) -> bool) {
        self.solo.retain(|id| keep(id));
        self.joins.retain(|(a, b)| keep(a) && keep(b));
    }
}

impl PlanEntry {
    /// Stable identity within a plan list: the asset and which of its
    /// windows this entry plays.
    pub fn key(&self) -> (&str, u32) {
        (&self.asset_id, self.window)
    }
}

/// How many distinct assets `entries` show: sibling windows of one clip
/// count once.
pub fn distinct_assets(entries: &[PlanEntry]) -> usize {
    entries
        .iter()
        .map(|e| e.asset_id.as_str())
        .collect::<std::collections::HashSet<_>>()
        .len()
}

impl Plan {
    pub const SCHEMA_VERSION: u32 = 1;

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    /// The chapter whose book flow holds `asset_id`.
    pub fn chapter_of(&self, asset_id: &str) -> Option<&Chapter> {
        self.chapters
            .iter()
            .find(|c| c.asset_ids.iter().any(|id| id == asset_id))
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
    /// Raw Hasler-Susstrunk colourfulness of a still (about 0 for grey,
    /// 80+ for vivid). `None` on older scores and for video. Ranked within
    /// the chapter by the plan, never compared across chapters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colourfulness: Option<f32>,
    /// Whether this asset is a story highlight.
    pub highlight: bool,
    /// One short string per contributing signal.
    pub reasons: Vec<String>,
    /// Playback windows picked at ingest for a long video clip, in time
    /// order: one per scene for long multi-scene clips, otherwise a single
    /// best window. Empty keeps the start-of-clip trim. Scores saved before
    /// scene splitting stored one `clip_window` pair; it still loads here.
    #[serde(
        default,
        alias = "clip_window",
        deserialize_with = "deserialize_clip_windows",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub clip_windows: Vec<ClipSpan>,
}

/// One playback window of a video clip, in seconds from the clip start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ClipSpan {
    pub start: f64,
    pub end: f64,
    /// Mean sample score (0..1) over the window; ranks sibling windows.
    /// 0 for windows converted from the old single-window format.
    #[serde(default)]
    pub quality: f32,
}

impl ClipSpan {
    /// The best of `windows` (highest quality, earliest on ties) and its
    /// index.
    pub fn best(windows: &[ClipSpan]) -> Option<(usize, ClipSpan)> {
        let mut best: Option<(usize, ClipSpan)> = None;
        for (i, w) in windows.iter().enumerate() {
            if best.is_none_or(|(_, b)| w.quality > b.quality) {
                best = Some((i, *w));
            }
        }
        best
    }
}

/// Accepts the current window list or the old single `(start, end)` pair.
fn deserialize_clip_windows<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> Result<Vec<ClipSpan>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Stored {
        Many(Vec<ClipSpan>),
        One(Option<(f64, f64)>),
    }
    Ok(match Stored::deserialize(de)? {
        Stored::Many(v) => v,
        Stored::One(w) => w
            .map(|(start, end)| ClipSpan {
                start,
                end,
                quality: 0.0,
            })
            .into_iter()
            .collect(),
    })
}

impl Score {
    /// The clip's best window (highest quality, earliest on ties) and its
    /// index in [`Score::clip_windows`].
    pub fn best_clip_window(&self) -> Option<(usize, ClipSpan)> {
        ClipSpan::best(&self.clip_windows)
    }

    /// Technical quality without story bonuses: the stored `quality`, or
    /// `total - bonus` for scores saved before it existed.
    pub fn tech_quality(&self) -> f32 {
        self.quality.unwrap_or(self.total - self.bonus)
    }
}

fn is_zero<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_single_clip_window_loads_as_one_window() {
        let old = r#"{"sharpness":0.8,"exposure":0.9,"blur":0.1,"total":0.7,
            "highlight":false,"reasons":[],"clip_window":[4.5,10.5]}"#;
        let s: Score = serde_json::from_str(old).unwrap();
        assert_eq!(
            s.clip_windows,
            vec![ClipSpan {
                start: 4.5,
                end: 10.5,
                quality: 0.0
            }]
        );
        assert_eq!(s.best_clip_window().map(|b| b.0), Some(0));
    }

    #[test]
    fn best_window_is_highest_quality_and_earliest_on_ties() {
        let w = |start, quality| ClipSpan {
            start,
            end: start + 6.0,
            quality,
        };
        assert_eq!(ClipSpan::best(&[]), None);
        let ws = [w(0.0, 0.5), w(20.0, 0.9), w(40.0, 0.9)];
        assert_eq!(ClipSpan::best(&ws), Some((1, ws[1])));
    }

    #[test]
    fn scores_without_a_window_and_new_lists_round_trip() {
        let none = r#"{"sharpness":0.8,"exposure":0.9,"blur":0.1,"total":0.7,
            "highlight":false,"reasons":[]}"#;
        assert!(serde_json::from_str::<Score>(none)
            .unwrap()
            .clip_windows
            .is_empty());
        let s = Score {
            clip_windows: vec![
                ClipSpan {
                    start: 1.0,
                    end: 7.0,
                    quality: 0.4,
                },
                ClipSpan {
                    start: 30.0,
                    end: 36.0,
                    quality: 0.8,
                },
            ],
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("clip_windows"));
        let back: Score = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        assert_eq!(back.best_clip_window().map(|b| b.0), Some(1));
        // `null` (the old Option written without skip) is also fine.
        let null = r#"{"sharpness":0,"exposure":0,"blur":0,"total":0,
            "highlight":false,"reasons":[],"clip_window":null}"#;
        assert!(serde_json::from_str::<Score>(null)
            .unwrap()
            .clip_windows
            .is_empty());
    }
}
