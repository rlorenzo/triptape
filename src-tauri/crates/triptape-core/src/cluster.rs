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

    /// Emit the accumulated cluster (if any) and reset for `next_day`.
    fn flush(&mut self, out: &mut Vec<Cluster>, next_day: String) {
        let acc = std::mem::replace(
            self,
            Acc {
                day: next_day,
                ..Default::default()
            },
        );
        if acc.ids.is_empty() {
            return;
        }
        let seq = out.len() + 1;
        let center = acc.centroid();
        let place = if center.is_some() {
            format!(" · place {seq}")
        } else {
            String::new()
        };
        out.push(Cluster {
            id: format!("c{seq:03}"),
            asset_ids: acc.ids,
            start_time: acc.start,
            end_time: acc.end,
            centroid_gps: center,
            label: format!("{}{place}", acc.day),
        });
    }
}

/// Group assets into day/place clusters in story order.
pub fn cluster_assets(assets: &[Asset]) -> Vec<Cluster> {
    let ordered = story_order(assets);
    let outliers = gps_outliers_ordered(&ordered);

    let mut clusters: Vec<Cluster> = vec![];
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
        // ~11 km apart on the same day.
        let a = vec![
            asset("a", Some(1_714_557_600), Some((37.77, -122.41)), None),
            asset("b", Some(1_714_558_000), Some((37.87, -122.41)), None),
        ];
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
        let a = vec![
            asset("b", Some(t0 + 600), None, None),
            asset("a", Some(t0), None, None),
            asset("c", Some(t0 + 600 + TIME_GAP_SECS + 1), None, None),
        ];
        let c = cluster_assets(&a);
        assert_eq!(c.len(), 2, "same day, >4 h gap splits: {c:?}");
        assert_eq!(c[0].asset_ids, vec!["a", "b"], "story order");
        assert_eq!((c[0].start_time, c[0].end_time), (Some(t0), Some(t0 + 600)));
        assert_eq!(c[0].label, "2024-05-01");
        assert_eq!(c[1].id, "c002");
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
