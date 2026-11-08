//! Stage 6 — Cluster: time-gap + GPS day/place clustering.
//!
//! Pipeline: sort by capture time, drop GPS outliers whose implied speed
//! from the previous fix exceeds 1,000 km/h, then group into clusters — a
//! new cluster starts on calendar-day change, on a time gap over
//! [`TIME_GAP_SECS`], or when both fixes have GPS and drift more than
//! [`PLACE_SPLIT_METERS`] from the running centroid. Near-dupes collapse to
//! a best-shot pick via pHash Hamming distance.

use crate::geo::haversine_m;
use crate::hash::is_near_dupe;
use crate::types::{Asset, Cluster, Score};
use std::collections::{HashMap, HashSet};

/// A time gap this large always starts a new cluster (4 h).
pub const TIME_GAP_SECS: i64 = 4 * 3600;
/// Drift from the running GPS centroid that starts a new place (5 km).
pub const PLACE_SPLIT_METERS: f64 = 5_000.0;
/// Implied speed that marks a GPS fix an outlier (1,000 km/h in m/s).
pub const OUTLIER_SPEED_MPS: f64 = 1_000_000.0 / 3600.0;
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
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown-date".to_string())
}

/// Assets in story order: capture time (unknown last), then id.
pub fn story_order(assets: &[Asset]) -> Vec<&Asset> {
    let mut ordered: Vec<&Asset> = assets.iter().collect();
    ordered.sort_by(|a, b| {
        (a.taken_at.unwrap_or(i64::MAX), &a.id).cmp(&(b.taken_at.unwrap_or(i64::MAX), &b.id))
    });
    ordered
}

/// Ids of assets whose GPS fix implies travel faster than 1,000 km/h from
/// the previous accepted fix. The later fix is dropped (kept for time
/// clustering, ignored for place/route math). `ordered` must be in story
/// order; fixes without a usable time delta are accepted unjudged.
fn gps_outliers_ordered(ordered: &[&Asset]) -> HashSet<String> {
    let mut bad = HashSet::new();
    let mut prev: Option<(Option<i64>, (f64, f64))> = None;
    for a in ordered {
        let Some(g) = a.gps else { continue };
        let Some((pt, pg)) = prev else {
            prev = Some((a.taken_at, g));
            continue;
        };
        if let (Some(t), Some(pt)) = (a.taken_at, pt) {
            let dt = (t - pt).abs() as f64;
            if dt > 0.0 && haversine_m(pg, g) / dt > OUTLIER_SPEED_MPS {
                bad.insert(a.id.clone());
                continue;
            }
        }
        prev = Some((a.taken_at, g));
    }
    bad
}

/// See [`gps_outliers_ordered`]; sorts `assets` into story order first.
pub fn gps_outliers(assets: &[Asset]) -> HashSet<String> {
    gps_outliers_ordered(&story_order(assets))
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
}

impl Acc {
    fn centroid(&self) -> Option<(f64, f64)> {
        let n = self.gps_n as f64;
        (self.gps_n > 0).then(|| (self.gps_sum.0 / n, self.gps_sum.1 / n))
    }

    fn push(&mut self, id: &str, t: Option<i64>, gps: Option<(f64, f64)>) {
        self.ids.push(id.to_string());
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
/// neighbour. Clusters never merge across days. The merged cluster keeps the
/// larger side's centroid, since the GPS sums are dominated by it.
fn merge_small(mut v: Vec<Acc>) -> Vec<Acc> {
    let neighbour = |v: &[Acc], i: usize| -> Option<usize> {
        [i.checked_sub(1), Some(i + 1)]
            .into_iter()
            .flatten()
            .filter(|&j| j < v.len() && v[j].day == v[i].day)
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
/// clusters into a same-day neighbour ([`MERGE_MAX_ASSETS`]).
pub fn cluster_assets(assets: &[Asset]) -> Vec<Cluster> {
    let ordered = story_order(assets);
    let outliers = gps_outliers_ordered(&ordered);

    let mut clusters: Vec<Acc> = vec![];
    let mut acc = Acc::default();
    for a in ordered {
        let day = day_key(a.taken_at, a.tz_offset_secs);
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
            if day_changed || gap || moved {
                acc.flush(&mut clusters, day);
            }
        }
        acc.push(&a.id, a.taken_at, gps);
    }
    acc.flush(&mut clusters, String::new());
    finish(merge_small(clusters))
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

/// Collapse near-dupes (pHash distance ≤ threshold, captured within
/// [`DUPE_WINDOW_SECS`] and, when both have GPS, [`DUPE_RADIUS_METERS`])
/// with union-find; groups merge only when every cross pair is the same
/// moment, so a whole group stays within the window and radius. The best
/// shot is the highest `total` score (ties → first in story order).
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
    let phashes: Vec<Option<u64>> = assets.iter().map(|a| a.phash).collect();
    let mut members: Vec<Vec<usize>> = (0..n).map(|i| vec![i]).collect();
    for i in 0..n {
        for j in (i + 1)..n {
            if let (Some(a), Some(b)) = (phashes[i], phashes[j]) {
                if !is_near_dupe(a, b) {
                    continue;
                }
                let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                // Complete linkage: merge only if every cross pair is the
                // same moment, so chains (0, 9, 18 min) can't span the
                // window or radius as a whole.
                if ri != rj
                    && members[ri].iter().all(|&x| {
                        members[rj].iter().all(|&y| {
                            same_moment(&assets[x], &assets[y])
                                && matches!(
                                    (phashes[x], phashes[y]),
                                    (Some(p), Some(q)) if is_near_dupe(p, q)
                                )
                        })
                    })
                {
                    parent[rj] = ri;
                    let moved = std::mem::take(&mut members[rj]);
                    members[ri].extend(moved);
                }
            }
        }
    }
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        groups.entry(r).or_default().push(i);
    }
    let score_of = |id: &str| scores.get(id).map(|s| s.total).unwrap_or(0.5);
    let mut out: Vec<DupeGroup> = groups
        .into_values()
        .map(|idx| {
            let members: Vec<String> = idx.iter().map(|&i| assets[i].id.clone()).collect();
            let mut best_id = members[0].clone();
            for m in &members[1..] {
                if score_of(m) > score_of(&best_id) {
                    best_id = m.clone();
                }
            }
            DupeGroup { members, best_id }
        })
        .collect();
    out.sort_by(|a, b| a.best_id.cmp(&b.best_id));
    out
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
    use crate::types::AssetKind;

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
}
