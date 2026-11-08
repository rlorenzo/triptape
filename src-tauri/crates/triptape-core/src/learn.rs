//! On-device learning of story-score weights from the owner's Story-editor
//! edits.
//!
//! Every include (pin), swap and exclude is a preference: the owner chose
//! one shot over another. Each is recorded as a pair of feature vectors
//! (what the story score sees in each asset) in the project's database.
//! [`fit`] turns the pairs into small per-feature multipliers on the default
//! weights with a regularised pairwise-logistic (Bradley-Terry) model:
//!
//! ```text
//! P(chosen beats rejected) = sigmoid(BETA * sum_k m_k * (x_chosen_k - x_rejected_k))
//! minimise  -sum log P  +  LAMBDA * sum_k (m_k - 1)^2,   MIN_MULT <= m_k <= MAX_MULT
//! ```
//!
//! The pull toward `m = 1` (the defaults) is strong, nothing adapts before
//! [`MIN_USABLE_EVENTS`] informative events, and every multiplier is bounded.
//! The fit is convex, runs a fixed number of projected gradient steps on
//! sorted input, and is deterministic. Nothing leaves the machine.
//!
//! Scope: per project, because `Db::list_preference_events` is keyed by
//! project. [`fit`] and [`summarize`] take plain event slices, so a global
//! profile is a matter of feeding them the events of every project.
//!
//! The learned weights are applied only when a plan is *built*
//! ([`replan`]); [`crate::db::Db::edited_plan`] never reads them, so an
//! existing edited plan is never silently reshuffled. Scores are stored raw
//! on the asset rows (so features never feed back on themselves); the
//! multipliers are applied to a copy at plan time.

use crate::cluster::{cluster_assets, collapse_near_dupes, suppressed_ids};
use crate::db::Db;
use crate::plan::{apply_edits, build_plan_with_length, FilmLength, PlaceOverride, PlanEdits};
use crate::score::{highlight_score, StorySignals, HIGHLIGHT_THRESHOLD, TECH_FLOOR, TECH_SPAN};
use crate::types::{Asset, Plan, Score};
use std::collections::{BTreeMap, HashMap};

/// Informative events (ones where the two shots differ on some feature)
/// needed before any weight moves off its default.
pub const MIN_USABLE_EVENTS: usize = 8;
/// Bounds on every multiplier.
pub const MIN_MULT: f64 = 0.5;
pub const MAX_MULT: f64 = 2.0;
/// Multipliers closer to 1 than this are reported (and applied) as 1.
pub const MIN_CHANGE: f64 = 0.05;
/// Sharpness of the logistic link: story-score gaps between rival shots
/// are ~0.1-0.3, so this makes a 0.2 gap a clear preference.
const BETA: f64 = 10.0;
/// Strength of the pull toward the default weights.
const LAMBDA: f64 = 3.0;
const ITERATIONS: usize = 600;
const EPS: f64 = 1e-9;
/// Pseudo asset id of an exclude event's winner: the shot's chapter peers.
pub const PEERS_ID: &str = "~peers";

/// One story-score feature the owner's edits can reweight.
pub struct FeatureDef {
    pub name: &'static str,
    /// Shown in explanations ("people weight x1.3").
    pub label: &'static str,
    /// Plain-English description of the kept shot having more / less of it.
    pub more: &'static str,
    pub less: &'static str,
}

/// The reweightable features, in fit order. Each feature's value is the
/// score points it contributes at the default weights, so a multiplier
/// scales that contribution directly.
pub const FEATURES: [FeatureDef; 6] = [
    FeatureDef {
        name: "people",
        label: "people",
        more: "more people",
        less: "fewer people",
    },
    FeatureDef {
        name: "caption",
        label: "caption",
        more: "a caption you wrote",
        less: "no caption of yours",
    },
    FeatureDef {
        name: "video",
        label: "video clip",
        more: "video footage",
        less: "a still instead of video",
    },
    FeatureDef {
        name: "place",
        label: "unique place",
        more: "a distinctive place",
        less: "a more ordinary place",
    },
    FeatureDef {
        name: "motion",
        label: "lively motion",
        more: "livelier audio or movement",
        less: "calmer audio and movement",
    },
    FeatureDef {
        name: "quality",
        label: "picture quality",
        more: "a sharper, better-exposed picture",
        less: "a softer or worse-exposed picture",
    },
];

/// Feature name -> value (score points at default weights).
pub type Features = BTreeMap<String, f64>;

