//! Split days: when the group splits up, tell each party's day in turn.
//!
//! A family trip shot on several phones interleaves everyone's shots in
//! capture order. On a day the group splits (some tour the city while the
//! others spend it at a theme park 12 km away) that reads city, park, city,
//! park. This module finds such days and orders them party by party; every
//! other day keeps plain capture order ([`crate::cluster::story_key`]).
//!
//! 1. **Camera streams** ([`camera_streams`]). Phones and cameras number
//!    their files with a per-device counter (`IMG_1620.HEIC`). Within one
//!    device the counter rises with capture time, so a trip's files fall
//!    into chains: a greedy chain partition links each file to the
//!    cheapest earlier file of the same naming scheme, where a link costs
//!    the counter step plus the hours between the shots. The step wraps at
//!    9999 → 0001, so a counter rollover stays one chain, and a phone whose
//!    counters overlap another's numerically weeks apart still forms its
//!    own chain (the hours make the cross link expensive). Files without a
//!    counter (`FullSizeRender.heic`, UUID names) have no stream.
//! 2. **Parties** (per capture-local day, [`crate::cluster::day_key`]). Two
//!    streams are *apart* when their positions at matching times stay at
//!    least [`APART_M`] apart over [`MIN_APART_SECS`], with at least
//!    [`MIN_APART_SAMPLES`] fixes from each. A stream's position at another
//!    stream's capture time is interpolated between its own fixes up to
//!    [`MATCH_BRACKET_SECS`] apart, else taken from its nearest fix within
//!    [`MATCH_NEAR_SECS`]. Streams seen within [`APART_M`] of each other
//!    merge into one party (single linkage, strongest evidence first, never
//!    joining two streams that were apart). A day with no apart pair is
//!    left exactly as it was.
//! 3. **Story order** on a split day: the shared stretch before the split
//!    stays in time order; the split stretch runs from the first moment the
//!    parties were seen apart until a fix puts one party back near where
//!    another was last seen (or that sighting is over [`CARRY_SECS`] old,
//!    or the day ends), and is told party by party (each party's shots in
//!    time order, the owner's party first); then the shared remainder.
//!    The primary stream is the one holding the most user captions (ties:
//!    most files): the owner's phone.

use crate::cluster::{day_of, same_day, speed_jump, story_key};
use crate::geo::haversine_m;
use crate::plan::has_caption;
use crate::types::Asset;
use std::collections::{BTreeMap, BTreeSet};

/// Largest counter step between two consecutive files of one stream, and
/// the most a link may cost (steps plus hours apart). On the 2026 Japan to
/// Singapore trip (3,196 files, six phones) all but two links cost under
/// 300: a phone rarely takes 500 shots that never reach the trip folder
/// between two that do.
pub const STREAM_MAX_STEP: u32 = 500;
/// A file may link to a next counter captured up to this much earlier: a
/// clip is dated when recording began or ended, so `IMG_1038.MOV` can be
/// stamped seconds before `IMG_1037.JPG` (10 min).
pub const STREAM_TIME_SLACK_SECS: i64 = 600;
/// Files sharing a counter are one shot (a copy, an `_edited` version, a
/// `(1)` export) only when captured this close together; the same number
/// at another time is another device (60 s).
pub const STREAM_TWIN_SECS: i64 = 60;
/// Cheapest candidate links kept per file: bounds the work on big trips.
const STREAM_CANDIDATES: usize = 8;
/// Counters run 0000..9999 and wrap.
const COUNTER_SPAN: u32 = 10_000;

/// Distance at matching times that counts as apart (3 km): further than a
/// group strays within one sight, a theme park or a museum, and well past
/// GPS error, yet under the 12-40 km a split party actually travels.
pub const APART_M: f64 = 3_000.0;
/// How long two streams must stay apart (45 min): a separate outing, not a
/// detour to the restroom or a lag between one bus and the next.
pub const MIN_APART_SECS: i64 = 45 * 60;
/// Fixes (distinct capture times) each stream needs inside one apart
/// stretch, so one stray fix cannot split a day.
pub const MIN_APART_SAMPLES: usize = 2;
/// A stream's position at time `t` is interpolated between its own fixes
/// up to this far apart (60 min)...
pub const MATCH_BRACKET_SECS: i64 = 60 * 60;
/// ...else it is its nearest fix within this (10 min); else unknown.
pub const MATCH_NEAR_SECS: i64 = 10 * 60;
/// After the last matching-time evidence, a party is still apart while
/// each new fix of its phones is [`APART_M`] or more from every other
/// party's latest fix, and that fix is at most this old (2 h): the parties
/// are not together again until a fix says so, but a stale fix says little.
pub const CARRY_SECS: i64 = 2 * 3600;
/// A shot with neither a party stream nor GPS joins the party of the
/// nearest party shot this close in time (10 min).
pub const STRAY_JOIN_SECS: i64 = 10 * 60;
/// Walking pace weighing distance against time when a shot with GPS picks
/// the party nearest to it in place and time (m/s).
const STRAY_WALK_MPS: f64 = 1.4;

/// Where an asset sits in story order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Track {
    /// The shared timeline: every non-split day, and the stretches of a
    /// split day before and after the parties are apart.
    Shared,
    /// One party's stretch of a split: `segment` numbers the split
    /// stretches of the trip, `rank` is the party's turn (0 = first told).
    Party { segment: u32, rank: u32 },
}

