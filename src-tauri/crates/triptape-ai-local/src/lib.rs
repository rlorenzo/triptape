//! triptape-ai-local: local scoring and model inference behind traits.
//!
//! Sprint 2 — heuristic layer, no weights bundled:
//!
//! - [`FaceDetector`], [`Embedder`], [`Tagger`], [`Transcriber`], and
//!   [`Captioner`] are the seam traits. Model-backed implementations slot in
//!   later (Sprint 0 gate G4 pins them); cloud implementations slot in after.
//! - [`HeuristicPipeline`] is the always-available default. It ships no
//!   weights: face counts come back as [`None`] with
//!   [`ModelStatus::ModelNotPinned`], captions are templates built from plan
//!   context, and the music pick is a deterministic "vibe fit" prior.
//! - Tier budget ceilings from PLAN.md Section 13 live here as constants
//!   ([`CORE_TIER_MAX_DISK_MB`], [`CORE_TIER_MAX_PEAK_RAM_MB`],
//!   [`CORE_TIER_SCORE_1000_PHOTOS_SECS`], [`OPTIONAL_TIER_MAX_DISK_MB`]).
//!   Every model slot is NOT PINNED; see `THIRD_PARTY_MODELS.md` for the
//!   ledger and per-model blockers.

use triptape_core::types::{Asset, AssetKind, MusicPick};

// -- Model status ------------------------------------------------------------

/// Availability of a model-backed capability.
///
/// Nothing is pinned in Sprint 2, so every heuristic result carries
/// [`ModelStatus::ModelNotPinned`] with the blocker that keeps weights out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStatus {
    /// Weights pinned, measured, and loaded.
    Ready,
    /// No weights: the G4 model spike has not pinned a build yet.
    ModelNotPinned { blocker: &'static str },
}

/// Shared blocker string for all Sprint 2 heuristic results.
pub const HEURISTIC_BLOCKER: &str =
    "G4 model spike not run on the hardware baseline; heuristic fallback in use";

// -- Tier budgets (PLAN.md Section 13 ceilings) -------------------------------

/// Core tier on-disk ceiling: face detector + embedding + tagger combined.
pub const CORE_TIER_MAX_DISK_MB: u64 = 250;
/// Core tier peak-RAM ceiling on the 8 GB / 4-core hardware baseline.
pub const CORE_TIER_MAX_PEAK_RAM_MB: u64 = 1536; // 1.5 GB
/// Core tier throughput ceiling: full score pass for 1,000 photos.
pub const CORE_TIER_SCORE_1000_PHOTOS_SECS: u64 = 600; // 10 min
/// Optional tier on-disk ceiling: Whisper + SAM 2.1 small + VLM combined.
pub const OPTIONAL_TIER_MAX_DISK_MB: u64 = 1024; // 1 GB

/// Bundled music library track ids the vibe-fit pick may choose from.
pub const MUSIC_LIBRARY: &[&str] = &["morning-light", "golden-hour", "quiet-piano", "night-drive"];

// -- Face detection -----------------------------------------------------------

/// Normalized face box (0..1 in thumbnail space).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FaceBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Face detection outcome.
///
/// `face_count` is [`None`] while no detector weights are pinned — callers
/// must treat it as "unknown", never as "zero faces".
#[derive(Debug, Clone)]
pub struct FaceResult {
    pub boxes: Vec<FaceBox>,
    pub face_count: Option<u32>,
    pub status: ModelStatus,
}

/// On-device face detection (count and boxes only in MVP core).
pub trait FaceDetector: Send + Sync {
    fn detect(&self, rgb: &[u8], width: u32, height: u32) -> FaceResult;
}

// -- Embeddings ----------------------------------------------------------------

/// Embedding outcome for dedupe/clustering similarity.
#[derive(Debug, Clone)]
pub struct EmbedResult {
    pub embedding: Option<Vec<f32>>,
    pub status: ModelStatus,
}

/// DINOv2-small-class embedding seam (cosine-similarity near-dupe grouping).
pub trait Embedder: Send + Sync {
    fn embed(&self, rgb: &[u8], width: u32, height: u32) -> EmbedResult;
}

// -- Tagging -------------------------------------------------------------------

