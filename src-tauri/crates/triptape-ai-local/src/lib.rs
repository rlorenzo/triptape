//! triptape-ai-local: local scoring and model inference behind traits.
//! Minimal placeholder so the workspace builds. Model-backed scoring lands in later sprints.

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
