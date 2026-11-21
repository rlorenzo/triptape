//! Parchment travel maps: the itinerary drawn like an old adventure-film
//! chart. Aged paper, hand-inked coastlines, a compass rose, and the route
//! as a dashed red line.
//!
//! Two things come out of it:
//!
//! * a **hop** per stretch of the journey ([`render_hop`]): the line draws
//!   itself from the last stop to the next behind a little plane or ship,
//!   then the place and date pop up on a plaque. The film plays each hop
//!   where the trip reaches that stop.
//! * the **whole route** on one sheet ([`render_overview`]): every leg and
//!   every stop named, for the film's closing card and the book's back
//!   cover.
//!
//! Everything is laid out in design units (the frame is 720 tall) and
//! written as SVG; the bundled Typst rasterizes it at the output size, so
//! the paper grain and line weights look the same at 1080p, 4K and print.
//! A hop's sheet is rendered once; the growing line, the vehicle and the
//! pop-ups are drawn over it per frame and streamed to ffmpeg. Offline:
//! land comes from the bundled Natural Earth coastlines.

use crate::ffmpeg::Capabilities;
use crate::resolution::Resolution;
use crate::routeanim::{blend_px, encode, frame_count, RecapReport, FPS};
use crate::MediaError;
use image::{Rgb, RgbImage, RgbaImage};
use std::fmt::Write as _;
use std::path::Path;
use triptape_core::itinerary::LegMode;
use triptape_core::journey::{Hop, Journey, JourneyLeg, HOP_HOLD_SECS, HOP_LEAD_SECS};

/// Height of the frame in design units; widths follow the aspect ratio.
const DESIGN_H: f64 = 720.0;
/// Georgia where the system has it; Typst's own serif otherwise.
const FONT: &str = "Georgia, Libertinus Serif, serif";
const RED: &str = "#b3241b";
const RED_RGB: Rgb<u8> = Rgb([0xb3, 0x24, 0x1b]);
const CREAM_RGB: Rgb<u8> = Rgb([0xf2, 0xe3, 0xbd]);
/// Route dashes, in design units.
const DASH_ON: f64 = 11.0;
const DASH_OFF: f64 = 9.0;
/// The sticker pops this long after the plaque.
const STICKER_DELAY_SECS: f64 = 0.18;
/// Sticker box, in design units.
const STICKER_BOX: f64 = 104.0;

type LatLon = (f64, f64);
type P = (f64, f64);

fn merc(lat: f64) -> f64 {
    let lat = lat.clamp(-85.0, 85.0).to_radians();
    (std::f64::consts::FRAC_PI_4 + lat / 2.0)
        .tan()
        .ln()
        .to_degrees()
}

fn unmerc(y: f64) -> f64 {
    (2.0 * y.to_radians().exp().atan() - std::f64::consts::FRAC_PI_2).to_degrees()
}

/// A Mercator window onto the world, in design units.
#[derive(Debug, Clone, Copy)]
struct View {
    lon_l: f64,
    /// Mercator y (degrees) of the top edge.
    y_top: f64,
    /// Design units per degree of longitude.
    k: f64,
    w: f64,
    h: f64,
}

impl View {
    /// The window that holds `pts` with `pad` (a share of their extent)
    /// around them, never narrower than `min_span` degrees.
    fn fit(pts: &[LatLon], pad: f64, min_span: f64, (w, h): (f64, f64)) -> Self {
        let (mut lon0, mut lon1, mut y0, mut y1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
        for &(lat, lon) in pts {
            lon0 = lon0.min(lon);
            lon1 = lon1.max(lon);
            y0 = y0.min(merc(lat));
            y1 = y1.max(merc(lat));
        }
        if pts.is_empty() {
            (lon0, lon1, y0, y1) = (-180.0, 180.0, -60.0, 60.0);
        }
        let span_w = ((lon1 - lon0) * (1.0 + pad)).max(min_span);
        let span_h = ((y1 - y0) * (1.0 + pad)).max(min_span * h / w);
        let k = (w / span_w).min(h / span_h);
        Self {
            lon_l: (lon0 + lon1) / 2.0 - w / 2.0 / k,
            y_top: (y0 + y1) / 2.0 + h / 2.0 / k,
            k,
            w,
            h,
        }
    }

    fn xy(&self, (lat, lon): LatLon) -> P {
        (
            (lon - self.lon_l) * self.k,
            (self.y_top - merc(lat)) * self.k,
        )
    }

    fn holds(&self, p: P, margin: f64) -> bool {
        p.0 > margin && p.0 < self.w - margin && p.1 > margin && p.1 < self.h - margin
    }
}

/// One side of a clip rectangle: which points are inside it, where a
/// segment crosses it, and the coordinate it sits at.
type ClipEdge = (fn(P, f64) -> bool, fn(P, P, f64) -> P, f64);

/// Sutherland-Hodgman: `ring` clipped to the rectangle `(x0, y0, x1, y1)`.
fn clip(ring: &[P], (x0, y0, x1, y1): (f64, f64, f64, f64)) -> Vec<P> {
    // Each edge: which side is inside, and where a segment crosses it.
    let edges: [ClipEdge; 4] = [
        (|p, v| p.0 >= v, cross_x, x0),
        (|p, v| p.0 <= v, cross_x, x1),
        (|p, v| p.1 >= v, cross_y, y0),
        (|p, v| p.1 <= v, cross_y, y1),
    ];
    let mut pts = ring.to_vec();
    for (inside, cross, v) in edges {
        if pts.is_empty() {
            break;
        }
        let mut out = Vec::with_capacity(pts.len());
        let mut prev = pts[pts.len() - 1];
        for &p in &pts {
            match (inside(prev, v), inside(p, v)) {
                (true, true) => out.push(p),
                (true, false) => out.push(cross(prev, p, v)),
                (false, true) => {
                    out.push(cross(prev, p, v));
                    out.push(p);
                }
                (false, false) => {}
            }
            prev = p;
        }
        pts = out;
    }
    pts
}

fn cross_x(a: P, b: P, x: f64) -> P {
    (x, a.1 + (b.1 - a.1) * (x - a.0) / (b.0 - a.0))
}

fn cross_y(a: P, b: P, y: f64) -> P {
    (a.0 + (b.0 - a.0) * (y - a.1) / (b.1 - a.1), y)
}

/// Douglas-Peucker: drop points that sit within `tol` of the line through
/// their kept neighbours. The first and last points stay.
fn simplify(pts: &[P], tol: f64) -> Vec<P> {
    if pts.len() < 3 {
        return pts.to_vec();
    }
    let mut keep = vec![false; pts.len()];
    keep[0] = true;
    keep[pts.len() - 1] = true;
    let mut stack = vec![(0, pts.len() - 1)];
    while let Some((a, b)) = stack.pop() {
        let (mut worst, mut at) = (0.0, a);
        for i in a + 1..b {
            let d = crate::routeanim::point_seg_dist(pts[i], pts[a], pts[b]);
            if d > worst {
                (worst, at) = (d, i);
            }
        }
        if worst > tol {
            keep[at] = true;
            stack.push((a, at));
            stack.push((at, b));
        }
    }
    pts.iter()
        .zip(keep)
        .filter_map(|(p, k)| k.then_some(*p))
        .collect()
}

/// SVG path data for the land in `v`: `(fill, coast)`. The fill closes
/// every ring; the coast is the same outline without the seams where the
/// source data cuts a landmass at the antimeridian or the view clips it.
fn land_paths(v: &View) -> (String, String) {
    const MARGIN: f64 = 24.0;
    let rect = (-MARGIN, -MARGIN, v.w + MARGIN, v.h + MARGIN);
    let (lon_l, lon_r) = (v.lon_l - MARGIN / v.k, v.lon_l + (v.w + MARGIN) / v.k);
    let lat_s = unmerc(v.y_top - (v.h + MARGIN) / v.k);
    let lat_n = unmerc(v.y_top + MARGIN / v.k);
    let on_edge = |p: P| {
        p.0 <= rect.0 + 0.01 || p.0 >= rect.2 - 0.01 || p.1 <= rect.1 + 0.01 || p.1 >= rect.3 - 0.01
    };
    let (mut fill, mut coast) = (String::new(), String::new());
    for ring in triptape_core::geo::coastlines() {
        let (x0, y0, x1, y1) = ring.bbox;
        if f64::from(y1) < lat_s || f64::from(y0) > lat_n {
            continue;
        }
        for turn in -1..=2 {
            let shift = 360.0 * f64::from(turn);
            if f64::from(x1) + shift < lon_l || f64::from(x0) + shift > lon_r {
                continue;
            }
            let seam = |p: P| {
                [-180.0, 180.0]
                    .iter()
                    .any(|m| (p.0 - v.xy((0.0, m + shift)).0).abs() < 0.02)
            };
            let projected: Vec<P> = ring
                .pts
                .iter()
                .map(|&(lon, lat)| v.xy((f64::from(lat), f64::from(lon) + shift)))
                .collect();
            let pts = simplify(&clip(&projected, rect), 0.4);
            let (mut bx, mut by) = ((f64::MAX, f64::MIN), (f64::MAX, f64::MIN));
            for p in &pts {
                bx = (bx.0.min(p.0), bx.1.max(p.0));
                by = (by.0.min(p.1), by.1.max(p.1));
            }
            // A speck under a unit across would only add noise.
            if pts.len() < 3 || (bx.1 - bx.0 < 0.8 && by.1 - by.0 < 0.8) {
                continue;
            }
            for (i, p) in pts.iter().enumerate() {
                let _ = write!(
                    fill,
                    "{}{:.1},{:.1}",
                    if i == 0 { 'M' } else { 'L' },
                    p.0,
                    p.1
                );
            }
            fill.push('Z');
            let mut pen_down = false;
            for i in 0..=pts.len() {
                let (a, b) = (pts[(i + pts.len() - 1) % pts.len()], pts[i % pts.len()]);
                let hidden = (seam(a) && seam(b)) || (on_edge(a) && on_edge(b));
                if i == 0 || hidden {
                    pen_down = false;
                    continue;
                }
                if !pen_down {
                    let _ = write!(coast, "M{:.1},{:.1}", a.0, a.1);
                    pen_down = true;
                }
                let _ = write!(coast, "L{:.1},{:.1}", b.0, b.1);
            }
        }
    }
    (fill, coast)
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\'', "&apos;")
}

