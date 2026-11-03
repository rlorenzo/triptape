# Agent Review Summary

## Overview

The original change made chapter titles readable (`2026-10-12 · place 21` becomes `Mon, Oct 12`) and stopped hero overlays from showing the date twice when a caption was already a plan dateline. The review loop tightened how a dateline is recognized. The original check treated any caption whose last segment contained a colon and a digit as a dateline, so user prose like `Ferry at 3:00` or `Dinner · 7:30 PM` lost its date line. The loop replaced that check with a strict parser that matches the exact format the plan builder emits. Overlays and the photo book now share that one parser, and the loop added a shared date-format constant and regression tests.

## Improvements Made

- **Fixed caption misclassification (the Medium finding).** Hero overlays used a loose heuristic to decide whether a caption was a dateline. Any caption ending in something time-like was treated as one, so a user caption such as `Ferry at 3:00` or `Dinner · 7:30 PM` dropped its capture date, or was split into a bogus day/time pair. A new `split_dateline` parser accepts only the exact plan formats: an optional `Day N · ` prefix, then a `Mon, Oct 12`-style weekday date (weekday and month names checked, day 1–2 digits), then a `3:08 AM`-style clock time (12-hour, two-digit minutes, AM/PM). Everything else falls through to the normal date-led layout.
- **One dateline definition for video and book.** The photo-book renderer had its own copy of the same loose heuristic. That copy now delegates to the shared parser, so the hero video and the book always agree on what counts as a dateline, and the book gets the same fix.
- **Simpler overlay logic.** The overlay path now destructures the parser's result directly. This removed the separate filter-then-resplit steps and the empty-time edge case. The date is also computed only when the caption is not a dateline.
- **Shared date format.** Chapter titles and caption datelines now share one `DAY_FORMAT` constant, so the two can't drift apart. `human_chapter_title` was also condensed to use `split_once` and `map_or_else`.
- **Regression tests.** New overlay tests check that prose containing a clock time (`Ferry at 3:00`) and a user caption ending in a time (`Dinner · 7:30 PM`) are not treated as datelines and still lead with the capture date.

## Created Files

None.

## Remaining Items

None. The final review reported no findings.

## Files Changed

- `src-tauri/crates/triptape-core/src/plan.rs`
- `src-tauri/crates/triptape-media/src/render_book.rs`
- `src-tauri/crates/triptape-media/src/render_video.rs`
- `src-tauri/crates/triptape-media/src/titlecard.rs`

## Metrics

- Cycles run: 1 (plus an initial refinement pass)
- Duration: 4m 1s
- Initial review: High 0, Medium 1, Low 0
- Final review: High 0, Medium 0, Low 0 (CLEAN)
