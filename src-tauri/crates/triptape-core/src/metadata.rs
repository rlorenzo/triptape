//! Stage 3 — Metadata: ffprobe JSON truth for video, EXIF for images.
//!
//! Normalizes rotation and timezone at ingest; falls back to file mtime
//! with `time_guessed = true` when no capture time exists.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Minimal ffprobe JSON surface we rely on.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FfprobeOutput {
    #[serde(default)]
    pub streams: Vec<FfprobeStream>,
    #[serde(default)]
    pub format: FfprobeFormat,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FfprobeStream {
    #[serde(default)]
    pub codec_type: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub duration: Option<String>,
    #[serde(default)]
    pub avg_frame_rate: Option<String>,
    #[serde(default)]
    pub tags: Option<std::collections::HashMap<String, String>>,
    #[serde(default)]
    pub side_data_list: Vec<FfprobeSideData>,
}

/// Per-stream side data. Modern ffprobe reports phone-video rotation here
/// (`Display Matrix`) rather than in the legacy `rotate` tag.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FfprobeSideData {
    #[serde(default)]
    pub side_data_type: Option<String>,
    /// Counter-clockwise degrees (opposite sign to the legacy `rotate` tag).
    #[serde(default)]
    pub rotation: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FfprobeFormat {
    #[serde(default)]
    pub duration: Option<String>,
    #[serde(default)]
    pub tags: Option<std::collections::HashMap<String, String>>,
}

impl FfprobeOutput {
    pub fn parse(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| e.to_string())
    }

    fn video_streams(&self) -> impl Iterator<Item = &FfprobeStream> {
        self.streams
            .iter()
            .filter(|s| s.codec_type.as_deref() == Some("video"))
    }

    /// Duration in seconds: first video-stream duration, else format duration.
    /// Unparseable values (ffprobe sometimes emits "N/A") are skipped.
    pub fn duration_secs(&self) -> Option<f64> {
        self.video_streams()
            .find_map(|s| s.duration.as_deref()?.parse().ok())
            .or_else(|| self.format.duration.as_deref()?.parse().ok())
    }

    pub fn has_video_stream(&self) -> bool {
        self.video_streams().next().is_some()
    }

    pub fn video_dims(&self) -> Option<(u32, u32)> {
        let s = self.video_streams().next()?;
        Some((s.width?, s.height?))
    }

    /// Clockwise display rotation, snapped down to a multiple of 90°.
    /// Prefers a video stream's display-matrix side data (CCW, so negated),
    /// falling back to the first stream carrying a legacy `rotate` tag.
    pub fn rotation_deg(&self) -> u16 {
        let display_matrix = self.video_streams().find_map(|s| {
            s.side_data_list
                .iter()
                .filter(|d| d.side_data_type.as_deref() == Some("Display Matrix"))
                .find_map(|d| d.rotation)
                .map(|r| -(r.round() as i32))
        });
        let legacy = || {
            self.streams
                .iter()
                .find_map(|s| s.tags.as_ref()?.get("rotate")?.parse::<i32>().ok())
        };
        display_matrix
            .or_else(legacy)
            .map_or(0, |r| (r.rem_euclid(360) / 90 * 90) as u16)
    }

    /// GPS from `location` tag (`+37.7-122.4/` ISO 6709-ish form).
    pub fn gps(&self) -> Option<(f64, f64)> {
        let tags = self.format.tags.as_ref()?;
        let loc = tags
            .get("location")
            .or_else(|| tags.get("com.apple.quicktime.location.ISO6709"))?;
        parse_iso6709(loc)
    }

    /// Capture time: `creation_time` tag as unix seconds, if present.
    pub fn creation_time(&self) -> Option<i64> {
        let tags = self.format.tags.as_ref()?;
        tags.get("creation_time")
            .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
            .map(|dt| dt.timestamp())
    }

    /// Live Photo / content-identifier UUIDs, when the container carries one.
    pub fn content_identifier(&self) -> Option<String> {
        let tags = self.format.tags.as_ref()?;
        [
            "com.apple.quicktime.content.identifier",
            "com.apple.quicktime.content-identifier",
            "content.identifier",
        ]
        .iter()
        .find_map(|k| tags.get(*k))
        .cloned()
    }
}

