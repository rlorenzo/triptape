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

    /// Container `creation_time` tag as unix seconds, if present. Apple
    /// Photos rewrites it when an edit (a trim) is saved, so it is the
    /// fallback behind [`Self::apple_creation_date`].
    pub fn creation_time(&self) -> Option<i64> {
        let tags = self.format.tags.as_ref()?;
        tags.get("creation_time")
            .and_then(|v| v.parse::<jiff::Timestamp>().ok())
            .map(unix_floor)
    }

    /// `com.apple.quicktime.creationdate` as unix seconds and its UTC offset
    /// (seconds east). iPhones write the real capture moment here, and it
    /// survives a trim in Photos while `creation_time` becomes the save time.
    pub fn apple_creation_date(&self) -> Option<(i64, i32)> {
        let tags = self.format.tags.as_ref()?;
        parse_apple_creation_date(tags.get("com.apple.quicktime.creationdate")?)
    }

    /// Capture instant and, when known, its UTC offset:
    /// [`Self::apple_creation_date`] first, else [`Self::creation_time`]
    /// (an instant with no offset).
    pub fn capture_time(&self) -> Option<(i64, Option<i32>)> {
        self.apple_creation_date()
            .map(|(t, o)| (t, Some(o)))
            .or_else(|| self.creation_time().map(|t| (t, None)))
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

pub(crate) fn valid_lat_lon(lat: f64, lon: f64) -> Option<(f64, f64)> {
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

/// Fill a still's capture time, UTC offset and GPS on `a`. The file's own
/// EXIF wins; when it has no capture time (osxphotos-exported edits carry
/// none) the photo library's date and offset (`library`) come next, and
/// only then file mtime with `time_guessed`. GPS likewise falls back to
/// the library only when the file has none.
pub fn fill_photo_capture(
    a: &mut crate::types::Asset,
    exif: &ImageExif,
    library: Option<&crate::osxphotos::LibraryEntry>,
) {
    let (taken_at, tz_offset_secs) = match (exif.taken_at, library.and_then(|l| l.taken_at)) {
        (Some(t), _) => (Some(t), exif.tz_offset_secs),
        (None, Some(t)) => (Some(t), library.and_then(|l| l.tz_offset_secs)),
        (None, None) => (None, exif.tz_offset_secs),
    };
    let ct = resolve_capture_time(taken_at, a.mtime);
    a.taken_at = Some(ct.unix_secs);
    a.time_guessed = ct.time_guessed;
    a.tz_offset_secs = tz_offset_secs;
    a.gps = exif.gps.or_else(|| library.and_then(|l| l.gps));
}

/// Fill a still's metadata from its `exif`: capture time, offset and GPS
/// ([`fill_photo_capture`]), story fields ([`story_fields`]), rotation and
/// the EXIF pixel size. `library` is the folder's osxphotos index (looked
/// up once per asset); `salt` (the project id) salts the people signature.
/// Shared by the CLI and app ingest so the two cannot drift.
pub fn fill_photo(
    a: &mut crate::types::Asset,
    exif: &ImageExif,
    library: Option<&crate::osxphotos::OsxPhotosIndex>,
    salt: &str,
) {
    let lib = library.and_then(|l| l.get(&a.path));
    fill_photo_capture(a, exif, lib);
    story_fields(&a.path, Some(exif), lib).apply(a, salt);
    a.rotation_deg = exif.orientation.map(orientation_to_degrees).unwrap_or(0);
    if let (Some(w), Some(h)) = (exif.width, exif.height) {
        a.width = Some(w);
        a.height = Some(h);
    }
}

/// Fill a video's metadata from its ffprobe output plus its story fields
/// (see [`fill_photo`] for `library` and `salt`). Capture time comes from
/// the probe ([`FfprobeOutput::capture_time`]: Apple's creation date, which
/// an edit keeps, before the container's `creation_time`, which an edit
/// resets), then the photo library's date, then file mtime with
/// `time_guessed` (also for unprobeable videos).
pub fn fill_video(
    a: &mut crate::types::Asset,
    probe: Option<&FfprobeOutput>,
    library: Option<&crate::osxphotos::OsxPhotosIndex>,
    salt: &str,
) {
    let lib = library.and_then(|l| l.get(&a.path));
    story_fields(&a.path, None, lib).apply(a, salt);
    if let Some(p) = probe {
        a.duration_secs = p.duration_secs();
        if let Some((w, h)) = p.video_dims() {
            a.width = Some(w);
            a.height = Some(h);
        }
        a.rotation_deg = p.rotation_deg();
        a.gps = p.gps();
    }
    let library_time = || {
        let l = lib?;
        Some((l.taken_at?, l.tz_offset_secs))
    };
    let (taken_at, tz_offset_secs) = probe
        .and_then(FfprobeOutput::capture_time)
        .or_else(library_time)
        .map_or((None, None), |(t, o)| (Some(t), o));
    let ct = resolve_capture_time(taken_at, a.mtime);
    a.taken_at = Some(ct.unix_secs);
    a.time_guessed = ct.time_guessed;
    a.tz_offset_secs = tz_offset_secs;
}

/// Parse `com.apple.quicktime.creationdate` (`2026-10-14T13:30:00+0900`;
/// the offset may also be `+09:00` or `Z`) to unix seconds and the offset in
/// seconds east of UTC. `None` without an offset.
pub fn parse_apple_creation_date(s: &str) -> Option<(i64, i32)> {
    let s = s.trim();
    // Apple writes the offset without a colon, which RFC 3339 needs.
    let b = s.as_bytes();
    let n = b.len();
    let colonless = n > 5
        && matches!(b[n - 5], b'+' | b'-')
        && b[n - 4..].iter().all(u8::is_ascii_digit)
        && b[..n - 5].contains(&b'T');
    if colonless {
        parse_iso_instant(&format!("{}:{}", &s[..n - 2], &s[n - 2..]))
    } else {
        parse_iso_instant(s)
    }
}

/// Parse an ISO 8601 / RFC 3339 instant with a UTC offset (as osxphotos
/// writes: `2026-10-27T10:05:38.928000+08:00`) to unix seconds (floored)
/// and the offset in seconds east of UTC. `None` without an offset.
pub fn parse_iso_instant(s: &str) -> Option<(i64, i32)> {
    use jiff::fmt::temporal::{Pieces, PiecesOffset};
    let pieces = Pieces::parse(s.trim()).ok()?;
    let offset = match pieces.offset()? {
        PiecesOffset::Zulu => jiff::tz::Offset::UTC,
        PiecesOffset::Numeric(n) => n.offset(),
        _ => return None,
    };
    let ts = offset
        .to_timestamp(pieces.date().to_datetime(pieces.time()?))
        .ok()?;
    Some((unix_floor(ts), offset.seconds()))
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
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImageExif {
    /// Raw EXIF orientation (1-8).
    pub orientation: Option<u32>,
    /// `DateTimeOriginal` as unix seconds.
    pub taken_at: Option<i64>,
    /// UTC offset at capture (`OffsetTimeOriginal`), seconds east of UTC.
    pub tz_offset_secs: Option<i32>,
    pub gps: Option<(f64, f64)>,
    /// Full-resolution pixel dimensions (EXIF PixelX/YDimension). The only
    /// reliable source for HEIC, whose ffprobe streams are tiles.
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Caption the user wrote into the file itself (EXIF `ImageDescription`
    /// or XMP `dc:description`), cleaned. Their own words outrank every
    /// auto-generated caption downstream.
    pub caption: Option<String>,
    /// People tagged in the file's own XMP (see [`xmp_people`]; only
    /// their count and salted signature are ever stored).
    pub people: Option<PeopleTags>,
}

/// Clean a raw embedded caption: collapse whitespace, drop empties and
/// camera-software placeholders, cap at 240 chars.
pub fn clean_caption(raw: &str) -> Option<String> {
    let joined = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let cleaned = joined.trim_matches('"').trim();
    if cleaned.len() < 2
        || cleaned.eq_ignore_ascii_case("default")
        || cleaned.chars().all(|c| !c.is_alphanumeric())
    {
        return None;
    }
    Some(cleaned.chars().take(240).collect())
}

/// Extract XMP `dc:description` text from a file prefix. Exports from
/// Photos/Lightroom put user captions there more often than in EXIF;
/// this is a tolerant byte scan, not an XML parse.
fn xmp_description(data: &[u8]) -> Option<String> {
    // No closing `>`: the opening tag may carry attributes (`xml:lang`).
    const TAG: &[u8] = b"<dc:description";
    let start = data.windows(TAG.len()).position(|w| w == TAG)? + TAG.len();
    let rest = String::from_utf8_lossy(&data[start..]);
    let open_end = rest.find('>')?;
    if rest[..open_end].ends_with('/') {
        return None; // Self-closed: empty, and what follows isn't its text.
    }
    let inner = &rest[open_end + 1..];
    // Photos writes the caption as plain element text; Lightroom wraps
    // it in rdf:Alt/rdf:li. Handle both. The rdf:li must be this element's:
    // a later one could be a dc:subject keyword, not a caption.
    let body = if inner.trim_start().starts_with('<') {
        let close = inner.find("</dc:description>").unwrap_or(inner.len());
        let li = inner[..close].find("<rdf:li")?;
        let gt = inner[li..].find('>')? + li + 1;
        &inner[gt..]
    } else {
        inner
    };
    let end = body.find('<')?;
    clean_caption(&xml_unescape(&body[..end]))
}

/// Decode the five predefined XML entities in element text.
fn xml_unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&") // Last, so `&amp;lt;` stays a literal `&lt;`.
}

/// Most people counted for one photo: a tagged crowd saturates the story
/// bonus long before this, and a keyword dump must not look like a crowd.
const MAX_PEOPLE: u32 = 20;
/// Longest keyword (chars) still read as a person's name.
const MAX_NAME_CHARS: usize = 40;
/// Most words in a keyword still read as a person's name.
const MAX_NAME_WORDS: usize = 4;

/// The XMP packet inside a file prefix (embedded in an image, or the whole
/// body of a `.xmp` sidecar), as text. `None` when no packet is present.
fn xmp_packet(data: &[u8]) -> Option<String> {
    const OPEN: &[u8] = b"<x:xmpmeta";
    const CLOSE: &[u8] = b"</x:xmpmeta>";
    let start = data.windows(OPEN.len()).position(|w| w == OPEN)?;
    let rest = &data[start..];
    let end = rest
        .windows(CLOSE.len())
        .position(|w| w == CLOSE)
        .map_or(rest.len(), |e| e + CLOSE.len());
    Some(String::from_utf8_lossy(&rest[..end]).into_owned())
}

/// Text of the first `<tag ...>...</tag>` element, or `None` when absent or
/// self-closed.
fn xmp_element<'a>(xmp: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}");
    let mut from = 0;
    while let Some(i) = xmp[from..].find(&open) {
        let at = from + i;
        let after = &xmp[at + open.len()..];
        // `<dc:subject>` must not match `<dc:subjectFoo>`.
        if !after.starts_with(['>', ' ', '/', '\n', '\t', '\r']) {
            from = at + open.len();
            continue;
        }
        let gt = after.find('>')?;
        if after[..gt].ends_with('/') {
            return None;
        }
        let body = &after[gt + 1..];
        let end = body.find(&format!("</{tag}>")).unwrap_or(body.len());
        return Some(&body[..end]);
    }
    None
}

/// Unescaped text of every non-empty `<rdf:li>` in an element body.
fn xmp_list_items(body: &str) -> Vec<String> {
    let mut out = vec![];
    let mut rest = body;
    while let Some(i) = rest.find("<rdf:li") {
        rest = &rest[i + "<rdf:li".len()..];
        let Some(gt) = rest.find('>') else { break };
        if rest[..gt].ends_with('/') {
            continue;
        }
        let text = &rest[gt + 1..];
        let end = text.find('<').unwrap_or(text.len());
        let item = xml_unescape(&text[..end]);
        let item = item.trim();
        if !item.is_empty() {
            out.push(item.to_string());
        }
    }
    out
}

/// Who a photo library tagged in one photo, held only long enough to count
/// and sign. Names are personal data: they never leave this type (its
/// `Debug` prints counts only), and only [`PeopleTags::count`] and the
/// salted [`PeopleTags::signature`] are ever stored.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct PeopleTags {
    /// Trimmed, lowercased, sorted, distinct names.
    named: Vec<String>,
    /// Faces or tags without a name.
    unnamed: u32,
}

impl std::fmt::Debug for PeopleTags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeopleTags")
            .field("named", &self.named.len())
            .field("unnamed", &self.unnamed)
            .finish()
    }
}