fn path_d(pts: &[P]) -> String {
    let mut d = String::new();
    for (i, p) in pts.iter().enumerate() {
        let _ = write!(d, "{}{:.1},{:.1}", if i == 0 { 'M' } else { 'L' }, p.0, p.1);
    }
    d
}

/// A dashed route line; `faint` for legs already travelled.
fn route_svg(pts: &[P], faint: bool) -> String {
    let dash = format!(
        "fill='none' stroke-linecap='round' stroke-linejoin='round' stroke-dasharray='{DASH_ON} {DASH_OFF}'"
    );
    let d = path_d(pts);
    if faint {
        format!("<path d='{d}' stroke='{RED}' stroke-opacity='.4' stroke-width='3' {dash}/>")
    } else {
        format!(
            "<path d='{d}' stroke='#f2e3bd' stroke-opacity='.5' stroke-width='6' transform='translate(.8,.8)' {dash}/>\
             <path d='{d}' stroke='{RED}' stroke-width='4' {dash}/>"
        )
    }
}

fn compass(cx: f64, cy: f64, r: f64) -> String {
    // An eight-point star: long points on the axes, short ones between.
    let star = |long: f64, short: f64, turn: f64, fill: &str| {
        let pts: Vec<String> = (0..8)
            .map(|i| {
                let a = std::f64::consts::FRAC_PI_4 * f64::from(i);
                let reach = if i % 2 == 0 { long } else { short };
                format!("{:.1},{:.1}", reach * a.sin(), -reach * a.cos())
            })
            .collect();
        format!(
            "<polygon transform='rotate({turn})' fill='{fill}' stroke='#4a2f14' stroke-width='1' points='{}'/>",
            pts.join(" ")
        )
    };
    format!(
        "<g transform='translate({cx:.1},{cy:.1})' opacity='.9'>\
         <circle r='{r}' fill='none' stroke='#4a2f14' stroke-width='2'/>\
         <circle r='{:.1}' fill='none' stroke='#4a2f14' stroke-width='1' stroke-dasharray='2 4'/>\
         {}{}\
         <circle r='{:.1}' fill='#f2e3bd' stroke='#4a2f14'/>\
         <text y='{:.1}' text-anchor='middle' font-family='{FONT}' font-weight='bold' font-size='{:.1}' fill='#3b2410'>N</text></g>",
        r * 0.86,
        star(r * 0.95, r * 0.25, 45.0, "#c9a66b"),
        star(r * 1.15, r * 0.22, 0.0, "#7a4a22"),
        r * 0.09,
        -r * 1.22,
        r * 0.5,
    )
}

/// A plaque's size in design units for `text` at `size`.
fn plaque_size(text: &str, size: f64) -> (f64, f64) {
    (
        size * (1.76 + 0.544 * text.chars().count() as f64),
        size * 1.76,
    )
}

/// A cream plaque with `text`, its top-left corner at the origin.
fn plaque_svg(text: &str, size: f64) -> String {
    let (w, h) = plaque_size(text, size);
    let s = size / 25.0;
    format!(
        "<rect x='{:.1}' y='{:.1}' width='{w:.1}' height='{h:.1}' rx='{:.1}' fill='#2a1604' opacity='.35'/>\
         <rect width='{w:.1}' height='{h:.1}' rx='{:.1}' fill='#f4e6bf' stroke='#4a2f14' stroke-width='{:.1}'/>\
         <rect x='{:.1}' y='{:.1}' width='{:.1}' height='{:.1}' rx='{:.1}' fill='none' stroke='#8a6a3a' stroke-width='{:.1}'/>\
         <text x='{:.1}' y='{:.1}' text-anchor='middle' font-family='{FONT}' font-style='italic' font-weight='bold' font-size='{size}' fill='#3b2410'>{}</text>",
        3.0 * s,
        4.0 * s,
        7.0 * s,
        7.0 * s,
        2.5 * s,
        4.0 * s,
        4.0 * s,
        w - 8.0 * s,
        h - 8.0 * s,
        4.0 * s,
        s,
        w / 2.0,
        h / 2.0 + size * 0.32,
        xml(text),
    )
}

/// The sheet: paper, graticule, land, then `under` (routes and pins, which
/// take the paper's vignette), the compass, and `over` (plaques) on top.
fn sheet_svg(v: &View, under: &str, over: &str) -> String {
    let (w, h) = (v.w, v.h);
    let (fill, coast) = land_paths(v);
    let mut grid = String::new();
    let mut lon = (v.lon_l / 10.0).ceil() * 10.0;
    while lon < v.lon_l + w / v.k {
        let x = (lon - v.lon_l) * v.k;
        let _ = write!(grid, "<line x1='{x:.1}' y1='0' x2='{x:.1}' y2='{h}'/>");
        lon += 10.0;
    }
    let mut lat = (unmerc(v.y_top - h / v.k) / 10.0).ceil() * 10.0;
    while lat < unmerc(v.y_top) {
        let y = v.xy((lat, 0.0)).1;
        let _ = write!(grid, "<line x1='0' y1='{y:.1}' x2='{w}' y2='{y:.1}'/>");
        lat += 10.0;
    }
    format!(
        "<svg xmlns='http://www.w3.org/2000/svg' width='{w}' height='{h}' viewBox='0 0 {w} {h}'>\
<defs>\
<filter id='paper' x='0' y='0' width='100%' height='100%'>\
<feTurbulence type='fractalNoise' baseFrequency='.012' numOctaves='4' seed='7' result='n1'/>\
<feColorMatrix in='n1' type='matrix' values='0 0 0 0 .45  0 0 0 0 .30  0 0 0 0 .12  0 0 0 1.1 -.35' result='stain'/>\
<feTurbulence type='fractalNoise' baseFrequency='.9' numOctaves='2' seed='3' result='n2'/>\
<feColorMatrix in='n2' type='matrix' values='0 0 0 0 .3  0 0 0 0 .2  0 0 0 0 .08  0 0 0 .5 -.12' result='grain'/>\
<feMerge><feMergeNode in='stain'/><feMergeNode in='grain'/></feMerge>\
</filter>\
<radialGradient id='vig' cx='50%' cy='50%' r='75%'><stop offset='.55' stop-color='#3a2208' stop-opacity='0'/><stop offset='1' stop-color='#2a1604' stop-opacity='.8'/></radialGradient>\
<filter id='rough' x='-5%' y='-5%' width='110%' height='110%'><feTurbulence type='fractalNoise' baseFrequency='.03' numOctaves='2' seed='2'/><feDisplacementMap in='SourceGraphic' scale='2.5'/></filter>\
</defs>\
<rect width='{w}' height='{h}' fill='#e9d8ab'/>\
<g stroke='#8a6a3a' stroke-opacity='.25' stroke-width='1'>{grid}</g>\
<g filter='url(#rough)'>\
<path d='{coast}' fill='none' stroke='#8a6a3a' stroke-opacity='.25' stroke-width='9' stroke-linejoin='round'/>\
<path d='{fill}' fill='#bf975a' fill-rule='evenodd'/>\
<path d='{coast}' fill='none' stroke='#5b3a17' stroke-width='1.7' stroke-linejoin='round'/></g>\
<rect width='{w}' height='{h}' filter='url(#paper)' opacity='.5'/>\
{under}{compass}\
<rect width='{w}' height='{h}' fill='url(#vig)'/>\
{over}\
<rect x='10' y='10' width='{bw}' height='{bh}' fill='none' stroke='#4a2f14' stroke-width='3' opacity='.7'/>\
<rect x='17' y='17' width='{iw}' height='{ih}' fill='none' stroke='#4a2f14' stroke-width='1' opacity='.7'/>\
</svg>",
        compass = compass(w - 95.0, h - 100.0, 46.0),
        bw = w - 20.0,
        bh = h - 20.0,
        iw = w - 34.0,
        ih = h - 34.0,
    )
}

