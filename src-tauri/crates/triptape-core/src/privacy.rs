//! Privacy: EXIF strip on export, home fuzzing, pairing-UUID hygiene, and
//! redaction for the local log.
//!
//! - Exports strip EXIF (`-map_metadata -1`): GPS, camera serial, and the
//!   Live Photo pairing UUID go away together, so a shared still cannot be
//!   quietly re-linked to its motion file.
//! - Map routes fuzz the first/last ~500 m near home (toggleable).
//!
//! What an export file may carry (PLAN.md Sec 14), per kind:
//!
//! - Film and reel MP4s, reel cover JPEGs: nothing copied from a source
//!   file. Every FFmpeg command that writes one ends with
//!   [`exif_strip_args`], so container and stream tags (GPS `©xyz` and
//!   ISO 6709 location, make/model/lens, serials, `creation_time`, Apple
//!   `com.apple.quicktime.*` keys including `content.identifier`) and
//!   chapters are dropped. Rotation is not metadata here: the filters
//!   apply it to the pixels, so the file plays upright with no matrix.
//! - Book page JPEGs and `cover.jpg`: re-encoded from decoded pixels by the
//!   `image` crate, which writes no EXIF, XMP or MakerNote at all.
//! - `proof.pdf` / `press.pdf`: Typst embeds those clean JPEGs; the PDF
//!   info carries only Typst's own producer and the export date.
//! - Dates: the only capture dates in an export are the ones the owner sees
//!   on screen or on the page (title-card range, chapter datelines). No
//!   file carries a source's original capture timestamp as metadata.

use crate::geo::{bearing_deg, haversine_m, project};

/// Output args that strip all container/stream metadata and chapters from
/// an FFmpeg output. FFmpeg otherwise copies the first input's tags, and
/// that includes a phone video's `©xyz` GPS location.
pub fn exif_strip_args() -> Vec<String> {
    ["-map_metadata", "-1", "-map_chapters", "-1"]
        .map(String::from)
        .to_vec()
}

/// Radius (m) around home inside which route points are fuzzed.
pub const HOME_FUZZ_METERS: f64 = 500.0;

/// Push every route point within [`HOME_FUZZ_METERS`] of `home` out along
/// its bearing from home to exactly that radius. Deterministic and stable:
/// points already outside the radius are untouched.
pub fn fuzz_home_route(route: &[(f64, f64)], home: (f64, f64)) -> Vec<(f64, f64)> {
    route.iter().map(|&p| fuzz_point(p, home)).collect()
}

fn fuzz_point(p: (f64, f64), home: (f64, f64)) -> (f64, f64) {
    let d = haversine_m(p, home);
    if d >= HOME_FUZZ_METERS {
        return p;
    }
    // Exactly at home has no bearing: step due north to the ring.
    let bearing = if d < 1e-9 { 0.0 } else { bearing_deg(home, p) };
    project(home, bearing, HOME_FUZZ_METERS)
}

/// True when the point needs fuzzing (inside the home radius).
pub fn needs_fuzz(p: (f64, f64), home: (f64, f64)) -> bool {
    haversine_m(p, home) < HOME_FUZZ_METERS
}

// ---------------------------------------------------------------------------
// Log redaction (Sec 14 "Local diagnostics"): applied at write time.
// ---------------------------------------------------------------------------

/// Extensions of the owner's photos and clips (and their sidecars): their
/// names become short hashes in logs.
const MEDIA_EXTS: &[&str] = &[
    "jpg", "jpeg", "heic", "heif", "png", "webp", "tif", "tiff", "dng", "arw", "cr2", "cr3", "nef",
    "raf", "orf", "rw2", "gif", "avif", "mov", "mp4", "m4v", "avi", "mts", "m2ts", "3gp", "mkv",
    "xmp", "aae", "srt", "pdf",
];

