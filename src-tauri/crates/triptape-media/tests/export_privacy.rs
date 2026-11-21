//! Exports carry nothing private from their sources (PLAN.md Sec 14).
//!
//! Real FFmpeg, Typst and the `triptape` CLI against sources tagged the way
//! phones tag them: EXIF GPS, a body serial, owner and lens, the original
//! capture time, a person's name in XMP, and on the videos a `©xyz` GPS
//! location plus the Apple Live Photo `ContentIdentifier` key. Every file
//! an export writes for sharing is then read back, by bytes and with
//! exiftool, and none of it may survive. Needs exiftool to tag the sources
//! (CI installs it); without it the tests say so and pass.

use std::path::{Path, PathBuf};
use std::process::Command;

use triptape_core::process::Toolchain;
use triptape_core::types::{PlanEntry, Rendition};
use triptape_media::render_reels::{render_reels, ReelOptions};
use triptape_media::render_video::ResolvedEntry;

const PAIRING_ID: &str = "A3710788-B6B3-49FC-8627-969E1DFF51F6";
const SERIAL: &str = "SN-PRIV-90210";
const OWNER: &str = "Jane Privtest";
const LENS: &str = "PrivLens 5.7mm";
const PERSON: &str = "Alice Privtest";
/// Home, and where the first day's photos were taken.
const HOME: (f64, f64) = (35.3192, 139.5522);

/// Strings no exported file may contain, in any form.
const SECRETS: [&str; 6] = [PAIRING_ID, SERIAL, OWNER, LENS, PERSON, "+35.3192"];

fn ff() -> &'static str {
    &Toolchain::global().ffmpeg.bin
}

fn have_exiftool() -> bool {
    Command::new("exiftool").arg("-ver").output().is_ok()
}

fn run(cmd: &mut Command) {
    let out = cmd.output().expect("spawn");
    assert!(
        out.status.success(),
        "{cmd:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn exiftool(args: &[&str], file: &Path) {
    run(Command::new("exiftool")
        .args(["-q", "-overwrite_original"])
        .args(args)
        .arg(file));
}

/// A still tagged like a phone photo (the HEIC MakerNote `ContentIdentifier`
/// cannot be written from scratch, so the pairing id rides in
/// `ImageUniqueID` and XMP instead: the bytes check catches it anywhere).
fn tagged_still(path: &Path, seed: usize, day: usize, gps: (f64, f64)) {
    // Phone-sized (prints at book dpi) and laid out differently per seed,
    // so near-duplicate grouping keeps every photo.
    let (bx, by) = ((seed % 6) * 600, (seed / 6) * 450);
    run(Command::new(ff())
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!(
            "color=c=0x{:02x}{:02x}{:02x}:s=4032x3024,\
             drawbox=x={bx}:y={by}:w=1300:h=1000:color=white:t=fill,\
             drawbox=x={}:y={}:w=300:h=300:color=black:t=fill,\
             noise=alls=20:allf=t,format=rgb24",
            seed * 7 % 256,
            255 - seed * 5,
            seed * 13 % 256,
            3400 - bx,
            2500 - by
        ))
        .args(["-frames:v", "1", "-q:v", "6"])
        .arg(path));
    let (lat, lon) = gps;
    exiftool(
        &[
            "-Make=Apple",
            "-Model=iPhone 14",
            &format!("-SerialNumber={SERIAL}"),
            &format!("-OwnerName={OWNER}"),
            &format!("-LensModel={LENS}"),
            &format!("-ImageUniqueID={PAIRING_ID}"),
            &format!("-XMP-dc:Identifier={PAIRING_ID}"),
            &format!("-XMP-iptcExt:PersonInImage={PERSON}"),
            &format!(
                "-DateTimeOriginal=2026:10:0{} {:02}:15:00",
                3 + day,
                9 + seed % 10
            ),
            &format!("-GPSLatitude={lat}"),
            "-GPSLatitudeRef=N",
            &format!("-GPSLongitude={lon}"),
            "-GPSLongitudeRef=E",
        ],
        path,
    );
}