/// Which edit produced a preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EventKind {
    /// A pinned photo was chosen over the shot it displaced.
    Include,
    /// A replacement was chosen over the original pick.
    Swap,
    /// A removed pick was rejected in favour of its chapter's other shots.
    Exclude,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Include => "include",
            Self::Swap => "swap",
            Self::Exclude => "exclude",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "include" => Self::Include,
            "swap" => Self::Swap,
            "exclude" => Self::Exclude,
            _ => return None,
        })
    }
}

/// One recorded preference: `chosen` beat `rejected`.
#[derive(Debug, Clone, PartialEq)]
pub struct PrefEvent {
    pub kind: EventKind,
    pub chosen_id: String,
    pub rejected_id: String,
    pub chosen: Features,
    pub rejected: Features,
}

impl PrefEvent {
    fn delta(&self, name: &str) -> f64 {
        self.chosen.get(name).copied().unwrap_or(0.0)
            - self.rejected.get(name).copied().unwrap_or(0.0)
    }

    fn deltas(&self) -> [f64; FEATURES.len()] {
        std::array::from_fn(|k| self.delta(FEATURES[k].name))
    }

    /// Whether the two shots differ on any feature at all.
    fn informative(&self) -> bool {
        self.deltas().iter().any(|d| d.abs() > EPS)
    }
}

/// Per-feature multipliers on the default weights. A missing feature is 1.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Weights {
    pub multipliers: BTreeMap<String, f64>,
}

impl Weights {
    pub fn get(&self, name: &str) -> f64 {
        self.multipliers.get(name).copied().unwrap_or(1.0)
    }

    /// True when every weight is at its default.
    pub fn is_identity(&self) -> bool {
        self.multipliers.values().all(|m| (m - 1.0).abs() < EPS)
    }
}

/// Score points a set of story signals contributes, via the scorer itself
/// so this never drifts from `score.rs`.
fn bonus_of(signals: StorySignals) -> f64 {
    f64::from(highlight_score(&Score::default(), &signals, false).bonus)
}

/// The story-score features of an asset: what its people, caption, kind,
/// place and motion add at default weights, plus its technical quality above
/// the floor. `score` defaults to the asset's stored score.
pub fn features_of(a: &Asset, score: Option<&Score>) -> Features {
    let s = StorySignals::for_asset(a);
    let quality = score
        .or(a.score.as_ref())
        .map_or(0.0, |sc| f64::from(sc.tech_quality() - TECH_FLOOR))
        .clamp(0.0, f64::from(TECH_SPAN));
    let mut f = Features::new();
    let mut put = |name: &str, v: f64| f.insert(name.to_string(), v);
    put(
        "people",
        bonus_of(StorySignals {
            people_count: s.people_count,
            ..Default::default()
        }),
    );
    put(
        "caption",
        bonus_of(StorySignals {
            has_caption: s.has_caption,
            ..Default::default()
        }),
    );
    put(
        "video",
        bonus_of(StorySignals {
            is_clip: s.is_clip,
            ..Default::default()
        }),
    );
    put(
        "place",
        bonus_of(StorySignals {
            gps_novelty: s.gps_novelty,
            ..Default::default()
        }),
    );
    put(
        "motion",
        bonus_of(StorySignals {
            audio_energy: s.audio_energy,
            movement: s.movement,
            ..Default::default()
        }),
    );
    put("quality", quality);
    f
}

// -- fitting ---------------------------------------------------------------

/// Result of fitting a set of events.
#[derive(Debug, Clone, PartialEq)]
pub struct Fit {
    /// All recorded events.
    pub events: usize,
    /// Events that differ on some feature (the ones that can teach).
    pub usable: usize,
    pub weights: Weights,
}

fn sigmoid(z: f64) -> f64 {
    1.0 / (1.0 + (-z).exp())
}

