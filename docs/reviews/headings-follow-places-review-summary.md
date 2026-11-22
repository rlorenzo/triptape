# Code Review Summary

## Overview

The original change gave the film a new lower third each time it moves to another place inside a chapter. The review loop fixed two bugs in that work. Headings could show the wrong place when a chapter resumed on a shot that was itself a place change. Shots with no GPS that followed a GPS-less named place were also measured from the place before it. The loop also added a regression test for drifting away from a named place and back, and moved repeated test setup into one helper. It finished clean after one cycle.

## Improvements Made

- **Place changes take priority over chapter starts.** Before, a shot that started a chapter always took the chapter's first-place heading. If the chapter resumed on a shot that moves to a new place, the film named the wrong place. The heading logic now checks for a place change first, on the shot itself and on any collage members. It falls back to the chapter's default heading only when neither has one.
- **Anchors reset when the place changes.** The proximity anchor carried forward with `a.gps.or(anchor)` even across place transitions. A named place with no GPS of its own, such as a time-window place, therefore kept the previous place's coordinates, and later shots were measured from the wrong spot. A new place now takes its anchor only from its own shot. Carry-forward still applies within the same place.
- **Regression test for leaving a heading and returning.** A new test covers a shot near a named garden, one just outside its radius, one several km away, and one back at the garden. It checks that the film stays on the garden while the shots are near it, switches back to the city when far away, and returns to the garden, and that `film_place_changes` and `film_places()` record exactly those switches.
- **Shared test fixture.** A `hero_plan` helper now builds the still-only hero plan that tests had been writing out inline.
- **Simpler heading lookup.** The collage-member search is now one iterator chain over the shot plus its members, with no separate branch for single shots.

## Created Files

None.

## Remaining Items

None. Both review findings (one Medium, one Low) were resolved and the final review found no new issues.

## Files Changed

- `src-tauri/crates/triptape-core/src/itinerary.rs`
- `src-tauri/crates/triptape-media/src/render_video.rs`

## Metrics

- Cycles run: 1
- Duration: 17m 41s
- Initial findings: High 0, Medium 1, Low 1
- Final findings: High 0, Medium 0, Low 0
- Final status: CLEAN
