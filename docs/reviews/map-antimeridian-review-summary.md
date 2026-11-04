# Code Review Summary

## Overview

The original change unwrapped longitudes so antimeridian-crossing routes would frame correctly on the route card, but that fix never reached exported cards. The export path still removed every jump over 1,000 km before drawing, so a Los Angeles -> Tokyo trip lost its Asian stops. The review loop fixed this by moving GPS outlier filtering into plan construction, where capture times can separate real flights from bad fixes. It also tightened the new code and added regression tests for the export path and the unwrapping behaviour.

## Improvements Made

- **Long-haul legs now survive export (High).** Saving a route card no longer applies the distance-only 1,000 km filter, which could not tell a flight from a GPS glitch. Without this change, the transpacific case the feature was meant to fix would still have been wrong in exported cards.
- **Outlier filtering now uses capture times.** The plan's map polyline now excludes fixes flagged by the existing speed-based outlier check. An 8,800 km hop in 12 hours stays in as a flight, while a jump to Sydney one hour later is dropped. The filter runs when the polyline is built because the polyline itself carries no timestamps.
- **Obsolete filter removed.** The distance-only outlier function and its test are gone, so it can't be reintroduced by accident. A doc comment on the save function now says the route is drawn as given and that the plan handles filtering.
- **Regression coverage closed the gaps.** New tests cover:
  - the plan polyline keeping flights while dropping impossible jumps
  - saved cards matching the directly drawn card for a route with long-haul legs
  - longitude unwrapping: same-hemisphere pass-through, empty routes, eastbound and westbound antimeridian hops, return trips, and latitude preservation
- **Smaller cleanups.** The unwrap formula now uses a simpler, equivalent constant (`+ 180.0` instead of `+ 540.0`), and a comment's span figure was corrected from 259° to 258°. Duplicated land and water pixel counting in the tests moved into a shared helper.

## Created Files

None.

## Remaining Items

None.

## Files Changed

- `src-tauri/crates/triptape-core/src/plan.rs`
- `src-tauri/crates/triptape-media/src/map.rs`

## Metrics

- **Cycles run:** 1
- **Duration:** 6m 17s
- **Initial review issues:** High 1, Medium 0, Low 0
- **Final issue counts:** High 0, Medium 0, Low 0
- **Final status:** CLEAN - all issues resolved
