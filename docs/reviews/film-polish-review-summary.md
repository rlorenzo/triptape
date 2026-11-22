# Agent Review Summary

## Overview

The original change made captioned shots in the film hold long enough to read, and gave the places card a duration that grows with the number of names on it. The review loop found three timing gaps in that work. Subtitle cues still disappeared on the old fixed schedule while their shots now stayed on screen longer. The new reading time could push past short mode's per-chapter budget cap. And collages scaled the reading time a second time, so a hold could run past the caption maximum. One cycle fixed all three, and the final review came back clean.

## Improvements Made

- **Subtitles match the longer shot holds.** The SRT writer still gave each cue a flat 3.5 s per line, so on a held caption shot the subtitle vanished while the image stayed up. Each cue now uses the same caption reading time as the shot (with at least 3.5 s per line for multi-line collage cues), still capped just short of the segment's end.
- **Reading time respects short mode's budgets.** The reading time was limited by the group's budget but not by its per-clip cap. A long caption could freeze-pad a clip that had just been trimmed to the cap, or take time from later chapters. It is now bounded by both the cap and the budget.
- **Collage holds stop overrunning the caption maximum.** The collage renderer multiplies its `still_secs` input by a per-tile factor. Raising `still_secs` to the reading time before that step meant the reading time got multiplied too, which pushed collages past `CAPTION_MAX_SECS`. The collage now gets the reading time divided by that factor, so its final length is the larger of the normal scaled hold and the reading time. Single-entry groups still raise `still_secs` directly.
- **Simpler caption lookup and a named constant.** In the refinement pass, the single-entry and multi-entry branches were merged into one call to the collage caption helper, because a lone entry's collage caption is its own caption, trimmed. The places card's literal 3 s base time also became the named constant `CREDITS_BASE_SECS`.

## Created Files

None.

## Remaining Items

None. The final review reported no open findings.

## Files Changed

- `src-tauri/crates/triptape-media/src/render_video.rs`

## Metrics

- Cycles run: 1 (plus an initial refinement pass)
- Duration: 12m 5s
- Initial review findings: High 1, Medium 2, Low 0
- Final issue counts: High 0, Medium 0, Low 0
- Final status: CLEAN
