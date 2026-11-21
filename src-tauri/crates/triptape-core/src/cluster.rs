//! Stage 6 — Cluster: time-gap + GPS day/place clustering.
//!
//! Pipeline: sort into story order (capture time; a day the group split up
//! is told party by party, see [`crate::party`]), drop GPS outliers whose
//! implied speed from the previous fix exceeds 1,000 km/h, then group into
//! clusters — a new cluster starts on calendar-day change, on a time gap
//! over [`TIME_GAP_SECS`], when both fixes have GPS and drift more than
//! [`PLACE_SPLIT_METERS`] from the running centroid, or where one party's
//! stretch of a split day hands over to the next party's. Near-dupes
//! collapse to a best-shot pick via pHash Hamming distance.

use crate::geo::haversine_m;
use crate::hash::is_near_dupe;
use crate::party::Track;
use crate::types::{Asset, AssetKind, Cluster, Score};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// A time gap this large always starts a new cluster (4 h).
pub const TIME_GAP_SECS: i64 = 4 * 3600;
/// Drift from the running GPS centroid that starts a new place (5 km).
pub const PLACE_SPLIT_METERS: f64 = 5_000.0;
/// Implied speed that marks a GPS fix an outlier (1,000 km/h in m/s).
pub const OUTLIER_SPEED_MPS: f64 = 1_000_000.0 / 3600.0;

/// Whether moving `from` -> `to` in `dt_secs` implies more than
/// [`OUTLIER_SPEED_MPS`]. A zero or negative delta judges nothing.
pub fn speed_jump(dt_secs: i64, from: (f64, f64), to: (f64, f64)) -> bool {
    dt_secs > 0 && haversine_m(from, to) / dt_secs as f64 > OUTLIER_SPEED_MPS
}
/// Max capture-time gap between two shots for them to count as near-dupes
/// (a burst / retake window, 10 min).
pub const DUPE_WINDOW_SECS: i64 = 10 * 60;
/// Max GPS distance between two near-dupe shots when both have a fix (200 m).
pub const DUPE_RADIUS_METERS: f64 = 200.0;

/// Clusters with at most this many assets are merged into a neighbour on
/// the same local day: a one-photo chapter (a hotel lobby, a bus window)
/// would otherwise get its own guaranteed shot and a title card of its own.
pub const MERGE_MAX_ASSETS: usize = 3;
/// Walking pace used to weigh GPS distance against time when picking which
/// neighbour a small cluster joins (m/s): 50 m counts as ~36 s apart.
const MERGE_WALK_MPS: f64 = 1.4;