/// A plane pointing right, centred on the origin.
const PLANE_SVG: &str = "<path d='M28,0 L8,-4 L-2,-26 L-9,-26 L-5,-3 L-19,-3 L-24,-11 L-29,-11 L-26,0 L-29,11 L-24,11 L-19,3 L-5,3 L-9,26 L-2,26 L8,4 Z' fill='#3b2410' stroke='#f2e3bd' stroke-width='2'/>";
/// Plane and ship sprite boxes, in design units.
const PLANE_BOX: (f64, f64) = (56.0, 52.0);
const SHIP_BOX: (f64, f64) = (70.0, 46.0);

/// A little liner sailing right in `SHIP_BOX`; each ship of the trip gets
/// its own colours (hull, trim, funnel).
fn ship_svg(ship: usize) -> String {
    const LIVERY: [(&str, &str, &str); 3] = [
        ("#3b2410", "#b3241b", "#b3241b"),
        ("#12224f", "#e0b23a", "#e0b23a"),
        ("#1f4d3a", "#f2e3bd", "#c9a66b"),
    ];
    let (hull, trim, funnel) = LIVERY[ship % LIVERY.len()];
    let portholes: String = (0..5)
        .map(|i| {
            format!(
                "<circle cx='{}' cy='28' r='1.8' fill='{hull}'/>",
                22 + i * 8
            )
        })
        .collect();
    format!(
        "<g transform='translate(1,2) scale(.82)'>\
         <path d='M2,34 L82,34 L68,50 L10,50 Z' fill='{hull}' stroke='#f2e3bd' stroke-width='1.5'/>\
         <rect x='5' y='38' width='72' height='3' fill='{trim}'/>\
         <rect x='16' y='22' width='46' height='12' fill='#fff' stroke='{hull}'/>\
         <rect x='24' y='12' width='28' height='10' fill='#fff' stroke='{hull}'/>\
         <rect x='29' y='0' width='7' height='12' fill='{funnel}' stroke='{hull}'/>\
         <rect x='41' y='2' width='7' height='10' fill='{funnel}' stroke='{hull}'/>\
         {portholes}</g>"
    )
}

/// Rasterize `svg` (design size `(dw, dh)`) to `px` pixels through Typst.
fn rasterize(
    typst_bin: &str,
    svg_body: &str,
    px: (u32, u32),
    stem: &Path,
) -> Result<RgbaImage, MediaError> {
    let io = |e: std::io::Error| MediaError::Io(e.to_string());
    if let Some(dir) = stem.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let (svg, png) = (stem.with_extension("svg"), stem.with_extension("png"));
    std::fs::write(&svg, svg_body).map_err(io)?;
    let doc = format!(
        "#set page(width: {}pt, height: {}pt, margin: 0pt, fill: none)\n#image(\"{}\", width: 100%, height: 100%)\n",
        px.0,
        px.1,
        crate::titlecard::esc(&svg.file_name().unwrap_or_default().to_string_lossy()),
    );
    let made = crate::titlecard::render_text_png(typst_bin, &doc, &png, 72).and_then(|()| {
        image::open(&png)
            .map(|im| im.to_rgba8())
            .map_err(|e| MediaError::Io(format!("read map: {e}")))
    });
    let _ = std::fs::remove_file(&svg);
    let _ = std::fs::remove_file(&png);
    let _ = std::fs::remove_file(stem.with_extension("typ"));
    let img = made?;
    Ok(if img.dimensions() == px {
        img
    } else {
        image::imageops::resize(&img, px.0, px.1, image::imageops::FilterType::Triangle)
    })
}

/// A transparent sprite: `body` drawn in a `box_` of design units.
fn sprite(
    typst_bin: &str,
    body: &str,
    box_: (f64, f64),
    origin: P,
    u: f64,
    stem: &Path,
) -> Result<RgbaImage, MediaError> {
    let svg = format!(
        "<svg xmlns='http://www.w3.org/2000/svg' width='{0}' height='{1}' viewBox='0 0 {0} {1}'><g transform='translate({2},{3})'>{body}</g></svg>",
        box_.0, box_.1, origin.0, origin.1
    );
    let px = (
        (box_.0 * u).round().max(1.0) as u32,
        (box_.1 * u).round().max(1.0) as u32,
    );
    rasterize(typst_bin, &svg, px, stem)
}

fn flatten(img: &RgbaImage) -> RgbImage {
    image::DynamicImage::ImageRgba8(img.clone()).to_rgb8()
}

/// A leg's path as drawn: a sailing's corners are rounded off (two rounds
/// of corner cutting), since a routed sea path turns in straight strokes.
fn drawn_path(leg: &JourneyLeg) -> Vec<LatLon> {
    let mut pts = leg.path.clone();
    if leg.mode != LegMode::Ship {
        return pts;
    }
    for _ in 0..2 {
        let mut out = vec![pts[0]];
        for w in pts.windows(2) {
            let cut = |t: f64| {
                (
                    w[0].0 + (w[1].0 - w[0].0) * t,
                    w[0].1 + (w[1].1 - w[0].1) * t,
                )
            };
            out.push(cut(0.25));
            out.push(cut(0.75));
        }
        out.push(pts[pts.len() - 1]);
        pts = out;
    }
    pts
}

type Rect = (f64, f64, f64, f64);

fn overlaps(a: Rect, b: Rect) -> bool {
    a.0 < b.2 && b.0 < a.2 && a.1 < b.3 && b.1 < a.3
}

/// Points every few units along `pts`, for keeping plaques off a line.
fn sampled(pts: &[P]) -> Vec<P> {
    let mut out = vec![];
    for w in pts.windows(2) {
        let n = ((w[1].0 - w[0].0).hypot(w[1].1 - w[0].1) / 6.0)
            .ceil()
            .max(1.0);
        for i in 0..n as u32 {
            let t = f64::from(i) / n;
            out.push((
                w[0].0 + (w[1].0 - w[0].0) * t,
                w[0].1 + (w[1].1 - w[0].1) * t,
            ));
        }
    }
    out.extend(pts.last());
    out
}

