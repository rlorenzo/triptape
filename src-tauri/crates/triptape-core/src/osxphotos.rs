//! Story fields from an `osxphotos export` folder's own bookkeeping
//! database (`.osxphotos_export.db`), for exports written without XMP
//! sidecars: the user's caption, who is tagged, Apple's aesthetic score,
//! and capture date/offset/place, keyed by the exported file's path.
//!
//! The database belongs to the user's export folder: it is opened strictly
//! read-only and immutable, so SQLite never writes a journal, `-wal` or
//! `-shm` file next to it. Every failure (missing file, unexpected schema,
//! bad JSON) degrades to "no index" or skips that row; it never fails a
//! scan.

use crate::metadata::PeopleTags;
use rusqlite::{Connection, OpenFlags};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};

/// File name osxphotos writes at the root of an export folder.
pub const EXPORT_DB_NAME: &str = ".osxphotos_export.db";

/// Name osxphotos uses for a detected but unnamed face.
const UNKNOWN_PERSON: &str = "_UNKNOWN_";

/// What the photo library knows about one exported file.
#[derive(Clone, Default, PartialEq)]
pub struct LibraryEntry {
    /// The user's caption (`None` when empty).
    pub description: Option<String>,
    /// Tagged people; unnamed faces appear as `"_UNKNOWN_"`. Personal
    /// data: read only through [`LibraryEntry::people`], never stored.
    pub persons: Vec<String>,
    /// Apple's overall aesthetic score (`score.overall`, roughly -1..1).
    pub overall: Option<f32>,
    /// Capture instant (`date`), unix seconds, floored.
    pub taken_at: Option<i64>,
    /// UTC offset at capture (from `date`), seconds east of UTC.
    pub tz_offset_secs: Option<i32>,
    /// `(latitude, longitude)` when both are known and in range.
    pub gps: Option<(f64, f64)>,
}

impl std::fmt::Debug for LibraryEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LibraryEntry")
            .field("description", &self.description.is_some())
            .field("persons", &self.persons.len())
            .field("overall", &self.overall)
            .field("taken_at", &self.taken_at)
            .field("tz_offset_secs", &self.tz_offset_secs)
            .field("gps", &self.gps.is_some())
            .finish()
    }
}

impl LibraryEntry {
    /// The tagged people: `"_UNKNOWN_"` (and blank) entries count as
    /// unnamed faces. `None` when nobody is tagged.
    pub fn people(&self) -> Option<PeopleTags> {
        let is_unnamed = |p: &&String| p.trim().is_empty() || p.trim() == UNKNOWN_PERSON;
        let unnamed = self.persons.iter().filter(is_unnamed).count();
        PeopleTags::new(
            self.persons.iter().filter(|p| !is_unnamed(p)).cloned(),
            u32::try_from(unnamed).unwrap_or(u32::MAX),
        )
    }
}

/// Every exported file's [`LibraryEntry`], keyed by its path relative to
/// the export folder (`/`-separated, lowercased).
#[derive(Debug, Clone, Default)]
pub struct OsxPhotosIndex {
    roots: Vec<PathBuf>,
    entries: HashMap<String, LibraryEntry>,
}

impl OsxPhotosIndex {
    /// Load `<folder>/.osxphotos_export.db`. `None` when the file is
    /// missing, unreadable, not an osxphotos database, or maps no files.
    pub fn load(folder: &Path) -> Option<Self> {
        let canon = folder.canonicalize().ok()?;
        let db_path = canon.join(EXPORT_DB_NAME);
        if !db_path.is_file() {
            return None;
        }
        let conn = open_immutable(&db_path).ok()?;
        let entries = read_entries(&conn).ok()?;
        if entries.is_empty() {
            return None;
        }
        let mut roots = vec![folder.to_path_buf()];
        if canon != folder {
            roots.push(canon);
        }
        Some(Self { roots, entries })
    }

    /// The entry for `path` (absolute, or relative to the same base the
    /// folder was given with), when it lies under the export folder.
    pub fn get(&self, path: &Path) -> Option<&LibraryEntry> {
        let rel = self.roots.iter().find_map(|r| path.strip_prefix(r).ok())?;
        self.entries.get(&key(rel)?)
    }
}

