//! Keepsake 1 — Trip Video (hero film).
//!
//! Pipeline: normalize every plan entry to a uniform 4K30 (or 1080p30, see
//! [`Resolution`]) / s48kHz-stereo
//! segment (Ken Burns stills, scored clip trims, title card, map push-in),
//! then assemble with a stepwise pairwise crossfade chain (never 5+ inputs
//! in one graph; each offset accounts for cumulative overlap), a final
//! loudnorm single pass, libx264 CRF19 faststart, an EDL JSON, and a
//! sidecar SRT (burned in too when ffmpeg has libass).

use crate::ffmpeg::{self, path_arg as s, Capabilities};
use crate::resolution::Resolution;
use crate::segment_cache::SegmentCache;
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use triptape_core::plan::{
    self, caption_hold_secs, caption_read_secs, collage_hold_secs, is_panorama, pano_coverage,
    pano_motion, places_card_secs, MAX_CLIP_SECS, MAX_COLLAGE, MIN_SEGMENT_SECS,
};
use triptape_core::types::{
    frame_aspect, Chapter, CollageEdits, Frame, Plan, PlanEntry, Rendition,
};

/// Crossfade applied at the cut *into* a segment (PLAN.md Sec 9, 6-pack).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Transition {
    Fade,
    Dissolve,
    /// Signature look: dissolve filter; the blur ramp is omitted on builds
    /// without per-tail filter resources (recorded in the EDL/fallbacks).
    BlurDissolve,
    FadeBlack,
    FadeWhite,
    SmoothLeft,
    SmoothRight,
    CircleOpen,
    CircleClose,
}

impl Transition {
    /// Concrete `xfade` filter name (all supported by stock FFmpeg).
    pub fn xfade_name(&self) -> &'static str {
        match self {
            Transition::Fade => "fade",
            Transition::Dissolve | Transition::BlurDissolve => "dissolve",
            Transition::FadeBlack => "fadeblack",
            Transition::FadeWhite => "fadewhite",
            Transition::SmoothLeft => "smoothleft",
            Transition::SmoothRight => "smoothright",
            Transition::CircleOpen => "circleopen",
            Transition::CircleClose => "circleclose",
        }
    }
}

/// Pure xfade timeline math (uniform overlap): cut `i` (into segment
/// `i+1`) sits at `sum(durations[0..=i]) - overlap*(i+1)`. Stepwise assembly
/// must use these offsets so overlap never compounds.
pub fn xfade_offsets(durations: &[f64], overlap: f64) -> Vec<f64> {
    let n = durations.len().saturating_sub(1);
    xfade_offsets_var(durations, &vec![overlap; n])
}

/// Per-cut-overlap timeline math: cut `i` sits at
/// `sum(durations[0..=i]) - sum(overlaps[0..=i])`. A missing overlap entry
/// falls back to 0.5 s.
pub fn xfade_offsets_var(durations: &[f64], overlaps: &[f64]) -> Vec<f64> {
    let mut offsets = vec![];
    let mut acc = 0.0;
    let mut ov = 0.0;
    for (i, d) in durations.iter().enumerate() {
        acc += d;
        if i + 1 < durations.len() {
            ov += overlaps.get(i).copied().unwrap_or(0.5);
            offsets.push(acc - ov);
        }
    }
    offsets
}

/// Inputs `0..n` chained through xfade (and acrossfade when
/// `keep_ambience`), each filter ending in `;`. Returns the graph and the
/// labels of the finished video and audio streams: `[v]`/`[a]`, or the lone
/// input's `[0:v]`/`[0:a]` when there is no cut. With `scale` (a draft's
/// frame size) every input is scaled to it before its cut, as `[s0]`,
/// `[s1]`..., so the cuts and all after them run on the small frame.
fn xfade_chain(
    transitions: &[Option<Transition>],
    overlaps: &[f64],
    offsets: &[f64],
    keep_ambience: bool,
    scale: Option<(u32, u32)>,
) -> (String, String, String) {
    let n = transitions.len();
    let mut graph = String::new();
    let vin = |i: usize| match scale {
        Some(_) => format!("[s{i}]"),
        None => format!("[{i}:v]"),
    };
    if let Some((w, h)) = scale {
        for i in 0..n {
            graph.push_str(&format!("[{i}:v]{}{};", ffmpeg::draft_scale(w, h), vin(i)));
        }
    }
    let (mut vl, mut al) = (vin(0), "[0:a]".to_string());
    for i in 1..n {
        let t = transitions[i].unwrap_or(Transition::Fade);
        let ov = overlaps[i - 1];
        let offset = offsets[i - 1].max(0.0);
        let tag = |p: &str| {
            if i == n - 1 {
                format!("[{p}]")
            } else {
                format!("[{p}{i}]")
            }
        };
        let nv = tag("v");
        graph.push_str(&format!(
            "{vl}{}xfade=transition={}:duration={ov:.3}:offset={offset:.3}{nv};",
            vin(i),
            t.xfade_name(),
        ));
        vl = nv;
        if keep_ambience {
            let na = tag("a");
            graph.push_str(&format!(
                "{al}[{i}:a]acrossfade=d={ov:.3}:c1=tri:c2=tri{na};"
            ));
            al = na;
        }
    }
    (graph, vl, al)
}

/// The single-encode filter graph for the hero film: segment inputs
/// `0..n` chained through xfade (and acrossfade when `keep_ambience`),
/// then the optional caption burn and the audio mix ending in `[aout]`.
/// `bed_graph` must emit `[mbed]`. `scale` as in [`xfade_chain`]: a
/// draft's captions are burned on the small frame. Returns the graph and
/// the `-map` target for the video stream.
#[allow(clippy::too_many_arguments)]
fn film_graph(
    transitions: &[Option<Transition>],
    cut_overlaps: &[f64],
    offsets: &[f64],
    keep_ambience: bool,
    burn_srt: Option<&str>,
    bed_graph: &str,
    norm: &str,
    scale: Option<(u32, u32)>,
) -> (String, String) {
    let (mut graph, vl, al) = xfade_chain(transitions, cut_overlaps, offsets, keep_ambience, scale);
    // `vl`/`al` now name the finished film streams ([v]/[a], or the lone
    // input's [0:v]/[0:a] when there is no cut).
    let video_map = match burn_srt {
        Some(sub) => {
            graph.push_str(&format!("{vl}{sub}[vsub];"));
            "[vsub]".to_string()
        }
        // A raw input stream is `0:v`; a filter-output label maps as-is.
        None if vl == "[0:v]" => "0:v".to_string(),
        None => vl,
    };
    if keep_ambience {
        graph.push_str(&format!(
            "{al}volume=0.12,aresample=48000,aformat=channel_layouts=stereo[film];{bed_graph};[mbed][film]sidechaincompress=threshold=0.02:ratio=8:attack=200:release=800[duck];[film][duck]amix=inputs=2:duration=first:dropout_transition=0,{norm}[aout]"
        ));
    } else {
        // Muted film audio: the music bed alone is the soundtrack.
        graph.push_str(&format!("{bed_graph};[mbed]{norm}[aout]"));
    }
    (graph, video_map)
}

/// Most segments joined in one ffmpeg graph. ffmpeg decodes every input
/// of a graph at once, so memory grows with the count: 159 4K segments
/// rendered, 226 did not.
const ASSEMBLY_CHUNK: usize = 40;

/// A chunk may end at a chapter start once it holds this share of
/// [`ASSEMBLY_CHUNK`] (a fifth: eight segments).
const CHUNK_MIN_SHARE: usize = 5;

/// Quality of a pre-joined chunk (x264, near-lossless): it is decoded once
/// more by the final encode.
const CHUNK_CRF: u32 = 12;

/// How the film is joined and encoded: the keepsake, or a draft for
/// reviewing an edit. Both use the same segments, cuts, captions and
/// music, so a draft shows the keepsake's timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recipe {
    /// Full size, near-lossless chunks, [`ffmpeg::film_encode_args`].
    Keepsake,
    /// Scaled to [`ffmpeg::DRAFT_RESOLUTION`] where the segments are
    /// joined, and encoded fast ([`ffmpeg::draft_encode_args`]).
    Draft(ffmpeg::DraftEncoder),
}

impl Recipe {
    /// The frame the joins scale every input to (`None`: left as it is).
    fn scale(self) -> Option<(u32, u32)> {
        match self {
            Self::Keepsake => None,
            Self::Draft(_) => Some(ffmpeg::DRAFT_RESOLUTION.dims()),
        }
    }

    /// Encode arguments of a pre-joined chunk.
    fn chunk_encode_args(self) -> Vec<String> {
        match self {
            Self::Keepsake => ffmpeg::x264_aac("veryfast", CHUNK_CRF),
            Self::Draft(e) => ffmpeg::draft_encode_args(e, ffmpeg::DRAFT_CHUNK_KBPS),
        }
    }

    /// Encode arguments of the finished film.
    fn film_encode_args(self, caps: &Capabilities, res: Resolution) -> Vec<String> {
        match self {
            Self::Keepsake => ffmpeg::film_encode_args(caps, res),
            Self::Draft(e) => ffmpeg::draft_encode_args(e, ffmpeg::DRAFT_FILM_KBPS),
        }
    }

    /// Frame size of the finished film.
    fn dims(self, res: Resolution) -> (u32, u32) {
        self.scale().unwrap_or(res.dims())
    }
}

/// The film's inputs for the final encode: the segments themselves, or
/// for a long film the pre-joined chunks, with the cuts between them.
#[derive(Debug, Clone, PartialEq)]
struct JoinedInputs {
    inputs: Vec<PathBuf>,
    durations: Vec<f64>,
    /// Transition into each input (`None` for the first), as
    /// [`assign_transitions`] gives them.
    transitions: Vec<Option<Transition>>,
    /// Overlap of each cut, `inputs.len() - 1` of them.
    overlaps: Vec<f64>,
}

/// Chunk boundaries of a film of `chapter_break.len()` segments
/// (`chapter_break[i]`: the cut into segment `i` starts a chapter). A film
/// of at most `chunk` segments (`chunk` at least two) is one run. A longer
/// one is cut where the story is: a chunk ends at the first chapter start
/// once it holds a fifth of `chunk` (more in a very long film, so the
/// final graph never joins more chunks than a chunk has segments), and at
/// `chunk` segments whatever the story. The last run may hold a single
/// segment.
///
/// Boundaries follow chapters instead of counting from the film's start, so
/// adding or removing a shot changes its own chunk and leaves the others'
/// segments as they were (at worst the next chunk's too, when its own
/// falls under the minimum or sits in a chapter longer than `chunk`). An
/// unchanged chunk is then reused from the chunk cache.
fn assembly_chunks(chapter_break: &[bool], chunk: usize) -> Vec<Range<usize>> {
    let n = chapter_break.len();
    let chunk = chunk.max(2);
    if n <= chunk {
        return vec![Range { start: 0, end: n }];
    }
    // The minimum doubles in wide bands of film length (8 up to 320
    // segments, 16 up to 640, ...) rather than growing with every 40, so
    // an edit moves chunk boundaries only when it crosses a band edge.
    let mut min = (chunk / CHUNK_MIN_SHARE).max(1);
    while min < chunk && n > chunk * min {
        min *= 2;
    }
    let min = min.min(chunk);
    let mut ranges = vec![];
    let mut start = 0;
    for (i, at_chapter) in chapter_break.iter().enumerate().skip(1) {
        let len = i - start;
        if len == chunk || (len >= min && *at_chapter) {
            ranges.push(start..i);
            start = i;
        }
    }
    ranges.push(start..n);
    ranges
}

/// ffmpeg arguments joining one chunk's segments (`inputs`, with their
/// `durations`, the `transitions` into each and the `overlaps` of the cuts
/// between them) into `out`.
fn chunk_join_args(
    inputs: &[PathBuf],
    durations: &[f64],
    transitions: &[Option<Transition>],
    overlaps: &[f64],
    keep_ambience: bool,
    recipe: Recipe,
    out: &Path,
) -> Vec<String> {
    let (graph, v, a) = xfade_chain(
        transitions,
        overlaps,
        &xfade_offsets_var(durations, overlaps),
        keep_ambience,
        recipe.scale(),
    );
    let mut args: Vec<String> = vec!["-y".into()];
    for p in inputs {
        args.extend(["-i".into(), s(p)]);
    }
    let graph = graph.trim_end_matches(';').to_string();
    args.extend(["-filter_complex".into(), graph, "-map".into(), v]);
    if keep_ambience {
        args.extend(["-map".into(), a]);
    }
    args.extend(recipe.chunk_encode_args());
    args.extend(["-r".into(), "30".into(), s(out)]);
    args
}

/// Cache key of a joined chunk: what each member segment holds
/// ([`segment_identity`], which carries the segment's resolution and
/// heading), and the join itself, as its ffmpeg arguments: every cut's
/// transition, overlap and offset, and the recipe's scale and encode
/// (a draft and the keepsake never share a chunk). Nothing in it counts
/// from the film's start, so a chunk keeps its key when another changes.
fn chunk_key(
    cache: &SegmentCache,
    members: &[String],
    durations: &[f64],
    transitions: &[Option<Transition>],
    overlaps: &[f64],
    keep_ambience: bool,
    recipe: Recipe,
) -> String {
    let mut k = cache.key("chunk");
    for m in members {
        k.part("member", m);
    }
    // Stand-in input names: a segment's file name carries its index.
    let inputs = vec![PathBuf::from(KEY_IN); members.len()];
    k.args(&chunk_join_args(
        &inputs,
        durations,
        transitions,
        overlaps,
        keep_ambience,
        recipe,
        Path::new(KEY_OUT),
    ));
    k.finish()
}

/// What the segment file `seg` holds, for its chunk's key: the segment
/// cache key it was rendered or overlaid under, else (cards, hop clips,
/// a film rendered without the segment cache) a hash of the file itself.
/// `None` when it cannot be read: its chunk is then joined uncached.
fn segment_identity(caps: &Capabilities, seg: &Path) -> Option<String> {
    let keyed = caps
        .segment_cache
        .as_deref()
        .and_then(|c| c.content_key_of(seg));
    if let Some(k) = keyed {
        return Some(format!("key:{k}"));
    }
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut std::fs::File::open(seg).ok()?, &mut hasher).ok()?;
    Some(format!("file:{}", hasher.finalize().to_hex()))
}

/// Pre-join a long film's segments into chunks (`ranges`, from
/// [`assembly_chunks`]), each with its own cuts at their transitions and
/// overlaps; the cuts between chunks are left for the final graph, so the
/// timeline is the same as one graph over every segment. A chunk the chunk
/// cache holds (see [`chunk_key`]) is taken from it instead of being joined
/// again. A film of one run passes through.
#[allow(clippy::too_many_arguments)]
fn prejoin_chunks(
    caps: &Capabilities,
    segs: &[PathBuf],
    durations: &[f64],
    transitions: &[Option<Transition>],
    overlaps: &[f64],
    keep_ambience: bool,
    work_dir: &Path,
    ranges: &[Range<usize>],
    recipe: Recipe,
    on_progress: &dyn Fn(f64),
) -> Result<JoinedInputs, crate::MediaError> {
    let plan = plan_chunks(durations, transitions, overlaps, ranges);
    let Some(ranges) = plan.ranges else {
        return Ok(JoinedInputs {
            inputs: segs.to_vec(),
            ..plan.joined
        });
    };
    // Progress over the whole join, weighted by each chunk's length.
    let total: f64 = plan.joined.durations.iter().sum::<f64>().max(1e-3);
    let mut done = 0.0;
    let mut inputs = vec![];
    for (k, r) in ranges.iter().enumerate() {
        // Cache hits skip ffmpeg, so they would not notice a cancel.
        caps.progress.check_cancelled()?;
        let secs = plan.joined.durations[k];
        // A lone segment needs no join.
        if r.len() == 1 {
            done += secs;
            on_progress((done / total).clamp(0.0, 1.0));
            inputs.push(segs[r.start].clone());
            continue;
        }
        let out = work_dir.join(format!("chunk_{k:02}.mp4"));
        let members = &segs[r.clone()];
        let (durs, trans) = (&durations[r.clone()], &transitions[r.clone()]);
        let local_overlaps = &overlaps[r.start..r.end - 1];
        let cached = caps.chunk_cache.as_deref().and_then(|c| {
            let ids: Option<Vec<String>> =
                members.iter().map(|p| segment_identity(caps, p)).collect();
            let key = chunk_key(c, &ids?, durs, trans, local_overlaps, keep_ambience, recipe);
            Some((c, key))
        });
        let hit = cached
            .as_ref()
            .is_some_and(|(c, key)| c.fetch(key, &out).is_some());
        if !hit {
            let args = chunk_join_args(
                members,
                durs,
                trans,
                local_overlaps,
                keep_ambience,
                recipe,
                &out,
            );
            let chunk_progress = |f: f64| on_progress(((done + f * secs) / total).clamp(0.0, 1.0));
            caps.run_progress(&args, &out, secs, &chunk_progress)
                .map_err(|e| match e {
                    crate::MediaError::Cancelled => e,
                    e => crate::MediaError::Ffmpeg(format!("joining chunk {k}: {e}")),
                })?;
            if let Some((c, key)) = &cached {
                c.store(key, &out, secs);
            }
        }
        done += secs;
        on_progress((done / total).clamp(0.0, 1.0));
        inputs.push(out);
    }
    Ok(JoinedInputs {
        inputs,
        ..plan.joined
    })
}

/// The chunk layout of [`prejoin_chunks`] (pure): the chunk ranges when the
/// film needs them, and the final graph's durations, transitions and
/// overlaps (the inputs are filled in by the caller).
struct ChunkPlan {
    ranges: Option<Vec<Range<usize>>>,
    joined: JoinedInputs,
}

fn plan_chunks(
    durations: &[f64],
    transitions: &[Option<Transition>],
    overlaps: &[f64],
    ranges: &[Range<usize>],
) -> ChunkPlan {
    if ranges.len() <= 1 {
        return ChunkPlan {
            ranges: None,
            joined: JoinedInputs {
                inputs: vec![],
                durations: durations.to_vec(),
                transitions: transitions.to_vec(),
                overlaps: overlaps.to_vec(),
            },
        };
    }
    let chunk_secs = |r: &Range<usize>| {
        durations[r.clone()].iter().sum::<f64>() - overlaps[r.start..r.end - 1].iter().sum::<f64>()
    };
    let joined = JoinedInputs {
        inputs: vec![],
        durations: ranges.iter().map(chunk_secs).collect(),
        // The cut into each chunk is the cut into its first segment.
        transitions: ranges.iter().map(|r| transitions[r.start]).collect(),
        overlaps: ranges
            .iter()
            .skip(1)
            .map(|r| overlaps[r.start - 1])
            .collect(),
    };
    ChunkPlan {
        ranges: Some(ranges.to_vec()),
        joined,
    }
}

/// A chapter heading as shown: the day line (`Day 16 · Sun, Oct 18`) on
/// the day's first heading, then only the place for later moves that day,
/// so a new place doesn't look like a new day. `last_day` tracks the day
/// line shown last.
fn day_once(
    last_day: &mut Option<String>,
    (top, bottom): (String, Option<String>),
) -> (String, Option<String>) {
    let same_day = last_day.as_deref() == Some(top.as_str());
    *last_day = Some(top.clone());
    match bottom {
        Some(place) if same_day => (place, None),
        bottom => (top, bottom),
    }
}

/// Per-cut overlap for a transition: slow fades get room to breathe,
/// everything else takes the 0.5 s standard.
pub fn transition_overlap(t: &Transition) -> f64 {
    match t {
        Transition::FadeBlack => 0.8,
        Transition::CircleOpen | Transition::CircleClose => 0.7,
        _ => 0.5,
    }
}

/// What a segment shows, for kind-change transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegKind {
    Title,
    Map,
    Still,
    Clip,
}

/// Assign one transition per cut from story position.
///
/// * `chapter_break[i]` — cut into segment `i` starts a new chapter.
/// * `kinds` — segment kinds; a kind change dissolves (shorter than `n`
///   is fine: missing entries never count as a change).
/// * First cut opens with `circleopen`, final cut closes with `circleclose`.
/// * Chapter breaks cycle `fadeblack, smoothleft, smoothleft`, so slow
///   fades stay rare (max ~1 in 3) however many micro-chapters exist.
/// * The blur-dissolve signature is capped at 3.
/// * Everything else rotates `fade/dissolve/smoothleft/smoothright/
///   circleopen`, never repeating the same xfade filter twice in a row
///   (special picks that would repeat fall back to the rotation).
pub fn assign_transitions(
    n_segments: usize,
    chapter_break: &[bool],
    kinds: &[SegKind],
) -> Vec<Option<Transition>> {
    let whole = Range {
        start: 0,
        end: n_segments,
    };
    assign_transitions_by_chunk(n_segments, chapter_break, kinds, &[whole])
}

/// [`assign_transitions`] for a film joined in chunks (`ranges`, from
/// [`assembly_chunks`]): the rules start over at each chunk, with the cut
/// into the chunk as their first. Every count they keep (the chapter-break
/// cycle, the rotation, the signature's every-fourth cut, no filter twice
/// in a row) then depends on the chunk's own segments alone, so adding or
/// removing a shot changes the transitions of its own chunk and no other.
/// A chunk gets one signature blur-dissolve (a film of one run: three);
/// the only cut not looked at across chunks is the pair around a chunk's
/// start, which may share a filter.
fn assign_transitions_by_chunk(
    n_segments: usize,
    chapter_break: &[bool],
    kinds: &[SegKind],
    ranges: &[Range<usize>],
) -> Vec<Option<Transition>> {
    const ROT: [Transition; 5] = [
        Transition::Fade,
        Transition::Dissolve,
        Transition::SmoothLeft,
        Transition::SmoothRight,
        Transition::CircleOpen,
    ];
    let mut out: Vec<Option<Transition>> = vec![None; n_segments];
    if n_segments <= 1 {
        return out;
    }
    let sig_cap = if ranges.len() > 1 { 1 } else { 3 };
    // Next rotation pick whose filter differs from `prev`.
    let next_rot = |prev: Option<&str>, rot: &mut usize| -> Transition {
        for _ in 0..ROT.len() {
            let t = ROT[*rot % ROT.len()];
            *rot += 1;
            if Some(t.xfade_name()) != prev {
                return t;
            }
        }
        Transition::Fade
    };
    for r in ranges {
        let mut prev: Option<&'static str> = None;
        let mut sig_used = 0u32;
        let mut break_n = 0u32;
        let mut rot = 0usize;
        for i in r.clone().filter(|i| (1..n_segments).contains(i)) {
            // Special pick for this story position; `None` means "rotate".
            let special = if i == 1 {
                Some(Transition::CircleOpen)
            } else if i == n_segments - 1 && n_segments > 2 {
                Some(Transition::CircleClose)
            } else if chapter_break.get(i).copied().unwrap_or(false) {
                break_n += 1;
                Some(if break_n % 3 == 1 {
                    Transition::FadeBlack
                } else {
                    Transition::SmoothLeft
                })
            } else if kinds.get(i) != kinds.get(i - 1) {
                Some(Transition::Dissolve)
            } else if sig_used < sig_cap && (i - r.start) % 4 == 2 {
                Some(Transition::BlurDissolve)
            } else {
                None
            };
            // A special pick that would repeat the previous filter rotates too.
            let t = match special {
                Some(c) if Some(c.xfade_name()) != prev => c,
                _ => next_rot(prev, &mut rot),
            };
            if t == Transition::BlurDissolve {
                sig_used += 1;
            }
            out[i] = Some(t);
            prev = Some(t.xfade_name());
        }
    }
    out
}

/// The places card's list: two sights in one place (`Flower Dome, Gardens
/// by the Bay` and `Cloud Forest, Gardens by the Bay`) are named once, by
/// that place, where the first of them stood. A sight alone keeps its
/// whole name. A country is no such place: `Tokyo, Japan` and
/// `Kyoto, Japan` stay two cities.
fn credit_places(places: &[String]) -> Vec<String> {
    fn parent(p: &str) -> Option<&str> {
        if triptape_core::geo::without_country(p) != p {
            return None;
        }
        p.rsplit_once(", ")
            .map(|(_, par)| par)
            .filter(|par| !par.is_empty())
    }
    let mut sights: HashMap<&str, usize> = HashMap::new();
    for par in places.iter().filter_map(|p| parent(p.as_str())) {
        *sights.entry(par).or_default() += 1;
    }
    let mut out: Vec<String> = vec![];
    for p in places {
        let name = match parent(p) {
            Some(par) if sights[par] > 1 => par,
            _ => p.as_str(),
        };
        if !out.iter().any(|q| q == name) {
            out.push(name.to_string());
        }
    }
    out
}

/// Resolved source files for one plan entry (export layer fills this).
#[derive(Debug, Clone)]
pub struct ResolvedEntry {
    pub entry: PlanEntry,
    pub still_path: PathBuf,
    pub motion_path: Option<PathBuf>,
    pub caption: Option<String>,
    /// Capture time (unix secs) for date lower-thirds; `None` skips the date.
    pub taken_at: Option<i64>,
    /// UTC offset at capture (seconds) for local-time display.
    pub tz_offset_secs: Option<i32>,
    /// Display width / height (EXIF rotation applied) when known; lets
    /// panoramas be kept out of collages before anything is decoded.
    pub aspect: Option<f64>,
    /// Where it was taken, when known: places the travel map home before
    /// the first shot back home.
    pub gps: Option<(f64, f64)>,
    /// The owner's framing rectangle: the film uses only this part of the
    /// source (`aspect` is already the framed aspect).
    pub frame: Option<Frame>,
    /// The owner set it to play as a still: no Live Photo motion, even in
    /// a collage cell.
    pub still: bool,
}

impl ResolvedEntry {
    /// When it was taken: unix seconds and UTC offset; `None` when undated.
    fn taken(&self) -> Option<(i64, Option<i32>)> {
        self.taken_at.map(|t| (t, self.tz_offset_secs))
    }
}

/// How an itinerary's travel is shown in the film.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MapStyle {
    /// Parchment maps: a hop plays where the trip reaches each stop, and
    /// the whole route closes the film (see [`crate::parchment`]).
    #[default]
    Parchment,
    /// One travelling recap of the whole route at the end, on the dark map
    /// (see [`crate::routetravel`]).
    Night,
}

impl MapStyle {
    pub fn from_flag(s: &str) -> Result<Self, crate::MediaError> {
        match s.to_ascii_lowercase().as_str() {
            "parchment" => Ok(Self::Parchment),
            "night" => Ok(Self::Night),
            _ => Err(crate::MediaError::InvalidArg(format!(
                "unknown map style {s:?} (want parchment|night)"
            ))),
        }
    }
}

#[derive(Debug, Clone)]
pub struct VideoOptions {
    pub trip_name: String,
    pub short_mode: bool,
    pub still_secs: f64,
    pub overlap_secs: f64,
    pub title_secs: f64,
    pub map_secs: f64,
    /// Show the route map card right after the title (default false: the
    /// film opens on its first shot; the closing recap always plays).
    pub opening_map: bool,
    /// Music playlist in order; crossfaded, cycled only if shorter than the
    /// film. Empty (or all unusable) falls back to the bundled theme.
    pub music_paths: Vec<PathBuf>,
    /// Keep the original film audio at 0.12 under a ducked music bed.
    /// Default (false) mutes it: the music is the primary audio.
    pub keep_ambience: bool,
    /// Live Photos in a collage play their motion in their cell (default
    /// true); false keeps every collage cell a still.
    pub live_collages: bool,
    /// Typst binary for title/lower-third text PNGs.
    pub typst_bin: String,
    /// Output frame size (default 4K).
    pub resolution: Resolution,
    /// Segments rendered at once; `None` picks [`default_jobs`]. Short mode
    /// always renders sequentially (its budget couples the segments).
    pub jobs: Option<usize>,
    /// Chapter stops the closing map travels between (indices into the
    /// plan's `map_polyline`). Fewer than two keep the static closing card.
    pub route_stops: Vec<triptape_core::route_stops::RouteStop>,
    /// The owner's itinerary as a journey: when set, the closing map travels
    /// it (flights, ships, camera) instead of the photo-derived stops, and
    /// ship names stay off the places card.
    pub journey: Option<triptape_core::journey::Journey>,
    /// How the journey is shown (default: parchment hops through the film).
    pub map_style: MapStyle,
    /// The owner's own clip for each hop, in journey order, played in
    /// place of the drawn ones (one per [`triptape_core::journey::Hop`]).
    /// The film then has no closing route map.
    pub hop_clips: Vec<PathBuf>,
    /// A draft for reviewing an edit, not the keepsake: the same segments
    /// (at `resolution`, so the segment cache serves both), cuts, captions
    /// and music, joined at [`ffmpeg::DRAFT_RESOLUTION`] with the fastest
    /// encoder this machine has ([`ffmpeg::draft_encoder`]).
    pub draft: bool,
    /// The owner's own date line for the title card (`October 3rd – 30th`)
    /// in place of the one worked out from the photos.
    pub title_dates: Option<String>,
}