/// Zero-shot tagging outcome: (label, confidence 0..1) pairs.
#[derive(Debug, Clone)]
pub struct TagResult {
    pub tags: Vec<(String, f32)>,
    pub status: ModelStatus,
}

/// SigLIP/CLIP-small-class zero-shot tagging seam.
pub trait Tagger: Send + Sync {
    fn tag(&self, rgb: &[u8], width: u32, height: u32) -> TagResult;
}

// -- Transcription ---------------------------------------------------------------

/// Transcription outcome for clip captions/quotes.
#[derive(Debug, Clone)]
pub struct TranscriptResult {
    pub text: String,
    pub status: ModelStatus,
}

/// Whisper-class local transcription seam.
///
/// Input is mono 16 kHz float PCM. The heuristic has no weights, so it
/// always returns empty text with [`ModelStatus::ModelNotPinned`] — and
/// therefore never produces quote captions from speech.
pub trait Transcriber: Send + Sync {
    fn transcribe(&self, pcm_mono_16k: &[f32]) -> TranscriptResult;
}

// -- Captions --------------------------------------------------------------------

/// Plan context a caption draft is built from.
#[derive(Debug, Clone)]
pub struct CaptionContext<'a> {
    pub kind: AssetKind,
    pub taken_at: Option<i64>,
    pub place_label: Option<&'a str>,
}

impl Default for CaptionContext<'_> {
    fn default() -> Self {
        Self {
            kind: AssetKind::Photo,
            taken_at: None,
            place_label: None,
        }
    }
}

impl<'a> CaptionContext<'a> {
    pub fn new(kind: AssetKind) -> Self {
        Self {
            kind,
            taken_at: None,
            place_label: None,
        }
    }

    pub fn from_asset(asset: &Asset) -> CaptionContext<'_> {
        CaptionContext {
            kind: asset.kind,
            taken_at: asset.taken_at,
            place_label: None,
        }
    }
}

/// Caption draft outcome. Drafts only, always editable.
#[derive(Debug, Clone)]
pub struct CaptionDraft {
    pub text: String,
    pub status: ModelStatus,
}

/// Local VLM caption seam. The Sprint 2 default builds template captions
/// from plan context (kind + date + place), never empty.
pub trait Captioner: Send + Sync {
    fn draft_caption(&self, ctx: &CaptionContext) -> CaptionDraft;
}

// -- Heuristic pipeline (no weights) ----------------------------------------------

/// Always-available heuristic backend. Implements every seam trait with no
/// model weights; each result reports [`ModelStatus::ModelNotPinned`]
/// (except template captions and the vibe-fit music pick, which are genuine
/// heuristic outputs, not model stand-ins — captions still report the
/// not-pinned status so callers know no VLM ran).
#[derive(Debug, Clone, Copy, Default)]
pub struct HeuristicPipeline;

fn not_pinned<T>(value: T) -> (T, ModelStatus) {
    (
        value,
        ModelStatus::ModelNotPinned {
            blocker: HEURISTIC_BLOCKER,
        },
    )
}

impl FaceDetector for HeuristicPipeline {
    fn detect(&self, _rgb: &[u8], _width: u32, _height: u32) -> FaceResult {
        let (face_count, status) = not_pinned(None);
        FaceResult {
            boxes: vec![],
            face_count,
            status,
        }
    }
}

impl Embedder for HeuristicPipeline {
    fn embed(&self, _rgb: &[u8], _width: u32, _height: u32) -> EmbedResult {
        let (embedding, status) = not_pinned(None);
        EmbedResult { embedding, status }
    }
}

impl Tagger for HeuristicPipeline {
    fn tag(&self, _rgb: &[u8], _width: u32, _height: u32) -> TagResult {
        let (tags, status) = not_pinned(vec![]);
        TagResult { tags, status }
    }
}

impl Transcriber for HeuristicPipeline {
    fn transcribe(&self, _pcm_mono_16k: &[f32]) -> TranscriptResult {
        let (text, status) = not_pinned(String::new());
        TranscriptResult { text, status }
    }
}

impl Captioner for HeuristicPipeline {
    fn draft_caption(&self, ctx: &CaptionContext) -> CaptionDraft {
        let ((), status) = not_pinned(());
        CaptionDraft {
            text: template_caption(ctx),
            status,
        }
    }
}