/// Evidence that two camera streams were apart.
#[derive(Debug, Clone, PartialEq)]
pub struct Apart {
    pub streams: (u32, u32),
    /// First and last matching time seen apart (unix seconds).
    pub start: i64,
    pub end: i64,
    /// Distinct fixes of each stream in the stretch.
    pub fixes: (usize, usize),
    /// Median distance between the two at matching times (m).
    pub median_m: f64,
}

/// One split stretch: parties told in turn between `start` and `end`.
#[derive(Debug, Clone, PartialEq)]
pub struct SplitStretch {
    /// Capture-local day (`YYYY-MM-DD`).
    pub day: String,
    pub start: i64,
    pub end: i64,
    /// Camera streams of each party, in the order the parties are told.
    pub parties: Vec<Vec<u32>>,
    pub apart: Vec<Apart>,
}

/// Story order of a set of assets.
#[derive(Debug, Clone, Default)]
pub struct StoryLayout {
    /// Indices into the assets, in story order.
    pub order: Vec<usize>,
    /// Track of each entry of `order`.
    pub tracks: Vec<Track>,
    /// Every split stretch found, in story order.
    pub splits: Vec<SplitStretch>,
    /// Camera stream per asset (input order), `None` without a counter.
    pub streams: Vec<Option<u32>>,
    /// The stream told first on a split day (most user captions).
    pub primary: Option<u32>,
}

/// The per-device counter in a file name: naming scheme and number.
/// `IMG_1620.HEIC`, `IMG_1620 (1).HEIC`, `IMG_1631_edited.heic` and
/// `IMG_E1620.HEIC` all give `("IMG_", 1620)`; `DSCF0042.JPG` gives
/// `("DSCF", 42)`. Names with a date or a UUID instead
/// (`IMG_20241005_101500.jpg`, `IMG-20241005-WA0001.jpg`,
/// `FullSizeRender.heic`) have no counter.
pub fn file_counter(file_name: &str) -> Option<(&'static str, u32)> {
    const SCHEMES: [(&str, &str); 7] = [
        ("IMG_E", "IMG_"),
        ("IMG_", "IMG_"),
        ("DSC_", "DSC_"),
        ("DSCF", "DSCF"),
        ("DSCN", "DSCN"),
        ("_DSC", "_DSC"),
        ("GOPR", "GOPR"),
    ];
    let upper = file_name.to_ascii_uppercase();
    SCHEMES.iter().find_map(|&(prefix, scheme)| {
        let rest = upper.strip_prefix(prefix)?;
        let digits = rest
            .get(..4)
            .filter(|d| d.bytes().all(|b| b.is_ascii_digit()))?;
        let ends = rest
            .as_bytes()
            .get(4)
            .is_none_or(|b| matches!(b, b'.' | b' ' | b'_' | b'(' | b'-'));
        if !ends {
            return None;
        }
        Some((scheme, digits.parse().ok()?))
    })
}

/// Camera stream of each asset (input order): assets in one stream came
/// from one device. Stream ids number the chains by their first capture.
/// Assets without a counter or a trusted capture time have none. See the
/// module docs for the chain rule. Deterministic.
pub fn camera_streams(assets: &[Asset]) -> Vec<Option<u32>> {
    struct Item<'a> {
        idx: usize,
        scheme: &'static str,
        n: u32,
        t: i64,
        id: &'a str,
    }
    let mut items: Vec<Item> = assets
        .iter()
        .enumerate()
        .filter_map(|(idx, a)| {
            let t = a.taken_at.filter(|_| !a.time_guessed)?;
            let name = a.path.file_name()?.to_string_lossy();
            let (scheme, n) = file_counter(&name)?;
            Some(Item {
                idx,
                scheme,
                n,
                t,
                id: &a.id,
            })
        })
        .collect();
    items.sort_by(|x, y| (x.t, x.n, x.id).cmp(&(y.t, y.n, y.id)));
    let mut by_scheme: BTreeMap<&str, Vec<(u32, usize)>> = BTreeMap::new();
    for (p, it) in items.iter().enumerate() {
        by_scheme.entry(it.scheme).or_default().push((it.n, p));
    }
    for list in by_scheme.values_mut() {
        list.sort_unstable();
    }
    // Candidate links (cost, later file, earlier file): cost is the
    // counter step in hours-equivalents plus the seconds between shots.
    let max_cost = i64::from(STREAM_MAX_STEP) * 3600;
    let mut links: Vec<(i64, usize, usize)> = vec![];
    for (i, it) in items.iter().enumerate() {
        let list = &by_scheme[it.scheme];
        let lo = i64::from(it.n) - i64::from(STREAM_MAX_STEP);
        let ranges = if lo >= 0 {
            [(lo as u32, it.n), (1, 0)]
        } else {
            [
                (0, it.n),
                ((i64::from(COUNTER_SPAN) + lo) as u32, COUNTER_SPAN - 1),
            ]
        };
        let mut cands: Vec<(i64, usize)> = vec![];
        for (from, to) in ranges.into_iter().filter(|(a, b)| a <= b) {
            let start = list.partition_point(|&(n, _)| n < from);
            for &(n, j) in list[start..].iter().take_while(|&&(n, _)| n <= to) {
                if j == i {
                    continue;
                }
                let step = (it.n + COUNTER_SPAN - n) % COUNTER_SPAN;
                let dt = it.t - items[j].t;
                let ok = if step == 0 {
                    j < i && dt.abs() <= STREAM_TWIN_SECS
                } else {
                    dt >= -STREAM_TIME_SLACK_SECS
                };
                let cost = i64::from(step) * 3600 + dt.abs();
                if ok && cost <= max_cost {
                    cands.push((cost, j));
                }
            }
        }
        cands.sort_unstable();
        links.extend(
            cands
                .into_iter()
                .take(STREAM_CANDIDATES)
                .map(|(c, j)| (c, i, j)),
        );
    }
    // Greedy matching, cheapest link first: each file has at most one
    // predecessor and one successor, and no link closes a loop.
    links.sort_unstable();
    let n = items.len();
    let mut pred: Vec<Option<usize>> = vec![None; n];
    let mut succ: Vec<Option<usize>> = vec![None; n];
    let mut root: Vec<usize> = (0..n).collect();
    fn find(root: &mut [usize], mut x: usize) -> usize {
        while root[x] != x {
            root[x] = root[root[x]];
            x = root[x];
        }
        x
    }
    for (_, i, j) in links {
        if pred[i].is_some() || succ[j].is_some() {
            continue;
        }
        let (ri, rj) = (find(&mut root, i), find(&mut root, j));
        if ri == rj {
            continue;
        }
        pred[i] = Some(j);
        succ[j] = Some(i);
        root[ri] = rj;
    }
    let mut out = vec![None; assets.len()];
    for (id, head) in (0..n).filter(|&h| pred[h].is_none()).enumerate() {
        let mut at = Some(head);
        while let Some(x) = at {
            out[items[x].idx] = Some(id as u32);
            at = succ[x];
        }
    }
    out
}