impl Default for VideoOptions {
    fn default() -> Self {
        Self {
            trip_name: "TripTape Film".into(),
            short_mode: false,
            still_secs: 4.0,
            overlap_secs: 0.6,
            title_secs: 2.5,
            map_secs: 3.0,
            opening_map: false,
            music_paths: Vec::new(),
            keep_ambience: false,
            live_collages: true,
            typst_bin: "typst".into(),
            resolution: Resolution::default(),
            jobs: None,
            route_stops: vec![],
            journey: None,
            map_style: MapStyle::default(),
            hop_clips: vec![],
            draft: false,
            title_dates: None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct EdlSegment {
    pub index: usize,
    pub asset_id: String,
    pub kind: String,
    pub segment_secs: f64,
    /// Every asset shown in a collage segment (`asset_id` is its lead).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub collage_asset_ids: Vec<String>,
    pub transition_in: Option<Transition>,
    pub offset_secs: Option<f64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Edl {
    pub version: u32,
    /// The film is a draft (see [`VideoOptions::draft`]), not the keepsake.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub draft: bool,
    pub trip_name: String,
    pub overlap_secs: f64,
    /// Per-cut overlap actually used (len = segments - 1).
    pub cut_overlaps_secs: Vec<f64>,
    pub segments: Vec<EdlSegment>,
    pub total_secs: f64,
    pub captions_sidecar: Option<String>,
    pub music: crate::music::EdlMusic,
    pub fallbacks: Vec<String>,
}

#[derive(Debug)]
pub struct VideoOutput {
    pub path: PathBuf,
    pub edl_path: PathBuf,
    pub srt_path: Option<PathBuf>,
    pub duration_secs: f64,
    pub fallbacks: Vec<String>,
}

/// Hard ceiling for a short film (PLAN.md: "short" is <=30 s).
const SHORT_FILM_SECS: f64 = 30.0;
/// Closing map recap runs this much longer than the opening map.
const CLOSING_MAP_EXTRA_SECS: f64 = 1.5;

/// Seconds the closing map runs: the animated recap's budgeted length when
/// there are stops to travel between, else the static card's hold.
fn closing_map_secs(route_stops: usize, map_secs: f64) -> f64 {
    if route_stops >= 2 {
        triptape_core::route_stops::closing_map_secs(route_stops)
    } else {
        map_secs + CLOSING_MAP_EXTRA_SECS
    }
}

/// The owner's hop clips play in the film: full mode, parchment style, and
/// one clip per hop of the journey.
fn hop_clips_adopted(opts: &VideoOptions) -> bool {
    opts.journey.as_ref().is_some_and(|j| {
        opts.map_style == MapStyle::Parchment
            && !opts.short_mode
            && !opts.hop_clips.is_empty()
            && opts.hop_clips.len() == j.hops().len()
    })
}

/// FFmpeg args writing the first frame of `clip` to `out` (a PNG).
fn first_frame_args(clip: &Path, out: &Path) -> Vec<String> {
    vec![
        "-y".into(),
        "-i".into(),
        s(clip),
        "-frames:v".into(),
        "1".into(),
        s(out),
    ]
}

/// The title card's date line: the owner's own wins (a blank one means no
/// date line at all), else the range worked out from the photos.
fn title_date_line(owner: Option<&str>, photos: Option<&str>) -> Option<String> {
    owner
        .map(str::trim)
        .or(photos)
        .filter(|d| !d.is_empty())
        .map(str::to_string)
}

/// Seconds the closing map segment will run for these options.
fn closing_map_len(opts: &VideoOptions) -> f64 {
    match &opts.journey {
        // The owner's hop clips close the film themselves when they are
        // adopted (one per hop, full mode only); otherwise a route map does.
        Some(_) if opts.map_style == MapStyle::Parchment => {
            if hop_clips_adopted(opts) {
                0.0
            } else {
                triptape_core::journey::HOP_OVERVIEW_SECS
            }
        }
        Some(j) => j.duration_secs(),
        None => closing_map_secs(
            triptape_core::route_stops::animated_stop_count(&opts.route_stops),
            opts.map_secs,
        ),
    }
}

/// Seconds the closing cards add, reserved up front in short mode.
fn closing_secs(has_route: bool, closing_map: f64, places: usize) -> f64 {
    let map = if has_route { closing_map } else { 0.0 };
    map + if places > 0 {
        places_card_secs(places)
    } else {
        0.0
    }
}

/// Choose at most `slots` entries for a short film so every chapter is seen,
/// instead of spending the whole budget on the first day. Slots are dealt to
/// chapters in turn, each chapter's picks are spaced evenly through it, and
/// the result keeps the plan's order.
fn spread_across_chapters<'a>(
    entries: &[&'a ResolvedEntry],
    slots: usize,
    chapter_of: impl Fn(&str) -> usize,
) -> Vec<&'a ResolvedEntry> {
    if entries.len() <= slots {
        return entries.to_vec();
    }
    let mut chapters: Vec<(usize, Vec<usize>)> = vec![];
    for (i, e) in entries.iter().enumerate() {
        let ch = chapter_of(&e.entry.asset_id);
        match chapters.iter_mut().find(|(c, _)| *c == ch) {
            Some((_, members)) => members.push(i),
            None => chapters.push((ch, vec![i])),
        }
    }
    let mut counts = vec![0usize; chapters.len()];
    let mut left = slots;
    while left > 0 {
        for (count, (_, members)) in counts.iter_mut().zip(&chapters) {
            if left > 0 && *count < members.len() {
                *count += 1;
                left -= 1;
            }
        }
    }
    let mut picked: Vec<usize> = chapters
        .iter()
        .zip(&counts)
        .flat_map(|((_, members), &k)| {
            (0..k).map(move |i| members[(2 * i + 1) * members.len() / (2 * k)])
        })
        .collect();
    picked.sort_unstable();
    picked.into_iter().map(|i| entries[i]).collect()
}

/// The most one segment may take of the remaining budget, so a long clip early
/// in a short film can't use up the time later chapters need.
fn fair_share(budget: f64, segments_left: usize) -> f64 {
    budget / segments_left.max(1) as f64
}

/// Sidecar text for a collage: each member's caption on its own line,
/// identical lines once.
fn collage_caption(members: &[&ResolvedEntry]) -> Option<String> {
    let mut lines: Vec<&str> = vec![];
    for c in members
        .iter()
        .filter_map(|m| m.caption.as_deref())
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        if !lines.contains(&c) {
            lines.push(c);
        }
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// Synthetic segment ids for fixed cards in the hero's segment list.
fn is_card(id: &str) -> bool {
    matches!(id, "title" | "map" | "closing" | "route" | "credits") || id.starts_with(HOP_ID)
}

/// Segment id prefix of a travel-map hop (`hop-01`...).
const HOP_ID: &str = "hop-";

/// Segment entry for a fixed card (not backed by a project asset).
fn card_entry(id: &str, path: &Path) -> ResolvedEntry {
    ResolvedEntry {
        entry: PlanEntry {
            asset_id: id.into(),
            window: 0,
            content_hash: None,
            size_bytes: 0,
            mtime: 0,
            rendition: Rendition::Still,
        },
        still_path: path.to_path_buf(),
        motion_path: None,
        caption: None,
        taken_at: None,
        tz_offset_secs: None,
        aspect: None,
        frame: None,
        still: false,
        gps: None,
    }
}

/// Render a full-frame Typst card, falling back to the styled solid when
/// Typst is unavailable or fails. Returns whether the text card rendered.
fn render_card_png(
    opts: &VideoOptions,
    typst_ok: bool,
    doc: &str,
    png: &Path,
    label: &str,
    fallbacks: &mut Vec<String>,
) -> Result<bool, crate::MediaError> {
    if typst_ok {
        match crate::titlecard::render_text_png(&opts.typst_bin, doc, png, opts.resolution.ppi()) {
            Ok(()) => return Ok(true),
            Err(e) => fallbacks.push(format!("{label} text failed ({e}); solid card")),
        }
    }
    let (w, h) = opts.resolution.dims();
    crate::titlecard::save_title_card(&opts.trip_name, w, h, png)?;
    Ok(false)
}

/// Render the hero film. `work_dir` holds intermediates. Outputs land at
/// `out_mp4` and beside it; `export_all` points that at a staging folder.
pub fn render_hero_video(
    caps: &Capabilities,
    plan: &Plan,
    resolved: &[ResolvedEntry],
    map_card: Option<&Path>,
    work_dir: &Path,
    out_mp4: &Path,
    opts: &VideoOptions,
) -> Result<VideoOutput, crate::MediaError> {
    if !caps.libx264 {
        return Err(crate::MediaError::Ffmpeg(
            "libx264 missing; no hero encode".into(),
        ));
    }
    if !caps.xfade {
        return Err(crate::MediaError::Ffmpeg(
            "xfade missing; no hero assembly".into(),
        ));
    }
    std::fs::create_dir_all(work_dir).map_err(|e| crate::MediaError::Io(e.to_string()))?;
    let mut fallbacks: Vec<String> = vec![];
    if !caps.full_tonemap() && caps.tonemap {
        fallbacks.push("zscale absent: HDR tone-map uses tonemap alone".into());
    }
    if !caps.perspective {
        fallbacks.push("perspective absent: stills, clips and maps hold without motion".into());
    }
    if !caps.can_burn_captions() {
        fallbacks.push("libass absent: captions as sidecar SRT, not burned in".into());
    }
    let recipe = if opts.draft {
        let encoder = ffmpeg::draft_encoder(caps);
        let (w, h) = ffmpeg::DRAFT_RESOLUTION.dims();
        fallbacks.push(format!(
            "draft film ({w}x{h}, {}): for reviewing edits, not the keepsake",
            encoder.name()
        ));
        Recipe::Draft(encoder)
    } else {
        Recipe::Keepsake
    };
    let typst_ok = crate::titlecard::typst_available(&opts.typst_bin);
    if !typst_ok {
        fallbacks.push(format!(
            "typst unavailable ({}): solid title card and no overlay text",
            opts.typst_bin
        ));
    }

    // Chapter breaks: cut into segment i when its chapter differs.
    let chapter_index = plan::chapter_index(plan);
    let chapter_of = |id: &str| chapter_index.get(id).copied().unwrap_or(usize::MAX);
    // Place sections: a chapter's film shots from one place change
    // (`Chapter::film_place_changes`) to the next, in film order. A collage
    // never spans two, and each names its own place.
    let section_of = place_sections(plan, chapter_of);
    let section = |id: &str| section_of.get(id).copied().unwrap_or(0);

    // 1. Normalize segments (skip missing sources, never fail the film).
    let still_secs = if opts.short_mode {
        3.0
    } else {
        opts.still_secs
    };
    let mut segs: Vec<(ResolvedEntry, PathBuf, f64, String)> = vec![];
    // Title card first: Typst full-frame card (trip name + date range)
    // when available, else the styled solid. With the owner's hop clips,
    // the title sits on the opening hop's first frame: the map before the
    // journey is drawn, which the first hop then sets off across.
    let title_png = work_dir.join("title.png");
    let date_range = crate::titlecard::format_date_range(
        &resolved
            .iter()
            .filter_map(ResolvedEntry::taken)
            .collect::<Vec<_>>(),
    );
    let title_dates = title_date_line(opts.title_dates.as_deref(), date_range.as_deref());
    const TITLE_MAP: &str = "title-map.png";
    let title_map_png = work_dir.join(TITLE_MAP);
    let mut title_map = None;
    if let Some(clip) = opts
        .hop_clips
        .first()
        .filter(|_| typst_ok && hop_clips_adopted(opts))
    {
        match caps.run(&first_frame_args(clip, &title_map_png), &title_map_png) {
            Ok(()) => title_map = Some(TITLE_MAP),
            Err(crate::MediaError::Cancelled) => return Err(crate::MediaError::Cancelled),
            Err(e) => fallbacks.push(format!("title map frame failed ({e}); plain title card")),
        }
    }
    let title_is_text = render_card_png(
        opts,
        typst_ok,
        &crate::titlecard::title_card_doc(&opts.trip_name, title_dates.as_deref(), title_map),
        &title_png,
        "title",
        &mut fallbacks,
    )?;
    if title_is_text {
        fallbacks.push(format!(
            "title card shows trip name{}{}",
            title_map.map_or("", |_| " over the opening map"),
            title_dates
                .as_deref()
                .map_or(String::new(), |d| format!(" + {d}"))
        ));
    }
    let title_mp4 = work_dir.join("seg_title.mp4");
    render_hold_segment(
        caps,
        &title_png,
        opts.title_secs,
        0,
        opts.resolution,
        None,
        &title_mp4,
    )?;
    segs.push((
        card_entry("title", &title_png),
        title_mp4,
        opts.title_secs,
        "title".into(),
    ));

    // Map push-in when asked for, GPS exists and a card was rendered. Off
    // by default: the title cuts straight to the first shot, and the route
    // is saved for the closing recap.
    if let Some(card) = map_card.filter(|_| opts.opening_map) {
        if !plan.map_polyline.is_empty() {
            let map_mp4 = work_dir.join("seg_map.mp4");
            match crate::map::render_map_push_in(
                caps,
                card,
                &map_mp4,
                opts.map_secs,
                0,
                opts.resolution,
            ) {
                Ok(()) => segs.push((
                    card_entry("map", card),
                    map_mp4,
                    opts.map_secs,
                    "map".into(),
                )),
                Err(e) => fallbacks.push(format!("map segment skipped: {e}")),
            }
        }
    }

    let mut places: Vec<String> = vec![];
    let ships = opts.journey.as_ref().map(|j| j.ships()).unwrap_or_default();
    // A ship is where we slept, not a place we went.
    let is_ship = |p: &str| triptape_core::itinerary::is_ship_place(&ships, p);
    // Home is where the trip starts and ends, not a place we went: its own
    // name, and a place every located shot of which was taken near home
    // (the drive home from the airport), stay off the card.
    let mut near_home: HashMap<&str, bool> = HashMap::new();
    if let Some(j) = opts.journey.as_ref() {
        for r in resolved {
            let id = r.entry.asset_id.as_str();
            let Some(ch) = plan.chapters.get(chapter_of(id)) else {
                continue;
            };
            if let (Some(p), Some(g)) = (section_place(ch, section(id)), r.gps) {
                let home = j.near_home(g);
                near_home
                    .entry(p)
                    .and_modify(|h| *h &= home)
                    .or_insert(home);
            }
        }
        near_home.extend(j.home_labels().map(|l| (l, true)));
    }
    let is_home = |p: &str| near_home.get(p) == Some(&true);
    for p in plan.chapters.iter().flat_map(|c| c.film_places()) {
        if !is_ship(p) && !is_home(p) && !places.iter().any(|q| q == p) {
            places.push(p.to_string());
        }
    }
    let places = credit_places(&places);

    // Hero entries (short mode: cap the whole film at 30 s).
    // Fixed segments (title, optional map, closing cards) come out of the
    // budget first; each entry is checked and clip-capped before it is
    // encoded.
    let mut budget = if opts.short_mode {
        let has_route = map_card.is_some() && !plan.map_polyline.is_empty();
        SHORT_FILM_SECS
            - segs.iter().map(|s| s.2).sum::<f64>()
            - closing_secs(has_route, closing_map_len(opts), places.len())
    } else {
        f64::INFINITY
    };
    // Group same-chapter stills into collages of up to four, so a day
    // shows several photos at once instead of a single pick. Clips, Live
    // Photo motion, panoramas and captioned stills stay solo.
    let mut collage_members: HashMap<String, Vec<String>> = HashMap::new();
    let selected: Vec<&ResolvedEntry> = if opts.short_mode {
        let slots = (budget / still_secs).floor().max(1.0) as usize;
        let on_disk: Vec<&ResolvedEntry> = resolved
            .iter()
            .filter(|r| {
                let ok =
                    r.still_path.exists() || r.motion_path.as_ref().is_some_and(|p| p.exists());
                if !ok {
                    fallbacks.push(format!("{} missing on disk; skipped", r.entry.asset_id));
                }
                ok
            })
            .collect();
        let chosen = spread_across_chapters(&on_disk, slots, chapter_of);
        if chosen.len() < on_disk.len() {
            fallbacks.push(format!(
                "short variant: {} of {} picks spread across the trip to fit ~30 s",
                chosen.len(),
                on_disk.len()
            ));
        }
        chosen
    } else {
        resolved.iter().collect()
    };
    // The short variant spreads single picks across the trip instead.
    let groups: Vec<Vec<&ResolvedEntry>> = if opts.short_mode {
        selected.into_iter().map(|r| vec![r]).collect()
    } else {
        // Shots either side of a travel-map hop stay in separate
        // segments, so the hop can play between them.
        let hops = opts
            .journey
            .as_ref()
            .filter(|_| opts.map_style == MapStyle::Parchment)
            .map_or_else(Vec::new, |j| {
                let shots: Vec<_> = selected.iter().map(|r| r.taken()).collect();
                let gps: Vec<_> = selected.iter().map(|r| r.gps).collect();
                triptape_core::journey::with_moves(j.hops(), &shots, &gps)
            });
        let stage_of =
            |r: &ResolvedEntry| triptape_core::journey::hops_reached(&hops, r.taken(), r.gps);
        hero_groups(
            &selected,
            |r| {
                let id = r.entry.asset_id.as_str();
                (chapter_of(id), section(id), stage_of(r))
            },
            &plan.collage,
        )
    };
    // Segment renders are independent unless a short-mode budget couples
    // them (each one's cap and freeze-pad depend on what earlier ones left),
    // so full mode fans out over `jobs` workers and short mode stays
    // sequential. Both paths go through `render_group`, and results are
    // folded back in group order, so the segment list, timeline and
    // fallbacks do not depend on the worker count.
    let variants = glide_variants(&groups);
    let progress = caps.progress.clone();
    let jobs = if opts.short_mode {
        1
    } else {
        opts.jobs
            .unwrap_or_else(|| default_jobs(opts.resolution))
            .max(1)
    };
    let job_caps = Capabilities {
        threads: thread_share(jobs),
        ..caps.clone()
    };
    let mut absorb = |segs: &mut Vec<(ResolvedEntry, PathBuf, f64, String)>,
                      fallbacks: &mut Vec<String>,
                      out: GroupOutcome| {
        fallbacks.extend(out.fallbacks);
        if let Some(seg) = out.seg {
            if !out.collage_members.is_empty() {
                collage_members.insert(seg.0.entry.asset_id.clone(), out.collage_members);
            }
            segs.push(seg);
        }
    };
    if opts.short_mode {
        progress.counted("video", "segment", 0, groups.len());
        for (idx, group) in groups.iter().enumerate() {
            progress.check_cancelled()?;
            let (present, missing) = split_present(group);
            fallbacks.extend(missing);
            let Some(&r) = present.first() else { continue };
            // Below the 0.5 s clip floor nothing more fits.
            if budget < 0.5 {
                fallbacks.push(format!(
                    "short variant: {} and later dropped to fit ~30 s",
                    r.entry.asset_id
                ));
                break;
            }
            let limits = SegmentLimits {
                cap: fair_share(budget, groups.len() - idx),
                budget,
            };
            let out = render_group(
                &job_caps,
                &present,
                idx,
                variants[idx],
                still_secs,
                limits,
                opts,
                work_dir,
            )?;
            if let Some(seg) = &out.seg {
                budget -= seg.2;
            }
            absorb(&mut segs, &mut fallbacks, out);
            progress.counted("video", "segment", idx + 1, groups.len());
        }
    } else {
        let plans: Vec<(Vec<&ResolvedEntry>, Vec<String>)> =
            groups.iter().map(|g| split_present(g)).collect();
        let work: Vec<usize> = (0..plans.len())
            .filter(|&i| !plans[i].0.is_empty())
            .collect();
        progress.counted("video", "segment", 0, work.len());
        let mut results: Vec<Option<Result<GroupOutcome, crate::MediaError>>> =
            (0..plans.len()).map(|_| None).collect();
        triptape_core::parallel::for_each_parallel(
            &work,
            jobs,
            |&idx| {
                progress.check_cancelled()?;
                render_group(
                    &job_caps,
                    &plans[idx].0,
                    idx,
                    variants[idx],
                    still_secs,
                    SegmentLimits::NONE,
                    opts,
                    work_dir,
                )
            },
            |wi, res, done, total| {
                results[work[wi]] = Some(res);
                progress.counted("video", "segment", done, total);
            },
        );
        progress.check_cancelled()?;
        for (idx, (_, missing)) in plans.into_iter().enumerate() {
            fallbacks.extend(missing);
            if let Some(res) = results[idx].take() {
                absorb(&mut segs, &mut fallbacks, res?);
            }
        }
    }
    if segs.len() <= 1 {
        return Err(crate::MediaError::Ffmpeg(
            "no usable hero segments (all sources missing)".into(),
        ));
    }

    // Travel maps: each hop of the itinerary plays where the trip reaches
    // it, and the whole route closes the film. A map that fails to render
    // is left out; the film never fails for one.
    let own_clips = !opts.hop_clips.is_empty();
    let parchment = opts
        .journey
        .as_ref()
        .filter(|_| (typst_ok || own_clips) && opts.map_style == MapStyle::Parchment);
    let mut route_closed = false;
    if let Some(journey) = parchment {
        let hops = if opts.short_mode {
            vec![]
        } else {
            journey.hops()
        };
        let first_shot = segs
            .iter()
            .position(|s| !is_card(&s.0.entry.asset_id))
            .unwrap_or(segs.len());
        let shots: Vec<Option<(i64, Option<i32>)>> =
            segs[first_shot..].iter().map(|s| s.0.taken()).collect();
        let gps: Vec<Option<(f64, f64)>> = segs[first_shot..].iter().map(|s| s.0.gps).collect();
        let cuts = triptape_core::journey::hop_cuts(&hops, &shots, &gps);
        let adopt = hop_clips_adopted(opts);
        if own_clips && !adopt && !hops.is_empty() {
            fallbacks.push(format!(
                "{} hop clips for {} hops: the maps are drawn instead",
                opts.hop_clips.len(),
                hops.len()
            ));
        }
        // The owner's clips tell the whole journey; no route map follows them.
        route_closed = adopt;
        // Without Typst only adopted clips can play; drawn hops would all fail.
        let todo = if adopt || typst_ok { hops.len() } else { 0 };
        progress.counted("video", "travel map", 0, todo);
        let mut made = vec![];
        for (i, (hop, cut)) in hops.iter().zip(&cuts).take(todo).enumerate() {
            progress.check_cancelled()?;
            let mp4 = work_dir.join(format!("seg_hop_{i:02}.mp4"));
            let made_hop = if adopt {
                crate::parchment::adopt_hop_clip(caps, &opts.hop_clips[i], opts.resolution, &mp4)
            } else {
                crate::parchment::render_hop(
                    caps,
                    journey,
                    hop,
                    opts.resolution.dims(),
                    &opts.typst_bin,
                    &mp4,
                )
            };
            match made_hop {
                Ok(rep) => {
                    fallbacks.extend(rep.notes);
                    made.push((first_shot + cut, i, mp4, rep.secs));
                }
                Err(crate::MediaError::Cancelled) => return Err(crate::MediaError::Cancelled),
                Err(e) => fallbacks.push(format!("travel map {} skipped: {e}", i + 1)),
            }
            progress.counted("video", "travel map", i + 1, todo);
        }
        // Back to front, so an insertion never shifts the ones before it.
        for (at, i, mp4, secs) in made.into_iter().rev() {
            let entry = card_entry(&format!("{HOP_ID}{:02}", i + 1), &mp4);
            segs.insert(at, (entry, mp4, secs, "map".into()));
        }
    }
    if let Some(journey) = parchment.filter(|_| !route_closed && typst_ok) {
        let png = work_dir.join("route.png");
        let close_mp4 = work_dir.join("seg_closing.mp4");
        let dur = triptape_core::journey::HOP_OVERVIEW_SECS;
        let drawn = crate::parchment::render_overview(
            journey,
            None,
            opts.resolution.dims(),
            &opts.typst_bin,
        )
        .and_then(|img| {
            img.save(&png)
                .map_err(|e| crate::MediaError::Io(format!("save route map: {e}")))
        })
        .and_then(|()| {
            crate::map::render_map_push_in(caps, &png, &close_mp4, dur, 1, opts.resolution)
        });
        match drawn {
            Ok(()) => {
                segs.push((card_entry("route", &png), close_mp4, dur, "still".into()));
                route_closed = true;
            }
            Err(crate::MediaError::Cancelled) => return Err(crate::MediaError::Cancelled),
            Err(e) => fallbacks.push(format!("parchment route map failed: {e}")),
        }
    }

    // Closing: pull back over the route card for a map recap, then a
    // places card, so the film ends deliberately instead of stopping on
    // the last photo. Both are fixed cards, like the title.
    if let Some(card) = map_card.filter(|_| !route_closed) {
        if !plan.map_polyline.is_empty() {
            let close_mp4 = work_dir.join("seg_closing.mp4");
            // Animated travel recap; the static pull-out card is the fallback.
            let animated = match &opts.journey {
                // A parchment film whose route map could not be drawn gets
                // the static card: it fits the time kept for the map, which
                // the longer travelling recap would overrun in a short film.
                Some(_) if opts.map_style == MapStyle::Parchment => None,
                Some(journey) => Some(crate::routetravel::render_journey_recap(
                    caps,
                    journey,
                    opts.resolution,
                    &opts.typst_bin,
                    work_dir,
                    &close_mp4,
                )),
                None => Some(crate::routeanim::render_route_recap(
                    caps,
                    &plan.map_polyline,
                    &opts.route_stops,
                    opts.resolution,
                    &opts.typst_bin,
                    work_dir,
                    &close_mp4,
                )),
            };
            let animated = match animated {
                None => None,
                Some(Ok(rep)) => {
                    fallbacks.extend(rep.notes);
                    Some(rep.secs)
                }
                Some(Err(crate::MediaError::Cancelled)) => {
                    return Err(crate::MediaError::Cancelled)
                }
                Some(Err(e)) => {
                    if opts.journey.is_some()
                        || triptape_core::route_stops::animated_stop_count(&opts.route_stops) >= 2
                    {
                        fallbacks.push(format!("route animation failed, static closing map: {e}"));
                    }
                    None
                }
            };
            let dur = animated.unwrap_or(opts.map_secs + CLOSING_MAP_EXTRA_SECS);
            let rendered = match animated {
                Some(_) => Ok(()),
                None => {
                    crate::map::render_map_push_in(caps, card, &close_mp4, dur, 1, opts.resolution)
                }
            };
            match rendered {
                Ok(()) => segs.push((card_entry("closing", card), close_mp4, dur, "still".into())),
                Err(e) => fallbacks.push(format!("closing map skipped: {e}")),
            }
        }
    }
    if !places.is_empty() {
        let png = work_dir.join("places.png");
        render_card_png(
            opts,
            typst_ok,
            &crate::titlecard::places_card_doc(&opts.trip_name, &places, date_range.as_deref()),
            &png,
            "places card",
            &mut fallbacks,
        )?;
        let places_mp4 = work_dir.join("seg_places.mp4");
        let hold = places_card_secs(places.len());
        // A card to read: it holds still, with no drift over the names.
        render_still_card(caps, &png, hold, opts.resolution, &places_mp4)?;
        segs.push((
            card_entry("credits", &png),
            places_mp4,
            hold,
            "still".into(),
        ));
    }

    // 2. Chapter starts: cut into segment i when its chapter differs.
    let mut chapter_break = vec![false; segs.len()];
    for i in 1..segs.len() {
        let (a, b) = (&segs[i - 1].0.entry.asset_id, &segs[i].0.entry.asset_id);
        let cut_card = |s: &str| is_card(s) && s != "title";
        chapter_break[i] =
            cut_card(a) || cut_card(b) || (a != "title" && chapter_of(a) != chapter_of(b));
    }

    // A segment opens its chapter when the previous content segment (title
    // and map cards skipped) is in another chapter or there is none. Kept
    // apart from `chapter_break`, which is false for the cut out of the
    // title card, so the first chapter still gets its title overlay.
    let mut chapter_start = vec![false; segs.len()];
    let mut prev_chapter: Option<usize> = None;
    for (i, seg) in segs.iter().enumerate() {
        let id = seg.0.entry.asset_id.as_str();
        if is_card(id) {
            continue;
        }
        let ch = chapter_of(id);
        chapter_start[i] = prev_chapter != Some(ch);
        prev_chapter = Some(ch);
    }

    // 3. Context overlays (Typst lower-thirds baked per segment so they
    // ride through the xfade chain): the day + place stated ONCE at each
    // chapter's first segment, and a label on the route card so it reads
    // as a map. Segments inside a chapter stay clean.
    if typst_ok {
        let chapter_of_entry: HashMap<&str, &triptape_core::types::Chapter> = plan
            .chapters
            .iter()
            .flat_map(|c| c.asset_ids.iter().map(move |id| (id.as_str(), c)))
            .collect();
        // Each overlay rewrites its own segment file, so they run alongside
        // one another; notes are folded back in segment order.
        let mut todo: Vec<(usize, String)> = vec![];
        // A chapter heading identical to the last one shown (the same day
        // and place, split by a few shots elsewhere) is not repeated.
        let mut last_heading: Option<(String, Option<String>)> = None;
        // The day line shown last, for `day_once`.
        let mut last_day: Option<String> = None;
        for (i, (r, _, _, _)) in segs.iter().enumerate() {
            let id = r.entry.asset_id.as_str();
            if id == "title" || id == "credits" {
                continue;
            }
            let lines = if id == "map" || id == "closing" {
                Some((
                    crate::titlecard::ROUTE_LABEL.to_string(),
                    date_range.clone(),
                ))
            } else {
                // Every segment names its chapter's place at this point in
                // the film (its place section), shown when it differs from
                // the last heading: a chapter start, a move to another place,
                // a chapter resuming where it had got to.
                let heading = chapter_of_entry.get(id).and_then(|ch| match section(id) {
                    // The chapter's own heading (old plans: its title).
                    0 => crate::titlecard::chapter_film_lines(ch),
                    k => crate::titlecard::chapter_lines_at(ch, section_place(ch, k)?),
                });
                heading
                    .filter(|lines| {
                        let repeat = last_heading.as_ref() == Some(lines);
                        last_heading = Some(lines.clone());
                        !repeat
                    })
                    .map(|lines| day_once(&mut last_day, lines))
            };
            let Some((top, bottom)) = lines else { continue };
            todo.push((
                i,
                crate::titlecard::lower_third_doc(&top, bottom.as_deref()),
            ));
        }
        progress.counted("video", "overlay", 0, todo.len());
        let mut notes: Vec<Option<String>> = vec![None; todo.len()];
        triptape_core::parallel::for_each_parallel(
            &todo,
            jobs,
            |(i, doc)| -> Result<Option<String>, crate::MediaError> {
                progress.check_cancelled()?;
                let (r, seg_mp4, dur, _) = &segs[*i];
                // A cached overlay replaces the segment outright and skips
                // the Typst render and the re-encode.
                let ov_key = job_caps.segment_cache.as_deref().and_then(|c| {
                    let base = c.key_of(seg_mp4)?;
                    Some((c, overlay_key(c, &base, doc, *dur, opts)))
                });
                if let Some((c, key)) = &ov_key {
                    if c.fetch_overlay(key, seg_mp4) {
                        return Ok(None);
                    }
                }
                let png = work_dir.join(format!("ov_{i:03}.png"));
                let result = crate::titlecard::render_text_png(
                    &opts.typst_bin,
                    doc,
                    &png,
                    opts.resolution.ppi(),
                )
                .and_then(|()| apply_lower_third(&job_caps, seg_mp4, &png, *dur, opts.resolution));
                if let (Ok(()), Some((c, key))) = (&result, &ov_key) {
                    c.store_overlay(key, seg_mp4, *dur);
                }
                Ok(result
                    .err()
                    .filter(|e| !matches!(e, crate::MediaError::Cancelled))
                    .map(|e| format!("overlay skipped for {}: {e}", r.entry.asset_id)))
            },
            |ti, res, done, total| {
                if let Ok(note) = res {
                    notes[ti] = note;
                }
                progress.counted("video", "overlay", done, total);
            },
        );
        progress.check_cancelled()?;
        fallbacks.extend(notes.into_iter().flatten());
    }

    // 4. Transitions + per-cut overlaps.
    let kinds: Vec<SegKind> = segs
        .iter()
        .map(|s| match s.3.as_str() {
            "title" => SegKind::Title,
            "map" => SegKind::Map,
            "clip" => SegKind::Clip,
            _ => SegKind::Still,
        })
        .collect();
    // A long film is joined in chunks cut at chapter starts, and its
    // transitions are assigned chunk by chunk: an edit then changes one
    // chunk, and the others come back from the chunk cache.
    let chunk_ranges = assembly_chunks(&chapter_break, ASSEMBLY_CHUNK);
    let transitions =
        assign_transitions_by_chunk(segs.len(), &chapter_break, &kinds, &chunk_ranges);
    let durations: Vec<f64> = segs.iter().map(|s| s.2).collect();
    let cut_overlaps: Vec<f64> = transitions
        .iter()
        .skip(1)
        .map(|t| transition_overlap(&t.unwrap_or(Transition::Fade)))
        .collect();
    let offsets = xfade_offsets_var(&durations, &cut_overlaps);

    // 5. One encode: every xfade (and, with --keep-ambience, acrossfade) in
    // a single filter graph feeds the caption burn and the music mix, and
    // x264 runs exactly once at final quality. The old pairwise chain
    // re-encoded the whole accumulated film at every cut (soft output); the
    // later two-pass layout (near-lossless intermediate, then final encode)
    // paid for a full extra decode+encode of the film. The music bed is the
    // PRIMARY audio (looped, trimmed, faded in/out); film audio is muted, or
    // kept at 0.12 ducked under the music with --keep-ambience. Loudnorm
    // lands on the mix, never on raw clip noise.
    let total = durations.iter().sum::<f64>() - cut_overlaps.iter().sum::<f64>();
    let norm = if caps.loudnorm {
        ffmpeg::LOUDNORM
    } else {
        fallbacks.push("loudnorm absent: flat resample, no loudness pass".into());
        "anull"
    };
    // Resolve the playlist: existing --music files win; with none usable
    // the bundled theme track is materialized into the work dir.
    let playlist = crate::music::load_playlist(
        &caps.ffmpeg_bin,
        &caps.ffprobe_bin,
        &opts.music_paths,
        work_dir,
        &mut fallbacks,
    )?;
    let bed_plan = crate::music::plan_bed(
        &playlist.iter().map(|t| t.duration_secs).collect::<Vec<_>>(),
        total,
    );
    // Captions: the sidecar SRT is always written; with libass the encode
    // also burns it into the picture. Segment 0 starts at 0; every later
    // segment starts at its xfade cut.
    let starts: Vec<f64> = std::iter::once(0.0)
        .chain(offsets.iter().copied())
        .collect();
    let srt_path = out_mp4.with_extension("srt");
    // The same caption on a run of shots (seven panoramas all "Some fun
    // multiple person panoramas") is shown once, on the first.
    let repeats = repeated_captions(
        &segs
            .iter()
            .map(|(r, ..)| {
                (
                    r.caption.as_deref(),
                    chapter_index.get(r.entry.asset_id.as_str()).copied(),
                    r.taken_at,
                )
            })
            .collect::<Vec<_>>(),
    );
    let srt_count = write_captions_srt(&segs, &starts, &repeats, &srt_path)?;
    let srt_opt = if srt_count > 0 {
        // A file name: the film may be moved after rendering.
        srt_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
    } else {
        let _ = std::fs::remove_file(&srt_path);
        None
    };
    let burn_srt =
        (srt_count > 0 && caps.can_burn_captions()).then(|| ffmpeg::subtitles_filter(&srt_path));
    progress.check_cancelled()?;
    let label = "assembling film (transitions, music, captions, encode)";
    progress.step("video", label, Some(0.0));
    // A long film is pre-joined in chunks first: one graph over every
    // segment buffers frames for all of them at once (a 226-segment 4K film
    // grew ffmpeg past 80 GB and macOS killed it). The chunks keep every
    // cut's transition and overlap, so the timeline is unchanged.
    let seg_paths: Vec<PathBuf> = segs.iter().map(|s| s.1.clone()).collect();
    let join_label = "joining film chunks";
    let joined = prejoin_chunks(
        caps,
        &seg_paths,
        &durations,
        &transitions,
        &cut_overlaps,
        opts.keep_ambience,
        work_dir,
        &chunk_ranges,
        recipe,
        &|f| progress.step("video", join_label, Some(f)),
    )?;
    progress.step("video", label, Some(0.0));
    let mut args: Vec<String> = vec!["-y".into()];
    for p in &joined.inputs {
        args.extend(["-i".into(), s(p)]);
    }
    for m in crate::music::bed_inputs(&bed_plan, &playlist) {
        args.extend(["-i".into(), s(m)]);
    }
    // Film inputs are 0..n; the bed placements follow.
    let bed_graph = crate::music::bed_filter(&bed_plan, &playlist, joined.inputs.len(), total);
    let (graph, video_map) = film_graph(
        &joined.transitions,
        &joined.overlaps,
        &xfade_offsets_var(&joined.durations, &joined.overlaps),
        opts.keep_ambience,
        burn_srt.as_deref(),
        &bed_graph,
        norm,
        recipe.scale(),
    );
    args.extend([
        "-filter_complex".into(),
        graph,
        "-map".into(),
        video_map,
        "-map".into(),
        "[aout]".into(),
    ]);
    // Streamable on a home network (Plex to a Roku): capped bitrate, HEVC
    // at 4K when available. A draft takes the fastest encode instead.
    args.extend(recipe.film_encode_args(caps, opts.resolution));
    // Nothing from a source file's tags reaches the shared film (Sec 14).
    args.extend(triptape_core::privacy::exif_strip_args());
    args.extend(["-r".into(), "30".into(), s(out_mp4)]);
    let on_progress = |f: f64| progress.step("video", label, Some(f));
    // Music bed is mandatory: a failed mix fails the film (the old
    // film-audio-only path is exactly the raw-clip-noise bug).
    caps.run_progress(&args, out_mp4, total, &on_progress)
        .map_err(|e| match e {
            crate::MediaError::Cancelled => e,
            e => crate::MediaError::Ffmpeg(format!("film assembly failed: {e}")),
        })?;

    // 6. Verify + EDL + sidecar SRT.
    let (out_w, out_h) = recipe.dims(opts.resolution);
    let probed = ffmpeg::verify_video(&caps.ffprobe_bin, out_mp4, out_w, out_h)?;
    let edl = Edl {
        version: 1,
        draft: opts.draft,
        trip_name: opts.trip_name.clone(),
        overlap_secs: opts.overlap_secs,
        cut_overlaps_secs: cut_overlaps
            .iter()
            .map(|o| (o * 1000.0).round() / 1000.0)
            .collect(),
        segments: segs
            .iter()
            .enumerate()
            .map(|(i, (r, _, d, kind))| EdlSegment {
                index: i,
                asset_id: r.entry.asset_id.clone(),
                kind: kind.clone(),
                segment_secs: (*d * 1000.0).round() / 1000.0,
                collage_asset_ids: collage_members
                    .get(&r.entry.asset_id)
                    .cloned()
                    .unwrap_or_default(),
                transition_in: transitions[i],
                offset_secs: if i == 0 {
                    None
                } else {
                    Some((offsets[i - 1] * 1000.0).round() / 1000.0)
                },
            })
            .collect(),
        total_secs: (total * 1000.0).round() / 1000.0,
        captions_sidecar: srt_opt.clone(),
        music: bed_plan.edl(&playlist),
        fallbacks: fallbacks.clone(),
    };
    let edl_path = out_mp4.with_extension("edl.json");
    std::fs::write(&edl_path, serde_json::to_string_pretty(&edl)?)
        .map_err(|e| crate::MediaError::Io(e.to_string()))?;

    Ok(VideoOutput {
        path: out_mp4.to_path_buf(),
        edl_path,
        srt_path: (srt_count > 0).then_some(srt_path),
        duration_secs: probed.duration_secs,
        fallbacks,
    })
}

/// Bake the lower-third PNG (1200x200 at 1080p, 2x that at 4K) onto `seg` in
/// place (`overlay=48:H-160` in 1080p units, visible
/// `between(t,0.3,segdur)`). `caps.run` writes a partial sibling and
/// renames, so a failure never leaves a torn segment.
fn apply_lower_third(
    caps: &Capabilities,
    seg: &Path,
    png: &Path,
    seg_dur: f64,
    res: Resolution,
) -> Result<(), crate::MediaError> {
    let graph = lower_third_graph(seg_dur, res);
    burn_png_overlay(caps, seg, png, &graph, LOWER_THIRD_CRF)
}

/// Quality of the re-encode that bakes a lower third into its segment.
const LOWER_THIRD_CRF: u32 = 16;

fn lower_third_graph(seg_dur: f64, res: Resolution) -> String {
    let k = res.scale();
    format!(
        "[0:v][1:v]overlay={}:H-{}:format=yuv420:enable='between(t,0.3,{seg_dur:.3})'[v]",
        48 * k,
        160 * k,
    )
}

/// Re-encode `video` in place with `png` composited by `filter_graph`
/// (inputs `[0:v]` and `[1:v]`, output `[v]`); audio is carried over.
/// A film segment is an intermediate the final encode caps, so it has no
/// bitrate cap of its own.
fn burn_png_overlay(
    caps: &Capabilities,
    video: &Path,
    png: &Path,
    filter_graph: &str,
    crf: u32,
) -> Result<(), crate::MediaError> {
    caps.run(&overlay_args(video, png, filter_graph, crf), video)
}

/// [`burn_png_overlay`] for a finished deliverable (a reel): the re-encode
/// keeps the streaming cap of `cap_kbps` ([`ffmpeg::bitrate_cap_args`]).
pub(crate) fn burn_png_overlay_capped(
    caps: &Capabilities,
    video: &Path,
    png: &Path,
    filter_graph: &str,
    crf: u32,
    cap_kbps: u32,
) -> Result<(), crate::MediaError> {
    caps.run(
        &capped_overlay_args(video, png, filter_graph, crf, cap_kbps),
        video,
    )
}

fn capped_overlay_args(
    video: &Path,
    png: &Path,
    filter_graph: &str,
    crf: u32,
    cap_kbps: u32,
) -> Vec<String> {
    let mut args = overlay_args(video, png, filter_graph, crf);
    // Before the output path, which ends the argument list.
    let out = args.pop();
    args.extend(ffmpeg::bitrate_cap_args(cap_kbps));
    args.extend(out);
    args
}

fn overlay_args(video: &Path, png: &Path, filter_graph: &str, crf: u32) -> Vec<String> {
    vec![
        "-y".into(),
        "-i".into(),
        s(video),
        "-loop".into(),
        "1".into(),
        "-i".into(),
        s(png),
        "-filter_complex".into(),
        filter_graph.into(),
        "-map".into(),
        "[v]".into(),
        "-map".into(),
        "0:a:0".into(),
    ]
    .into_iter()
    .chain(ffmpeg::x264_aac("veryfast", crf))
    .chain(["-shortest".into(), s(video)])
    .collect()
}

/// Cache key of the lower third baked onto a segment: the base segment's key
/// (its pixels), the overlay text, the burn recipe and the final length. The
/// text PNG's look also rests on the Typst binary and fonts, which the key
/// cannot see; bump `SEGMENT_CACHE_VERSION` if the card templates change.
fn overlay_key(
    cache: &SegmentCache,
    base_key: &str,
    doc: &str,
    seg_dur: f64,
    opts: &VideoOptions,
) -> String {
    let mut k = cache.key("overlay");
    k.part("base", base_key)
        .part("doc", doc)
        .num("dur", seg_dur)
        .part("ppi", &opts.resolution.ppi().to_string())
        .part("typst", &opts.typst_bin)
        .args(&overlay_args(
            Path::new(KEY_IN),
            Path::new(KEY_PNG),
            &lower_third_graph(seg_dur, opts.resolution),
            LOWER_THIRD_CRF,
        ));
    k.finish()
}

/// Which segments' captions repeat the caption last shown, so only the
/// first of a run is shown. Per segment: its caption, chapter and capture
/// time. A caption repeats when its text (trimmed, case-insensitive) is
/// the last shown one's and it is in the same chapter or taken within
/// [`plan::COLLAGE_WINDOW_SECS`] of it ([`plan::within_window`]).
/// Uncaptioned segments in between do not break the run; a different
/// caption does. A skipped caption only loses
/// its text on screen: the shot still counts as captioned for layout.
fn repeated_captions(segs: &[(Option<&str>, Option<usize>, Option<i64>)]) -> Vec<bool> {
    let mut last: Option<(String, Option<usize>, Option<i64>)> = None;
    segs.iter()
        .map(|&(caption, chapter, taken)| {
            let Some(text) = caption.map(str::trim).filter(|c| !c.is_empty()) else {
                return false;
            };
            let text = text.to_lowercase();
            let repeat = last.as_ref().is_some_and(|(t, ch, at)| {
                *t == text && ((ch.is_some() && *ch == chapter) || plan::within_window(*at, taken))
            });
            if !repeat {
                last = Some((text, chapter, taken));
            }
            repeat
        })
        .collect()
}

/// Start and end of a caption's cue in a segment starting at `start` and
/// lasting `d`: it appears [`plan::CAPTION_LEAD_IN_SECS`] in and stays for
/// `read_secs`, but never past the segment's end margin or the cut into
/// the next shot (`next`; a dip to black there would leave the text
/// floating over black). `None` when under [`CAPTION_CUE_MIN_SECS`] remain.
fn caption_cue(start: f64, d: f64, next: f64, read_secs: f64) -> Option<(f64, f64)> {
    let st = start + plan::CAPTION_LEAD_IN_SECS;
    let en = (st + read_secs)
        .min(start + d - CAPTION_CUE_END_MARGIN_SECS)
        .max(st + CAPTION_CUE_MIN_SECS)
        .min(next);
    (en - st >= CAPTION_CUE_MIN_SECS).then_some((st, en))
}

/// Shortest caption cue worth showing: anything briefer only flashes.
const CAPTION_CUE_MIN_SECS: f64 = 0.5;
/// A caption's cue ends this long before its segment does.
const CAPTION_CUE_END_MARGIN_SECS: f64 = 0.6;

/// Write the film's captions as SRT cues timed to each segment's `starts`,
/// leaving out the captions `repeats` marks ([`repeated_captions`]).
fn write_captions_srt(
    segs: &[(ResolvedEntry, PathBuf, f64, String)],
    starts: &[f64],
    repeats: &[bool],
    out: &Path,
) -> Result<usize, crate::MediaError> {
    let mut n = 0;
    let mut body = String::new();
    for (i, (r, _, d, _)) in segs.iter().enumerate() {
        if let Some(c) = &r.caption {
            if c.trim().is_empty() || repeats.get(i).copied().unwrap_or(false) {
                continue;
            }
            // As long as the shot was held to read it; multi-line
            // (collage) cues get at least 3.5 s per caption.
            let read_secs = caption_read_secs(Some(c)).max(3.5 * c.trim().lines().count() as f64);
            let next = starts.get(i + 1).copied().unwrap_or(f64::INFINITY);
            let Some((st, en)) = caption_cue(starts[i], *d, next, read_secs) else {
                // No time to read it before the cut: skip the cue.
                continue;
            };
            n += 1;
            body.push_str(&format!(
                "{n}\n{} --> {}\n{}\n\n",
                srt_ts(st),
                srt_ts(en),
                c.trim()
            ));
        }
    }
    if n == 0 {
        return Ok(0);
    }
    std::fs::write(out, body).map_err(|e| crate::MediaError::Io(e.to_string()))?;
    Ok(n)
}

fn srt_ts(t: f64) -> String {
    let ms = (t.max(0.0) * 1000.0).round() as u64;
    format!(
        "{:02}:{:02}:{:02},{:03}",
        ms / 3_600_000,
        (ms / 60_000) % 60,
        (ms / 1000) % 60,
        ms % 1000
    )
}

/// Seconds to freeze-pad a clip to: the minimum hold (or `hold`, when a
/// caption needs longer to read), limited by what is left of the
/// short-mode budget. `None` when the clip already reaches that.
fn clip_pad_target(probed: f64, budget: f64, hold: f64) -> Option<f64> {
    let target = MIN_SEGMENT_SECS.max(hold).min(budget);
    (probed < target).then_some(target)
}

/// Freeze-pad `seg` (video + audio) to `target` seconds on its last frame.
fn pad_segment_freeze(
    caps: &Capabilities,
    seg: &Path,
    target: f64,
) -> Result<(), crate::MediaError> {
    let args: Vec<String> = vec![
        "-y".into(),
        "-i".into(),
        s(seg),
        "-vf".into(),
        format!("tpad=stop_mode=clone:stop_duration={target:.3}"),
        "-af".into(),
        format!("apad=whole_dur={target:.3}"),
        "-t".into(),
        format!("{target:.3}"),
    ]
    .into_iter()
    .chain(ffmpeg::x264_aac("veryfast", 16))
    .chain([s(seg)])
    .collect();
    caps.run(&args, seg)
}

/// Split a collage group into the members whose sources still exist and the
/// fallback notes for those that vanished.
fn split_present<'a>(group: &[&'a ResolvedEntry]) -> (Vec<&'a ResolvedEntry>, Vec<String>) {
    let mut missing = vec![];
    let present = group
        .iter()
        .copied()
        .filter(|r| {
            let ok = r.still_path.exists() || r.motion_path.as_ref().is_some_and(|p| p.exists());
            if !ok {
                missing.push(format!("{} missing on disk; skipped", r.entry.asset_id));
            }
            ok
        })
        .collect();
    (present, missing)
}

/// One rendered hero segment: what to push on the timeline plus the
/// fallback notes it produced, in the order the sequential loop wrote them.
struct GroupOutcome {
    /// Entry shown (a collage carries all members' captions), segment file,
    /// probed seconds, kind. `None` when the group failed and was skipped.
    seg: Option<(ResolvedEntry, PathBuf, f64, String)>,
    /// Every member id when the segment is a collage (keyed by the lead,
    /// `seg.0.entry.asset_id`); empty otherwise.
    collage_members: Vec<String>,
    fallbacks: Vec<String>,
}

/// Short-mode time limits for one segment: `cap` bounds a single entry's
/// length, `budget` what is left of the film for the freeze-pad. Full mode
/// has no budget.
#[derive(Clone, Copy)]
struct SegmentLimits {
    cap: f64,
    budget: f64,
}

impl SegmentLimits {
    const NONE: Self = Self {
        cap: f64::INFINITY,
        budget: f64::INFINITY,
    };
}

/// Motion variant (glide direction, pan direction) of each group, from a
/// stable hash of its lead asset id rather than its position, so adding or
/// removing a shot leaves every other segment's picture, and so its cache
/// key, unchanged.
///
/// Neighbours should differ, so a group whose hashed variant equals the
/// previous group's hashed variant takes the next one. The comparison is
/// against the previous group's *hashed* value, not its final one: a
/// comparison with the final value would chain through the whole film and
/// make every later segment depend on position again. The cost: a group's
/// variant depends on its immediate neighbour, so inserting or removing a
/// shot can change the shot after it, and a rotated group can occasionally
/// land on the same variant as a neighbour that was itself rotated (about
/// 1 in 16 neighbours instead of 1 in 4, versus never with position).
fn glide_variants(groups: &[Vec<&ResolvedEntry>]) -> Vec<usize> {
    let hashed: Vec<usize> = groups
        .iter()
        .map(|g| g.first().map_or(0, |r| stable_variant(&r.entry)))
        .collect();
    hashed
        .iter()
        .enumerate()
        .map(|(i, &h)| {
            if i > 0 && hashed[i - 1] == h {
                (h + 1) % ffmpeg::GLIDE_VARIANTS
            } else {
                h
            }
        })
        .collect()
}

/// Hashed over the asset id and window index, so sibling windows of one clip
/// are not forced to the same variant.
fn stable_variant(entry: &PlanEntry) -> usize {
    let h = blake3::hash(format!("{}#{}", entry.asset_id, entry.window).as_bytes());
    usize::from(h.as_bytes()[0]) % ffmpeg::GLIDE_VARIANTS
}

/// Render, probe and (for short clips) freeze-pad one group. A failed render
/// is a fallback note, not an error: only a cancel comes back as `Err`.
#[allow(clippy::too_many_arguments)]
fn render_group(
    caps: &Capabilities,
    present: &[&ResolvedEntry],
    idx: usize,
    variant: usize,
    still_secs: f64,
    limits: SegmentLimits,
    opts: &VideoOptions,
    work_dir: &Path,
) -> Result<GroupOutcome, crate::MediaError> {
    let r = present[0];
    let mut fallbacks = vec![];
    // Long enough to read what is written on it (a lone entry's caption
    // is its collage caption, trimmed).
    // Short mode also keeps it within the group's fair share, so one long
    // caption neither starves later chapters nor freezes a clip just trimmed.
    let hold_secs = caption_hold_secs(collage_caption(present).as_deref())
        .min(limits.cap)
        .min(limits.budget);
    let rendered = if present.len() == 1 {
        render_entry_segment(
            caps,
            r,
            idx,
            variant,
            still_secs.max(hold_secs),
            limits.cap,
            opts.resolution,
            work_dir,
            &mut fallbacks,
        )
    } else {
        render_collage_segment(
            caps,
            present,
            idx,
            variant,
            // The collage's scaled hold, raised to the caption hold (not the
            // caption hold scaled, which would overrun it).
            still_secs.max(hold_secs / collage_hold_secs(present.len(), 1.0)),
            opts.live_collages,
            opts.resolution,
            work_dir,
        )
    };
    let (seg_path, dur, kind) = match rendered {
        Ok(v) => v,
        Err(crate::MediaError::Cancelled) => return Err(crate::MediaError::Cancelled),
        Err(e) => {
            fallbacks.push(format!("{} skipped: {e}", r.entry.asset_id));
            return Ok(GroupOutcome {
                seg: None,
                collage_members: vec![],
                fallbacks,
            });
        }
    };
    // Record the actual encoded duration: Live Photo trims and short
    // sources routinely encode shorter than planned, and an xfade offset
    // past the accumulated length truncates the whole chain (the film's
    // video ended at the title while music played on). Budget, overlays
    // and timeline math all read this value.
    let probed = ffmpeg::ffprobe_file(&caps.ffprobe_bin, &seg_path)
        .ok()
        .map(|p| p.duration_secs)
        .filter(|d| *d > 0.0)
        .unwrap_or(dur);
    // Flash frames are unwatchable: a sub-2.5s clip (most Live Photo
    // motions) is freeze-padded on its last frame to a minimum hold.
    let dur = match clip_pad_target(probed, limits.budget, hold_secs).filter(|_| kind == "clip") {
        Some(target) => match pad_segment_freeze(caps, &seg_path, target) {
            Ok(()) => target,
            Err(crate::MediaError::Cancelled) => return Err(crate::MediaError::Cancelled),
            Err(e) => {
                fallbacks.push(format!("{} freeze-pad failed ({e})", r.entry.asset_id));
                probed
            }
        },
        None => probed,
    };
    // A collage segment carries every member's caption and id, so the
    // sidecar and EDL account for all the photos on screen.
    let mut seg_entry = r.clone();
    let mut collage_members = vec![];
    if present.len() > 1 {
        seg_entry.caption = collage_caption(present);
        // Where it was taken is any member's place: the travel map is cut
        // in by GPS, and the lead photo may have none.
        seg_entry.gps = present.iter().find_map(|m| m.gps);
        collage_members = present.iter().map(|m| m.entry.asset_id.clone()).collect();
    }
    Ok(GroupOutcome {
        seg: Some((seg_entry, seg_path, dur, kind)),
        collage_members,
        fallbacks,
    })
}

/// Hero segments rendered at once by default: a quarter of the cores (the
/// single-threaded stages between encodes leave the rest idle for the other
/// workers), cut down so the workers' peak memory stays within half of RAM.
pub fn default_jobs(res: Resolution) -> usize {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    jobs_for(cores, total_memory_bytes(), res)
}

/// [`default_jobs`] for explicit machine facts. Unknown memory assumes 8 GiB.
pub fn jobs_for(cores: usize, mem_bytes: Option<u64>, res: Resolution) -> usize {
    let by_cpu = (cores / 4).max(1);
    let mem = mem_bytes.unwrap_or(8 << 30);
    let by_mem = (mem / 2 / res.segment_peak_bytes()).max(1) as usize;
    by_cpu.min(by_mem)
}

/// ffmpeg threads per concurrent worker: ffmpeg's own default is 1.5x the
/// cores for one process, so split that between the workers. `None` for a
/// lone worker keeps the unrestricted default.
fn thread_share(jobs: usize) -> Option<usize> {
    if jobs <= 1 {
        return None;
    }
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    Some((cores * 3 / 2 / jobs).max(2))
}

/// Physical RAM in bytes, when the OS says.
pub fn total_memory_bytes() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        let kb: u64 = text
            .lines()
            .find_map(|l| l.strip_prefix("MemTotal:"))?
            .split_whitespace()
            .next()?
            .parse()
            .ok()?;
        Some(kb * 1024)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Normalize one plan entry to a uniform segment. Returns (path, secs, kind).
#[allow(clippy::too_many_arguments)]
fn render_entry_segment(
    caps: &Capabilities,
    r: &ResolvedEntry,
    idx: usize,
    variant: usize,
    still_secs: f64,
    max_secs: f64,
    res: Resolution,
    work_dir: &Path,
    fallbacks: &mut Vec<String>,
) -> Result<(PathBuf, f64, String), crate::MediaError> {
    match &r.entry.rendition {
        Rendition::Still => {
            let out = work_dir.join(format!("seg_{idx:03}.mp4"));
            let key = |c: &SegmentCache| still_key(c, caps, r, variant, still_secs, max_secs, res);
            cached_segment(caps, key, out, "still", |out| {
                // Decode first: looping a raw HEIC fails on tmap/Dolby Vision items.
                let png = ffmpeg::decode_still_to_png(&caps.ffmpeg_bin, &r.still_path)?;
                // The decoded (oriented) pixels are the truth about aspect.
                let aspect = image::image_dimensions(&png.path)
                    .ok()
                    .filter(|&(w, h)| w > 0 && h > 0)
                    .map(|(w, h)| frame_aspect(f64::from(w) / f64::from(h), r.frame))
                    .or(r.aspect);
                let dur = if let Some(aspect) = aspect.filter(|&a| is_panorama(a)) {
                    let motion = pano_motion(aspect);
                    // A caption's reading time (carried in `still_secs`) can
                    // lengthen the pan; a short-mode budget can shorten it,
                    // never below a still.
                    let dur = motion
                        .hold_secs
                        .max(still_secs)
                        .min(max_secs.max(still_secs));
                    // Travel less when shortened so the pan keeps its speed bound.
                    let coverage = pano_coverage(aspect, dur);
                    render_pano_segment(
                        caps, &png.path, dur, aspect, coverage, variant, res, r.frame, out,
                    )?;
                    dur
                } else {
                    render_hold_segment(caps, &png.path, still_secs, variant, res, r.frame, out)?;
                    still_secs
                };
                Ok(dur)
            })
        }
        Rendition::Motion {
            trim_start,
            trim_end,
        }
        | Rendition::Clip {
            trim_start,
            trim_end,
        } => {
            let src = r.motion_path.as_ref().unwrap_or(&r.still_path);
            let len = (trim_end - trim_start)
                .clamp(0.5, MAX_CLIP_SECS)
                .min(max_secs.max(0.5));
            // Live motion (≤3 s class) may run short; reels/video allow that.
            let out = work_dir.join(format!("seg_{idx:03}.mp4"));
            // HDR check on the real source.
            let probe = ffmpeg::ffprobe_file(&caps.ffprobe_bin, src).ok();
            // Muted/camera-silent clips get a matching silent track so the
            // acrossfade assembly always has `[n:a]` to consume.
            let has_audio = probe.as_ref().is_none_or(|i| i.has_audio);
            // Upscaled first so the push-in samples, not smears.
            let (pw, ph) = res.clip_prescale();
            // Framing follows the displayed (autorotated) aspect.
            // The framed part of the source is what gets fitted.
            let display = probe
                .as_ref()
                .map(ffmpeg::ProbeInfo::display_dims)
                .map(|(w, h)| {
                    r.frame.map_or((w, h), |(_, _, fw, fh)| {
                        (
                            (u64::from(w) * u64::from(fw) / 100).max(1) as u32,
                            (u64::from(h) * u64::from(fh) / 100).max(1) as u32,
                        )
                    })
                });
            let framing = display.map_or(ClipFraming::Cover, |(w, h)| {
                clip_framing(f64::from(w) / f64::from(h.max(1)))
            });
            // Lanczos where the source is smaller than the pre-scale (the
            // upscale must not smear); the default filter for downscales,
            // where lanczos only costs time.
            let upscaling = display.is_some_and(|(w, h)| match framing {
                ClipFraming::Cover => w < pw || h < ph,
                ClipFraming::Contain => w < pw && h < ph,
            });
            let tonemap = probe.as_ref().and_then(|i| {
                let chain = ffmpeg::tonemap_chain(caps, &i.color_transfer)?;
                fallbacks.push(format!(
                    "{} HDR tone-mapped ({})",
                    r.entry.asset_id, i.color_transfer
                ));
                Some(chain)
            });
            let frames = (len * 30.0).round().max(1.0) as u32;
            let vf = clip_filter(
                caps,
                r.frame,
                framing,
                (pw, ph),
                upscaling,
                tonemap.as_deref(),
                frames,
                res,
            );
            let mut args: Vec<String> = vec![
                "-y".into(),
                "-ss".into(),
                format!("{trim_start:.3}"),
                "-t".into(),
                format!("{len:.3}"),
                "-i".into(),
                s(src),
            ];
            let audio_in = if has_audio {
                // Only the first usable stream: iPhone spatial `apple_apac`
                // tracks cannot be muxed to MP4 and sank the whole clip.
                ffmpeg::audio_spec(probe.as_ref())
            } else {
                fallbacks.push(format!(
                    "{} has no audio; silent track added",
                    r.entry.asset_id
                ));
                args.extend(silent_input(len));
                "1:a".to_string()
            };
            args.extend([
                "-map".into(),
                "0:v".into(),
                "-map".into(),
                audio_in,
                "-vf".into(),
                vf,
                "-af".into(),
                "aresample=48000,aformat=channel_layouts=stereo".into(),
            ]);
            args.extend(ffmpeg::x264_aac("veryfast", 16));
            args.extend(["-shortest".into(), s(&out)]);
            // The finished argument list (trim, filters, audio choice) is the
            // whole recipe; the output name, last, only carries the position.
            let recipe = &args[..args.len() - 1];
            let key = |c: &SegmentCache| {
                let mut k = c.key("clip");
                k.file("src", src).args(recipe);
                k.finish()
            };
            cached_segment(caps, key, out, "clip", |out| {
                caps.run(&args, out)?;
                Ok(len)
            })
        }
    }
}

/// Video filter chain of a clip or Live Photo motion: the owner's framing
/// cuts the source first, then it is normalized to 30 fps (60 fps sources
/// would otherwise composite twice the frames, and the glide's progress
/// counts frames), covered or fitted over blur into the `pw`x`ph`
/// pre-scale (upscaled first so the push-in samples, not smears), and
/// given a slow glide push-in (1.0 -> 1.05, gentler when the clip is shown
/// whole), matching the stills' Ken Burns drift so cuts don't sit static.
#[allow(clippy::too_many_arguments)]
fn clip_filter(
    caps: &Capabilities,
    frame: Option<Frame>,
    framing: ClipFraming,
    (pw, ph): (u32, u32),
    upscaling: bool,
    tonemap: Option<&str>,
    frames: u32,
    res: Resolution,
) -> String {
    let mut vf = ffmpeg::frame_crop(frame);
    vf.push_str("fps=30,");
    match framing {
        ClipFraming::Cover => {
            let flags = if upscaling { ":flags=lanczos" } else { "" };
            vf.push_str(&format!(
                "scale={pw}:{ph}:force_original_aspect_ratio=increase{flags},crop={pw}:{ph},setsar=1"
            ));
            if let Some(chain) = tonemap {
                vf.push(',');
                vf.push_str(chain);
            }
        }
        ClipFraming::Contain => vf.push_str(&ffmpeg::video_contain_over_blur(
            caps, pw, ph, upscaling, tonemap,
        )),
    }
    vf.push(',');
    vf.push_str(&ffmpeg::glide_depth(
        caps,
        frames,
        res.width(),
        res.height(),
        0,
        framing.glide_depth(),
    ));
    vf.push_str(",format=yuv420p");
    vf
}

/// Most of a clip's area a 16:9 cover crop may cut away before the clip is
/// shown whole over a blurred fill instead. 16:10 (0.10) and 16:9 itself
/// cover; 3:2 (0.16), 4:3 (0.25), squares and portraits (0.44 and up) do
/// not.
const MAX_CLIP_CROP: f64 = 0.15;

/// How a video clip fills the 16:9 frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipFraming {
    /// Scale to cover and crop the overflow (clips close to 16:9).
    Cover,
    /// Whole frame, centred over a blurred copy of itself.
    Contain,
}

impl ClipFraming {
    /// Glide window divisor: contained clips keep the whole subject on
    /// screen, so they push in more gently.
    fn glide_depth(self) -> u32 {
        match self {
            Self::Cover => ffmpeg::GLIDE_DEPTH,
            Self::Contain => ffmpeg::GLIDE_DEPTH_GENTLE,
        }
    }
}

/// Framing for a clip of displayed aspect `aspect` (width / height, after
/// rotation metadata is applied).
fn clip_framing(aspect: f64) -> ClipFraming {
    if crop_loss(aspect, 16.0 / 9.0) <= MAX_CLIP_CROP {
        ClipFraming::Cover
    } else {
        ClipFraming::Contain
    }
}

/// Extra lavfi input producing `secs` of silent 48 kHz stereo, for sources
/// that carry no audio stream.
pub(crate) fn silent_input(secs: f64) -> Vec<String> {
    vec![
        "-f".into(),
        "lavfi".into(),
        "-t".into(),
        format!("{secs:.3}"),
        "-i".into(),
        "anullsrc=r=48000:cl=stereo".into(),
    ]
}

/// Filter graph for a still hold: the full photo fitted over its own
/// blurred fill at 16:9, then a gentle Ken Burns move (≤1.05×) on the
/// finished composite, varied by index. Zooming the raw photo instead
/// both crops the subject and distorts non-16:9 sources.
/// The owner's framing, if any, cuts the photo first.
fn hold_graph(
    caps: &Capabilities,
    variant: usize,
    frames: u32,
    res: Resolution,
    frame: Option<Frame>,
) -> String {
    // Supersample the composite once, hold it, then glide over it. The fill
    // blur radius is in canvas pixels, so it scales with the resolution.
    let (sw, sh) = res.supersample();
    let (w, h) = res.dims();
    let input = format!("[0:v]{}", ffmpeg::frame_crop(frame));
    let composite = ffmpeg::fit_over_blur(caps, &input, sw, sh, "", 40 * res.scale());
    let hold = ffmpeg::hold_still(frames);
    let glide = ffmpeg::glide(caps, frames, w, h, variant);
    format!("{composite},{hold},{glide},format=yuv420p[v]")
}

/// Collage segment: up to four same-chapter stills composed into one
/// supersample canvas (1.5x the frame; cover-fitted cells, dark gaps), then held with the
/// usual glide. One segment shows the day's spread at once. With
/// `live_collages`, a Live Photo member plays its motion in its cell
/// ([`live_cells`]).
#[allow(clippy::too_many_arguments)]
fn render_collage_segment(
    caps: &Capabilities,
    members: &[&ResolvedEntry],
    idx: usize,
    variant: usize,
    still_secs: f64,
    live_collages: bool,
    res: Resolution,
    work_dir: &Path,
) -> Result<(PathBuf, f64, String), crate::MediaError> {
    let out = work_dir.join(format!("seg_{idx:03}.mp4"));
    let dur = collage_hold_secs(members.len(), still_secs);
    let live = live_collages && members.iter().any(|m| live_motion(m).is_some());
    // Members in order, plus the hold recipe; the layout and crop code is
    // pixel logic outside ffmpeg's arguments, covered by the cache version.
    let key = |c: &SegmentCache| {
        let mut k = c.key("collage");
        for (i, m) in members.iter().enumerate() {
            k.file(&format!("member{i}"), &m.still_path);
            // The owner's framing changes the pixels, not the arguments.
            k.part(&format!("frame{i}"), &format!("{:?}", m.frame));
        }
        k.part("res", &format!("{:?}", res.dims()));
        k.args(&hold_args(
            caps,
            Path::new(KEY_IN),
            dur,
            variant,
            res,
            None,
            Path::new(KEY_OUT),
        ));
        // Layout and crop rules are pixel logic the arguments do not
        // show; bump when they change so old collages are not reused.
        k.part("collage-layout", "2");
        if live {
            // The motion recipe is not in the hold arguments; bump this
            // tag whenever the moving-cell graph or timing changes.
            k.part("live", "9");
            // The tone-map chain follows these, which the ffmpeg version
            // line does not record.
            k.part("tonemap", &format!("{}:{}", caps.tonemap, caps.zscale));
            for (i, m) in members.iter().enumerate() {
                if let Some(p) = live_motion(m) {
                    k.file(&format!("motion{i}"), p);
                }
            }
        }
        k.finish()
    };
    cached_segment(caps, key, out, "still", |out| {
        let canvas = work_dir.join(format!("collage_{idx:03}.png"));
        let photos = compose_collage(&caps.ffmpeg_bin, members, res, &canvas)?;
        let moving = if live {
            live_cells(caps, members, &photos, dur)
        } else {
            vec![]
        };
        if moving.is_empty() {
            render_hold_segment(caps, &canvas, dur, variant, res, None, out)?;
        } else {
            let args = live_collage_args(caps, &canvas, &moving, dur, variant, res, out);
            match caps.run(&args, out) {
                // A motion file that will not decode must not cost the
                // collage its stills.
                Err(e) if !matches!(e, crate::MediaError::Cancelled) => {
                    render_hold_segment(caps, &canvas, dur, variant, res, None, out)?;
                }
                done => done?,
            }
        }
        Ok(dur)
    })
}

/// Serve the segment at `out` from the export's segment cache when it holds
/// `key`; otherwise `render` it (returning its seconds) and store the result.
/// The key is only built when a cache is in use.
fn cached_segment(
    caps: &Capabilities,
    key: impl FnOnce(&SegmentCache) -> String,
    out: PathBuf,
    kind: &str,
    render: impl FnOnce(&Path) -> Result<f64, crate::MediaError>,
) -> Result<(PathBuf, f64, String), crate::MediaError> {
    let cached = caps.segment_cache.as_deref().map(|c| (c, key(c)));
    if let Some((c, key)) = &cached {
        if let Some(m) = c.fetch(key, &out) {
            return Ok((out, m.secs, kind.into()));
        }
    }
    let secs = render(&out)?;
    if let Some((c, key)) = &cached {
        c.store(key, &out, secs);
    }
    Ok((out, secs, kind.into()))
}

/// How an entry may join a collage: a plain still does; a captioned still
/// or a Live Photo playing its motion only when the user joined it to a
/// neighbour; a panorama (pans solo) or a clip never does. Same rule as
/// the plan's timing model and the Story editor, through
/// [`plan::entry_candidacy`].
fn collage_candidacy(e: &ResolvedEntry) -> plan::Candidacy {
    plan::entry_candidacy(&e.entry.rendition, e.aspect, e.caption.as_deref())
}

/// Each hero asset's place section within its chapter: how many of the
/// chapter's place changes ([`Chapter::film_place_changes`]) the film has
/// reached at it, in hero order.
fn place_sections(plan: &Plan, chapter_of: impl Fn(&str) -> usize) -> HashMap<&str, usize> {
    let changes: std::collections::HashSet<&str> = plan
        .chapters
        .iter()
        .flat_map(|c| c.film_place_changes.iter().map(|(id, _)| id.as_str()))
        .collect();
    let mut reached: HashMap<usize, usize> = HashMap::new();
    let mut out = HashMap::new();
    for e in &plan.hero {
        let id = e.asset_id.as_str();
        let n = reached.entry(chapter_of(id)).or_default();
        if changes.contains(id) && !out.contains_key(id) {
            *n += 1;
        }
        out.entry(id).or_insert(*n);
    }
    out
}

/// The place a chapter's film names in place section `k`
/// ([`place_sections`]): its heading place, then each change in turn.
fn section_place(ch: &Chapter, k: usize) -> Option<&str> {
    match k {
        0 => ch.film_heading_place(),
        k => ch.film_place_changes.get(k - 1).map(|(_, p)| p.as_str()),
    }
}

/// Hero segments in playback order: each is one entry or a collage of
/// same-chapter stills, as [`plan::collage_groups`] decides under the
/// user's `collage` overrides (the plan's length estimate and the Story
/// editor group the same way). `chapter_of` may tell more apart than the
/// plan's chapters do: shots it separates never share a collage.
fn hero_groups<'a, C: PartialEq>(
    entries: &[&'a ResolvedEntry],
    chapter_of: impl Fn(&ResolvedEntry) -> C,
    collage: &CollageEdits,
) -> Vec<Vec<&'a ResolvedEntry>> {
    let slots = plan::collage_slots(
        entries.iter().map(|r| {
            (
                r.entry.asset_id.as_str(),
                chapter_of(r),
                collage_candidacy(r),
                r.taken_at,
            )
        }),
        collage,
    );
    plan::collage_groups(&slots)
        .into_iter()
        .map(|g| g.into_iter().map(|i| entries[i]).collect())
        .collect()
}

