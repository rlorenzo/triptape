# Code Review Summary

## Overview

The review loop hardened the new Story editor, mainly its swap handling. Edits now replay correctly, invalid replacements are rejected by the backend, and the UI no longer shows stale data or discards unsaved input. The loop also corrected the claim that edits survive rescans and merged duplicated plan-loading and asset-mapping code. One low-severity issue in the browser preview remains open.

## Improvements Made

- **Duplicate and invalid swaps are rejected.** The backend previously accepted any swap, and a replacement that was already picked silently shortened the video or reel. It now rejects swaps that target the same asset, replace something that isn't a current pick, use a replacement already in that keepsake, or move a pick to another chapter. The UI mirrors these rules by offering Replace only on current picks and disabling replacements already in that keepsake.
- **Swap history replays correctly.** Multi-step edits could produce the wrong picks because a new swap always discarded the earlier swap of the same asset. That earlier swap is now kept when replay still needs it to vacate its original slot, as in `a→x`, `b→a`, `a→y`.
- **Removing a replacement no longer revives the original pick.** A removed replacement used to be skipped during replay, which brought back the pick it had replaced. Swaps now apply first and exclusions afterward, so the removal holds and restoring the replacement brings the swap back.
- **Project switches no longer show stale data.** Late responses from a previous project are now ignored, and switching projects clears the story, errors, and open alternatives. Loads wait behind pending edits, so reopening a project can't read a pre-edit story. Failed saves still show an error after a switch, because that edit was lost.
- **The alternatives list can't show stale results.** The list clears when opened, and a late response for a different pick is ignored.
- **Unsaved place names are preserved.** Any story update used to rebuild every chapter's place draft, wiping text the user hadn't saved. Each place field is now its own form, read on submit, so Enter also saves.
- **Buttons stay disabled until all queued edits finish.** The first edit to finish used to re-enable them while others were still queued. A pending-edit count replaces the single busy flag.
- **The rescan promise now matches the import flow.** The UI claimed that edits survive rescans, but importing a folder again creates a new project without them. The editor text and code comments now say so.
- **Plan resolution has one source.** A single `edited_plan` helper now returns the base plan, edited plan, edits, and assets for the app, CLI, and Story editor. This replaces separate loading paths and repeated asset queries.
- **Duplicate view code is gone.** `StoryEntryView` now reuses `AssetView`, and asset mapping moved into a single `From` conversion. A test locks in the flat JSON shape the webview expects.
- **Edit storage and mutation are tidier.** Place and caption overrides now use ordered maps, so a key has only one value and is applied in a stable order. Exclude and swap rules moved from the command handlers into `PlanEdits`, where they can be unit tested.
- **Test coverage expanded.** Eight new tests cover swap validation, replay order, superseded swaps, removal and restoration, exclusion ordering, and JSON serialization.

## Created Files

None

## Remaining Items

- **Low, browser preview only:** `swapAsset` in `src/lib/tauri.ts` transfers `inHero` but not `inReels`. Replacing a reel-only pick leaves the reel unchanged, and replacing a pick in both keepsakes updates only the video. The reviewer marked this non-blocking and recommended moving `inReels` along with `inHero`.

## Files Changed

- `src-tauri/crates/triptape-core/src/db.rs`
- `src-tauri/crates/triptape-core/src/plan.rs`
- `src-tauri/crates/triptape-media/src/bin/triptape.rs`
- `src-tauri/src/lib.rs`
- `src-tauri/src/main.rs`
- `src/Story.svelte`
- `src/lib/tauri.ts`

## Metrics

- **Cycles run:** 1
- **Duration:** 21m 58s
- **Initial review:** High 0, Medium 2, Low 0
- **Final review:** High 0, Medium 0, Low 1
- **Final verdict:** Good to go