/// The stream with the most user captions (ties: most assets, then the
/// earliest): the trip owner's phone. `None` without streams.
fn primary_stream(assets: &[Asset], streams: &[Option<u32>]) -> Option<u32> {
    let mut tally: BTreeMap<u32, (usize, usize)> = BTreeMap::new();
    for (a, s) in assets.iter().zip(streams) {
        let Some(s) = s else { continue };
        let t = tally.entry(*s).or_default();
        t.0 += usize::from(has_caption(a.user_caption.as_deref()));
        t.1 += 1;
    }
    tally
        .into_iter()
        .max_by(|(sa, ta), (sb, tb)| ta.cmp(tb).then(sb.cmp(sa)))
        .map(|(s, _)| s)
}

/// One GPS fix of a stream.
#[derive(Debug, Clone, Copy)]
struct Fix {
    t: i64,
    gps: (f64, f64),
}

/// Where a stream (fixes in time order) was at `t`; see the module docs.
fn position(track: &[Fix], t: i64) -> Option<(f64, f64)> {
    let k = track.partition_point(|f| f.t <= t);
    let prev = k.checked_sub(1).map(|i| track[i]);
    let next = track.get(k).copied();
    if let Some(p) = prev.filter(|p| p.t == t) {
        return Some(p.gps);
    }
    if let (Some(p), Some(n)) = (prev, next) {
        if n.t - p.t <= MATCH_BRACKET_SECS {
            let f = (t - p.t) as f64 / (n.t - p.t) as f64;
            // Longitude the short way round, so a crossing of ±180° stays
            // near it instead of swinging through 0°.
            let dlon = (n.gps.1 - p.gps.1 + 540.0).rem_euclid(360.0) - 180.0;
            let lon = (p.gps.1 + f * dlon + 540.0).rem_euclid(360.0) - 180.0;
            return Some((p.gps.0 + f * (n.gps.0 - p.gps.0), lon));
        }
    }
    [prev, next]
        .into_iter()
        .flatten()
        .filter(|f| (f.t - t).abs() <= MATCH_NEAR_SECS)
        .min_by_key(|f| (f.t - t).abs())
        .map(|f| f.gps)
}

/// A distance between two streams at one of their capture times.
#[derive(Debug, Clone, Copy)]
struct Obs {
    t: i64,
    d: f64,
    /// Whose fix it was: 0 for the first stream of the pair, 1 the second.
    side: u8,
}

/// Every matching-time distance between two streams, in time order.
fn observe(x: &[Fix], y: &[Fix]) -> Vec<Obs> {
    let mut obs: Vec<Obs> = [(0u8, x, y), (1u8, y, x)]
        .into_iter()
        .flat_map(|(side, from, to)| {
            from.iter().filter_map(move |f| {
                Some(Obs {
                    t: f.t,
                    d: haversine_m(f.gps, position(to, f.t)?),
                    side,
                })
            })
        })
        .collect();
    obs.sort_by(|a, b| (a.t, a.side).cmp(&(b.t, b.side)).then(a.d.total_cmp(&b.d)));
    obs
}

