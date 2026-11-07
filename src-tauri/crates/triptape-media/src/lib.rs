//! triptape-media: FFmpeg/ffprobe orchestration and render-plan execution.

pub mod export;
pub mod ffmpeg;
pub mod map;
pub mod render_book;
pub mod render_reels;
pub mod render_video;
pub mod titlecard;

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("ffmpeg failed: {0}")]
    Ffmpeg(String),
    #[error("io: {0}")]
    Io(String),
    #[error("export cancelled")]
    Cancelled,
    #[error("{0}")]
    NoSpace(String),
    #[error("invalid argument: {0}")]
    InvalidArg(String),
    #[error("book preflight blocked: {0}")]
    Preflight(String),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Stop a render between steps once its job is cancelled. Children already
/// die on their own (see `triptape_core::process`); this covers the in-process
/// work between them.
pub(crate) fn check_cancelled() -> Result<(), MediaError> {
    if triptape_core::process::cancelled() {
        return Err(MediaError::Cancelled);
    }
    Ok(())
}