/// Domain tag mixed into every people signature (bump on format changes).
const PEOPLE_SIG_DOMAIN: &[u8] = b"triptape.people_sig.v1";

impl PeopleTags {
    /// Tags from `named` people (normalized: trimmed, lowercased, empties
    /// dropped, duplicates collapsed) plus `unnamed` faces. `None` when
    /// nobody is tagged.
    pub fn new(named: impl IntoIterator<Item = String>, unnamed: u32) -> Option<Self> {
        let mut named: Vec<String> = named
            .into_iter()
            .map(|n| n.trim().to_lowercase())
            .filter(|n| !n.is_empty())
            .collect();
        named.sort();
        named.dedup();
        (!named.is_empty() || unnamed > 0).then_some(Self { named, unnamed })
    }

    /// How many people are tagged, saturating at [`MAX_PEOPLE`].
    pub fn count(&self) -> u32 {
        u32::try_from(self.named.len())
            .unwrap_or(u32::MAX)
            .saturating_add(self.unnamed)
            .min(MAX_PEOPLE)
    }

    /// Stable signature of WHO is tagged: the same people (in any order or
    /// case) give the same value, different people give different values
    /// even at the same count. `salt` (the project id) keeps signatures
    /// from being comparable across projects. BLAKE3 over length-prefixed
    /// fields, so it is stable across runs and builds (unlike
    /// `DefaultHasher`).
    pub fn signature(&self, salt: &str) -> u64 {
        let mut h = blake3::Hasher::new();
        let mut field = |bytes: &[u8]| {
            h.update(&(bytes.len() as u64).to_le_bytes());
            h.update(bytes);
        };
        field(PEOPLE_SIG_DOMAIN);
        field(salt.as_bytes());
        for name in &self.named {
            field(name.as_bytes());
        }
        field(&self.unnamed.to_le_bytes());
        let digest = h.finalize();
        let first: [u8; 8] = digest.as_bytes()[..8].try_into().expect("32-byte digest");
        u64::from_le_bytes(first)
    }
}

/// A `dc:subject` keyword that plausibly names a person: short, no digits,
/// no hierarchy or URL punctuation.
fn looks_like_name(keyword: &str) -> bool {
    keyword.chars().count() <= MAX_NAME_CHARS
        && keyword.split_whitespace().count() <= MAX_NAME_WORDS
        && !keyword
            .chars()
            .any(|c| c.is_ascii_digit() || matches!(c, '/' | '\\' | ':' | '|' | '>' | '#' | '@'))
}