/// A portrait phone clip stored sideways with a 90° display rotation:
/// red on top and blue below once rotated, with `©xyz` GPS and Apple keys.
fn tagged_clip(path: &Path, secs: u32) {
    run(Command::new(ff())
        .args(["-v", "error", "-y"])
        .args(["-f", "lavfi", "-i"])
        .arg(format!("color=red:s=480x320:r=30:d={secs}"))
        .args(["-f", "lavfi", "-i"])
        .arg(format!("color=blue:s=480x320:r=30:d={secs}"))
        .args(["-f", "lavfi", "-i"])
        .arg(format!("sine=d={secs}"))
        .args(["-filter_complex", "[0:v][1:v]vstack,transpose=2[v]"])
        .args(["-map", "[v]", "-map", "2:a", "-c:v", "libx264"])
        .args(["-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest"])
        .args(["-metadata", "location=+35.3192+139.5522/"])
        .arg(path));
    exiftool(
        &[
            "-Rotation=90",
            &format!("-Keys:ContentIdentifier={PAIRING_ID}"),
            "-Keys:GPSCoordinates=35.3192, 139.5522, 12",
            "-Keys:Make=Apple",
            "-Keys:Model=iPhone 14",
            "-Keys:CreationDate=2026:10:03 14:43:24+09:00",
        ],
        path,
    );
}

/// Every tag exiftool reads that would give away a place, a device, a
/// capture time or a pairing.
fn leaked_tags(file: &Path) -> Vec<String> {
    let out = Command::new("exiftool")
        .args(["-a", "-G1", "-s", "-n"])
        .arg(file)
        .output()
        .expect("exiftool");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let banned = [
        "GPS",
        "Location",
        "ContentIdentifier",
        "Serial",
        "Owner",
        "Lens",
        "Make",
        "Model",
        "DateTimeOriginal",
        "CreationDate",
        "ImageUniqueID",
        "PersonInImage",
        "MakerNote",
    ];
    text.lines()
        .filter(|l| {
            let tag = l.split(':').next().unwrap_or("");
            banned.iter().any(|b| tag.contains(b))
        })
        .map(str::to_string)
        .collect()
}

fn assert_clean(file: &Path) {
    let bytes = std::fs::read(file).unwrap();
    for secret in SECRETS {
        assert!(
            !bytes.windows(secret.len()).any(|w| w == secret.as_bytes()),
            "{} contains {secret:?}",
            file.display()
        );
    }
    let leaked = leaked_tags(file);
    assert!(leaked.is_empty(), "{}: {leaked:#?}", file.display());
}

fn files_with(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut found = vec![];
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            found.extend(files_with(&p, ext));
        } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case(ext)) {
            found.push(p);
        }
    }
    found
}

/// One decoded frame as RGB24.
fn frame_rgb(video: &Path, at: f64) -> (u32, u32, Vec<u8>) {
    let caps = triptape_media::ffmpeg::ffprobe_file(&Toolchain::global().ffprobe.bin, video)
        .expect("probe");
    let out = Command::new(ff())
        .args(["-v", "error", "-ss", &format!("{at}"), "-i"])
        .arg(video)
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .output()
        .unwrap();
    (caps.width, caps.height, out.stdout)
}