fn valid_lat_lon(lat: f64, lon: f64) -> Option<(f64, f64)> {
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}

/// Parse `+37.7749-122.4194/` style ISO 6709 coordinates. An optional
/// signed altitude (`+37.7749-122.4194+15.0/`) is accepted and ignored.
pub fn parse_iso6709(s: &str) -> Option<(f64, f64)> {
    let s = s.trim().trim_end_matches('/');
    // Each field starts with a sign: lat, lon, then optional altitude.
    let mut starts = s.match_indices(['+', '-']).map(|(i, _)| i);
    if starts.next()? != 0 {
        return None;
    }
    let lon_at = starts.next()?;
    let lon_end = starts.next().unwrap_or(s.len());
    if starts.next().is_some() {
        return None;
    }
    valid_lat_lon(s[..lon_at].parse().ok()?, s[lon_at..lon_end].parse().ok()?)
}

/// Full EXIF orientation transform: optional horizontal mirror applied
/// first, then a clockwise rotation. Covers all eight EXIF values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct OrientationTransform {
    pub mirror_h: bool,
    pub rotate_cw_deg: u16,
}

/// EXIF orientation (1-8) to its display transform. Unknown values are
/// treated as identity.
pub fn orientation_transform(orientation: u32) -> OrientationTransform {
    let (mirror_h, rotate_cw_deg) = match orientation {
        2 => (true, 0),
        3 => (false, 180),
        4 => (true, 180), // vertical flip == mirror + 180°
        5 => (true, 270), // transpose
        6 => (false, 90),
        7 => (true, 90), // transverse
        8 => (false, 270),
        _ => (false, 0),
    };
    OrientationTransform {
        mirror_h,
        rotate_cw_deg,
    }
}

/// Rotation component only of an EXIF orientation. Mirrored values (2, 4,
/// 5, 7) also need `orientation_transform(..).mirror_h` to render correctly.
pub fn orientation_to_degrees(orientation: u32) -> u16 {
    orientation_transform(orientation).rotate_cw_deg
}

/// Normalized capture-time outcome.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct CaptureTime {
    pub unix_secs: i64,
    /// True when we fell back to file mtime (UI shows "time guessed").
    pub time_guessed: bool,
}

/// Prefer EXIF/taken time; fall back to `mtime_secs` with the guessed flag.
pub fn resolve_capture_time(exif_secs: Option<i64>, mtime_secs: i64) -> CaptureTime {
    match exif_secs {
        Some(t) => CaptureTime {
            unix_secs: t,
            time_guessed: false,
        },
        None => CaptureTime {
            unix_secs: mtime_secs,
            time_guessed: true,
        },
    }
}

/// Run ffprobe and parse its JSON. `ffprobe_bin` allows tests to stub it.
pub fn probe_video(ffprobe_bin: &str, path: &Path) -> Result<FfprobeOutput, String> {
    let out = std::process::Command::new(ffprobe_bin)
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            path.to_string_lossy().as_ref(),
        ])
        .output()
        .map_err(|e| format!("ffprobe spawn failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())
}

/// EXIF fields we read from stills.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ImageExif {
    /// Raw EXIF orientation (1-8).
    pub orientation: Option<u32>,
    /// `DateTimeOriginal` as unix seconds.
    pub taken_at: Option<i64>,
    pub gps: Option<(f64, f64)>,
}

/// Upper bound on bytes buffered from one image for metadata parsing.
/// EXIF IFDs in TIFF-based RAWs sit near the file start; this keeps a
/// large or hostile file from forcing a whole-file allocation.
pub const EXIF_READ_CAP: u64 = 16 * 1024 * 1024;

fn read_prefix(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(path)?
        .take(EXIF_READ_CAP)
        .read_to_end(&mut buf)?;
    Ok(buf)
}

fn is_tiff_magic(head: &[u8]) -> bool {
    head.starts_with(b"II*\0") || head.starts_with(b"MM\0*")
}