/// The top-left corner for a `size` box: the candidate that stays inside
/// the frame (by `margin`), clear of `taken`, and covers the least of the
/// `ink` points. Earlier candidates win ties. With no candidate that
/// fits, the first one pulled into the frame.
fn pick_spot(candidates: &[P], size: P, frame: P, margin: f64, taken: &[Rect], ink: &[P]) -> P {
    let rect = |&(x, y): &P| (x, y, x + size.0, y + size.1);
    let fits = |c: &P| {
        let r = rect(c);
        r.0 >= margin
            && r.1 >= margin
            && r.2 <= frame.0 - margin
            && r.3 <= frame.1 - margin
            && !taken.iter().any(|&t| overlaps(r, t))
    };
    let covered = |c: &P| {
        let r = rect(c);
        let pad = 5.0;
        ink.iter()
            .filter(|p| p.0 > r.0 - pad && p.0 < r.2 + pad && p.1 > r.1 - pad && p.1 < r.3 + pad)
            .count()
    };
    candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| fits(c))
        .min_by_key(|(i, c)| (covered(c), *i))
        .map_or_else(
            || {
                let (x, y) = candidates[0];
                (
                    x.clamp(margin, (frame.0 - size.0 - margin).max(margin)),
                    y.clamp(margin, (frame.1 - size.1 - margin).max(margin)),
                )
            },
            |(_, c)| *c,
        )
}

/// Stops and sea or ground paths up to `legs` (flights left out): what a
/// regional view has to hold. Empty when the trip so far is all flights.
fn regional(journey: &Journey, legs: usize) -> Vec<LatLon> {
    let mut pts = vec![];
    for (i, leg) in journey.legs.iter().enumerate().take(legs) {
        if leg.mode != LegMode::Flight {
            pts.extend(drawn_path(leg));
        }
        for n in [&journey.nodes[i], &journey.nodes[i + 1]] {
            if !n.home {
                pts.push(n.pos);
            }
        }
    }
    pts
}

/// A screen polyline with arc lengths.
struct Track {
    pts: Vec<P>,
    cum: Vec<f64>,
}

impl Track {
    fn new(pts: Vec<P>) -> Self {
        let mut cum = vec![0.0];
        for w in pts.windows(2) {
            cum.push(cum[cum.len() - 1] + (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1));
        }
        Self { pts, cum }
    }

    fn len(&self) -> f64 {
        self.cum[self.cum.len() - 1]
    }

    fn at(&self, d: f64) -> P {
        let d = d.clamp(0.0, self.len());
        let i = self
            .cum
            .partition_point(|&c| c < d)
            .clamp(1, self.pts.len() - 1);
        let (c0, c1) = (self.cum[i - 1], self.cum[i]);
        let f = if c1 > c0 { (d - c0) / (c1 - c0) } else { 0.0 };
        let (a, b) = (self.pts[i - 1], self.pts[i]);
        (a.0 + (b.0 - a.0) * f, a.1 + (b.1 - a.1) * f)
    }

    /// The stretch from `d0` to `d1`.
    fn piece(&self, d0: f64, d1: f64) -> Vec<P> {
        let mut out = vec![self.at(d0)];
        out.extend(
            self.pts
                .iter()
                .zip(&self.cum)
                .filter(|(_, &c)| c > d0 && c < d1)
                .map(|(p, _)| *p),
        );
        out.push(self.at(d1));
        out
    }
}

/// The inked route so far, kept as coverage so overlapping strokes never
/// darken twice: a cream underlay and the red line over it.
struct Ink {
    w: usize,
    h: usize,
    under: Vec<u8>,
    line: Vec<u8>,
    /// Rows touched so far.
    rows: (usize, usize),
    /// How far along the track the ink reaches.
    upto: f64,
}

impl Ink {
    fn new((w, h): (u32, u32)) -> Self {
        let n = (w * h) as usize;
        Self {
            w: w as usize,
            h: h as usize,
            under: vec![0; n],
            line: vec![0; n],
            rows: (usize::MAX, 0),
            upto: 0.0,
        }
    }

    /// Ink the track's dashes out to distance `d` (`u` is pixels per unit).
    fn extend(&mut self, track: &Track, d: f64, u: f64) {
        let (on, period) = (DASH_ON * u, (DASH_ON + DASH_OFF) * u);
        let mut s = self.upto;
        while s < d {
            let start = (s / period).floor() * period;
            let (dash_end, next) = (start + on, start + period);
            if s < dash_end {
                let e = dash_end.min(d);
                for w in track.piece(s, e).windows(2) {
                    self.stroke(w[0], w[1], u);
                }
            }
            s = next.min(d);
        }
        self.upto = d.max(self.upto);
    }

    fn stroke(&mut self, a: P, b: P, u: f64) {
        let shift = 0.8 * u;
        Self::capsule(
            &mut self.under,
            (self.w, self.h),
            &mut self.rows,
            (a.0 + shift, a.1 + shift),
            (b.0 + shift, b.1 + shift),
            3.0 * u,
        );
        Self::capsule(
            &mut self.line,
            (self.w, self.h),
            &mut self.rows,
            a,
            b,
            2.0 * u,
        );
    }

    fn capsule(
        mask: &mut [u8],
        (w, h): (usize, usize),
        rows: &mut (usize, usize),
        a: P,
        b: P,
        r: f64,
    ) {
        let reach = r + 1.0;
        let x0 = (a.0.min(b.0) - reach).floor().max(0.0) as usize;
        let y0 = (a.1.min(b.1) - reach).floor().max(0.0) as usize;
        let x1 = ((a.0.max(b.0) + reach).ceil().max(0.0) as usize).min(w.saturating_sub(1));
        let y1 = ((a.1.max(b.1) + reach).ceil().max(0.0) as usize).min(h.saturating_sub(1));
        for y in y0..=y1 {
            for x in x0..=x1 {
                let d = crate::routeanim::point_seg_dist((x as f64 + 0.5, y as f64 + 0.5), a, b);
                let cov = ((r + 0.5 - d).clamp(0.0, 1.0) * 255.0) as u8;
                let px = &mut mask[y * w + x];
                *px = (*px).max(cov);
            }
        }
        if y0 <= y1 && x0 <= x1 {
            *rows = (rows.0.min(y0), rows.1.max(y1));
        }
    }

    fn paint(&self, img: &mut RgbImage) {
        if self.rows.0 > self.rows.1 {
            return;
        }
        for y in self.rows.0..=self.rows.1 {
            for x in 0..self.w {
                let i = y * self.w + x;
                let (under, line) = (self.under[i], self.line[i]);
                if under > 0 {
                    blend_px(
                        img,
                        x as i32,
                        y as i32,
                        CREAM_RGB,
                        0.5 * f64::from(under) / 255.0,
                    );
                }
                if line > 0 {
                    blend_px(img, x as i32, y as i32, RED_RGB, f64::from(line) / 255.0);
                }
            }
        }
    }
}

/// Draw `sprite` centred on `c`, scaled, turned by `angle` (radians) and
/// faded by `alpha`. Bilinear, with colour weighted by coverage so edges
/// stay clean.
fn stamp(
    img: &mut RgbImage,
    sprite: &RgbaImage,
    c: P,
    (sx, sy): (f64, f64),
    angle: f64,
    alpha: f64,
) {
    if alpha <= 0.0 || sx == 0.0 || sy == 0.0 {
        return;
    }
    let (sw, sh) = (f64::from(sprite.width()), f64::from(sprite.height()));
    let reach = (sw * sx.abs()).hypot(sh * sy.abs()) / 2.0 + 1.0;
    let (sin, cos) = angle.sin_cos();
    let texel = |x: i64, y: i64| -> [f64; 4] {
        if x < 0 || y < 0 || x >= sprite.width().into() || y >= sprite.height().into() {
            return [0.0; 4];
        }
        let p = sprite.get_pixel(x as u32, y as u32).0;
        let a = f64::from(p[3]) / 255.0;
        [
            f64::from(p[0]) * a,
            f64::from(p[1]) * a,
            f64::from(p[2]) * a,
            a,
        ]
    };
    for y in (c.1 - reach).floor() as i32..=(c.1 + reach).ceil() as i32 {
        for x in (c.0 - reach).floor() as i32..=(c.0 + reach).ceil() as i32 {
            let (dx, dy) = (f64::from(x) + 0.5 - c.0, f64::from(y) + 0.5 - c.1);
            // Back through the turn and the scale into sprite pixels.
            let (tx, ty) = (
                (dx * cos + dy * sin) / sx + sw / 2.0 - 0.5,
                (dy * cos - dx * sin) / sy + sh / 2.0 - 0.5,
            );
            if tx < -1.0 || ty < -1.0 || tx > sw || ty > sh {
                continue;
            }
            let (fx, fy) = (tx - tx.floor(), ty - ty.floor());
            let (x0, y0) = (tx.floor() as i64, ty.floor() as i64);
            let mut acc = [0.0; 4];
            for (ox, oy, wgt) in [
                (0, 0, (1.0 - fx) * (1.0 - fy)),
                (1, 0, fx * (1.0 - fy)),
                (0, 1, (1.0 - fx) * fy),
                (1, 1, fx * fy),
            ] {
                let t = texel(x0 + ox, y0 + oy);
                for (a, v) in acc.iter_mut().zip(t) {
                    *a += v * wgt;
                }
            }
            if acc[3] > 0.003 {
                let col = Rgb([
                    (acc[0] / acc[3]).round().clamp(0.0, 255.0) as u8,
                    (acc[1] / acc[3]).round().clamp(0.0, 255.0) as u8,
                    (acc[2] / acc[3]).round().clamp(0.0, 255.0) as u8,
                ]);
                blend_px(img, x, y, col, acc[3] * alpha);
            }
        }
    }
}

