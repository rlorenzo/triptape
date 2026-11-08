//! Output resolution of the hero film.
//!
//! Every size in the hero path (output frame, 1.5x supersample canvas, clip
//! pre-scale, collage canvas, map card, Typst PNGs) derives from one
//! [`Resolution`], so 4K is a setting rather than a second code path.

use crate::MediaError;

/// Hero film frame size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Resolution {
    /// 1920x1080: smaller files, faster renders.
    Hd1080,
    /// 3840x2160: the default; matches the user's 4K source footage.
    #[default]
    Uhd4k,
}

impl Resolution {
    /// Parse a CLI/IPC flag: `4k` / `uhd` / `2160p` or `1080p` / `hd` / `fhd`.
    pub fn from_flag(s: &str) -> Result<Self, MediaError> {
        match s.trim().to_ascii_lowercase().as_str() {
            "4k" | "uhd" | "2160p" | "2160" => Ok(Self::Uhd4k),
            "1080p" | "1080" | "hd" | "fhd" => Ok(Self::Hd1080),
            other => Err(MediaError::InvalidArg(format!(
                "--resolution must be 4k or 1080p, got {other:?}"
            ))),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Hd1080 => "1080p",
            Self::Uhd4k => "4k",
        }
    }

    /// Output frame width and height.
    pub fn dims(self) -> (u32, u32) {
        match self {
            Self::Hd1080 => (1920, 1080),
            Self::Uhd4k => (3840, 2160),
        }
    }

    pub fn width(self) -> u32 {
        self.dims().0
    }

    pub fn height(self) -> u32 {
        self.dims().1
    }

    /// Linear factor over the 1080p design units used for fixed pixel sizes
    /// (Typst pages, collage gap, map strokes).
    pub fn scale(self) -> u32 {
        self.width() / 1920
    }

    /// Supersample canvas (1.5x the frame): composites are built once at
    /// this size and the glide downsamples from it.
    pub fn supersample(self) -> (u32, u32) {
        let (w, h) = self.dims();
        (w * 3 / 2, h * 3 / 2)
    }

    /// Clip pre-scale before the push-in. 4/3 of the frame at 1080p
    /// (downscales 4K sources, upscales 720p). At 4K it is the frame itself:
    /// 4K sources are never upscaled, lower-resolution ones get one lanczos
    /// upscale.
    pub fn clip_prescale(self) -> (u32, u32) {
        match self {
            Self::Hd1080 => (2560, 1440),
            Self::Uhd4k => (3840, 2160),
        }
    }

    /// Typst `--ppi` that turns the 1080p-design pt pages into frame pixels.
    pub fn ppi(self) -> u32 {
        72 * self.scale()
    }

    /// Explicit H.264 profile/level for the delivery encode. 4K30 needs level
    /// 5.1 (x264 would pick it anyway; stating it keeps players and TVs sure).
    pub fn x264_level_args(self) -> Vec<String> {
        match self {
            Self::Hd1080 => vec![],
            Self::Uhd4k => ["-profile:v", "high", "-level:v", "5.1"]
                .map(String::from)
                .to_vec(),
        }
    }

    /// Peak memory of one hero segment render, used to cap how many run at
    /// once. Measured on a 10-core M-series as ffmpeg's resident size for a
    /// still-hold segment (the supersampled canvas through zoompan): ~1.7 GB
    /// at 1080p, ~3.8 GB at 4K, with ~9.6 GB aggregate seen at 4K once clips
    /// and collages were in the mix, so 4K is rounded well up.
    pub fn segment_peak_bytes(self) -> u64 {
        match self {
            Self::Hd1080 => 1_800_000_000,
            Self::Uhd4k => 5_000_000_000,
        }
    }

    /// Rough bytes the whole export may need on disk (intermediates plus
    /// outputs), for the free-space precheck.
    pub fn export_disk_estimate(self) -> u64 {
        match self {
            Self::Hd1080 => 2_000_000_000,
            Self::Uhd4k => 12_000_000_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_derive_from_the_preset() {
        assert_eq!(Resolution::Hd1080.dims(), (1920, 1080));
        assert_eq!(Resolution::Hd1080.supersample(), (2880, 1620));
        assert_eq!(Resolution::Hd1080.clip_prescale(), (2560, 1440));
        assert_eq!(Resolution::Hd1080.ppi(), 72);
        assert_eq!(Resolution::Uhd4k.dims(), (3840, 2160));
        assert_eq!(Resolution::Uhd4k.supersample(), (5760, 3240));
        assert_eq!(Resolution::Uhd4k.clip_prescale(), (3840, 2160));
        assert_eq!(Resolution::Uhd4k.ppi(), 144);
    }

    #[test]
    fn only_4k_states_an_h264_level() {
        assert!(Resolution::Hd1080.x264_level_args().is_empty());
        let uhd = Resolution::Uhd4k.x264_level_args();
        assert!(uhd.windows(2).any(|p| p == ["-level:v", "5.1"]), "{uhd:?}");
        assert!(
            Resolution::Uhd4k.export_disk_estimate() > Resolution::Hd1080.export_disk_estimate()
        );
    }

    #[test]
    fn flag_parsing_and_default() {
        assert_eq!(Resolution::default(), Resolution::Uhd4k);
        assert_eq!(Resolution::from_flag("4K").unwrap(), Resolution::Uhd4k);
        assert_eq!(Resolution::from_flag("1080p").unwrap(), Resolution::Hd1080);
        assert!(Resolution::from_flag("720p").is_err());
    }
}