/// Path parts that say nothing about the owner: system and app folders,
/// the app's own files and the media tools. Everything else is hashed.
const PLAIN_PARTS: &[&str] = &[
    "users",
    "home",
    "volumes",
    "applications",
    "library",
    "caches",
    "application support",
    "logs",
    "app.triptape.desktop",
    "triptape",
    "triptape.app",
    "contents",
    "macos",
    "resources",
    "licenses",
    "binaries",
    "opt",
    "homebrew",
    "bin",
    "sbin",
    "usr",
    "local",
    "tmp",
    "private",
    "var",
    "folders",
    "program files",
    "program files (x86)",
    "appdata",
    "roaming",
    "temp",
    "segments",
    "thumb",
    "proxy",
    "leases",
    ".work",
    "stage",
    "video",
    "reels",
    "book",
    "movies",
    "videos",
    "pictures",
    "downloads",
    "desktop",
    "documents",
    "target",
    "debug",
    "release",
    "hero.mp4",
    "hero.srt",
    "hero.edl.json",
    "export.json",
    "cover.jpg",
    "proof.pdf",
    "press.pdf",
    "guide.txt",
    "triptape.log",
    "settings.json",
    "triptape.sqlite3",
    "ffmpeg",
    "ffprobe",
    "typst",
    "ffmpeg.exe",
    "ffprobe.exe",
    "typst.exe",
];

/// Where the path starting at `i` ends. Quotes, `<>|`, and `": "`, `", "`,
/// `"; "` end it (an apostrophe inside a word does not). A space ends it
/// too, unless a folder separator follows before the next hard end
/// (`My Trip/IMG_1.HEIC`) or the next words finish a media file name
/// (`IMG_0144 (1).HEIC`).
fn path_end(s: &[char], i: usize) -> usize {
    let hard_end = |k: usize| {
        let c = s[k];
        let next = s.get(k + 1);
        matches!(c, '"' | '`' | '<' | '>' | '|')
            || (c == '\'' && next.is_none_or(|n| !n.is_alphanumeric()))
            || (matches!(c, ':' | ',' | ';') && next.is_none_or(|n| *n == ' '))
    };
    let mut end = i;
    while end < s.len() && !hard_end(end) {
        if s[end] == ' ' {
            let stop = (end..s.len()).find(|&k| hard_end(k)).unwrap_or(s.len());
            let ahead = &s[end..stop];
            if !ahead.iter().any(|c| matches!(c, '/' | '\\')) {
                // Extend to the end of the first word that is a media name.
                let text: String = ahead.iter().collect();
                let mut at = 0;
                let mut media_end = None;
                for word in text.split(' ') {
                    at += word.chars().count() + 1;
                    let w = word.trim_end_matches(['.', ')', ']']);
                    if media_ext(w).is_some() {
                        media_end = Some(end + at - 1 - (word.len() - w.len()));
                        break;
                    }
                }
                return media_end.unwrap_or(end);
            }
        }
        end += 1;
    }
    end
}

/// Turns log text into something safe to share: the home folder becomes
/// `~`, every other folder and file name of the owner's becomes a stable
/// short hash (`<dir:1a2b3c4d>`, `<file:5e6f7a8b>.heic`), coordinates
/// become `<gps>`, and registered secrets (captions, names) become
/// `<text>`. Hashes are salted per install, so one file reads the same
/// throughout a report but cannot be looked up from a list of likely names.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    home: Option<String>,
    salt: String,
    /// Longest first, so a caption containing another is replaced whole.
    secrets: Vec<String>,
}

impl Redactor {
    pub fn new(home: Option<&std::path::Path>, salt: &str) -> Self {
        let home = home
            .map(|h| {
                h.to_string_lossy()
                    .trim_end_matches(['/', '\\'])
                    .to_string()
            })
            .filter(|h| h.len() > 1);
        Self {
            home,
            salt: salt.to_string(),
            secrets: vec![],
        }
    }

    /// Never write `text` (a caption, a person's or place's name) to a log.
    /// Matching ignores case. Very short strings are ignored: they would
    /// blank out ordinary words.
    pub fn add_secret(&mut self, text: &str) {
        let t = text.trim();
        if t.chars().count() < 3 || self.secrets.iter().any(|s| s.eq_ignore_ascii_case(t)) {
            return;
        }
        self.secrets.push(t.to_string());
        self.secrets
            .sort_by_key(|s| std::cmp::Reverse(s.chars().count()));
    }