/// Under this width / height a photo counts as a portrait, which the
/// collage layout never puts in a stacked (half-height) cell.
const PORTRAIT_BELOW: f64 = 0.95;
/// Most of a photo a collage cell may crop away before the photo is
/// shown whole over its own blurred fill instead. Set so four 3:4
/// portraits side by side (41% cut from the sides) still fill their
/// columns: a filled panel reads better than a photo floating on blur. Faces survive because
/// the crop keeps the top ([`CROP_FOCUS_Y`]); beyond this the layout and
/// the renderer both treat the cell as a poor fit.
const MAX_CELL_CROP: f64 = 0.45;
/// Focal height for cover crops that must cut height: people sit in the
/// upper part of a portrait, so keep the top (0 = top, 0.5 = centre).
const CROP_FOCUS_Y: f64 = 0.35;
/// Focal height for a framed photo's cover crop: the owner placed the
/// rectangle, so any remaining cut is taken evenly from both sides.
const FRAMED_FOCUS_Y: f64 = 0.5;

/// Focal height of a cell's cover crop, still or motion alike.
fn cover_focus_y(framed: bool) -> f64 {
    if framed {
        FRAMED_FOCUS_Y
    } else {
        CROP_FOCUS_Y
    }
}
/// Weight of fill that [`fill_cell`] contains rather than covers, against
/// one that crops the same amount: shown whole beats cut off, but blur
/// still wastes the cell.
const CONTAIN_PENALTY: f64 = 0.6;
/// Layout tie-break: cost per pair of photos out of capture order.
const REORDER_PENALTY: f64 = 0.01;

/// One collage cell: which member it holds and its rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CollageCell {
    member: usize,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

/// Column structures tried per photo count: each entry is how many cells
/// (1 whole, 2 stacked) its column holds, left to right.
fn column_shapes(n: usize) -> &'static [&'static [usize]] {
    match n {
        1 => &[&[1]],
        2 => &[&[1, 1]],
        3 => &[&[1, 1, 1], &[1, 2], &[2, 1]],
        4 => &[&[1, 1, 1, 1], &[2, 2], &[1, 1, 2], &[2, 1, 1], &[1, 2, 1]],
        _ => &[],
    }
}

fn permutations(n: usize) -> Vec<Vec<usize>> {
    fn go(cur: &mut Vec<usize>, left: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
        if left.is_empty() {
            out.push(cur.clone());
            return;
        }
        for i in 0..left.len() {
            let m = left.remove(i);
            cur.push(m);
            go(cur, left, out);
            cur.pop();
            left.insert(i, m);
        }
    }
    let mut out = vec![];
    go(&mut vec![], &mut (0..n).collect(), &mut out);
    out
}

/// Badness of showing a `sa`-aspect photo in a `ca`-aspect cell: the share
/// cropped away while it still covers, then a discounted rate for the
/// share of the cell wasted on blur once it is shown whole. A framed photo
/// (`cover`) always fills its cell, so only the share cropped counts.
fn cell_penalty(sa: f64, ca: f64, cover: bool) -> f64 {
    let loss = crop_loss(sa, ca);
    if cover || loss <= MAX_CELL_CROP {
        loss
    } else {
        MAX_CELL_CROP + CONTAIN_PENALTY * (loss - MAX_CELL_CROP)
    }
}

/// Cells for `order` (members, left to right and top to bottom) arranged
/// into columns of `shape`, with column widths following `weights`.
fn build_columns(
    order: &[usize],
    shape: &[usize],
    weights: &[f64],
    w: u32,
    h: u32,
) -> Vec<CollageCell> {
    let total: f64 = weights.iter().sum();
    let (mut cells, mut x, mut acc, mut next) = (vec![], 0u32, 0.0, 0usize);
    for (k, (&count, wt)) in shape.iter().zip(weights).enumerate() {
        acc += wt;
        let right = if k + 1 == shape.len() {
            w
        } else {
            (f64::from(w) * acc / total).round() as u32
        };
        let (cw, half) = (right - x, h / 2);
        for j in 0..count {
            let (y, ch) = if count == 1 {
                (0, h)
            } else if j == 0 {
                (0, half)
            } else {
                (half, h - half)
            };
            cells.push(CollageCell {
                member: order[next],
                x,
                y,
                w: cw,
                h: ch,
            });
            next += 1;
        }
        x = right;
    }
    cells
}

/// Ideal width of each column of `shape` filled by `order`: the width at
/// which its cells match their photos' mean aspect. `None` when a portrait
/// would land in a stacked (short) cell.
fn ideal_column_widths(
    order: &[usize],
    shape: &[usize],
    aspects: &[f64],
    h: u32,
) -> Option<Vec<f64>> {
    let mut at = 0;
    shape
        .iter()
        .map(|&count| {
            let members = &order[at..at + count];
            at += count;
            if count > 1 && members.iter().any(|&m| aspects[m] < PORTRAIT_BELOW) {
                return None;
            }
            let mean = members.iter().map(|&m| aspects[m]).sum::<f64>() / count as f64;
            Some(mean * f64::from(h) / count as f64)
        })
        .collect()
}