/// A pin: red with a cream rim.
fn pin(img: &mut RgbImage, c: P, r: f64, u: f64) {
    crate::routeanim::blend_shape(img, c, r + 1.6 * u, None, CREAM_RGB, 1.0);
    crate::routeanim::blend_shape(img, c, r, None, RED_RGB, 1.0);
}

/// Pop-in scale for something that appears at `t0`: grows past full size
/// and settles (a back-out ease).
fn pop(t: f64, t0: f64) -> f64 {
    let x = (t - t0) / 0.35;
    if x <= 0.0 {
        0.0
    } else if x >= 1.0 {
        1.0
    } else {
        1.0 + 2.2 * (x - 1.0).powi(3) + 1.2 * (x - 1.0).powi(2)
    }
}

/// A sticker fitted into its box at `u` pixels per unit.
fn load_sticker(path: &Path, u: f64) -> Result<RgbaImage, MediaError> {
    let img = image::open(path)
        .map_err(|e| MediaError::Io(format!("sticker {}: {e}", path.display())))?
        .to_rgba8();
    let side = (STICKER_BOX * u).round().max(1.0) as u32;
    Ok(image::imageops::thumbnail(
        &img,
        side.min(side * img.width() / img.height().max(1)).max(1),
        side.min(side * img.height() / img.width().max(1)).max(1),
    ))
}

/// Where the vehicle is on a hop: which leg's kind applies up to a distance.
struct Stretch {
    end: f64,
    mode: LegMode,
    ship: usize,
}

struct HopScene {
    sheet: RgbImage,
    track: Track,
    stretches: Vec<Stretch>,
    ink: Ink,
    u: f64,
    secs: f64,
    plane: Option<RgbaImage>,
    ships: Vec<Option<RgbaImage>>,
    /// Arrival plaque and its centre.
    plaque: Option<(RgbaImage, P)>,
    /// Boarding tag and its centre.
    tag: Option<(RgbaImage, P)>,
    sticker: Option<(RgbaImage, P)>,
}

impl HopScene {
    fn build(
        journey: &Journey,
        hop: &Hop,
        (w, h): (u32, u32),
        typst_bin: &str,
        work_dir: &Path,
        tag_name: &str,
        notes: &mut Vec<String>,
    ) -> Result<Self, MediaError> {
        let u = f64::from(h) / DESIGN_H;
        let frame = (f64::from(w) / u, DESIGN_H);
        let legs = &journey.legs[hop.legs.clone()];
        let last = &legs[legs.len() - 1];
        let own: Vec<LatLon> = legs.iter().flat_map(drawn_path).collect();
        let view = if legs.iter().any(|l| l.mode == LegMode::Flight) {
            View::fit(&own, 0.25, 30.0, frame)
        } else if last.is_loop {
            View::fit(&own, 0.6, 3.0, frame)
        } else {
            View::fit(&regional(journey, hop.legs.end), 0.4, 5.0, frame)
        };

        // The sheet: legs already travelled in faint ink, stops already made.
        let mut under = String::new();
        for leg in &journey.legs[..hop.legs.start] {
            let pts: Vec<P> = drawn_path(leg).iter().map(|&p| view.xy(p)).collect();
            under += &route_svg(&pts, true);
        }
        for n in journey.nodes[..=hop.legs.start].iter().filter(|n| n.pin) {
            let (x, y) = view.xy(n.pos);
            let _ = write!(
                under,
                "<circle cx='{x:.1}' cy='{y:.1}' r='4.5' fill='{RED}' opacity='.55'/>"
            );
        }
        let stem = |part: &str| work_dir.join(format!("parchment_{tag_name}_{part}"));
        let sheet = flatten(&rasterize(
            typst_bin,
            &sheet_svg(&view, &under, ""),
            (w, h),
            &stem("sheet"),
        )?);

        // The hop's own line, in pixels, and what travels each part of it.
        let mut pts: Vec<P> = vec![];
        let mut stretches = vec![];
        let ship_names = journey.ships();
        for leg in legs {
            pts.extend(drawn_path(leg).iter().map(|&p| {
                let (x, y) = view.xy(p);
                (x * u, y * u)
            }));
            stretches.push(Stretch {
                end: Track::new(pts.clone()).len(),
                mode: leg.mode,
                ship: leg
                    .ship
                    .as_ref()
                    .and_then(|s| ship_names.iter().position(|n| n == s))
                    .unwrap_or(0),
            });
        }
        let track = Track::new(pts);

        // A sprite that fails to render is left out and noted, never fatal.
        let skipped = std::cell::RefCell::new(vec![]);
        let try_sprite = |what: &str, made: Result<RgbaImage, MediaError>| match made {
            Ok(img) => Some(img),
            Err(e) => {
                skipped
                    .borrow_mut()
                    .push(format!("travel map {what} skipped: {e}"));
                None
            }
        };
        let plane = stretches
            .iter()
            .any(|s| s.mode == LegMode::Flight)
            .then(|| {
                try_sprite(
                    "plane",
                    sprite(
                        typst_bin,
                        &format!("<g transform='scale(.85)'>{PLANE_SVG}</g>"),
                        PLANE_BOX,
                        (PLANE_BOX.0 / 2.0, PLANE_BOX.1 / 2.0),
                        u,
                        &stem("plane"),
                    ),
                )
            })
            .flatten();
        let ships: Vec<Option<RgbaImage>> = (0..ship_names.len())
            .map(|i| {
                stretches
                    .iter()
                    .any(|s| s.mode == LegMode::Ship && s.ship == i)
                    .then(|| {
                        try_sprite(
                            "ship",
                            sprite(
                                typst_bin,
                                &ship_svg(i),
                                SHIP_BOX,
                                (0.0, 0.0),
                                u,
                                &stem(&format!("ship{i}")),
                            ),
                        )
                    })
                    .flatten()
            })
            .collect();

        // Arrival plaque: beside the pin, off the line where it can be.
        let arrive = track.at(track.len());
        let (ax, ay) = (arrive.0 / u, arrive.1 / u);
        let ink: Vec<P> = sampled(&track.pts)
            .iter()
            .map(|p| (p.0 / u, p.1 / u))
            .collect();
        // The vehicle comes to rest on the pin.
        let mut taken: Vec<Rect> = vec![(ax - 36.0, ay - 46.0, ax + 36.0, ay + 8.0)];
        let plaque_sprite = |text: &str, size: f64, spot: P, part: &str| {
            let (lw, lh) = plaque_size(text, size);
            try_sprite(
                part,
                sprite(
                    typst_bin,
                    &plaque_svg(text, size),
                    (lw + 6.0, lh + 8.0),
                    (1.5, 1.5),
                    u,
                    &stem(part),
                ),
            )
            .map(|img| {
                (
                    img,
                    ((spot.0 + lw / 2.0 + 1.5) * u, (spot.1 + lh / 2.0 + 2.5) * u),
                )
            })
        };
        let mut plaque = None;
        if let Some(text) = journey.hop_text(hop) {
            let (lw, lh) = plaque_size(&text, 25.0);
            let spots = [
                (ax + 34.0, ay - 92.0),
                (ax + 34.0, ay + 50.0),
                (ax - 34.0 - lw, ay - 92.0),
                (ax - 34.0 - lw, ay + 50.0),
                (ax - lw / 2.0, ay - 112.0),
                (ax - lw / 2.0, ay + 60.0),
            ];
            let spot = pick_spot(&spots, (lw, lh), frame, 30.0, &taken, &ink);
            taken.push((spot.0, spot.1, spot.0 + lw, spot.1 + lh));
            plaque = plaque_sprite(&text, 25.0, spot, "plaque");
        }
        let sticker = journey.nodes[hop.node]
            .sticker
            .as_deref()
            .filter(|_| !last.is_loop)
            .and_then(|p| try_sprite("sticker", load_sticker(p, u)))
            .map(|img| {
                let b = STICKER_BOX;
                let spots = [
                    (ax - b - 30.0, ay - b / 2.0),
                    (ax + 40.0, ay - b / 2.0),
                    (ax - b / 2.0, ay + 20.0),
                    (ax - b / 2.0, ay - b - 56.0),
                    (ax - b - 30.0, ay + 14.0),
                    (ax + 40.0, ay + 14.0),
                ];
                let spot = pick_spot(&spots, (b, b), frame, 30.0, &taken, &ink);
                taken.push((spot.0, spot.1, spot.0 + b, spot.1 + b));
                (img, ((spot.0 + b / 2.0) * u, (spot.1 + b / 2.0) * u))
            });

        // Boarding tag: beside the start of the hop, clear of the arrival.
        let mut tag = None;
        if let Some(text) = legs
            .iter()
            .filter(|l| !l.is_loop)
            .find_map(|l| l.boarding_tag.as_deref())
        {
            let (tw, th) = plaque_size(text, 19.0);
            let start = track.at(0.0);
            let (sx, sy) = (start.0 / u, start.1 / u);
            let spots = [
                (sx + 26.0, sy - 30.0 - th),
                (sx - 26.0 - tw, sy - 30.0 - th),
                (sx + 26.0, sy + 30.0),
                (sx - 26.0 - tw, sy + 30.0),
                (sx - tw / 2.0, sy + 44.0),
                (sx - tw / 2.0, sy - 44.0 - th),
            ];
            let spot = pick_spot(&spots, (tw, th), frame, 30.0, &taken, &ink);
            tag = plaque_sprite(text, 19.0, spot, "tag");
        }

        notes.extend(skipped.into_inner());
        Ok(Self {
            sheet,
            track,
            stretches,
            ink: Ink::new((w, h)),
            u,
            secs: hop.secs,
            plane,
            ships,
            plaque,
            tag,
            sticker,
        })
    }