    /// `<kind:8 hex>` for `name`, stable for this salt.
    pub fn token(&self, kind: &str, name: &str) -> String {
        let mut h = blake3::Hasher::new();
        h.update(self.salt.as_bytes());
        h.update(b"\0");
        h.update(name.to_lowercase().as_bytes());
        format!("<{kind}:{}>", &h.finalize().to_hex()[..8])
    }

    /// One redacted log line. Control characters become spaces, so a
    /// crafted file name cannot forge extra lines.
    pub fn redact(&self, text: &str) -> String {
        let one_line: String = text
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let no_secrets = self.redact_secrets(&one_line);
        let no_paths = self.redact_paths(&no_secrets);
        let no_names = self.redact_bare_names(&no_paths);
        redact_gps(&no_names)
    }

    fn redact_secrets(&self, text: &str) -> String {
        let mut out = text.to_string();
        for s in &self.secrets {
            out = replace_ignore_case(&out, s, "<text>");
        }
        out
    }

    fn redact_paths(&self, text: &str) -> String {
        let chars: Vec<char> = text.chars().collect();
        let home: Option<Vec<char>> = self.home.as_ref().map(|h| h.chars().collect());
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        while i < chars.len() {
            let boundary = i == 0
                || matches!(
                    chars[i - 1],
                    ' ' | '"' | '\'' | '(' | '[' | '=' | ':' | ',' | '`' | '/'
                );
            // The home folder itself, not a longer name starting the same way.
            let at_home = home.as_ref().is_some_and(|h| {
                chars[i..].starts_with(h)
                    && chars
                        .get(i + h.len())
                        .is_none_or(|c| !c.is_alphanumeric() && !matches!(c, '_' | '-' | '.'))
            });
            if !(at_home || (boundary && starts_path(&chars[i..]))) {
                out.push(chars[i]);
                i += 1;
                continue;
            }
            let mut end = path_end(&chars, i);
            // Sentence punctuation after a path stays outside it.
            while end > i + 1 && matches!(chars[end - 1], '.' | ')' | ']' | ' ') {
                end -= 1;
            }
            let path: String = chars[i..end].iter().collect();
            out.push_str(&self.redact_path(&path, at_home));
            i = end;
        }
        out
    }

    fn redact_path(&self, path: &str, at_home: bool) -> String {
        let (lead, rest) = match &self.home {
            Some(h) if at_home => ("~".to_string(), &path[h.len()..]),
            _ => {
                let root_len = if path.starts_with("~/") || path.starts_with("~\\") {
                    1
                } else if path.as_bytes().get(1) == Some(&b':') {
                    2
                } else {
                    0
                };
                (path[..root_len].to_string(), &path[root_len..])
            }
        };
        let parts: Vec<&str> = rest.split(['/', '\\']).collect();
        let last = parts.len().saturating_sub(1);
        let mut out = lead;
        for (n, part) in parts.iter().enumerate() {
            if n > 0 {
                out.push('/');
            }
            if !part.is_empty() {
                out.push_str(&self.redact_part(part, n == last));
            }
        }
        out
    }

