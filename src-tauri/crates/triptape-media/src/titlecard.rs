//! Title card PNG (Typst text when available, styled-solid fallback).
//!
//! Location/time context is rendered as pixels via Typst (`typst compile
//! --format png --ppi 72`, DejaVu/Helvetica stack): a full-frame title card
//! with the trip name + date range, and 1200x200 lower-thirds (chapter day
//! and place) overlaid at chapter starts. When Typst is
//! missing the renderers fall back to the text-free solid card and record a
//! note, never failing the export.

use image::{Rgb, RgbImage};
use std::path::Path;
use std::process::Command;
use triptape_core::types::Chapter;

/// Font stack for Typst text: Helvetica on macOS, DejaVu on Linux.
/// Unknown families only warn; Typst falls through to the next entry.
pub const FONT_STACK: &str = r#"("Helvetica", "Helvetica Neue", "DejaVu Sans")"#;

/// True when `typst_bin` runs (`typst --version` exits 0).
pub fn typst_available(typst_bin: &str) -> bool {
    Command::new(typst_bin)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Compile a Typst document string to a PNG at `ppi` (72 makes 1pt == 1px).
/// Writes a sibling `.typ` next to `out_png` (removed on success).
pub fn render_text_png(
    typst_bin: &str,
    doc: &str,
    out_png: &Path,
    ppi: u32,
) -> Result<(), crate::MediaError> {
    let typ = out_png.with_extension("typ");
    std::fs::write(&typ, doc).map_err(|e| crate::MediaError::Io(e.to_string()))?;
    let out = Command::new(typst_bin)
        .args(["compile", "--format", "png", "--ppi", &ppi.to_string()])
        .arg(&typ)
        .arg(out_png)
        .output()
        .map_err(|e| crate::MediaError::Io(format!("spawn {typst_bin}: {e}")))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(out_png);
        return Err(crate::MediaError::Ffmpeg(format!(
            "typst compile failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    if !out_png.is_file() {
        return Err(crate::MediaError::Ffmpeg("typst produced no png".into()));
    }
    let _ = std::fs::remove_file(&typ);
    Ok(())
}

/// Escape a string for use inside a Typst `"..."` literal.
fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Full-frame 1920x1080 title card: trip name, accent rule, date range.
pub fn title_card_doc(trip_name: &str, date_range: Option<&str>) -> String {
    let date_block = match date_range {
        Some(d) => format!(
            "\n  line(length: 340pt, stroke: 6pt + rgb(\"#f5b242\")),\n  text(size: 46pt, fill: rgb(\"#c8cfdb\"), \"{}\"),",
            esc(d)
        ),
        None => String::new(),
    };
    format!(
        r#"#set page(width: 1920pt, height: 1080pt, fill: rgb(13, 20, 32), margin: 0pt)
#set text(font: {fonts}, fill: white)
#place(center + horizon, stack(dir: ttb, spacing: 22pt,
  text(size: 96pt, weight: "bold", "{name}"),{dates}
))"#,
        fonts = FONT_STACK,
        name = esc(trip_name),
        dates = date_block,
    )
}

/// 1200x200 lower-third bar: bold top line, optional dimmer bottom line.
pub fn lower_third_doc(top: &str, bottom: Option<&str>) -> String {
    let bottom_block = match bottom {
        Some(b) => format!(
            "\n  line(length: 200pt, stroke: 4pt + rgb(\"#f5b242\")),\n  text(size: 38pt, fill: rgb(\"#c8cfdb\"), \"{}\"),",
            esc(b)
        ),
        None => String::new(),
    };
    format!(
        r#"#set page(width: 1200pt, height: 200pt, fill: rgb(13, 20, 32), margin: 0pt)
#set text(font: {fonts}, fill: white)
#place(left + horizon, dx: 60pt, stack(dir: ttb, spacing: 10pt,
  text(size: 52pt, weight: "bold", "{top}"),{bottom}
))"#,
        fonts = FONT_STACK,
        top = esc(top),
        bottom = bottom_block,
    )
}

/// Full-frame closing card: the places the trip went, wrapped and
/// centered, with the trip name and date range as a footer. Gives the
/// film an ending instead of stopping on the last photo.
pub fn places_card_doc(trip_name: &str, places: &[String], date_range: Option<&str>) -> String {
    let list = places.join("  ·  ");
    let footer = match date_range {
        Some(d) => format!("{} · {}", esc(trip_name), esc(d)),
        None => esc(trip_name),
    };
    format!(
        r##"#set page(width: 1920pt, height: 1080pt, fill: rgb(13, 20, 32), margin: 0pt)
#set text(font: {fonts}, fill: white)
#place(center + horizon, stack(dir: ttb, spacing: 26pt,
  text(size: 72pt, weight: "bold", "The places we went"),
  line(length: 340pt, stroke: 6pt + rgb("#f5b242")),
  box(width: 1560pt, text(size: 40pt, fill: rgb("#c8cfdb"), align(center, "{list}"))),
  text(size: 34pt, fill: rgb("#8b95a8"), "{footer}"),
))"##,
        fonts = FONT_STACK,
        list = esc(&list),
        footer = footer,
    )
}

/// Label on the route-map card (hero overlay and book page caption).
pub const ROUTE_LABEL: &str = "Our route";