/// Calendar day (`YYYY-MM-DD`) in the photo's own timezone, or
/// `"unknown-date"`. An 8 AM Japan photo is 11 PM UTC the day before —
/// day grouping must be local or mornings land on the wrong day.
pub fn day_key(taken_at: Option<i64>, tz_offset_secs: Option<i32>) -> String {
    taken_at
        .and_then(|t| crate::metadata::local_datetime(t, tz_offset_secs))
        .map(|d| d.strftime("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown-date".to_string())
}

/// An asset's capture-local day ([`day_key`]).
pub fn day_of(a: &Asset) -> String {
    day_key(a.taken_at, a.tz_offset_secs)
}

/// Whether two assets share a capture-local day ([`day_key`]); an unknown
/// date matches nothing.
pub fn same_day(a: &Asset, b: &Asset) -> bool {
    a.taken_at.is_some() && day_of(a) == day_of(b)
}

/// Capture-time sort key: capture time (unknown last), then id. This is
/// story order on every day the group stayed together, and the order
/// within each party's stretch of a split day; [`story_order`] is the
/// whole story order.
pub fn story_key(a: &Asset) -> (i64, &str) {
    (a.taken_at.unwrap_or(i64::MAX), &a.id)
}

/// Assets in story order: capture time (unknown last), then id, except
/// that a day the group split up tells each party's stretch in turn
/// ([`crate::party::story_layout`]). Deterministic.
pub fn story_order(assets: &[Asset]) -> Vec<&Asset> {
    story_tracks(assets).into_iter().map(|(a, _)| a).collect()
}

/// [`story_order`] with each asset's [`Track`]: the shared timeline, or
/// one party's stretch of a split day.
pub fn story_tracks(assets: &[Asset]) -> Vec<(&Asset, Track)> {
    let layout = crate::party::story_layout(assets);
    layout
        .order
        .iter()
        .zip(&layout.tracks)
        .map(|(&i, &t)| (&assets[i], t))
        .collect()
}

/// Asset id -> position in [`story_order`].
pub fn story_positions(assets: &[Asset]) -> HashMap<&str, usize> {
    story_order(assets)
        .into_iter()
        .enumerate()
        .map(|(i, a)| (a.id.as_str(), i))
        .collect()
}

/// Ids of assets whose GPS fix implies travel faster than 1,000 km/h from
/// the previous accepted fix. The later fix is dropped (kept for time
/// clustering, ignored for place/route math). `ordered` must be in story
/// order ([`story_tracks`]); fixes without a usable time delta are
/// accepted unjudged, and so is the first fix of each party's stretch: one
/// party's last fix is not where the next party set off from.
pub fn gps_outliers_in(ordered: &[(&Asset, Track)]) -> HashSet<String> {
    let mut bad = HashSet::new();
    let mut prev: Option<(Option<i64>, (f64, f64))> = None;
    let mut track = Track::Shared;
    for &(a, t) in ordered {
        if t != track {
            track = t;
            prev = None;
        }
        let Some(g) = a.gps else { continue };
        let Some((pt, pg)) = prev else {
            prev = Some((a.taken_at, g));
            continue;
        };
        if let (Some(t), Some(pt)) = (a.taken_at, pt) {
            if speed_jump((t - pt).abs(), pg, g) {
                bad.insert(a.id.clone());
                continue;
            }
        }
        prev = Some((a.taken_at, g));
    }
    bad
}

/// See [`gps_outliers_in`]; sorts `assets` into story order first.
pub fn gps_outliers(assets: &[Asset]) -> HashSet<String> {
    gps_outliers_in(&story_tracks(assets))
}

/// Running state for the cluster being built; O(1) per pushed asset.
#[derive(Default)]
struct Acc {
    ids: Vec<String>,
    day: String,
    start: Option<i64>,
    end: Option<i64>,
    gps_n: usize,
    gps_sum: (f64, f64),
    /// The split-day party whose stretch this cluster holds, if any: a
    /// cluster never holds two parties.
    party: Option<Track>,
}

impl Acc {
    fn centroid(&self) -> Option<(f64, f64)> {
        let n = self.gps_n as f64;
        (self.gps_n > 0).then(|| (self.gps_sum.0 / n, self.gps_sum.1 / n))
    }

    fn push(&mut self, id: &str, t: Option<i64>, gps: Option<(f64, f64)>, track: Track) {
        self.ids.push(id.to_string());
        if track != Track::Shared {
            self.party = Some(track);
        }
        if let Some(t) = t {
            self.start = Some(self.start.map_or(t, |s| s.min(t)));
            self.end = Some(self.end.map_or(t, |e| e.max(t)));
        }
        if let Some(g) = gps {
            self.gps_n += 1;
            self.gps_sum.0 += g.0;
            self.gps_sum.1 += g.1;
        }
    }

    /// Fold the next cluster in story order into this one.
    fn absorb(&mut self, next: Acc) {
        self.ids.extend(next.ids);
        self.start = match (self.start, next.start) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self.end = match (self.end, next.end) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        self.gps_n += next.gps_n;
        self.gps_sum.0 += next.gps_sum.0;
        self.gps_sum.1 += next.gps_sum.1;
        self.party = self.party.or(next.party);
    }

    /// Whether `other` may join this cluster: same day, and the same
    /// party of a split day (or neither holds one), so a party's small
    /// cluster never lands in another party's or the shared timeline's.
    fn can_merge(&self, other: &Acc) -> bool {
        self.day == other.day && self.party == other.party
    }

    /// How far apart two clusters are: time gap plus GPS distance at
    /// walking pace. A missing time or fix contributes nothing.
    fn distance_secs(&self, other: &Acc) -> f64 {
        let gap = match (self.start, self.end, other.start, other.end) {
            (Some(s1), Some(e1), Some(s2), Some(e2)) => (s2 - e1).max(s1 - e2).max(0) as f64,
            _ => 0.0,
        };
        let walk = match (self.centroid(), other.centroid()) {
            (Some(a), Some(b)) => haversine_m(a, b) / MERGE_WALK_MPS,
            _ => 0.0,
        };
        gap + walk
    }

    /// Push the accumulated cluster (if any) and reset for `next_day`.
    fn flush(&mut self, out: &mut Vec<Acc>, next_day: String) {
        let acc = std::mem::replace(
            self,
            Acc {
                day: next_day,
                ..Default::default()
            },
        );
        if !acc.ids.is_empty() {
            out.push(acc);
        }
    }
}

/// Merge every cluster of at most [`MERGE_MAX_ASSETS`] assets into the
/// adjacent cluster on the same local day that is closer in time and place.
/// Smallest clusters go first (earliest on ties), and a merge that is still
/// small keeps merging, so a run of tiny clusters collapses into a
/// neighbour. Clusters never merge across days, nor out of one party's
/// stretch of a split day. The merged cluster keeps the
/// larger side's centroid, since the GPS sums are dominated by it.
fn merge_small(mut v: Vec<Acc>) -> Vec<Acc> {
    let neighbour = |v: &[Acc], i: usize| -> Option<usize> {
        [i.checked_sub(1), Some(i + 1)]
            .into_iter()
            .flatten()
            .filter(|&j| j < v.len() && v[i].can_merge(&v[j]))
            .min_by(|&a, &b| {
                v[i].distance_secs(&v[a])
                    .total_cmp(&v[i].distance_secs(&v[b]))
                    .then(a.cmp(&b))
            })
    };
    loop {
        let pick = (0..v.len())
            .filter(|&i| v[i].ids.len() <= MERGE_MAX_ASSETS)
            .filter_map(|i| neighbour(&v, i).map(|n| (i, n)))
            .min_by_key(|&(i, _)| (v[i].ids.len(), i));
        let Some((i, n)) = pick else { return v };
        let (lo, hi) = (i.min(n), i.max(n));
        let later = v.remove(hi);
        v[lo].absorb(later);
    }
}

/// Finish clusters: sequential ids and `day · place N` labels.
fn finish(accs: Vec<Acc>) -> Vec<Cluster> {
    accs.into_iter()
        .enumerate()
        .map(|(i, acc)| {
            let seq = i + 1;
            let center = acc.centroid();
            let place = if center.is_some() {
                format!(" · place {seq}")
            } else {
                String::new()
            };
            Cluster {
                id: format!("c{seq:03}"),
                asset_ids: acc.ids,
                start_time: acc.start,
                end_time: acc.end,
                centroid_gps: center,
                label: format!("{}{place}", acc.day),
            }
        })
        .collect()
}

/// Group assets into day/place clusters in story order, then merge tiny
/// clusters into a same-day neighbour ([`MERGE_MAX_ASSETS`]). On a split
/// day each party's stretch gets clusters of its own.
pub fn cluster_assets(assets: &[Asset]) -> Vec<Cluster> {
    cluster_tracks(story_tracks(assets))
}

/// Clusters as if no day were split (capture order, one shared timeline),
/// with tiny clusters left unmerged. A split day's chapters are fewer and
/// larger than these: the planner weighs a split day by them so it keeps
/// the share of the film it had before it was told party by party.
pub fn cluster_by_capture<'a>(assets: impl IntoIterator<Item = &'a Asset>) -> Vec<Cluster> {
    let mut ordered: Vec<&Asset> = assets.into_iter().collect();
    ordered.sort_by(|a, b| story_key(a).cmp(&story_key(b)));
    finish(split_clusters(
        ordered.into_iter().map(|a| (a, Track::Shared)).collect(),
    ))
}

fn cluster_tracks(ordered: Vec<(&Asset, Track)>) -> Vec<Cluster> {
    finish(merge_small(split_clusters(ordered)))
}

/// Start a new cluster on day change, time gap, place drift or party
/// handover, before any merging.
fn split_clusters(ordered: Vec<(&Asset, Track)>) -> Vec<Acc> {
    let outliers = gps_outliers_in(&ordered);

    let mut clusters: Vec<Acc> = vec![];
    let mut acc = Acc::default();
    for (a, track) in ordered {
        let day = day_of(a);
        let gps = a.gps.filter(|_| !outliers.contains(&a.id));
        if acc.ids.is_empty() {
            acc.day = day;
        } else {
            let day_changed = day != acc.day;
            let gap = matches!((a.taken_at, acc.end), (Some(t), Some(m)) if t - m > TIME_GAP_SECS);
            let moved = matches!(
                (gps, acc.centroid()),
                (Some(g), Some(c)) if haversine_m(c, g) > PLACE_SPLIT_METERS
            );
            let other_party = matches!(acc.party, Some(p) if track != Track::Shared && track != p);
            if day_changed || gap || moved || other_party {
                acc.flush(&mut clusters, day);
            }
        }
        acc.push(&a.id, a.taken_at, gps, track);
    }
    acc.flush(&mut clusters, String::new());
    clusters
}