#[test]
fn reel_from_a_tagged_rotated_clip_is_clean_and_upright() {
    if !have_exiftool() {
        eprintln!("skipped: exiftool not installed (CI installs it)");
        return;
    }
    let t = tempfile::tempdir().unwrap();
    let clip = t.path().join("IMG_0144.MOV");
    tagged_clip(&clip, 7);
    assert!(
        !leaked_tags(&clip).is_empty(),
        "the source must carry the tags under test"
    );
    let caps = triptape_media::ffmpeg::probe_capabilities(ff(), &Toolchain::global().ffprobe.bin);
    let entry = ResolvedEntry {
        entry: PlanEntry {
            asset_id: "a1".into(),
            window: 0,
            content_hash: None,
            size_bytes: 0,
            mtime: 0,
            rendition: Rendition::Clip {
                trim_start: 0.0,
                trim_end: 6.0,
            },
        },
        still_path: clip.clone(),
        motion_path: Some(clip.clone()),
        caption: None,
        taken_at: None,
        tz_offset_secs: None,
        aspect: Some(0.75),
    };
    let out = t.path().join("reels");
    let made = render_reels(
        &caps,
        &[entry],
        &out,
        &ReelOptions {
            typst_bin: Toolchain::global().typst.bin.clone(),
            ..Default::default()
        },
    )
    .expect("reel renders");
    assert_eq!(made.reels.len(), 1, "{:?}", made.fallbacks);
    let reel = out.join(&made.reels[0].path);
    let cover = out.join(&made.reels[0].cover);
    assert_clean(&reel);
    assert_clean(&cover);

    // Upright: the portrait clip fills the 9:16 frame's width with red above
    // blue. Played sideways, red and blue would sit left and right.
    let (w, h, px) = frame_rgb(&reel, 1.0);
    assert_eq!((w, h), (1080, 1920));
    let at = |x: u32, y: u32| {
        let i = ((y * w + x) * 3) as usize;
        (px[i], px[i + 1], px[i + 2])
    };
    let (top, bottom) = (at(540, 700), at(540, 1220));
    assert!(top.0 > 180 && top.2 < 80, "top should be red: {top:?}");
    assert!(
        bottom.2 > 180 && bottom.0 < 80,
        "bottom should be blue: {bottom:?}"
    );
}

#[test]
fn film_from_tagged_sources_is_clean_and_carries_no_rotation() {
    if !have_exiftool() {
        eprintln!("skipped: exiftool not installed (CI installs it)");
        return;
    }
    let tools = Toolchain::global();
    if !triptape_media::titlecard::typst_available(&tools.typst.bin) {
        eprintln!("skipped: typst missing");
        return;
    }
    let t = tempfile::tempdir().unwrap();
    let still = t.path().join("IMG_0100.JPG");
    tagged_still(&still, 1, 0, HOME);
    let clip = t.path().join("IMG_0144.MOV");
    tagged_clip(&clip, 4);
    let entry =
        |id: &str, rendition: Rendition, still_path: &Path, motion: Option<&Path>| ResolvedEntry {
            entry: PlanEntry {
                asset_id: id.into(),
                window: 0,
                content_hash: None,
                size_bytes: 0,
                mtime: 0,
                rendition,
            },
            still_path: still_path.to_path_buf(),
            motion_path: motion.map(Path::to_path_buf),
            caption: None,
            taken_at: Some(1_791_288_000),
            tz_offset_secs: None,
            aspect: None,
        };
    let entries = [
        entry("a0", Rendition::Still, &still, None),
        entry(
            "a1",
            Rendition::Clip {
                trim_start: 0.0,
                trim_end: 3.0,
            },
            &clip,
            Some(&clip),
        ),
    ];
    let plan = triptape_core::types::Plan {
        plan_version: 1,
        schema_version: 1,
        hero: vec![],
        reel_selects: vec![],
        chapters: vec![triptape_core::types::Chapter {
            title: "Day 1".into(),
            asset_ids: vec!["a0".into(), "a1".into()],
            ..Default::default()
        }],
        captions: Default::default(),
        map_polyline: vec![],
        music: triptape_core::types::MusicPick {
            track: "bundled".into(),
            rationale: "default".into(),
        },
        collage: Default::default(),
        cover: Default::default(),
        crop: Default::default(),
        book: Default::default(),
    };
    let caps = triptape_media::ffmpeg::probe_capabilities(ff(), &tools.ffprobe.bin);
    let film = t.path().join("hero.mp4");
    triptape_media::render_video::render_hero_video(
        &caps,
        &plan,
        &entries,
        None,
        &t.path().join("work"),
        &film,
        &triptape_media::render_video::VideoOptions {
            trip_name: "Privacy Trip".into(),
            still_secs: 1.5,
            title_secs: 1.0,
            typst_bin: tools.typst.bin.clone(),
            resolution: triptape_media::resolution::Resolution::Hd1080,
            ..Default::default()
        },
    )
    .expect("film renders");
    assert_clean(&film);
    // Upright with no display matrix: the filters turned the pixels.
    let out = Command::new("exiftool")
        .args(["-s3", "-n", "-Rotation"])
        .arg(&film)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "0");
}