/// Lay out a collage by photo shape (`aspects` are width / height in
/// member order). Every column arrangement (a row of full-height cells,
/// some columns split in two) of every ordering is scored by how much of
/// each photo its cell crops or wastes; the cheapest wins, with a small
/// preference for capture order. Portraits never go in a stacked (short)
/// cell, so selfies get tall columns and landscapes share the rest.
/// Cells tile the canvas exactly.
/// `covers[i]` marks a member the owner framed: it always covers its cell
/// (missing entries are `false`).
fn collage_layout(aspects: &[f64], covers: &[bool], w: u32, h: u32) -> Vec<CollageCell> {
    let n = aspects.len().min(MAX_COLLAGE);
    let total_area = f64::from(w) * f64::from(h);
    let fit = |cells: &[CollageCell]| -> f64 {
        cells
            .iter()
            .map(|c| {
                let ca = f64::from(c.w) / f64::from(c.h);
                cell_penalty(
                    aspects[c.member],
                    ca,
                    covers.get(c.member).copied().unwrap_or(false),
                ) * f64::from(c.w)
                    * f64::from(c.h)
                    / total_area
            })
            .sum()
    };
    let mut best: Option<(f64, Vec<CollageCell>)> = None;
    for order in permutations(n) {
        let inversions = (0..n)
            .flat_map(|i| (i + 1..n).map(move |j| (i, j)))
            .filter(|&(i, j)| order[i] > order[j])
            .count();
        let reorder = REORDER_PENALTY * inversions as f64;
        for shape in column_shapes(n) {
            let Some(ideal) = ideal_column_widths(&order, shape, aspects, h) else {
                continue;
            };
            // Candidate widths: each column's ideal (floored so no column
            // collapses below half the mean), a sqrt-damped blend, and
            // equal columns.
            let mean_ideal = ideal.iter().sum::<f64>() / ideal.len() as f64;
            let floored: Vec<f64> = ideal.iter().map(|&v| v.max(0.5 * mean_ideal)).collect();
            let damped = floored.iter().map(|v| v.sqrt()).collect();
            let sets = [floored, damped, vec![1.0; ideal.len()]];
            for weights in &sets {
                let cells = build_columns(&order, shape, weights, w, h);
                let sc = fit(&cells) + reorder;
                // Strictly better only, so earlier (closer to capture
                // order) candidates win ties despite float noise.
                if best.as_ref().is_none_or(|(b, _)| sc < *b - 1e-9) {
                    best = Some((sc, cells));
                }
            }
        }
    }
    best.map(|(_, cells)| cells).unwrap_or_default()
}

/// Fraction of a `sa`-aspect photo cut away to cover a `ca`-aspect cell.
fn crop_loss(sa: f64, ca: f64) -> f64 {
    1.0 - (sa / ca).min(ca / sa)
}

/// Cover-fit `src` into `tw`x`th`, cutting height around `focus_y`
/// (0 top .. 1 bottom) and width around the centre.
fn cover_fit_focus(src: &image::RgbImage, tw: u32, th: u32, focus_y: f64) -> image::RgbImage {
    let (sw, sh) = (
        f64::from(src.width().max(1)),
        f64::from(src.height().max(1)),
    );
    let scale = (f64::from(tw) / sw).max(f64::from(th) / sh);
    let (nw, nh) = (
        ((sw * scale).round() as u32).max(tw),
        ((sh * scale).round() as u32).max(th),
    );
    let big = image::imageops::resize(src, nw, nh, image::imageops::FilterType::Triangle);
    let x = (nw - tw) / 2;
    let y = (f64::from(nh - th) * focus_y).round() as u32;
    image::imageops::crop_imm(&big, x, y.min(nh - th), tw, th).to_image()
}

/// The whole photo centred over a blurred, cover-fitted copy of itself
/// (the film's usual fit-over-blur, per cell).
fn contain_over_blur(src: &image::RgbImage, tw: u32, th: u32) -> image::RgbImage {
    // Blur a small copy and scale it up: same look, a fraction of the cost.
    let small = cover_fit_focus(src, (tw / 8).max(1), (th / 8).max(1), 0.5);
    let blurred = image::imageops::blur(&small, 3.0);
    let mut cell = image::imageops::resize(&blurred, tw, th, image::imageops::FilterType::Triangle);
    let (sw, sh) = (
        f64::from(src.width().max(1)),
        f64::from(src.height().max(1)),
    );
    let (fw, fh) = contain_size(sw / sh, tw, th);
    let fg = image::imageops::resize(src, fw, fh, image::imageops::FilterType::Triangle);
    image::imageops::replace(
        &mut cell,
        &fg,
        i64::from((tw - fw) / 2),
        i64::from((th - fh) / 2),
    );
    cell
}

/// Size of a photo of aspect `sa` (width / height) shown whole inside
/// `tw`x`th`.
fn contain_size(sa: f64, tw: u32, th: u32) -> (u32, u32) {
    let (tw_f, th_f) = (f64::from(tw), f64::from(th));
    let (fw, fh) = if sa >= tw_f / th_f {
        (tw_f, tw_f / sa)
    } else {
        (th_f * sa, th_f)
    };
    (
        (fw.round() as u32).clamp(1, tw),
        (fh.round() as u32).clamp(1, th),
    )
}

/// Where [`fill_cell`] puts the photo itself inside a `cw`x`ch` tile at
/// (`x`, `y`): the whole tile when it cover-crops, the centred fitted
/// rectangle when the photo is shown whole over its blur.
fn photo_rect(sa: f64, cover: bool, x: u32, y: u32, cw: u32, ch: u32) -> (u32, u32, u32, u32) {
    let ca = f64::from(cw) / f64::from(ch.max(1));
    if cover || crop_loss(sa, ca) <= MAX_CELL_CROP {
        (x, y, cw, ch)
    } else {
        let (fw, fh) = contain_size(sa, cw, ch);
        (x + (cw - fw) / 2, y + (ch - fh) / 2, fw, fh)
    }
}

/// Fill a cell: cover-crop (top-biased) when little is lost, else the
/// whole photo over its own blur so faces and subjects are never cut off.
/// A photo the owner framed (`framed`) always covers, cut around its
/// centre: the owner chose that rectangle so nobody's head is lost.
fn fill_cell(img: &image::RgbImage, cw: u32, ch: u32, framed: bool) -> image::RgbImage {
    let sa = f64::from(img.width().max(1)) / f64::from(img.height().max(1));
    let ca = f64::from(cw) / f64::from(ch.max(1));
    if framed || crop_loss(sa, ca) <= MAX_CELL_CROP {
        cover_fit_focus(img, cw, ch, cover_focus_y(framed))
    } else {
        contain_over_blur(img, cw, ch)
    }
}