/// Lookup key for a path relative to the export folder.
fn key(rel: &Path) -> Option<String> {
    let mut parts = vec![];
    for c in rel.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_str()?.to_lowercase()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Normalize a stored `export_data.filepath` the same way as [`key`].
fn stored_key(filepath: &str) -> Option<String> {
    key(Path::new(&filepath.replace('\\', "/")))
}

/// Open `path` (absolute) read-only as an immutable database: SQLite then
/// takes no locks and creates no journal, `-wal` or `-shm` files beside it.
fn open_immutable(path: &Path) -> rusqlite::Result<Connection> {
    let uri = format!(
        "file:{}?mode=ro&immutable=1",
        percent_encode(&path.to_string_lossy())
    );
    Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
}

/// Percent-encode a filesystem path for a SQLite `file:` URI, keeping
/// unreserved characters and `/` as-is.
fn percent_encode(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn read_entries(conn: &Connection) -> rusqlite::Result<HashMap<String, LibraryEntry>> {
    let mut st = conn.prepare(
        "SELECT e.filepath, p.uuid, p.photoinfo FROM export_data e JOIN photoinfo p ON p.uuid = e.uuid",
    )?;
    let rows = st.query_map([], |r| {
        Ok((
            r.get::<_, Option<String>>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    // One photo exports as several files (still + Live Photo motion,
    // original + edit): parse its JSON once per uuid, not once per file.
    let mut parsed: HashMap<String, Option<LibraryEntry>> = HashMap::new();
    let mut out = HashMap::new();
    for row in rows {
        // A bad row (wrong type, bad JSON) is skipped, never fatal.
        let Ok((Some(filepath), uuid, Some(json))) = row else {
            continue;
        };
        let Some(k) = stored_key(&filepath) else {
            continue;
        };
        let entry = parsed.entry(uuid).or_insert_with(|| parse_photoinfo(&json));
        if let Some(entry) = entry {
            out.insert(k, entry.clone());
        }
    }
    Ok(out)
}

/// The fields we use from one `photoinfo.photoinfo` JSON document.
fn parse_photoinfo(json: &str) -> Option<LibraryEntry> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let description = v
        .get("description")
        .and_then(|d| d.as_str())
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(str::to_string);
    let persons = v
        .get("persons")
        .and_then(|p| p.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|p| p.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let overall = v
        .get("score")
        .and_then(|s| s.get("overall"))
        .and_then(|o| o.as_f64())
        .filter(|o| o.is_finite())
        .map(|o| o as f32);
    let (taken_at, tz_offset_secs) = v
        .get("date")
        .and_then(|d| d.as_str())
        .and_then(crate::metadata::parse_iso_instant)
        .map_or((None, None), |(t, o)| (Some(t), Some(o)));
    let coord = |k: &str| v.get(k).and_then(|c| c.as_f64()).filter(|c| c.is_finite());
    let gps = coord("latitude")
        .zip(coord("longitude"))
        .and_then(|(lat, lon)| crate::metadata::valid_lat_lon(lat, lon));
    Some(LibraryEntry {
        description,
        persons,
        overall,
        taken_at,
        tz_offset_secs,
        gps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal osxphotos export database in `dir`.
    fn write_db(dir: &Path, rows: &[(&str, &str, &str)]) {
        let conn = Connection::open(dir.join(EXPORT_DB_NAME)).unwrap();
        conn.execute_batch(
            "CREATE TABLE export_data (id INTEGER PRIMARY KEY, filepath TEXT, filepath_normalized TEXT, uuid TEXT);
             CREATE TABLE photoinfo (id INTEGER PRIMARY KEY, uuid TEXT UNIQUE, photoinfo JSON);",
        )
        .unwrap();
        for (path, uuid, info) in rows {
            conn.execute(
                "INSERT INTO export_data (filepath, filepath_normalized, uuid) VALUES (?1, lower(?1), ?2)",
                [path, uuid],
            )
            .unwrap();
            conn.execute(
                "INSERT OR IGNORE INTO photoinfo (uuid, photoinfo) VALUES (?1, ?2)",
                [uuid, info],
            )
            .unwrap();
        }
    }

    fn listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn index_maps_still_and_motion_names() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("Trip with spaces");
        std::fs::create_dir(&folder).unwrap();
        write_db(
            &folder,
            &[
                (
                    "IMG_1.HEIC",
                    "u1",
                    r#"{"description":"Dinner by the river","persons":["Ann","_UNKNOWN_"],"score":{"overall":0.42},"date":"2026-10-27T10:05:38.928000+08:00","latitude":1.29,"longitude":103.85}"#,
                ),
                ("IMG_1 (1).mov", "u1", "{}"),
                (
                    "sub/IMG_2.JPG",
                    "u2",
                    r#"{"description":"","persons":[],"score":{"overall":null}}"#,
                ),
                ("IMG_3.HEIC", "u3", "not json"),
            ],
        );
        let idx = OsxPhotosIndex::load(&folder).unwrap();
        assert_eq!(idx.entries.len(), 3, "the bad-JSON row is skipped");
        let still = idx.get(&folder.join("IMG_1.HEIC")).unwrap();
        assert_eq!(still.description.as_deref(), Some("Dinner by the river"));
        assert_eq!(still.overall, Some(0.42));
        assert_eq!(still.taken_at, Some(1_793_066_738));
        assert_eq!(still.tz_offset_secs, Some(8 * 3600));
        assert_eq!(still.gps, Some((1.29, 103.85)));
        assert_eq!(still.people().map(|p| p.count()), Some(2));
        // Same uuid: the motion half maps to the same library entry; the
        // lookup is case-insensitive.
        let motion = idx.get(&folder.join("img_1 (1).MOV")).unwrap();
        assert_eq!(motion, still);
        let plain = idx.get(&folder.join("sub").join("IMG_2.JPG")).unwrap();
        assert_eq!(plain.description, None);
        assert_eq!(plain.overall, None);
        assert_eq!(plain.people(), None);
        assert!(idx.get(&folder.join("IMG_3.HEIC")).is_none());
        assert!(idx.get(&dir.path().join("IMG_1.HEIC")).is_none());
        // Debug never prints names.
        assert!(!format!("{still:?}").contains("Ann"));
    }

    #[test]
    fn shared_fill_helpers_use_the_library_for_stills_and_videos() {
        use crate::metadata::{fill_photo, fill_video, ImageExif};
        use crate::types::{Asset, AssetKind};
        let dir = tempfile::tempdir().unwrap();
        let info = r#"{"description":"River","persons":["Ann"],"date":"2026-10-27T10:05:38+08:00","latitude":1.29,"longitude":103.85}"#;
        write_db(
            dir.path(),
            &[("IMG_1_edited.HEIC", "u1", info), ("IMG_1.mov", "u1", info)],
        );
        let idx = OsxPhotosIndex::load(dir.path()).unwrap();

        let mut still = Asset::new("s", dir.path().join("IMG_1_edited.HEIC"), AssetKind::Photo);
        still.mtime = 2_000_000_000;
        let exif = ImageExif {
            orientation: Some(6),
            width: Some(40),
            height: Some(30),
            ..Default::default()
        };
        fill_photo(&mut still, &exif, Some(&idx), "prj");
        assert_eq!(still.taken_at, Some(1_793_066_738));
        assert!(!still.time_guessed);
        assert_eq!(still.gps, Some((1.29, 103.85)));
        assert_eq!(still.user_caption.as_deref(), Some("River"));
        assert_eq!(still.people_count, Some(1));
        assert_eq!(still.rotation_deg, 90);
        assert_eq!((still.width, still.height), (Some(40), Some(30)));

        // Unprobeable video: mtime, guessed, but story fields still land.
        let mut clip = Asset::new("v", dir.path().join("IMG_1.mov"), AssetKind::Video);
        clip.mtime = 2_000_000_001;
        fill_video(&mut clip, None, Some(&idx), "prj");
        assert_eq!(clip.taken_at, Some(2_000_000_001));
        assert!(clip.time_guessed);
        assert_eq!(clip.user_caption.as_deref(), Some("River"));
        assert_eq!(clip.people_sig, still.people_sig);
    }

    #[test]
    fn unknown_faces_count_but_are_not_names() {
        let e = |persons: &[&str]| LibraryEntry {
            persons: persons.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        assert_eq!(e(&[]).people(), None);
        let unknown = e(&["_UNKNOWN_", "_UNKNOWN_"]).people().unwrap();
        assert_eq!(unknown.count(), 2);
        // Two unnamed faces are not the same as one named person + one face.
        let mixed = e(&["Ann", "_UNKNOWN_"]).people().unwrap();
        assert_eq!(mixed.count(), 2);
        assert_ne!(unknown.signature("p"), mixed.signature("p"));
        assert!(!format!("{mixed:?}").contains("ann"));
    }

    #[test]
    fn missing_or_corrupt_db_is_no_index() {
        let dir = tempfile::tempdir().unwrap();
        assert!(OsxPhotosIndex::load(dir.path()).is_none());
        std::fs::write(dir.path().join(EXPORT_DB_NAME), b"definitely not sqlite").unwrap();
        assert!(OsxPhotosIndex::load(dir.path()).is_none());
        // A real SQLite file without the osxphotos tables.
        let other = tempfile::tempdir().unwrap();
        Connection::open(other.path().join(EXPORT_DB_NAME))
            .unwrap()
            .execute_batch("CREATE TABLE t (x)")
            .unwrap();
        assert!(OsxPhotosIndex::load(other.path()).is_none());
    }

    #[test]
    fn loading_leaves_no_files_in_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("My Trip #1 %20");
        std::fs::create_dir(&folder).unwrap();
        write_db(&folder, &[("IMG_1.HEIC", "u1", r#"{"persons":["Ann"]}"#)]);
        let before = listing(&folder);
        assert_eq!(before, vec![EXPORT_DB_NAME.to_string()]);
        let idx = OsxPhotosIndex::load(&folder).unwrap();
        assert_eq!(idx.entries.len(), 1);
        assert_eq!(listing(&folder), before);
        drop(idx);
        assert_eq!(listing(&folder), before);
    }

    #[test]
    fn percent_encoding_keeps_paths_unambiguous() {
        assert_eq!(percent_encode("/a b/c?d#e%f"), "/a%20b/c%3Fd%23e%25f");
        assert_eq!(
            stored_key("Sub\\IMG_1.HEIC").as_deref(),
            Some("sub/img_1.heic")
        );
        assert_eq!(stored_key("../x.jpg"), None);
    }
}