/// Every keepsake through the CLI, as the owner runs it. 4032x3024 book
/// pages take minutes in a debug build: run with
/// `cargo test -p triptape-media --release --test export_privacy -- --include-ignored`.
#[test]
#[ignore = "slow in debug builds; run with --release -- --include-ignored"]
fn cli_export_strips_every_shared_file_and_keeps_home_out_of_sidecars() {
    if !have_exiftool() {
        eprintln!("skipped: exiftool not installed (CI installs it)");
        return;
    }
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("trip");
    std::fs::create_dir_all(&src).unwrap();
    // Three days: home, Tokyo, Osaka. 33 stills clear the book's 30.
    let places = [HOME, (35.6812, 139.7671), (34.6937, 135.5023)];
    for i in 0..33 {
        let day = i / 11;
        let (lat, lon) = places[day];
        let gps = (lat + (i % 11) as f64 * 1e-4, lon);
        tagged_still(&src.join(format!("IMG_{:04}.JPG", 100 + i)), i, day, gps);
    }
    // A Live Photo's motion half (pairs with IMG_0100 by name and length)
    // and a real clip.
    tagged_clip(&src.join("IMG_0100.MOV"), 2);
    tagged_clip(&src.join("IMG_0200.MOV"), 7);

    let db = t.path().join("trip.db");
    let out = t.path().join("export");
    let cli = env!("CARGO_BIN_EXE_triptape");
    // Never the app's saved settings: home comes from --home below.
    let no_settings = t.path().join("no-settings.json");
    run(Command::new(cli)
        .env("TRIPTAPE_SETTINGS", &no_settings)
        .args(["scan", "--folder"])
        .arg(&src)
        .arg("--db")
        .arg(&db)
        .arg("--cache")
        .arg(t.path().join("cache")));
    run(Command::new(cli)
        .env("TRIPTAPE_SETTINGS", &no_settings)
        .args(["export", "--db"])
        .arg(&db)
        .arg("--out")
        .arg(&out)
        .args(["--resolution", "1080p", "--quiet", "--no-segment-cache"])
        .args(["--home", &format!("{},{}", HOME.0, HOME.1)]));

    let film = out.join("hero.mp4");
    assert!(film.exists(), "no film");
    assert_clean(&film);
    let reels = files_with(&out.join("reels"), "mp4");
    assert!(!reels.is_empty(), "no reels");
    let covers = files_with(&out.join("reels"), "jpg");
    assert_eq!(covers.len(), reels.len());
    let pages = files_with(&out.join("book"), "jpg");
    assert!(pages.len() >= 24, "book pages: {}", pages.len());
    assert!(out.join("book").join("cover.jpg").exists());
    let pdfs = files_with(&out.join("book"), "pdf");
    assert!(!pdfs.is_empty(), "no book PDF (is Typst installed?)");
    for f in reels.iter().chain(&covers).chain(&pages).chain(&pdfs) {
        assert_clean(f);
    }

    // Owner-facing sidecars: no home coordinates, no people's names.
    let mut sidecars = files_with(&out, "json");
    sidecars.extend(files_with(&out, "txt"));
    sidecars.extend(files_with(&out, "typ"));
    assert!(sidecars.iter().any(|p| p.ends_with("export.json")));
    for f in &sidecars {
        let text = std::fs::read_to_string(f).unwrap();
        for needle in ["35.319", "139.552", PERSON] {
            assert!(!text.contains(needle), "{} holds {needle:?}", f.display());
        }
    }
}