/// One near-dupe stack: members plus the best-shot pick.
#[derive(Debug, Clone)]
pub struct DupeGroup {
    pub members: Vec<String>,
    pub best_id: String,
}

/// True when two shots are close enough in time (and place, if both have
/// GPS) to be retakes of one moment. Unknown capture time never qualifies:
/// keeping a duplicate is cheaper than dropping a distinct moment.
fn same_moment(a: &Asset, b: &Asset) -> bool {
    let close_in_time = matches!(
        (a.taken_at, b.taken_at),
        (Some(x), Some(y)) if (x - y).abs() <= DUPE_WINDOW_SECS
    );
    let close_in_place = match (a.gps, b.gps) {
        (Some(g), Some(h)) => haversine_m(g, h) <= DUPE_RADIUS_METERS,
        _ => true,
    };
    close_in_time && close_in_place
}

/// Whether two stills look like retakes of one shot: near pHash, or (when
/// both carry an image embedding and tag the same people) embeddings at
/// least [`crate::embedding::NEAR_DUPE_COSINE`] alike. Missing embeddings
/// leave the pHash rule as it was. The people check keeps "the same view
/// with someone else in it" apart, as the best-of pick does.
/// Videos never look alike (see [`collapse_near_dupes`]).
fn looks_alike(a: &Asset, b: &Asset) -> bool {
    if a.kind == AssetKind::Video || b.kind == AssetKind::Video {
        return false;
    }
    matches!((a.phash, b.phash), (Some(p), Some(q)) if is_near_dupe(p, q))
        || (a.people_sig == b.people_sig
            && crate::embedding::alike(
                a.embedding.as_ref(),
                b.embedding.as_ref(),
                crate::embedding::NEAR_DUPE_COSINE,
            ))
}

/// Collapse near-dupes ([`looks_alike`]: pHash distance ≤ threshold or a
/// retake by embedding, captured within [`DUPE_WINDOW_SECS`] and, when
/// both have GPS, [`DUPE_RADIUS_METERS`]) with union-find; groups merge
/// only when every cross pair is the same moment, so a whole group stays
/// within the window and radius. The best shot is the highest `total`
/// score (ties → first in story order). Byte-identical files (same
/// content hash) always collapse, clips included. Otherwise videos never
/// collapse: their pHash is only the poster frame, so two clips (or a
/// clip and a photo) opening on the same view can still hold different
/// footage. An edit and its original ([`edit_families`]) always collapse
/// too, and the edit is always the kept one, whatever the scores.
pub fn collapse_near_dupes(assets: &[Asset], scores: &HashMap<String, Score>) -> Vec<DupeGroup> {
    let n = assets.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(p: &mut [usize], mut x: usize) -> usize {
        while p[x] != x {
            p[x] = p[p[x]];
            x = p[x];
        }
        x
    }
    /// Join root `from` into root `into`, moving its member list along.
    fn union(p: &mut [usize], members: &mut [Vec<usize>], into: usize, from: usize) {
        p[from] = into;
        let moved = std::mem::take(&mut members[from]);
        members[into].extend(moved);
    }
    let alike = |x: usize, y: usize| looks_alike(&assets[x], &assets[y]);
    let mut members: Vec<Vec<usize>> = (0..n).map(|i| vec![i]).collect();
    // Byte-identical files are one item whatever their kind: the same
    // photo or clip added to the library twice. They join first, with no
    // look or moment test, so a clip's twin is collapsed too.
    for twins in identical_files(assets) {
        let root = twins[0];
        for &t in &twins[1..] {
            union(&mut parent, &mut members, root, t);
        }
    }
    // An edit and its original are one item too, whatever their bytes,
    // timestamps or pixels (a crop or trim changes all three); the edit
    // is the one kept, below.
    let mut is_edit = vec![false; n];
    for (edits, originals) in edit_families(assets) {
        for &e in &edits {
            is_edit[e] = true;
        }
        let root = find(&mut parent, edits[0]);
        for &m in edits[1..].iter().chain(&originals) {
            let r = find(&mut parent, m);
            if r != root {
                union(&mut parent, &mut members, root, r);
            }
        }
    }
    for i in 0..n {
        for j in (i + 1)..n {
            // The cheap time/place test gates the (embedding) look test;
            // complete linkage below requires it for (i, j) anyway.
            if !same_moment(&assets[i], &assets[j]) || !alike(i, j) {
                continue;
            }
            let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
            // Complete linkage: merge only if every cross pair is the
            // same moment, so chains (0, 9, 18 min) can't span the
            // window or radius as a whole.
            if ri != rj
                && members[ri].iter().all(|&x| {
                    members[rj]
                        .iter()
                        .all(|&y| same_moment(&assets[x], &assets[y]) && alike(x, y))
                })
            {
                union(&mut parent, &mut members, ri, rj);
            }
        }
    }
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        groups.entry(r).or_default().push(i);
    }
    let score_of = |i: usize| scores.get(&assets[i].id).map(|s| s.total).unwrap_or(0.5);
    let mut out: Vec<DupeGroup> = groups
        .into_values()
        .map(|idx| {
            // "I want to keep my edits": an edit of an original beats any
            // score; among edits (or with none), the best score wins.
            let mut best = idx[0];
            for &m in &idx[1..] {
                let better = (is_edit[m], score_of(m)) > (is_edit[best], score_of(best));
                if better {
                    best = m;
                }
            }
            let members: Vec<String> = idx.iter().map(|&i| assets[i].id.clone()).collect();
            DupeGroup {
                members,
                best_id: assets[best].id.clone(),
            }
        })
        .collect();
    out.sort_by(|a, b| a.best_id.cmp(&b.best_id));
    out
}

/// Indices of assets that share a content hash, one list per file that
/// appears more than once, in input order. Unhashed assets are never twins.
fn identical_files(assets: &[Asset]) -> Vec<Vec<usize>> {
    let mut by_hash: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, a) in assets.iter().enumerate() {
        if let Some(h) = a.content_hash.as_deref() {
            by_hash.entry(h).or_default().push(i);
        }
    }
    let mut twins: Vec<Vec<usize>> = by_hash.into_values().filter(|v| v.len() > 1).collect();
    twins.sort();
    twins
}