    fn redact_part(&self, part: &str, is_last: bool) -> String {
        let lower = part.to_lowercase();
        if PLAIN_PARTS.contains(&lower.as_str())
            || ["ffmpeg-", "ffprobe-", "typst-"]
                .iter()
                .any(|p| lower.starts_with(p))
        {
            return part.to_string();
        }
        match media_ext(part) {
            Some(ext) => format!("{}.{ext}", self.token("file", part)),
            None if is_last && part.contains('.') => {
                let ext = part.rsplit('.').next().unwrap_or_default();
                if (1..=5).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
                    format!("{}.{}", self.token("file", part), ext.to_lowercase())
                } else {
                    self.token("file", part)
                }
            }
            None => self.token("dir", part),
        }
    }

    /// A media file name on its own (no folder): hashed like one in a path.
    fn redact_bare_names(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut word = String::new();
        for c in text.chars() {
            if c.is_whitespace() || matches!(c, '"' | '\'' | '(' | '[' | '=' | '/' | '\\' | '`') {
                self.flush_word(&mut word, &mut out);
                out.push(c);
            } else {
                word.push(c);
            }
        }
        self.flush_word(&mut word, &mut out);
        out
    }

    fn flush_word(&self, word: &mut String, out: &mut String) {
        let trimmed = word.trim_end_matches(['.', ',', ';', ':', ')', ']']);
        match media_ext(trimmed).filter(|_| !trimmed.contains('<')) {
            Some(ext) => {
                let tail = &word[trimmed.len()..];
                out.push_str(&self.token("file", trimmed));
                out.push('.');
                out.push_str(&ext);
                out.push_str(tail);
            }
            None => out.push_str(word),
        }
        word.clear();
    }
}

/// A path starts here: `/x`, `~/x`, `C:\x`, `C:/x` or `\\server`.
fn starts_path(s: &[char]) -> bool {
    match s {
        ['/', c, ..] => c.is_alphanumeric() || matches!(c, '.' | '_' | '~' | '/'),
        ['~', '/' | '\\', ..] => true,
        [d, ':', '\\' | '/', ..] => d.is_ascii_alphabetic(),
        ['\\', '\\', c, ..] => c.is_alphanumeric(),
        _ => false,
    }
}

/// The lowercase extension of `name` when it is one of the owner's media.
fn media_ext(name: &str) -> Option<String> {
    let (stem, ext) = name.rsplit_once('.')?;
    let ext = ext.to_lowercase();
    (!stem.is_empty() && MEDIA_EXTS.contains(&ext.as_str())).then_some(ext)
}