    fn frame(&mut self, f: u32, frames: u32) -> RgbImage {
        let (u, t) = (self.u, f64::from(f) / f64::from(FPS));
        // The line waits out the cut into the map, and lands with time to
        // read the plaque before the cut away.
        let landed = (self.secs - HOP_HOLD_SECS).max(HOP_LEAD_SECS + 0.5);
        let lin = ((t - HOP_LEAD_SECS) / (landed - HOP_LEAD_SECS)).clamp(0.0, 1.0);
        // A gentle ease: mostly steady, softened at both ends.
        let mut p = lin * lin * (3.0 - 2.0 * lin) * 0.35 + lin * 0.65;
        if f + 1 >= frames {
            p = 1.0;
        }
        let total = self.track.len();
        let d = p * total;
        self.ink.extend(&self.track, d, u);
        let mut img = self.sheet.clone();
        self.ink.paint(&mut img);

        // Boarding tag: there while the vehicle sets off, gone before it lands.
        if let Some((tag, c)) = &self.tag {
            let alpha = ((t - 0.15) / 0.25)
                .clamp(0.0, 1.0)
                .min(((landed - 0.1 - t) / 0.3).clamp(0.0, 1.0));
            stamp(&mut img, tag, *c, (1.0, 1.0), 0.0, alpha);
        }

        // Arrival: the pin, then the plaque, then the sticker.
        let arrive = self.track.at(total);
        let k = pop(t, landed);
        if k > 0.0 {
            pin(&mut img, arrive, 7.0 * u * k, u);
        }
        if let Some((sticker, c)) = &self.sticker {
            let k2 = pop(t, landed + STICKER_DELAY_SECS);
            stamp(
                &mut img,
                sticker,
                *c,
                (k2, k2),
                (-6.0 * k2).to_radians(),
                1.0,
            );
        }

        // The vehicle, heading the way the line runs.
        let here = self.track.at(d);
        let step = 4.0 * u;
        let (from, to) = if d + step <= total {
            (here, self.track.at(d + step))
        } else {
            (self.track.at((d - step).max(0.0)), here)
        };
        let stretch = self
            .stretches
            .iter()
            .find(|s| d <= s.end)
            .unwrap_or(&self.stretches[self.stretches.len() - 1]);
        match stretch.mode {
            LegMode::Flight => {
                if let Some(plane) = &self.plane {
                    let heading = (to.1 - from.1).atan2(to.0 - from.0);
                    stamp(&mut img, plane, here, (1.0, 1.0), heading, 1.0);
                }
            }
            LegMode::Ship => {
                if let Some(ship) = self.ships.get(stretch.ship).and_then(Option::as_ref) {
                    let bob = 2.5 * u * (t * 7.5).sin();
                    let facing = if to.0 < from.0 { -1.0 } else { 1.0 };
                    stamp(
                        &mut img,
                        ship,
                        (here.0, here.1 - 20.0 * u + bob),
                        (facing, 1.0),
                        0.0,
                        1.0,
                    );
                }
            }
            LegMode::Ground => pin(&mut img, here, 5.0 * u, u),
        }
        if let Some((plaque, c)) = &self.plaque {
            stamp(&mut img, plaque, *c, (k, k), 0.0, 1.0);
        }
        img
    }
}

/// Render hop `index` of the journey (see [`Journey::hops`]) at `res` into
/// `out_mp4`: H.264 with stereo silence, like the other map segments.
pub fn render_hop(
    caps: &Capabilities,
    journey: &Journey,
    index: usize,
    res: Resolution,
    typst_bin: &str,
    work_dir: &Path,
    out_mp4: &Path,
) -> Result<RecapReport, MediaError> {
    render_hop_sized(
        caps,
        journey,
        index,
        res.dims(),
        typst_bin,
        work_dir,
        out_mp4,
    )
}

pub(crate) fn render_hop_sized(
    caps: &Capabilities,
    journey: &Journey,
    index: usize,
    size: (u32, u32),
    typst_bin: &str,
    work_dir: &Path,
    out_mp4: &Path,
) -> Result<RecapReport, MediaError> {
    let hops = journey.hops();
    let hop = hops
        .get(index)
        .ok_or_else(|| MediaError::InvalidArg(format!("journey has no hop {index}")))?;
    let mut notes = vec![];
    let mut scene = HopScene::build(
        journey,
        hop,
        size,
        typst_bin,
        work_dir,
        &format!("hop{index:02}"),
        &mut notes,
    )?;
    let frames = frame_count(hop.secs);
    let secs = f64::from(frames) / f64::from(FPS);
    encode(caps, size, frames, secs, out_mp4, |f| {
        scene.frame(f, frames)
    })?;
    Ok(RecapReport { secs, notes })
}

/// Lead-in and hold added around a ready-made hop clip, so the crossfades
/// into and out of it do not eat its first stroke or its arrival.
const CLIP_LEAD_SECS: f64 = 0.4;
const CLIP_HOLD_SECS: f64 = 1.2;

/// Video filter that fits a ready-made hop clip to `w` x `h` at the film's
/// frame rate and holds its first and last frames.
fn clip_filter((w, h): (u32, u32)) -> String {
    format!(
        "scale={w}:{h}:force_original_aspect_ratio=decrease:flags=lanczos,\
         pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,fps={FPS},\
         tpad=start_mode=clone:start_duration={CLIP_LEAD_SECS}:stop_mode=clone:stop_duration={CLIP_HOLD_SECS},\
         format=yuv420p"
    )
}