/// Fit multipliers to `events`. Identity weights below
/// [`MIN_USABLE_EVENTS`]. Independent of input order.
pub fn fit(events: &[PrefEvent]) -> Fit {
    const K: usize = FEATURES.len();
    let mut rows: Vec<(&PrefEvent, [f64; K])> = events
        .iter()
        .filter(|e| e.informative())
        .map(|e| (e, e.deltas()))
        .collect();
    rows.sort_by(|a, b| {
        (a.0.kind, &a.0.chosen_id, &a.0.rejected_id).cmp(&(
            b.0.kind,
            &b.0.chosen_id,
            &b.0.rejected_id,
        ))
    });
    let usable = rows.len();
    let mut out = Fit {
        events: events.len(),
        usable,
        weights: Weights::default(),
    };
    if usable < MIN_USABLE_EVENTS {
        return out;
    }
    // Lipschitz bound of the gradient, so a fixed step always descends.
    let sq: f64 = rows
        .iter()
        .map(|(_, d)| d.iter().map(|x| x * x).sum::<f64>())
        .sum();
    let step = 1.0 / (BETA * BETA / 4.0 * sq + 2.0 * LAMBDA);
    let mut m = [1.0f64; K];
    for _ in 0..ITERATIONS {
        let mut grad: [f64; K] = std::array::from_fn(|k| 2.0 * LAMBDA * (m[k] - 1.0));
        for (_, d) in &rows {
            let z: f64 = BETA * m.iter().zip(d).map(|(m, d)| m * d).sum::<f64>();
            let g = -BETA * sigmoid(-z);
            for k in 0..K {
                grad[k] += g * d[k];
            }
        }
        for k in 0..K {
            m[k] = (m[k] - step * grad[k]).clamp(MIN_MULT, MAX_MULT);
        }
    }
    for (k, def) in FEATURES.iter().enumerate() {
        let v = (m[k] * 100.0).round() / 100.0;
        if (v - 1.0).abs() >= MIN_CHANGE {
            out.weights.multipliers.insert(def.name.to_string(), v);
        }
    }
    out
}

/// Plain-English reasons for each changed multiplier, strongest first.
pub fn explain(events: &[PrefEvent], weights: &Weights) -> Vec<String> {
    let mut changed: Vec<(&FeatureDef, f64)> = FEATURES
        .iter()
        .map(|d| (d, weights.get(d.name)))
        .filter(|(_, m)| (m - 1.0).abs() >= EPS)
        .collect();
    changed.sort_by(|a, b| {
        (b.1 - 1.0)
            .abs()
            .total_cmp(&(a.1 - 1.0).abs())
            .then(a.0.name.cmp(b.0.name))
    });
    changed
        .into_iter()
        .map(|(def, m)| {
            let differing: Vec<&PrefEvent> = events
                .iter()
                .filter(|e| e.informative() && e.delta(def.name).abs() > EPS)
                .collect();
            let agree = |e: &&PrefEvent| (e.delta(def.name) > 0.0) == (m > 1.0);
            let with: Vec<&PrefEvent> = differing.iter().copied().filter(agree).collect();
            let count = |k: EventKind| with.iter().filter(|e| e.kind == k).count();
            let mut parts = vec![];
            for (k, word) in [
                (EventKind::Include, "added"),
                (EventKind::Swap, "swapped in"),
                (EventKind::Exclude, "kept over a removed shot"),
            ] {
                if count(k) > 0 {
                    parts.push(format!("{} {word}", count(k)));
                }
            }
            format!(
                "{} weight x{m:.2}: the shot you kept had {} in {} of {} edits that differed on it ({})",
                def.label,
                if m > 1.0 { def.more } else { def.less },
                with.len(),
                differing.len(),
                parts.join(", ")
            )
        })
        .collect()
}

/// Everything the CLI/UI shows about a project's learned weights.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub fit: Fit,
    pub explanations: Vec<String>,
}

pub fn summarize(events: &[PrefEvent]) -> Summary {
    let fit = fit(events);
    let explanations = explain(events, &fit.weights);
    Summary { fit, explanations }
}

/// Learned weights for a project (identity until enough edits exist).
pub fn project_summary(db: &Db, project_id: &str) -> Result<Summary, String> {
    Ok(summarize(&db.list_preference_events(project_id)?))
}

// -- applying --------------------------------------------------------------

/// `score` with the learned multipliers applied: story bonuses scale per
/// feature, technical quality scales above the floor. Identity weights
/// return `score` unchanged.
pub fn adapt_score(a: &Asset, score: &Score, w: &Weights) -> Score {
    if w.is_identity() {
        return score.clone();
    }
    let x = features_of(a, Some(score));
    let bonus_delta: f64 = FEATURES
        .iter()
        .filter(|d| d.name != "quality")
        .map(|d| (w.get(d.name) - 1.0) * x[d.name])
        .sum();
    let q_delta = (w.get("quality") - 1.0) * x["quality"];
    let mut out = score.clone();
    out.bonus = (f64::from(score.bonus) + bonus_delta).max(0.0) as f32;
    let quality = (f64::from(score.tech_quality()) + q_delta).clamp(0.0, 1.0) as f32;
    out.quality = Some(quality);
    out.total = (quality + out.bonus).clamp(0.0, 1.0);
    out.highlight = out.total >= HIGHLIGHT_THRESHOLD;
    if (out.total - score.total).abs() > f32::EPSILON {
        out.reasons.push("tuned to your edits".to_string());
    }
    out
}