/// Compose 2-4 photos into a supersample-size collage canvas with an
/// orientation-aware layout ([`collage_layout`]) and thin dark gaps.
/// Photos decode, then fill their cells, in parallel: each is an
/// independent ffmpeg spawn plus a resize.
fn compose_collage(
    ffmpeg_bin: &str,
    members: &[&ResolvedEntry],
    res: Resolution,
    out: &Path,
) -> Result<Vec<CollageCell>, crate::MediaError> {
    let (w_px, h_px) = res.supersample();
    let gap = collage_gap(res);
    // Scoped threads start with no cancel flag; hand them this thread's.
    let cancel = triptape_core::process::current().unwrap_or_default();
    let images = std::thread::scope(|scope| {
        let cancel = &cancel;
        let workers: Vec<_> = members
            .iter()
            .map(|m| {
                scope.spawn(move || -> Result<image::RgbImage, crate::MediaError> {
                    let png = triptape_core::process::with_cancel(cancel, || {
                        ffmpeg::decode_still_to_png(ffmpeg_bin, &m.still_path)
                    })?;
                    let img = image::open(&png.path)
                        .map_err(|e| crate::MediaError::Io(format!("collage open: {e}")))?;
                    let img = img.to_rgb8();
                    Ok(match m.frame {
                        Some(f) => crop_to_frame(&img, f),
                        None => img,
                    })
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().expect("collage worker panicked"))
            .collect::<Result<Vec<_>, _>>()
    })?;
    let aspects: Vec<f64> = images
        .iter()
        .map(|i| f64::from(i.width().max(1)) / f64::from(i.height().max(1)))
        .collect();
    let covers: Vec<bool> = members.iter().map(|m| m.frame.is_some()).collect();
    let cells = collage_layout(&aspects, &covers, w_px, h_px);
    let tiles: Vec<image::RgbImage> = std::thread::scope(|scope| {
        let workers: Vec<_> = cells
            .iter()
            .map(|c| {
                let img = &images[c.member];
                let framed = covers[c.member];
                scope.spawn(move || fill_cell(img, c.w - gap, c.h - gap, framed))
            })
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().expect("collage worker panicked"))
            .collect()
    });
    let mut canvas = image::RgbImage::from_pixel(w_px, h_px, image::Rgb([13, 20, 32]));
    for (tile, c) in tiles.iter().zip(&cells) {
        image::imageops::replace(
            &mut canvas,
            tile,
            i64::from(c.x + gap / 2),
            i64::from(c.y + gap / 2),
        );
    }
    canvas
        .save(out)
        .map_err(|e| crate::MediaError::Io(format!("collage save: {e}")))?;
    // Where each member's photo sits on the canvas, for moving cells.
    Ok(cells
        .iter()
        .map(|c| {
            let (x, y, w, h) = photo_rect(
                aspects[c.member],
                covers[c.member],
                c.x + gap / 2,
                c.y + gap / 2,
                c.w - gap,
                c.h - gap,
            );
            CollageCell {
                member: c.member,
                x,
                y,
                w,
                h,
            }
        })
        .collect())
}

/// The part of `img` inside the owner's framing rectangle (percent of the
/// image from its top left), at least one pixel each way.
fn crop_to_frame(img: &image::RgbImage, f: Frame) -> image::RgbImage {
    let (iw, ih) = (img.width().max(1), img.height().max(1));
    let at = |len: u32, pct: u8| ((u64::from(len) * u64::from(pct) + 50) / 100) as u32;
    let x = at(iw, f.0).min(iw - 1);
    let y = at(ih, f.1).min(ih - 1);
    let w = at(iw, f.2).clamp(1, iw - x);
    let h = at(ih, f.3).clamp(1, ih - y);
    image::imageops::crop_imm(img, x, y, w, h).to_image()
}

/// Dark gap between collage cells, in supersample-canvas pixels.
fn collage_gap(res: Resolution) -> u32 {
    18 * res.scale()
}

/// The Live Photo motion file of a collage member, when it has one.
fn live_motion(m: &ResolvedEntry) -> Option<&Path> {
    if m.still {
        return None;
    }
    m.motion_path
        .as_deref()
        .filter(|p| *p != m.still_path.as_path() && p.exists())
}

/// Seconds cut from the end of a Live Photo's motion in a collage when
/// its steadiness cannot be measured: the phone is often being lowered
/// there.
const LIVE_TAIL_TRIM: f64 = 0.6;
/// Shortest motion worth playing in a collage cell.
const LIVE_MIN_SECS: f64 = 0.8;

/// Seconds a collage rests after its last moving cell has stopped.
const LIVE_LINGER: f64 = 1.0;
/// Seconds over which a moving cell's motion dissolves in over its still
/// when its turn comes. The motion file is low resolution and often
/// blurred, so a cell waiting its turn shows the sharp still, never the
/// motion's first frame.
const LIVE_FADE_IN: f64 = 0.25;
/// Seconds over which a finished motion dissolves back into its sharp
/// still, where the cell rests (a Live Photo on a phone ends on its key
/// photo). The dissolve also hides any framing difference between the
/// motion's last frame and the still.
const LIVE_FADE_OUT: f64 = 0.3;

/// When each moving cell of a collage starts, given their playable
/// seconds in playing order and the collage's hold. All starting at once
/// would leave a long frozen wait before the cut, so the starts spread
/// out until the last motion ends [`LIVE_LINGER`] before the hold does.
/// A cell never waits past the end of the one before it (no dead gap),
/// and a single moving cell starts at once.
fn stagger_starts(secs: &[f64], dur: f64) -> Vec<f64> {
    let mut starts = vec![0.0; secs.len()];
    let Some(last) = secs.last() else {
        return starts;
    };
    let last_start = (dur - LIVE_LINGER - last).max(0.0);
    for i in 1..secs.len() {
        // Even steps over what is left, so a step cut short by a brief
        // motion is made up by the later ones.
        let step = (last_start - starts[i - 1]) / (secs.len() - i) as f64;
        starts[i] = starts[i - 1] + step.clamp(0.0, secs[i - 1]);
    }
    starts
}

/// One moving collage cell: its motion file, tile rectangle on the
/// canvas, when it starts and for how many seconds it plays, and an
/// optional HDR tone-map chain.
struct LiveCell {
    src: PathBuf,
    start: f64,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    secs: f64,
    tonemap: Option<String>,
    /// The member's framing, applied to the motion as to its still.
    frame: Option<Frame>,
}

/// Motion cells of a composed collage, from the rectangle each member's
/// photo occupies (`photos`, as [`compose_collage`] returns). The clip is
/// cover-cropped onto its still, so a photo shown whole over its blur
/// moves whole over the same blur. The motion stops where the phone
/// starts being lowered ([`crate::clip_analysis::live_steady_secs`]). A clip whose shape is too far from its
/// still's ([`MAX_CELL_CROP`]) or that is too short to read stays a still.
fn live_cells(
    caps: &Capabilities,
    members: &[&ResolvedEntry],
    photos: &[CollageCell],
    dur: f64,
) -> Vec<LiveCell> {
    // Playing order: left to right, then top to bottom.
    let mut photos: Vec<&CollageCell> = photos.iter().collect();
    photos.sort_by_key(|c| (c.x, c.y));
    let mut live: Vec<LiveCell> = photos
        .into_iter()
        .filter_map(|c| {
            let src = live_motion(members[c.member])?;
            let probe = ffmpeg::ffprobe_file(&caps.ffprobe_bin, src).ok()?;
            let (dw, dh) = probe.display_dims();
            let frame = members[c.member].frame;
            let sa = frame_aspect(f64::from(dw.max(1)) / f64::from(dh.max(1)), frame);
            let ca = f64::from(c.w.max(1)) / f64::from(c.h.max(1));
            // As long as it looks good: up to where the phone drops.
            let steady = crate::clip_analysis::live_steady_secs(&caps.ffmpeg_bin, src)
                .unwrap_or(probe.duration_secs - LIVE_TAIL_TRIM);
            let secs = steady.min(probe.duration_secs).min(dur);
            // A framed cell covers its still, so its motion covers too.
            let poor_fit = frame.is_none() && crop_loss(sa, ca) > MAX_CELL_CROP;
            if !probe.has_video || poor_fit || secs < LIVE_MIN_SECS {
                return None;
            }
            Some(LiveCell {
                src: src.to_path_buf(),
                start: 0.0,
                x: c.x,
                y: c.y,
                w: c.w,
                h: c.h,
                secs,
                tonemap: ffmpeg::tonemap_chain(caps, &probe.color_transfer),
                frame,
            })
        })
        .collect();
    let secs: Vec<f64> = live.iter().map(|c| c.secs).collect();
    for (c, start) in live.iter_mut().zip(stagger_starts(&secs, dur)) {
        c.start = start;
        // Whatever the stagger, the motion ends inside the hold.
        c.secs = c.secs.min(dur - start);
    }
    live
}

/// Filter graph of a collage whose `live` cells move: input 0 is the
/// composed canvas, inputs 1.. the motion files in `live` order. Each
/// motion is cover-cropped like its still. Until its start the cell shows
/// the sharp still the canvas holds (nothing is overlaid); then the motion
/// dissolves in over [`LIVE_FADE_IN`], plays from its first frame, and
/// dissolves back out over [`LIVE_FADE_OUT`] to the still, where the cell
/// rests while the whole canvas glides. No cell ever rests on a video
/// frame.
fn live_collage_graph(
    caps: &Capabilities,
    live: &[LiveCell],
    frames: u32,
    variant: usize,
    res: Resolution,
) -> String {
    let (sw, sh) = res.supersample();
    let (w, h) = res.dims();
    let mut g = format!(
        "[0:v]scale={sw}:{sh},setsar=1,{},format=rgb24[c0];",
        ffmpeg::hold_still(frames)
    );
    for (i, c) in live.iter().enumerate() {
        let n = i + 1;
        let tone = c
            .tonemap
            .as_ref()
            .map_or(String::new(), |t| format!("{t},"));
        // Alpha fades in and out so the still shows through on either side;
        // the motion is shifted to its start and the overlay only runs in
        // its window, so outside it the canvas passes through untouched.
        let fade_in = LIVE_FADE_IN.min(c.secs / 4.0);
        let fade_out = LIVE_FADE_OUT.min(c.secs / 4.0);
        g.push_str(&format!(
            "[{n}:v]{crop}fps=30,trim=duration={secs:.3},setpts=PTS-STARTPTS,{tone}format=rgb24,scale={cw}:{ch}:force_original_aspect_ratio=increase:flags=lanczos,crop={cw}:{ch}:(iw-{cw})/2:(ih-{ch})*{fy}:exact=1,setsar=1,format=rgba,fade=t=in:st=0:d={fade_in:.3}:alpha=1,fade=t=out:st={out_st:.3}:d={fade_out:.3}:alpha=1,setpts=PTS+{start:.3}/TB[m{n}];[c{i}][m{n}]overlay={x}:{y}:eof_action=pass:enable='between(t,{start:.3},{end:.3})'[c{n}];",
            secs = c.secs,
            out_st = c.secs - fade_out,
            start = c.start,
            end = c.start + c.secs,
            cw = c.w,
            ch = c.h,
            fy = cover_focus_y(c.frame.is_some()),
            crop = ffmpeg::frame_crop(c.frame),
            x = c.x,
            y = c.y,
        ));
    }
    g.push_str(&format!(
        "[c{}]{},format=yuv420p[v]",
        live.len(),
        ffmpeg::glide(caps, frames, w, h, variant)
    ));
    g
}

/// Arguments of a moving collage: the canvas, each motion file, a silent
/// track, and [`live_collage_graph`].
fn live_collage_args(
    caps: &Capabilities,
    canvas: &Path,
    live: &[LiveCell],
    dur: f64,
    variant: usize,
    res: Resolution,
    out: &Path,
) -> Vec<String> {
    let frames = (dur * 30.0).round() as u32;
    let mut args = vec!["-y".to_string(), "-i".into(), s(canvas)];
    for c in live {
        args.extend(["-i".to_string(), s(&c.src)]);
    }
    args.extend([
        "-f".to_string(),
        "lavfi".into(),
        "-i".into(),
        format!("anullsrc=r=48000:cl=stereo:d={dur}"),
        "-filter_complex".into(),
        live_collage_graph(caps, live, frames, variant, res),
        "-map".into(),
        "[v]".into(),
        "-map".into(),
        format!("{}:a", live.len() + 1),
        "-frames:v".into(),
        frames.to_string(),
    ]);
    args.extend(ffmpeg::x264_aac("veryfast", 16));
    args.extend(["-shortest".to_string(), s(out)]);
    args
}

/// Filter graph of a panorama pan.
fn pano_graph(
    dur: f64,
    aspect: f64,
    coverage: f64,
    variant: usize,
    res: Resolution,
    frame: Option<Frame>,
) -> String {
    let frames = (dur * 30.0).round() as u32;
    format!(
        "[0:v]{}{},format=yuv420p[v]",
        ffmpeg::frame_crop(frame),
        ffmpeg::pano_pan(
            frames,
            res.width(),
            res.height(),
            aspect,
            coverage,
            variant % 2 == 1
        )
    )
}

/// Panorama segment: pans across the whole image ([`ffmpeg::pano_pan`]);
/// direction alternates with the variant.
#[allow(clippy::too_many_arguments)]
fn render_pano_segment(
    caps: &Capabilities,
    img: &Path,
    dur: f64,
    aspect: f64,
    coverage: f64,
    variant: usize,
    res: Resolution,
    frame: Option<Frame>,
    out: &Path,
) -> Result<(), crate::MediaError> {
    let graph = pano_graph(dur, aspect, coverage, variant, res, frame);
    render_graph_segment(caps, img, dur, graph, out)
}

/// Arguments of a still Ken Burns hold.
fn hold_args(
    caps: &Capabilities,
    img: &Path,
    dur: f64,
    variant: usize,
    res: Resolution,
    frame: Option<Frame>,
    out: &Path,
) -> Vec<String> {
    let frames = (dur * 30.0).round() as u32;
    graph_segment_args(img, dur, hold_graph(caps, variant, frames, res, frame), out)
}

/// Stand-ins for the input and output paths when a segment's ffmpeg
/// arguments are hashed into its key: the recipe, not where it runs.
const KEY_IN: &str = "<IN>";
const KEY_OUT: &str = "<OUT>";
/// Stand-in for the per-segment overlay PNG, whose name carries its index.
const KEY_PNG: &str = "<PNG>";

/// Cache key of a still entry, before anything is decoded. The source's
/// identity stands in for its pixels (and so for the aspect that picks hold
/// or pan); the generated hold arguments, and the pan arguments for a
/// reference panorama, stand in for the renderer code.
fn still_key(
    cache: &SegmentCache,
    caps: &Capabilities,
    r: &ResolvedEntry,
    variant: usize,
    still_secs: f64,
    max_secs: f64,
    res: Resolution,
) -> String {
    let mut k = cache.key("still");
    k.file("src", &r.still_path)
        .part("res", &format!("{:?}", res.dims()))
        .num("still_secs", still_secs)
        .num("max_secs", max_secs)
        .part("aspect", &format!("{:?}", r.aspect))
        .part("frame", &format!("{:?}", r.frame));
    k.args(&hold_args(
        caps,
        Path::new(KEY_IN),
        still_secs,
        variant,
        res,
        r.frame,
        Path::new(KEY_OUT),
    ));
    // Any panorama aspect will do: its pan arguments fingerprint the pan code.
    let reference = 3.0;
    let dur = pano_motion(reference).hold_secs;
    k.args(&graph_segment_args(
        Path::new(KEY_IN),
        dur,
        pano_graph(
            dur,
            reference,
            pano_coverage(reference, dur),
            variant,
            res,
            r.frame,
        ),
        Path::new(KEY_OUT),
    ));
    k.finish()
}

/// Still Ken Burns hold.
fn render_hold_segment(
    caps: &Capabilities,
    img: &Path,
    dur: f64,
    variant: usize,
    res: Resolution,
    frame: Option<Frame>,
    out: &Path,
) -> Result<(), crate::MediaError> {
    caps.run(&hold_args(caps, img, dur, variant, res, frame, out), out)
}

/// Encode a text card that holds perfectly still for `dur` seconds, at the
/// film's 30 fps so it crossfades with the moving segments around it.
fn render_still_card(
    caps: &Capabilities,
    img: &Path,
    dur: f64,
    res: Resolution,
    out: &Path,
) -> Result<(), crate::MediaError> {
    caps.run(&still_card_args(img, dur, res, out), out)
}

fn still_card_args(img: &Path, dur: f64, res: Resolution, out: &Path) -> Vec<String> {
    let (w, h) = res.dims();
    graph_segment_args(
        img,
        dur,
        format!(
            "[0:v]fps=30,scale={w}:{h}:force_original_aspect_ratio=decrease:flags=lanczos,\
             pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1,format=yuv420p[v]"
        ),
        out,
    )
}

/// Encode a one-image segment of `dur` seconds through `graph` (a
/// `[0:v]` ... `[v]` filter chain) with silent stereo audio.
fn render_graph_segment(
    caps: &Capabilities,
    img: &Path,
    dur: f64,
    graph: String,
    out: &Path,
) -> Result<(), crate::MediaError> {
    caps.run(&graph_segment_args(img, dur, graph, out), out)
}

fn graph_segment_args(img: &Path, dur: f64, graph: String, out: &Path) -> Vec<String> {
    let frames = (dur * 30.0).round() as u32;
    [
        vec![
            "-y".into(),
            "-loop".into(),
            "1".into(),
            "-i".into(),
            s(img),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            format!("anullsrc=r=48000:cl=stereo:d={dur}"),
            "-filter_complex".into(),
            graph,
            "-map".into(),
            "[v]".into(),
            "-map".into(),
            "1:a".into(),
            "-frames:v".into(),
            frames.to_string(),
        ],
        ffmpeg::x264_aac("veryfast", 16),
        vec!["-shortest".into(), s(out)],
    ]
    .concat()
}

#[cfg(test)]
mod tests {
    use super::*;
    use triptape_core::plan::{CAPTION_MAX_SECS, PLACES_CARD_MAX_SECS};

    #[test]
    fn a_caption_ends_before_the_cut_into_the_next_shot() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = card_entry("a", Path::new("a.png"));
        a.caption = Some("Onto cruise #2! Disney Adventure".into());
        let mut b = card_entry("b", Path::new("b.png"));
        b.caption = Some("Castaway Cay".into());
        // A 3 s shot whose cut into the next (a dip to black) starts at 2.2 s.
        let segs = vec![
            (a, PathBuf::from("a.mp4"), 3.0, "still".to_string()),
            (b, PathBuf::from("b.mp4"), 3.0, "still".to_string()),
        ];
        let out = dir.path().join("c.srt");
        assert_eq!(
            write_captions_srt(&segs, &[0.0, 2.2], &[false, false], &out).unwrap(),
            2
        );
        let srt = std::fs::read_to_string(&out).unwrap();
        assert!(srt.contains("00:00:00,300 --> 00:00:02,200"), "{srt}");
        // A cut too soon after the shot starts leaves no time: no cue.
        let short = dir.path().join("s.srt");
        let n = write_captions_srt(&segs, &[0.0, 0.6], &[false, true], &short).unwrap();
        assert_eq!(n, 0);
        // The last shot has no cut after it, so only its own length bounds it.
        assert!(srt.contains("00:00:02,500 --> 00:00:04,600"), "{srt}");
    }

    #[test]
    fn the_places_card_holds_still() {
        let args = still_card_args(
            Path::new("p.png"),
            6.0,
            Resolution::Hd1080,
            Path::new("o.mp4"),
        );
        let graph = &args[args.iter().position(|a| a == "-filter_complex").unwrap() + 1];
        assert!(
            !graph.contains("zoompan") && !graph.contains("perspective") && !graph.contains("crop")
        );
        assert!(graph.contains("scale=1920:1080"));
        assert!(graph.starts_with("[0:v]fps=30,"), "{graph}");
    }

    #[test]
    fn credits_name_a_shared_parent_place_once() {
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            credit_places(&names(&[
                "Marina Bay Sands",
                "Flower Dome, Gardens by the Bay",
                "Cloud Forest, Gardens by the Bay",
                "Gardens by the Bay",
                "A-Mei Tea House, Jiufen",
                "Ontario, California",
                "Tokyo, Japan",
                "Kyoto, Japan",
            ])),
            names(&[
                "Marina Bay Sands",
                "Gardens by the Bay",
                "A-Mei Tea House, Jiufen",
                "Ontario, California",
                "Tokyo, Japan",
                "Kyoto, Japan",
            ])
        );
    }

    #[test]
    fn the_day_line_shows_once_a_day() {
        let mut last = None;
        let h = |d: &str, p: &str| (d.to_string(), Some(p.to_string()));
        assert_eq!(
            day_once(&mut last, h("Day 16 · Sun, Oct 18", "Victoria Peak")),
            h("Day 16 · Sun, Oct 18", "Victoria Peak")
        );
        assert_eq!(
            day_once(&mut last, h("Day 16 · Sun, Oct 18", "Stanley Market")),
            ("Stanley Market".to_string(), None)
        );
        assert_eq!(
            day_once(&mut last, h("Day 17 · Mon, Oct 19", "Carnival Luminosa")),
            h("Day 17 · Mon, Oct 19", "Carnival Luminosa")
        );
        // An old plan's title-only heading passes through.
        assert_eq!(
            day_once(&mut last, ("Chapter".to_string(), None)),
            ("Chapter".to_string(), None)
        );
    }

    #[test]
    fn a_long_film_is_joined_in_chunks_on_the_same_timeline() {
        let n = 2 * ASSEMBLY_CHUNK + 5;
        let durations: Vec<f64> = (0..n).map(|i| 3.0 + (i % 4) as f64).collect();
        let transitions: Vec<Option<Transition>> = (0..n)
            .map(|i| {
                (i > 0).then_some(if i % 7 == 0 {
                    Transition::FadeBlack
                } else {
                    Transition::Fade
                })
            })
            .collect();
        let overlaps: Vec<f64> = transitions[1..]
            .iter()
            .map(|t| transition_overlap(&t.unwrap()))
            .collect();
        let ranges = assembly_chunks(&vec![false; n], ASSEMBLY_CHUNK);
        let plan = plan_chunks(&durations, &transitions, &overlaps, &ranges);
        let ranges = plan.ranges.expect("chunked");
        assert_eq!(ranges.len(), 3);
        assert_eq!(ranges.last().unwrap().end, n);
        let j = plan.joined;
        // Same film length as one graph over every segment.
        let total = |d: &[f64], o: &[f64]| d.iter().sum::<f64>() - o.iter().sum::<f64>();
        assert!((total(&j.durations, &j.overlaps) - total(&durations, &overlaps)).abs() < 1e-9);
        // The cut into each chunk is its first segment's cut.
        assert_eq!(j.transitions[1], transitions[ASSEMBLY_CHUNK]);
        assert_eq!(j.overlaps[0], overlaps[ASSEMBLY_CHUNK - 1]);
        assert_eq!(j.transitions[0], None);
        // A short film is one graph, as before.
        let short = plan_chunks(
            &durations[..5],
            &transitions[..5],
            &overlaps[..4],
            &assembly_chunks(&[false; 5], ASSEMBLY_CHUNK),
        );
        assert!(short.ranges.is_none());
        assert_eq!(short.joined.durations, durations[..5]);
    }

    #[test]
    fn an_xfade_chain_names_its_finished_streams() {
        let tr = [None, Some(Transition::Fade), Some(Transition::FadeBlack)];
        let (g, v, a) = xfade_chain(&tr, &[0.5, 0.8], &[4.5, 8.7], true, None);
        assert_eq!(
            g,
            "[0:v][1:v]xfade=transition=fade:duration=0.500:offset=4.500[v1];\
             [0:a][1:a]acrossfade=d=0.500:c1=tri:c2=tri[a1];\
             [v1][2:v]xfade=transition=fadeblack:duration=0.800:offset=8.700[v];\
             [a1][2:a]acrossfade=d=0.800:c1=tri:c2=tri[a];"
        );
        assert_eq!((v.as_str(), a.as_str()), ("[v]", "[a]"));
        let (g, v, a) = xfade_chain(&[None], &[], &[], false, None);
        assert!(g.is_empty());
        assert_eq!((v.as_str(), a.as_str()), ("[0:v]", "[0:a]"));
    }

    /// A draft scales every input to its frame first, so the cuts, the
    /// caption burn and the encode all run at 1080p on the same timeline.
    #[test]
    fn a_draft_graph_scales_each_input_before_its_cut() {
        let tr = [None, Some(Transition::Fade), Some(Transition::FadeBlack)];
        let scale = Recipe::Draft(ffmpeg::DraftEncoder::X264Ultrafast).scale();
        assert_eq!(scale, Some((1920, 1080)));
        assert_eq!(Recipe::Keepsake.scale(), None);
        let (g, v, _) = xfade_chain(&tr, &[0.5, 0.8], &[4.5, 8.7], false, scale);
        assert_eq!(
            g,
            "[0:v]scale=1920:1080:flags=bilinear,setsar=1[s0];\
             [1:v]scale=1920:1080:flags=bilinear,setsar=1[s1];\
             [2:v]scale=1920:1080:flags=bilinear,setsar=1[s2];\
             [s0][s1]xfade=transition=fade:duration=0.500:offset=4.500[v1];\
             [v1][s2]xfade=transition=fadeblack:duration=0.800:offset=8.700[v];"
        );
        assert_eq!(v, "[v]");
        // Captions are burned after the scale; a lone input is scaled too.
        let (g, map) = film_graph(
            &tr,
            &[0.5, 0.8],
            &[4.5, 8.7],
            false,
            Some("SUB"),
            "BED",
            "NORM",
            scale,
        );
        assert!(g.contains("[v]SUB[vsub];"), "{g}");
        assert_eq!(map, "[vsub]");
        let (g, map) = film_graph(&[None], &[], &[], false, None, "BED", "NORM", scale);
        assert!(
            g.starts_with("[0:v]scale=1920:1080:flags=bilinear,setsar=1[s0];BED;"),
            "{g}"
        );
        assert_eq!(map, "[s0]");
    }

    #[test]
    fn a_draft_recipe_swaps_the_encodes_and_the_frame_only() {
        let caps = Capabilities {
            libx264: true,
            libx265: true,
            libx265_10bit: true,
            ..Default::default()
        };
        let at = ffmpeg::arg_after;
        let res = Resolution::Uhd4k;
        let keep = Recipe::Keepsake;
        assert_eq!(
            keep.film_encode_args(&caps, res),
            ffmpeg::film_encode_args(&caps, res)
        );
        assert_eq!(
            keep.chunk_encode_args(),
            ffmpeg::x264_aac("veryfast", CHUNK_CRF)
        );
        assert_eq!(keep.dims(res), (3840, 2160));
        let draft = Recipe::Draft(ffmpeg::DraftEncoder::HevcVideotoolbox);
        let film = draft.film_encode_args(&caps, res);
        assert_eq!(at(&film, "-c:v"), "hevc_videotoolbox");
        assert_eq!(at(&film, "-b:v"), "10000k");
        assert_eq!(at(&draft.chunk_encode_args(), "-b:v"), "20000k");
        assert_eq!(draft.dims(res), (1920, 1080));
        // No hardware encoder: libx264 at its fastest, still 1080p.
        let soft = Recipe::Draft(ffmpeg::DraftEncoder::X264Ultrafast);
        assert_eq!(
            at(&soft.film_encode_args(&caps, res), "-preset"),
            "ultrafast"
        );
        assert_eq!(soft.dims(res), (1920, 1080));
    }

    #[test]
    fn place_sections_follow_each_chapter_through_the_film() {
        let hero: Vec<serde_json::Value> = ["a1", "a2", "b1", "a3", "a4"]
            .iter()
            .map(|id| {
                serde_json::json!({
                    "asset_id": id, "content_hash": null, "size_bytes": 0, "mtime": 0,
                    "rendition": "still"
                })
            })
            .collect();
        let mut plan: Plan = serde_json::from_value(serde_json::json!({
            "plan_version": 1, "schema_version": 1, "reel_selects": [], "chapters": [],
            "captions": {}, "map_polyline": [], "music": {"track": "", "rationale": ""},
            "hero": hero,
        }))
        .unwrap();
        let chapter = |ids: &[&str], place: &str| Chapter {
            asset_ids: ids.iter().map(|s| s.to_string()).collect(),
            place: Some(place.into()),
            ..Default::default()
        };
        plan.chapters = vec![
            chapter(&["a1", "a2", "a3", "a4"], "Garden"),
            chapter(&["b1"], "Pier"),
        ];
        plan.chapters[0].film_place_changes = vec![("a2".into(), "Market".into())];
        let idx = plan::chapter_index(&plan);
        let sec = place_sections(&plan, |id| idx.get(id).copied().unwrap_or(usize::MAX));
        // a3 resumes chapter A after B where it had got to: the market.
        assert_eq!(["a1", "a2", "a3", "a4"].map(|id| sec[id]), [0, 1, 1, 1]);
        assert_eq!(section_place(&plan.chapters[0], sec["a3"]), Some("Market"));
        assert_eq!(section_place(&plan.chapters[0], 0), Some("Garden"));
    }

    #[test]
    fn a_capped_overlay_keeps_the_streaming_cap() {
        let a = capped_overlay_args(Path::new("/r.mp4"), Path::new("/p.png"), "[v]", 20, 15_000);
        let at = |k: &str| a[a.iter().position(|x| x == k).unwrap() + 1].as_str();
        assert_eq!((at("-maxrate"), at("-bufsize")), ("15000k", "15000k"));
        assert_eq!(
            a.last().map(String::as_str),
            Some("/r.mp4"),
            "output stays last"
        );
    }

    #[test]
    fn film_graph_chains_cuts_into_one_graph() {
        let tr = [None, Some(Transition::Fade), Some(Transition::FadeBlack)];
        let (g, map) = film_graph(
            &tr,
            &[0.5, 0.8],
            &[4.5, 8.7],
            false,
            None,
            "BED",
            "NORM",
            None,
        );
        assert!(
            g.starts_with("[0:v][1:v]xfade=transition=fade:duration=0.500:offset=4.500[v1];"),
            "{g}"
        );
        assert!(
            g.contains("[v1][2:v]xfade=transition=fadeblack:duration=0.800:offset=8.700[v];"),
            "{g}"
        );
        // Muted film audio: no acrossfade, the bed is the soundtrack.
        assert!(!g.contains("acrossfade"), "{g}");
        assert!(g.ends_with("BED;[mbed]NORM[aout]"), "{g}");
        assert_eq!(map, "[v]");
    }

    #[test]
    fn film_graph_keeps_ambience_and_burns_captions() {
        let tr = [None, Some(Transition::Fade)];
        let (g, map) = film_graph(
            &tr,
            &[0.5],
            &[4.5],
            true,
            Some("subtitles=x.srt"),
            "BED",
            "NORM",
            None,
        );
        assert!(
            g.contains("[0:a][1:a]acrossfade=d=0.500:c1=tri:c2=tri[a];"),
            "{g}"
        );
        assert!(g.contains("[v]subtitles=x.srt[vsub];"), "{g}");
        assert!(g.contains("[a]volume=0.12"), "{g}");
        assert!(g.ends_with("NORM[aout]"), "{g}");
        assert_eq!(map, "[vsub]");
    }

    #[test]
    fn film_graph_single_segment_passes_input_through() {
        let (g, map) = film_graph(&[None], &[], &[], true, None, "BED", "NORM", None);
        assert!(!g.contains("xfade"), "{g}");
        assert!(g.starts_with("[0:a]volume=0.12"), "{g}");
        assert_eq!(map, "0:v");
        let (g, map) = film_graph(&[None], &[], &[], false, Some("SUB"), "BED", "NORM", None);
        assert!(g.starts_with("[0:v]SUB[vsub];BED;"), "{g}");
        assert_eq!(map, "[vsub]");
    }

    #[test]
    fn hold_graph_shows_full_photo_with_gentle_zoom() {
        let caps = Capabilities {
            gblur: true,
            perspective: true,
            ..Default::default()
        };
        for idx in 0..4 {
            let g = hold_graph(&caps, idx, 120, Resolution::Hd1080, None);
            // Full photo over blurred fill, motion on the composite only,
            // never past 1.05x (window floor is 20/21 of the frame).
            assert!(g.contains("[bg][fg]overlay"), "{g}");
            assert!(
                g.find("overlay").unwrap() < g.find("perspective=").unwrap(),
                "{g}"
            );
            assert!(!g.contains("0.12"), "zoom capped at 1.05: {g}");
            assert!(!g.contains("scale=2400"), "no raw-photo zoom: {g}");
        }
    }

    #[test]
    fn hold_graph_glides_without_zoompan() {
        // The composite must be built once and held, then glided at
        // sub-pixel steps and downscaled with lanczos.
        let caps = Capabilities {
            gblur: true,
            perspective: true,
            ..Default::default()
        };
        for idx in 0..4 {
            let g = hold_graph(&caps, idx, 120, Resolution::Hd1080, None);
            assert!(!g.contains("zoompan"), "no zoompan jitter: {g}");
            assert!(
                g.find("overlay").unwrap() < g.find("loop=loop=119").unwrap(),
                "composite held once: {g}"
            );
            assert!(g.contains("perspective="), "animated window: {g}");
            assert!(g.contains("lanczos"), "lanczos downscale: {g}");
            assert!(g.contains("2880"), "supersampled composite: {g}");
        }
    }

    /// Cells tile the canvas exactly, never overlap, use each member once.
    fn assert_tiles(cells: &[CollageCell], n: usize) {
        assert_eq!(cells.len(), n);
        let area: u64 = cells.iter().map(|c| u64::from(c.w) * u64::from(c.h)).sum();
        assert_eq!(area, 2880 * 1620, "{cells:?}");
        let mut members: Vec<usize> = cells.iter().map(|c| c.member).collect();
        members.sort_unstable();
        assert_eq!(members, (0..n).collect::<Vec<_>>());
        for (i, a) in cells.iter().enumerate() {
            assert!(a.x + a.w <= 2880 && a.y + a.h <= 1620, "{a:?}");
            for b in &cells[i + 1..] {
                let apart =
                    a.x + a.w <= b.x || b.x + b.w <= a.x || a.y + a.h <= b.y || b.y + b.h <= a.y;
                assert!(apart, "overlap {a:?} {b:?}");
            }
        }
    }

    /// The plan's film-length estimate must track what the renderer builds:
    /// replay the renderer's own grouping, card durations and transition
    /// assignment over synthetic films and compare totals.
    #[test]
    fn plan_estimate_tracks_renderer_timeline() {
        use triptape_core::plan::{estimate_hero_secs, HeroCards};

        let opts = VideoOptions::default();
        // Deterministic pseudo-random films: (chapter, rendition) per entry.
        let mut seed = 12_345u64;
        let mut next = move |n: u64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 33) % n
        };
        for film in 0..12 {
            let chapters = 3 + film * 2;
            let mut entries: Vec<PlanEntry> = vec![];
            // Some stills carry a caption (some only whitespace, which
            // does not count): captioned ones must play solo.
            let mut captions: HashMap<String, String> = HashMap::new();
            for ch in 0..chapters {
                for k in 0..(1 + next(6) as usize) {
                    let still = next(5) < 3;
                    let id = format!("c{ch:02}e{k}");
                    let secs = 1.0 + next(8) as f64;
                    let caption = match next(5) {
                        0 => Some("Mia on the ferry"),
                        1 => Some("  "),
                        _ => None,
                    };
                    if let Some(c) = caption {
                        captions.insert(id.clone(), c.into());
                    }
                    entries.push(PlanEntry {
                        asset_id: id,
                        window: 0,
                        content_hash: None,
                        size_bytes: 0,
                        mtime: 0,
                        rendition: if still {
                            Rendition::Still
                        } else {
                            Rendition::Clip {
                                trim_start: 0.0,
                                trim_end: secs,
                            }
                        },
                    });
                }
            }
            let chapter_of = |id: &str| id[..3].to_string();
            // Shots 9 minutes apart, so the collage time window splits
            // some runs; every tenth undated.
            let taken: HashMap<&str, i64> = entries
                .iter()
                .enumerate()
                .filter(|(i, _)| i % 10 != 9)
                .map(|(i, e)| (e.asset_id.as_str(), i as i64 * 540))
                .collect();
            let taken_of = |id: &str| taken.get(id).copied();
            // Each card set with and without the opening map.
            for cards in [false, true].into_iter().flat_map(|opening_map| {
                [
                    HeroCards {
                        route: true,
                        opening_map,
                        route_stops: 0,
                        route_secs: None,
                        places: 1,
                    },
                    HeroCards {
                        route: false,
                        opening_map,
                        route_stops: 0,
                        route_secs: None,
                        places: 0,
                    },
                    HeroCards {
                        route: true,
                        opening_map,
                        route_stops: 0,
                        route_secs: None,
                        places: 0,
                    },
                    HeroCards {
                        route: true,
                        opening_map,
                        route_stops: 6,
                        route_secs: None,
                        places: 1,
                    },
                ]
            }) {
                // -- renderer replay: the renderer's own grouping.
                let resolved: Vec<ResolvedEntry> = entries
                    .iter()
                    .map(|e| ResolvedEntry {
                        entry: e.clone(),
                        caption: captions.get(&e.asset_id).cloned(),
                        taken_at: taken_of(&e.asset_id),
                        ..card_entry(&e.asset_id, Path::new("x.jpg"))
                    })
                    .collect();
                let refs: Vec<&ResolvedEntry> = resolved.iter().collect();
                let groups: Vec<Vec<&PlanEntry>> = hero_groups(
                    &refs,
                    |r| chapter_of(&r.entry.asset_id),
                    &CollageEdits::default(),
                )
                .into_iter()
                .map(|g| g.into_iter().map(|r| &r.entry).collect())
                .collect();
                // (asset id, duration, kind) as in the renderer's `segs`.
                let mut segs: Vec<(String, f64, &str)> =
                    vec![("title".into(), opts.title_secs, "title")];
                if cards.route && cards.opening_map {
                    segs.push(("map".into(), opts.map_secs, "map"));
                }
                for g in &groups {
                    let e = g[0];
                    let (dur, kind) = match &e.rendition {
                        Rendition::Still if g.len() > 1 => {
                            (collage_hold_secs(g.len(), opts.still_secs), "still")
                        }
                        Rendition::Still => (opts.still_secs, "still"),
                        Rendition::Motion {
                            trim_start,
                            trim_end,
                        }
                        | Rendition::Clip {
                            trim_start,
                            trim_end,
                        } => (
                            (trim_end - trim_start)
                                .clamp(0.5, MAX_CLIP_SECS)
                                .max(MIN_SEGMENT_SECS),
                            "clip",
                        ),
                    };
                    // A caption holds its (solo) shot until it can be read.
                    let dur = dur.max(caption_hold_secs(
                        captions.get(&e.asset_id).map(String::as_str),
                    ));
                    segs.push((e.asset_id.clone(), dur, kind));
                }
                if cards.route {
                    segs.push((
                        "closing".into(),
                        closing_map_secs(cards.route_stops, opts.map_secs),
                        "still",
                    ));
                }
                if cards.places > 0 {
                    segs.push(("credits".into(), places_card_secs(cards.places), "still"));
                }
                let mut chapter_break = vec![false; segs.len()];
                for i in 1..segs.len() {
                    let (a, b) = (&segs[i - 1].0, &segs[i].0);
                    let cut_card = |s: &str| is_card(s) && s != "title";
                    chapter_break[i] = cut_card(a)
                        || cut_card(b)
                        || (a != "title" && chapter_of(a) != chapter_of(b));
                }
                let kinds: Vec<SegKind> = segs
                    .iter()
                    .map(|s| match s.2 {
                        "title" => SegKind::Title,
                        "map" => SegKind::Map,
                        "clip" => SegKind::Clip,
                        _ => SegKind::Still,
                    })
                    .collect();
                let transitions = assign_transitions(segs.len(), &chapter_break, &kinds);
                let overlaps: f64 = transitions
                    .iter()
                    .skip(1)
                    .map(|t| transition_overlap(&t.unwrap_or(Transition::Fade)))
                    .sum();
                let actual = segs.iter().map(|s| s.1).sum::<f64>() - overlaps;

                let est = estimate_hero_secs(
                    &entries,
                    |id| CHAPTER_LABELS[id[1..3].parse::<usize>().unwrap()],
                    |_| None,
                    |id| caption_hold_secs(captions.get(id).map(String::as_str)),
                    taken_of,
                    cards,
                );
                let err = (est - actual).abs() / actual;
                assert!(
                    err < 0.015,
                    "film {film} {cards:?}: estimate {est:.1} vs renderer {actual:.1} ({:.2}%)",
                    err * 100.0
                );
            }
        }
    }

    /// Static chapter names for the estimator test (`c00`..`c40`).
    const CHAPTER_LABELS: [&str; 41] = [
        "c00", "c01", "c02", "c03", "c04", "c05", "c06", "c07", "c08", "c09", "c10", "c11", "c12",
        "c13", "c14", "c15", "c16", "c17", "c18", "c19", "c20", "c21", "c22", "c23", "c24", "c25",
        "c26", "c27", "c28", "c29", "c30", "c31", "c32", "c33", "c34", "c35", "c36", "c37", "c38",
        "c39", "c40",
    ];

    #[test]
    fn collage_layouts_tile_the_canvas() {
        const L: f64 = 1.5;
        const P: f64 = 0.75;
        for aspects in [
            vec![L, L],
            vec![P, P],
            vec![P, L],
            vec![L, L, L],
            vec![P, P, P],
            vec![L, P, L],
            vec![P, L, P],
            vec![L, L, L, L],
            vec![P, P, P, L],
            vec![P, P, L, L],
            vec![P, P, P, P],
        ] {
            assert_tiles(&collage_layout(&aspects, &[], 2880, 1620), aspects.len());
        }
    }

    #[test]
    fn two_portraits_sit_side_by_side_at_full_height() {
        let cells = collage_layout(&[0.75, 0.75], &[], 2880, 1620);
        assert!(cells.iter().all(|c| c.h == 1620 && c.y == 0));
        assert_eq!((cells[0].w, cells[1].w), (1440, 1440));
        // A portrait + landscape pair: full-height columns, the landscape
        // wider, each cell near its photo's shape.
        let mixed = collage_layout(&[1.5, 0.75], &[], 2880, 1620);
        assert!(mixed.iter().all(|c| c.h == 1620));
        assert!(mixed[0].w > mixed[1].w);
        let pa = f64::from(mixed[1].w) / f64::from(mixed[1].h);
        assert!(crop_loss(0.75, pa) < 0.1, "portrait cell aspect {pa}");
    }

    #[test]
    fn portraits_get_tall_columns_never_stacked_cells() {
        const L: f64 = 1.5;
        const P: f64 = 0.75;
        // The reviewed case: one landscape + two portrait selfies. Every
        // portrait owns a full-height column that crops it by <= the cap.
        for aspects in [[L, P, P], [P, L, P], [P, P, L]] {
            let cells = collage_layout(&aspects, &[], 3840, 2160);
            for c in &cells {
                if aspects[c.member] < PORTRAIT_BELOW {
                    assert_eq!(c.h, 2160, "portrait in a short cell: {c:?}");
                    let ca = f64::from(c.w) / f64::from(c.h);
                    assert!(crop_loss(P, ca) <= MAX_CELL_CROP, "{c:?} ca={ca}");
                }
            }
        }
        // Mixed fours keep portraits tall too, with landscapes stacked.
        let mixed = [L, P, P, L];
        let cells = collage_layout(&mixed, &[], 3840, 2160);
        assert!(cells
            .iter()
            .filter(|c| mixed[c.member] == P)
            .all(|c| c.h == 2160));
        assert!(cells.iter().any(|c| c.h == 1080));
        // Three portraits: three equal columns.
        let cols = collage_layout(&[0.7, 0.75, 0.8], &[], 2880, 1620);
        assert!(cols.iter().all(|c| c.h == 1620 && c.w == 960));
    }

    #[test]
    fn degenerate_collages_fill_the_canvas() {
        assert!(collage_layout(&[], &[], 2880, 1620).is_empty());
        let one = collage_layout(&[0.75], &[], 2880, 1620);
        assert_eq!(
            one,
            vec![CollageCell {
                member: 0,
                x: 0,
                y: 0,
                w: 2880,
                h: 1620
            }]
        );
    }

    #[test]
    fn landscapes_stack_beside_a_lone_portrait_or_share_a_big_cell() {
        // Lone portrait: tall column; landscapes stack opposite it.
        let cells = collage_layout(&[1.5, 0.75, 1.5], &[], 2880, 1620);
        let tall = cells.iter().find(|c| c.member == 1).unwrap();
        assert_eq!(tall.h, 1620);
        assert!(cells.iter().filter(|c| c.member != 1).all(|c| c.h == 810));
        // All landscape: one big cell beside two stacked.
        let cells = collage_layout(&[1.5, 1.5, 1.5], &[], 2880, 1620);
        assert_eq!(cells.iter().filter(|c| c.h == 1620).count(), 1);
        assert_eq!(cells.iter().filter(|c| c.h == 810).count(), 2);
    }

    #[test]
    fn four_photos_grid_unless_mostly_portrait() {
        let grid = collage_layout(&[1.5, 1.5, 1.5, 1.5], &[], 2880, 1620);
        assert!(grid.iter().all(|c| c.w == 1440 && c.h == 810));
        let cols = collage_layout(&[0.75; 4], &[], 2880, 1620);
        assert!(cols.iter().all(|c| c.h == 1620 && c.w == 720));
    }

    #[test]
    fn clip_framing_follows_displayed_aspect() {
        // 16:9 and 16:10 landscapes keep the cover crop.
        assert_eq!(clip_framing(16.0 / 9.0), ClipFraming::Cover);
        assert_eq!(clip_framing(1.6), ClipFraming::Cover);
        assert_eq!(clip_framing(2.0), ClipFraming::Cover);
        // 4:3, 3:2 Live Photos and 2.39:1 would lose too much.
        assert_eq!(clip_framing(4.0 / 3.0), ClipFraming::Contain);
        assert_eq!(clip_framing(1.5), ClipFraming::Contain);
        assert_eq!(clip_framing(2.39), ClipFraming::Contain);
        // Squares and portraits (a 3840x2160 stream rotated 90) are whole.
        assert_eq!(clip_framing(1.0), ClipFraming::Contain);
        assert_eq!(clip_framing(9.0 / 16.0), ClipFraming::Contain);
        assert_eq!(clip_framing(3.0 / 4.0), ClipFraming::Contain);
    }

    #[test]
    fn rotated_streams_report_displayed_dimensions() {
        let v: serde_json::Value = serde_json::json!({"streams": [{
            "codec_type": "video", "width": 3840, "height": 2160,
            "side_data_list": [{"side_data_type": "Display Matrix", "rotation": -90}]
        }]});
        let info = ffmpeg::parse_probe(&v, Path::new("x.mov")).unwrap();
        assert_eq!(info.display_dims(), (2160, 3840));
        let v = serde_json::json!({"streams": [{
            "codec_type": "video", "width": 1920, "height": 1080,
            "tags": {"rotate": "270"}
        }]});
        let info = ffmpeg::parse_probe(&v, Path::new("x.mov")).unwrap();
        assert_eq!(info.display_dims(), (1080, 1920));
        let v = serde_json::json!({"streams": [{
            "codec_type": "video", "width": 1920, "height": 1080
        }]});
        let info = ffmpeg::parse_probe(&v, Path::new("x.mov")).unwrap();
        assert_eq!(info.display_dims(), (1920, 1080));
    }

    #[test]
    fn contained_clip_graph_blurs_a_small_copy_and_glides_gently() {
        let caps = ffmpeg::Capabilities {
            gblur: true,
            perspective: true,
            ..Default::default()
        };
        let g = ffmpeg::video_contain_over_blur(&caps, 3840, 2160, false, None);
        assert!(
            g.contains("scale=480:270:force_original_aspect_ratio=increase"),
            "{g}"
        );
        assert!(
            g.contains("scale=3840:2160:force_original_aspect_ratio=decrease"),
            "{g}"
        );
        assert!(g.contains("overlay=(W-w)/2:(H-h)/2"), "{g}");
        let gentle = ffmpeg::glide_depth(&caps, 90, 3840, 2160, 0, ffmpeg::GLIDE_DEPTH_GENTLE);
        assert!(gentle.contains("/64"), "{gentle}");
        assert_eq!(
            ffmpeg::glide(&caps, 90, 3840, 2160, 0),
            ffmpeg::glide_depth(&caps, 90, 3840, 2160, 0, 21)
        );
    }

    #[test]
    fn cells_cover_when_close_and_contain_when_far() {
        // 2:1 landscape into a 144x162 cell would lose ~56%: shown whole,
        // so even the photo's top-left corner marker (green) survives.
        let mut land = image::RgbImage::from_pixel(400, 200, image::Rgb([200, 30, 30]));
        for y in 0..20 {
            for x in 0..20 {
                land.put_pixel(x, y, image::Rgb([30, 200, 30]));
            }
        }
        let cell = fill_cell(&land, 144, 162, false);
        assert_eq!((cell.width(), cell.height()), (144, 162));
        // Whole photo is 144x72 at y 45..117.
        assert_eq!(cell.get_pixel(2, 47), &image::Rgb([30, 200, 30]));
        assert_eq!(cell.get_pixel(100, 81), &image::Rgb([200, 30, 30]));
        // Near-matching portrait covers the cell edge to edge.
        let port = image::RgbImage::from_pixel(300, 400, image::Rgb([30, 30, 200]));
        let cell = fill_cell(&port, 144, 162, false);
        assert_eq!(cell.get_pixel(0, 0), &image::Rgb([30, 30, 200]));
        assert_eq!(cell.get_pixel(143, 161), &image::Rgb([30, 30, 200]));
    }

    #[test]
    fn panoramas_never_join_collages() {
        let pano = ResolvedEntry {
            aspect: Some(4.4),
            ..card_entry("p", Path::new("p.jpg"))
        };
        let plain = ResolvedEntry {
            aspect: Some(1.5),
            ..card_entry("s", Path::new("s.jpg"))
        };
        let unknown = card_entry("u", Path::new("u.jpg"));
        assert_eq!(collage_candidacy(&pano), plan::Candidacy::Never);
        assert_eq!(collage_candidacy(&plain), plan::Candidacy::Plain);
        assert_eq!(collage_candidacy(&unknown), plan::Candidacy::Plain);
    }

    #[test]
    fn captioned_stills_join_collages_only_when_the_user_joins_them() {
        let with = |c: Option<&str>| ResolvedEntry {
            caption: c.map(String::from),
            ..card_entry("s", Path::new("s.jpg"))
        };
        let if_joined = plan::Candidacy::IfJoined;
        assert_eq!(
            collage_candidacy(&with(Some("Mia on the ferry"))),
            if_joined
        );
        assert_eq!(collage_candidacy(&with(Some(" x "))), if_joined);
        // Blank captions are no captions.
        assert_eq!(collage_candidacy(&with(Some("  "))), plan::Candidacy::Plain);
        assert_eq!(collage_candidacy(&with(None)), plan::Candidacy::Plain);
    }

    #[test]
    fn a_joined_captioned_live_photo_groups_as_the_planner_groups_it() {
        let mut live = ResolvedEntry {
            caption: Some("Egg tarts".into()),
            ..card_entry("live", Path::new("live.heic"))
        };
        live.entry.rendition = Rendition::Motion {
            trim_start: 0.0,
            trim_end: 3.0,
        };
        live.motion_path = Some(PathBuf::from("live.mov"));
        let (b, c) = (
            card_entry("b", Path::new("b.jpg")),
            card_entry("c", Path::new("c.jpg")),
        );
        let entries = [&live, &b, &c];
        let ids = |g: Vec<Vec<&ResolvedEntry>>| -> Vec<Vec<String>> {
            g.iter()
                .map(|g| g.iter().map(|e| e.entry.asset_id.clone()).collect())
                .collect()
        };
        let group = |edits: &CollageEdits| ids(hero_groups(&entries, |_| 0, edits));
        // Alone by default (taken_at is unknown, so b and c join nothing
        // on their own).
        assert!(group(&CollageEdits::default()).iter().all(|g| g.len() == 1));
        let mut joined = CollageEdits::default();
        joined.join("live".into(), "b".into());
        joined.join("b".into(), "c".into());
        assert_eq!(group(&joined), [["live", "b", "c"]]);
        // The collage's caption is the member's.
        assert_eq!(collage_caption(&entries).as_deref(), Some("Egg tarts"),);
        // Clips never join, even when linked.
        let mut clip = card_entry("clip", Path::new("clip.mov"));
        clip.entry.rendition = Rendition::Clip {
            trim_start: 0.0,
            trim_end: 3.0,
        };
        let mut with_clip = CollageEdits::default();
        with_clip.join("clip".into(), "b".into());
        let pair = [&clip, &b];
        assert!(hero_groups(&pair, |_| 0, &with_clip)
            .iter()
            .all(|g| g.len() == 1));
    }

    #[test]
    fn hold_graph_derives_every_size_from_the_resolution() {
        let caps = Capabilities {
            gblur: true,
            perspective: true,
            ..Default::default()
        };
        let hd = hold_graph(&caps, 0, 120, Resolution::Hd1080, None);
        assert!(
            hd.contains("scale=2880:1620") && hd.contains("scale=1920:1080"),
            "{hd}"
        );
        assert!(hd.contains("gblur=sigma=40"), "{hd}");
        let uhd = hold_graph(&caps, 0, 120, Resolution::Uhd4k, None);
        assert!(
            uhd.contains("scale=5760:3240") && uhd.contains("scale=3840:2160"),
            "{uhd}"
        );
        assert!(uhd.contains("gblur=sigma=80"), "{uhd}");
        for leaked in ["2880", "1620", "1920", "1080"] {
            assert!(
                !uhd.contains(leaked),
                "{leaked} leaked into 4K graph: {uhd}"
            );
        }
    }

    #[test]
    fn lower_third_overlay_scales_with_resolution() {
        let hd = lower_third_graph(3.0, Resolution::Hd1080);
        assert!(hd.contains("overlay=48:H-160"), "{hd}");
        let uhd = lower_third_graph(3.0, Resolution::Uhd4k);
        assert!(uhd.contains("overlay=96:H-320"), "{uhd}");
    }

    #[test]
    fn pano_graph_pans_with_crop_window_not_zoompan() {
        let g = ffmpeg::pano_pan(240, 1920, 1080, 4.4, 1.0, false);
        // Strip short side = 2x the frame's, window = the frame at 2x.
        assert!(g.starts_with("scale=-2:2160"), "fills 2x height: {g}");
        assert!(g.contains("crop=w=3840:h=2160"), "{g}");
        assert!(g.ends_with("scale=1920:1080:flags=lanczos,setsar=1"), "{g}");
        assert!(!g.contains("zoompan") && !g.contains("blur"), "{g}");
        assert!(!g.contains("perspective"), "no per-frame resample: {g}");
        assert!(
            g.find("loop=loop=239").unwrap() < g.find("crop=").unwrap(),
            "strip scaled and held once: {g}"
        );
        // Direction flips with `reverse`; tall images scan vertically.
        assert_ne!(g, ffmpeg::pano_pan(240, 1920, 1080, 4.4, 1.0, true));
        let tall = ffmpeg::pano_pan(240, 1920, 1080, 0.25, 1.0, false);
        assert!(tall.starts_with("scale=3840:-2"), "{tall}");
        assert!(
            tall.contains("x='0'") && tall.contains("y='floor((ih-oh)"),
            "{tall}"
        );
        // 4K: 2160 output short side -> 4320 strip, no 1080p numbers.
        let uhd = ffmpeg::pano_pan(240, 3840, 2160, 4.4, 1.0, false);
        assert!(uhd.starts_with("scale=-2:4320"), "{uhd}");
        assert!(uhd.contains("crop=w=7680:h=4320"), "{uhd}");
        assert!(
            uhd.ends_with("scale=3840:2160:flags=lanczos,setsar=1"),
            "{uhd}"
        );
        for leaked in ["1920", "1080"] {
            assert!(!uhd.contains(leaked), "{leaked} leaked: {uhd}");
        }
    }

    #[test]
    fn offsets_account_for_overlap() {
        // 3 x 4 s segments, 0.6 overlap: cuts at 3.4 and 6.8; total 10.8.
        let o = xfade_offsets(&[4.0, 4.0, 4.0], 0.6);
        assert_eq!(o.len(), 2);
        assert!((o[0] - 3.4).abs() < 1e-9);
        assert!((o[1] - 6.8).abs() < 1e-9);
        assert!(xfade_offsets(&[5.0], 0.6).is_empty());
        assert!(xfade_offsets(&[], 0.6).is_empty());
    }

    #[test]
    fn transition_discipline() {
        use SegKind::*;
        // 8 segments, chapter breaks into #3 and #5.
        let breaks = vec![false, false, false, true, false, true, false, false];
        let kinds = vec![Title, Still, Still, Still, Clip, Still, Still, Still];
        let t = assign_transitions(8, &breaks, &kinds);
        assert_eq!(t[0], None);
        assert_eq!(t[1], Some(Transition::CircleOpen));
        assert_eq!(t[7], Some(Transition::CircleClose));
        // First chapter break takes the fadeblack slot.
        assert_eq!(t[3], Some(Transition::FadeBlack));
        // Kind change (still -> clip) dissolves.
        assert_eq!(t[4], Some(Transition::Dissolve));
        // Blur-dissolve signature capped at 3 and mapped to dissolve.
        let plain = assign_transitions(30, &[false; 30], &[Still; 30]);
        let sig = plain
            .iter()
            .filter(|t| **t == Some(Transition::BlurDissolve))
            .count();
        assert!(sig <= 3, "signature capped, got {sig}");
        assert_eq!(Transition::BlurDissolve.xfade_name(), "dissolve");
    }

    #[test]
    fn transition_variety_no_consecutive_equal() {
        use SegKind::*;
        // Micro-chapter soup: every cut a chapter break (the 104-chapter bug).
        let n = 40;
        let breaks = vec![true; n];
        let kinds = vec![Still; n];
        let t = assign_transitions(n, &breaks, &kinds);
        let names: Vec<&str> = t.iter().skip(1).map(|t| t.unwrap().xfade_name()).collect();
        for w in names.windows(2) {
            assert_ne!(w[0], w[1], "consecutive equal xfades");
        }
        // fadeblack stays rare: at most 1 in 3 cuts.
        let fb = names.iter().filter(|n| ***n == *"fadeblack").count();
        assert!(fb * 3 <= n, "fadeblack capped, got {fb}/{n}");
        // Mixed story: rotation still varies.
        let kinds2: Vec<SegKind> = (0..24)
            .map(|i| if i % 3 == 2 { Clip } else { Still })
            .collect();
        let t2 = assign_transitions(24, &[false; 24], &kinds2);
        let names2: Vec<&str> = t2.iter().skip(1).map(|t| t.unwrap().xfade_name()).collect();
        for w in names2.windows(2) {
            assert_ne!(w[0], w[1], "consecutive equal xfades (mixed)");
        }
        let distinct: std::collections::HashSet<&str> = names2.iter().copied().collect();
        assert!(distinct.len() >= 4, "rotation varies, got {distinct:?}");
    }

    #[test]
    fn offsets_varied_overlaps() {
        // 3 x 4 s segments, overlaps [0.8, 0.5]: cuts at 3.2 and 6.7.
        let o = xfade_offsets_var(&[4.0, 4.0, 4.0], &[0.8, 0.5]);
        assert_eq!(o.len(), 2);
        assert!((o[0] - 3.2).abs() < 1e-9, "got {}", o[0]);
        assert!((o[1] - 6.7).abs() < 1e-9, "got {}", o[1]);
        // Uniform helper agrees with the legacy math.
        let u = xfade_offsets(&[4.0, 4.0, 4.0], 0.6);
        assert!((u[0] - 3.4).abs() < 1e-9);
        assert!((u[1] - 6.8).abs() < 1e-9);
        // Per-transition overlaps per spec.
        assert_eq!(transition_overlap(&Transition::FadeBlack), 0.8);
        assert_eq!(transition_overlap(&Transition::CircleOpen), 0.7);
        assert_eq!(transition_overlap(&Transition::CircleClose), 0.7);
        assert_eq!(transition_overlap(&Transition::Fade), 0.5);
        assert_eq!(transition_overlap(&Transition::BlurDissolve), 0.5);
    }

    #[test]
    fn legacy_chapter_falls_back_to_title() {
        use crate::titlecard::{chapter_book_title, chapter_lines};
        let legacy: triptape_core::types::Chapter =
            serde_json::from_str(r#"{"title":"Day 2 · Coast","asset_ids":["a1"]}"#).unwrap();
        assert_eq!(
            chapter_lines(&legacy),
            Some(("Day 2 · Coast".to_string(), None))
        );
        assert_eq!(chapter_book_title(&legacy), "Day 2 · Coast");
        let mut current = legacy.clone();
        current.day_index = Some(2);
        current.place = Some("Kyoto, Japan".into());
        assert_eq!(
            chapter_lines(&current),
            Some(("Day 2".to_string(), Some("Kyoto, Japan".to_string())))
        );
        current.date_label = Some("Wed, Oct 8".into());
        assert_eq!(
            chapter_book_title(&current),
            "Day 2 · Wed, Oct 8 \u{2014} Kyoto, Japan"
        );
        let mut blank = legacy;
        blank.title = "  ".into();
        assert_eq!(chapter_lines(&blank), None);
    }

    #[test]
    fn the_film_heading_names_the_film_place_and_the_book_keeps_the_chapter_place() {
        use crate::titlecard::{chapter_book_title, chapter_film_lines};
        let mut ch = triptape_core::types::Chapter {
            title: "Osaka Castle".into(),
            place: Some("Osaka Castle".into()),
            day_index: Some(3),
            ..Default::default()
        };
        // No film place: the film names the chapter's own place.
        assert_eq!(
            chapter_film_lines(&ch),
            Some(("Day 3".to_string(), Some("Osaka Castle".to_string())))
        );
        ch.film_place = Some("Dotonbori".into());
        assert_eq!(
            chapter_film_lines(&ch),
            Some(("Day 3".to_string(), Some("Dotonbori".to_string())))
        );
        assert_eq!(chapter_book_title(&ch), "Day 3 \u{2014} Osaka Castle");
        // Legacy chapters with nothing but a title still fall back to it.
        let legacy = triptape_core::types::Chapter {
            title: "Coast".into(),
            ..Default::default()
        };
        assert_eq!(
            chapter_film_lines(&legacy),
            Some(("Coast".to_string(), None))
        );
    }

    #[test]
    fn chapter_overlay_and_date_range() {
        use crate::titlecard::{chapter_overlay, format_date_range};
        // 2026-10-06 12:00 UTC.
        let day = 1791288000i64;
        // Oct 4 22:50 UTC is already Oct 5 in Japan (+09:00).
        assert_eq!(
            format_date_range(&[(1791154200, Some(9 * 3600))]).as_deref(),
            Some("Oct 5, 2026")
        );
        assert_eq!(
            format_date_range(&[(day, None), (day + 3 * 86400, None)]).as_deref(),
            Some("Oct 6, 2026 – Oct 9, 2026")
        );
        assert_eq!(
            format_date_range(&[(day, None)]).as_deref(),
            Some("Oct 6, 2026")
        );
        assert_eq!(format_date_range(&[]), None);
        // Date-line crossing: the earlier instant (Oct 6 12:00 UTC at
        // +13:00 → Oct 7) has a later local date than the next one
        // (Oct 6 13:00 UTC at -10:00 → Oct 6 03:00).
        assert_eq!(
            format_date_range(&[(day, Some(13 * 3600)), (day + 3600, Some(-10 * 3600))]).as_deref(),
            Some("Oct 6, 2026 – Oct 7, 2026")
        );
        // Chapter overlay: day once, place underneath.
        assert_eq!(
            chapter_overlay(Some(3), Some("Wed, Oct 8"), Some("Kyoto, Japan")),
            Some((
                "Day 3 · Wed, Oct 8".to_string(),
                Some("Kyoto, Japan".to_string())
            ))
        );
        assert_eq!(
            chapter_overlay(Some(3), Some("Wed, Oct 8"), None),
            Some(("Day 3 · Wed, Oct 8".to_string(), None))
        );
        assert_eq!(
            chapter_overlay(None, None, Some("Kyoto, Japan")),
            Some(("Kyoto, Japan".to_string(), None))
        );
        assert_eq!(chapter_overlay(None, None, None), None);
    }

    fn clip_entry(id: &str, src: PathBuf, trim_end: f64) -> ResolvedEntry {
        ResolvedEntry {
            entry: PlanEntry {
                asset_id: id.into(),
                window: 0,
                content_hash: None,
                size_bytes: 0,
                mtime: 0,
                rendition: Rendition::Clip {
                    trim_start: 0.0,
                    trim_end,
                },
            },
            still_path: src.clone(),
            motion_path: Some(src),
            caption: None,
            taken_at: None,
            tz_offset_secs: None,
            aspect: None,
            gps: None,
            frame: None,
            still: false,
        }
    }

    /// A 160x90 segment of `secs` seconds showing the lavfi `source`.
    fn tiny_segment(caps: &Capabilities, p: &Path, source: &str, secs: f64) -> PathBuf {
        let st = std::process::Command::new(&caps.ffmpeg_bin)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg(format!("{source}=s=160x90:r=30:d={secs}"))
            .args(["-f", "lavfi", "-i"])
            .arg(format!("anullsrc=r=48000:cl=stereo:d={secs}"))
            .args([
                "-shortest",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
            ])
            .arg(p)
            .status()
            .unwrap();
        assert!(st.success());
        p.to_path_buf()
    }

    /// The chunk plan of a film as the renderer makes it.
    struct FilmChunks {
        ranges: Vec<Range<usize>>,
        /// The transition into every segment.
        cuts: Vec<Option<Transition>>,
        /// Each joined chunk's cache key (`None` for a lone segment, which
        /// is not joined).
        keys: Vec<Option<String>>,
    }

    fn film_chunk_keys(
        cache: &SegmentCache,
        members: &[String],
        breaks: &[bool],
        recipe: Recipe,
        keep_ambience: bool,
    ) -> FilmChunks {
        let n = members.len();
        let kinds = vec![SegKind::Still; n];
        let durations: Vec<f64> = (0..n).map(|_| 4.0).collect();
        let ranges = assembly_chunks(breaks, ASSEMBLY_CHUNK);
        let transitions = assign_transitions_by_chunk(n, breaks, &kinds, &ranges);
        let overlaps: Vec<f64> = transitions[1..]
            .iter()
            .map(|t| transition_overlap(&t.unwrap()))
            .collect();
        let keys = ranges
            .iter()
            .map(|r| {
                (r.len() > 1).then(|| {
                    chunk_key(
                        cache,
                        &members[r.clone()],
                        &durations[r.clone()],
                        &transitions[r.clone()],
                        &overlaps[r.start..r.end - 1],
                        keep_ambience,
                        recipe,
                    )
                })
            })
            .collect();
        FilmChunks {
            ranges,
            cuts: transitions,
            keys,
        }
    }

    #[test]
    fn chunk_boundaries_follow_chapters() {
        // 100 segments in chapters of six: a chunk ends at the first
        // chapter start once it holds eight segments, so every chunk is two
        // whole chapters and starts where a chapter does.
        let n = 100;
        let breaks: Vec<bool> = (0..n).map(|i| i > 0 && i % 6 == 0).collect();
        let ranges = assembly_chunks(&breaks, ASSEMBLY_CHUNK);
        assert_eq!(ranges[0], 0..12);
        assert_eq!(ranges[1], 12..24);
        assert_eq!(ranges.last().unwrap(), &(96..100));
        for w in ranges.windows(2) {
            assert_eq!(w[0].end, w[1].start, "chunks tile the film");
            assert!(breaks[w[1].start], "{:?} starts mid-chapter", w[1]);
        }
        assert!(ranges.iter().all(|r| r.len() <= ASSEMBLY_CHUNK));
        assert!(
            ranges.len() <= ASSEMBLY_CHUNK,
            "the final graph stays small"
        );
        // A chapter longer than a chunk is cut at the cap, then the story
        // takes over again at the next chapter start.
        let mut long = vec![false; 130];
        long[100] = true;
        long[115] = true;
        assert_eq!(
            assembly_chunks(&long, ASSEMBLY_CHUNK),
            [0..40, 40..80, 80..100, 100..115, 115..130]
        );
        // A chapter start too soon after the last cut is not a boundary.
        let mut soon = vec![false; 60];
        (soon[3], soon[20]) = (true, true);
        assert_eq!(assembly_chunks(&soon, ASSEMBLY_CHUNK), [0..20, 20..60]);
        // A film that fits one graph is one run, whatever its chapters.
        let short: Vec<bool> = (0..ASSEMBLY_CHUNK).map(|i| i % 5 == 0).collect();
        let one = assembly_chunks(&short, ASSEMBLY_CHUNK);
        assert_eq!((one.len(), one[0].clone()), (1, 0..ASSEMBLY_CHUNK));
        let none = assembly_chunks(&[], ASSEMBLY_CHUNK);
        assert_eq!((none.len(), none[0].clone()), (1, 0..0));
        // A very long film raises the minimum, so its final graph never
        // joins more chunks than a chunk has segments.
        let soup = vec![true; 1200];
        assert!(assembly_chunks(&soup, ASSEMBLY_CHUNK).len() <= ASSEMBLY_CHUNK + 1);
        // One more shot in a long film of short chapters leaves the other
        // chunks' boundaries where they were (within a length band).
        let eights = |n: usize| (0..n).map(|i| i > 0 && i % 8 == 0).collect::<Vec<_>>();
        for n in [216, 300, 330] {
            let before = assembly_chunks(&eights(n), ASSEMBLY_CHUNK);
            let mut grown = eights(n);
            grown.insert(100, false);
            let after = assembly_chunks(&grown, ASSEMBLY_CHUNK);
            let unchanged = before
                .iter()
                .filter(|r| r.end <= 96)
                .all(|r| after.contains(r));
            assert!(unchanged, "n={n}: {before:?} -> {after:?}");
        }
    }

    /// Transitions start over at each chunk: the cuts of a chunk are the
    /// same wherever the chunk sits in the film.
    #[test]
    fn transitions_restart_at_each_chunk() {
        use SegKind::*;
        let n = 100;
        let breaks: Vec<bool> = (0..n).map(|i| i > 0 && i % 10 == 0).collect();
        let kinds = vec![Still; n];
        let ranges = assembly_chunks(&breaks, ASSEMBLY_CHUNK);
        assert_eq!(ranges.len(), 10);
        let t = assign_transitions_by_chunk(n, &breaks, &kinds, &ranges);
        assert_eq!(t[0], None);
        assert_eq!(t[1], Some(Transition::CircleOpen));
        assert_eq!(t[n - 1], Some(Transition::CircleClose));
        // Every middle chunk is cut alike, opening on the chapter's fade.
        for r in &ranges[1..9] {
            assert_eq!(t[r.clone()], t[ranges[1].clone()], "{r:?}");
            assert_eq!(t[r.start], Some(Transition::FadeBlack));
            let sig = t[r.clone()]
                .iter()
                .filter(|x| **x == Some(Transition::BlurDissolve));
            assert_eq!(sig.count(), 1, "one signature dissolve a chunk");
            let names: Vec<&str> = t[r.clone()]
                .iter()
                .map(|x| x.unwrap().xfade_name())
                .collect();
            assert!(names.windows(2).all(|w| w[0] != w[1]), "{names:?}");
        }
        // One run is the old film-wide assignment.
        let whole = assembly_chunks(&breaks[..30], ASSEMBLY_CHUNK);
        assert_eq!(
            assign_transitions_by_chunk(30, &breaks[..30], &kinds[..30], &whole),
            assign_transitions(30, &breaks[..30], &kinds[..30])
        );
    }

    /// Removing a shot re-joins its own chunk only: every other chunk
    /// keeps its segments, its cuts and so its cache key, and the cuts
    /// between the chunks stay as they were.
    #[test]
    fn removing_a_shot_changes_only_its_own_chunk_key() {
        use crate::segment_cache::SegmentCacheConfig;
        let dir = tempfile::tempdir().unwrap();
        let cfg = SegmentCacheConfig::roomy(dir.path().join("chunks"));
        let cache = SegmentCache::open(&cfg, "ffmpeg test".into(), dir.path()).unwrap();
        let n = 120;
        let members: Vec<String> = (0..n).map(|i| format!("key:{i:03}")).collect();
        let breaks: Vec<bool> = (0..n).map(|i| i > 0 && i % 10 == 0).collect();
        let keep = Recipe::Keepsake;
        let FilmChunks { ranges, cuts, keys } =
            film_chunk_keys(&cache, &members, &breaks, keep, false);
        assert_eq!(ranges.len(), 12);
        assert!(keys.iter().all(Option::is_some));
        let distinct: std::collections::HashSet<_> = keys.iter().collect();
        assert_eq!(distinct.len(), 12, "every chunk has its own key");
        // Stable from run to run.
        assert_eq!(
            film_chunk_keys(&cache, &members, &breaks, keep, false).keys,
            keys
        );

        // Drop shot 25, in the third chunk (20..30).
        let without = |v: &[String]| [&v[..25], &v[26..]].concat();
        let breaks2 = [&breaks[..25], &breaks[26..]].concat();
        let FilmChunks {
            ranges: ranges2,
            cuts: cuts2,
            keys: keys2,
        } = film_chunk_keys(&cache, &without(&members), &breaks2, keep, false);
        assert_eq!(ranges2.len(), 12);
        assert_eq!(ranges2[2], 20..29);
        for k in 0..12 {
            if k == 2 {
                assert_ne!(keys2[k], keys[k], "the edited chunk is joined again");
            } else {
                assert_eq!(keys2[k], keys[k], "chunk {k} is reused");
            }
            // The final graph's cut into each chunk is unchanged too.
            assert_eq!(
                cuts2[ranges2[k].start], cuts[ranges[k].start],
                "cut into {k}"
            );
        }
        // Counting from the film's start, as before, the same removal
        // moved every later chunk's segments and changed later cuts.
        let kinds = vec![SegKind::Still; n];
        let old = assign_transitions(n, &breaks, &kinds);
        let old2 = assign_transitions(n - 1, &breaks2, &kinds[1..]);
        assert_ne!(old[41..60], old2[40..59]);

        // Replacing a shot's picture, or its heading (the overlay key is
        // its identity), changes that chunk alone as well.
        let mut edited = members.clone();
        edited[57] = "key:new".into();
        let keys3 = film_chunk_keys(&cache, &edited, &breaks, keep, false).keys;
        let changed: Vec<usize> = (0..12).filter(|k| keys3[*k] != keys[*k]).collect();
        assert_eq!(changed, [5]);

        // A draft and the keepsake never share a chunk, nor do two draft
        // encoders, nor films with and without their own sound.
        let draft = Recipe::Draft(ffmpeg::DraftEncoder::HevcVideotoolbox);
        let soft = Recipe::Draft(ffmpeg::DraftEncoder::X264Ultrafast);
        let draft_keys = film_chunk_keys(&cache, &members, &breaks, draft, false).keys;
        let soft_keys = film_chunk_keys(&cache, &members, &breaks, soft, false).keys;
        let sound_keys = film_chunk_keys(&cache, &members, &breaks, keep, true).keys;
        for k in 0..12 {
            assert_ne!(draft_keys[k], keys[k]);
            assert_ne!(draft_keys[k], soft_keys[k]);
            assert_ne!(sound_keys[k], keys[k]);
        }
    }

    /// Real ffmpeg: a second join of the same segments takes every chunk
    /// from the chunk cache and runs no encode; a changed segment re-joins
    /// its own chunk only; a draft joins its own chunks.
    #[test]
    fn a_cached_chunk_is_not_joined_again() {
        use crate::segment_cache::SegmentCacheConfig;
        let caps = crate::ffmpeg::probe_capabilities("ffmpeg", "ffprobe");
        if !caps.libx264 || !caps.xfade {
            eprintln!("ffmpeg caps insufficient; skipping chunk cache test");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        // No two chunks alike: equal chunks would share one cache entry.
        let durations = [1.5, 1.0, 1.25, 1.0, 1.0];
        let segs: Vec<PathBuf> = (0..5)
            .map(|i| {
                tiny_segment(
                    &caps,
                    &temp.path().join(format!("s{i}.mp4")),
                    "testsrc2",
                    durations[i],
                )
            })
            .collect();
        let transitions = [
            None,
            Some(Transition::Fade),
            Some(Transition::FadeBlack),
            Some(Transition::Fade),
            Some(Transition::Fade),
        ];
        let overlaps = [0.5, 0.8, 0.5, 0.5];
        let ranges = assembly_chunks(&[false; 5], 2);
        let cfg = SegmentCacheConfig::roomy(temp.path().join("cache")).chunks();
        // Each join is its own export: a new work dir and cache handle.
        let join = |name: &str, ffmpeg_bin: &str, recipe: Recipe| {
            let work = temp.path().join(name);
            std::fs::create_dir_all(&work).unwrap();
            let cache = SegmentCache::open(&cfg, "ffmpeg test".into(), &work).unwrap();
            let caps = Capabilities {
                ffmpeg_bin: ffmpeg_bin.into(),
                chunk_cache: Some(cache.clone()),
                ..caps.clone()
            };
            let joined = prejoin_chunks(
                &caps,
                &segs,
                &durations,
                &transitions,
                &overlaps,
                false,
                &work,
                &ranges,
                recipe,
                &|_| {},
            );
            let stats = cache.stats();
            (joined, (stats.hits, stats.misses, stats.stored))
        };
        let real = caps.ffmpeg_bin.clone();
        let (first, stats) = join("a", &real, Recipe::Keepsake);
        // Two joined chunks; the lone last segment is neither joined nor cached.
        assert_eq!(stats, (0, 2, 2));
        let first = first.unwrap();
        assert_eq!(first.inputs[2], segs[4]);
        // No ffmpeg to run: the join only succeeds without encoding.
        let (second, stats) = join("b", "/nonexistent/ffmpeg", Recipe::Keepsake);
        assert_eq!(stats, (2, 0, 0));
        let second = second.expect("every chunk came from the cache");
        assert_eq!(second.durations, first.durations);
        for (a, b) in first.inputs[..2].iter().zip(&second.inputs[..2]) {
            assert_ne!(a, b, "materialized in the new work dir");
            assert_eq!(std::fs::read(a).unwrap(), std::fs::read(b).unwrap());
        }
        // A draft's chunks are its own: nothing to reuse, so it must encode.
        let draft = Recipe::Draft(ffmpeg::DraftEncoder::X264Ultrafast);
        let (failed, stats) = join("c", "/nonexistent/ffmpeg", draft);
        assert!(failed.is_err());
        assert_eq!((stats.0, stats.2), (0, 0));
        let (drafted, stats) = join("d", &real, draft);
        assert_eq!(stats, (0, 2, 2));
        let chunk = ffmpeg::ffprobe_file(&caps.ffprobe_bin, &drafted.unwrap().inputs[0]).unwrap();
        assert_eq!((chunk.width, chunk.height), (1920, 1080));
        // Another picture in the first chunk: that chunk alone is joined.
        tiny_segment(&caps, &segs[1], "smptebars", durations[1]);
        let (_, stats) = join("e", &real, Recipe::Keepsake);
        assert_eq!(stats, (1, 1, 1));
    }

    /// Real ffmpeg: five tiny segments joined two to a chunk, then the chunks
    /// joined by `film_graph`, come out at the length one graph would give.
    #[test]
    fn chunked_assembly_keeps_the_film_length() {
        let caps = crate::ffmpeg::probe_capabilities("ffmpeg", "ffprobe");
        if !caps.libx264 || !caps.xfade {
            eprintln!("ffmpeg caps insufficient; skipping chunked assembly test");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let durations = [2.0, 1.5, 2.5, 2.0, 1.0];
        let segs: Vec<PathBuf> = durations
            .iter()
            .enumerate()
            .map(|(i, d)| {
                tiny_segment(
                    &caps,
                    &temp.path().join(format!("s{i}.mp4")),
                    "testsrc2",
                    *d,
                )
            })
            .collect();
        let transitions = [
            None,
            Some(Transition::Fade),
            Some(Transition::FadeBlack),
            Some(Transition::Fade),
            Some(Transition::Fade),
        ];
        let overlaps = [0.5, 0.8, 0.5, 0.5];
        let seen = std::cell::RefCell::new(vec![]);
        let joined = prejoin_chunks(
            &caps,
            &segs,
            &durations,
            &transitions,
            &overlaps,
            false,
            temp.path(),
            &assembly_chunks(&[false; 5], 2),
            Recipe::Keepsake,
            &|f| seen.borrow_mut().push(f),
        )
        .unwrap();
        assert_eq!(joined.inputs.len(), 3);
        // Progress climbs to done across the chunks.
        let seen = seen.into_inner();
        assert!(seen.windows(2).all(|w| w[0] <= w[1] + 1e-9), "{seen:?}");
        assert!(
            (seen.last().copied().unwrap() - 1.0).abs() < 1e-9,
            "{seen:?}"
        );
        // The lone trailing segment is used as is, not re-encoded.
        assert_eq!(joined.inputs[2], segs[4]);
        let out = temp.path().join("film.mp4");
        let (graph, map) = film_graph(
            &joined.transitions,
            &joined.overlaps,
            &xfade_offsets_var(&joined.durations, &joined.overlaps),
            false,
            None,
            "anullsrc=r=48000:cl=stereo,atrim=0:20[mbed]",
            "anull",
            None,
        );
        let mut args: Vec<String> = vec!["-y".into()];
        for p in &joined.inputs {
            args.extend(["-i".into(), s(p)]);
        }
        args.extend(["-filter_complex".into(), graph, "-map".into(), map]);
        args.extend(["-map".into(), "[aout]".into(), "-shortest".into()]);
        args.extend(ffmpeg::x264_aac("veryfast", 23));
        args.push(s(&out));
        caps.run(&args, &out).unwrap();
        let got = ffmpeg::ffprobe_file(&caps.ffprobe_bin, &out)
            .unwrap()
            .duration_secs;
        let want = durations.iter().sum::<f64>() - overlaps.iter().sum::<f64>();
        assert!(
            (got - want).abs() < 0.1,
            "film {got:.3} s, one graph {want:.3} s"
        );
    }

    /// A landscape-coded stream with a 90 degree display matrix (how iPhones
    /// store portrait clips) plus a native odd-sized portrait clip both render
    /// whole over a blurred fill into a clean 1080p segment (odd fitted sizes
    /// once broke zscale's subsampling check).
    #[test]
    fn portrait_clips_render_whole_into_a_landscape_frame() {
        let caps = crate::ffmpeg::probe_capabilities("ffmpeg", "ffprobe");
        if !caps.libx264 || !caps.perspective || !caps.gblur {
            eprintln!("ffmpeg caps insufficient; skipping portrait clip test");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let st = std::process::Command::new(&caps.ffmpeg_bin)
                .args(["-v", "error", "-y"])
                .args(args)
                .status()
                .unwrap();
            assert!(st.success());
        };
        let path = |name: &str| temp.path().join(name).to_string_lossy().into_owned();
        let testsrc = |size: &str, name: &str| {
            let out = path(name);
            let src = format!("testsrc2=s={size}:r=30:d=2");
            run(&["-f", "lavfi", "-i", &src, "-pix_fmt", "yuv420p", &out]);
            out
        };
        let flat = testsrc("640x360", "flat.mp4");
        let rotated = path("rotated.mp4");
        run(&[
            "-display_rotation",
            "90",
            "-i",
            &flat,
            "-c",
            "copy",
            &rotated,
        ]);
        let native = testsrc("358x638", "native.mp4");
        for (name, src) in [("rotated", rotated), ("native", native)] {
            let src = PathBuf::from(src);
            let probe = ffmpeg::ffprobe_file(&caps.ffprobe_bin, &src).unwrap();
            let (dw, dh) = probe.display_dims();
            assert!(dh > dw, "{name}: displayed {dw}x{dh} should be portrait");
            let r = clip_entry(name, src, 1.5);
            let mut fb = vec![];
            let (out, _, kind) = render_entry_segment(
                &caps,
                &r,
                0,
                0,
                3.0,
                10.0,
                Resolution::Hd1080,
                temp.path(),
                &mut fb,
            )
            .unwrap();
            assert_eq!(kind, "clip");
            let info = ffmpeg::ffprobe_file(&caps.ffprobe_bin, &out).unwrap();
            assert_eq!((info.width, info.height), (1920, 1080), "{name}");
        }
    }

    /// Regression: iPhone clips carry a second, spatial `apple_apac` audio
    /// track that the MP4 muxer cannot take; `-map 0:a` mapped it and sank
    /// the clip (ffmpeg exit 234). ffmpeg cannot synthesize apac, so a second
    /// PCM track stands in: the segment must carry exactly one audio stream.
    #[test]
    fn clip_with_second_audio_stream_encodes_one_track() {
        let caps = crate::ffmpeg::probe_capabilities("ffmpeg", "ffprobe");
        if !caps.libx264 || !caps.perspective {
            eprintln!("ffmpeg caps insufficient; skipping multi-audio segment test");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("multi.mkv");
        let made = std::process::Command::new(&caps.ffmpeg_bin)
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=320x180:r=30:d=3",
            ])
            .args([
                "-f",
                "lavfi",
                "-i",
                "sine=f=440:d=3",
                "-f",
                "lavfi",
                "-i",
                "sine=f=880:d=3",
            ])
            .args(["-map", "0", "-map", "1", "-map", "2", "-c:v", "libx264"])
            .args(["-c:a:0", "aac", "-c:a:1", "pcm_s16le", "-ac:a:1", "24"])
            .arg(&src)
            .status()
            .unwrap();
        assert!(made.success());
        let r = clip_entry("c1", src, 2.0);
        let mut fallbacks = vec![];
        let (out, secs, kind) = render_entry_segment(
            &caps,
            &r,
            0,
            0,
            3.0,
            10.0,
            Resolution::Hd1080,
            temp.path(),
            &mut fallbacks,
        )
        .unwrap();
        assert_eq!(kind, "clip");
        assert!((secs - 2.0).abs() < 0.01);
        let info = ffmpeg::ffprobe_file(&caps.ffprobe_bin, &out).unwrap();
        assert!(info.has_audio && info.has_video);
        assert!(fallbacks.is_empty(), "{fallbacks:?}");
        let streams = std::process::Command::new(&caps.ffprobe_bin)
            .args(["-v", "error", "-select_streams", "a", "-show_entries"])
            .args(["stream=index", "-of", "csv=p=0"])
            .arg(&out)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&streams.stdout).lines().count(), 1);
    }

    fn live_cell(src: &str, x: u32, secs: f64) -> LiveCell {
        LiveCell {
            src: PathBuf::from(src),
            start: 0.0,
            x,
            y: 9,
            w: 1422,
            h: 1602,
            secs,
            tonemap: None,
            frame: None,
        }
    }

    #[test]
    fn live_collage_graph_waits_on_the_still_and_dissolves_in_and_out() {
        let caps = Capabilities {
            perspective: true,
            ..Default::default()
        };
        let mut live = [live_cell("a.mov", 9, 2.5), live_cell("b.mov", 1449, 3.0)];
        live[1].start = 1.5;
        let g = live_collage_graph(&caps, &live, 150, 0, Resolution::Hd1080);
        // Still staggered, but a waiting cell is never padded with its
        // motion's (blurred) first frame: the motion is shifted to its
        // start and the still shows until then.
        assert!(!g.contains("tpad="), "{g}");
        assert!(g.contains("setpts=PTS+0.000/TB[m1]"), "{g}");
        assert!(g.contains("setpts=PTS+1.500/TB[m2]"), "{g}");
        // Each motion dissolves in over the still and back out to it.
        assert!(
            g.contains("format=rgba,fade=t=in:st=0:d=0.250:alpha=1,fade=t=out:st=2.200:d=0.300:alpha=1,setpts=PTS+0.000/TB[m1]"),
            "{g}"
        );
        assert!(
            g.contains("format=rgba,fade=t=in:st=0:d=0.250:alpha=1,fade=t=out:st=2.700:d=0.300:alpha=1,setpts=PTS+1.500/TB[m2]"),
            "{g}"
        );
        // Canvas held once, each motion laid over it in input order, only
        // during its own window; outside it the canvas passes through.
        assert!(g.starts_with("[0:v]scale=2880:1620"), "{g}");
        assert!(
            g.contains("[c0][m1]overlay=9:9:eof_action=pass:enable='between(t,0.000,2.500)'[c1]"),
            "{g}"
        );
        assert!(
            g.contains(
                "[c1][m2]overlay=1449:9:eof_action=pass:enable='between(t,1.500,4.500)'[c2]"
            ),
            "{g}"
        );
        assert!(!g.contains("eof_action=repeat"), "{g}");
        assert!(g.contains("trim=duration=2.500"), "{g}");
        // Cover-cropped to the tile with the stills' top-biased focus.
        assert!(g.contains("crop=1422:1602:(iw-1422)/2:(ih-1602)*0.35:exact=1"));
        // The glide runs once, over the finished composite.
        assert!(g.rfind("overlay").unwrap() < g.find("perspective=").unwrap());
        assert!(g.ends_with("format=yuv420p[v]"), "{g}");
    }

    #[test]
    fn stagger_starts_spread_motion_across_the_hold() {
        let near = |a: &[f64], b: &[f64]| {
            assert_eq!(a.len(), b.len());
            assert!(a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-9), "{a:?}");
        };
        // Four 2.2 s motions in a 7 s hold: evenly spaced, the last ends
        // LIVE_LINGER before the cut.
        let s = stagger_starts(&[2.2; 4], 7.0);
        near(&s, &[0.0, 3.8 / 3.0, 7.6 / 3.0, 3.8]);
        assert!((s[3] + 2.2 - (7.0 - LIVE_LINGER)).abs() < 1e-9);
        // Short motions never leave a dead gap between cells.
        near(&stagger_starts(&[1.0, 1.0], 5.0), &[0.0, 1.0]);
        // A short first motion: the later gap grows to keep the ending.
        near(&stagger_starts(&[1.0, 3.0, 3.0], 7.0), &[0.0, 1.0, 3.0]);
        // Motions longer than the hold allows all start at once.
        near(&stagger_starts(&[3.0, 3.0], 3.5), &[0.0, 0.0]);
        // One moving cell starts at once; none is fine.
        near(&stagger_starts(&[2.0], 5.0), &[0.0]);
        assert!(stagger_starts(&[], 5.0).is_empty());
    }

    #[test]
    fn photo_rect_matches_cover_and_contain_fills() {
        // 4:3 photo in a 16:9 tile loses 25%: it covers the whole tile.
        assert_eq!(
            photo_rect(4.0 / 3.0, false, 9, 9, 1600, 900),
            (9, 9, 1600, 900)
        );
        // 3:4 photo in a tall 4:9 tile loses 41%: still fills the tile
        // (four portraits side by side fill their columns).
        assert_eq!(photo_rect(0.75, false, 9, 9, 400, 900), (9, 9, 400, 900));
        // Square photo in the same tile would lose over half: shown
        // whole, full width, centred vertically.
        assert_eq!(photo_rect(1.0, false, 9, 9, 400, 900), (9, 259, 400, 400));
        // Wide photo in a tall tile: same, and never larger than the tile.
        let (x, y, w, h) = photo_rect(16.0 / 9.0, false, 0, 0, 400, 900);
        assert_eq!((x, w), (0, 400));
        assert_eq!((y, h), ((900 - 225) / 2, 225));
        // The fitted rectangle is where `contain_over_blur` draws it.
        assert_eq!(contain_size(0.75, 400, 900), (400, 533));
    }

    #[test]
    fn live_collage_graph_tonemaps_hdr_motion_before_compositing() {
        let caps = Capabilities::default();
        let mut cell = live_cell("a.mov", 9, 2.0);
        cell.tonemap = Some("tonemap=hable:desat=0".into());
        let g = live_collage_graph(&caps, &[cell], 150, 0, Resolution::Hd1080);
        assert!(
            g.find("tonemap=hable").unwrap() < g.find("format=rgb24,scale=1422").unwrap(),
            "{g}"
        );
    }

    #[test]
    fn live_collage_args_feed_canvas_then_motions_then_silence() {
        let caps = Capabilities::default();
        let live = [live_cell("a.mov", 9, 2.5), live_cell("b.mov", 1449, 3.0)];
        let args = live_collage_args(
            &caps,
            Path::new("canvas.png"),
            &live,
            5.0,
            0,
            Resolution::Hd1080,
            Path::new("out.mp4"),
        );
        let inputs: Vec<&str> = args
            .windows(2)
            .filter(|w| w[0] == "-i")
            .map(|w| w[1].as_str())
            .collect();
        assert_eq!(
            inputs,
            [
                "canvas.png",
                "a.mov",
                "b.mov",
                "anullsrc=r=48000:cl=stereo:d=5"
            ]
        );
        // Audio comes from the silent track (input 3), never the motions.
        let maps: Vec<&str> = args
            .windows(2)
            .filter(|w| w[0] == "-map")
            .map(|w| w[1].as_str())
            .collect();
        assert_eq!(maps, ["[v]", "3:a"]);
        let frames = args.iter().position(|a| a == "-frames:v").unwrap();
        assert_eq!(args[frames + 1], "150");
        assert_eq!(args.last().unwrap(), "out.mp4");
    }

    /// The frame of `video` at `t` s, scaled to 320x180, saved as `png`.
    fn grab_frame(caps: &Capabilities, video: &Path, t: f64, png: &Path) -> image::RgbImage {
        let ok = std::process::Command::new(&caps.ffmpeg_bin)
            .args(["-v", "error", "-y", "-ss", &format!("{t}"), "-i"])
            .arg(video)
            .args(["-frames:v", "1", "-vf", "scale=320:180"])
            .arg(png)
            .status()
            .unwrap();
        assert!(ok.success());
        image::open(png).unwrap().to_rgb8()
    }

    /// Mean absolute difference, 0..255, between the frame of `va` at `a`
    /// seconds and the frame of `vb` at `b` seconds.
    fn frame_diff(caps: &Capabilities, va: &Path, a: f64, vb: &Path, b: f64, dir: &Path) -> f64 {
        let (fa, fb) = (
            grab_frame(caps, va, a, &dir.join("fa.png")),
            grab_frame(caps, vb, b, &dir.join("fb.png")),
        );
        let sum: u64 = fa
            .as_raw()
            .iter()
            .zip(fb.as_raw())
            .map(|(x, y)| u64::from(x.abs_diff(*y)))
            .sum();
        sum as f64 / fa.as_raw().len() as f64
    }

    /// A Live Photo in a collage dissolves into motion from the start, then
    /// rests on its still; with the option off the same collage stays still.
    #[test]
    fn collage_live_photo_moves_then_rests() {
        let caps = crate::ffmpeg::probe_capabilities("ffmpeg", "ffprobe");
        if !caps.libx264 || !caps.perspective {
            eprintln!("ffmpeg caps insufficient; skipping live collage test");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let still = |name: &str, rgb: [u8; 3]| {
            let p = dir.join(name);
            image::RgbImage::from_pixel(480, 640, image::Rgb(rgb))
                .save(&p)
                .unwrap();
            p
        };
        let motion = dir.join("live.mov");
        let made = std::process::Command::new(&caps.ffmpeg_bin)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc2=s=480x640:r=30:d=2")
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(&motion)
            .status()
            .unwrap();
        assert!(made.success());
        let mut live = card_entry("live", &still("live.png", [40, 90, 160]));
        live.motion_path = Some(motion);
        let plain = card_entry("plain", &still("plain.png", [160, 90, 40]));
        let members = [&live, &plain];
        let render = |on: bool, sub: &str| {
            let work = dir.join(sub);
            std::fs::create_dir_all(&work).unwrap();
            let (out, secs, kind) =
                render_collage_segment(&caps, &members, 0, 0, 4.0, on, Resolution::Hd1080, &work)
                    .unwrap();
            assert_eq!(kind, "still");
            assert!((secs - 5.0).abs() < 0.01);
            let info = ffmpeg::ffprobe_file(&caps.ffprobe_bin, &out).unwrap();
            assert!((info.duration_secs - 5.0).abs() < 0.1, "{info:?}");
            assert!(info.has_audio);
            out
        };
        let moving = render(true, "on");
        let playing = frame_diff(&caps, &moving, 0.3, &moving, 1.3, dir);
        let frozen = frame_diff(&caps, &moving, 3.0, &moving, 4.0, dir);
        let off = render(false, "off");
        let still_only = frame_diff(&caps, &off, 0.3, &off, 1.3, dir);
        // The rested still drifts with the canvas glide, so it is not
        // zero; playback changes the cell far more.
        assert!(
            playing > 2.0 * frozen.max(0.2),
            "playing {playing} frozen {frozen}"
        );
        assert!(
            playing > 4.0 * still_only.max(0.2),
            "playing {playing} still {still_only}"
        );
        // Once dissolved in, the cell shows the video, not the still.
        let early = frame_diff(&caps, &moving, 0.3, &off, 0.3, dir);
        assert!(
            early > 10.0,
            "the motion never shows over the still collage: {early}"
        );
    }

    /// Mean absolute difference, 0..255, of the left and right halves of
    /// the frames of `va` and `vb` at `t` seconds (scaled to 320x180).
    /// When `TRIPTAPE_FRAME_DIR` is set the frames are kept there to look at.
    fn half_diffs(caps: &Capabilities, va: &Path, vb: &Path, t: f64, dir: &Path) -> (f64, f64) {
        let keep = std::env::var_os("TRIPTAPE_FRAME_DIR").map(PathBuf::from);
        let grab = |video: &Path, tag: &str| {
            let png = dir.join(format!("{tag}_{t:.2}.png"));
            let frame = grab_frame(caps, video, t, &png);
            if let Some(k) = &keep {
                std::fs::create_dir_all(k).unwrap();
                std::fs::copy(&png, k.join(png.file_name().unwrap())).unwrap();
            }
            frame
        };
        let (fa, fb) = (grab(va, "moving"), grab(vb, "still"));
        let half = |left: bool| {
            let (mut sum, mut n) = (0u64, 0u64);
            for (x, y, pa) in fa.enumerate_pixels() {
                if (x < fa.width() / 2) == left {
                    let pb = fb.get_pixel(x, y);
                    sum +=
                        pa.0.iter()
                            .zip(pb.0)
                            .map(|(a, b)| u64::from(a.abs_diff(b)))
                            .sum::<u64>();
                    n += 3;
                }
            }
            sum as f64 / n as f64
        };
        (half(true), half(false))
    }

    /// Two Live Photos side by side play one after the other: the second
    /// shows its sharp still (not its motion's first frame) while the first
    /// moves, then moves in its own turn, and both end resting on their
    /// stills.
    #[test]
    fn collage_live_photos_wait_and_rest_on_their_stills() {
        let caps = crate::ffmpeg::probe_capabilities("ffmpeg", "ffprobe");
        if !caps.libx264 || !caps.perspective {
            eprintln!("ffmpeg caps insufficient; skipping live collage test");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let member = |name: &str, rgb: [u8; 3], src: &str| {
            let still = dir.join(format!("{name}.png"));
            image::RgbImage::from_pixel(480, 640, image::Rgb(rgb))
                .save(&still)
                .unwrap();
            let motion = dir.join(format!("{name}.mov"));
            let made = std::process::Command::new(&caps.ffmpeg_bin)
                .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
                .arg(src)
                .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
                .arg(&motion)
                .status()
                .unwrap();
            assert!(made.success());
            let mut e = card_entry(name, &still);
            e.motion_path = Some(motion);
            e
        };
        let a = member("a", [40, 90, 160], "testsrc2=s=480x640:r=30:d=2");
        let b = member("b", [160, 90, 40], "testsrc=s=480x640:r=30:d=2");
        let members = [&a, &b];
        // When the second (right-hand) cell starts, as the render will.
        let canvas = dir.join("canvas.png");
        let photos =
            compose_collage(&caps.ffmpeg_bin, &members, Resolution::Hd1080, &canvas).unwrap();
        let cells = live_cells(&caps, &members, &photos, 5.0);
        assert_eq!(cells.len(), 2, "both motions play");
        let second = cells[1].start;
        assert!(second >= 1.0, "the second cell waits its turn: {second}");
        let render = |on: bool, sub: &str| {
            let work = dir.join(sub);
            std::fs::create_dir_all(&work).unwrap();
            let (out, secs, _) =
                render_collage_segment(&caps, &members, 0, 0, 4.0, on, Resolution::Hd1080, &work)
                    .unwrap();
            assert!((secs - 5.0).abs() < 0.01);
            out
        };
        let moving = render(true, "on");
        let still = render(false, "off");
        if let Some(k) = std::env::var_os("TRIPTAPE_FRAME_DIR") {
            std::fs::create_dir_all(&k).unwrap();
            std::fs::copy(&moving, Path::new(&k).join("moving.mp4")).unwrap();
        }
        // (a) Before its turn the second cell is its sharp still while the
        // first moves.
        let (l, r) = half_diffs(&caps, &moving, &still, second - 0.4, dir);
        assert!(l > 10.0, "the first cell moves: {l}");
        assert!(r < 2.0, "the waiting cell shows its still: {r}");
        // (b) In its window the second cell moves.
        let (_, r) = half_diffs(&caps, &moving, &still, second + 0.6, dir);
        assert!(r > 10.0, "the second cell moves in its turn: {r}");
        // (c) Both rest sharp on their stills at the end.
        for t in [4.5, 4.75, 4.95] {
            let (l, r) = half_diffs(&caps, &moving, &still, t, dir);
            assert!(
                l < 2.0 && r < 2.0,
                "both cells rest sharp at {t} s: {l} {r}"
            );
        }
    }

    /// Real iPhone clip with an `apple_apac` track: `TRIPTAPE_APAC_CLIP=<copy of
    /// a .MOV> cargo test -p triptape-media real_apac -- --ignored`.
    #[test]
    #[ignore = "needs a real iPhone clip via TRIPTAPE_APAC_CLIP"]
    fn real_apac_clip_encodes() {
        let Ok(src) = std::env::var("TRIPTAPE_APAC_CLIP") else {
            return;
        };
        let caps = crate::ffmpeg::probe_capabilities("ffmpeg", "ffprobe");
        let temp = tempfile::tempdir().unwrap();
        let src = PathBuf::from(src);
        let r = clip_entry("apac", src, 3.0);
        let mut fallbacks = vec![];
        let (out, _, _) = render_entry_segment(
            &caps,
            &r,
            0,
            0,
            3.0,
            10.0,
            Resolution::Hd1080,
            temp.path(),
            &mut fallbacks,
        )
        .unwrap();
        let info = ffmpeg::ffprobe_file(&caps.ffprobe_bin, &out).unwrap();
        assert!(info.has_audio && info.has_video);
        assert!(
            !fallbacks.iter().any(|f| f.contains("no audio")),
            "{fallbacks:?}"
        );
    }

    /// Full hero pipeline on synthetic stills: Typst title + lower-thirds,
    /// per-cut transitions, and the bundled music bed (no --music passed).
    /// Works in a per-test temp dir (set `KEEP_HERO_SMOKE=1` to keep it and
    /// print its path, to measure RMS after the run).
    /// Skips quietly when ffmpeg/typst are unavailable.
    #[test]
    fn hero_smoke_bundled_music_and_overlays() {
        use triptape_core::types::Plan;

        let caps = crate::ffmpeg::probe_capabilities("ffmpeg", "ffprobe");
        if !caps.libx264 || !caps.xfade || !caps.loudnorm || !caps.perspective {
            eprintln!("ffmpeg caps insufficient; skipping hero smoke");
            return;
        }
        if !crate::titlecard::typst_available("typst") {
            eprintln!("typst missing; skipping hero smoke");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        if std::env::var_os("KEEP_HERO_SMOKE").is_some() {
            eprintln!("hero smoke kept in {}", temp.keep().display());
        }
        std::fs::create_dir_all(dir.join("work")).unwrap();
        // Three solid stills (red/green/blue) as PNG sources.
        let mut entries = vec![];
        for (i, px) in [[255u8, 0, 0], [0, 255, 0], [0, 0, 255]].iter().enumerate() {
            let img: image::RgbImage = image::ImageBuffer::from_pixel(640, 480, image::Rgb(*px));
            let p = dir.join(format!("still_{i}.png"));
            img.save(&p).unwrap();
            entries.push(ResolvedEntry {
                entry: PlanEntry {
                    asset_id: format!("a{i}"),
                    window: 0,
                    content_hash: None,
                    size_bytes: 0,
                    mtime: 0,
                    rendition: Rendition::Still,
                },
                still_path: p,
                motion_path: None,
                caption: if i == 2 {
                    Some("Golden Gate at sunset".into())
                } else {
                    None
                },
                // Oct 6 2026 12:00 and 12:10 UTC (one moment, so a0 and
                // a1 collage), then Oct 8 12:00.
                taken_at: Some(1791288000 + [0, 600, 2 * 86400][i]),
                tz_offset_secs: None,
                aspect: None,
                gps: None,
                frame: None,
                still: false,
            });
        }
        let mut captions = HashMap::new();
        captions.insert("a2".to_string(), "Golden Gate at sunset".to_string());
        let plan = Plan {
            plan_version: 1,
            schema_version: 1,
            hero: vec![],
            reel_selects: vec![],
            chapters: vec![
                triptape_core::types::Chapter {
                    title: "Day 1 · Arrival".into(),
                    asset_ids: vec!["a0".into(), "a1".into()],
                    ..Default::default()
                },
                triptape_core::types::Chapter {
                    title: "Day 2 · Coast".into(),
                    asset_ids: vec!["a2".into()],
                    ..Default::default()
                },
            ],
            captions,
            collage: Default::default(),
            map_polyline: vec![],
            music: triptape_core::types::MusicPick {
                track: "bundled".into(),
                rationale: "default".into(),
            },
            cover: Default::default(),
            crop: Default::default(),
            frame: Default::default(),
            still: Default::default(),
            book: Default::default(),
            closing: Default::default(),
        };
        let out = dir.join("hero.mp4");
        let v = render_hero_video(
            &caps,
            &plan,
            &entries,
            None,
            &dir.join("work"),
            &out,
            &VideoOptions {
                trip_name: "Smoke Trip".into(),
                still_secs: 1.5,
                title_secs: 1.0,
                music_paths: Vec::new(),
                keep_ambience: false,
                typst_bin: "typst".into(),
                resolution: Resolution::Hd1080,
                ..Default::default()
            },
        )
        .expect("hero smoke render");
        assert!(out.is_file());
        assert!(
            v.fallbacks
                .iter()
                .any(|f| f.contains("bundled triptape-theme")),
            "music bed fallback recorded: {:?}",
            v.fallbacks
        );
        // EDL: per-cut overlaps recorded, transitions vary, offsets match.
        let edl_json = std::fs::read_to_string(&v.edl_path).unwrap();
        let edl: serde_json::Value = serde_json::from_str(&edl_json).unwrap();
        let cuts = edl["cut_overlaps_secs"].as_array().unwrap();
        assert_eq!(
            cuts.len(),
            2,
            "title + same-chapter collage + solo still = 2 cuts"
        );
        let names: Vec<String> = edl["segments"]
            .as_array()
            .unwrap()
            .iter()
            .skip(1)
            .map(|s| s["transition_in"].as_str().unwrap().to_string())
            .collect();
        for w in names.windows(2) {
            assert_ne!(w[0], w[1], "no consecutive equal transitions");
        }
        assert_eq!(names[0], "circle_open");
        assert_eq!(names[names.len() - 1], "circle_close");
        // The captioned photo plays solo (keeping its caption), and the EDL
        // names the two uncaptioned photos collaged before it.
        let srt = std::fs::read_to_string(v.srt_path.as_ref().expect("sidecar")).unwrap();
        assert!(srt.contains("Golden Gate at sunset"));
        assert_eq!(
            edl["segments"][1]["collage_asset_ids"],
            serde_json::json!(["a0", "a1"])
        );
    }

    #[test]
    fn collage_caption_keeps_every_member() {
        let member = |caption: Option<&str>| ResolvedEntry {
            caption: caption.map(String::from),
            ..card_entry("a", Path::new("a.png"))
        };
        let (a, b, c) = (member(Some(" One ")), member(None), member(Some("Two")));
        assert_eq!(collage_caption(&[&a, &b, &c]).as_deref(), Some("One\nTwo"));
        let blank = member(Some("  "));
        assert_eq!(collage_caption(&[&b, &blank]), None);
        // The same words on every member show once.
        let (x, y, z) = (
            member(Some("Tower")),
            member(Some(" Tower ")),
            member(Some("Tower")),
        );
        assert_eq!(collage_caption(&[&x, &y, &z]).as_deref(), Some("Tower"));
    }

    #[test]
    fn closing_secs_counts_only_cards_that_render() {
        assert_eq!(closing_secs(true, 4.5, 3), 9.0);
        assert_eq!(closing_secs(true, 4.5, 0), 4.5);
        assert_eq!(closing_secs(false, 4.5, 3), places_card_secs(3));
        assert_eq!(closing_secs(false, 4.5, 0), 0.0);
        // A long list holds long enough to read, within the ceiling.
        assert!((places_card_secs(28) - 8.6).abs() < 1e-9);
        assert_eq!(places_card_secs(60), PLACES_CARD_MAX_SECS);
        // Two or more stops: the animated recap's budget replaces the card.
        let stops = |n| {
            let opts = VideoOptions {
                route_stops: (0..n)
                    .map(|i| triptape_core::route_stops::RouteStop {
                        route_index: i,
                        label: None,
                        at_sea: false,
                    })
                    .collect(),
                ..Default::default()
            };
            closing_map_len(&opts)
        };
        assert_eq!(stops(6), 8.4);
        assert_eq!(stops(40), 14.0);
        // An itinerary journey sets its own length.
        assert_eq!(stops(0), 4.5);
        let itin = triptape_core::itinerary::Itinerary::from_json(
            r#"{"stops": [
                {"id": "a", "label": "Los Angeles", "lat": 33.9, "lon": -118.4, "kind": "home"},
                {"id": "b", "label": "Tokyo", "lat": 35.7, "lon": 139.8, "arrive": "2026-10-04"}],
              "legs": [{"from": "a", "to": "b", "mode": "flight"}]}"#,
        )
        .unwrap();
        let journey = triptape_core::journey::Journey::build(&itin, &[]);
        let with = |map_style| {
            closing_map_len(&VideoOptions {
                journey: Some(journey.clone()),
                map_style,
                ..Default::default()
            })
        };
        // Told in hops, the film closes on the whole-route sheet; the
        // night style travels the route in one recap.
        assert_eq!(
            with(MapStyle::Parchment),
            triptape_core::journey::HOP_OVERVIEW_SECS
        );
        assert_eq!(with(MapStyle::Night), journey.duration_secs());
        assert_eq!(MapStyle::from_flag("Night").unwrap(), MapStyle::Night);
        assert!(MapStyle::from_flag("sepia").is_err());
        assert!(is_card("hop-03") && is_card("route") && !is_card("a1b2"));
        // The owner's clips are adopted only with one per hop, in full
        // parchment mode; then they close the film themselves.
        let clips = |n: usize, short_mode| VideoOptions {
            journey: Some(journey.clone()),
            hop_clips: (0..n)
                .map(|i| PathBuf::from(format!("hop{i}.mp4")))
                .collect(),
            short_mode,
            ..Default::default()
        };
        let hops = journey.hops().len();
        assert!(hop_clips_adopted(&clips(hops, false)));
        assert_eq!(closing_map_len(&clips(hops, false)), 0.0);
        assert!(!hop_clips_adopted(&clips(hops, true)));
        assert!(!hop_clips_adopted(&clips(hops + 1, false)));
        assert!(!hop_clips_adopted(&clips(0, false)));
        assert!(!hop_clips_adopted(&VideoOptions {
            map_style: MapStyle::Night,
            ..clips(hops, false)
        }));
        // The owner's date line wins over the photos'; a blank one hides it.
        assert_eq!(
            title_date_line(Some(" Oct 3rd – 30th "), Some("Oct 4–29")).as_deref(),
            Some("Oct 3rd – 30th")
        );
        assert_eq!(title_date_line(Some("  "), Some("Oct 4–29")), None);
        assert_eq!(
            title_date_line(None, Some("Oct 4–29")).as_deref(),
            Some("Oct 4–29")
        );
        assert_eq!(title_date_line(None, None), None);
        let args = first_frame_args(Path::new("hop0.mp4"), Path::new("title-map.png"));
        assert_eq!(
            args,
            ["-y", "-i", "hop0.mp4", "-frames:v", "1", "title-map.png"]
        );
    }

    #[test]
    fn long_clips_leave_time_for_later_segments() {
        const CLIP_SECS: f64 = 8.0;
        let segments = 6;
        let mut budget = 24.0;
        for done in 0..segments {
            assert!(budget >= 0.5, "segment {done} would be dropped");
            budget -= CLIP_SECS.min(fair_share(budget, segments - done));
        }
        assert!(budget.abs() < 1e-9, "the film uses its budget: {budget}");
        // Full-length films have no budget to share.
        assert!(fair_share(f64::INFINITY, 3).is_infinite());
    }

    #[test]
    fn short_film_picks_reach_every_chapter_in_order() {
        let entries: Vec<ResolvedEntry> = (0..39)
            .map(|i| ResolvedEntry {
                entry: PlanEntry {
                    asset_id: format!("a{i}"),
                    window: 0,
                    content_hash: None,
                    size_bytes: 0,
                    mtime: 0,
                    rendition: Rendition::Still,
                },
                still_path: PathBuf::new(),
                motion_path: None,
                caption: None,
                taken_at: None,
                tz_offset_secs: None,
                aspect: None,
                gps: None,
                frame: None,
                still: false,
            })
            .collect();
        let refs: Vec<&ResolvedEntry> = entries.iter().collect();
        let index = |id: &str| id[1..].parse::<usize>().unwrap();
        // Days of 12, 13 and 14 photos.
        let chapter = |id: &str| match index(id) {
            0..=11 => 0,
            12..=24 => 1,
            _ => 2,
        };

        let picks: Vec<usize> = spread_across_chapters(&refs, 7, chapter)
            .iter()
            .map(|r| index(&r.entry.asset_id))
            .collect();
        assert_eq!(picks.len(), 7);
        assert!(
            picks.windows(2).all(|w| w[0] < w[1]),
            "keeps order: {picks:?}"
        );
        let per_day = |day| {
            picks
                .iter()
                .filter(|&&p| chapter(&format!("a{p}")) == day)
                .count()
        };
        assert_eq!((per_day(0), per_day(1), per_day(2)), (3, 2, 2));

        // Everything fits: nothing is dropped.
        assert_eq!(spread_across_chapters(&refs, 39, chapter).len(), 39);
        // A chapter smaller than its share gives its slots to the others.
        let two = |id: &str| usize::from(index(id) > 0);
        assert_eq!(spread_across_chapters(&refs[..4], 3, two).len(), 3);
    }

    /// Regression: GPS + named places in short mode. The closing cards used
    /// to be appended after the budget was spent, overrunning 30 s.
    #[test]
    fn short_film_with_closing_cards_stays_within_limit() {
        use triptape_core::types::Plan;

        let caps = crate::ffmpeg::probe_capabilities("ffmpeg", "ffprobe");
        if !caps.libx264 || !caps.xfade || !caps.loudnorm {
            eprintln!("ffmpeg caps insufficient; skipping short-film limit smoke");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let img: image::RgbImage = image::ImageBuffer::from_pixel(64, 36, image::Rgb([9, 90, 160]));
        let still = dir.join("still.png");
        img.save(&still).unwrap();
        let ids: Vec<String> = (0..10).map(|i| format!("a{i}")).collect();
        let entries: Vec<ResolvedEntry> = ids
            .iter()
            .map(|id| ResolvedEntry {
                entry: PlanEntry {
                    asset_id: id.clone(),
                    window: 0,
                    content_hash: None,
                    size_bytes: 0,
                    mtime: 0,
                    rendition: Rendition::Still,
                },
                still_path: still.clone(),
                motion_path: None,
                caption: None,
                taken_at: Some(1791288000),
                tz_offset_secs: None,
                aspect: None,
                gps: None,
                frame: None,
                still: false,
            })
            .collect();
        let plan = Plan {
            plan_version: 1,
            schema_version: 1,
            hero: vec![],
            reel_selects: vec![],
            chapters: vec![triptape_core::types::Chapter {
                title: "Day 1".into(),
                asset_ids: ids,
                place: Some("Kyoto, Japan".into()),
                ..Default::default()
            }],
            captions: HashMap::new(),
            collage: Default::default(),
            map_polyline: vec![(35.0, 135.0), (35.1, 135.1)],
            music: triptape_core::types::MusicPick {
                track: "bundled".into(),
                rationale: "default".into(),
            },
            cover: Default::default(),
            crop: Default::default(),
            frame: Default::default(),
            still: Default::default(),
            book: Default::default(),
            closing: Default::default(),
        };
        // By default the title cuts straight to the first shot; the opening
        // map only plays when asked for. The closing recap plays either way.
        for opening_map in [false, true] {
            let out = dir.join(format!("hero_{opening_map}.mp4"));
            let v = render_hero_video(
                &caps,
                &plan,
                &entries,
                Some(&still),
                &dir.join(format!("work_{opening_map}")),
                &out,
                &VideoOptions {
                    short_mode: true,
                    resolution: Resolution::Hd1080,
                    opening_map,
                    ..Default::default()
                },
            )
            .expect("short film render");
            assert!(
                v.duration_secs <= SHORT_FILM_SECS,
                "short film ran {:.2}s",
                v.duration_secs
            );
            let edl: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&v.edl_path).unwrap()).unwrap();
            let segs = edl["segments"].as_array().unwrap();
            let ids: Vec<&str> = segs
                .iter()
                .map(|s| s["asset_id"].as_str().unwrap())
                .collect();
            let opening: &[&str] = if opening_map {
                &["title", "map"]
            } else {
                &["title"]
            };
            assert_eq!(&ids[..opening.len()], opening, "{ids:?}");
            // ...then straight into the first shot.
            assert!(ids[opening.len()].starts_with('a'), "{ids:?}");
            assert_eq!(
                ids.iter().filter(|&&i| i == "map").count(),
                usize::from(opening_map)
            );
            assert!(
                ids.contains(&"closing") && ids.contains(&"credits"),
                "{ids:?}"
            );
            // The title opens onto whatever follows with the opening move.
            assert_eq!(segs[1]["transition_in"], "circle_open", "{ids:?}");
        }
    }

    /// Regression: a clip whose planned trim (4 s) exceeds its real length
    /// (0.5 s) must not truncate the film. Timeline math uses probed
    /// segment durations; with planned durations the second xfade offset
    /// lands past the accumulated video and the chain collapses.
    #[test]
    fn hero_smoke_short_clip_does_not_truncate() {
        use triptape_core::types::Plan;

        let caps = crate::ffmpeg::probe_capabilities("ffmpeg", "ffprobe");
        if !caps.libx264 || !caps.xfade || !caps.loudnorm || !caps.perspective {
            eprintln!("ffmpeg caps insufficient; skipping short-clip smoke");
            return;
        }
        if !crate::titlecard::typst_available("typst") {
            eprintln!("typst missing; skipping short-clip smoke");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        std::fs::create_dir_all(dir.join("work")).unwrap();
        // 0.5 s clip source, planned as a 4 s trim.
        let clip = dir.join("short.mp4");
        let st = std::process::Command::new(&caps.ffmpeg_bin)
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=0.5:size=640x480:rate=30",
                "-pix_fmt",
                "yuv420p",
                &clip.to_string_lossy(),
            ])
            .output()
            .unwrap();
        assert!(
            st.status.success(),
            "make short clip: {}",
            String::from_utf8_lossy(&st.stderr)
        );
        let img: image::RgbImage =
            image::ImageBuffer::from_pixel(640, 480, image::Rgb([0, 200, 0]));
        let still = dir.join("still.png");
        img.save(&still).unwrap();
        let entries = vec![
            ResolvedEntry {
                entry: PlanEntry {
                    asset_id: "clip0".into(),
                    window: 0,
                    content_hash: None,
                    size_bytes: 0,
                    mtime: 0,
                    rendition: Rendition::Clip {
                        trim_start: 0.0,
                        trim_end: 4.0,
                    },
                },
                still_path: clip.clone(),
                motion_path: Some(clip.clone()),
                caption: None,
                taken_at: Some(1791288000),
                tz_offset_secs: None,
                aspect: None,
                gps: None,
                frame: None,
                still: false,
            },
            ResolvedEntry {
                entry: PlanEntry {
                    asset_id: "still1".into(),
                    window: 0,
                    content_hash: None,
                    size_bytes: 0,
                    mtime: 0,
                    rendition: Rendition::Still,
                },
                still_path: still,
                motion_path: None,
                caption: None,
                taken_at: Some(1791374400),
                tz_offset_secs: None,
                aspect: None,
                gps: None,
                frame: None,
                still: false,
            },
        ];
        let plan = Plan {
            plan_version: 1,
            schema_version: 1,
            hero: vec![],
            reel_selects: vec![],
            chapters: vec![triptape_core::types::Chapter {
                title: "Day 1".into(),
                asset_ids: vec!["clip0".into(), "still1".into()],
                ..Default::default()
            }],
            captions: HashMap::new(),
            collage: Default::default(),
            map_polyline: vec![],
            music: triptape_core::types::MusicPick {
                track: "bundled".into(),
                rationale: "default".into(),
            },
            cover: Default::default(),
            crop: Default::default(),
            frame: Default::default(),
            still: Default::default(),
            book: Default::default(),
            closing: Default::default(),
        };
        let out = dir.join("hero.mp4");
        render_hero_video(
            &caps,
            &plan,
            &entries,
            None,
            &dir.join("work"),
            &out,
            &VideoOptions {
                trip_name: "Short Clip Trip".into(),
                still_secs: 1.5,
                title_secs: 1.0,
                music_paths: Vec::new(),
                keep_ambience: false,
                typst_bin: "typst".into(),
                resolution: Resolution::Hd1080,
                ..Default::default()
            },
        )
        .expect("short-clip hero render");
        // Video stream duration (not format, which audio can inflate):
        // title 1.0 + clip freeze-padded to 2.5 + still 1.5 - overlaps
        // (~1.2) ~= 3.8 s. Without the pad it would be ~1.8 s.
        let probe = std::process::Command::new(&caps.ffprobe_bin)
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=duration",
                "-of",
                "csv=p=0",
                &out.to_string_lossy(),
            ])
            .output()
            .unwrap();
        let vdur: f64 = String::from_utf8_lossy(&probe.stdout)
            .trim()
            .parse()
            .unwrap_or(0.0);
        assert!(
            vdur > 3.2,
            "video stream must span all segments with the clip padded, got {vdur:.2}s"
        );
    }

    #[test]
    fn clip_pad_target_respects_budget() {
        assert_eq!(
            clip_pad_target(1.0, f64::INFINITY, 0.0),
            Some(MIN_SEGMENT_SECS)
        );
        assert_eq!(clip_pad_target(1.0, 10.0, 0.0), Some(MIN_SEGMENT_SECS));
        // Reading time grows with the words, within a ceiling.
        assert_eq!(caption_read_secs(None), 0.0);
        assert_eq!(caption_read_secs(Some("  ")), 0.0);
        assert!((caption_read_secs(Some("Very good pork rib soup")) - 3.0).abs() < 1e-9);
        assert_eq!(
            caption_read_secs(Some(&"word ".repeat(25))),
            CAPTION_MAX_SECS
        );
        // A caption that needs longer to read holds the clip longer.
        assert_eq!(clip_pad_target(1.6, 10.0, 6.0), Some(6.0));
        assert_eq!(clip_pad_target(1.6, 4.0, 6.0), Some(4.0));
        assert_eq!(clip_pad_target(0.8, 1.5, 0.0), Some(1.5));
        // Already capped to the remaining budget: nothing to pad.
        assert_eq!(clip_pad_target(0.6, 0.6, 0.0), None);
        assert_eq!(clip_pad_target(3.0, 10.0, 0.0), None);
    }

    /// A captioned shot held for `caption_hold_secs` shows its caption for
    /// the whole read time, even when the next cut overlaps by 0.8 s.
    #[test]
    fn caption_cue_lasts_the_read_time() {
        for cap in [
            "Alora is behind Lana's head",
            "Very good pork rib soup",
            "Hi there",
        ] {
            let read = caption_read_secs(Some(cap));
            let hold = caption_hold_secs(Some(cap));
            assert!(hold >= read + 1.1 - 1e-9, "{cap}");
            let next = 10.0 + hold - 0.8;
            let (st, en) = caption_cue(10.0, hold, next, read).expect("cue");
            assert!(en - st >= read - 1e-9, "{cap}: {st}..{en}");
        }
        // The hold's cut allowance covers the slowest transition.
        assert_eq!(
            plan::CAPTION_CUT_OVERLAP_SECS,
            transition_overlap(&Transition::FadeBlack)
        );
        // A short shot with no hold padding still skips rather than flashes.
        assert_eq!(caption_cue(0.0, 1.0, 0.5, 3.0), None);
        // The cut still bounds the cue.
        assert_eq!(caption_cue(0.0, 10.0, 2.0, 3.0), Some((0.3, 2.0)));
        // No caption, no hold.
        assert_eq!(caption_hold_secs(None), 0.0);
        assert_eq!(caption_hold_secs(Some("  ")), 0.0);
    }

    /// Default (4K) hero render end to end: Typst title, caption burn-in
    /// capable build, level 5.1 H.264, 3840x2160 output.
    #[test]
    fn hero_smoke_4k_default() {
        use triptape_core::types::Plan;

        let caps = crate::ffmpeg::probe_capabilities("ffmpeg", "ffprobe");
        if !caps.libx264 || !caps.xfade || !caps.loudnorm || !caps.perspective {
            eprintln!("ffmpeg caps insufficient; skipping 4K hero smoke");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let img: image::RgbImage =
            image::ImageBuffer::from_pixel(160, 90, image::Rgb([20, 120, 90]));
        let still = dir.join("still.png");
        img.save(&still).unwrap();
        let entry = card_entry("a0", &still);
        let plan = Plan {
            plan_version: 1,
            schema_version: 1,
            hero: vec![],
            reel_selects: vec![],
            chapters: vec![triptape_core::types::Chapter {
                title: "Day 1".into(),
                asset_ids: vec!["a0".into()],
                ..Default::default()
            }],
            captions: HashMap::new(),
            collage: Default::default(),
            map_polyline: vec![],
            music: triptape_core::types::MusicPick {
                track: "bundled".into(),
                rationale: "default".into(),
            },
            cover: Default::default(),
            crop: Default::default(),
            frame: Default::default(),
            still: Default::default(),
            book: Default::default(),
            closing: Default::default(),
        };
        let out = dir.join("hero4k.mp4");
        // No `resolution` override: the default must be 4K.
        render_hero_video(
            &caps,
            &plan,
            &[entry],
            None,
            &dir.join("work"),
            &out,
            &VideoOptions {
                trip_name: "4K Smoke".into(),
                still_secs: 1.0,
                title_secs: 1.0,
                ..Default::default()
            },
        )
        .expect("4k hero render");
        let info = ffmpeg::ffprobe_file(&caps.ffprobe_bin, &out).unwrap();
        assert_eq!((info.width, info.height), (3840, 2160));
        assert!(info.has_audio);
    }

    /// Bench capabilities and resolution: `TRIPTAPE_FFMPEG` (ffprobe beside
    /// it) and `TRIPTAPE_BENCH_RES` (default 4k).
    fn bench_setup() -> (Capabilities, Resolution) {
        let ffmpeg_bin = std::env::var("TRIPTAPE_FFMPEG").unwrap_or_else(|_| "ffmpeg".into());
        let ffprobe_bin = Path::new(&ffmpeg_bin)
            .with_file_name("ffprobe")
            .to_string_lossy()
            .into_owned();
        let caps = crate::ffmpeg::probe_capabilities(&ffmpeg_bin, &ffprobe_bin);
        let res = Resolution::from_flag(
            &std::env::var("TRIPTAPE_BENCH_RES").unwrap_or_else(|_| "4k".into()),
        )
        .unwrap();
        (caps, res)
    }

    /// Visual check: full hero render of real copies (`TRIPTAPE_BENCH_DIR`,
    /// `TRIPTAPE_BENCH_RES`, optional `TRIPTAPE_FFMPEG` for a libass build).
    #[test]
    #[ignore = "needs real media via TRIPTAPE_BENCH_DIR"]
    fn bench_hero() {
        use triptape_core::types::{Chapter, MusicPick, Plan};
        let Ok(dir) = std::env::var("TRIPTAPE_BENCH_DIR") else {
            return;
        };
        let dir = PathBuf::from(dir);
        let (caps, res) = bench_setup();
        let out_dir = dir.join(format!("hero_{}", res.label()));
        let _ = std::fs::remove_dir_all(&out_dir);
        std::fs::create_dir_all(&out_dir).unwrap();
        let mk = |n: &str, cap: Option<&str>, aspect: Option<f64>, clip: bool| {
            let mut e = clip_entry(n, dir.join(n), 4.0);
            if !clip {
                e.entry.rendition = Rendition::Still;
                e.motion_path = None;
            }
            e.caption = cap.map(String::from);
            e.aspect = aspect;
            e.taken_at = Some(1791288000);
            e
        };
        let entries = vec![
            mk(
                "s1.HEIC",
                Some("Golden hour at the temple"),
                Some(1.333),
                false,
            ),
            mk("s2.HEIC", None, Some(1.333), false),
            mk("s3.HEIC", None, Some(1.333), false),
            mk("s4.HEIC", None, Some(1.333), false),
            mk(
                "pano.HEIC",
                Some("The whole bay, left to right"),
                Some(4.436),
                false,
            ),
            mk(
                "portrait.jpg",
                Some("Portrait with a caption"),
                Some(0.5625),
                false,
            ),
            mk("clip4k.mov", Some("A 4K clip"), None, true),
        ];
        let ch = |title: &str, day: u32, place: &str, ids: &[&str]| Chapter {
            title: title.into(),
            asset_ids: ids.iter().map(|s| s.to_string()).collect(),
            place: Some(place.into()),
            day_index: Some(day),
            date_label: Some("Wed, Oct 8".into()),
            film_place: None,
            film_place_changes: vec![],
        };
        let plan = Plan {
            plan_version: 1,
            schema_version: 1,
            hero: vec![],
            reel_selects: vec![],
            chapters: vec![
                ch(
                    "a",
                    1,
                    "Kyoto, Japan",
                    &["s1.HEIC", "s2.HEIC", "s3.HEIC", "s4.HEIC"],
                ),
                ch(
                    "b",
                    2,
                    "Osaka, Japan",
                    &["pano.HEIC", "portrait.jpg", "clip4k.mov"],
                ),
            ],
            captions: HashMap::new(),
            collage: Default::default(),
            map_polyline: vec![(35.0, 135.7), (34.7, 135.5), (35.6, 139.7), (1.35, 103.8)],
            music: MusicPick {
                track: "bundled".into(),
                rationale: "default".into(),
            },
            cover: Default::default(),
            crop: Default::default(),
            frame: Default::default(),
            still: Default::default(),
            book: Default::default(),
            closing: Default::default(),
        };
        let (cw, ch_) = res.dims();
        let card = out_dir.join("card.png");
        crate::map::save_route_card(&plan.map_polyline, &[], cw, ch_, &card).unwrap();
        let t = std::time::Instant::now();
        let out = out_dir.join("hero.mp4");
        let v = render_hero_video(
            &caps,
            &plan,
            &entries,
            Some(&card),
            &out_dir.join("work"),
            &out,
            &VideoOptions {
                trip_name: "Japan to Singapore".into(),
                resolution: res,
                ..Default::default()
            },
        )
        .unwrap();
        eprintln!(
            "BENCH hero {}: {:.1}s wall for {:.1}s film; fallbacks {:?}",
            res.label(),
            t.elapsed().as_secs_f64(),
            v.duration_secs,
            v.fallbacks
        );
    }

    /// Timing bench: `TRIPTAPE_BENCH_DIR=<dir with s1..s4.HEIC pano.HEIC clip4k.mov ...>`
    /// (plus the [`bench_setup`] variables).
    #[test]
    #[ignore = "needs real media via TRIPTAPE_BENCH_DIR"]
    fn bench_segments() {
        let Ok(dir) = std::env::var("TRIPTAPE_BENCH_DIR") else {
            return;
        };
        let dir = PathBuf::from(dir);
        let (caps, res) = bench_setup();
        let work = dir.join(format!("work_{}", res.label()));
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).unwrap();
        let still = |n: &str| card_entry(n, &dir.join(n));
        let only = std::env::var("TRIPTAPE_BENCH_ONLY").unwrap_or_default();
        let time = |label: &str, f: &mut dyn FnMut()| {
            if !only.is_empty() && !only.split(',').any(|o| o == label) {
                return;
            }
            let t = std::time::Instant::now();
            f();
            eprintln!("BENCH {label}: {:.2}s", t.elapsed().as_secs_f64());
        };
        let mut fb = vec![];
        for (label, name) in [("still12mp", "s1.HEIC"), ("pano", "pano.HEIC")] {
            let r = still(name);
            time(label, &mut || {
                let (o, d, _) =
                    render_entry_segment(&caps, &r, 0, 0, 4.0, 99.0, res, &work, &mut fb).unwrap();
                eprintln!("  {label} dur {d:.2}s -> {}", o.display());
                std::fs::copy(&o, work.join(format!("{label}.mp4"))).unwrap();
            });
        }
        let members: Vec<ResolvedEntry> = ["s1.HEIC", "s2.HEIC", "s3.HEIC", "s4.HEIC"]
            .map(still)
            .to_vec();
        let refs: Vec<&ResolvedEntry> = members.iter().collect();
        time("collage4", &mut || {
            let (o, _, _) =
                render_collage_segment(&caps, &refs, 1, 1, 4.0, false, res, &work).unwrap();
            std::fs::copy(&o, work.join("collage.mp4")).unwrap();
        });
        // Optional s1..s4.mov beside the stills make them Live Photos.
        let live: Vec<ResolvedEntry> = members
            .iter()
            .cloned()
            .map(|mut m| {
                let mov = m.still_path.with_extension("mov");
                m.motion_path = mov.exists().then_some(mov);
                m
            })
            .collect();
        let refs: Vec<&ResolvedEntry> = live.iter().collect();
        time("collage4live", &mut || {
            let (o, _, _) =
                render_collage_segment(&caps, &refs, 3, 1, 4.0, true, res, &work).unwrap();
            std::fs::copy(&o, work.join("collage_live.mp4")).unwrap();
        });
        for (label, name, end) in [
            ("clip4k", "clip4k.mov", 5.0),
            ("clip1080", "clip1080.mov", 5.0),
            ("live", "live.mov", 3.0),
        ] {
            let r = clip_entry(label, dir.join(name), end);
            time(label, &mut || {
                let (o, d, _) =
                    render_entry_segment(&caps, &r, 2, 2, 4.0, 99.0, res, &work, &mut fb).unwrap();
                eprintln!("  {label} dur {d:.2}s");
                std::fs::copy(&o, work.join(format!("{label}.mp4"))).unwrap();
            });
        }
    }

    /// Five solid stills across two chapters (a trio and a pair collage).
    fn parallel_fixture(dir: &Path) -> (Plan, Vec<ResolvedEntry>) {
        let mut entries = vec![];
        let mut chapters: Vec<triptape_core::types::Chapter> = vec![];
        for i in 0..5usize {
            let img: image::RgbImage = image::ImageBuffer::from_pixel(
                320 + 16 * i as u32,
                240,
                image::Rgb([(i * 20) as u8, 90, 200 - (i * 10) as u8]),
            );
            let p = dir.join(format!("still_{i}.png"));
            img.save(&p).unwrap();
            let id = format!("a{i}");
            if i % 3 == 0 {
                chapters.push(triptape_core::types::Chapter {
                    title: format!("Day {}", chapters.len() + 1),
                    ..Default::default()
                });
            }
            chapters.last_mut().unwrap().asset_ids.push(id.clone());
            entries.push(ResolvedEntry {
                entry: PlanEntry {
                    asset_id: id,
                    window: 0,
                    content_hash: None,
                    size_bytes: 0,
                    mtime: 0,
                    rendition: Rendition::Still,
                },
                still_path: p,
                motion_path: None,
                caption: (i % 3 == 0).then(|| format!("Caption {i}")),
                taken_at: Some(1791288000 + i as i64 * 60),
                tz_offset_secs: None,
                aspect: Some(f64::from(320 + 16 * i as u32) / 240.0),
                gps: None,
                frame: None,
                still: false,
            });
        }
        let plan = Plan {
            plan_version: 1,
            schema_version: 1,
            hero: vec![],
            reel_selects: vec![],
            chapters,
            captions: HashMap::new(),
            collage: Default::default(),
            map_polyline: vec![],
            music: triptape_core::types::MusicPick {
                track: "bundled".into(),
                rationale: "default".into(),
            },
            cover: Default::default(),
            crop: Default::default(),
            frame: Default::default(),
            still: Default::default(),
            book: Default::default(),
            closing: Default::default(),
        };
        (plan, entries)
    }

    /// Two stills of one day with a clip between them encode as one
    /// collage at the first still's place, the clip still playing after it.
    #[test]
    fn stills_collage_across_a_clip_and_encode() {
        let Some(caps) = parallel_caps() else {
            eprintln!("ffmpeg caps insufficient; skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("clip.mp4");
        let made = std::process::Command::new(&caps.ffmpeg_bin)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc=size=320x180:rate=30:duration=3")
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=3"])
            .args(["-shortest", "-c:v", "libx264", "-c:a", "aac"])
            .arg(&clip)
            .status()
            .unwrap();
        assert!(made.success());
        let (mut plan, all) = parallel_fixture(dir.path());
        // a1 and a2 are uncaptioned stills of day 1; put the clip between.
        let entries = vec![all[1].clone(), clip_entry("c", clip, 2.5), all[2].clone()];
        plan.chapters = vec![triptape_core::types::Chapter {
            title: "Day 1".into(),
            asset_ids: vec!["a1".into(), "c".into(), "a2".into()],
            ..Default::default()
        }];
        let out = dir.path().join("hero.mp4");
        let v = render_hero_video(
            &caps,
            &plan,
            &entries,
            None,
            &dir.path().join("work"),
            &out,
            &parallel_opts(2),
        )
        .expect("render");
        let edl: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&v.edl_path).unwrap()).unwrap();
        let segs = edl["segments"].as_array().unwrap();
        let ids: Vec<&str> = segs
            .iter()
            .map(|s| s["asset_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["title", "a1", "c"], "{segs:?}");
        assert_eq!(
            segs[1]["collage_asset_ids"],
            serde_json::json!(["a1", "a2"])
        );
        assert_eq!(segs[2]["kind"], "clip");
        let info = ffmpeg::ffprobe_file(&caps.ffprobe_bin, &out).unwrap();
        assert!(info.has_video && info.duration_secs > 2.0, "{info:?}");
    }

    fn parallel_caps() -> Option<Capabilities> {
        let caps = crate::ffmpeg::probe_capabilities("ffmpeg", "ffprobe");
        (caps.libx264 && caps.xfade && caps.loudnorm && caps.perspective).then_some(caps)
    }

    fn parallel_opts(jobs: usize) -> VideoOptions {
        VideoOptions {
            trip_name: "Parallel".into(),
            still_secs: 1.0,
            title_secs: 0.5,
            resolution: Resolution::Hd1080,
            jobs: Some(jobs),
            // No Typst: keeps the test independent of installed fonts.
            typst_bin: "typst-not-installed".into(),
            ..Default::default()
        }
    }

    /// A cached re-export reuses every unchanged segment (rendered to a
    /// different work dir), and changing one shot re-renders only its group.
    #[test]
    fn segment_cache_reuses_unchanged_and_rerenders_changed() {
        use crate::segment_cache::{SegmentCache, SegmentCacheConfig};
        let Some(caps) = parallel_caps() else {
            eprintln!("ffmpeg caps insufficient; skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (plan, entries) = parallel_fixture(dir.path());
        let cfg = SegmentCacheConfig::roomy(dir.path().join("cache"));
        let run = |name: &str, entries: &[ResolvedEntry]| {
            let work = dir.path().join(name).join(".work");
            let cache = SegmentCache::open(&cfg, "ffmpeg test".into(), &work).unwrap();
            let caps = Capabilities {
                segment_cache: Some(cache.clone()),
                ..caps.clone()
            };
            let out = dir.path().join(format!("{name}.mp4"));
            let v = render_hero_video(&caps, &plan, entries, None, &work, &out, &parallel_opts(2))
                .expect("render");
            let edl: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&v.edl_path).unwrap()).unwrap();
            (cache.stats(), edl["segments"].clone(), v.duration_secs)
        };
        let (first, edl1, dur1) = run("a", &entries);
        assert_eq!(
            (first.hits, first.misses, first.stored),
            (0, 4, 4),
            "{first:?}"
        );
        let (second, edl2, dur2) = run("b", &entries);
        assert_eq!((second.hits, second.misses), (4, 0), "{second:?}");
        assert_eq!(edl1, edl2, "cached film has the same timeline");
        assert!((dur1 - dur2).abs() < 0.05, "{dur1} vs {dur2}");

        // Groups: a0 solo (captioned), a1+a2 collage, a3 solo (captioned), a4
        // solo. Replace a collage member: that group only re-renders.
        let changed = &entries[2].still_path;
        image::ImageBuffer::from_pixel(400, 240, image::Rgb([250u8, 10, 10]))
            .save(changed)
            .unwrap();
        let (third, _, _) = run("c", &entries);
        assert_eq!((third.hits, third.misses), (3, 1), "{third:?}");
    }

    /// A draft of a 4K film is a 1080p film on the keepsake's timeline,
    /// made from the same 4K segments: a second draft reuses them all.
    #[test]
    fn a_draft_renders_at_1080p_from_the_film_4k_segments() {
        use crate::segment_cache::{SegmentCache, SegmentCacheConfig};
        let Some(caps) = parallel_caps() else {
            eprintln!("ffmpeg caps insufficient; skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (plan, entries) = parallel_fixture(dir.path());
        let cfg = SegmentCacheConfig::roomy(dir.path().join("cache"));
        let opts = VideoOptions {
            resolution: Resolution::Uhd4k,
            draft: true,
            ..parallel_opts(2)
        };
        let run = |name: &str| {
            let work = dir.path().join(name).join(".work");
            let cache = SegmentCache::open(&cfg, "ffmpeg test".into(), &work).unwrap();
            let caps = Capabilities {
                segment_cache: Some(cache.clone()),
                ..caps.clone()
            };
            let out = dir.path().join(name).join("hero-draft.mp4");
            let v =
                render_hero_video(&caps, &plan, &entries, None, &work, &out, &opts).expect("draft");
            // The segments stay 4K: they are the keepsake's own.
            let seg = ffmpeg::ffprobe_file(&caps.ffprobe_bin, &work.join("seg_000.mp4")).unwrap();
            assert_eq!((seg.width, seg.height), (3840, 2160));
            (cache.stats(), v)
        };
        let (first, v) = run("a");
        assert_eq!((first.hits, first.misses), (0, 4), "{first:?}");
        let film = ffmpeg::ffprobe_file(&caps.ffprobe_bin, &v.path).unwrap();
        assert_eq!((film.width, film.height), (1920, 1080));
        assert!(film.has_audio);
        assert!(
            v.fallbacks
                .iter()
                .any(|f| f.starts_with("draft film (1920x1080, ")),
            "{:?}",
            v.fallbacks
        );
        let edl: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&v.edl_path).unwrap()).unwrap();
        assert_eq!(edl["draft"], true);
        // The timeline is the plan's, not the frame size's.
        assert!((film.duration_secs - edl["total_secs"].as_f64().unwrap()).abs() < 0.2);
        assert_eq!(v.srt_path.unwrap().file_name().unwrap(), "hero-draft.srt");
        let (second, _) = run("b");
        assert_eq!((second.hits, second.misses), (4, 0), "{second:?}");
    }

    /// A collage the user split ("Show on its own") is a new grouping: the
    /// collage's members re-render as their own segments instead of reusing
    /// the cached collage, while the other groups stay cached, and the
    /// film plays them apart.
    #[test]
    fn segment_cache_rerenders_a_collage_the_user_split() {
        use crate::segment_cache::{SegmentCache, SegmentCacheConfig};
        let Some(caps) = parallel_caps() else {
            eprintln!("ffmpeg caps insufficient; skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (plan, entries) = parallel_fixture(dir.path());
        let cfg = SegmentCacheConfig::roomy(dir.path().join("cache"));
        let run = |name: &str, plan: &Plan| {
            let work = dir.path().join(name).join(".work");
            let cache = SegmentCache::open(&cfg, "ffmpeg test".into(), &work).unwrap();
            let caps = Capabilities {
                segment_cache: Some(cache.clone()),
                ..caps.clone()
            };
            let out = dir.path().join(format!("{name}.mp4"));
            let v = render_hero_video(&caps, plan, &entries, None, &work, &out, &parallel_opts(2))
                .expect("render");
            let edl: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&v.edl_path).unwrap()).unwrap();
            (cache.stats(), edl["segments"].clone())
        };
        let (first, edl1) = run("a", &plan);
        assert_eq!(first.misses, 4, "{first:?}");
        assert_eq!(
            edl1[2]["collage_asset_ids"],
            serde_json::json!(["a1", "a2"])
        );
        // Groups: a0 solo (captioned), a1+a2 collage, a3 solo (captioned),
        // a4 solo. Setting a2 solo splits the collage.
        let split = Plan {
            collage: CollageEdits {
                solo: vec!["a2".into()],
                joins: vec![],
            },
            ..plan.clone()
        };
        let (second, edl2) = run("b", &split);
        let ids: Vec<&str> = edl2
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["asset_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["title", "a0", "a1", "a2", "a3", "a4"], "{edl2}");
        assert!(edl2[2].get("collage_asset_ids").is_none_or(|v| v.is_null()));
        // a1 and a2 are new segments; at most the shot after them changes
        // its glide; a0 and a4 come from the cache.
        assert!((2..=3).contains(&second.misses), "{second:?}");
        assert!(second.hits >= 2, "{second:?}");
    }

    /// Inserting a shot must not change the recipe (so the cache key) of
    /// any still segment except possibly the one right after it.
    #[test]
    fn inserting_a_shot_leaves_other_still_keys_unchanged() {
        use crate::segment_cache::{SegmentCache, SegmentCacheConfig};
        let caps = Capabilities {
            gblur: true,
            perspective: true,
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let cfg = SegmentCacheConfig::roomy(dir.path().join("c"));
        let cache = SegmentCache::open(&cfg, "ffmpeg test".into(), &dir.path().join("w")).unwrap();
        let mk = |id: &str| {
            let p = dir.path().join(format!("{id}.png"));
            std::fs::write(&p, id).unwrap();
            let mut e = card_entry(id, &p);
            e.entry.asset_id = id.into();
            e.aspect = Some(1.5);
            e
        };
        let ids: Vec<String> = (0..40).map(|i| format!("asset-{i}")).collect();
        let keys = |entries: &[ResolvedEntry]| -> Vec<(String, String)> {
            let groups: Vec<Vec<&ResolvedEntry>> = entries.iter().map(|e| vec![e]).collect();
            let variants = glide_variants(&groups);
            entries
                .iter()
                .zip(variants)
                .map(|(e, v)| {
                    let k = still_key(&cache, &caps, e, v, 4.0, f64::INFINITY, Resolution::Hd1080);
                    (e.entry.asset_id.clone(), k)
                })
                .collect()
        };
        let base: Vec<ResolvedEntry> = ids.iter().map(|i| mk(i)).collect();
        let before: HashMap<String, String> = keys(&base).into_iter().collect();
        for k in [0, 1, 7, 20, 39, 40] {
            let mut with = base.clone();
            with.insert(k, mk("inserted"));
            let after = keys(&with);
            let changed: Vec<&str> = after
                .iter()
                .filter(|(id, key)| before.get(id).is_some_and(|b| b != key))
                .map(|(id, _)| id.as_str())
                .collect();
            assert!(changed.len() <= 1, "insert at {k} changed {changed:?}");
            if let Some(c) = changed.first() {
                let pos = with.iter().position(|e| e.entry.asset_id == *c).unwrap();
                assert_eq!(pos, k + 1, "only the shot after the insert may change");
            }
        }
    }

    #[test]
    fn the_film_frame_crops_stills_clips_and_pans_before_any_fit() {
        let caps = Capabilities {
            gblur: true,
            perspective: true,
            ..Default::default()
        };
        let f: Frame = (10, 20, 50, 40);
        let framed = ffmpeg::frame_crop(Some(f));
        let crop = framed.trim_end_matches(',');
        assert!(
            crop.contains("iw*50/100") && crop.contains("ih*20/100"),
            "{crop}"
        );
        assert!(crop.contains("max(2,"), "never under 2 pixels: {crop}");
        assert_eq!(ffmpeg::frame_crop(None), "");
        // Hold: right after the input, ahead of the blur fit.
        let hold = hold_graph(&caps, 0, 120, Resolution::Hd1080, Some(f));
        assert!(hold.starts_with(&format!("[0:v]{crop},split")), "{hold}");
        assert!(hold.find(crop).unwrap() < hold.find("overlay").unwrap());
        assert!(!hold_graph(&caps, 0, 120, Resolution::Hd1080, None).contains("iw*"));
        // The hold arguments (and so the cache key) carry it.
        let args = |frame| {
            hold_args(
                &caps,
                Path::new("i.png"),
                4.0,
                0,
                Resolution::Hd1080,
                frame,
                Path::new("o.mp4"),
            )
        };
        assert_ne!(args(Some(f)), args(None));
        // Pan: cropped before the strip is scaled.
        let pano = pano_graph(5.0, 3.0, 0.5, 0, Resolution::Hd1080, Some(f));
        assert!(pano.starts_with(&format!("[0:v]{crop},scale=")), "{pano}");
        // Clip and Live Photo motion: first in the chain, before scale.
        let clip = clip_filter(
            &caps,
            Some(f),
            ClipFraming::Contain,
            (3840, 2160),
            false,
            None,
            90,
            Resolution::Hd1080,
        );
        assert!(clip.starts_with(&format!("{crop},fps=30,scale=")), "{clip}");
        let clip = clip_filter(
            &caps,
            Some(f),
            ClipFraming::Cover,
            (3840, 2160),
            false,
            None,
            90,
            Resolution::Hd1080,
        );
        assert!(clip.starts_with(&format!("{crop},fps=30,scale=")), "{clip}");
        let plain = clip_filter(
            &caps,
            None,
            ClipFraming::Cover,
            (3840, 2160),
            false,
            None,
            90,
            Resolution::Hd1080,
        );
        assert!(plain.starts_with("fps=30,"), "{plain}");
    }

    #[test]
    fn the_film_frame_changes_still_cache_keys() {
        use crate::segment_cache::{SegmentCache, SegmentCacheConfig};
        let caps = Capabilities {
            gblur: true,
            perspective: true,
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let cfg = SegmentCacheConfig::roomy(dir.path().join("c"));
        let cache = SegmentCache::open(&cfg, "ffmpeg test".into(), &dir.path().join("w")).unwrap();
        let p = dir.path().join("a.png");
        std::fs::write(&p, "a").unwrap();
        let mut e = card_entry("a", &p);
        let key = |e: &ResolvedEntry| {
            still_key(&cache, &caps, e, 0, 4.0, f64::INFINITY, Resolution::Hd1080)
        };
        let plain = key(&e);
        e.frame = Some((10, 20, 50, 40));
        let framed = key(&e);
        assert_ne!(plain, framed);
        e.frame = Some((10, 20, 50, 41));
        assert_ne!(framed, key(&e));
    }

    #[test]
    fn a_framed_collage_member_covers_its_cell_and_lays_out_by_its_framed_aspect() {
        // 2:1 photo in a 144x162 cell would normally sit whole over blur
        // (see cells_cover_when_close_and_contain_when_far); framed, it
        // fills the cell edge to edge, cut around the centre.
        let mut land = image::RgbImage::from_pixel(400, 200, image::Rgb([200, 30, 30]));
        for y in 0..20 {
            for x in 0..20 {
                land.put_pixel(x, y, image::Rgb([30, 200, 30]));
            }
        }
        let cell = fill_cell(&land, 144, 162, true);
        assert_eq!((cell.width(), cell.height()), (144, 162));
        assert_eq!(cell.get_pixel(0, 0), &image::Rgb([200, 30, 30]));
        assert_eq!(cell.get_pixel(143, 161), &image::Rgb([200, 30, 30]));
        // The photo rectangle of a framed member is the whole tile.
        assert_eq!(photo_rect(2.0, true, 9, 9, 400, 900), (9, 9, 400, 900));
        assert_ne!(photo_rect(2.0, false, 9, 9, 400, 900), (9, 9, 400, 900));
        // A framed photo is always charged its crop, never the contain rate.
        assert!(cell_penalty(2.0, 0.44, false) < 1.0);
        assert_eq!(cell_penalty(2.0, 0.44, true), crop_loss(2.0, 0.44));
        // The frame cuts the decoded pixels in display orientation.
        let mut img = image::RgbImage::from_pixel(200, 100, image::Rgb([0, 0, 0]));
        for y in 0..100 {
            for x in 100..200 {
                img.put_pixel(x, y, image::Rgb([9, 9, 9]));
            }
        }
        let right = crop_to_frame(&img, (50, 0, 50, 100));
        assert_eq!((right.width(), right.height()), (100, 100));
        assert_eq!(right.get_pixel(0, 0), &image::Rgb([9, 9, 9]));
        let sliver = crop_to_frame(&img, (99, 99, 1, 1));
        assert_eq!((sliver.width(), sliver.height()), (2, 1));
        // Layout: every framed member covers (never the contain penalty),
        // so the layout of far-off aspects still tiles the canvas.
        let aspects = [2.5, 0.5];
        let cells = collage_layout(&aspects, &[true, true], 2880, 1620);
        assert_eq!(cells.len(), 2);
        let area: u32 = cells.iter().map(|c| c.w * c.h).sum();
        assert_eq!(area, 2880 * 1620);
    }

    #[test]
    fn a_still_member_never_plays_motion_and_a_framed_one_frames_its_motion() {
        let caps = crate::ffmpeg::probe_capabilities("ffmpeg", "ffprobe");
        if !caps.libx264 || !caps.perspective {
            eprintln!("ffmpeg caps insufficient; skipping still-member test");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let member = |name: &str, rgb: [u8; 3]| {
            let still = dir.join(format!("{name}.png"));
            image::RgbImage::from_pixel(480, 640, image::Rgb(rgb))
                .save(&still)
                .unwrap();
            let motion = dir.join(format!("{name}.mov"));
            let made = std::process::Command::new(&caps.ffmpeg_bin)
                .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
                .arg("testsrc2=s=480x640:r=30:d=2")
                .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
                .arg(&motion)
                .status()
                .unwrap();
            assert!(made.success());
            let mut e = card_entry(name, &still);
            e.motion_path = Some(motion);
            e
        };
        let a = member("a", [40, 90, 160]);
        let mut b = member("b", [160, 90, 40]);
        let canvas = dir.join("canvas.png");
        let play = |members: &[&ResolvedEntry]| {
            let photos =
                compose_collage(&caps.ffmpeg_bin, members, Resolution::Hd1080, &canvas).unwrap();
            live_cells(&caps, members, &photos, 5.0)
        };
        assert_eq!(play(&[&a, &b]).len(), 2);
        // Played as a still: its cell stays a still.
        b.still = true;
        assert!(live_motion(&b).is_none());
        let cells = play(&[&a, &b]);
        assert_eq!(cells.len(), 1);
        assert!(cells[0].src.ends_with("a.mov"));
        // Framed: the motion gets the same rectangle, and covers.
        b.still = false;
        b.frame = Some((0, 25, 100, 50));
        let cells = play(&[&a, &b]);
        assert_eq!(cells.len(), 2);
        let framed = cells.iter().find(|c| c.src.ends_with("b.mov")).unwrap();
        assert_eq!(framed.frame, Some((0, 25, 100, 50)));
        let g = live_collage_graph(&caps, &cells, 150, 0, Resolution::Hd1080);
        assert!(
            g.contains(&format!(
                "{}fps=30",
                ffmpeg::frame_crop(Some((0, 25, 100, 50)))
            )),
            "{g}"
        );
    }

    /// Neighbouring groups with the same hash take different variants.
    #[test]
    fn glide_variants_separate_equal_neighbours() {
        let mk = |id: &str| {
            let mut e = card_entry(id, Path::new("x.png"));
            e.entry.asset_id = id.into();
            e
        };
        let a = mk("same");
        let b = mk("same");
        let groups = vec![vec![&a], vec![&b]];
        let v = glide_variants(&groups);
        assert_ne!(v[0], v[1]);
        assert_eq!(v[0], stable_variant(&a.entry));
    }

    /// Lower thirds are cached on the base segment plus their text: a repeat
    /// export skips them all, and changing one chapter-opening shot redoes
    /// only that chapter's overlay.
    #[test]
    fn overlay_cache_reuses_unchanged_lower_thirds() {
        use crate::segment_cache::{SegmentCache, SegmentCacheConfig};
        let Some(caps) = parallel_caps() else {
            eprintln!("ffmpeg caps insufficient; skipping");
            return;
        };
        if !crate::titlecard::typst_available("typst") {
            eprintln!("typst missing; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (plan, entries) = parallel_fixture(dir.path());
        let cfg = SegmentCacheConfig::roomy(dir.path().join("cache"));
        let run = |name: &str| {
            let work = dir.path().join(name).join(".work");
            let cache = SegmentCache::open(&cfg, "ffmpeg test".into(), &work).unwrap();
            let caps = Capabilities {
                segment_cache: Some(cache.clone()),
                ..caps.clone()
            };
            let opts = VideoOptions {
                typst_bin: "typst".into(),
                ..parallel_opts(2)
            };
            let out = dir.path().join(format!("{name}.mp4"));
            render_hero_video(&caps, &plan, &entries, None, &work, &out, &opts).expect("render");
            cache.stats()
        };
        // Two chapters open with a0 and a3, so two lower thirds.
        let first = run("a");
        assert_eq!(
            (first.overlay_hits, first.overlay_misses),
            (0, 2),
            "{first:?}"
        );
        let second = run("b");
        assert_eq!(
            (second.overlay_hits, second.overlay_misses),
            (2, 0),
            "{second:?}"
        );
        // Segment counts stay about segments only.
        assert_eq!((second.hits, second.misses), (4, 0), "{second:?}");
        // Replace the shot that opens chapter 1.
        image::ImageBuffer::from_pixel(400, 240, image::Rgb([250u8, 10, 10]))
            .save(&entries[0].still_path)
            .unwrap();
        let third = run("c");
        assert_eq!(
            (third.overlay_hits, third.overlay_misses),
            (1, 1),
            "{third:?}"
        );
    }

    /// Sibling windows of one clip differ in trim, so they get distinct keys
    /// and (hashed with the window index) are free to differ in variant.
    #[test]
    fn sibling_windows_get_distinct_keys_and_variants() {
        use crate::segment_cache::{SegmentCache, SegmentCacheConfig};
        let Some(caps) = parallel_caps() else {
            eprintln!("ffmpeg caps insufficient; skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("long.mp4");
        let made = std::process::Command::new(&caps.ffmpeg_bin)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc=size=320x180:rate=30:duration=4")
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=4"])
            .args(["-shortest", "-c:v", "libx264", "-c:a", "aac"])
            .arg(&src)
            .status()
            .unwrap();
        assert!(made.success());
        let cfg = SegmentCacheConfig::roomy(dir.path().join("cache"));
        let work = dir.path().join("w");
        std::fs::create_dir_all(&work).unwrap();
        let cache = SegmentCache::open(&cfg, "ffmpeg test".into(), &work).unwrap();
        let caps = Capabilities {
            segment_cache: Some(cache.clone()),
            ..caps
        };
        let mut windows: Vec<ResolvedEntry> = vec![];
        for (w, (a, b)) in [(0.0, 1.5), (2.0, 3.5)].into_iter().enumerate() {
            let mut r = clip_entry("long", src.clone(), b);
            r.entry.window = w as u32;
            r.entry.rendition = Rendition::Clip {
                trim_start: a,
                trim_end: b,
            };
            windows.push(r);
        }
        for (i, r) in windows.iter().enumerate() {
            let mut fb = vec![];
            render_entry_segment(
                &caps,
                r,
                i,
                0,
                1.0,
                f64::INFINITY,
                Resolution::Hd1080,
                &work,
                &mut fb,
            )
            .unwrap();
        }
        let st = cache.stats();
        assert_eq!((st.hits, st.misses, st.stored), (0, 2, 2), "{st:?}");
        // The window index feeds the variant hash.
        let h = |w: u32| {
            let mut e = windows[0].entry.clone();
            e.window = w;
            stable_variant(&e)
        };
        assert!((1..64).any(|w| h(w) != h(0)), "window ignored by variant");
    }

    /// A trimmed clip is cached on its full recipe: the same trim hits from
    /// another work dir, a different trim misses.
    #[test]
    fn segment_cache_keys_clips_by_trim() {
        use crate::segment_cache::{SegmentCache, SegmentCacheConfig};
        let Some(caps) = parallel_caps() else {
            eprintln!("ffmpeg caps insufficient; skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("clip.mp4");
        let made = std::process::Command::new(&caps.ffmpeg_bin)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc=size=320x180:rate=30:duration=3")
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=3"])
            .args(["-shortest", "-c:v", "libx264", "-c:a", "aac"])
            .arg(&src)
            .status()
            .unwrap();
        assert!(made.success());
        let cfg = SegmentCacheConfig::roomy(dir.path().join("cache"));
        let render = |work_name: &str, end: f64| {
            let work = dir.path().join(work_name);
            std::fs::create_dir_all(&work).unwrap();
            let cache = SegmentCache::open(&cfg, "ffmpeg test".into(), &work).unwrap();
            let caps = Capabilities {
                segment_cache: Some(cache.clone()),
                ..caps.clone()
            };
            let r = clip_entry("clip", src.clone(), end);
            let mut fb = vec![];
            let (out, secs, kind) = render_entry_segment(
                &caps,
                &r,
                1,
                1,
                1.0,
                f64::INFINITY,
                Resolution::Hd1080,
                &work,
                &mut fb,
            )
            .unwrap();
            assert_eq!(kind, "clip");
            assert!(out.metadata().unwrap().len() > 0);
            (cache.stats(), secs)
        };
        let (a, secs_a) = render("w1", 2.0);
        assert_eq!((a.hits, a.misses), (0, 1));
        let (b, secs_b) = render("w2", 2.0);
        assert_eq!((b.hits, b.misses), (1, 0), "{b:?}");
        assert_eq!(secs_a, secs_b);
        let (c, _) = render("w3", 1.5);
        assert_eq!((c.hits, c.misses), (0, 1), "{c:?}");
    }

    #[test]
    fn parallel_render_matches_sequential_order_and_timeline() {
        let Some(caps) = parallel_caps() else {
            eprintln!("ffmpeg caps insufficient; skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (plan, entries) = parallel_fixture(dir.path());
        let run = |jobs: usize| {
            let work = dir.path().join(format!("work{jobs}"));
            let out = dir.path().join(format!("hero{jobs}.mp4"));
            let seen = std::sync::Arc::new(Collect::default());
            let caps = Capabilities {
                progress: crate::progress::Progress::new(seen.clone()),
                ..caps.clone()
            };
            let v = render_hero_video(
                &caps,
                &plan,
                &entries,
                None,
                &work,
                &out,
                &parallel_opts(jobs),
            )
            .expect("render");
            let edl: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&v.edl_path).unwrap()).unwrap();
            (edl, v.fallbacks, seen)
        };
        let (edl1, fb1, seen1) = run(1);
        let (edl3, fb3, seen3) = run(3);
        // Same segments, order, probed durations, offsets and notes.
        assert_eq!(edl1["segments"], edl3["segments"]);
        assert_eq!(edl1["total_secs"], edl3["total_secs"]);
        assert_eq!(edl1["cut_overlaps_secs"], edl3["cut_overlaps_secs"]);
        assert_eq!(fb1, fb3);
        assert!(edl1["segments"].as_array().unwrap().len() >= 3);
        // Collages happened, so the plan exercised both group kinds.
        assert!(edl1["segments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s.get("collage_asset_ids").is_some()));
        // Progress counted every segment once, ending complete, both ways.
        for seen in [seen1, seen3] {
            let counts = seen.counts("segment");
            let total = counts.last().unwrap().1;
            assert_eq!(counts.last().unwrap().0, total);
            let dones: Vec<usize> = counts.iter().map(|c| c.0).collect();
            assert!(dones.windows(2).all(|w| w[0] <= w[1]), "{dones:?}");
        }
    }

    #[test]
    fn cancel_mid_parallel_render_leaves_no_partials() {
        let Some(caps) = parallel_caps() else {
            eprintln!("ffmpeg caps insufficient; skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (plan, entries) = parallel_fixture(dir.path());
        // Cancels as soon as the first segment reports done, with the
        // other workers mid-encode.
        let seen = std::sync::Arc::new(Collect::cancel_after_segments(1));
        let caps = Capabilities {
            progress: crate::progress::Progress::new(seen.clone()),
            ..caps
        };
        let work = dir.path().join("work");
        let out = dir.path().join("hero.mp4");
        let res = render_hero_video(&caps, &plan, &entries, None, &work, &out, &parallel_opts(3));
        assert!(matches!(res, Err(crate::MediaError::Cancelled)), "{res:?}");
        assert!(!out.exists());
        let mut leftovers = vec![];
        let mut stack = vec![dir.path().to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(d).unwrap().flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if e.path().is_dir() {
                    stack.push(e.path());
                } else if name.contains(".partial") {
                    leftovers.push(name);
                }
            }
        }
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// Records counted events; optionally asks for cancel after N segments.
    #[derive(Default)]
    struct Collect {
        events: std::sync::Mutex<Vec<crate::progress::ProgressEvent>>,
        cancel_after: Option<usize>,
        stop: std::sync::atomic::AtomicBool,
    }

    impl Collect {
        fn cancel_after_segments(n: usize) -> Self {
            Self {
                cancel_after: Some(n),
                ..Default::default()
            }
        }

        fn counts(&self, unit: &str) -> Vec<(usize, usize)> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|e| match e {
                    crate::progress::ProgressEvent::Counted {
                        unit: u,
                        done,
                        total,
                        ..
                    } if *u == unit => Some((*done, *total)),
                    _ => None,
                })
                .collect()
        }
    }

    impl crate::progress::ProgressSink for Collect {
        fn report(&self, e: &crate::progress::ProgressEvent) {
            if let (
                Some(n),
                crate::progress::ProgressEvent::Counted {
                    unit: "segment",
                    done,
                    ..
                },
            ) = (self.cancel_after, e)
            {
                if *done >= n {
                    self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            self.events.lock().unwrap().push(e.clone());
        }

        fn cancelled(&self) -> bool {
            self.stop.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[test]
    fn job_count_follows_cores_and_memory() {
        // A quarter of the cores, at least one.
        assert_eq!(jobs_for(10, Some(64 << 30), Resolution::Hd1080), 2);
        assert_eq!(jobs_for(16, Some(64 << 30), Resolution::Hd1080), 4);
        assert_eq!(jobs_for(2, Some(64 << 30), Resolution::Uhd4k), 1);
        // Memory caps 4K: half of 16 GiB over the 4K estimate.
        let mem = 16u64 << 30;
        let by_mem = (mem / 2 / Resolution::Uhd4k.segment_peak_bytes()) as usize;
        assert_eq!(jobs_for(32, Some(mem), Resolution::Uhd4k), by_mem.max(1));
        assert_eq!(jobs_for(32, Some(1 << 30), Resolution::Uhd4k), 1);
        // A lone worker keeps ffmpeg's default threading.
        assert_eq!(thread_share(1), None);
        assert!(thread_share(2).unwrap() >= 2);
    }

    /// Visual check of clip framing, a portrait collage and the map card on
    /// real copies in `TRIPTAPE_BENCH_DIR` (portrait.mov, portrait2.mov,
    /// live.mov, land.mov, c1..c3.HEIC); writes `vis_*.mp4` there.
    #[test]
    #[ignore = "needs real media via TRIPTAPE_BENCH_DIR"]
    fn visual_check_framing() {
        let Ok(dir) = std::env::var("TRIPTAPE_BENCH_DIR") else {
            return;
        };
        let dir = PathBuf::from(dir);
        let (caps, res) = bench_setup();
        let work = dir.join(format!("work_{}", res.label()));
        std::fs::create_dir_all(&work).unwrap();
        let mut fb = vec![];
        for (i, name) in ["portrait.mov", "portrait2.mov", "live.mov", "land.mov"]
            .iter()
            .enumerate()
        {
            let r = clip_entry(name, dir.join(name), 3.0);
            let t = std::time::Instant::now();
            let (o, _, _) =
                render_entry_segment(&caps, &r, i, i, 4.0, 99.0, res, &work, &mut fb).unwrap();
            eprintln!("VIS {name}: {:.2}s", t.elapsed().as_secs_f64());
            std::fs::copy(&o, dir.join(format!("vis_{}_{name}.mp4", res.label()))).unwrap();
        }
        let members: Vec<ResolvedEntry> = ["c1.HEIC", "c2.HEIC", "c3.HEIC"]
            .map(|n| card_entry(n, &dir.join(n)))
            .to_vec();
        let refs: Vec<&ResolvedEntry> = members.iter().collect();
        let (o, _, _) = render_collage_segment(&caps, &refs, 9, 1, 4.0, true, res, &work).unwrap();
        std::fs::copy(&o, dir.join(format!("vis_{}_collage.mp4", res.label()))).unwrap();
        let (w, h) = res.dims();
        let card = dir.join(format!("map_{}.png", res.label()));
        let route = [(35.7, 139.8), (34.7, 135.5), (1.35, 103.8)];
        crate::map::save_route_card(&route, &[0], w, h, &card).unwrap();
        let out = dir.join(format!("vis_{}_map.mp4", res.label()));
        crate::map::render_map_push_in(&caps, &card, &out, 3.0, 0, res).unwrap();
        eprintln!("fallbacks: {fb:?}");
    }

    #[test]
    fn a_repeated_caption_is_shown_once() {
        let h = 3600;
        let cap = |c: Option<&'static str>, ch: usize, t: i64| (c, Some(ch), Some(t));
        // A A A -> A _ _, case and spacing aside, an uncaptioned shot
        // between them or not.
        let a = Some("Some fun multiple person panoramas");
        let a2 = Some("  some fun Multiple person panoramas ");
        assert_eq!(
            repeated_captions(&[
                cap(a, 0, 0),
                cap(a2, 0, 60),
                cap(None, 0, 90),
                cap(a, 0, 120)
            ]),
            [false, true, false, true]
        );
        // A B A -> all shown.
        let b = Some("Lunch");
        assert_eq!(
            repeated_captions(&[cap(a, 0, 0), cap(b, 0, 60), cap(a, 0, 120)]),
            [false, false, false]
        );
        // The same words in another chapter hours later are a new showing;
        // minutes later across a chapter change, still a repeat.
        assert_eq!(
            repeated_captions(&[cap(a, 0, 0), cap(a, 1, 5 * h)]),
            [false, false]
        );
        assert_eq!(
            repeated_captions(&[cap(a, 0, 0), cap(a, 1, 600)]),
            [false, true]
        );
        // Unknown chapter and time: never a repeat.
        assert_eq!(
            repeated_captions(&[(a, None, None), (a, None, None)]),
            [false, false]
        );
    }

    #[test]
    fn srt_timestamps() {
        assert_eq!(srt_ts(0.0), "00:00:00,000");
        assert_eq!(srt_ts(61.5), "00:01:01,500");
        assert_eq!(srt_ts(3723.25), "01:02:03,250");
    }
}
