//! triptape-media: FFmpeg/ffprobe orchestration and render-plan execution.

pub mod clip_analysis;
pub mod export;
pub mod ffmpeg;
pub mod map;
pub mod render_book;
pub mod render_reels;
pub mod render_video;
pub mod resolution;
pub mod titlecard;

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("ffmpeg failed: {0}")]
    Ffmpeg(String),
    #[error("io: {0}")]
    Io(String),
    #[error("invalid argument: {0}")]
    InvalidArg(String),
    #[error("book preflight blocked: {0}")]
    Preflight(String),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
