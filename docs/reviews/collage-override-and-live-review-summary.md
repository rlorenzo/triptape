# Code Review Summary

## Overview

The original change let a film collage include captioned photos and Live Photos when the user explicitly joins them, and added a `--live` edit to play a photo as a Live Photo. The review loop tightened the rules for which entries join a collage. Stale links could reach across chapters, and an unjoined entry sitting between two linked photos could block their join; both are fixed. It also made the CLI check `--live` targets before saving, and removed duplicated logic so the "plays alone" and "collage candidacy" rules now come from one place. One cycle settled both medium findings, and the final review came back clean.

## Improvements Made

- **Collage joins stay inside their chapter.** Before the fix, an `IfJoined` entry (captioned or motion) counted as eligible if its neighbour in a flat candidate list linked to it, even when that neighbour belonged to another chapter. A stale link left over from an earlier plan could activate an entry across a chapter boundary. Eligibility is now found by scanning outward from the entry, and the scan stops at a chapter boundary or at the first plain collage photo.
- **Unjoined entries no longer block a valid join.** The old neighbour-only check meant an unjoined captioned photo placed between two linked photos broke the link. The outward scan steps over entries that play on their own, so the user's join still holds. New tests cover this case and the stale cross-chapter case.
- **`--live` targets are checked against the film.** `edit --live` checked only that the asset was a Live Photo with motion. It never checked that the asset was in the current film, so it could save an edit that did nothing. Live targets now go through the same "is in the film" check as solo targets, before anything is saved.
- **One source of truth for collage rules.** The original added a `collage_candidacy` match that repeated the logic of `plays_alone`, plus a separate per-hero `candidacy` vector. The review replaced both with `Candidacy::of(plays_alone(...))`, deleted the redundant `collage_eligible` helper, and removed the extra cached vector from `HeroGrouping`.
- **Panorama precedence made consistent.** `plays_alone` now reports `Panorama` before `Caption`, so a captioned panorama is labelled for the real reason it can never join: a caption doesn't make a panorama joinable. The doc comment and test were updated to match.
- **`Asset::has_live_motion()` helper.** The check "Live Photo with a companion video" was written out in four places across planning and the CLI. It is now one method, which removes a source of drift.

## Created Files

None.

## Remaining Items

None. The final review found no issues.

## Files Changed

- `src-tauri/crates/triptape-core/src/plan.rs`
- `src-tauri/crates/triptape-core/src/types.rs`
- `src-tauri/crates/triptape-media/src/bin/triptape.rs`

## Metrics

- Cycles run: 1
- Duration: 25m 15s
- Initial review: High 0, Medium 2, Low 0
- Final issue counts: High 0, Medium 0, Low 0 (CLEAN)