/// The original's stem behind a file name, lowercased, and whether the
/// name is an edit. Apple Photos / osxphotos export an edit as
/// `IMG_1234_edited.heic` next to `IMG_1234.HEIC`, and number copies of
/// either with a trailing ` (n)` (`IMG_1234 (1).HEIC`,
/// `IMG_1234_edited (1).heic`), so both forms map to `img_1234`.
pub fn edit_stem(file_name: &str) -> (String, bool) {
    fn strip_copy(s: &str) -> &str {
        // ` (n)` with n all digits, at the very end.
        if let Some(open) = s.strip_suffix(')').and_then(|r| r.rfind(" (")) {
            let n = &s[open + 2..s.len() - 1];
            if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) {
                return &s[..open];
            }
        }
        s
    }
    let stem = Path::new(file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(file_name);
    let stem = strip_copy(stem).to_lowercase();
    match stem.strip_suffix("_edited") {
        Some(base) if !base.is_empty() => (strip_copy(base).to_string(), true),
        _ => (stem, false),
    }
}

/// Indices of one original's edits, then of its originals.
type EditFamily = (Vec<usize>, Vec<usize>);

/// Edits that have an original beside them: for each (folder, original
/// stem, kind family) holding at least one edit and one non-edit, the
/// indices of its edits and of its originals, in input order. Families
/// are stills (photos and Live Photos) and videos, so a Live Photo's edit
/// never joins a same-named clip. Different folders never join.
fn edit_families(assets: &[Asset]) -> Vec<EditFamily> {
    let mut by_key: HashMap<(&Path, String, bool), EditFamily> = HashMap::new();
    for (i, a) in assets.iter().enumerate() {
        let (Some(dir), Some(name)) = (a.path.parent(), a.path.file_name()) else {
            continue;
        };
        let (stem, edit) = edit_stem(&name.to_string_lossy());
        let e = by_key
            .entry((dir, stem, a.kind == AssetKind::Video))
            .or_default();
        if edit { &mut e.0 } else { &mut e.1 }.push(i);
    }
    let mut fams: Vec<_> = by_key
        .into_values()
        .filter(|(e, o)| !e.is_empty() && !o.is_empty())
        .collect();
    fams.sort();
    fams
}

/// Make byte-identical copies of a file agree before they are collapsed.
///
/// A photo library can hold the same file twice with the caption on one
/// copy and the people tags on the other (or on neither). Every copy gets
/// the group's caption, its fullest people tags (count and signature kept
/// together) and its best library score, and every copy's story score
/// becomes the best one any copy earned. So whichever copy
/// [`collapse_near_dupes`] keeps carries everything the owner entered.
/// Each copy's own file facts (path, dates, size) are left alone.
///
/// An edit and its original ([`edit_families`]) share too, one way: the
/// edit takes what it lacks from the original. Run this before clustering
/// so an edit that lost its date is placed by its original's.
pub fn share_identical_metadata(assets: &mut [Asset], scores: &mut HashMap<String, Score>) {
    for twins in identical_files(assets) {
        let caption = twins.iter().find_map(|&i| assets[i].user_caption.clone());
        let people = twins
            .iter()
            .map(|&i| (assets[i].people_count, assets[i].people_sig))
            .max_by_key(|(count, _)| count.unwrap_or(0));
        let library = twins
            .iter()
            .filter_map(|&i| assets[i].library_score)
            .max_by(f32::total_cmp);
        let best_score = twins
            .iter()
            .filter_map(|&i| scores.get(&assets[i].id))
            .max_by(|a, b| a.total.total_cmp(&b.total))
            .cloned();
        for &i in &twins {
            let a = &mut assets[i];
            a.user_caption = caption.clone();
            if let Some((count, sig)) = people {
                (a.people_count, a.people_sig) = (count, sig);
            }
            a.library_score = library;
            if let Some(s) = &best_score {
                scores.insert(a.id.clone(), s.clone());
            }
        }
    }
    // An edit inherits what the owner entered on its original (caption,
    // people tags, library score) and, only where the edit has none, the
    // original's capture time, offset and place: an exported edit often
    // loses its EXIF, and would otherwise be dated by its file time.
    for (edits, originals) in edit_families(assets) {
        let caption = originals
            .iter()
            .find_map(|&i| assets[i].user_caption.clone());
        let people = originals
            .iter()
            .map(|&i| (assets[i].people_count, assets[i].people_sig))
            .max_by_key(|(count, _)| count.unwrap_or(0));
        let library = originals
            .iter()
            .filter_map(|&i| assets[i].library_score)
            .max_by(f32::total_cmp);
        let dated = originals
            .iter()
            .map(|&i| &assets[i])
            .find(|o| o.taken_at.is_some() && !o.time_guessed)
            .map(|o| (o.taken_at, o.tz_offset_secs));
        let gps = originals.iter().find_map(|&i| assets[i].gps);
        for &i in &edits {
            let a = &mut assets[i];
            if a.user_caption.is_none() {
                a.user_caption = caption.clone();
            }
            if let Some((count, sig)) = people {
                if count.unwrap_or(0) > a.people_count.unwrap_or(0) {
                    (a.people_count, a.people_sig) = (count, sig);
                }
            }
            a.library_score = match (a.library_score, library) {
                (Some(x), Some(y)) => Some(x.max(y)),
                (x, y) => x.or(y),
            };
            if a.taken_at.is_none() || a.time_guessed {
                if let Some((t, o)) = dated {
                    (a.taken_at, a.tz_offset_secs, a.time_guessed) = (t, o, false);
                }
            }
            if a.gps.is_none() {
                a.gps = gps;
            }
        }
    }
}

/// Asset id -> its cluster's label: the chapter map plans are built from.
pub fn chapter_labels(clusters: &[Cluster]) -> HashMap<String, String> {
    clusters
        .iter()
        .flat_map(|c| c.asset_ids.iter().map(|id| (id.clone(), c.label.clone())))
        .collect()
}

