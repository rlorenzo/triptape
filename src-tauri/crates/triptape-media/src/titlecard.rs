//! Title card PNG (Typst text when available, styled-solid fallback).
//!
//! Location/time context is rendered as pixels via Typst (`typst compile
//! --format png --ppi 72`, DejaVu/Helvetica stack): a full-frame title card
//! with the trip name + date range, and 1200x200 lower-thirds (chapter title
//! and/or `Oct 6, 2026` date + caption) overlaid per segment. When Typst is
//! missing the renderers fall back to the text-free solid card and record a
//! note, never failing the export.

use image::{Rgb, RgbImage};
use std::path::Path;
use std::process::Command;

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

/// `taken_at` (unix secs, UTC) as `Oct 6, 2026`.
pub fn format_day(unix_secs: i64) -> Option<String> {
    use chrono::{DateTime, Utc};
    DateTime::<Utc>::from_timestamp(unix_secs, 0).map(|d| d.format("%b %-d, %Y").to_string())
}

/// Date range over `taken_at`s as `Oct 3, 2026 – Oct 9, 2026`
/// (single day collapses to one date; empty input yields `None`).
pub fn format_date_range(taken: &[i64]) -> Option<String> {
    let (mut lo, mut hi) = (i64::MAX, i64::MIN);
    for &t in taken {
        lo = lo.min(t);
        hi = hi.max(t);
    }
    if lo == i64::MAX {
        return None;
    }
    let (lo_s, hi_s) = (format_day(lo)?, format_day(hi)?);
    if lo_s == hi_s {
        Some(lo_s)
    } else {
        Some(format!("{lo_s} – {hi_s}"))
    }
}

/// (top, bottom) lines for one segment lower-third, or `None` when there is
/// nothing to show. Chapter starts lead with the chapter title; otherwise
/// the top line is the capture date (or the caption when dateless).
pub fn overlay_lines(
    chapter_title: Option<&str>,
    taken_at: Option<i64>,
    caption: Option<&str>,
) -> Option<(String, Option<String>)> {
    let chapter = chapter_title.map(str::trim).filter(|c| !c.is_empty());
    let cap = caption.map(str::trim).filter(|c| !c.is_empty());
    let date = taken_at.and_then(format_day);
    match (chapter, date, cap) {
        (None, None, None) => None,
        (Some(ch), d, c) => {
            let bottom = match (d, c) {
                (Some(d), Some(c)) => Some(format!("{d} · {c}")),
                (Some(d), None) => Some(d),
                (None, Some(c)) => Some(c.to_string()),
                (None, None) => None,
            };
            Some((ch.to_string(), bottom))
        }
        (None, Some(d), Some(c)) => Some((d, Some(c.to_string()))),
        (None, Some(d), None) => Some((d, None)),
        (None, None, Some(c)) => Some((c.to_string(), None)),
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
