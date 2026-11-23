# Agent Review Summary

## Overview

The original work added owner framing rectangles (`--frame`/`--unframe`) and a persistent play-as-still override (`--unlive`) to the film pipeline. The review loop then ran a refinement pass. It changed no behaviour. It folded repeated "apply the frame if there is one" logic into the helpers, so every call site handles a missing frame the same way, and it made the framing tests stronger. The reviewer found no High or Medium issues and passed the change as good to go. It raised three Low findings, and none of them were applied.

## Improvements Made

- **`Option` handling moved into the framing helpers.** `frame_aspect` and `frame_crop` now take an `Option<Frame>` and return the source unchanged when there is no frame. Before this, each renderer call site (clip, hold, pano, live-collage and still-collage graphs, and the entry-segment aspect probe) wrapped these helpers in its own `map_or`. Those copies are gone, so one renderer can no longer get the no-frame case wrong while the others get it right.
- **`frame_crop` adds its own trailing comma.** Callers used to add the `,` separator themselves, and the call sites did it in slightly different ways. The helper now returns either `"crop=…,"` or an empty string, so every graph that uses it gets the same separator.
- **One percent parser for the CLI.** The `--crop` and `--frame` arguments had two separate inline parsers for 0–100 percent values. They now share one `parse_pct` helper, so both flags validate the same way and give the same style of error.
- **One rule for the collage cover-crop focus.** The rule "centre the crop for a framed photo, otherwise bias it upward" was written out separately in the still-collage and live-collage code. A single `cover_focus_y` helper now holds it, which also made `fill_cell`'s branching shorter.
- **Smaller tidy-ups.** `apply_edits` now builds a `HashSet` of still-override IDs and checks it for each hero entry, instead of scanning the `Vec` linearly every time. The frame-printing loop in `edit_cmd` destructures the frame once, not twice.
- **Stronger tests.** The framed-aspect test used to check its result only when the test asset happened to have dimensions. That condition could be false, so the test could pass without checking anything. It now sets explicit dimensions and checks the framed ratio exactly. It also checks the no-dimensions case (`None`) and confirms that a missing frame leaves the aspect unchanged. The `frame_crop` test now covers both the framed output (with its trailing comma) and the unframed output (an empty string).

## Created Files

None.

## Remaining Items

The final review raised three Low findings. None of them were applied:

- **`--unlive` accepts videos.** Unlike `--live`, it does not reject videos or photos that aren't Live Photos. For a video it reports success and lists the video under "plays as a still", but the video keeps playing as a clip. Suggested fix: validate the targets the same way `--live` does.
- **`frame_crop` does not check the frame.** A hand-edited or directly loaded `Plan` that skipped `apply_edits` could pass in a zero-size or out-of-range frame, and FFmpeg would then fail. Suggested fix: check the frame with `valid_frame` before using it, for defence in depth.
- **Excluded assets are kept in `frame` and `still`.** `apply_edits` filters these two fields by `in_plan` but not by `dead`, unlike the fields next to them (`cover`, `book`, `closing`, `collage`). Excluded assets therefore stay in the saved plan.

## Files Changed

- `src-tauri/crates/triptape-core/src/plan.rs`
- `src-tauri/crates/triptape-core/src/types.rs`
- `src-tauri/crates/triptape-media/src/bin/triptape.rs`
- `src-tauri/crates/triptape-media/src/ffmpeg.rs`
- `src-tauri/crates/triptape-media/src/render_video.rs`

## Metrics

- Review cycles run: 0 (one refinement pass, then one review)
- Duration: 12m 50s
- Final issue counts: High 0, Medium 0, Low 3
- Final status: CLEAN (the verdict was good to go, and the three Low findings were not applied)