/// Use the owner's own clip for a hop (one they made elsewhere) instead of
/// drawing it: fitted to `res`, with stereo silence like the other map
/// segments.
pub fn adopt_hop_clip(
    caps: &Capabilities,
    clip: &Path,
    res: Resolution,
    out_mp4: &Path,
) -> Result<RecapReport, MediaError> {
    let info = crate::ffmpeg::ffprobe_file(&caps.ffprobe_bin, clip)?;
    if !info.has_video || info.duration_secs <= 0.0 {
        return Err(MediaError::InvalidArg(format!(
            "{} is not a video clip",
            clip.display()
        )));
    }
    let secs = info.duration_secs + CLIP_LEAD_SECS + CLIP_HOLD_SECS;
    let args: Vec<String> = [
        vec![
            "-y".into(),
            "-i".into(),
            clip.to_string_lossy().into_owned(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            format!("anullsrc=r=48000:cl=stereo:d={secs}"),
            "-vf".into(),
            clip_filter(res.dims()),
            "-map".into(),
            "0:v:0".into(),
            "-map".into(),
            "1:a".into(),
            "-t".into(),
            format!("{secs}"),
        ],
        crate::ffmpeg::x264_aac("veryfast", 17),
        vec![out_mp4.to_string_lossy().into_owned()],
    ]
    .concat();
    caps.run(&args, out_mp4)?;
    Ok(RecapReport {
        secs,
        notes: vec![],
    })
}

/// The whole route on one sheet at `size` pixels: every leg inked, every
/// stop pinned and named with its date, and `title` on a banner along the
/// top when given. Flights home run off the sheet; the map frames where
/// the trip went.
pub fn render_overview(
    journey: &Journey,
    title: Option<&str>,
    size: (u32, u32),
    typst_bin: &str,
    work_dir: &Path,
) -> Result<RgbImage, MediaError> {
    let u = f64::from(size.1) / DESIGN_H;
    let frame = (f64::from(size.0) / u, DESIGN_H);
    let banner = if title.is_some() { 64.0 } else { 0.0 };
    let mut held = regional(journey, journey.legs.len());
    if held.is_empty() {
        held = journey.legs.iter().flat_map(|l| l.path.clone()).collect();
    }
    // Fitted below the banner, then slid down under it.
    let mut view = View::fit(&held, 0.4, 8.0, (frame.0, frame.1 - banner));
    view.y_top += banner / view.k;
    view.h = frame.1;

    // On a narrow sheet (a book page) the lettering is smaller.
    let scale = (frame.0 / 1280.0).sqrt().min(1.0);
    let (label_size, title_size) = (19.0 * scale, 27.0 * scale);

    // The trip's own legs in full ink; a flight from or to home, most of
    // which is off the sheet, stays faint behind them.
    let mut under = String::new();
    let mut ink: Vec<P> = vec![];
    for (i, leg) in journey.legs.iter().enumerate() {
        let pts: Vec<P> = drawn_path(leg).iter().map(|&p| view.xy(p)).collect();
        let faint = leg.mode == LegMode::Flight
            && (journey.nodes[i].home || journey.nodes[i + 1].home)
            && !(view.holds(pts[0], 0.0) && view.holds(pts[pts.len() - 1], 0.0));
        if faint {
            under.insert_str(0, &route_svg(&pts, true));
        } else {
            under += &route_svg(&pts, false);
            ink.extend(sampled(&pts));
        }
    }

    let anchors: Vec<P> = journey.nodes.iter().map(|n| view.xy(n.pos)).collect();
    // Plaques keep clear of the banner, the compass and every pin.
    let mut taken: Vec<Rect> = vec![(frame.0 - 160.0, frame.1 - 175.0, frame.0, frame.1)];
    let mut over = String::new();
    if let Some(title) = title {
        let (tw, th) = plaque_size(title, title_size);
        let (x, y) = ((frame.0 - tw) / 2.0, 34.0);
        taken.push((x, y, x + tw, y + th));
        let _ = write!(
            over,
            "<g transform='translate({x:.1},{y:.1})'>{}</g>",
            plaque_svg(title, title_size)
        );
    }
    // A stop is named once, where the trip first reaches it, and only when
    // its pin is on the sheet.
    let named: Vec<(usize, String)> = (0..journey.nodes.len())
        .filter_map(|i| {
            let n = &journey.nodes[i];
            let seen = journey.nodes[..i]
                .iter()
                .any(|m| m.label.is_some() && m.label == n.label);
            let on_sheet = view.holds(anchors[i], 24.0);
            journey
                .node_text(i)
                .filter(|_| n.label.is_some() && !seen && on_sheet)
                .map(|t| (i, t))
        })
        .collect();
    taken.extend(named.iter().map(|&(i, _)| {
        let (x, y) = anchors[i];
        (x - 9.0, y - 9.0, x + 9.0, y + 9.0)
    }));
    for (i, text) in &named {
        let (ax, ay) = anchors[*i];
        let (lw, lh) = plaque_size(text, label_size);
        // Nearest spots first: around the pin, then farther rings.
        let spots: Vec<P> = [14.0, 30.0, 52.0, 80.0, 120.0]
            .iter()
            .flat_map(|&g| {
                [
                    (ax + g, ay - g - lh),
                    (ax + g, ay + g),
                    (ax - g - lw, ay - g - lh),
                    (ax - g - lw, ay + g),
                    (ax + g + 4.0, ay - lh / 2.0),
                    (ax - g - 4.0 - lw, ay - lh / 2.0),
                    (ax - lw / 2.0, ay - g - 4.0 - lh),
                    (ax - lw / 2.0, ay + g + 4.0),
                ]
            })
            .collect();
        let (x, y) = pick_spot(&spots, (lw, lh), frame, 26.0, &taken, &ink);
        taken.push((x, y, x + lw, y + lh));
        // A plaque set away from its pin is tied back with a leader.
        let edge = (ax.clamp(x, x + lw), ay.clamp(y, y + lh));
        if (edge.0 - ax).hypot(edge.1 - ay) > 24.0 {
            let _ = write!(
                over,
                "<line x1='{ax:.1}' y1='{ay:.1}' x2='{:.1}' y2='{:.1}' stroke='#4a2f14' stroke-width='1.4'/>",
                edge.0, edge.1
            );
        }
        let _ = write!(
            over,
            "<circle cx='{ax:.1}' cy='{ay:.1}' r='6' fill='{RED}' stroke='#f2e3bd' stroke-width='1.8'/>\
             <g transform='translate({x:.1},{y:.1})'>{}</g>",
            plaque_svg(text, label_size)
        );
    }
    Ok(flatten(&rasterize(
        typst_bin,
        &sheet_svg(&view, &under, &over),
        size,
        &work_dir.join("parchment_overview"),
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use triptape_core::itinerary::Itinerary;

    const JSON: &str = r#"{
      "stops": [
        { "id": "lax-out", "label": "Los Angeles", "lat": 33.9416, "lon": -118.4085, "depart": "2026-10-03", "kind": "home" },
        { "id": "tokyo", "label": "Tokyo", "lat": 35.7148, "lon": 139.7967, "arrive": "2026-10-04", "depart": "2026-10-09" },
        { "id": "tokyo-port", "label": "Tokyo", "lat": 35.6177, "lon": 139.771, "arrive": "2026-10-09", "depart": "2026-10-09", "kind": "port", "hide_label": true },
        { "id": "osaka", "label": "Osaka", "lat": 34.656, "lon": 135.4306, "arrive": "2026-10-11", "depart": "2026-10-11", "kind": "port" },
        { "id": "singapore", "label": "Singapore", "lat": 1.2644, "lon": 103.8222, "arrive": "2026-10-23", "depart": "2026-10-30" },
        { "id": "lax-home", "label": "Los Angeles", "lat": 33.9416, "lon": -118.4085, "arrive": "2026-10-30", "kind": "home", "hide_label": true }
      ],
      "legs": [
        { "from": "lax-out", "to": "tokyo", "mode": "flight" },
        { "from": "tokyo", "to": "tokyo-port", "mode": "ground" },
        { "from": "tokyo-port", "to": "osaka", "mode": "ship", "ship": "Carnival Luminosa" },
        { "from": "osaka", "to": "singapore", "mode": "ship", "ship": "Carnival Luminosa" },
        { "from": "singapore", "to": "singapore", "mode": "ship", "ship": "Disney Adventure", "loop": true, "dates": "2026-10-26..2026-10-29" },
        { "from": "singapore", "to": "lax-home", "mode": "flight" }
      ]
    }"#;

    fn journey() -> Journey {
        Journey::build(&Itinerary::from_json(JSON).unwrap(), &[])
    }

    #[test]
    fn view_fits_points_and_keeps_shapes_true() {
        let pts = [(35.0, 135.0), (1.0, 104.0)];
        let v = View::fit(&pts, 0.3, 5.0, (1280.0, 720.0));
        for p in pts {
            assert!(v.holds(v.xy(p), 20.0), "{p:?} at {:?}", v.xy(p));
        }
        // Mercator: a degree of latitude is taller far from the equator.
        let tall = |lat: f64| v.xy((lat, 120.0)).1 - v.xy((lat + 1.0, 120.0)).1;
        assert!(tall(60.0) > 1.8 * tall(0.0));
        // A single point still gets a window of the minimum span.
        let one = View::fit(&[(35.0, 135.0)], 0.3, 5.0, (1280.0, 720.0));
        assert!((one.k - 1280.0 / 5.0).abs() < 1e-6);
    }

    #[test]
    fn clipping_keeps_what_is_inside() {
        let square = [(-10.0, -10.0), (10.0, -10.0), (10.0, 10.0), (-10.0, 10.0)];
        let cut = clip(&square, (0.0, 0.0, 100.0, 100.0));
        assert_eq!(cut.len(), 4);
        assert!(cut
            .iter()
            .all(|p| (0.0..=10.0).contains(&p.0) && (0.0..=10.0).contains(&p.1)));
        assert!(clip(&square, (50.0, 50.0, 100.0, 100.0)).is_empty());
    }

    #[test]
    fn simplifying_drops_points_on_a_line_and_keeps_corners() {
        let pts = [(0.0, 0.0), (5.0, 0.1), (10.0, 0.0), (10.0, 10.0)];
        assert_eq!(
            simplify(&pts, 0.5),
            vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)]
        );
    }

    #[test]
    fn land_is_drawn_for_japan_and_not_for_open_ocean() {
        let japan = View::fit(&[(31.0, 130.0), (41.0, 142.0)], 0.2, 5.0, (1280.0, 720.0));
        let (fill, coast) = land_paths(&japan);
        assert!(fill.len() > 2000 && coast.len() > 2000);
        let pacific = View::fit(&[(20.0, -150.0), (24.0, -144.0)], 0.0, 5.0, (1280.0, 720.0));
        assert!(land_paths(&pacific).0.is_empty());
    }

    #[test]
    fn a_view_across_the_antimeridian_has_land_on_both_sides() {
        // Tokyo to Los Angeles in continuous longitudes (LA at 241.6).
        let v = View::fit(&[(35.7, 139.8), (33.9, 241.6)], 0.2, 30.0, (1280.0, 720.0));
        let (fill, _) = land_paths(&v);
        let xs: Vec<f64> = fill
            .split(['M', 'L', 'Z'])
            .filter_map(|p| p.split_once(',')?.0.parse().ok())
            .collect();
        assert!(xs.iter().any(|&x| x < 300.0), "Asia");
        assert!(xs.iter().any(|&x| x > 1000.0), "America");
    }

    #[test]
    fn spots_stay_in_frame_off_the_line_and_off_each_other() {
        let frame = (400.0, 300.0);
        let spots = [(10.0, 10.0), (150.0, 100.0), (200.0, 200.0)];
        // The first is outside the margin; the second sits on the line.
        let ink = sampled(&[(100.0, 110.0), (300.0, 110.0)]);
        assert_eq!(
            pick_spot(&spots, (100.0, 30.0), frame, 26.0, &[], &ink),
            (200.0, 200.0)
        );
        // With the clear spot taken, the inked one is still better than none.
        let taken = [(190.0, 190.0, 310.0, 240.0)];
        assert_eq!(
            pick_spot(&spots, (100.0, 30.0), frame, 26.0, &taken, &ink),
            (150.0, 100.0)
        );
        // Nothing fits: the first candidate, pulled inside the margin.
        assert_eq!(
            pick_spot(&spots[..1], (100.0, 30.0), frame, 26.0, &[], &ink),
            (26.0, 26.0)
        );
    }

    #[test]
    fn a_sailing_is_drawn_with_rounded_corners_and_the_same_ends() {
        let j = journey();
        let sail = &j.legs[3];
        let drawn = drawn_path(sail);
        assert!(drawn.len() > sail.path.len());
        assert_eq!(drawn[0], sail.path[0]);
        assert_eq!(drawn[drawn.len() - 1], sail.path[sail.path.len() - 1]);
        // A flight keeps its exact arc.
        assert_eq!(drawn_path(&j.legs[0]), j.legs[0].path);
    }

    #[test]
    fn a_ready_made_clip_is_fitted_and_held_at_both_ends() {
        let f = clip_filter((1920, 1080));
        assert!(f.starts_with("scale=1920:1080:force_original_aspect_ratio=decrease"));
        assert!(f.contains("pad=1920:1080") && f.contains("fps=30"));
        assert!(f.contains("start_duration=0.4") && f.contains("stop_duration=1.2"));
        assert!(!f.contains(' '), "one filter chain, no stray spaces: {f}");
    }

    #[test]
    fn ink_draws_dashes_once_however_it_is_extended() {
        let track = Track::new(vec![(10.0, 20.0), (110.0, 20.0)]);
        let mut whole = Ink::new((120, 40));
        whole.extend(&track, 100.0, 1.0);
        let mut stepped = Ink::new((120, 40));
        for d in 1..=100 {
            stepped.extend(&track, f64::from(d), 1.0);
        }
        assert_eq!(whole.line, stepped.line);
        // On at the start of a dash, off in the gap.
        let at = |x: usize| whole.line[20 * 120 + x];
        assert_eq!(at(15), 255);
        assert_eq!(at(10 + 16), 0);
    }

    #[test]
    fn stamp_places_scales_and_turns_a_sprite() {
        let sprite = RgbaImage::from_pixel(10, 4, image::Rgba([200, 0, 0, 255]));
        let mut img = RgbImage::new(40, 40);
        stamp(&mut img, &sprite, (20.0, 20.0), (1.0, 1.0), 0.0, 1.0);
        assert_eq!(img.get_pixel(20, 20).0, [200, 0, 0]);
        assert_eq!(img.get_pixel(20, 26).0, [0, 0, 0]);
        // A quarter turn stands it on end.
        let mut turned = RgbImage::new(40, 40);
        stamp(
            &mut turned,
            &sprite,
            (20.0, 20.0),
            (1.0, 1.0),
            std::f64::consts::FRAC_PI_2,
            1.0,
        );
        assert_eq!(turned.get_pixel(20, 23).0, [200, 0, 0]);
        assert_eq!(turned.get_pixel(24, 20).0, [0, 0, 0]);
        // Scale 0 draws nothing; pop settles at full size.
        let mut none = RgbImage::new(40, 40);
        stamp(&mut none, &sprite, (20.0, 20.0), (0.0, 0.0), 0.0, 1.0);
        assert!(none.pixels().all(|p| p.0 == [0, 0, 0]));
        assert_eq!(pop(0.0, 1.0), 0.0);
        assert_eq!(pop(2.0, 1.0), 1.0);
        assert!(pop(1.25, 1.0) > 1.0, "overshoots before settling");
    }

    #[test]
    fn overview_and_hop_render_when_typst_is_there() {
        let typst = std::env::var("TRIPTAPE_TYPST").unwrap_or_else(|_| "typst".into());
        if !crate::titlecard::typst_available(&typst) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let j = journey();
        let img = render_overview(&j, Some("Asia 2026"), (640, 640), &typst, dir.path()).unwrap();
        assert_eq!(img.dimensions(), (640, 640));
        // Paper, not a blank page: warm and light in the middle of the sea.
        let mid = img.get_pixel(320, 520).0;
        assert!(mid[0] > mid[2] && mid[0] > 120, "{mid:?}");
        let mut notes = vec![];
        let hops = j.hops();
        let mut scene = HopScene::build(
            &j,
            &hops[1],
            (640, 360),
            &typst,
            dir.path(),
            "t",
            &mut notes,
        )
        .unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        let frames = frame_count(hops[1].secs);
        let first = scene.frame(0, frames);
        let last = scene.frame(frames - 1, frames);
        let red = |im: &RgbImage| {
            im.pixels()
                .filter(|p| p.0[0] > 150 && p.0[1] < 70 && p.0[2] < 60)
                .count()
        };
        assert!(red(&last) > red(&first) + 50, "the line was drawn");
        // Nothing is left behind in the work dir.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