/// Read EXIF orientation + capture time + GPS from an image file.
///
/// `kamadak-exif` streams JPEG/PNG/HEIF/WebP containers from a buffered
/// file (only the metadata segment is buffered). TIFF-based files (incl.
/// most RAWs) and the `rexif` fallback parse a prefix bounded by
/// `EXIF_READ_CAP`, so memory never scales with full file size.
pub fn read_image_exif(path: &Path) -> ImageExif {
    use std::io::{Read, Seek};
    let reader = exif::Reader::new();
    let Ok(mut file) = std::fs::File::open(path) else {
        return ImageExif::default();
    };
    let mut head = [0u8; 4];
    let is_tiff = file.read_exact(&mut head).is_ok() && is_tiff_magic(&head);
    if !is_tiff && file.rewind().is_ok() {
        let mut buffered = std::io::BufReader::new(file);
        if let Ok(exif) = reader.read_from_container(&mut buffered) {
            return kamadak_fields(&exif);
        }
    }
    let Ok(data) = read_prefix(path) else {
        return ImageExif::default();
    };
    if is_tiff {
        if let Ok(exif) = reader.read_raw(data.clone()) {
            return kamadak_fields(&exif);
        }
    }
    rexif::parse_buffer(&data)
        .map(|parsed| rexif_fields(&parsed))
        .unwrap_or_default()
}

fn kamadak_fields(exif: &exif::Exif) -> ImageExif {
    let field = |tag| exif.get_field(tag, exif::In::PRIMARY).map(|f| &f.value);
    let ascii = |tag| match field(tag)? {
        exif::Value::Ascii(rows) => rows
            .iter()
            .find(|r| !r.is_empty())
            .map(|r| String::from_utf8_lossy(r).trim().to_string()),
        _ => None,
    };
    let deg = |tag| match field(tag)? {
        exif::Value::Rational(rs) => dms_to_deg(&rs.iter().map(|r| r.to_f64()).collect::<Vec<_>>()),
        _ => None,
    };
    ImageExif {
        orientation: match field(exif::Tag::Orientation) {
            Some(exif::Value::Short(v)) => v.first().map(|&x| x as u32),
            Some(exif::Value::Long(v)) => v.first().copied(),
            _ => None,
        },
        // Raw ASCII ("YYYY:MM:DD HH:MM:SS"), plus OffsetTimeOriginal
        // ("+09:00") when present so the instant is exact.
        taken_at: ascii(exif::Tag::DateTimeOriginal).and_then(|dt| {
            let offset = ascii(exif::Tag::OffsetTimeOriginal).unwrap_or_default();
            exif_datetime_to_unix(&format!("{dt}{offset}"))
        }),
        gps: signed_gps(
            deg(exif::Tag::GPSLatitude),
            ascii(exif::Tag::GPSLatitudeRef),
            deg(exif::Tag::GPSLongitude),
            ascii(exif::Tag::GPSLongitudeRef),
        ),
    }
}

fn rexif_fields(parsed: &rexif::ExifData) -> ImageExif {
    let mut out = ImageExif::default();
    let (mut lat, mut lat_ref, mut lon, mut lon_ref) = (None, None, None, None);
    for entry in &parsed.entries {
        let ascii = || match &entry.value {
            rexif::TagValue::Ascii(s) => Some(s.clone()),
            _ => None,
        };
        let deg = || {
            let comps: Vec<f64> = match &entry.value {
                rexif::TagValue::URational(rs) => rs.iter().map(|r| r.value()).collect(),
                rexif::TagValue::IRational(rs) => rs.iter().map(|r| r.value()).collect(),
                _ => return None,
            };
            dms_to_deg(&comps)
        };
        match entry.tag {
            rexif::ExifTag::Orientation => {
                out.orientation = match &entry.value {
                    rexif::TagValue::U16(v) => v.first().map(|&x| x as u32),
                    rexif::TagValue::U32(v) => v.first().copied(),
                    rexif::TagValue::U8(v) => v.first().map(|&x| x as u32),
                    _ => None,
                };
            }
            rexif::ExifTag::DateTimeOriginal => {
                out.taken_at = out
                    .taken_at
                    .or_else(|| ascii().and_then(|s| exif_datetime_to_unix(&s)));
            }
            rexif::ExifTag::GPSLatitude => lat = deg(),
            rexif::ExifTag::GPSLatitudeRef => lat_ref = ascii(),
            rexif::ExifTag::GPSLongitude => lon = deg(),
            rexif::ExifTag::GPSLongitudeRef => lon_ref = ascii(),
            _ => {}
        }
    }
    out.gps = signed_gps(lat, lat_ref, lon, lon_ref);
    out
}

