# Code Review Loop Summary

## Overview

The review loop corrected the motion technique behind the original smoothness work, closed three review findings (one High, two Medium), and removed duplicated code. Still images now animate through FFmpeg's `perspective` filter, which replaces an animated-crop approach that could not produce the intended motion. Short films now stay within their 30-second limit, collages keep the captions and IDs of all their photos, and FFmpeg builds without `perspective` still render. The final review found no remaining issues.

## Improvements Made

### Review findings

- **Short films respect the 30-second limit.** Closing map and places cards were added after the content budget had been spent, so films with GPS and named places could overrun. Their time is now reserved up front, counting only the cards that will render. A regression test covers this case.
- **Collages account for every photo.** A collage segment carried only its lead photo's caption and ID, so other members' captions were lost and the EDL did not show everything on screen. Segments now combine all member captions, the EDL lists every member through `collage_asset_ids`, and multi-line subtitle cues get reading time per caption.
- **Missing `perspective` no longer breaks rendering.** On FFmpeg builds without the filter, stills, clips and maps now hold static instead of producing an unrunnable filter graph. The render's fallback notes record the downgrade. Slideshow reels get the same treatment.

### Motion and rendering

- **Ken Burns motion was rebuilt.** `crop` evaluates its size only once and snaps its position to whole pixels, so the original glide could freeze or jitter. The new glide uses a window resampled per frame at sub-pixel positions, then downscaled with lanczos.
- **Motion is now tested on real pixels.** A new test runs each glide variant through FFmpeg and checks that frames change and that opposing variants mirror each other. Filter-string assertions could not catch motion that FFmpeg silently ignored.
- **Costly composites are built once.** Replacing input-side `-loop 1` with a one-frame input and a `hold_still` filter stops the composite being rebuilt for every frame.
- **Clip glides use the correct frame count.** Clips are normalized to 30 fps before the glide, because its progress counts frames.
- **Tooling matches the new dependency.** The capability probe, CLI output, FFmpeg check script and build and gate docs now name `perspective` instead of `zoompan`.

### Correctness

- **XMP captions decode correctly.** `&amp;` is now unescaped last, so `&amp;lt;` stays a literal `&lt;` rather than becoming `<`. A test covers this.
- **XMP search works on the raw bytes.** The `dc:description` tag is now located in the raw bytes, so only the matching section is converted to text rather than the whole file.

### Simplification

- **Map geometry has one source.** Route, pins and land now share a `RouteFit` mapping, so they cannot drift apart. A round-trip test covers the conversion between GPS and pixel positions.
- **Collage composition is simpler and faster.** Layout moved into a tested `collage_cells` helper, cover-fitting reuses the book renderer's implementation, and photos decode in parallel.
- **Card handling is consolidated.** Shared helpers now cover card segments, Typst rendering with its solid-card fallback, and card ID checks. Named constants replace the repeated durations and collage size.
- **Smaller cleanups.** Caption merging in the plan is now a single chained expression, the place de-duplication has no redundant check, and one test name no longer states a specific hero cap.

## Created Files

None.

## Remaining Items

None. The final review reported no open findings.

## Files Changed

- `docs/ffmpeg-build.md`
- `docs/gates.md`
- `scripts/check-ffmpeg.sh`
- `src-tauri/crates/triptape-core/src/metadata.rs`
- `src-tauri/crates/triptape-core/src/plan.rs`
- `src-tauri/crates/triptape-media/src/bin/triptape.rs`
- `src-tauri/crates/triptape-media/src/ffmpeg.rs`
- `src-tauri/crates/triptape-media/src/map.rs`
- `src-tauri/crates/triptape-media/src/render_book.rs`
- `src-tauri/crates/triptape-media/src/render_reels.rs`
- `src-tauri/crates/triptape-media/src/render_video.rs`

## Metrics

| Metric | Value |
| --- | --- |
| Cycles run | 1 |
| Duration | 14m 43s |
| Initial review issues | High 1, Medium 2, Low 0 |
| Final review issues | High 0, Medium 0, Low 0 |
| Final status | Clean |
