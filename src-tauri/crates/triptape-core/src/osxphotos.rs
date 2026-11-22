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

        // Unprobeable video: the library's date and offset (not mtime),
        // and story fields still land.
        let mut clip = Asset::new("v", dir.path().join("IMG_1.mov"), AssetKind::Video);
        clip.mtime = 2_000_000_001;
        fill_video(&mut clip, None, Some(&idx), "prj");
        assert_eq!(clip.taken_at, Some(1_793_066_738));
        assert!(!clip.time_guessed);
        assert_eq!(clip.tz_offset_secs, Some(8 * 3600));
        assert_eq!(clip.user_caption.as_deref(), Some("River"));
        assert_eq!(clip.people_sig, still.people_sig);

        // A probed video's own dates beat the library: Apple's creation
        // date first, then the container's creation_time.
        let probe = |tags: &str| {
            crate::metadata::FfprobeOutput::parse(&format!(r#"{{"format":{{"tags":{{{tags}}}}}}}"#))
                .unwrap()
        };
        let mut clip = Asset::new("v", dir.path().join("IMG_1.mov"), AssetKind::Video);
        let p = probe(r#""creation_time":"2026-10-15T19:19:09.000000Z""#);
        fill_video(&mut clip, Some(&p), Some(&idx), "prj");
        assert_eq!(clip.taken_at, Some(1_792_091_949));
        assert_eq!(clip.tz_offset_secs, None);
        let p = probe(
            r#""creation_time":"2026-10-15T19:19:09.000000Z","com.apple.quicktime.creationdate":"2026-10-14T13:30:00+0900""#,
        );
        fill_video(&mut clip, Some(&p), Some(&idx), "prj");
        assert_eq!(clip.taken_at, Some(1_791_952_200));
        assert_eq!(clip.tz_offset_secs, Some(9 * 3600));
        // A probe with no date at all falls to the library.
        fill_video(&mut clip, Some(&probe("")), Some(&idx), "prj");
        assert_eq!(clip.taken_at, Some(1_793_066_738));
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

/// How far apart (seconds) a library photo and a trip file may be dated
/// and still be the same shot.
const KNOWN_FACE_MATCH_SECS: i64 = 120;

/// The file stem a trip file shares with its library original:
/// `IMG_1208 (1).HEIC` → `img_1208`. `None` for an `_edited` file, whose
/// crop may differ from the original the regions describe.
fn library_stem(path: &std::path::Path) -> Option<String> {
    let stem = path.file_stem()?.to_string_lossy().to_lowercase();
    if stem.ends_with("_edited") {
        return None;
    }
    let base = match stem.rsplit_once(" (") {
        Some((b, n))
            if n.ends_with(')') && n[..n.len() - 1].bytes().all(|c| c.is_ascii_digit()) =>
        {
            b.to_string()
        }
        _ => stem,
    };
    Some(base)
}

/// Named people's regions per trip asset, from `osxphotos query --json`
/// output (Apple Photos' face tags). A library photo matches a trip file
/// with the same file stem taken within [`KNOWN_FACE_MATCH_SECS`]. Faces
/// with a name become [`KnownFace`](crate::types::KnownFace)s (unnamed
/// ones are strangers to the library); a named person with only a body
/// region gives that body's box, marked `body`. Regions are in the upright
/// photo, top-left origin, as Photos stores them.
pub fn known_faces_from_osxphotos(
    json: &str,
    assets: &[crate::types::Asset],
) -> Result<std::collections::HashMap<String, Vec<crate::types::KnownFace>>, String> {
    use crate::types::KnownFace;
    let items: Vec<serde_json::Value> = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let mut by_stem: std::collections::HashMap<String, Vec<&crate::types::Asset>> =
        std::collections::HashMap::new();
    for a in assets {
        if let Some(s) = library_stem(&a.path) {
            by_stem.entry(s).or_default().push(a);
        }
    }
    let num = |v: &serde_json::Value, k: &str| {
        v.get(k).and_then(serde_json::Value::as_f64).unwrap_or(0.0) as f32
    };
    let mut out = std::collections::HashMap::new();
    for item in &items {
        let Some(name) = item.get("original_filename").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(when) = item
            .get("date")
            .and_then(|v| v.as_str())
            .and_then(|d| d.parse::<jiff::Timestamp>().ok())
        else {
            continue;
        };
        let stem = library_stem(std::path::Path::new(name)).unwrap_or_default();
        let Some(files) = by_stem.get(&stem) else {
            continue;
        };
        let mut faces: Vec<KnownFace> = vec![];
        for f in item
            .get("face_info")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            let person = f.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if person.is_empty() || f.get("is_pet").and_then(|v| v.as_bool()) == Some(true) {
                continue;
            }
            let area = f.get("mwg_rs_area").cloned().unwrap_or_default();
            let (w, h) = (num(&area, "w"), num(&area, "h"));
            if w > 0.0 && h > 0.0 {
                // MWG regions: centre x, y and size, top-left origin.
                faces.push(KnownFace {
                    name: person.to_string(),
                    x: num(&area, "x") - w / 2.0,
                    y: num(&area, "y") - h / 2.0,
                    w,
                    h,
                    body: false,
                });
            } else {
                let (bw, bh) = (num(f, "body_width"), num(f, "body_height"));
                if bw > 0.0 && bh > 0.0 {
                    // Body centres count from the bottom edge.
                    let (cx, cy) = (num(f, "body_center_x"), 1.0 - num(f, "body_center_y"));
                    faces.push(KnownFace {
                        name: person.to_string(),
                        x: cx - bw / 2.0,
                        y: cy - bh / 2.0,
                        w: bw,
                        h: bh,
                        body: true,
                    });
                }
            }
        }
        if faces.is_empty() {
            continue;
        }
        let orientation = item
            .get("orientation")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1);
        for f in &mut faces {
            upright(f, orientation);
        }
        let t = when.as_second();
        for a in files.iter().filter(|a| {
            a.taken_at
                .is_some_and(|u| (u - t).abs() <= KNOWN_FACE_MATCH_SECS)
        }) {
            // The same shot can sit in the library twice (an import and
            // a synced copy); the one with more people tagged wins.
            if faces.len() > out.get(&a.id).map_or(0, Vec::len) {
                out.insert(a.id.clone(), faces.clone());
            }
        }
    }
    Ok(out)
}

/// Turn a region from the camera's stored frame (where Photos keeps face
/// regions) into the upright photo, by its EXIF `orientation`.
fn upright(f: &mut crate::types::KnownFace, orientation: u64) {
    let map = |x: f32, y: f32| match orientation {
        2 => (1.0 - x, y),
        3 => (1.0 - x, 1.0 - y),
        4 => (x, 1.0 - y),
        5 => (y, x),
        6 => (1.0 - y, x),
        7 => (1.0 - y, 1.0 - x),
        8 => (y, 1.0 - x),
        _ => (x, y),
    };
    let (ax, ay) = map(f.x, f.y);
    let (bx, by) = map(f.x + f.w, f.y + f.h);
    f.x = ax.min(bx);
    f.y = ay.min(by);
    f.w = (ax - bx).abs();
    f.h = (ay - by).abs();
}

#[cfg(test)]
mod known_face_tests {
    use super::*;
    use crate::types::{Asset, AssetKind};

    #[test]
    fn library_faces_reach_every_copy_of_the_shot() {
        let mut a = Asset::new("a", "/t/IMG_1208 (1).HEIC".into(), AssetKind::Photo);
        a.taken_at = Some(1_791_865_165);
        let mut b = Asset::new("b", "/t/IMG_1208.HEIC".into(), AssetKind::Photo);
        b.taken_at = a.taken_at;
        let mut old = Asset::new("c", "/old/IMG_1208.JPG".into(), AssetKind::Photo);
        old.taken_at = Some(1_300_000_000);
        let edited = Asset::new("d", "/t/IMG_1208_edited.heic".into(), AssetKind::Photo);
        let json = r#"[{"original_filename":"IMG_1208.HEIC","date":"2026-10-13T13:19:25.336000+09:00",
          "face_info":[
            {"name":"Lana","is_pet":false,"mwg_rs_area":{"x":0.157,"y":0.631,"w":0.1,"h":0.135}},
            {"name":"","is_pet":false,"mwg_rs_area":{"x":0.5,"y":0.5,"w":0.1,"h":0.1}},
            {"name":"Ailan","is_pet":false,"mwg_rs_area":{"x":0,"y":1,"w":0,"h":0},
             "body_center_x":0.732,"body_center_y":0.532,"body_width":0.162,"body_height":0.497}]}]"#;
        let faces = known_faces_from_osxphotos(json, &[a, b, old, edited]).unwrap();
        assert_eq!(faces.len(), 2, "both copies, not the 2010 file or the edit");
        let lana = &faces["a"][0];
        assert_eq!(lana.name, "Lana");
        assert!((lana.x - 0.107).abs() < 1e-3 && (lana.y - 0.5635).abs() < 1e-3);
        assert_eq!(faces["a"].len(), 2, "the stranger is skipped");
        let ailan = &faces["a"][1];
        assert!(ailan.body && (ailan.y - (0.468 - 0.2485)).abs() < 1e-3);
    }

    #[test]
    fn a_portrait_shot_turns_its_regions_upright() {
        // Orientation 6: a face left of centre in the stored frame sits
        // near the top of the upright photo.
        let mut f = crate::types::KnownFace {
            name: "Lana".into(),
            x: 0.183,
            y: 0.477,
            w: 0.087,
            h: 0.116,
            body: false,
        };
        upright(&mut f, 6);
        assert!((f.x - 0.407).abs() < 1e-3 && (f.y - 0.183).abs() < 1e-3);
        assert!((f.w - 0.116).abs() < 1e-3 && (f.h - 0.087).abs() < 1e-3);
    }
}