/// Template caption from plan context (local-VLM fallback).
///
/// Never empty: `{what} from {Month D}` / `{what} in {place}` /
/// `{what} from the trip`. Mirrors the core plan builder's wording.
pub fn template_caption(ctx: &CaptionContext) -> String {
    let what = match ctx.kind {
        AssetKind::Video => "a favorite clip",
        AssetKind::LivePhoto => "a living moment",
        AssetKind::Photo => "a favorite photo",
    };
    if let Some(place) = ctx.place_label {
        return format!("{what} in {place}");
    }
    if let Some(taken) = ctx.taken_at {
        if let Some(stamp) = chrono::DateTime::from_timestamp(taken, 0) {
            use chrono::Datelike;
            let month = stamp.format("%B").to_string();
            let day = stamp.day();
            return format!("{what} from {month} {day}");
        }
    }
    format!("{what} from the trip")
}

// -- Music fit ("vibe fit", never fake emotion) -------------------------------------

/// Deterministic vibe-fit music pick from plan context.
///
/// Priors only — tempo/energy/daylight/activity, never detected emotion.
/// The rationale always starts with "Vibe fit:".
pub fn vibe_fit_music_pick(assets: &[Asset]) -> MusicPick {
    if assets.is_empty() {
        return MusicPick {
            track: MUSIC_LIBRARY[0].to_string(),
            rationale: "Vibe fit: default mid-tempo acoustic for an empty pick".to_string(),
        };
    }
    let videos = assets
        .iter()
        .filter(|a| matches!(a.kind, AssetKind::Video | AssetKind::LivePhoto))
        .count();
    let daylight = assets
        .iter()
        .filter_map(|a| {
            a.taken_at
                .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                .map(|d| {
                    use chrono::Timelike;
                    d.hour()
                })
        })
        .filter(|h| (6..18).contains(h))
        .count();
    let n = assets.len();
    // Majority video/motion trip: warmer, driving bed under kept clip audio.
    let (track, why) = if videos * 2 > n {
        (
            "golden-hour",
            "Vibe fit: warm, driving bed for a motion-heavy trip; ducked under kept clip audio",
        )
    } else if daylight * 2 >= n {
        (
            "morning-light",
            "Vibe fit: mid-tempo acoustic for mixed daylight outdoor moments",
        )
    } else if videos == 0 {
        (
            "quiet-piano",
            "Vibe fit: gentle piano for a stills-first, low-light trip",
        )
    } else {
        (
            "night-drive",
            "Vibe fit: mellow evening bed for a mixed indoor/evening trip",
        )
    };
    debug_assert!(MUSIC_LIBRARY.contains(&track));
    MusicPick {
        track: track.to_string(),
        rationale: why.to_string(),
    }
}

// -- Legacy scorer seam (kept; cloud implementations slot in later) ------------------

/// Scoring backend trait so cloud implementations can slot in later.
pub trait LocalScorer: Send + Sync {
    /// Score an asset thumbnail (raw RGB bytes, width, height). Higher is better.
    fn score_thumbnail(&self, rgb: &[u8], width: u32, height: u32) -> f32;
}

/// Heuristic fallback scorer (mean-brightness based, always available).
pub struct HeuristicScorer;

