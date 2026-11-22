# Agent Review Summary

## Overview

The original change moved the travel map's final "home" hop so it plays before the first shot taken back home, instead of after every shot. The review loop fixed one correctness gap in that logic. An undated hop home could be placed ahead of a back-home shot, which disagreed with how `hops_reached` counts hops. The loop also cleaned up repeated longitude-wrapping code and added tests for the edge cases. It finished clean after one cycle.

## Improvements Made

- **Undated hops home now close the trip.** The original condition used `hop.day.is_none_or(...)`. With that condition, a home hop with no day matched any shot at the home location, so the map could jump home early. `hops_reached` never counts an undated hop, so the cut position and the hop count disagreed. The fix only searches for a back-home shot when the hop has a day. An undated hop falls back to `shots.len()`, so it plays after every shot. The doc comment now covers this case.
- **A departure shot at the home airport no longer pulls the hop home to the start.** The search now requires each candidate shot to be at home and dated on or after the hop's day. A new test checks that a shot taken at the home airport on departure leaves the home hop at its correct late position.
- **Longitude wrapping is shared.** The `(lon + 180).rem_euclid(360) - 180` expression appeared in production code and twice in tests. It is now one `wrapped()` helper, which `Journey` construction and the ship-path water tests both call.
- **Tests cover the new branches.** New assertions cover an undated hop home (cut at `shots.len()`, and `hops_reached` stays at 9) and the home-airport departure shot.

## Created Files

None

## Remaining Items

None. The final review reported no unresolved findings.

## Files Changed

- `src-tauri/crates/triptape-core/src/journey.rs`

## Metrics

- Cycles run: 1
- Duration: 7m 47s
- Final issue counts: High 0, Medium 0, Low 0 (initial review: 1 Medium)
- Final status: CLEAN