/// Stored scores of `assets` with `w` applied.
pub fn adapt_scores(assets: &[Asset], w: &Weights) -> HashMap<String, Score> {
    assets
        .iter()
        .filter_map(|a| Some((a.id.clone(), adapt_score(a, a.score.as_ref()?, w))))
        .collect()
}

/// Rebuild a project's plan from its stored assets with learned weights
/// applied. Clusters and near-dupe collapse are recomputed from the assets,
/// exactly as at scan time, so identity weights reproduce a scan's plan.
pub fn replan(
    assets: &[Asset],
    w: &Weights,
    overrides: &[PlaceOverride],
    length: FilmLength,
) -> Plan {
    let scores = adapt_scores(assets, w);
    let clusters = cluster_assets(assets);
    let cluster_of: HashMap<String, String> = clusters
        .iter()
        .flat_map(|c| c.asset_ids.iter().map(|id| (id.clone(), c.label.clone())))
        .collect();
    let suppressed = suppressed_ids(&collapse_near_dupes(assets, &scores));
    let plan_assets: Vec<Asset> = assets
        .iter()
        .filter(|a| !suppressed.contains(&a.id))
        .cloned()
        .collect();
    build_plan_with_length(&plan_assets, &cluster_of, &scores, overrides, length)
}

// -- recording -------------------------------------------------------------

/// Events to add and to forget for one change of edits.
#[derive(Debug, Default, PartialEq)]
pub struct EditDelta {
    pub added: Vec<PrefEvent>,
    /// `(kind, chosen_id, rejected_id)`; an empty `rejected_id` matches any
    /// (an undone pin, whatever it displaced).
    pub removed: Vec<(EventKind, String, String)>,
}

fn hero_ids(plan: &Plan) -> Vec<&str> {
    plan.hero.iter().map(|e| e.asset_id.as_str()).collect()
}

/// Preference events implied by changing `before` into `after`. Pure: the
/// caller supplies the stored plan and assets. Additions are replayed one
/// at a time from `before` (swaps, then removals, then pins) so each pin's
/// displaced shot is known; undone edits come back as `removed`.
pub fn events_for_edit(
    base: &Plan,
    assets: &[Asset],
    before: &PlanEdits,
    after: &PlanEdits,
) -> EditDelta {
    let by_id: HashMap<&str, &Asset> = assets.iter().map(|a| (a.id.as_str(), a)).collect();
    let feats = |id: &str| by_id.get(id).map(|a| features_of(a, None));
    let mut delta = EditDelta::default();

    // Undone edits.
    for id in before
        .included
        .iter()
        .filter(|i| !after.included.contains(i))
    {
        delta
            .removed
            .push((EventKind::Include, id.clone(), String::new()));
    }
    for id in before
        .excluded
        .iter()
        .filter(|i| !after.excluded.contains(i))
    {
        delta
            .removed
            .push((EventKind::Exclude, PEERS_ID.to_string(), id.clone()));
    }
    for (f, t) in before.swaps.iter().filter(|s| !after.swaps.contains(s)) {
        delta.removed.push((EventKind::Swap, t.clone(), f.clone()));
    }

    let new_swaps: Vec<&(String, String)> = after
        .swaps
        .iter()
        .filter(|s| !before.swaps.contains(s))
        .collect();
    let mut cur = before.clone();
    let mut push =
        |kind, chosen_id: &str, rejected_id: &str, chosen: Features, rejected: Features| {
            delta.added.push(PrefEvent {
                kind,
                chosen_id: chosen_id.to_string(),
                rejected_id: rejected_id.to_string(),
                chosen,
                rejected,
            });
        };

    for (from, to) in &new_swaps {
        cur.swap(from.clone(), to.clone());
        if let (Some(c), Some(r)) = (feats(to), feats(from)) {
            push(EventKind::Swap, to, from, c, r);
        }
    }

    for id in after
        .excluded
        .iter()
        .filter(|i| !before.excluded.contains(i))
    {
        let was = apply_edits(base, assets, &cur);
        let picked = was
            .hero
            .iter()
            .chain(&was.reel_selects)
            .any(|e| e.asset_id == *id);
        cur.set_excluded(id.clone(), true);
        let (Some(rejected), Some(chapter)) = (feats(id), base.chapter_of(id)) else {
            continue;
        };
        if !picked {
            continue;
        }
        let now = apply_edits(base, assets, &cur);
        let in_film = hero_ids(&now);
        let members = chapter
            .asset_ids
            .iter()
            .map(String::as_str)
            .filter(|m| *m != id);
        let mut peers: Vec<&str> = members.clone().filter(|m| in_film.contains(m)).collect();
        if peers.is_empty() {
            peers = members
                .filter(|m| !cur.excluded.iter().any(|x| x == m))
                .collect();
        }
        let vecs: Vec<Features> = peers.iter().filter_map(|p| feats(p)).collect();
        if vecs.is_empty() {
            continue;
        }
        let mean: Features = FEATURES
            .iter()
            .map(|d| {
                let sum: f64 = vecs
                    .iter()
                    .map(|v| v.get(d.name).copied().unwrap_or(0.0))
                    .sum();
                (d.name.to_string(), sum / vecs.len() as f64)
            })
            .collect();
        push(EventKind::Exclude, PEERS_ID, id, mean, rejected);
    }

    // A swap of a pinned shot moves the pin to the replacement; that is the
    // swap's event, not a second include.
    let pins = after
        .included
        .iter()
        .filter(|i| !before.included.contains(i) && !new_swaps.iter().any(|(_, t)| t == *i));
    for id in pins {
        let was = apply_edits(base, assets, &cur);
        cur.set_included(id.clone(), true);
        let now = apply_edits(base, assets, &cur);
        let kept = hero_ids(&now);
        let Some(chosen) = feats(id) else { continue };
        for gone in hero_ids(&was)
            .into_iter()
            .filter(|g| *g != id && !kept.contains(g))
        {
            if let Some(rejected) = feats(gone) {
                push(EventKind::Include, id, gone, chosen.clone(), rejected);
            }
        }
    }
    delta
}