impl LocalScorer for HeuristicScorer {
    fn score_thumbnail(&self, rgb: &[u8], _width: u32, _height: u32) -> f32 {
        if rgb.is_empty() {
            return 0.0;
        }
        let mean: f32 = rgb.iter().map(|&b| b as f32).sum::<f32>() / rgb.len() as f32 / 255.0;
        // Prefer mid-brightness frames; punish pure black/white.
        1.0 - (mean - 0.5).abs() * 2.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn asset(id: &str, kind: AssetKind, taken_at: Option<i64>) -> Asset {
        let mut a = Asset::new(id, PathBuf::from(format!("/t/{id}.jpg")), kind);
        a.taken_at = taken_at;
        a
    }

    #[test]
    fn heuristic_faces_unknown_not_zero() {
        let pipe = HeuristicPipeline;
        let r = pipe.detect(&[128u8; 3 * 8 * 8], 8, 8);
        assert_eq!(r.face_count, None, "unpinned detector reports unknown");
        assert!(r.boxes.is_empty());
        assert!(
            matches!(r.status, ModelStatus::ModelNotPinned { .. }),
            "status must be ModelNotPinned"
        );
    }

    #[test]
    fn heuristic_embed_tag_transcript_all_not_pinned() {
        let pipe = HeuristicPipeline;
        let e = pipe.embed(&[0u8; 12], 2, 2);
        assert!(e.embedding.is_none());
        assert!(matches!(e.status, ModelStatus::ModelNotPinned { .. }));
        let t = pipe.tag(&[0u8; 12], 2, 2);
        assert!(t.tags.is_empty());
        assert!(matches!(t.status, ModelStatus::ModelNotPinned { .. }));
        // No quote captions from speech while Whisper is unpinned.
        let s = pipe.transcribe(&vec![0.1f32; 16000]);
        assert!(s.text.is_empty());
        assert!(matches!(s.status, ModelStatus::ModelNotPinned { .. }));
    }

    #[test]
    fn template_captions_never_empty() {
        let pipe = HeuristicPipeline;
        // Dated photo: kind + month/day.
        let d = pipe.draft_caption(&CaptionContext {
            kind: AssetKind::Photo,
            taken_at: Some(1_714_557_600), // 2024-05-04 UTC
            place_label: None,
        });
        assert!(!d.text.is_empty());
        assert!(d.text.contains("favorite photo"));
        assert!(d.text.contains("May"));
        // Place label wins over date.
        let p = pipe.draft_caption(&CaptionContext {
            kind: AssetKind::Video,
            taken_at: Some(1_714_557_600),
            place_label: Some("Lisbon"),
        });
        assert!(p.text.contains("Lisbon"));
        // No context at all: still non-empty.
        let b = pipe.draft_caption(&CaptionContext::new(AssetKind::LivePhoto));
        assert!(!b.text.is_empty());
        assert!(b.text.contains("living moment"));
    }

    #[test]
    fn caption_context_from_asset() {
        let a = asset("x", AssetKind::Video, Some(1_714_557_600));
        let ctx = CaptionContext::from_asset(&a);
        assert_eq!(ctx.kind, AssetKind::Video);
        assert_eq!(ctx.taken_at, Some(1_714_557_600));
        let d = HeuristicPipeline.draft_caption(&ctx);
        assert!(d.text.contains("favorite clip"));
    }

    #[test]
    fn vibe_fit_pick_labels_and_library() {
        // Daylight stills trip.
        let stills = vec![
            asset("a", AssetKind::Photo, Some(1_714_557_600)),
            asset("b", AssetKind::Photo, Some(1_714_561_200)),
        ];
        let m = vibe_fit_music_pick(&stills);
        assert!(MUSIC_LIBRARY.contains(&m.track.as_str()));
        assert!(
            m.rationale.starts_with("Vibe fit:"),
            "rationale={}",
            m.rationale
        );
        // Motion-heavy trip picks a different bed.
        let motion = vec![
            asset("c", AssetKind::Video, Some(1_714_557_600)),
            asset("d", AssetKind::Video, Some(1_714_561_200)),
            asset("e", AssetKind::LivePhoto, Some(1_714_561_200)),
        ];
        let mm = vibe_fit_music_pick(&motion);
        assert!(mm.rationale.starts_with("Vibe fit:"));
        assert_ne!(mm.track, "quiet-piano");
        // Empty plan still yields a usable default.
        let empty = vibe_fit_music_pick(&[]);
        assert!(MUSIC_LIBRARY.contains(&empty.track.as_str()));
        assert!(empty.rationale.starts_with("Vibe fit:"));
    }

    #[test]
    fn tier_budgets_match_plan_ceilings() {
        assert_eq!(CORE_TIER_MAX_DISK_MB, 250);
        assert_eq!(CORE_TIER_MAX_PEAK_RAM_MB, 1536);
        assert_eq!(CORE_TIER_SCORE_1000_PHOTOS_SECS, 600);
        assert_eq!(OPTIONAL_TIER_MAX_DISK_MB, 1024);
    }

    #[test]
    fn heuristic_scorer_still_works() {
        let s = HeuristicScorer;
        assert_eq!(s.score_thumbnail(&[], 0, 0), 0.0);
        let mid = s.score_thumbnail(&vec![128u8; 300], 10, 10);
        let dark = s.score_thumbnail(&vec![0u8; 300], 10, 10);
        assert!(mid > dark);
    }
}