/// Date range over `taken_at`s as `Oct 3, 2026 – Oct 9, 2026`
/// (single day collapses to one date; empty input yields `None`).
pub fn format_date_range(taken: &[(i64, Option<i32>)]) -> Option<String> {
    // Compare local calendar dates, not UTC instants: across the date line
    // a later instant can fall on an earlier local date.
    let dates: Vec<chrono::NaiveDate> = taken
        .iter()
        .filter_map(|&(t, off)| triptape_core::metadata::local_date(t, off))
        .collect();
    let (lo, hi) = (dates.iter().min()?, dates.iter().max()?);
    let fmt = |d: &chrono::NaiveDate| d.format("%b %-d, %Y").to_string();
    Some(if lo == hi {
        fmt(lo)
    } else {
        format!("{} – {}", fmt(lo), fmt(hi))
    })
}

/// Splits a caption dateline (`Day 4 · Sun, Oct 5 · 4:32 PM` from older saved
/// plans, or `Sun, Oct 5 · 4:32 PM` from `triptape_core::plan::dateline`)
/// into (date part, clock time). `None` for anything else, including user
/// captions that merely end in a time (`Dinner · 7:30 PM`).
pub(crate) fn split_dateline(c: &str) -> Option<(&str, &str)> {
    let (head, time) = c.rsplit_once(" · ")?;
    let day = match head.split_once(" · ") {
        Some((prefix, day)) => {
            let n = prefix.strip_prefix("Day ")?;
            (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).then_some(day)?
        }
        None => head,
    };
    (is_weekday_date(day) && is_clock_time(time)).then_some((head, time))
}

fn all_digits(s: &str, len: std::ops::RangeInclusive<usize>) -> bool {
    len.contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

/// `Sun, Oct 5` (chrono `%a, %b %-d`).
fn is_weekday_date(s: &str) -> bool {
    const WD: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    const MO: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let Some((wd, rest)) = s.split_once(", ") else {
        return false;
    };
    let Some((mo, d)) = rest.split_once(' ') else {
        return false;
    };
    WD.contains(&wd) && MO.contains(&mo) && all_digits(d, 1..=2)
}

/// `4:32 PM` (chrono `%-I:%M %p`).
fn is_clock_time(s: &str) -> bool {
    let Some((hm, ampm)) = s.split_once(' ') else {
        return false;
    };
    let Some((h, m)) = hm.split_once(':') else {
        return false;
    };
    matches!(ampm, "AM" | "PM") && all_digits(h, 1..=2) && all_digits(m, 2..=2)
}

/// (top, bottom) lines for a chapter-start lower-third: the day shown
/// once (`Day 3 · Wed, Oct 8`), with the place underneath. Segments
/// inside a chapter get no overlay at all — the day is stated once and
/// the photos speak. `None` when nothing is known.
pub fn chapter_overlay(
    day_index: Option<u32>,
    date_label: Option<&str>,
    place: Option<&str>,
) -> Option<(String, Option<String>)> {
    let place = place.map(str::trim).filter(|s| !s.is_empty());
    let top = match (day_index, date_label) {
        (Some(n), Some(d)) => format!("Day {n} · {d}"),
        (Some(n), None) => format!("Day {n}"),
        (None, Some(d)) => d.to_string(),
        (None, None) => return place.map(|p| (p.to_string(), None)),
    };
    Some((top, place.map(str::to_string)))
}

/// Chapter heading as lower-third lines. Plans saved before chapters carried
/// day/date/place deserialize with those fields `None`; they fall back to
/// the stored chapter title so re-exports keep their chapter context.
pub(crate) fn chapter_lines(ch: &Chapter) -> Option<(String, Option<String>)> {
    chapter_overlay(ch.day_index, ch.date_label.as_deref(), ch.place.as_deref()).or_else(|| {
        let t = ch.title.trim();
        (!t.is_empty()).then(|| (t.to_string(), None))
    })
}

/// Chapter heading on one line, for book day-marker pages.
pub(crate) fn chapter_book_title(ch: &Chapter) -> String {
    match chapter_lines(ch) {
        Some((top, Some(bottom))) => format!("{top} \u{2014} {bottom}"),
        Some((top, None)) => top,
        None => ch.title.clone(),
    }
}

pub fn save_title_card(
    _trip_name: &str,
    width: u32,
    height: u32,
    out: &Path,
) -> Result<(), crate::MediaError> {
    let (w, h) = (width.max(64), height.max(64));
    let mut img = RgbImage::new(w, h);
    for y in 0..h {
        let t = y as f32 / h as f32;
        let c = Rgb([
            (10.0 + 14.0 * t) as u8,
            (12.0 + 16.0 * t) as u8,
            (22.0 + 24.0 * t) as u8,
        ]);
        for x in 0..w {
            img.put_pixel(x, y, c);
        }
    }
    // Accent rule across the middle third + soft inner frame.
    let rule_y = h * 6 / 10;
    for y in rule_y..(rule_y + (h / 120).max(3)) {
        for x in w / 6..w * 5 / 6 {
            img.put_pixel(x, y, Rgb([245, 178, 66]));
        }
    }
    let m = (w.min(h) / 24).max(4);
    for x in m..w - m {
        for y in [m, h - m - 1] {
            img.put_pixel(x, y, Rgb([120, 130, 145]));
        }
    }
    for y in m..h - m {
        for x in [m, w - m - 1] {
            img.put_pixel(x, y, Rgb([120, 130, 145]));
        }
    }
    img.save(out)
        .map_err(|e| crate::MediaError::Io(format!("save title card: {e}")))?;
    Ok(())
}