/// Runs of apart observations (broken by any closer one) long and well
/// sampled enough to count: see [`MIN_APART_SECS`], [`MIN_APART_SAMPLES`].
fn apart_runs(streams: (u32, u32), obs: &[Obs]) -> Vec<Apart> {
    let mut out = vec![];
    let mut flush = |run: &mut Vec<Obs>| {
        if let (Some(first), Some(last)) = (run.first(), run.last()) {
            let fixes = |side: u8| {
                run.iter()
                    .filter(|o| o.side == side)
                    .map(|o| o.t)
                    .collect::<BTreeSet<_>>()
                    .len()
            };
            let fixes = (fixes(0), fixes(1));
            if last.t - first.t >= MIN_APART_SECS
                && fixes.0 >= MIN_APART_SAMPLES
                && fixes.1 >= MIN_APART_SAMPLES
            {
                let mut d: Vec<f64> = run.iter().map(|o| o.d).collect();
                d.sort_by(f64::total_cmp);
                out.push(Apart {
                    streams,
                    start: first.t,
                    end: last.t,
                    fixes,
                    median_m: d[d.len() / 2],
                });
            }
        }
        run.clear();
    };
    let mut run = vec![];
    for o in obs {
        if o.d >= APART_M {
            run.push(*o);
        } else {
            flush(&mut run);
        }
    }
    flush(&mut run);
    out
}

/// Each stream's own GPS track on one day: time order, copies dropped,
/// fixes implying more than [`crate::cluster::OUTLIER_SPEED_MPS`] from the stream's
/// previous kept fix dropped. Judged per stream: two phones 40 km apart
/// are not one phone teleporting.
fn day_tracks(assets: &[Asset], run: &[usize], streams: &[Option<u32>]) -> BTreeMap<u32, Vec<Fix>> {
    let mut tracks: BTreeMap<u32, Vec<Fix>> = BTreeMap::new();
    for &i in run {
        let a = &assets[i];
        if let (Some(s), Some(gps), Some(t)) = (streams[i], a.gps, a.taken_at) {
            if !a.time_guessed {
                tracks.entry(s).or_default().push(Fix { t, gps });
            }
        }
    }
    for fixes in tracks.values_mut() {
        fixes.sort_by(|a, b| {
            a.t.cmp(&b.t)
                .then(a.gps.0.total_cmp(&b.gps.0))
                .then(a.gps.1.total_cmp(&b.gps.1))
        });
        fixes.dedup_by(|b, a| a.t == b.t && a.gps == b.gps);
        let mut kept: Vec<Fix> = Vec::with_capacity(fixes.len());
        for f in fixes.drain(..) {
            if !kept
                .last()
                .is_some_and(|p| speed_jump(f.t - p.t, p.gps, f.gps))
            {
                kept.push(f);
            }
        }
        *fixes = kept;
    }
    tracks.retain(|_, f| f.len() >= MIN_APART_SAMPLES);
    tracks
}

/// Story order of `assets`: capture order ([`story_key`]) on every day,
/// with each split stretch told party by party (module docs).
/// Deterministic; every asset appears exactly once.
pub fn story_layout(assets: &[Asset]) -> StoryLayout {
    let mut timeline: Vec<usize> = (0..assets.len()).collect();
    timeline.sort_by(|&x, &y| story_key(&assets[x]).cmp(&story_key(&assets[y])));
    let streams = camera_streams(assets);
    let primary = primary_stream(assets, &streams);
    let mut layout = StoryLayout {
        order: Vec::with_capacity(assets.len()),
        tracks: Vec::with_capacity(assets.len()),
        splits: vec![],
        streams,
        primary,
    };
    // Runs of one capture-local day (undated assets sort last, untouched).
    let dated = timeline.partition_point(|&x| assets[x].taken_at.is_some());
    for run in timeline[..dated].chunk_by(|&x, &y| same_day(&assets[x], &assets[y])) {
        lay_out_day(assets, run, day_of(&assets[run[0]]), &mut layout);
    }
    layout.push_shared(&timeline[dated..]);
    layout
}

impl StoryLayout {
    /// Append `run` as told by the whole group.
    fn push_shared(&mut self, run: &[usize]) {
        self.order.extend(run);
        self.tracks
            .extend(std::iter::repeat_n(Track::Shared, run.len()));
    }
}

/// A party's shot in a split stretch: capture time, GPS, party turn.
type PartyShot = (i64, Option<(f64, f64)>, u32);

