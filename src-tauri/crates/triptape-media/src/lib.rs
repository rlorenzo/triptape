//! triptape-media: FFmpeg/ffprobe orchestration and render-plan execution.
//! Minimal placeholder so the workspace builds. Full render stages land in later sprints.

use serde::{Deserialize, Serialize};

/// Target output for a render job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderTarget {
    pub path: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// Placeholder render-plan executor. Returns an error until implemented.
pub fn execute_render_plan(_plan_json: &str, _target: &RenderTarget) -> Result<(), MediaError> {
    Err(MediaError::NotImplemented)
}

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("render execution not implemented yet")]
    NotImplemented,
    #[error("ffmpeg failed: {0}")]
    Ffmpeg(String),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