/// Who a file's XMP says appears in the photo.
///
/// Sources, in order of trust:
/// 1. `Iptc4xmpExt:PersonInImage`: the standard people field.
/// 2. MWG face regions (`mwg-rs:Type` = `Face`): one unnamed person per
///    detected face.
/// 3. Plain `dc:subject` keywords. Apple Photos exports each named person
///    as a keyword, but other libraries put topics there too, so this is a
///    heuristic: each distinct keyword that looks like a name (1 to 4
///    words, no digits or path/URL punctuation) counts as one person.
///
/// Zero people yields `None`.
pub fn xmp_people(data: &[u8]) -> Option<PeopleTags> {
    let xmp = xmp_packet(data)?;
    let tagged = xmp_element(&xmp, "Iptc4xmpExt:PersonInImage")
        .and_then(|b| PeopleTags::new(xmp_list_items(b), 0));
    let faces = || {
        let n = xmp.matches("mwg-rs:Type=\"Face\"").count()
            + xmp.matches("<mwg-rs:Type>Face</mwg-rs:Type>").count();
        PeopleTags::new([], u32::try_from(n).unwrap_or(u32::MAX))
    };
    let keywords = || {
        xmp_element(&xmp, "dc:subject").and_then(|b| {
            PeopleTags::new(
                xmp_list_items(b).into_iter().filter(|k| looks_like_name(k)),
                0,
            )
        })
    };
    tagged.or_else(faces).or_else(keywords)
}

/// User caption and tagged people from an XMP sidecar next to `path`
/// (`IMG_1.heic.xmp`, `IMG_1.xmp`, or `IMG_1.XMP`), reading each sidecar
/// at most once. `IMG_1.heic.xmp` names exactly this photo, so when it
/// exists it is the answer even for fields it lacks. `IMG_1.xmp` may belong
/// to a same-stem sibling (RAW + HEIC), and its values must not land on
/// this photo instead. Photos exports write one sidecar per photo and put
/// the user's caption inside; the image itself may carry none.
pub fn read_sidecar_story(path: &Path) -> (Option<String>, Option<PeopleTags>) {
    if let Ok(data) = read_prefix(&path.with_added_extension("xmp")) {
        return (xmp_description(&data), xmp_people(&data));
    }
    let (mut caption, mut people) = (None, None);
    for c in [path.with_extension("xmp"), path.with_extension("XMP")] {
        if caption.is_some() && people.is_some() {
            break;
        }
        let Ok(data) = read_prefix(&c) else { continue };
        caption = caption.or_else(|| xmp_description(&data));
        people = people.or_else(|| xmp_people(&data));
    }
    (caption, people)
}

/// An asset's story fields, resolved across every source.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Story {
    pub caption: Option<String>,
    pub people: Option<PeopleTags>,
    /// The photo library's own aesthetic score, carried as-is.
    pub library_score: Option<f32>,
}

impl Story {
    /// Write these fields onto `a`. Only the people count and the salted
    /// signature land on the asset, never the names.
    pub fn apply(self, a: &mut crate::types::Asset, salt: &str) {
        a.user_caption = self.caption;
        a.people_count = self.people.as_ref().map(PeopleTags::count);
        a.people_sig = self.people.as_ref().map(|p| p.signature(salt));
        a.library_score = self.library_score;
    }
}

/// An asset's story fields. Per field, in-file values from `exif` (photos)
/// win, the XMP sidecar comes next, and the photo library's export
/// database (`library`, see [`crate::osxphotos`]) fills what is still
/// missing. The sidecar is not read when `exif` already supplies both
/// caption and people.
pub fn story_fields(
    path: &Path,
    exif: Option<&ImageExif>,
    library: Option<&crate::osxphotos::LibraryEntry>,
) -> Story {
    let mut caption = exif.and_then(|e| e.caption.clone());
    let mut people = exif.and_then(|e| e.people.clone());
    if caption.is_none() || people.is_none() {
        let (side_caption, side_people) = read_sidecar_story(path);
        caption = caption.or(side_caption);
        people = people.or(side_people);
    }
    if let Some(lib) = library {
        caption = caption.or_else(|| lib.description.as_deref().and_then(clean_caption));
        people = people.or_else(|| lib.people());
    }
    Story {
        caption,
        people,
        library_score: library.and_then(|l| l.overall),
    }
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
    let mut out = read_image_exif_fields(path);
    if let Ok(data) = read_prefix(path) {
        if out.caption.is_none() {
            out.caption = xmp_description(&data);
        }
        out.people = xmp_people(&data);
        if out.width.is_none() || out.height.is_none() {
            if let Some((w, h)) = heif_dimensions(&data) {
                (out.width, out.height) = (Some(w), Some(h));
            }
        }
    }
    out
}

/// Stored pixel size of a HEIF/HEIC still, from its item properties: the
/// largest `ispe` (image spatial extents) box, which is the full image
/// (the others are grid tiles and the thumbnail). Photos edited in Apple
/// Photos are exported without EXIF, so this is their only size. `None`
/// for anything that is not an ISO-BMFF file or has no usable box.
pub fn heif_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.get(4..8) != Some(b"ftyp") {
        return None;
    }
    let be = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    // Properties live in the `meta` box, ahead of the pixel data.
    let meta_end = data
        .windows(4)
        .position(|w| w == b"mdat")
        .unwrap_or(data.len());
    data[..meta_end]
        .windows(20)
        // size(4) "ispe" version+flags(4) width(4) height(4)
        .filter(|b| &b[4..8] == b"ispe" && be(&b[0..4]) == 20)
        .map(|b| (be(&b[12..16]), be(&b[16..20])))
        .filter(|&(w, h)| w > 0 && h > 0)
        .max_by_key(|&(w, h)| u64::from(w) * u64::from(h))
}