/// Append one day's run (time order) to `layout`, split party by party
/// where the parties were apart.
fn lay_out_day(assets: &[Asset], run: &[usize], day: String, layout: &mut StoryLayout) {
    let tracks = day_tracks(assets, run, &layout.streams);
    if tracks.len() < 2 {
        return layout.push_shared(run);
    }
    let ids: Vec<u32> = tracks.keys().copied().collect();
    let mut obs: BTreeMap<(u32, u32), Vec<Obs>> = BTreeMap::new();
    let mut apart: Vec<Apart> = vec![];
    for (k, &x) in ids.iter().enumerate() {
        for &y in &ids[k + 1..] {
            let o = observe(&tracks[&x], &tracks[&y]);
            apart.extend(apart_runs((x, y), &o));
            obs.insert((x, y), o);
        }
    }
    if apart.is_empty() {
        return layout.push_shared(run);
    }
    // Split stretches: overlapping apart runs merged.
    let mut spans: Vec<(i64, i64)> = apart.iter().map(|a| (a.start, a.end)).collect();
    spans.sort_unstable();
    let mut segments: Vec<(i64, i64)> = vec![];
    for (s, e) in spans {
        match segments.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => segments.push((s, e)),
        }
    }
    let time_of = |i: usize| assets[i].taken_at.unwrap_or(i64::MAX);
    // Per run position: (anchor position, turn, track).
    let mut keyed: Vec<(usize, u32, usize, Track)> = Vec::with_capacity(run.len());
    let mut placed = vec![false; run.len()];
    for &(start, end) in &segments {
        let segment = layout.splits.len() as u32;
        let evidence: Vec<Apart> = apart
            .iter()
            .filter(|a| a.start >= start && a.end <= end)
            .cloned()
            .collect();
        let parties = parties_in(&ids, &obs, &evidence, (start, end), layout.primary, |s| {
            run.iter()
                .copied()
                .filter(|&i| layout.streams[i] == Some(s) && (start..=end).contains(&time_of(i)))
                .map(time_of)
                .min()
        });
        let rank_of_stream: BTreeMap<u32, u32> = parties
            .iter()
            .enumerate()
            .flat_map(|(r, members)| members.iter().map(move |&s| (s, r as u32)))
            .collect();
        let limit = segments
            .iter()
            .find(|(s, _)| *s > end)
            .map_or(i64::MAX, |(s, _)| s - 1);
        let end = carry_end(&tracks, &rank_of_stream, end, limit);
        let window: Vec<usize> = (0..run.len())
            .filter(|&p| (start..=end).contains(&time_of(run[p])))
            .collect();
        let Some(&anchor) = window.first() else {
            continue;
        };
        // Party shots of the stretch, for placing everything else.
        let party_shots: Vec<PartyShot> = window
            .iter()
            .filter_map(|&p| {
                let i = run[p];
                let rank = layout.streams[i].and_then(|s| rank_of_stream.get(&s))?;
                Some((time_of(i), assets[i].gps, *rank))
            })
            .collect();
        let stray_rank = |i: usize| -> Option<u32> {
            let a = &assets[i];
            let t = time_of(i);
            if let Some(g) = a.gps {
                let cost = |&(pt, pg, _): &PartyShot| {
                    pg.map(|pg| (t - pt).abs() as f64 + haversine_m(g, pg) / STRAY_WALK_MPS)
                };
                if let Some((_, rank)) = party_shots
                    .iter()
                    .filter_map(|s| Some((cost(s)?, s.2)))
                    .min_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)))
                {
                    return Some(rank);
                }
            }
            party_shots
                .iter()
                .filter(|(pt, _, _)| (t - pt).abs() <= STRAY_JOIN_SECS)
                .min_by_key(|(pt, _, rank)| ((t - pt).abs(), *rank))
                .map(|s| s.2)
        };
        let turns = parties.len() as u32;
        for &p in &window {
            let i = run[p];
            let rank = layout.streams[i]
                .and_then(|s| rank_of_stream.get(&s).copied())
                .or_else(|| stray_rank(i));
            let track = rank.map_or(Track::Shared, |rank| Track::Party { segment, rank });
            // A shot no party claims plays after every party's turn.
            keyed.push((anchor, rank.unwrap_or(turns), p, track));
            placed[p] = true;
        }
        layout.splits.push(SplitStretch {
            day: day.clone(),
            start,
            end,
            parties,
            apart: evidence,
        });
    }
    for (p, _) in placed.iter().enumerate().filter(|(_, done)| !**done) {
        keyed.push((p, 0, p, Track::Shared));
    }
    keyed.sort_unstable_by_key(|&(anchor, turn, p, _)| (anchor, turn, p));
    for (_, _, p, track) in keyed {
        layout.order.push(run[p]);
        layout.tracks.push(track);
    }
}

/// Where a split stretch ends: its last apart sighting, carried forward
/// over each later fix (before `limit`) of a party's phones while it is
/// still [`APART_M`] from every other party's latest fix, at most
/// [`CARRY_SECS`] old.
fn carry_end(
    tracks: &BTreeMap<u32, Vec<Fix>>,
    rank_of_stream: &BTreeMap<u32, u32>,
    end: i64,
    limit: i64,
) -> i64 {
    let mut fixes: Vec<(Fix, u32)> = tracks
        .iter()
        .filter_map(|(s, f)| Some((f, *rank_of_stream.get(s)?)))
        .flat_map(|(f, r)| f.iter().map(move |x| (*x, r)))
        .collect();
    fixes.sort_by_key(|(f, r)| (f.t, *r));
    let mut latest: BTreeMap<u32, Fix> = BTreeMap::new();
    for (f, r) in fixes.iter().filter(|(f, _)| f.t <= end) {
        latest.insert(*r, *f);
    }
    let mut carried = end;
    for (f, r) in fixes.iter().filter(|(f, _)| f.t > end && f.t <= limit) {
        let apart = latest
            .iter()
            .filter(|(k, _)| *k != r)
            .all(|(_, o)| f.t - o.t <= CARRY_SECS && haversine_m(f.gps, o.gps) >= APART_M);
        if !apart {
            break;
        }
        carried = f.t;
        latest.insert(*r, *f);
    }
    carried
}