/// Ids of every near-dupe group member except the group's best shot: the
/// assets to keep out of plans (they stay in the library).
pub fn suppressed_ids(groups: &[DupeGroup]) -> HashSet<String> {
    groups
        .iter()
        .flat_map(|g| g.members.iter().filter(|m| **m != g.best_id))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(id: &str, taken: Option<i64>, gps: Option<(f64, f64)>, phash: Option<u64>) -> Asset {
        let mut a = Asset::new(id, format!("/tmp/{id}.jpg").into(), AssetKind::Photo);
        a.taken_at = taken;
        a.gps = gps;
        a.phash = phash;
        a
    }

    #[test]
    fn day_split_and_gap_split() {
        let a = vec![
            asset("a", Some(1_714_557_600), None, None),
            asset("b", Some(1_714_557_900), None, None),
            asset("c", Some(1_714_557_600 + 86_400), None, None),
        ];
        let c = cluster_assets(&a);
        assert_eq!(c.len(), 2, "different calendar days: {c:?}");
        assert_eq!(c[0].asset_ids, vec!["a", "b"]);
    }

    #[test]
    fn place_split_on_drift() {
        // ~11 km apart on the same day, four shots at each place.
        let at = |lat: f64, offset: i64, n: usize| -> Vec<Asset> {
            (0..n)
                .map(|i| {
                    let id = format!("{lat}-{i}");
                    let t = 1_714_557_600 + offset + i as i64 * 60;
                    asset(&id, Some(t), Some((lat, -122.41)), None)
                })
                .collect()
        };
        let a: Vec<Asset> = [at(37.77, 0, 4), at(37.87, 3600, 4)].concat();
        let c = cluster_assets(&a);
        assert_eq!(c.len(), 2, "far drift starts a new place: {c:?}");
    }

    #[test]
    fn outlier_drop_keeps_time_grouping() {
        // Teleport SF -> Tokyo in 60 s: >1000 km/h.
        let a = vec![
            asset("a", Some(1_714_557_600), Some((37.77, -122.41)), None),
            asset("b", Some(1_714_557_660), Some((35.68, 139.69)), None),
        ];
        assert_eq!(gps_outliers(&a), HashSet::from(["b".to_string()]));
        let c = cluster_assets(&a);
        assert_eq!(c.len(), 1, "same day+hour stays one cluster: {c:?}");
        assert!(c[0].centroid_gps.is_some(), "outlier excluded, anchor kept");
    }

    #[test]
    fn near_dupe_collapse_picks_best_shot() {
        let a = vec![
            asset("a", Some(1), None, Some(0b101)),
            asset("b", Some(2), None, Some(0b101)),
            asset("c", Some(3), None, Some(u64::MAX)),
        ];
        let mut scores = HashMap::new();
        scores.insert(
            "a".to_string(),
            Score {
                total: 0.4,
                ..Default::default()
            },
        );
        scores.insert(
            "b".to_string(),
            Score {
                total: 0.9,
                ..Default::default()
            },
        );
        let g = collapse_near_dupes(&a, &scores);
        assert_eq!(g.len(), 2);
        let ab = g
            .iter()
            .find(|x| x.members.contains(&"a".to_string()))
            .unwrap();
        assert_eq!(ab.best_id, "b");
    }

    #[test]
    fn identical_files_collapse_whatever_their_kind() {
        // The same clip in the library twice, and a different clip that
        // opens on the same frame at the same time.
        let mut clip = asset("clip", Some(1000), None, Some(7));
        clip.kind = AssetKind::Video;
        clip.content_hash = Some("h-clip".into());
        let mut twin = clip.clone();
        twin.id = "clip-twin".into();
        let mut other = clip.clone();
        other.id = "other".into();
        other.content_hash = Some("h-other".into());
        // Identical photos collapse even with no pHash and no capture time.
        let mut photo = asset("photo", None, None, None);
        photo.content_hash = Some("h-photo".into());
        let mut photo_twin = photo.clone();
        photo_twin.id = "photo-twin".into();
        // Unhashed files are never twins of each other.
        let bare_a = asset("bare-a", None, None, None);
        let bare_b = asset("bare-b", None, None, None);
        let assets = [clip, twin, other, photo, photo_twin, bare_a, bare_b];
        let groups = collapse_near_dupes(&assets, &HashMap::new());
        let mut sizes: Vec<usize> = groups.iter().map(|g| g.members.len()).collect();
        sizes.sort_unstable();
        assert_eq!(sizes, [1, 1, 1, 2, 2]);
        let suppressed = suppressed_ids(&groups);
        assert_eq!(suppressed.len(), 2);
        assert!(suppressed.contains("clip-twin") && suppressed.contains("photo-twin"));
    }

    #[test]
    fn edit_stem_forms_seen_in_osxphotos_exports() {
        let e = |n: &str| edit_stem(n);
        assert_eq!(e("IMG_6674.MOV"), ("img_6674".into(), false));
        assert_eq!(e("IMG_6674_edited.mov"), ("img_6674".into(), true));
        assert_eq!(e("IMG_1132 (1).HEIC"), ("img_1132".into(), false));
        assert_eq!(e("IMG_1095_edited (1).heic"), ("img_1095".into(), true));
        assert_eq!(e("IMG_1273_EDITED.jpeg"), ("img_1273".into(), true));
        // Not the form osxphotos writes, but harmless to accept.
        assert_eq!(e("IMG_1 (2)_edited.heic"), ("img_1".into(), true));
        // Only a trailing all-digit copy number is stripped.
        assert_eq!(e("Trip (Day 2).jpg"), ("trip (day 2)".into(), false));
        assert_eq!(e("_edited.jpg"), ("_edited".into(), false));
        assert_eq!(e("noext"), ("noext".into(), false));
    }

    fn at(id: &str, path: &str, kind: AssetKind, taken: Option<i64>, phash: u64) -> Asset {
        let mut a = Asset::new(id, path.into(), kind);
        a.taken_at = taken;
        a.phash = Some(phash);
        a
    }

    #[test]
    fn an_edit_beats_a_higher_scored_original() {
        let score = |total: f32| Score {
            total,
            ..Default::default()
        };
        // Different pixels, a day apart: only the names tie them.
        let orig = at("o", "/t/d/IMG_6674.MOV", AssetKind::Video, Some(1000), 0);
        let copy = at(
            "c",
            "/t/d/IMG_6674 (1).MOV",
            AssetKind::Video,
            Some(1000),
            1,
        );
        let edit = at(
            "e",
            "/t/d/IMG_6674_edited.mov",
            AssetKind::Video,
            Some(90_000),
            u64::MAX,
        );
        let scores = HashMap::from([
            ("o".to_string(), score(0.9)),
            ("c".to_string(), score(0.95)),
            ("e".to_string(), score(0.2)),
        ]);
        let g = collapse_near_dupes(&[orig, copy, edit], &scores);
        assert_eq!(g.len(), 1, "{g:?}");
        assert_eq!(g[0].best_id, "e");
        assert_eq!(g[0].members.len(), 3);
    }

    #[test]
    fn edits_join_only_their_own_original() {
        let t = Some(1000);
        let a = [
            // A still's edit and a same-named clip: different families.
            at("pe", "/t/d/IMG_1_edited.heic", AssetKind::LivePhoto, t, 0),
            at("vo", "/t/d/IMG_1.MOV", AssetKind::Video, t, u64::MAX),
            // An edit with no original beside it.
            at(
                "lone",
                "/t/d/IMG_2_edited.heic",
                AssetKind::Photo,
                t,
                0xFFFF_FFFF_0000_0000,
            ),
            // The same stem in another folder is another photo.
            at(
                "x",
                "/t/other/IMG_2.HEIC",
                AssetKind::Photo,
                Some(500_000),
                0xF0F0,
            ),
        ];
        let g = collapse_near_dupes(&a, &HashMap::new());
        assert_eq!(g.len(), 4, "{g:?}");
        assert!(g.iter().all(|g| g.members.len() == 1));
    }

    #[test]
    fn an_edit_inherits_what_the_original_carries() {
        let mut orig = at("o", "/t/d/IMG_7.HEIC", AssetKind::LivePhoto, Some(1_000), 0);
        orig.user_caption = Some("Night market".into());
        (orig.people_count, orig.people_sig) = (Some(2), Some(7));
        orig.library_score = Some(0.6);
        orig.tz_offset_secs = Some(8 * 3600);
        orig.gps = Some((1.29, 103.85));
        // The exported edit lost its EXIF: dated by mtime, no place.
        let mut edit = at(
            "e",
            "/t/d/IMG_7_edited.heic",
            AssetKind::Photo,
            Some(9_999),
            1,
        );
        edit.time_guessed = true;
        // A dated edit with its own caption keeps its own.
        let mut orig2 = at("o2", "/t/d/IMG_8.HEIC", AssetKind::Photo, Some(2_000), 0);
        orig2.user_caption = Some("old".into());
        orig2.gps = Some((1.0, 1.0));
        let mut edit2 = at(
            "e2",
            "/t/d/IMG_8_edited.heic",
            AssetKind::Photo,
            Some(2_100),
            1,
        );
        edit2.user_caption = Some("new".into());
        edit2.tz_offset_secs = Some(3600);
        edit2.gps = Some((2.0, 2.0));
        let mut assets = [orig, edit, orig2, edit2];
        share_identical_metadata(&mut assets, &mut HashMap::new());
        let e = &assets[1];
        assert_eq!(e.user_caption.as_deref(), Some("Night market"));
        assert_eq!((e.people_count, e.people_sig), (Some(2), Some(7)));
        assert_eq!(e.library_score, Some(0.6));
        assert_eq!(
            (e.taken_at, e.tz_offset_secs),
            (Some(1_000), Some(8 * 3600))
        );
        assert!(!e.time_guessed);
        assert_eq!(e.gps, Some((1.29, 103.85)));
        let e2 = &assets[3];
        assert_eq!(e2.user_caption.as_deref(), Some("new"));
        assert_eq!((e2.taken_at, e2.tz_offset_secs), (Some(2_100), Some(3600)));
        assert_eq!(e2.gps, Some((2.0, 2.0)));
        // The originals are left as they were.
        assert_eq!(assets[0].taken_at, Some(1_000));
        assert_eq!(assets[2].user_caption.as_deref(), Some("old"));
    }

    #[test]
    fn identical_copies_share_what_the_owner_entered() {
        let score = |total: f32| Score {
            total,
            ..Default::default()
        };
        // The caption is on one copy, the people tags on the other.
        let mut a = asset("a", Some(1000), None, Some(1));
        a.content_hash = Some("h".into());
        a.user_caption = Some("Egg tarts".into());
        let mut b = a.clone();
        b.id = "b".into();
        b.user_caption = None;
        (b.people_count, b.people_sig) = (Some(2), Some(42));
        b.library_score = Some(0.7);
        // A different file keeps its own (empty) metadata.
        let mut c = asset("c", Some(90_000), None, Some(u64::MAX));
        c.content_hash = Some("other".into());
        let mut assets = [a, b, c];
        let mut scores = HashMap::from([
            ("a".to_string(), score(0.6)),
            ("b".to_string(), score(0.8)),
            ("c".to_string(), score(0.9)),
        ]);
        share_identical_metadata(&mut assets, &mut scores);
        for twin in &assets[..2] {
            assert_eq!(
                twin.user_caption.as_deref(),
                Some("Egg tarts"),
                "{}",
                twin.id
            );
            assert_eq!((twin.people_count, twin.people_sig), (Some(2), Some(42)));
            assert_eq!(twin.library_score, Some(0.7));
            assert_eq!(scores[&twin.id].total, 0.8, "the best copy's score");
        }
        assert_eq!(assets[2].user_caption, None);
        assert_eq!(assets[2].people_count, None);
        assert_eq!(scores["c"].total, 0.9);
        // Whichever copy the collapse keeps now carries the caption.
        let groups = collapse_near_dupes(&assets, &scores);
        let kept = groups.iter().find(|g| g.members.len() == 2).unwrap();
        let kept = assets.iter().find(|x| x.id == kept.best_id).unwrap();
        assert!(kept.user_caption.is_some() && kept.people_sig.is_some());
    }

    #[test]
    fn videos_never_collapse_as_near_dupes() {
        // Same poster frame, same moment: distinct clips (and a clip vs. a
        // photo) all stay, since the pHash only covers the first frame.
        let video = |id: &str, t: i64| {
            let mut a = asset(id, Some(t), None, Some(0b101));
            a.kind = AssetKind::Video;
            a
        };
        let a = vec![
            video("v1", 1),
            video("v2", 2),
            asset("p", Some(3), None, Some(0b101)),
        ];
        assert_eq!(collapse_near_dupes(&a, &HashMap::new()).len(), 3);
    }

    #[test]
    fn near_dupes_need_same_moment() {
        let t0 = 1_714_550_400;
        let paris = Some((48.8566, 2.3522));
        let lyon = Some((45.7640, 4.8357));
        let a = vec![
            asset("burst1", Some(t0), paris, Some(0b101)),
            asset("burst2", Some(t0 + 30), paris, Some(0b101)),
            asset("next_day", Some(t0 + 86_400), paris, Some(0b101)),
            asset("elsewhere", Some(t0 + 60), lyon, Some(0b101)),
            asset("no_time", None, None, Some(0b101)),
        ];
        let g = collapse_near_dupes(&a, &HashMap::new());
        assert_eq!(g.len(), 4, "only the burst collapses: {g:?}");
        let burst = g.iter().find(|x| x.members.len() == 2).unwrap();
        assert!(burst.members.contains(&"burst1".to_string()));
        assert!(burst.members.contains(&"burst2".to_string()));
    }

    #[test]
    fn same_scene_by_embedding_collapses_within_the_window_only() {
        use crate::embedding::Embedding;
        let t0 = 1_714_550_400;
        let scene = Embedding::from_f32(&[1.0, 0.1, 0.0]).unwrap();
        let reframed = Embedding::from_f32(&[0.9, 0.3, 0.1]).unwrap();
        let other = Embedding::from_f32(&[0.0, 0.2, 1.0]).unwrap();
        let same_place = Embedding::from_f32(&[0.92, 0.1, 0.38]).unwrap();
        let shot = |id: &str, t: i64, phash: u64, e: Option<&Embedding>| {
            let mut a = asset(id, Some(t), None, Some(phash));
            a.embedding = e.cloned();
            a
        };
        let a = vec![
            shot("wide", t0, 0x0000_0000_0000_0000, Some(&scene)),
            // Far pHash (another framing), same scene: collapses.
            shot("zoom", t0 + 60, 0xFFFF_FFFF_0000_0000, Some(&reframed)),
            // Far pHash, another scene: stays.
            shot("menu", t0 + 90, 0x0000_0000_FFFF_FFFF, Some(&other)),
            // Same scene but outside the window: stays.
            shot(
                "later",
                t0 + DUPE_WINDOW_SECS + 1,
                0xFFFF_0000_FFFF_0000,
                Some(&scene),
            ),
            // No embedding: pHash alone decides, and it is far.
            shot("plain", t0 + 30, 0xF0F0_F0F0_F0F0_F0F0, None),
            // Same place (cos ~0.93) but not a retake: stays.
            shot("place", t0 + 120, 0x00FF_00FF_00FF_00FF, Some(&same_place)),
        ];
        let g = collapse_near_dupes(&a, &HashMap::new());
        assert_eq!(g.len(), 5, "{g:?}");
        let pair = g.iter().find(|x| x.members.len() == 2).unwrap();
        assert!(pair.members.contains(&"wide".to_string()));
        assert!(pair.members.contains(&"zoom".to_string()));

        // Different people in the same view are different keepsakes.
        let mut tagged = a[..2].to_vec();
        tagged[1].people_sig = Some(42);
        assert_eq!(collapse_near_dupes(&tagged, &HashMap::new()).len(), 2);
    }

    #[test]
    fn near_dupe_visual_chain_does_not_merge_endpoints() {
        let t0 = 1_714_550_400;
        // Same moment, visual chain: A-B and B-C are near-dupes, A-C is not.
        let a = vec![
            asset("v0", Some(t0), None, Some(0x0000)),
            asset("v1", Some(t0), None, Some(0x00ff)),
            asset("v2", Some(t0), None, Some(0xffff)),
        ];
        let g = collapse_near_dupes(&a, &HashMap::new());
        assert!(
            g.iter().all(|grp| !(grp.members.contains(&"v0".to_string())
                && grp.members.contains(&"v2".to_string()))),
            "endpoints failing is_near_dupe can't share a group: {g:?}"
        );
    }

    #[test]
    fn near_dupe_chain_does_not_span_window() {
        let t0 = 1_714_550_400;
        let a = vec![
            asset("m0", Some(t0), None, Some(0b101)),
            asset("m9", Some(t0 + 9 * 60), None, Some(0b101)),
            asset("m18", Some(t0 + 18 * 60), None, Some(0b101)),
        ];
        let g = collapse_near_dupes(&a, &HashMap::new());
        assert_eq!(
            g.len(),
            2,
            "endpoints 18 min apart can't share a group: {g:?}"
        );
        for grp in &g {
            let ts: Vec<i64> = grp
                .members
                .iter()
                .map(|id| a.iter().find(|x| &x.id == id).unwrap().taken_at.unwrap())
                .collect();
            let span = ts.iter().max().unwrap() - ts.iter().min().unwrap();
            assert!(span <= DUPE_WINDOW_SECS, "group spans {span}s: {grp:?}");
        }
    }

    #[test]
    fn near_dupe_chain_does_not_span_radius() {
        let t0 = 1_714_550_400;
        // ~150 m steps north: adjacent pairs qualify, endpoints ~300 m apart.
        let a = vec![
            asset("p0", Some(t0), Some((48.8566, 2.3522)), Some(0b101)),
            asset("p1", Some(t0 + 30), Some((48.8580, 2.3522)), Some(0b101)),
            asset("p2", Some(t0 + 60), Some((48.8593, 2.3522)), Some(0b101)),
        ];
        let g = collapse_near_dupes(&a, &HashMap::new());
        assert_eq!(
            g.len(),
            2,
            "endpoints >200 m apart can't share a group: {g:?}"
        );
    }

    #[test]
    fn time_gap_split_and_bounds() {
        let t0 = 1_714_550_400; // 2024-05-01 08:00 UTC
        let mut a = vec![
            asset("b", Some(t0 + 600), None, None),
            asset("a", Some(t0), None, None),
            asset("a2", Some(t0 + 1200), None, None),
            asset("a3", Some(t0 + 1800), None, None),
        ];
        let later = t0 + 1800 + TIME_GAP_SECS + 1;
        a.extend((0..4).map(|i| asset(&format!("c{i}"), Some(later + i * 60), None, None)));
        let c = cluster_assets(&a);
        assert_eq!(c.len(), 2, "same day, >4 h gap splits: {c:?}");
        assert_eq!(c[0].asset_ids, vec!["a", "b", "a2", "a3"], "story order");
        assert_eq!(
            (c[0].start_time, c[0].end_time),
            (Some(t0), Some(t0 + 1800))
        );
        assert_eq!(c[0].label, "2024-05-01");
        assert_eq!(c[1].id, "c002");
    }

    /// `n` shots a minute apart from `t0`, ids `<prefix><i>`.
    fn run(prefix: &str, t0: i64, n: usize, gps: Option<(f64, f64)>) -> Vec<Asset> {
        (0..n)
            .map(|i| asset(&format!("{prefix}{i}"), Some(t0 + i as i64 * 60), gps, None))
            .collect()
    }

    #[test]
    fn small_clusters_merge_into_the_closer_same_day_neighbour() {
        let t0 = 1_714_550_400; // 2024-05-01 08:00 UTC
        let h = 3600;
        // Big morning walk, a lone hotel shot 5 h later, big evening walk
        // 6 h after that: the loner is nearer the morning in time and place.
        let morning = (37.7700, -122.4100);
        let evening = (37.7710, -122.4100);
        let mut a = run("m", t0, 6, Some(morning));
        a.extend(run("h", t0 + 5 * h, 1, Some((37.7701, -122.4100))));
        a.extend(run("e", t0 + 11 * h, 5, Some(evening)));
        let c = cluster_assets(&a);
        assert_eq!(c.len(), 2, "{c:?}");
        assert_eq!(c[0].asset_ids.len(), 7);
        assert!(c[0].asset_ids.contains(&"h0".to_string()));
        assert_eq!(c[1].asset_ids.len(), 5);
        assert_eq!(c[0].id, "c001");
        assert_eq!(c[1].id, "c002", "ids stay sequential after merging");
        assert_eq!(
            c[0].end_time,
            Some(t0 + 5 * h),
            "bounds cover the merged shot"
        );
        // The merged centroid is still the big cluster's place (the loner
        // adds one fix to six).
        let g = c[0].centroid_gps.unwrap();
        assert!((g.0 - morning.0).abs() < 0.001);
    }

    #[test]
    fn small_clusters_prefer_the_neighbour_closer_in_place_when_time_ties() {
        let t0 = 1_714_550_400;
        let h = 3600;
        // Loner exactly 5 h after one cluster ends and 5 h before the next
        // begins, but 11 km from the first and right next to the second.
        let mut a = run("a", t0, 4, Some((37.77, -122.41)));
        let lone = a.last().unwrap().taken_at.unwrap() + 5 * h;
        a.extend(run("x", lone, 1, Some((37.87, -122.41))));
        a.extend(run("b", lone + 5 * h, 4, Some((37.87, -122.41))));
        let c = cluster_assets(&a);
        let with_x = c
            .iter()
            .find(|c| c.asset_ids.contains(&"x0".into()))
            .unwrap();
        assert!(with_x.asset_ids.contains(&"b0".into()), "{c:?}");
    }

    #[test]
    fn small_clusters_never_merge_across_days_and_runs_collapse() {
        let t0 = 1_714_550_400; // 08:00 UTC
        let day = 86_400;
        // A lone shot on its own day stays alone, even next to a big day.
        let mut a = run("d1", t0, 5, None);
        a.extend(run("d2", t0 + day, 1, None));
        a.extend(run("d3", t0 + 2 * day, 5, None));
        assert_eq!(cluster_assets(&a).len(), 3);
        // Singletons strung through one day (each >4 h apart) collapse
        // into one cluster rather than staying a string of one-shot chapters.
        let h = 3600;
        let strung: Vec<Asset> = (0..4)
            .map(|i| asset(&format!("s{i}"), Some(t0 + i * 5 * h), None, None))
            .collect();
        let c = cluster_assets(&strung);
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(c[0].asset_ids, vec!["s0", "s1", "s2", "s3"]);
    }

    #[test]
    fn suppressed_ids_drop_everything_but_the_best_shot() {
        let g = [
            DupeGroup {
                members: vec!["a".into(), "b".into(), "c".into()],
                best_id: "b".into(),
            },
            DupeGroup {
                members: vec!["d".into()],
                best_id: "d".into(),
            },
        ];
        let mut s: Vec<String> = suppressed_ids(&g).into_iter().collect();
        s.sort();
        assert_eq!(s, vec!["a", "c"]);
    }

    #[test]
    fn unknown_time_goes_last_and_unlabeled_day() {
        let a = vec![
            asset("x", None, None, None),
            asset("a", Some(1_714_557_600), None, None),
        ];
        let order: Vec<&str> = story_order(&a).iter().map(|a| a.id.as_str()).collect();
        assert_eq!(order, vec!["a", "x"]);
        assert_eq!(day_key(None, None), "unknown-date");
        // 2026-10-04 22:50 UTC is Oct 5 in Japan (+09:00).
        assert_eq!(day_key(Some(1_791_154_200), Some(9 * 3600)), "2026-10-05");
        assert_eq!(day_key(Some(1_791_154_200), None), "2026-10-04");
        let c = cluster_assets(&a);
        assert_eq!(c.last().unwrap().label, "unknown-date");
    }

    #[test]
    fn place_label_and_centroid() {
        let a = vec![
            asset("a", Some(1_714_557_600), Some((10.0, 20.0)), None),
            asset("b", Some(1_714_557_700), Some((10.0002, 20.0002)), None),
        ];
        let c = cluster_assets(&a);
        assert_eq!(c.len(), 1);
        let (lat, lon) = c[0].centroid_gps.unwrap();
        assert!((lat - 10.0001).abs() < 1e-9 && (lon - 20.0001).abs() < 1e-9);
        assert!(c[0].label.ends_with(" · place 1"));
    }

    /// A day the group split: phone 1xxx in the city, phone 5xxx at the
    /// park 12 km away, shooting in turn every 25 minutes.
    fn split_day(park_shots: usize) -> Vec<Asset> {
        let (city, park) = ((22.282, 114.154), (22.313, 114.043));
        let t0 = 1_792_285_200;
        let mut a = vec![];
        for k in 0..8 {
            let t = t0 + k * 50 * 60;
            a.push(asset(
                &format!("IMG_{}", 1502 + k),
                Some(t),
                Some(city),
                None,
            ));
            if (k as usize) < park_shots {
                let t = t0 + k * 50 * 60 + 25 * 60;
                a.push(asset(
                    &format!("IMG_{}", 5801 + k),
                    Some(t),
                    Some(park),
                    None,
                ));
            }
        }
        a
    }

    #[test]
    fn a_split_day_gets_chapters_per_party_in_party_order() {
        let a = split_day(8);
        let c = cluster_assets(&a);
        assert_eq!(c.len(), 2, "{c:?}");
        assert!(c[0].asset_ids.iter().all(|id| id.starts_with("IMG_15")));
        assert!(c[1].asset_ids.iter().all(|id| id.starts_with("IMG_58")));
        let tracks: Vec<Track> = story_tracks(&a).into_iter().map(|(_, t)| t).collect();
        // The first city shot comes before anyone is seen apart.
        assert_eq!(tracks[0], Track::Shared);
        let party = |rank| Track::Party { segment: 0, rank };
        assert!(tracks[1..8].iter().all(|&t| t == party(0)));
        assert!(tracks[8..].iter().all(|&t| t == party(1)));
    }

    #[test]
    fn same_day_needs_a_known_shared_day() {
        let a = split_day(1);
        assert!(same_day(&a[0], &a[1]));
        let mut later = a[0].clone();
        later.taken_at = later.taken_at.map(|t| t + 86_400);
        assert!(!same_day(&a[0], &later));
        let mut undated = a[0].clone();
        undated.taken_at = None;
        assert!(!same_day(&undated, &undated));
    }

    #[test]
    fn a_small_party_cluster_never_merges_into_another_party() {
        // Two park shots an hour apart: a cluster small enough to merge,
        // but its only neighbour is the other party's.
        let mut a = split_day(0);
        let (park, t0) = ((22.313, 114.043), 1_792_285_200);
        a.push(asset("IMG_5801", Some(t0 + 600), Some(park), None));
        a.push(asset("IMG_5802", Some(t0 + 600 + 3600), Some(park), None));
        let c = cluster_assets(&a);
        let park: Vec<&Cluster> = c
            .iter()
            .filter(|c| c.asset_ids[0].starts_with("IMG_58"))
            .collect();
        assert_eq!(park.len(), 1, "{c:?}");
        assert_eq!(park[0].asset_ids, ["IMG_5801", "IMG_5802"]);
    }
}