fn read_image_exif_fields(path: &Path) -> ImageExif {
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
    let dim = |tag| match field(tag)? {
        exif::Value::Long(v) => v.first().copied(),
        exif::Value::Short(v) => v.first().map(|&x| x as u32),
        exif::Value::Rational(rs) => rs.first().map(|r| r.to_f64() as u32),
        _ => None,
    };
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
    let offset = ascii(exif::Tag::OffsetTimeOriginal);
    ImageExif {
        orientation: match field(exif::Tag::Orientation) {
            Some(exif::Value::Short(v)) => v.first().map(|&x| x as u32),
            Some(exif::Value::Long(v)) => v.first().copied(),
            _ => None,
        },
        // Raw ASCII ("YYYY:MM:DD HH:MM:SS"), plus OffsetTimeOriginal
        // ("+09:00") when present so the instant is exact.
        taken_at: ascii(exif::Tag::DateTimeOriginal).and_then(|dt| {
            let offset = offset.as_deref().unwrap_or_default();
            exif_datetime_to_unix(&format!("{dt}{offset}"))
        }),
        tz_offset_secs: offset.as_deref().and_then(parse_tz_offset),
        gps: signed_gps(
            deg(exif::Tag::GPSLatitude),
            ascii(exif::Tag::GPSLatitudeRef),
            deg(exif::Tag::GPSLongitude),
            ascii(exif::Tag::GPSLongitudeRef),
        ),
        width: dim(exif::Tag::PixelXDimension),
        height: dim(exif::Tag::PixelYDimension),
        caption: ascii(exif::Tag::ImageDescription).and_then(|s| clean_caption(&s)),
        people: None,
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
            rexif::ExifTag::ImageDescription => {
                out.caption = out
                    .caption
                    .take()
                    .or_else(|| ascii().and_then(|s| clean_caption(&s)));
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

/// Parse an EXIF UTC offset (`+09:00`, `-0530`, `+09`) to seconds east.
pub fn parse_tz_offset(s: &str) -> Option<i32> {
    let s = s.trim();
    let (sign, rest) = match s.strip_prefix('-') {
        Some(r) => (-1i32, r),
        None => (1i32, s.strip_prefix('+').unwrap_or(s)),
    };
    if !rest.bytes().all(|b| b.is_ascii_digit() || b == b':') {
        return None;
    }
    let digits: String = rest.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 2 || digits.len() > 4 {
        return None;
    }
    let (h, m) = if digits.len() <= 2 {
        (digits.parse::<i32>().ok()?, 0)
    } else {
        (
            digits[..2].parse::<i32>().ok()?,
            digits[2..].parse::<i32>().ok()?,
        )
    };
    if h > 14 || m > 59 {
        return None;
    }
    Some(sign * (h * 3600 + m * 60))
}

/// Capture wall-clock in the photo's own timezone: the instant shifted
/// by its UTC offset, read as a naive datetime. Without an offset the
/// instant is read as UTC (legacy behavior). `None` when out of range.
pub fn local_datetime(taken_at: i64, tz_offset_secs: Option<i32>) -> Option<jiff::civil::DateTime> {
    let shifted = taken_at.checked_add(i64::from(tz_offset_secs.unwrap_or(0)))?;
    let ts = jiff::Timestamp::from_second(shifted).ok()?;
    Some(jiff::tz::TimeZone::UTC.to_datetime(ts))
}

/// Whole unix seconds of an instant, rounded down: half a second before
/// 1970 is second -1, not 0.
fn unix_floor(ts: jiff::Timestamp) -> i64 {
    ts.as_millisecond().div_euclid(1000)
}

/// Unix seconds of a wall-clock time read as UTC.
pub fn wall_to_unix(wall: jiff::civil::DateTime) -> Option<i64> {
    let zoned = wall.to_zoned(jiff::tz::TimeZone::UTC).ok()?;
    Some(zoned.timestamp().as_second())
}

/// Calendar date of [`local_datetime`].
pub fn local_date(taken_at: i64, tz_offset_secs: Option<i32>) -> Option<jiff::civil::Date> {
    local_datetime(taken_at, tz_offset_secs).map(|d| d.date())
}

/// Fill missing capture-timezone offsets from the nearest-in-time asset
/// that has one (same trip, usually same timezone block). Assets with no
/// dated neighbor keep `None`. Deterministic (ties go to the earlier
/// neighbor); the input slice order is untouched.
///
/// Photo/Live Photo times read from EXIF without an offset are naive wall
/// clock encoded as UTC (see `exif_datetime_to_unix`); once an offset is
/// inferred for one, `taken_at` is shifted back by it so it becomes a true
/// instant and `local_datetime` reproduces the original wall clock. Video
/// `creation_time` (RFC 3339) and mtime fallbacks are already instants.
///
/// A naive wall clock is not comparable to a known instant until it has an
/// offset, so its nearest neighbor is found per candidate offset: the time
/// is normalized with that offset, then compared to the assets carrying it.
pub fn fill_tz_offsets(assets: &mut [crate::types::Asset]) {
    let mut known: Vec<(i64, i32)> = assets
        .iter()
        .filter_map(|a| Some((a.taken_at?, a.tz_offset_secs?)))
        .collect();
    if known.is_empty() {
        return;
    }
    known.sort_unstable();
    let mut by_offset: std::collections::BTreeMap<i32, Vec<i64>> = Default::default();
    for &(kt, o) in &known {
        by_offset.entry(o).or_default().push(kt);
    }
    for a in assets.iter_mut().filter(|a| a.tz_offset_secs.is_none()) {
        let Some(t) = a.taken_at else { continue };
        let wall_clock = a.kind != crate::types::AssetKind::Video && !a.time_guessed;
        a.tz_offset_secs = if wall_clock {
            nearest_wall_clock_offset(t, &by_offset)
        } else {
            // Nearest by time: the neighbors either side of `t`'s sorted slot.
            let i = known.partition_point(|&(kt, _)| kt < t);
            let before = i.checked_sub(1).map(|j| known[j]);
            let after = known.get(i).copied();
            match (before, after) {
                (Some(b), Some(n)) if n.0 - t < t - b.0 => Some(n.1),
                (Some(b), _) => Some(b.1),
                (None, n) => n.map(|n| n.1),
            }
        };
        if let Some(o) = a.tz_offset_secs.filter(|_| wall_clock) {
            a.taken_at = t.checked_sub(i64::from(o));
        }
    }
}

/// Offset under which naive wall clock `t` lands closest to a known instant
/// with that same offset. Ties go to the earlier neighbor.
fn nearest_wall_clock_offset(
    t: i64,
    by_offset: &std::collections::BTreeMap<i32, Vec<i64>>,
) -> Option<i32> {
    by_offset
        .iter()
        .filter_map(|(&o, times)| {
            let instant = t.saturating_sub(i64::from(o));
            let i = times.partition_point(|&k| k < instant);
            let before = i.checked_sub(1).map(|j| times[j]);
            let nearest = [before, times.get(i).copied()]
                .into_iter()
                .flatten()
                .min_by_key(|&k| (k.abs_diff(instant), k))?;
            Some((nearest.abs_diff(instant), nearest, o))
        })
        .min()
        .map(|(_, _, o)| o)
}

/// Parse an EXIF/ISO capture time to unix seconds.
///
/// Offset-bearing forms are parsed first and yield the exact instant.
/// Naive EXIF wall-clock times (no offset) are encoded as if UTC — i.e. the
/// local wall clock is preserved, not a true instant — until trip-timezone
/// normalization lands (see agent-code-review.md, capture-time finding).
fn exif_datetime_to_unix(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Ok(ts) = s.parse::<jiff::Timestamp>() {
        return Some(unix_floor(ts));
    }
    // EXIF's colon-dated form is not RFC 3339: split any offset (`Z`,
    // `+09:00`, `+0900`, `+09`) off the wall clock by hand. The date takes
    // the first 10 bytes, so a sign past them starts the offset.
    let cut = s
        .bytes()
        .enumerate()
        .skip(10)
        .find(|(_, b)| matches!(b, b'+' | b'-' | b'Z' | b'z'))
        .map_or(s.len(), |(i, _)| i);
    let (wall, offset) = s.split_at(cut);
    let offset = match offset {
        "" | "Z" | "z" => 0,
        o => parse_tz_offset(o)?,
    };
    let wall = ["%Y:%m:%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S"]
        .iter()
        .find_map(|fmt| jiff::civil::DateTime::strptime(fmt, wall.trim_end()).ok())?;
    Some(wall_to_unix(wall)? - i64::from(offset))
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
    fn captions_clean_and_extract_from_xmp() {
        assert_eq!(
            clean_caption("  Mia's   first\n shinkansen  ").as_deref(),
            Some("Mia's first shinkansen")
        );
        assert_eq!(clean_caption(""), None);
        assert_eq!(clean_caption("default"), None);
        assert_eq!(clean_caption(" - "), None);
        let xmp = br#"<x:xmpmeta><rdf:RDF><rdf:Description><dc:description><rdf:Alt><rdf:li xml:lang="x-default">Tom &amp; Jerry at the park</rdf:li></rdf:Alt></dc:description></rdf:Description></rdf:RDF></x:xmpmeta>"#;
        assert_eq!(
            xmp_description(xmp).as_deref(),
            Some("Tom & Jerry at the park")
        );
        assert_eq!(xmp_description(b"no xmp here"), None);
        let escaped = br#"<dc:description><rdf:li>Learning &amp;lt;html&amp;gt;</rdf:li>"#;
        assert_eq!(
            xmp_description(escaped).as_deref(),
            Some("Learning &lt;html&gt;")
        );
        // Photos' plain-text form (what its XMP sidecars carry).
        let plain = b"<x:xmpmeta><rdf:RDF><rdf:Description><dc:description>Won trophy for winning trivia</dc:description></rdf:Description></rdf:RDF></x:xmpmeta>";
        assert_eq!(
            xmp_description(plain).as_deref(),
            Some("Won trophy for winning trivia")
        );
        // Pretty-printed rdf:Alt: whitespace before the first child.
        let indented = b"<dc:description>\n  <rdf:Alt>\n    <rdf:li xml:lang=\"x-default\">On the ferry</rdf:li>\n  </rdf:Alt>\n</dc:description>";
        assert_eq!(xmp_description(indented).as_deref(), Some("On the ferry"));
        // Attributes on the opening tag, in both body forms.
        let attr_plain = br#"<dc:description xml:lang="en">At the summit</dc:description>"#;
        assert_eq!(
            xmp_description(attr_plain).as_deref(),
            Some("At the summit")
        );
        let attr_alt = br#"<dc:description xml:lang="en"><rdf:Alt><rdf:li>At the lake</rdf:li></rdf:Alt></dc:description>"#;
        assert_eq!(xmp_description(attr_alt).as_deref(), Some("At the lake"));
        // Empty descriptions must not borrow a later element's text.
        let keywords = "<rdf:Bag><rdf:li>keyword</rdf:li></rdf:Bag>";
        for empty in ["<dc:description/>", "<dc:description></dc:description>"] {
            let xmp = format!("{empty}<dc:subject>{keywords}</dc:subject>");
            assert_eq!(xmp_description(xmp.as_bytes()), None, "{empty}");
        }
    }

    #[test]
    fn sidecar_caption_reads_next_to_photo() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar = |name: &str, words: &str| {
            let body = format!(
                "<x:xmpmeta><rdf:Description><dc:description>{words}</dc:description></rdf:Description></x:xmpmeta>"
            );
            std::fs::write(dir.path().join(name), body).unwrap();
        };
        let caption = |photo: &str| read_sidecar_story(&dir.path().join(photo)).0;

        // Every supported sidecar naming.
        sidecar("IMG_1.xmp", "Replaced extension");
        sidecar("IMG_2.XMP", "Upper-case extension");
        sidecar("IMG_3.heic.xmp", "Appended extension");
        assert_eq!(caption("IMG_1.heic").as_deref(), Some("Replaced extension"));
        assert_eq!(
            caption("IMG_2.heic").as_deref(),
            Some("Upper-case extension")
        );
        assert_eq!(caption("IMG_3.heic").as_deref(), Some("Appended extension"));

        // No sidecar, and a sidecar that carries no caption.
        assert_eq!(caption("IMG_4.heic"), None);
        std::fs::write(dir.path().join("IMG_5.xmp"), "<x:xmpmeta/>").unwrap();
        assert_eq!(caption("IMG_5.heic"), None);

        // The photo's own sidecar beats a same-stem sibling's, even when
        // it has no caption to offer.
        sidecar("IMG_6.xmp", "The RAW's words");
        sidecar("IMG_6.heic.xmp", "The HEIC's words");
        assert_eq!(caption("IMG_6.heic").as_deref(), Some("The HEIC's words"));
        sidecar("IMG_7.xmp", "The RAW's words");
        std::fs::write(dir.path().join("IMG_7.heic.xmp"), "<x:xmpmeta/>").unwrap();
        assert_eq!(caption("IMG_7.heic"), None);
    }

    #[test]
    fn parse_tz_offset_forms() {
        assert_eq!(parse_tz_offset("+09:00"), Some(9 * 3600));
        assert_eq!(parse_tz_offset("-0530"), Some(-(5 * 3600 + 30 * 60)));
        assert_eq!(parse_tz_offset("+09"), Some(9 * 3600));
        assert_eq!(parse_tz_offset(" 00:00 "), Some(0));
        assert_eq!(parse_tz_offset("+15:00"), None);
        assert_eq!(parse_tz_offset("+09:75"), None);
        assert_eq!(parse_tz_offset(""), None);
        assert_eq!(parse_tz_offset("+09:xx"), None);
        assert_eq!(parse_tz_offset("+09:00junk"), None);
    }

    #[test]
    fn local_datetime_shifts_by_offset() {
        // 2026-10-04 22:50 UTC is 07:50 next day in Japan.
        let t = 1_791_154_200;
        let jp = local_datetime(t, Some(9 * 3600)).unwrap();
        assert_eq!(
            jp.strftime("%Y-%m-%d %H:%M").to_string(),
            "2026-10-05 07:50"
        );
        let utc = local_datetime(t, None).unwrap();
        assert_eq!(
            utc.strftime("%Y-%m-%d %H:%M").to_string(),
            "2026-10-04 22:50"
        );
        assert_eq!(local_datetime(i64::MAX, Some(3600)), None);
    }

    #[test]
    fn fill_tz_offsets_uses_nearest_neighbor() {
        use crate::types::{Asset, AssetKind};
        let mk = |id: &str, t: Option<i64>, o: Option<i32>| {
            // Video: a true instant, directly comparable to known instants.
            let mut a = Asset::new(id, std::path::PathBuf::from(id), AssetKind::Video);
            a.taken_at = t;
            a.tz_offset_secs = o;
            a
        };
        let mut assets = vec![
            mk("late", Some(1_000), Some(9 * 3600)),
            mk("near-early", Some(120), None),
            mk("early", Some(100), Some(-5 * 3600)),
            mk("tie", Some(550), None),
            mk("near-late", Some(990), None),
            mk("undated", None, None),
            mk("own", Some(500), Some(3600)),
        ];
        fill_tz_offsets(&mut assets);
        let off = |id: &str| assets.iter().find(|a| a.id == id).unwrap().tz_offset_secs;
        assert_eq!(off("near-early"), Some(-5 * 3600));
        assert_eq!(off("near-late"), Some(9 * 3600));
        // 550 is 50 s after `own` (500): nearest wins over the far neighbors.
        assert_eq!(off("tie"), Some(3600));
        assert_eq!(off("own"), Some(3600), "known offsets are kept");
        assert_eq!(off("undated"), None);
        assert_eq!(assets[0].id, "late", "input order untouched");

        // Equidistant neighbors: the earlier one wins.
        let mut eq = vec![
            mk("a", Some(0), Some(1)),
            mk("b", Some(10), None),
            mk("c", Some(20), Some(2)),
        ];
        fill_tz_offsets(&mut eq);
        assert_eq!(eq[1].tz_offset_secs, Some(1));

        // Nothing known: nothing filled.
        let mut none = vec![mk("x", Some(0), None)];
        fill_tz_offsets(&mut none);
        assert_eq!(none[0].tz_offset_secs, None);
    }

    #[test]
    fn fill_tz_offsets_normalizes_naive_wall_clock_per_candidate() {
        use crate::types::{Asset, AssetKind};
        let wall = |s: &str| {
            wall_to_unix(jiff::civil::DateTime::strptime("%Y-%m-%d %H:%M", s).unwrap()).unwrap()
        };
        let known = |id: &str, utc: &str, o: i32| {
            let mut a = Asset::new(id, id.into(), AssetKind::Photo);
            a.taken_at = Some(wall(utc));
            a.tz_offset_secs = Some(o);
            a
        };
        let naive = |id: &str, w: &str| {
            let mut a = Asset::new(id, id.into(), AssetKind::Photo);
            a.taken_at = Some(wall(w));
            a
        };
        let (jst, hst) = (9 * 3600, -10 * 3600);
        let mut assets = vec![
            known("japan", "2026-10-04 14:30", jst),
            // Closer to the Japan photo's 23:00 encoding, but hours later.
            known("hawaii", "2026-10-04 23:10", hst),
            // 23:00 JST is 14:00 UTC, 30 min from the Japan photo.
            naive("shot-in-japan", "2026-10-04 23:00"),
            // 13:00 HST is 23:00 UTC, 10 min from the Hawaii photo.
            naive("shot-in-hawaii", "2026-10-04 13:00"),
        ];
        fill_tz_offsets(&mut assets);
        assert_eq!(assets[2].tz_offset_secs, Some(jst));
        assert_eq!(assets[2].taken_at, Some(wall("2026-10-04 14:00")));
        assert_eq!(assets[3].tz_offset_secs, Some(hst));
        assert_eq!(assets[3].taken_at, Some(wall("2026-10-04 23:00")));
    }

    #[test]
    fn fill_tz_offsets_keeps_naive_wall_clock_near_midnight() {
        use crate::types::{Asset, AssetKind};
        let wall = |s: &str| {
            wall_to_unix(jiff::civil::DateTime::strptime("%Y-%m-%d %H:%M", s).unwrap()).unwrap()
        };
        let local = |a: &Asset| {
            local_datetime(a.taken_at.unwrap(), a.tz_offset_secs)
                .unwrap()
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        };
        let jst = 9 * 3600;
        // Offset-bearing photo: 22:30 JST as an exact instant.
        let mut known = Asset::new("k", "k".into(), AssetKind::Photo);
        known.taken_at = Some(wall("2026-10-04 22:30") - i64::from(jst));
        known.tz_offset_secs = Some(jst);
        // Offset-free EXIF photo: 23:00 wall clock encoded as UTC.
        let mut naive = Asset::new("n", "n".into(), AssetKind::LivePhoto);
        naive.taken_at = Some(wall("2026-10-04 23:00"));
        // mtime fallback and video creation_time are true instants.
        let instant = wall("2026-10-04 14:10");
        let mut guessed = Asset::new("g", "g".into(), AssetKind::Photo);
        guessed.taken_at = Some(instant);
        guessed.time_guessed = true;
        let mut video = Asset::new("v", "v".into(), AssetKind::Video);
        video.taken_at = Some(instant);
        let mut assets = vec![known, naive, guessed, video];
        fill_tz_offsets(&mut assets);
        assert!(assets.iter().all(|a| a.tz_offset_secs == Some(jst)));
        assert_eq!(local(&assets[1]), "2026-10-04 23:00", "wall clock kept");
        assert_eq!(local(&assets[2]), "2026-10-04 23:10");
        assert_eq!(local(&assets[3]), "2026-10-04 23:10");
    }

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
        // EXIF's colon form takes every offset spelling, and `Z`.
        for offset in ["+09:00", "+0900", "+09", " +09:00"] {
            assert_eq!(
                exif_datetime_to_unix(&format!("2024:05:01 10:00:00{offset}")),
                Some(1714557600 - 9 * 3600),
                "{offset}"
            );
        }
        assert_eq!(
            exif_datetime_to_unix("2024:05:01 10:00:00Z"),
            Some(1714557600)
        );
        assert_eq!(
            exif_datetime_to_unix("2024-05-01T10:00:00.000000Z"),
            Some(1714557600)
        );
        assert_eq!(exif_datetime_to_unix("1969:12:31 23:59:59"), Some(-1));
        // A fraction before 1970 rounds down, as whole seconds do.
        assert_eq!(exif_datetime_to_unix("1969-12-31T23:59:59.500Z"), Some(-1));
        // Impossible dates, offsets and partial times are refused.
        for bad in [
            "2024:02:30 10:00:00",
            "0000:00:00 00:00:00",
            "2024:05:01 24:00:00",
            "2024:05:01 10:00:00+25:00",
            "2024:05:01 10:00:00+09:xx",
            "2024:05:01 10:00",
            "2024:05:01",
        ] {
            assert_eq!(exif_datetime_to_unix(bad), None, "{bad}");
        }
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

    const XMP_HEAD: &str = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF><rdf:Description>"#;
    const XMP_TAIL: &str = "</rdf:Description></rdf:RDF></x:xmpmeta>";

    fn xmp(body: &str) -> Vec<u8> {
        format!("junk\0{XMP_HEAD}{body}{XMP_TAIL}trailing").into_bytes()
    }

    #[test]
    fn people_count_from_dc_subject_keywords() {
        let data = xmp(
            "<dc:subject><rdf:Bag><rdf:li>Ann</rdf:li><rdf:li>Bo Lee</rdf:li>\
             <rdf:li>ann</rdf:li><rdf:li>2026/Japan</rdf:li><rdf:li>Trip 2026</rdf:li>\
             </rdf:Bag></dc:subject>",
        );
        // Duplicates fold, digit and path-like keywords are topics.
        assert_eq!(xmp_people_count(&data), Some(2));
    }

    #[test]
    fn person_in_image_beats_keywords_and_regions() {
        let data = xmp(
            "<dc:subject><rdf:Bag><rdf:li>A</rdf:li><rdf:li>B</rdf:li><rdf:li>C</rdf:li>\
             </rdf:Bag></dc:subject>\
             <Iptc4xmpExt:PersonInImage><rdf:Bag><rdf:li>Ann</rdf:li></rdf:Bag>\
             </Iptc4xmpExt:PersonInImage>",
        );
        assert_eq!(xmp_people_count(&data), Some(1));
    }

    #[test]
    fn face_regions_beat_keywords() {
        let data = xmp(
            r#"<mwg-rs:Regions><rdf:Bag><rdf:li><rdf:Description mwg-rs:Type="Face"/></rdf:li>
               <rdf:li><rdf:Description mwg-rs:Type="Face"/></rdf:li>
               <rdf:li><rdf:Description mwg-rs:Type="Pet"/></rdf:li></rdf:Bag></mwg-rs:Regions>
               <dc:subject><rdf:Bag><rdf:li>X</rdf:li></rdf:Bag></dc:subject>"#,
        );
        assert_eq!(xmp_people_count(&data), Some(2));
    }

    #[test]
    fn heif_dimensions_takes_the_largest_extent_before_the_pixel_data() {
        let ispe = |w: u32, h: u32| {
            let mut b = vec![0, 0, 0, 20];
            b.extend(b"ispe\0\0\0\0");
            b.extend(w.to_be_bytes());
            b.extend(h.to_be_bytes());
            b
        };
        let mut heic = b"\0\0\0\x18ftypheic".to_vec();
        heic.extend(ispe(896, 960)); // grid tile
        heic.extend(ispe(4284, 5712)); // the photo
        heic.extend(ispe(576, 768)); // thumbnail
        heic.extend(b"\0\0\0\x08mdat");
        heic.extend(ispe(9000, 9000)); // pixel bytes that happen to look like a box
        assert_eq!(heif_dimensions(&heic), Some((4284, 5712)));
        // Not ISO-BMFF, no box, or a zero-sized box: no size.
        assert_eq!(
            heif_dimensions(b"\xff\xd8\xff\xe0 just a jpeg, long enough"),
            None
        );
        assert_eq!(heif_dimensions(b"\0\0\0\x18ftypheic"), None);
        let mut empty = b"\0\0\0\x18ftypheic".to_vec();
        empty.extend(ispe(0, 100));
        assert_eq!(heif_dimensions(&empty), None);
    }

    #[test]
    fn people_count_edge_cases() {
        // No packet, no list, empty and self-closed lists, look-alike tag.
        assert_eq!(xmp_people_count(b"just a jpeg"), None);
        assert_eq!(xmp_people_count(&xmp("<dc:title>x</dc:title>")), None);
        assert_eq!(xmp_people_count(&xmp("<dc:subject/>")), None);
        assert_eq!(
            xmp_people_count(&xmp("<dc:subjectX><rdf:li>Ann</rdf:li></dc:subjectX>")),
            None
        );
        // Escapes unescape; a keyword dump saturates.
        let names: String = (0..40)
            .map(|i| format!("<rdf:li>{}</rdf:li>", "n".repeat(i + 1)))
            .collect();
        let data = xmp(&format!(
            "<dc:subject><rdf:Bag>{names}</rdf:Bag></dc:subject>"
        ));
        assert_eq!(xmp_people_count(&data), Some(MAX_PEOPLE));
        let amp = xmp("<dc:subject><rdf:Bag><rdf:li>A &amp; B</rdf:li></rdf:Bag></dc:subject>");
        assert_eq!(xmp_people_count(&amp), Some(1));
    }

    #[test]
    fn sidecar_people_follows_caption_sidecar_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, n: usize| {
            let items: String = (0..n)
                .map(|i| format!("<rdf:li>P{}</rdf:li>", "z".repeat(i)))
                .collect();
            std::fs::write(
                dir.path().join(name),
                xmp(&format!(
                    "<dc:subject><rdf:Bag>{items}</rdf:Bag></dc:subject>"
                )),
            )
            .unwrap();
        };
        write("IMG_1.heic.xmp", 3);
        write("IMG_1.xmp", 1);
        write("IMG_2.xmp", 2);
        let people = |photo: &str| {
            read_sidecar_story(&dir.path().join(photo))
                .1
                .map(|p| p.count())
        };
        assert_eq!(people("IMG_1.heic"), Some(3), "exact sidecar wins");
        assert_eq!(people("IMG_2.heic"), Some(2));
        assert_eq!(people("IMG_3.heic"), None);
        // An exact sidecar without people hides the same-stem sibling's.
        std::fs::write(
            dir.path().join("IMG_2.heic.xmp"),
            xmp("<dc:title>t</dc:title>"),
        )
        .unwrap();
        assert_eq!(people("IMG_2.heic"), None);
    }

    fn xmp_people_count(data: &[u8]) -> Option<u32> {
        xmp_people(data).map(|p| p.count())
    }

    fn count_of(story: &Story) -> Option<u32> {
        story.people.as_ref().map(PeopleTags::count)
    }

    fn names(n: &[&str]) -> Option<PeopleTags> {
        PeopleTags::new(n.iter().map(|s| s.to_string()), 0)
    }

    #[test]
    fn story_fields_prefer_in_file_values_and_fill_from_the_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let photo = dir.path().join("IMG_1.heic");
        std::fs::write(
            dir.path().join("IMG_1.heic.xmp"),
            "<x:xmpmeta><rdf:Description><dc:description>Sidecar words</dc:description>\
             <Iptc4xmpExt:PersonInImage><rdf:Bag><rdf:li>Ann</rdf:li><rdf:li>Bo</rdf:li>\
             </rdf:Bag></Iptc4xmpExt:PersonInImage></rdf:Description></x:xmpmeta>",
        )
        .unwrap();
        // One sidecar read yields both fields.
        let (caption, people) = read_sidecar_story(&photo);
        assert_eq!(caption.as_deref(), Some("Sidecar words"));
        assert_eq!(people, names(&["Ann", "Bo"]));
        let s = story_fields(&photo, None, None);
        assert_eq!(s.caption.as_deref(), Some("Sidecar words"));
        assert_eq!(count_of(&s), Some(2));
        // In-file values win; the sidecar fills only what is missing.
        let exif = ImageExif {
            caption: Some("In-file words".into()),
            ..Default::default()
        };
        let s = story_fields(&photo, Some(&exif), None);
        assert_eq!(s.caption.as_deref(), Some("In-file words"));
        assert_eq!(count_of(&s), Some(2));
        let full = ImageExif {
            caption: Some("In-file words".into()),
            people: PeopleTags::new([], 5),
            ..Default::default()
        };
        let s = story_fields(&photo, Some(&full), None);
        assert_eq!(s.caption.as_deref(), Some("In-file words"));
        assert_eq!(count_of(&s), Some(5));
        assert_eq!(
            story_fields(&dir.path().join("IMG_9.heic"), None, None),
            Story::default()
        );
    }

    #[test]
    fn xmp_wins_over_the_library_which_fills_the_gaps() {
        use crate::osxphotos::LibraryEntry;
        let dir = tempfile::tempdir().unwrap();
        let photo = dir.path().join("IMG_1.heic");
        std::fs::write(
            dir.path().join("IMG_1.heic.xmp"),
            xmp("<Iptc4xmpExt:PersonInImage><rdf:Bag><rdf:li>Ann</rdf:li>\
                 </rdf:Bag></Iptc4xmpExt:PersonInImage>"),
        )
        .unwrap();
        let lib = LibraryEntry {
            description: Some("  Library   words ".into()),
            persons: vec!["Cy".into(), "Di".into(), "_UNKNOWN_".into()],
            overall: Some(0.3),
            ..Default::default()
        };
        // Sidecar people win; the library supplies the missing caption and
        // its score.
        let s = story_fields(&photo, None, Some(&lib));
        assert_eq!(s.caption.as_deref(), Some("Library words"));
        assert_eq!(s.people, names(&["Ann"]));
        assert_eq!(s.library_score, Some(0.3));
        // An in-file caption wins over the library's.
        let exif = ImageExif {
            caption: Some("Mine".into()),
            ..Default::default()
        };
        let s = story_fields(&photo, Some(&exif), Some(&lib));
        assert_eq!(s.caption.as_deref(), Some("Mine"));
        // No XMP at all: everything comes from the library.
        let bare = dir.path().join("IMG_2.heic");
        let s = story_fields(&bare, None, Some(&lib));
        assert_eq!(count_of(&s), Some(3));
        // Applying the story stores only the count and the salted sig.
        let mut a = crate::types::Asset::new("a", bare, crate::types::AssetKind::Photo);
        s.clone().apply(&mut a, "prj_1");
        assert_eq!(a.user_caption.as_deref(), Some("Library words"));
        assert_eq!(a.people_count, Some(3));
        assert_eq!(a.people_sig, s.people.map(|p| p.signature("prj_1")));
        assert_eq!(a.library_score, Some(0.3));
    }

    #[test]
    fn people_signature_tracks_who_not_order_or_case() {
        let ab = names(&["Ann", "Bo"]).unwrap();
        let ba = names(&[" bo ", "ANN", "Ann"]).unwrap();
        assert_eq!(ab, ba);
        assert_eq!(ab.signature("p"), ba.signature("p"));
        // Same count, different people.
        let cd = names(&["Cy", "Di"]).unwrap();
        assert_eq!(ab.count(), cd.count());
        assert_ne!(ab.signature("p"), cd.signature("p"));
        // Unnamed faces matter, and the salt separates projects.
        let ab_face = PeopleTags::new(["Ann".to_string(), "Bo".to_string()], 1).unwrap();
        assert_ne!(ab.signature("p"), ab_face.signature("p"));
        assert_ne!(ab.signature("p"), ab.signature("q"));
        assert_eq!(PeopleTags::new([" ".to_string()], 0), None);
        assert_eq!(PeopleTags::new([], 0), None);
        assert!(!format!("{ab:?}").contains("ann"));
        // The XMP path signs the names it parses, like the library path.
        let data = xmp("<Iptc4xmpExt:PersonInImage><rdf:Bag><rdf:li>Bo</rdf:li>\
                        <rdf:li>Ann</rdf:li></rdf:Bag></Iptc4xmpExt:PersonInImage>");
        assert_eq!(xmp_people(&data), Some(ab));
    }

    #[test]
    fn iso_instants_parse_with_fraction_and_offset() {
        // 2026-10-27T02:05:38.928Z, floored to the second.
        assert_eq!(
            parse_iso_instant("2026-10-27T10:05:38.928000+08:00"),
            Some((1_793_066_738, 8 * 3600))
        );
        assert_eq!(
            parse_iso_instant("2026-10-26T21:05:38.5-05:00"),
            Some((1_793_066_738, -5 * 3600))
        );
        assert_eq!(
            parse_iso_instant("2026-10-27T02:05:38Z"),
            Some((1_793_066_738, 0))
        );
        assert_eq!(
            parse_iso_instant("2026-10-27T07:35:38+05:30"),
            Some((1_793_066_738, 5 * 3600 + 1800))
        );
        // Floor, not truncate: half a second before the epoch is -1.
        assert_eq!(
            parse_iso_instant("1969-12-31T23:59:59.500+00:00"),
            Some((-1, 0))
        );
        assert_eq!(parse_iso_instant("2026-10-27T10:05:38"), None);
        assert_eq!(parse_iso_instant(""), None);
    }

    #[test]
    fn apple_creation_date_forms() {
        // 2026-10-14T04:30:00Z in each form Apple and ffprobe produce.
        let utc = 1_791_952_200;
        assert_eq!(
            parse_apple_creation_date("2026-10-14T13:30:00+0900"),
            Some((utc, 9 * 3600))
        );
        assert_eq!(
            parse_apple_creation_date("2026-10-13T21:30:00-0700"),
            Some((utc, -7 * 3600))
        );
        assert_eq!(
            parse_apple_creation_date("2026-10-14T04:30:00Z"),
            Some((utc, 0))
        );
        assert_eq!(
            parse_apple_creation_date("2026-10-14T13:30:00+09:00"),
            Some((utc, 9 * 3600))
        );
        assert_eq!(parse_apple_creation_date("2026-10-14T13:30:00"), None);
        assert_eq!(parse_apple_creation_date("+0900"), None);
        assert_eq!(parse_apple_creation_date(""), None);
    }

    /// A trim saved in Apple Photos resets `creation_time` to the save
    /// moment; Apple's creation date still holds the capture.
    #[test]
    fn video_capture_prefers_apple_creation_date() {
        use crate::types::{Asset, AssetKind};
        let probe = |tags: &str| {
            FfprobeOutput::parse(&format!(r#"{{"format":{{"tags":{{{tags}}}}}}}"#)).unwrap()
        };
        let edited = probe(
            r#""creation_time":"2026-10-15T19:19:09.000000Z","com.apple.quicktime.creationdate":"2026-10-14T13:30:00+0900""#,
        );
        assert_eq!(edited.capture_time(), Some((1_791_952_200, Some(9 * 3600))));
        let plain = probe(r#""creation_time":"2026-10-15T19:19:09.000000Z""#);
        assert_eq!(plain.capture_time(), Some((1_792_091_949, None)));
        // An unreadable Apple date does not hide creation_time.
        let bad = probe(
            r#""creation_time":"2026-10-15T19:19:09.000000Z","com.apple.quicktime.creationdate":"soon""#,
        );
        assert_eq!(bad.capture_time(), Some((1_792_091_949, None)));
        assert_eq!(probe("").capture_time(), None);

        let mut a = Asset::new("v", "/t/IMG_6674_edited.mov".into(), AssetKind::Video);
        a.mtime = 2_000_000_000;
        fill_video(&mut a, Some(&edited), None, "prj");
        assert_eq!(a.taken_at, Some(1_791_952_200));
        assert_eq!(a.tz_offset_secs, Some(9 * 3600));
        assert!(!a.time_guessed);
        // The capture lands on its own local day, Oct 14 in Jeju.
        assert_eq!(
            local_date(a.taken_at.unwrap(), a.tz_offset_secs).unwrap(),
            jiff::civil::date(2026, 10, 14)
        );
        // No date anywhere: mtime, guessed, no offset.
        fill_video(&mut a, Some(&probe("")), None, "prj");
        assert_eq!(a.taken_at, Some(2_000_000_000));
        assert!(a.time_guessed);
        assert_eq!(a.tz_offset_secs, None);
    }

    #[test]
    fn photo_capture_prefers_exif_then_library_then_mtime() {
        use crate::osxphotos::LibraryEntry;
        use crate::types::{Asset, AssetKind};
        let lib = LibraryEntry {
            taken_at: Some(1_793_066_738),
            tz_offset_secs: Some(8 * 3600),
            gps: Some((1.29, 103.85)),
            ..Default::default()
        };
        let fresh = || {
            let mut a = Asset::new("a", "/t/IMG_1_edited.heic".into(), AssetKind::Photo);
            a.mtime = 2_000_000_000;
            a
        };
        // No EXIF date: the library's date, offset and place, not mtime.
        let mut a = fresh();
        fill_photo_capture(&mut a, &ImageExif::default(), Some(&lib));
        assert_eq!(a.taken_at, Some(1_793_066_738));
        assert!(!a.time_guessed);
        assert_eq!(a.tz_offset_secs, Some(8 * 3600));
        assert_eq!(a.gps, Some((1.29, 103.85)));
        // EXIF wins for each field it has.
        let exif = ImageExif {
            taken_at: Some(1_700_000_000),
            tz_offset_secs: Some(9 * 3600),
            gps: Some((35.0, 139.0)),
            ..Default::default()
        };
        let mut a = fresh();
        fill_photo_capture(&mut a, &exif, Some(&lib));
        assert_eq!(a.taken_at, Some(1_700_000_000));
        assert_eq!(a.tz_offset_secs, Some(9 * 3600));
        assert_eq!(a.gps, Some((35.0, 139.0)));
        // EXIF date without GPS: the library fills only the place.
        let dated = ImageExif {
            taken_at: Some(1_700_000_000),
            ..Default::default()
        };
        let mut a = fresh();
        fill_photo_capture(&mut a, &dated, Some(&lib));
        assert_eq!(a.taken_at, Some(1_700_000_000));
        assert_eq!(a.tz_offset_secs, None);
        assert_eq!(a.gps, Some((1.29, 103.85)));
        // Nothing anywhere: mtime, guessed.
        let mut a = fresh();
        fill_photo_capture(&mut a, &ImageExif::default(), None);
        assert_eq!(a.taken_at, Some(2_000_000_000));
        assert!(a.time_guessed);
        assert_eq!(a.gps, None);
    }
}