/// Parse an EXIF/ISO capture time to unix seconds.
///
/// Offset-bearing forms are parsed first and yield the exact instant.
/// Naive EXIF wall-clock times (no offset) are encoded as if UTC — i.e. the
/// local wall clock is preserved, not a true instant — until trip-timezone
/// normalization lands (see agent-code-review.md, capture-time finding).
fn exif_datetime_to_unix(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp());
    }
    for fmt in ["%Y:%m:%d %H:%M:%S%#z", "%Y-%m-%dT%H:%M:%S%#z"] {
        if let Ok(dt) = chrono::DateTime::parse_from_str(s, fmt) {
            return Some(dt.timestamp());
        }
    }
    for fmt in ["%Y:%m:%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(dt.and_utc().timestamp());
        }
    }
    None
}

/// Degrees/minutes/seconds components (1–3 of them) to decimal degrees.
fn dms_to_deg(comps: &[f64]) -> Option<f64> {
    match comps {
        [d, m, s] => Some(d + m / 60.0 + s / 3600.0),
        [d, m] => Some(d + m / 60.0),
        [d] => Some(*d),
        _ => None,
    }
}

/// Apply S/W hemisphere refs to unsigned EXIF coordinates and range-check.
fn signed_gps(
    lat: Option<f64>,
    lat_ref: Option<String>,
    lon: Option<f64>,
    lon_ref: Option<String>,
) -> Option<(f64, f64)> {
    let sign = |v: f64, r: String, neg: char| {
        if r.to_ascii_uppercase().starts_with(neg) {
            -v
        } else {
            v
        }
    };
    valid_lat_lon(sign(lat?, lat_ref?, 'S'), sign(lon?, lon_ref?, 'W'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso6709_parses_apple_style() {
        assert_eq!(
            parse_iso6709("+37.7749-122.4194/"),
            Some((37.7749, -122.4194))
        );
        assert_eq!(parse_iso6709("+48.85+002.35/"), Some((48.85, 2.35)));
        assert_eq!(parse_iso6709("N/A"), None);
        assert_eq!(parse_iso6709("+91.0+000.0/"), None);
        assert_eq!(
            parse_iso6709("+37.7749-122.4194+15.0/"),
            Some((37.7749, -122.4194)),
            "positive altitude ignored"
        );
        assert_eq!(
            parse_iso6709("-33.8688+151.2093-002.5/"),
            Some((-33.8688, 151.2093)),
            "negative altitude ignored"
        );
    }

    #[test]
    fn rotation_normalizes() {
        // 45° is not a real EXIF rotation; ffprobe rotate tags round to 90s.
        let mut out = FfprobeOutput::default();
        out.streams.push(FfprobeStream {
            codec_type: Some("video".into()),
            tags: Some(
                [("rotate".to_string(), "90".to_string())]
                    .into_iter()
                    .collect(),
            ),
            ..Default::default()
        });
        assert_eq!(out.rotation_deg(), 90);
        assert_eq!(orientation_to_degrees(6), 90);
        assert_eq!(orientation_to_degrees(1), 0);
        let t = |o| {
            let x = orientation_transform(o);
            (x.mirror_h, x.rotate_cw_deg)
        };
        assert_eq!(
            [1, 2, 3, 4, 5, 6, 7, 8, 0, 9].map(t),
            [
                (false, 0),
                (true, 0),
                (false, 180),
                (true, 180),
                (true, 270),
                (false, 90),
                (true, 90),
                (false, 270),
                (false, 0),
                (false, 0),
            ]
        );
    }

    #[test]
    fn mtime_fallback_flags_guessed() {
        let c = resolve_capture_time(None, 1700000000);
        assert!(c.time_guessed);
        assert_eq!(c.unix_secs, 1700000000);
        let c2 = resolve_capture_time(Some(1699999999), 1700000000);
        assert!(!c2.time_guessed);
    }

    #[test]
    fn ffprobe_json_parses() {
        let json = r#"{
            "streams": [{"codec_type":"video","width":1920,"height":1080,
                         "duration":"3.200000","tags":{"rotate":"0"}}],
            "format": {"duration":"3.200000",
                       "tags":{"creation_time":"2024-05-01T10:00:00.000Z",
                               "location":"+37.7749-122.4194/"}}
        }"#;
        let out = FfprobeOutput::parse(json).unwrap();
        assert!(out.has_video_stream());
        assert_eq!(out.video_dims(), Some((1920, 1080)));
        assert!((out.duration_secs().unwrap() - 3.2).abs() < 1e-6);
        assert_eq!(out.gps(), Some((37.7749, -122.4194)));
        assert!(out.creation_time().is_some());
    }

    #[test]
    fn dms_and_hemisphere_signing() {
        assert_eq!(dms_to_deg(&[37.0, 30.0, 0.0]), Some(37.5));
        assert_eq!(dms_to_deg(&[10.0, 30.0]), Some(10.5));
        assert_eq!(dms_to_deg(&[]), None);
        let r = |s: &str| Some(s.to_string());
        assert_eq!(
            signed_gps(Some(33.9), r("S"), Some(151.2), r("E")),
            Some((-33.9, 151.2))
        );
        assert_eq!(
            signed_gps(Some(40.7), r("n"), Some(74.0), r("w")),
            Some((40.7, -74.0))
        );
        assert_eq!(signed_gps(Some(95.0), r("N"), Some(1.0), r("E")), None);
        assert_eq!(signed_gps(Some(1.0), None, Some(1.0), r("E")), None);
    }

    #[test]
    fn exif_datetime_formats() {
        assert_eq!(
            exif_datetime_to_unix("2024:05:01 10:00:00"),
            Some(1714557600)
        );
        assert_eq!(
            exif_datetime_to_unix(" 2024-05-01T10:00:00 "),
            Some(1714557600)
        );
        assert_eq!(exif_datetime_to_unix("garbage"), None);
        // Offsets are honored, not dropped: 10:00-07:00 == 17:00Z.
        assert_eq!(
            exif_datetime_to_unix("2024:05:01 10:00:00-07:00"),
            Some(1714557600 + 7 * 3600)
        );
        assert_eq!(
            exif_datetime_to_unix("2024-05-01T10:00:00+09:00"),
            Some(1714557600 - 9 * 3600)
        );
        // Local midnight in Tokyo is the previous UTC day.
        assert_eq!(
            exif_datetime_to_unix("2024:05:02 00:30:00+09:00"),
            exif_datetime_to_unix("2024-05-01T15:30:00Z")
        );
    }

    #[test]
    fn rotation_prefers_display_matrix() {
        let json = r#"{"streams":[{"codec_type":"video",
            "side_data_list":[{"side_data_type":"Display Matrix",
                               "displaymatrix":"...","rotation":-90}],
            "tags":{"rotate":"180"}}]}"#;
        assert_eq!(FfprobeOutput::parse(json).unwrap().rotation_deg(), 90);
        let json = r#"{"streams":[{"codec_type":"video",
            "side_data_list":[{"side_data_type":"Display Matrix","rotation":90.0}]}]}"#;
        assert_eq!(FfprobeOutput::parse(json).unwrap().rotation_deg(), 270);
    }

    #[test]
    fn ffprobe_fallbacks_and_tags() {
        let json = r#"{
            "streams": [{"codec_type":"audio","duration":"9.0"},
                        {"codec_type":"video","duration":"N/A",
                         "tags":{"rotate":"-90"}}],
            "format": {"duration":"2.5",
                       "tags":{"com.apple.quicktime.content.identifier":"uuid-9"}}
        }"#;
        let out = FfprobeOutput::parse(json).unwrap();
        assert_eq!(
            out.duration_secs(),
            Some(2.5),
            "N/A video falls back to format"
        );
        assert_eq!(out.rotation_deg(), 270);
        assert_eq!(out.content_identifier().as_deref(), Some("uuid-9"));
        assert_eq!(out.video_dims(), None);
        assert!(out.gps().is_none());
        assert!(!FfprobeOutput::default().has_video_stream());
    }

    #[test]
    fn non_exif_file_yields_empty() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.jpg");
        std::fs::write(&p, b"not an image").unwrap();
        assert_eq!(read_image_exif(&p), ImageExif::default());
        assert_eq!(
            read_image_exif(&dir.path().join("missing.jpg")),
            ImageExif::default()
        );
    }
}
