# Agent Review Summary

## Overview

The review loop took the new `pick` command, which copies a trip's best photos into a folder, and fixed its correctness and consistency problems. The original work defined its own "same scene" rule and checked captions in its own way. The loop removed that duplicate logic so the pick reuses the film planner's same-moment rule and its caption check. It also fixed a naming bug that could split a Live Photo still from its motion file. Finally, it made the CLI handle boolean flags and output-directory errors properly. One review-and-fix cycle brought the findings to zero.

## Improvements Made

- **One definition of "same moment".** The pick had its own copy of the burst, time-window and perceptual-hash thresholds (`SAME_SCENE_SECS`, `BURST_SECS`, `SAME_SCENE_PHASH_DISTANCE`), which could drift away from the planner's values. The planner's check now lives in a shared helper that works on `(taken_at, phash)` pairs, and the pick calls it. The film and the photo pick now agree on which shots are duplicates.
- **Whitespace-only captions no longer count as curation.** A caption made only of whitespace used to earn the "curated" score bonus and win the tie-break between copies. The pick now uses the planner's `has_caption` check, so blank captions are ignored everywhere. A new test covers this.
- **Live Photo pairs keep a matching name.** The still and its motion file used to get collision-free names separately. If only one of them collided, the pair could land as `IMG_1-2.HEIC` next to `IMG_1.MOV`, and apps match Live Photos by file name. A new `free_names` helper reserves one stem for both files. It also handles the edge case where the motion file has the same extension as the still. Tests cover collisions on either side of the pair.
- **`--stills-only=false` now means false.** Before, the flag was on whenever the key was present, so passing `false` still turned it on. Now any value other than `false` turns it on.
- **The output-folder check reports I/O errors.** Before, any error reading `--out` was treated as "empty", so a folder you can't read (for example, one with a permissions error) was treated as fine to write into. Now a missing folder is allowed, a non-empty folder is rejected, and any other error stops the run with a clear message.
- **Small cleanups and test coverage.** The filter for usable photos now runs once and the scoring and pool-building steps share the result, instead of each running the filter again. A new test checks that photos with no hash and no size still get picked.

## Created Files

These files were added by a browser session during the loop. They are tool artifacts, not code, and are staged by mistake. Remove them or add them to `.gitignore` before committing:


## Remaining Items

- No review findings are open.

## Files Changed

- `src-tauri/crates/triptape-core/src/pick.rs`
- `src-tauri/crates/triptape-core/src/plan.rs`
- `src-tauri/crates/triptape-media/src/bin/triptape.rs`

## Metrics

- Cycles run: 1
- Duration: 9m 35s
- Initial review findings: High 0, Medium 1, Low 2
- Final review findings: High 0, Medium 0, Low 0
- Final status: CLEAN, all issues resolved