fn replace_ignore_case(text: &str, needle: &str, with: &str) -> String {
    let lower_text = text.to_lowercase();
    let lower_needle = needle.to_lowercase();
    // Lowercasing can change byte lengths outside ASCII; then fall back to
    // an exact match rather than slicing at a wrong offset.
    if lower_text.len() != text.len() || lower_needle.len() != needle.len() {
        return text.replace(needle, with);
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some(pos) = lower_text[at..].find(&lower_needle) {
        out.push_str(&text[at..at + pos]);
        out.push_str(with);
        at += pos + needle.len();
    }
    out.push_str(&text[at..]);
    out
}

/// Replace coordinates with `<gps>`: decimal pairs (`35.3192, 139.5522`,
/// ISO 6709 `+35.3192+139.5522/`), degree-minute-second positions, and
/// any number right after a `lat`/`lon`/`gps`/`home` key.
pub fn redact_gps(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if let Some(len) = gps_at(&chars, i) {
            out.push_str("<gps>");
            i += len;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// A signed decimal at `i`: (chars consumed, value, fractional digits).
fn decimal_at(s: &[char], i: usize) -> Option<(usize, f64, usize)> {
    let mut j = i;
    if matches!(s.get(j), Some('+' | '-')) {
        j += 1;
    }
    let int_start = j;
    while s.get(j).is_some_and(char::is_ascii_digit) {
        j += 1;
    }
    if j == int_start || j - int_start > 3 {
        return None;
    }
    let mut frac = 0;
    if s.get(j) == Some(&'.') && s.get(j + 1).is_some_and(char::is_ascii_digit) {
        j += 1;
        while s.get(j).is_some_and(char::is_ascii_digit) {
            j += 1;
            frac += 1;
        }
    }
    let v: f64 = s[i..j].iter().collect::<String>().parse().ok()?;
    Some((j - i, v, frac))
}

/// The word (letters and `_`) ending just before `i`, skipping `= : "` and
/// spaces, lowercased.
fn key_before(s: &[char], i: usize) -> String {
    let mut k = i;
    while k > 0 && matches!(s[k - 1], ' ' | '=' | ':' | '"') {
        k -= 1;
    }
    if k == i {
        return String::new();
    }
    let start = (0..k)
        .rev()
        .take_while(|&n| s[n].is_ascii_alphabetic() || s[n] == '_')
        .last()
        .unwrap_or(k);
    s[start..k].iter().collect::<String>().to_lowercase()
}

fn gps_at(s: &[char], i: usize) -> Option<usize> {
    // Inside a longer number or word: not a coordinate start.
    if i > 0 && (s[i - 1].is_ascii_alphanumeric() || s[i - 1] == '.') {
        return None;
    }
    let (n1, a, f1) = decimal_at(s, i)?;
    // Degrees, minutes, seconds: 35 deg 19' 9.12" N
    if f1 == 0 && s[i + n1..].starts_with(&[' ', 'd', 'e', 'g']) {
        let mut j = i + n1 + 4;
        while j < s.len() && (s[j].is_ascii_digit() || matches!(s[j], ' ' | '\'' | '"' | '.')) {
            j += 1;
        }
        if matches!(s.get(j), Some('N' | 'S' | 'E' | 'W')) {
            j += 1;
        }
        return Some(j - i);
    }
    // Any decimal after a lat/lon/gps/home key, with a second one after it.
    let key = key_before(s, i);
    let keyed = [
        "lat",
        "lon",
        "lng",
        "latitude",
        "longitude",
        "gps",
        "home",
        "location",
    ]
    .iter()
    .any(|w| key.ends_with(w));
    if keyed && f1 > 0 {
        let mut j = i + n1;
        while matches!(s.get(j), Some(',' | ' ')) {
            j += 1;
        }
        return match decimal_at(s, j) {
            Some((n2, _, f2)) if f2 > 0 => Some(j + n2 - i),
            _ => Some(n1),
        };
    }
    // A pair of decimals, each with 3+ places, in latitude/longitude range.
    if f1 < 3 || a.abs() > 90.0 {
        return None;
    }
    let mut j = i + n1;
    if !matches!(s.get(j), Some('+' | '-')) {
        if !matches!(s.get(j), Some(',' | ' ' | '/')) {
            return None;
        }
        while matches!(s.get(j), Some(',' | ' ' | '/')) {
            j += 1;
        }
    }
    let (n2, b, f2) = decimal_at(s, j)?;
    if f2 < 3 || b.abs() > 180.0 {
        return None;
    }
    let mut end = j + n2;
    if s.get(end) == Some(&'/') {
        end += 1;
    }
    Some(end - i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_args_cover_metadata() {
        let a = exif_strip_args();
        assert!(a.contains(&"-map_metadata".to_string()));
        assert!(a.contains(&"-1".to_string()));
        assert!(a.contains(&"-map_chapters".to_string()));
    }

    fn redactor() -> Redactor {
        let mut r = Redactor::new(Some(std::path::Path::new("/Users/jane")), "salt");
        r.add_secret("Grandma's 80th at the lake");
        r.add_secret("Alice Example");
        r
    }

    #[test]
    fn redacts_home_folders_and_file_names() {
        let r = redactor();
        let line = r.redact(
            "decode failed for /Users/jane/Downloads/Japan to Singapore 2026/IMG_0144 (1).HEIC: no frame",
        );
        assert!(
            line.starts_with("decode failed for ~/Downloads/<dir:"),
            "{line}"
        );
        assert!(line.ends_with(".heic: no frame"), "{line}");
        for leak in ["jane", "Japan", "Singapore", "IMG_0144"] {
            assert!(!line.contains(leak), "{leak} in {line}");
        }
        // Stable: the same file gives the same token.
        let again = r.redact("/Users/jane/Downloads/Japan to Singapore 2026/IMG_0144 (1).HEIC");
        assert!(line.contains(&again), "{line} vs {again}");
        // Another install's salt gives another token.
        let other = Redactor::new(Some(std::path::Path::new("/Users/jane")), "pepper");
        assert_ne!(
            other.token("file", "IMG_0144.HEIC"),
            r.token("file", "IMG_0144.HEIC")
        );
        // A neighbour of the home folder is not mistaken for it.
        let janet = r.redact("/Users/janet/a.mov");
        assert!(janet.starts_with("/Users/<dir:"), "{janet}");
    }

    #[test]
    fn keeps_tool_and_app_paths_readable() {
        let r = redactor();
        assert_eq!(
            r.redact("ffmpeg: /opt/homebrew/bin/ffmpeg exited 1"),
            "ffmpeg: /opt/homebrew/bin/ffmpeg exited 1"
        );
        assert_eq!(
            r.redact("cache /Users/jane/Library/Caches/app.triptape.desktop/segments"),
            "cache ~/Library/Caches/app.triptape.desktop/segments"
        );
        let win = r.redact(r"open C:\Users\bob\Pictures\Kids\IMG_1.JPG.");
        assert!(win.starts_with("open C:/Users/<dir:"), "{win}");
        assert!(win.ends_with(".jpg."), "{win}");
        assert!(!win.contains("bob") && !win.contains("Kids"), "{win}");
    }

    #[test]
    fn adversarial_strings_never_leak() {
        let r = redactor();
        let cases = [
            // Captions and names, in any case, inside other text.
            "caption GRANDMA'S 80TH AT THE LAKE burned in",
            "people: alice example, Bob",
            // A forged second log line.
            "ok\n2026-01-01T00:00:00Z INFO /Users/jane/secret.mov",
            // Bare file names, quoted and with punctuation.
            "reel 'Alice Example.mov' failed; IMG_0001.HEIC, IMG_0002.mov.",
            // Coordinates in every shape the pipeline produces.
            "home=35.3192,139.5522 gps (35.3192, 139.5522) loc +35.3192+139.5522/",
            "GPSLatitude 35 deg 19' 9.02\" N lat: -33.8688 lon -151.2093",
            // The home folder alone, and inside a file URL.
            "/Users/jane file:///Users/jane/Movies/Trip/hero.mp4",
            // A Windows share and a forward-slashed drive path.
            r"\\nas\family\Trip\IMG_9.HEIC and D:/Trips/Bali/clip.MP4",
            // A path with a quote in a folder name.
            "open \"/Volumes/USB/Mom's Trip/IMG_7.HEIC\"",
        ];
        let banned = [
            "Grandma", "GRANDMA", "alice", "Alice", "jane", "secret", "IMG_0001", "IMG_0002",
            "35.3192", "139.5522", "33.8688", "151.2093", "9.02", "Trip/", "family", "Bali",
            "Trips", "IMG_9", "nas", "IMG_7", "USB",
        ];
        for case in cases {
            let line = r.redact(case);
            assert!(!line.contains('\n'), "{line}");
            for b in banned {
                assert!(!line.contains(b), "{b:?} leaked from {case:?}: {line}");
            }
        }
    }

    #[test]
    fn ordinary_facts_survive() {
        let r = redactor();
        let line = "job job_1a2b render-video: ffmpeg exit 1 after 25.5 s, 1920x1080 h264, 3 of 9 picks, v0.1.0";
        assert_eq!(r.redact(line), line);
    }

    #[test]
    fn home_fuzz_pushes_to_ring() {
        let home = (37.7749, -122.4194);
        // ~111 m north of home.
        let near = (37.7759, -122.4194);
        assert!(needs_fuzz(near, home));
        let out = fuzz_home_route(&[near], home);
        let d = haversine_m(out[0], home);
        assert!(
            (d - HOME_FUZZ_METERS).abs() < 5.0,
            "fuzzed point must sit ~500 m out, got {d}"
        );
        // Far points untouched.
        let far = (34.0522, -118.2437);
        assert!(!needs_fuzz(far, home));
        assert_eq!(fuzz_home_route(&[far], home), vec![far]);
        // At home: deterministic push north, also ~500 m.
        let out = fuzz_home_route(&[home], home);
        let d = haversine_m(out[0], home);
        assert!((d - HOME_FUZZ_METERS).abs() < 5.0);
    }
}