/// The parties of one split stretch, in the order they are told: streams
/// seen together merge (most shared sightings first) unless that would
/// join two streams seen apart; groups apart from another group are the
/// parties. The primary stream's party goes first, the rest by when they
/// first appear (`first_seen`). Streams in no party are left out: their
/// shots are placed one by one.
fn parties_in(
    ids: &[u32],
    obs: &BTreeMap<(u32, u32), Vec<Obs>>,
    apart: &[Apart],
    (start, end): (i64, i64),
    primary: Option<u32>,
    first_seen: impl Fn(u32) -> Option<i64>,
) -> Vec<Vec<u32>> {
    let apart_pairs: BTreeSet<(u32, u32)> = apart.iter().map(|a| a.streams).collect();
    let mut together: Vec<(usize, u32, u32)> = obs
        .iter()
        .filter(|(pair, _)| !apart_pairs.contains(pair))
        .map(|(&(x, y), o)| {
            let near = o
                .iter()
                .filter(|o| (start..=end).contains(&o.t) && o.d < APART_M)
                .count();
            (near, x, y)
        })
        .filter(|(near, _, _)| *near > 0)
        .collect();
    together.sort_by(|a, b| b.0.cmp(&a.0).then((a.1, a.2).cmp(&(b.1, b.2))));
    let mut group: BTreeMap<u32, u32> = ids.iter().map(|&s| (s, s)).collect();
    fn find(group: &BTreeMap<u32, u32>, mut s: u32) -> u32 {
        while group[&s] != s {
            s = group[&s];
        }
        s
    }
    for (_, x, y) in together {
        let (gx, gy) = (find(&group, x), find(&group, y));
        let clash = apart_pairs.iter().any(|&(p, q)| {
            let (gp, gq) = (find(&group, p), find(&group, q));
            (gp == gx && gq == gy) || (gp == gy && gq == gx)
        });
        if gx != gy && !clash {
            group.insert(gx.max(gy), gx.min(gy));
        }
    }
    let party_groups: BTreeSet<u32> = apart_pairs
        .iter()
        .flat_map(|&(p, q)| [find(&group, p), find(&group, q)])
        .collect();
    let mut parties: Vec<(bool, i64, Vec<u32>)> = party_groups
        .into_iter()
        .map(|g| {
            let members: Vec<u32> = ids
                .iter()
                .copied()
                .filter(|&s| find(&group, s) == g)
                .collect();
            let owner = primary.is_some_and(|p| members.contains(&p));
            let seen = members
                .iter()
                .filter_map(|&s| first_seen(s))
                .min()
                .unwrap_or(i64::MAX);
            (!owner, seen, members)
        })
        .collect();
    parties.sort();
    parties.into_iter().map(|(_, _, m)| m).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AssetKind;

    /// 2026-10-18 09:00 UTC+8 as unix seconds, plus `mins`.
    fn at(mins: i64) -> i64 {
        1_792_285_200 + mins * 60
    }

    fn shot(name: &str, mins: i64, gps: Option<(f64, f64)>) -> Asset {
        let mut a = Asset::new(name, format!("/trip/{name}").into(), AssetKind::Photo);
        a.taken_at = Some(at(mins));
        a.tz_offset_secs = Some(8 * 3600);
        a.gps = gps;
        a
    }

    fn streams_by_name(assets: &[Asset]) -> BTreeMap<String, Option<u32>> {
        assets
            .iter()
            .map(|a| a.id.clone())
            .zip(camera_streams(assets))
            .collect()
    }

    #[test]
    fn counters_come_from_camera_file_names() {
        for (name, want) in [
            ("IMG_1620.HEIC", Some(("IMG_", 1620))),
            ("IMG_1620 (1).HEIC", Some(("IMG_", 1620))),
            ("IMG_1631_edited.heic", Some(("IMG_", 1631))),
            ("img_0007.mov", Some(("IMG_", 7))),
            ("IMG_E1620.HEIC", Some(("IMG_", 1620))),
            ("DSCF0042.JPG", Some(("DSCF", 42))),
            ("IMG_1620", Some(("IMG_", 1620))),
            ("IMG_20241005_101500.jpg", None),
            ("IMG-20241005-WA0001.jpg", None),
            ("FullSizeRender.heic", None),
            (
                "81424954837__3DE3CC9B-BD74-4986-9188-C3AA35660DBC.heic",
                None,
            ),
            ("E5B14CFD-19EB-4E93-BC67-D9178E414418.png", None),
            ("IMG_123.jpg", None),
        ] {
            assert_eq!(file_counter(name), want, "{name}");
        }
    }

    #[test]
    fn phones_whose_counters_overlap_at_different_times_are_two_streams() {
        // Phone A shoots 7160..7166 on days 0-1 and has moved on to 7300+
        // by day 3; phone B reaches the same numbers on day 17.
        let day = 24 * 60;
        let mut assets = vec![
            shot("IMG_7160.HEIC", 0, None),
            shot("IMG_7163.HEIC", 30, None),
            shot("IMG_7166.HEIC", day, None),
            shot("IMG_7300.HEIC", 3 * day, None),
            shot("IMG_7301.HEIC", 3 * day + 5, None),
            shot("IMG_7111.HEIC", 15 * day, None),
            shot("IMG_7162.HEIC", 17 * day + 2, None),
            shot("IMG_7164.HEIC", 17 * day + 9, None),
        ];
        // An unrelated, counterless file.
        assets.push(shot("FullSizeRender.heic", 3, None));
        let s = streams_by_name(&assets);
        let a = s["IMG_7160.HEIC"];
        let b = s["IMG_7111.HEIC"];
        assert!(a.is_some() && b.is_some() && a != b);
        for n in [
            "IMG_7163.HEIC",
            "IMG_7166.HEIC",
            "IMG_7300.HEIC",
            "IMG_7301.HEIC",
        ] {
            assert_eq!(s[n], a, "{n}");
        }
        for n in ["IMG_7162.HEIC", "IMG_7164.HEIC"] {
            assert_eq!(s[n], b, "{n}");
        }
        assert_eq!(s["FullSizeRender.heic"], None);
    }

    #[test]
    fn streams_survive_rollover_twins_and_clip_dates() {
        let assets = vec![
            shot("IMG_9998.HEIC", 0, None),
            shot("IMG_9999.HEIC", 1, None),
            shot("IMG_0001.HEIC", 2, None),
            // A copy and an edited version of one shot.
            shot("IMG_0001 (1).HEIC", 2, None),
            shot("IMG_0002_edited.heic", 3, None),
            // The clip is stamped a little before the photo numbered below it.
            shot("IMG_0004.MOV", 5, None),
            shot("IMG_0003.JPG", 6, None),
            shot("IMG_0005.HEIC", 9, None),
            // Another phone, numbered far away.
            shot("IMG_5000.HEIC", 1, None),
            shot("IMG_5001.HEIC", 4, None),
        ];
        let s = streams_by_name(&assets);
        let a = s["IMG_9998.HEIC"];
        for (name, stream) in &s {
            let want = if name.starts_with("IMG_5") {
                s["IMG_5000.HEIC"]
            } else {
                a
            };
            assert_eq!(*stream, want, "{name}");
        }
        assert_ne!(s["IMG_5000.HEIC"], a);
        // Ids number chains by first capture.
        assert_eq!(a, Some(0));
    }

    const CITY: (f64, f64) = (22.282, 114.154);
    const PARK: (f64, f64) = (22.313, 114.043);
    const SHIP: (f64, f64) = (22.294, 114.168);

    /// A Hong Kong day: everyone on the ship at breakfast; the owner's
    /// phone (1xxx, captioned) and 7xxx tour the city while 5xxx and 6xxx
    /// spend the day at the park; everyone back on the ship at night.
    fn hong_kong() -> Vec<Asset> {
        let mut v = vec![];
        let mut add = |name: &str, mins: i64, gps| v.push(shot(name, mins, Some(gps)));
        add("IMG_1500.HEIC", -60, SHIP);
        add("IMG_5800.HEIC", -58, SHIP);
        add("IMG_1501.HEIC", -50, SHIP);
        for k in 0..8 {
            let m = k * 50;
            add(&format!("IMG_{}.HEIC", 1502 + k), m, CITY);
            add(&format!("IMG_{}.HEIC", 5801 + k), m + 5, PARK);
            add(&format!("IMG_{}.HEIC", 6801 + k), m + 12, PARK);
            add(&format!("IMG_{}.HEIC", 7201 + k), m + 20, CITY);
        }
        add("IMG_1520.HEIC", 600, SHIP);
        add("IMG_5820.HEIC", 605, SHIP);
        v[0].user_caption = Some("Breakfast before the split".into());
        v
    }

    fn names(assets: &[Asset], order: &[usize]) -> Vec<String> {
        order.iter().map(|&i| assets[i].id.clone()).collect()
    }

    #[test]
    fn a_split_day_is_told_party_by_party() {
        let assets = hong_kong();
        let layout = story_layout(&assets);
        assert_eq!(layout.splits.len(), 1, "{:?}", layout.splits);
        let order = names(&assets, &layout.order);
        let pos = |n: &str| order.iter().position(|o| o == n).unwrap();
        // Breakfast first, the owner's city tour, then the park, then night.
        assert!(pos("IMG_1501.HEIC") < pos("IMG_1502.HEIC"));
        let city_last = ["IMG_1509.HEIC", "IMG_7208.HEIC"]
            .map(pos)
            .into_iter()
            .max()
            .unwrap();
        let park_first = ["IMG_5801.HEIC", "IMG_6801.HEIC"]
            .map(pos)
            .into_iter()
            .min()
            .unwrap();
        assert!(city_last < park_first, "{order:?}");
        let park_last = ["IMG_5808.HEIC", "IMG_6808.HEIC"]
            .map(pos)
            .into_iter()
            .max()
            .unwrap();
        assert!(park_last < pos("IMG_1520.HEIC"));
        // Inside a party, time order.
        assert!(pos("IMG_1502.HEIC") < pos("IMG_7201.HEIC"));
        assert!(pos("IMG_7201.HEIC") < pos("IMG_1503.HEIC"));
        // Two parties: the owner's phone with 7xxx, then 5xxx with 6xxx.
        let s = &layout.streams;
        let stream_of = |n: &str| s[assets.iter().position(|a| a.id == n).unwrap()].unwrap();
        assert_eq!(
            layout.splits[0].parties,
            vec![
                vec![stream_of("IMG_1500.HEIC"), stream_of("IMG_7201.HEIC")],
                vec![stream_of("IMG_5800.HEIC"), stream_of("IMG_6801.HEIC")],
            ]
        );
        let mut sorted = layout.order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..assets.len()).collect::<Vec<_>>());
    }

    #[test]
    fn streamless_shots_join_the_party_nearest_in_place_and_time() {
        let mut assets = hong_kong();
        // A UUID-named shot at the park; one without GPS a minute after a
        // city shot; one without GPS and no party shot within 10 minutes.
        assets.push(shot("0C81E648.jpg", 156, Some(PARK)));
        assets.push(shot("FullSizeRender.heic", 101, None));
        assets.push(shot("81424954837__X.heic", 35, None));
        let layout = story_layout(&assets);
        let at_pos = |n: &str| {
            let i = assets.iter().position(|a| a.id == n).unwrap();
            layout.order.iter().position(|&o| o == i).unwrap()
        };
        let track = |n: &str| layout.tracks[at_pos(n)];
        assert_eq!(
            track("0C81E648.jpg"),
            Track::Party {
                segment: 0,
                rank: 1
            }
        );
        assert_eq!(
            track("FullSizeRender.heic"),
            Track::Party {
                segment: 0,
                rank: 0
            }
        );
        // The unclaimed one plays after both parties, before the evening.
        assert_eq!(track("81424954837__X.heic"), Track::Shared);
        assert!(at_pos("81424954837__X.heic") > at_pos("IMG_6808.HEIC"));
        assert!(at_pos("81424954837__X.heic") < at_pos("IMG_1520.HEIC"));
    }

    #[test]
    fn days_without_a_sustained_split_keep_capture_order() {
        let plain = |assets: &[Asset]| {
            let mut t: Vec<usize> = (0..assets.len()).collect();
            t.sort_by(|&x, &y| story_key(&assets[x]).cmp(&story_key(&assets[y])));
            t
        };
        // Everyone together all day.
        let together: Vec<Asset> = hong_kong()
            .into_iter()
            .map(|mut a| {
                a.gps = Some(CITY);
                a
            })
            .collect();
        // Apart, but only for 30 minutes; and apart with one fix only.
        let mut brief = vec![];
        for k in 0..4 {
            brief.push(shot(&format!("IMG_{}.HEIC", 1000 + k), k * 10, Some(CITY)));
            brief.push(shot(
                &format!("IMG_{}.HEIC", 5000 + k),
                k * 10 + 2,
                Some(PARK),
            ));
        }
        let mut thin = vec![
            shot("IMG_1000.HEIC", 0, Some(CITY)),
            shot("IMG_1001.HEIC", 60, Some(CITY)),
            shot("IMG_1002.HEIC", 120, Some(CITY)),
            shot("IMG_5000.HEIC", 61, Some(PARK)),
        ];
        thin.push(shot("IMG_5001.HEIC", 300, Some(CITY)));
        for assets in [together, brief, thin] {
            let layout = story_layout(&assets);
            assert!(layout.splits.is_empty(), "{:?}", layout.splits);
            assert_eq!(layout.order, plain(&assets));
            assert!(layout.tracks.iter().all(|t| *t == Track::Shared));
        }
    }

    /// xorshift64: deterministic pseudo-random numbers for the property test.
    fn rng(seed: u64) -> impl FnMut(u64) -> u64 {
        let mut s = seed.max(1);
        move |n| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s % n
        }
    }

    #[test]
    fn random_multi_phone_days_place_every_shot_once() {
        for seed in 1..=60u64 {
            let mut r = rng(seed * 7919);
            let phones = 2 + r(4) as usize;
            // Each phone: a counter base, and (on split runs) a far place.
            let split = seed % 2 == 0;
            let mut assets = vec![];
            for p in 0..phones {
                let base = 1000 * (p as i64 + 1) + r(900) as i64;
                let far = split && p % 2 == 1;
                let mut mins = r(60) as i64;
                for k in 0..(3 + r(25) as i64) {
                    mins += 1 + r(40) as i64;
                    let jitter = r(100) as f64 * 1e-5;
                    let gps = match r(10) {
                        0 => None,
                        _ if far => Some((PARK.0 + jitter, PARK.1)),
                        _ => Some((CITY.0 + jitter, CITY.1)),
                    };
                    let name = match r(15) {
                        0 => format!("{seed}-{p}-{k}.png"),
                        _ => format!("IMG_{:04}.HEIC", (base + k) % 10_000),
                    };
                    let mut a = shot(&name, mins.min(14 * 60), gps);
                    a.id = format!("{p}-{k}");
                    assets.push(a);
                }
            }
            let layout = story_layout(&assets);
            let mut seen = layout.order.clone();
            seen.sort_unstable();
            assert_eq!(seen, (0..assets.len()).collect::<Vec<_>>(), "seed {seed}");
            assert_eq!(layout.tracks.len(), assets.len());
            // Deterministic, whatever the input order.
            let mut shuffled = assets.clone();
            shuffled.reverse();
            let again = story_layout(&shuffled);
            assert_eq!(
                names(&assets, &layout.order),
                names(&shuffled, &again.order),
                "seed {seed}"
            );
            if !split {
                let mut plain: Vec<usize> = (0..assets.len()).collect();
                plain.sort_by(|&x, &y| story_key(&assets[x]).cmp(&story_key(&assets[y])));
                assert_eq!(layout.order, plain, "seed {seed}");
            }
        }
    }

    #[test]
    fn positions_interpolate_across_the_antimeridian() {
        let fix = |t, lon| Fix { t, gps: (0.0, lon) };
        let track = [fix(0, 179.0), fix(100, -179.0)];
        let (_, lon) = position(&track, 50).unwrap();
        assert!(lon.abs() > 179.9, "{lon}");
        let (_, lon) = position(&[fix(0, 10.0), fix(100, 20.0)], 50).unwrap();
        assert!((lon - 15.0).abs() < 1e-9, "{lon}");
    }
}
