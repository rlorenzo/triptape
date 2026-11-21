# Agent Review Summary — Split Days

## Overview

The split-days work arrived largely sound, so the review loop added no new behaviour. It ran one refinement pass that cut duplication and put rules that had been copied between modules in one place each. The main change is that the core crate and the CLI now share one validation function for same-day film moves. Before, each had its own copy of the rules, and the copies could drift apart. The loop also swapped several hand-written index loops for standard slice helpers and dropped an unneeded asset clone from plan building. The reviewer then found no High or Medium issues, raised three Low notes, and passed the change.

## Improvements Made

- **One set of film-move rules.** The CLI's `edit` command and `PlanEdits` application each checked for themselves that a move's shot and anchor were both in the film, were different shots, and fell on the same capture-local day. The loop moved these checks into a single `move_rejection` function. It returns a typed `MoveRejection` reason: `NotInFilm { anchor }`, `SameShot` or `OtherDay`. The CLI turns each reason into its error message, and edit application skips any move that gets a reason. A CLI rejection and a silently dropped saved edit can no longer disagree. A new unit test covers each rejection variant and the accepted case.
- **A shared capture-day helper.** Six or more call sites spelled out `day_key(a.taken_at, a.tz_offset_secs)` in full. A new `cluster::day_of(&Asset)` replaces them, and `same_day` now uses it too. This removes a small but real chance of one site passing the wrong timezone field.
- **Simpler day-run iteration.** Story layout and the per-day hero pass split the timeline into same-day runs with hand-written `while`/index loops. These became `partition_point` plus `chunk_by`, which removed off-by-one-prone bookkeeping. A duplicated closure that added a run to the story as shared became the `StoryLayout::push_shared` method.
- **No asset clones in plan building.** `cluster_by_capture` now accepts any iterator of `&Asset`. Plan building therefore stopped cloning every asset on a split day into a temporary `Vec<Asset>`. The `capture_unit` map also lost an empty-set special case and a `get_key_value` lookup used to borrow keys.
- **Reuse of the existing chapter lookup.** The CLI searched every chapter linearly for each shot it printed. It now uses the existing `plan::chapter_index` map. It also borrows the plan's hero list instead of moving it out of the plan.

## Created Files

None.

## Remaining Items

The final review raised three Low findings. The loop did not apply them and they are still open:

1. **Order of a floating-point sum in hero budget weighting.** `select_hero_by_budget` keeps per-unit counts in a `HashMap` and sums `f64` weights in that map's random iteration order. When chapter weights tie, the result could differ by one ULP from run to run. Fix: switch to `BTreeMap`.
2. **Longitude interpolation across the antimeridian.** `party::position` interpolates longitude linearly. On a route that crosses ±180°, an interpolated point can land near 0°. That could make two people look far apart and wrongly split a sea day. Fix: interpolate along the shortest wrapped longitude delta.
3. **Where unmapped book photos sort.** The book sort uses `Option<usize>` as its key, so a photo missing from the story order would sort first instead of last. That can't happen today, but `plan.rs` uses `unwrap_or(usize::MAX)` for the same case, and the book sort should match.

## Files Changed

- `src-tauri/crates/triptape-core/src/cluster.rs`
- `src-tauri/crates/triptape-core/src/party.rs`
- `src-tauri/crates/triptape-core/src/plan.rs`
- `src-tauri/crates/triptape-media/src/bin/triptape.rs`

## Metrics

- Review-fix cycles: 0 (one refinement pass, then one review)
- Duration: 11m 56s
- Final status: CLEAN
- Final issue counts: High 0, Medium 0, Low 3