/// Record the preference events of an edit (`before` -> `after`) for a
/// project. A no-op when the pins, removals and swaps did not change, or
/// when the project has no plan yet.
pub fn record_edit(
    db: &Db,
    project_id: &str,
    before: &PlanEdits,
    after: &PlanEdits,
) -> Result<(), String> {
    if before.excluded == after.excluded
        && before.included == after.included
        && before.swaps == after.swaps
    {
        return Ok(());
    }
    let Some((_, base)) = db.latest_plan_with_project(Some(project_id))? else {
        return Ok(());
    };
    let assets = db.list_assets(project_id)?;
    let delta = events_for_edit(&base, &assets, before, after);
    db.in_transaction(|db| {
        for (kind, chosen, rejected) in &delta.removed {
            if rejected.is_empty() {
                // An undone pin: whatever it displaced.
                for e in db.list_preference_events(project_id)? {
                    if e.kind == *kind && e.chosen_id == *chosen {
                        db.delete_preference_event(project_id, *kind, chosen, &e.rejected_id)?;
                    }
                }
            } else {
                db.delete_preference_event(project_id, *kind, chosen, rejected)?;
            }
        }
        for e in &delta.added {
            db.put_preference_event(project_id, e)?;
        }
        Ok(())
    })
}

/// Record events for edits made before learning existed, by replaying a
/// project's stored edits from scratch against its latest plan.
pub fn backfill(db: &Db, project_id: &str) -> Result<(), String> {
    let edits = db.get_plan_edits(project_id)?;
    record_edit(db, project_id, &PlanEdits::default(), &edits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::build_plan;
    use crate::types::AssetKind;
    use std::path::PathBuf;

    fn feat(pairs: &[(&str, f64)]) -> Features {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn ev(chosen: &[(&str, f64)], rejected: &[(&str, f64)]) -> PrefEvent {
        PrefEvent {
            kind: EventKind::Swap,
            chosen_id: "c".into(),
            rejected_id: "r".into(),
            chosen: feat(chosen),
            rejected: feat(rejected),
        }
    }

    /// `n` events where the kept shot has a group and the other nobody,
    /// with pseudo-random quality noise that favours neither side.
    fn group_events(n: usize, favour_group: bool) -> Vec<PrefEvent> {
        let mut seed = 12345u32;
        let mut noise = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            f64::from(seed >> 16) / 65536.0 * 0.2
        };
        (0..n)
            .map(|i| {
                let (qa, qb) = (noise(), noise());
                let (g, none) = (("people", 0.28), ("people", 0.0));
                let (c, r) = if favour_group { (g, none) } else { (none, g) };
                let mut e = ev(&[c, ("quality", qa)], &[r, ("quality", qb)]);
                e.chosen_id = format!("c{i}");
                e
            })
            .collect()
    }

    #[test]
    fn fit_raises_the_weight_the_owner_keeps_choosing() {
        let f = fit(&group_events(20, true));
        assert_eq!(f.usable, 20);
        assert!(f.weights.get("people") > 1.2, "{:?}", f.weights);
        // Never differed, so never moved.
        assert_eq!(f.weights.get("caption"), 1.0);
        assert_eq!(f.weights.get("video"), 1.0);
    }

    #[test]
    fn fit_lowers_a_weight_the_owner_keeps_overriding() {
        let f = fit(&group_events(20, false));
        assert!(f.weights.get("people") < 0.8, "{:?}", f.weights);
    }

    #[test]
    fn fit_waits_for_the_minimum_event_count() {
        let few = group_events(MIN_USABLE_EVENTS - 1, true);
        assert!(fit(&few).weights.is_identity());
        assert!(!fit(&group_events(MIN_USABLE_EVENTS, true))
            .weights
            .is_identity());
        // Events that cannot teach anything do not count toward the minimum.
        let mut padded = few;
        padded.extend((0..20).map(|_| ev(&[("people", 0.1)], &[("people", 0.1)])));
        let f = fit(&padded);
        assert_eq!(
            (f.events, f.usable),
            (MIN_USABLE_EVENTS - 1 + 20, MIN_USABLE_EVENTS - 1)
        );
        assert!(f.weights.is_identity());
    }

    #[test]
    fn fit_is_regularised_and_bounded() {
        // A handful of events moves a weight, but gently.
        let small = fit(&group_events(MIN_USABLE_EVENTS, true))
            .weights
            .get("people");
        assert!(small > 1.0 && small < 1.5, "{small}");
        // More evidence moves it further, never past the bounds.
        let big = fit(&group_events(2000, true)).weights.get("people");
        assert!(big > small && big <= MAX_MULT, "{big}");
        let low = fit(&group_events(2000, false)).weights.get("people");
        assert!((MIN_MULT..1.0).contains(&low), "{low}");
        for m in fit(&group_events(2000, true)).weights.multipliers.values() {
            assert!((MIN_MULT..=MAX_MULT).contains(m));
        }
    }

    #[test]
    fn fit_is_deterministic_and_order_independent() {
        let mut events = group_events(25, true);
        let a = fit(&events);
        assert_eq!(a, fit(&events));
        events.reverse();
        assert_eq!(a, fit(&events));
    }

    #[test]
    fn explanation_says_which_weight_and_why() {
        let events = group_events(20, true);
        let s = summarize(&events);
        let line = s
            .explanations
            .iter()
            .find(|l| l.starts_with("people"))
            .unwrap();
        assert!(line.contains("weight x1."), "{line}");
        assert!(line.contains("more people in 20 of 20 edits"), "{line}");
        assert!(line.contains("20 swapped in"), "{line}");
        assert!(summarize(&[]).explanations.is_empty());
    }

    fn photo(id: &str, i: i64, people: u32, quality: f32) -> Asset {
        let mut a = Asset::new(id, PathBuf::from(format!("/t/{id}.jpg")), AssetKind::Photo);
        a.taken_at = Some(1_714_557_600 + i * 600);
        a.people_count = Some(people);
        let bonus = crate::score::people_bonus(people);
        a.score = Some(Score {
            total: 0.3 + 0.45 * quality + bonus,
            quality: Some(0.3 + 0.45 * quality),
            bonus,
            ..Default::default()
        });
        a
    }

    /// Project of one chapter: a(0 people) b(1) c(3) d(0, best quality) e(2).
    /// Hero starts as a, b, c.
    fn db_with_chapter() -> (Db, String, Vec<Asset>) {
        let db = Db::open_in_memory().unwrap();
        let p = db.create_project("trip", "/t").unwrap();
        let assets = vec![
            photo("a", 0, 0, 0.5),
            photo("b", 1, 1, 0.5),
            photo("c", 2, 3, 0.5),
            photo("d", 3, 0, 0.9),
            photo("e", 4, 2, 0.5),
        ];
        for a in &assets {
            db.upsert_asset(&p.id, a).unwrap();
        }
        let cluster_of = assets
            .iter()
            .map(|a| (a.id.clone(), "2024-05-01".to_string()))
            .collect();
        let scores = assets
            .iter()
            .map(|a| (a.id.clone(), a.score.clone().unwrap()))
            .collect();
        let mut plan = build_plan(&assets, &cluster_of, &scores);
        plan.hero
            .retain(|e| ["a", "b", "c"].contains(&e.asset_id.as_str()));
        plan.reel_selects.clear();
        db.insert_plan(&p.id, &plan, "local").unwrap();
        (db, p.id, assets)
    }

    fn events(db: &Db, pid: &str) -> Vec<(EventKind, String, String)> {
        db.list_preference_events(pid)
            .unwrap()
            .into_iter()
            .map(|e| (e.kind, e.chosen_id, e.rejected_id))
            .collect()
    }

    #[test]
    fn pin_records_a_win_over_the_shot_it_displaced() {
        let (db, pid, _) = db_with_chapter();
        let mut edits = PlanEdits::default();
        edits.set_included("e".into(), true);
        db.save_plan_edits(&pid, &edits).unwrap();
        // The weakest unpinned hero pick (a: no people, ties break by order).
        assert_eq!(
            events(&db, &pid),
            [(EventKind::Include, "e".into(), "a".into())]
        );
        let ev = &db.list_preference_events(&pid).unwrap()[0];
        assert_eq!(
            ev.chosen["people"],
            f64::from(crate::score::people_bonus(2))
        );
        assert_eq!(ev.rejected["people"], 0.0);
        // Saving unrelated edits records nothing more; undoing forgets it.
        edits.set_caption("a".into(), Some("hi".into()));
        db.save_plan_edits(&pid, &edits).unwrap();
        assert_eq!(events(&db, &pid).len(), 1);
        edits.set_included("e".into(), false);
        db.save_plan_edits(&pid, &edits).unwrap();
        assert!(events(&db, &pid).is_empty());
    }

    #[test]
    fn backfill_replays_edits_made_before_learning() {
        let (db, pid, _) = db_with_chapter();
        let mut edits = PlanEdits::default();
        edits.set_included("e".into(), true);
        edits.swap("b".into(), "d".into());
        // As if an older build had saved them: edits without events.
        db.save_plan_edits(&pid, &edits).unwrap();
        db.clear_preference_events(&pid).unwrap();
        assert!(events(&db, &pid).is_empty());
        backfill(&db, &pid).unwrap();
        let got = events(&db, &pid);
        assert!(
            got.contains(&(EventKind::Swap, "d".into(), "b".into())),
            "{got:?}"
        );
        assert!(
            got.iter()
                .any(|(k, c, _)| *k == EventKind::Include && c == "e"),
            "{got:?}"
        );
        // Idempotent.
        backfill(&db, &pid).unwrap();
        assert_eq!(events(&db, &pid), got);
    }

    #[test]
    fn swap_records_replacement_over_original() {
        let (db, pid, _) = db_with_chapter();
        let mut edits = PlanEdits::default();
        edits.swap("b".into(), "d".into());
        db.save_plan_edits(&pid, &edits).unwrap();
        assert_eq!(
            events(&db, &pid),
            [(EventKind::Swap, "d".into(), "b".into())]
        );
        let ev = &db.list_preference_events(&pid).unwrap()[0];
        assert!(ev.chosen["quality"] > ev.rejected["quality"]);
        assert!(ev.rejected["people"] > ev.chosen["people"]);
    }

    #[test]
    fn swapping_a_pinned_shot_is_one_swap_event_not_an_include() {
        let (db, pid, _) = db_with_chapter();
        let mut edits = PlanEdits::default();
        edits.set_included("e".into(), true);
        db.save_plan_edits(&pid, &edits).unwrap();
        edits.swap("e".into(), "d".into());
        db.save_plan_edits(&pid, &edits).unwrap();
        // The pin moved to d: e's include is forgotten, the swap recorded.
        assert_eq!(
            events(&db, &pid),
            [(EventKind::Swap, "d".into(), "e".into())]
        );
    }

    #[test]
    fn exclude_records_a_loss_against_the_chapter_peers() {
        let (db, pid, _) = db_with_chapter();
        let mut edits = PlanEdits::default();
        edits.set_excluded("a".into(), true);
        db.save_plan_edits(&pid, &edits).unwrap();
        assert_eq!(
            events(&db, &pid),
            [(EventKind::Exclude, PEERS_ID.into(), "a".into())]
        );
        let ev = &db.list_preference_events(&pid).unwrap()[0];
        // Peers still in the film: b (1 person) and c (3).
        let want = f64::from(crate::score::people_bonus(1) + crate::score::people_bonus(3)) / 2.0;
        assert!((ev.chosen["people"] - want).abs() < 1e-6, "{ev:?}");
        // Excluding a shot the film never picked teaches nothing.
        edits.set_excluded("d".into(), true);
        db.save_plan_edits(&pid, &edits).unwrap();
        assert_eq!(events(&db, &pid).len(), 1);
        // Restoring forgets the event.
        edits.set_excluded("a".into(), false);
        db.save_plan_edits(&pid, &edits).unwrap();
        assert!(events(&db, &pid).is_empty());
    }

    #[test]
    fn events_survive_the_edit_that_made_them_and_reset_clears_them() {
        let (db, pid, _) = db_with_chapter();
        let mut edits = PlanEdits::default();
        edits.swap("b".into(), "d".into());
        edits.set_excluded("a".into(), true);
        db.save_plan_edits(&pid, &edits).unwrap();
        assert_eq!(events(&db, &pid).len(), 2);
        let other = db.create_project("other", "/o").unwrap();
        assert!(db.list_preference_events(&other.id).unwrap().is_empty());
        assert_eq!(db.clear_preference_events(&pid).unwrap(), 2);
        assert!(events(&db, &pid).is_empty());
        let s = project_summary(&db, &pid).unwrap();
        assert!(s.fit.weights.is_identity());
    }

    #[test]
    fn migration_adds_the_events_table_to_older_databases() {
        use rusqlite::Connection;
        let path = std::env::temp_dir().join(format!("triptape-learn-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE projects (id TEXT PRIMARY KEY, name TEXT NOT NULL, folder TEXT NOT NULL,
                 created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
                 cloud_consent INTEGER NOT NULL DEFAULT 0, book_preset TEXT, music_pick TEXT);",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let p = db.create_project("t", "/t").unwrap();
        db.put_preference_event(&p.id, &ev(&[("people", 0.1)], &[]))
            .unwrap();
        assert_eq!(db.list_preference_events(&p.id).unwrap().len(), 1);
        drop(db);
        // Reopening is idempotent and keeps the data.
        let db = Db::open(&path).unwrap();
        assert_eq!(db.list_preference_events(&p.id).unwrap().len(), 1);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    #[test]
    fn identity_weights_change_nothing_and_learned_weights_reorder() {
        let (_, _, assets) = db_with_chapter();
        let raw = assets[2].score.clone().unwrap();
        assert_eq!(adapt_score(&assets[2], &raw, &Weights::default()), raw);
        let mut w = Weights::default();
        w.multipliers.insert("people".into(), 2.0);
        let boosted = adapt_score(&assets[2], &raw, &w); // 3 people
        assert!((boosted.bonus - 2.0 * crate::score::people_bonus(3)).abs() < 1e-6);
        assert!(boosted.total > raw.total);
        assert_eq!(boosted.reasons.last().unwrap(), "tuned to your edits");
        // No people: unchanged by a people weight.
        let none = assets[0].score.clone().unwrap();
        assert_eq!(adapt_score(&assets[0], &none, &w).total, none.total);
        // Quality scales above the floor only.
        let mut wq = Weights::default();
        wq.multipliers.insert("quality".into(), 2.0);
        let q = adapt_score(&assets[2], &assets[2].score.clone().unwrap(), &wq);
        let base_q = assets[2].score.as_ref().unwrap().tech_quality();
        assert!((q.tech_quality() - (base_q + (base_q - TECH_FLOOR))).abs() < 1e-5);
    }

    #[test]
    fn replan_reproduces_a_plain_build_and_learned_weights_change_scores() {
        let (_, _, assets) = db_with_chapter();
        let cluster_of: HashMap<String, String> = assets
            .iter()
            .map(|a| (a.id.clone(), "2024-05-01".to_string()))
            .collect();
        let scores: HashMap<String, Score> = assets
            .iter()
            .map(|a| (a.id.clone(), a.score.clone().unwrap()))
            .collect();
        let plain = build_plan(&assets, &cluster_of, &scores);
        let same = replan(&assets, &Weights::default(), &[], FilmLength::Auto);
        assert_eq!(plain.to_json().unwrap(), same.to_json().unwrap());
        assert!(!same.hero.is_empty());
        // Replanning never reads or writes edits.
        let mut w = Weights::default();
        w.multipliers.insert("quality".into(), 2.0);
        let tuned = adapt_scores(&assets, &w);
        assert!(tuned["d"].total > scores["d"].total);
        assert_eq!(
            tuned["a"].total,
            scores["a"].total + (scores["a"].tech_quality() - TECH_FLOOR)
        );
    }
}
