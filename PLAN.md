# TripTape — Plan

- Version: v0.5 — Production plan (hackathon submitted; Section 23 tracks the road to a public release). Earlier: v0.4 Hackathon MVP plan (adds Live Photos + modern transition pack; codex review cycle 1: FFmpeg licensing, vendor/privacy gates, degraded-input rules, job recovery; cycle 2: blur-by-default share gate, kid marks force blur; cycle 3: face decisions cover reel cover frames and book proof PDF; v0.4 owner decisions: kid-protection blur/gate removed in favor of a share disclosure on Reels, FFmpeg switched to a full GPL build with `libx264` + `vidstab` for best out-of-box quality)
- Context: Free, open-source project for the Meta Global AI Developer Hackathon, "Build for Your World" track
- Orchestration: Planning by muse-wonder-1.0, research/implementation slices by muse-spark-1.3-contributor subagents
- Status: MVP merged and submitted to the hackathon. Sections 1-22 are the target design; they are not a description of what ships today. Section 23 records the actual state and the remaining work, audited against the code on 2026-11-04.

## 1. Vision

- TripTape turns a messy folder of family vacation photos and videos into three keepsakes from one curation pass.
- Keepsake 1 — Trip Video: a finished, watchable trip film with transitions, animation, and location maps.
- Keepsake 2 — Social Posts: Reels-ready vertical clips that isolate specific memories and highlights.
- Keepsake 3 — Photo Book: a print-ready book export that photo book publishers can print.
- North star moment: a judge or parent drops in a messy folder and gets all three keepsakes with almost no clicks.
- Tagline candidate: "Drop in the trip. Get the story back."

## 2. Interview Locks (Decisions Already Made)

- Form factor: Desktop app first.
- Platforms: Cross-platform macOS + Windows from day one.
- Shell: Tauri (Rust) for a small, fast binary. No Electron.
- Processing: Hybrid.
  - Local by default for ingest, preview, faces, and first-pass curation.
  - Cloud only as per-trip opt-in for heavy AI polish.
- Primary user: Families and consumers. Not pro editors.
- Editing model: Auto + light tweaks.
  - App makes the first cut automatically.
  - User can swap a clip, reorder, pick music/theme, edit captions.
  - No full timeline editor in v1.
- Input v1: Local folder only (photos + videos on disk).
  - No Google Photos, iCloud, Dropbox, or phone sync in v1.
  - Apple Live Photos in scope for v1: a still (HEIC/JPG) + paired ~3 s MOV counts as one asset with two renditions (see Section 8).
- AI ambition: Full intelligence as the direction, shipped in phases.
  - Faces, best-shot selection, dedupe, location/time clustering, captions, story structure, highlights, mood/music fit.
  - MVP ships the local, shippable subset. Cloud VLM polish is opt-in and later.
- Trip Video v1: Single hero film. One polished look, not a theme store.
- Transitions v1: Modern 6-pack, not fade-only (see Section 9): fade, dissolve, smooth push, circle/iris, chapter fade-through-black, blur-dissolve signature. Mosaics, fly-overs, and parallax are Later.
- Photo Book v1: PDF only as the headline, corrected by research reality in Section 11. Read that section before promising anything.
  - Optimize presets for the Wirecutter-cited services: Mixbook, MILK Books, VistaPrint.
- Social v1: Auto Reels pack. 3–5 vertical clips, not a single teaser and not a full multi-platform kit.
- Music v1: Bundled royalty-free library + user-supplied MP3.
  - Keep original clip audio where it matters (speech, laughter, ambience).
- Business: Free and open source. No paywall, no subscription, no per-export charge in this plan.
- Timeline: Best effort. No fixed ship date. Sprints below are sequenced, not dated.
- AI preference: Meta open models first (Llama family, SAM family, DINO family) plus best open complements.
- Maps: Animated journey map from GPS EXIF in video. Still route map in print.
- Captions: Text overlays + burned-in captions. No voiceover in v1.
- Demo win: "Messy folder → magic" beats "most beautiful outputs" and beats "privacy story" alone. Privacy is still load-bearing underneath.

## 3. Users and Jobs

- Primary persona: Parent or family organizer after a vacation.
  - Has 500–10,000 photos and clips spread across phones and cameras.
  - Wants something to watch together, something to post, and something to keep on a shelf.
  - Will not learn a timeline editor. Will abandon anything that needs 50 decisions.
- Secondary persona: Hackathon judge.
  - Has 3 minutes. Needs to see the messy-to-magic transformation without setup help.
- Jobs to be done:
  - "Show me the trip without making me sort 2,000 files."
  - "Give me 3 clips I can post today without editing."
  - "Give me a book I can actually order without rebuilding it in another tool."
  - "Do not upload my kids' faces anywhere without asking me."

## 4. Principles

- One curation, three keepsakes. Never build three separate pipelines.
- Local first, cloud by consent. Faces never leave the device by default.
- Auto first, tweak second. Every manual control must have a good default.
- Editable AI. Every generated caption, title, and selection can be changed by the user.
- No fake precision: call music matching "vibe fit," not detected emotion.
- Small binary, honest size: Tauri shell stays tiny. FFmpeg and models dominate size and must be disclosed.
- Best tools that work out of the box. GPL components (FFmpeg with `libx264`/`vidstab`) are fine. Only avoid things we cannot legally redistribute at all (research-only/non-commercial weights, `--enable-nonfree` FFmpeg builds).

## 5. Scope

### 5.1 MVP Must Ship (All Three Keepsakes Equal)

- Local folder ingest at scale (target: browsable grid in under 10 seconds for 10k assets, thumbnails filling in lazily).
- Exact dedupe, near-dupe grouping, blur/exposure/sharpness scoring.
- Time + GPS clustering into days and places.
- Face detection on-device (count and boxes only in MVP core; naming is opt-in).
- One shared AI plan (shot list, hero selects, reel selects, chapters, caption drafts, map route, music pick).
- Trip Video export: 1080p, length scaled to the trip (~3 min for a weekend, ~10 min for a month; `scan --length` overrides), Ken Burns stills, Live Photo motion clips, trimmed clips, modern 6-pack transitions (Section 9), title/location cards, captions, music ducking, loudness-normalized.
- Reels export: 3–5 clips, 1080×1920, 15–30 seconds each, smart static crop, burned-in captions (fewer/shorter for small trips, Section 5.4).
- Photo Book export: screen proof PDF + per-page JPEG set for Mixbook / MILK / VistaPrint + generic press PDF. See Section 11.
- Animated map segment in the hero video (MVP: styled route card + push-in; full fly-through is later).
- Light-tweak UI: swap, remove, reorder, retitle, pick music, toggle captions, re-export.
- Consent and privacy controls in Section 14.
- Graceful degradation for small, short, low-resolution, or no-GPS trips (Section 5.4).
- Signed macOS + Windows builds with auto-update wiring.

### 5.2 Explicit Non-Goals for v1

- No full timeline / keyframe editor.
- No voiceover, AI narration, or generative music.
- No cloud photo import (Google Photos, iCloud, Dropbox) and no phone companion app.
- No in-app print ordering or commerce. User orders in the vendor's designer.
- No 4K hero export as the default. 1080p is the MVP default.
- No face recognition embeddings shipped from research-only weights.
- No mobile app (iOS/Android) in v1.
- No multi-user accounts, sharing backend, or social network.

### 5.3 Later (Post-Hackathon Backlog)

- Cloud Llama Vision story polish, chapter titles, and re-captioning (opt-in).
- Mosaic / photo-wall opener and end-recap, map fly-over transitions, parallax Ken Burns hero treatment, zoom-through via pre-rendered segments (Section 9 Later pack).
- Full animated map fly-through with eased camera and drawing route.
- Tracked vertical reframing that follows the speaker frame to frame.
- Two-pass loudness, LUTs/color science, two-pass stabilization as defaults.
- Beat-perfect cuts via isolated beat-tracking sidecar.
- CMYK / PDF/X press variant and auto cover + spine calculator.
- More book sizes, 100+ page books, additional vendor presets.
- Cloud import sources and phone ingest.

### 5.4 Minimum Content and Graceful Degradation

- Rule: each keepsake has an eligibility threshold. Below it, the Export card shows a smaller variant or a disabled state with a plain-language reason. Never pad with repeated shots, never fail silently.
- Trip Video:
  - Full film when the plan has enough distinct, usable moments (≈40+ after dedupe): target length `100 s × √(trip days)`, clamped to 2–12 min, or the `scan --length` request (30 s–60 min). The plan fills that time budget: every chapter gets its best shot, then remaining time goes to chapters in proportion to √(chapter size), best shots first. Otherwise duration scales down to the content (floor ~30 s, ~4–6 s per still, clip trims at scored length).
  - Fewer than ~5 usable assets: offer a "short" (≤30 s) slideshow, labeled as such.
- Reels Pack:
  - Pack size = number of distinct qualifying moments, capped at 5. 1–2 reels is a valid pack; 0 disables the card ("Not enough distinct moments for a reel").
  - A reel may run under 15 s when its source moment is short. No looping or stretching to hit 15 s.
- Photo Book:
  - Needs enough placeable photos to reach the preset's 24-page vendor minimum at sane density (≈30+ usable photos). Below that, the card is disabled with "Add more photos for a printable book (24-page vendor minimum)"; the proof PDF stays available.
  - Low-resolution handling per Section 11 preflight (reduce placement before blocking).
- No GPS / partial GPS: map segment and map page are omitted (no GPS) or show only GPS-bearing places with "some places not shown" (partial). Chapters fall back to days.
- No usable time: story order falls back to filename/modified order with the "time guessed" badge.
- Single-photo, corrupt-only, or unsupported-only folders: import completes, Library explains what was skipped, Export cards are disabled with reasons.
- These rules are covered by fixtures in Section 19.

## 6. Experience Flow (The 3-Click Promise)

- Click 1 — Add folder:
  - User picks a local folder. Ingest starts immediately with progress, counts, and a filling grid.
  - First-run consent sheet appears once (faces on-device, optional naming, cloud off).
- Click 2 — Review the auto story:
  - App shows the AI plan as chapters by day/place with hero picks, not as a file list.
  - User can swap a hero, remove a moment, fix a caption, rename a place, pick music. All optional.
- Click 3 — Export keepsakes:
  - One Export screen with three cards: Trip Video, Reels Pack, Photo Book. Each shows time estimate and size.
  - Primary button "Export all keepsakes" is the default action. It uses the plan's music pick, captions on, book preset auto-chosen from photo orientation mix (P2 if mostly landscape, else P1), vendor pack for Mixbook as the default target, and destination `~/Movies/TripTape/<trip name>/` (Windows: `Videos\TripTape\<trip name>\`).
  - Renders run in the background in sequence (video → reels → book) with per-card progress, cancel, and "reveal in Finder/Explorer" on completion.
  - Per-card export and option changes remain available but are never required.
- Click budget (measured on the demo fixture, Section 22):
  - The 3 intentional clicks are: Add folder (plus the OS folder-picker confirm), Continue on the Story screen, Export all.
  - The one-time consent sheet "Got it" is counted separately and disclosed in the demo script.
  - The Reels card and the Reels completion view show a non-blocking share disclosure (Section 14). It adds no click.
  - Extra input is required only when: a keepsake is ineligible (Section 5.4, card disabled, others proceed), book preflight has a blocking issue (book is held with fix hints; video and reels still export), or free space is insufficient.
- Supporting screens:
  - Import (folder picker, ingest status, missing-GPS/time warnings).
  - Library (virtualized grid, filters: day, place, people count, best / all).
  - Story (chapter list, hero/reel selects, caption drafts, map preview).
  - Export (presets, music picker, caption toggle, book preset picker, preflight results).
  - Settings (cache location/size, cloud opt-in defaults, face data delete, about/licenses).

## 7. System Architecture

- App shell: Tauri v2 (system webview: WKWebView on macOS, WebView2 on Windows; ~10–15 MB shell).
- Frontend: Vite static build. Preferred: Svelte 5 + Tailwind for lowest memory on 10k-thumbnail grids. React 19 + Vite is acceptable if contributors know it better. Decision must be pinned in Sprint 0 and not revisited mid-build.
- Backend: Rust workspace inside the Tauri app.
  - `triptape-core`: scan, hash, EXIF, thumbnails, proxies, SQLite, job types. Pure Rust, testable headless without the UI.
  - `triptape-media`: FFmpeg/ffprobe orchestration, hardware-encode probing, render-plan execution.
  - `triptape-ai-local`: local scoring and model inference behind traits, so cloud implementations can slot in later.
  - Tauri glue crate: commands, events, app state only. No business logic in glue.
- IPC contract:
  - Coarse commands only: `scan_folder`, `get_assets(page)`, `start_job(kind)`, `cancel_job(id)`, `export_video`, `export_reels`, `export_book`.
  - Progress via Tauri events/Channels (e.g. `job://progress`), throttled to ~5 Hz. Never one invoke per file.
  - Thumbnails served via `asset://` protocol or file URLs. Never base64 thumbnails through IPC JSON.
  - IPC hardening:
    - Commands take scoped identifiers (project id, asset id, job id), never raw filesystem paths, except the folder-picker result returned by the OS dialog.
    - Backend resolves ids to paths and checks the asset belongs to the active project.
    - Tauri asset-protocol scope is limited to the TripTape cache directory. Originals are not exposed to the webview; previews always come from cache.
    - Unit tests cover malformed ids, cross-project ids, and `../` path traversal on the protocol handler.
- Media engine: Bundled static FFmpeg 7.x/8.x + ffprobe as Tauri sidecar binaries per target (separate executables, never linked). PATH fallback in dev only.
  - Do not link `ffmpeg-next`. It needs system FFmpeg dev libraries and breaks on user machines.
  - Build: full GPL static build (`--enable-gpl --enable-version3`) with `libx264` and `libvidstab`. Use a reputable prebuilt static build per target (Windows: BtbN or gyan.dev full build; macOS arm64/x86_64: evermeet.cx or equivalent static build), pinned by version + SHA-256 in `scripts/fetch-ffmpeg`. Record the source, version, and `-buildconf` output in `docs/ffmpeg-build.md`.
    - No `--enable-nonfree` (e.g. `libfdk_aac`): nonfree builds cannot be redistributed. Native `aac` is good enough.
    - GPL obligations (low effort): ship the GPL text and the build's source link in About/licenses and the repo. FFmpeg stays a separate sidecar executable, so the project license is unaffected.
  - Encoding default: `libx264` (CRF ~18–20, `-preset medium`, `yuv420p`, `+faststart`). Identical output on every machine, no GPU or driver dependency, best quality per MB.
    - Optional "Fast export" toggle uses hardware encoders when a 1-frame test encode succeeds at startup (cached): macOS `h264_videotoolbox`; Windows NVENC → QSV → AMF. Off by default; any failure falls back to `libx264` silently.
  - Stabilization: `vidstab` two-pass (`vidstabdetect` → `vidstabtransform`) only on clips flagged shaky.
  - Verify the bundled build includes: `libx264`, `libvidstab`, `libass`, `drawtext` (freetype + harfbuzz), `loudnorm`/`ebur128`, `xfade`, `zoompan`, `zscale` (zimg), native `aac`, HEVC/HEIC decode.
  - CI build check: run `ffmpeg -buildconf`, `-encoders`, and `-filters` on every bundled binary per target; fail the build if a required encoder/filter is missing or `--enable-nonfree` appears.
- Book engine: Typst (Rust `typst` / `typst-pdf`) as layout + PDF engine.
  - `printpdf` only as fallback if Typst cannot hit a geometry requirement.
  - Rejected for v1: headless Chromium HTML→PDF (binary size), LaTeX (toolchain), `genpdf` (too primitive for spreads).
  - Embed one OFL font family (Inter / Source Serif) so exports never depend on system fonts.
- Storage:
  - SQLite via `rusqlite` (bundled, FTS5, WAL mode, single writer + pool, `refinery` migrations).
  - Cache in OS cache dir: content-addressed thumbnails/proxies named by content hash, 5–10 GB LRU cap, user-clearable.
  - Temp in job-scoped folders, deleted on complete or cancel. Originals are never copied into temp.
- Jobs:
  - Durable job rows in SQLite (kind, state, progress, timestamps, current step, temp dir, child PIDs).
  - State machine: `queued → running → (finalizing) → done | failed | cancelled`, plus `interrupted` set only by startup recovery. No other transitions.
  - Checkpoints are the render steps themselves: each pre-rendered segment, crossfade stage, reel, and book page is written to the job temp dir as `name.partial`, then renamed on success and recorded in the job row. Resume skips steps whose output exists and whose recorded input hash still matches.
  - Atomic finalization: final outputs are written to temp, verified (ffprobe duration/stream check for video; page count + dimensions for book), then renamed into the destination. An existing output with the same name is replaced only after verification succeeds (keep `name (1).mp4` style if the user chose "keep both").
  - Startup recovery: any `running`/`finalizing` row becomes `interrupted`; recorded child PIDs are checked and killed if still alive and still FFmpeg; the user sees "Resume / Discard" for each interrupted export. Ingest/score jobs auto-resume silently.
  - Each job runs as a cancellable async task. Cancel kills child FFmpeg processes (process group on macOS, job object on Windows so children die with the app), marks the job cancelled, and deletes its temp dir.
  - Sleep/wake: FFmpeg children are not restarted mid-step; a step that fails after wake is retried once from its last checkpoint.
  - Temp cleanup: on launch, delete job temp dirs with no matching `interrupted` row, and any older than 7 days.
  - Free-space precheck before any export (estimate × 1.5 headroom). Partial exports never appear in the destination folder.
- Distribution:
  - macOS: universal binary (arm64 + x86_64), `.dmg` + updater `.app.tar.gz`, Developer ID signing + notarization.
  - Windows: x64 NSIS installer (+ optional MSI), Authenticode signing.
  - Auto-update via Tauri updater with a minisign keypair generated on day one.
    - Windows order matters: Authenticode sign first, then Tauri minisign. Reversing it invalidates the signature.
  - Crash reporting opt-in only (Sentry Rust + frontend SDK). Off by default for a family privacy story.

## 8. Shared Curation Core (One Pipeline, Three Renderers)

- Stage 1 — Enumerate: walk the folder in batches of ~500. Skip hidden/system files. Record path, size, type.
- Stage 2 — Fingerprint: content hash (BLAKE3) for exact identity. Perceptual hash (pHash) for near-dupes.
- Stage 3 — Metadata: EXIF date/GPS for images (`kamadak-exif`, fallback `rexif`). `ffprobe` JSON as truth for video time, GPS, rotation, duration.
  - Normalize rotation and timezone at ingest. Fall back to file modified time with a visible "time guessed" badge.
  - Live Photo pairing (two-tier, never guess-merge real videos):
    - On disk, one Live Photo is two files: still `IMG_####.HEIC` (or JPG) + motion `IMG_####.MOV` with the same stem. Total motion is ~3 s (~1.5 s before + after the shutter) with audio. The still is the key photo; always prefer the full-res still for print.
    - Tier 1 (authoritative): pair on the shared content-identifier UUID present in both the still metadata and the MOV metadata, even if basenames differ or files sit in different subfolders.
    - Tier 2 (fallback): same stem (case-insensitive) + MOV duration at most ~3.5 s + MOV has a video stream. Mark `confidence: basename-only`. A same-stem MOV over ~5 s is a real video and is never auto-merged.
    - `.AAE` sidecars are Photos edit recipes, not assets: never count, score, or render them. Render from the original still.
    - HEIC on Windows: the Rust `image` crate cannot decode HEIC. Decode via the bundled FFmpeg sidecar to a JPEG proxy/thumbnail at ingest. Never depend on OS codecs.
- Stage 4 — Proxies: 320 px JPEG thumbnails for images (HEIC decoded via FFmpeg sidecar). 720p H.264 proxy + poster frame for video and for Live Photo motion. Generated lazily, cached by hash.
  - A Live Photo pair is one asset with two renditions (still + motion). If the still is missing, use the MOV frame at ~1.5 s as the poster. If the motion is missing/corrupt, fall back to Ken Burns on the still.
- Stage 5 — Score: per-asset quality and story signals.
  - Sharpness, blur, exposure, aesthetic score, face count, speech/audio energy, motion, GPS novelty.
  - Live Photo motion is scored (audio energy, movement, faces) only when a motion rendition exists. A lively 3 s Live clip can outrank its own still for video, never for print.
- Stage 6 — Cluster: group by time gaps + GPS distance into days and places. Near-dupes collapse to a best-shot pick (user can expand the stack).
- Stage 7 — Plan: emit one AI Plan JSON saved with the project for reproducibility.
  - Hero shot list in story order, 3–5 reel selects, chapter titles, caption drafts, map polyline with day markers, music pick with rationale.
  - Each Live Photo emits one plan entry with a per-renderer rendition choice: book always uses the still; hero video and Reels are motion-first (2–3 s scored trim, clip audio kept and music ducked).
  - Plan is data, not code. All three renderers consume the same plan. Re-export after a tweak reuses the plan.
- Stage 8 — Render: three independent renderers read the plan. See Sections 9–11 specs below (video, reels, book).
  - Save the edit decision list (EDL/JSON) beside every export so any output can be reproduced or debugged.
- Source integrity (moved, edited, deleted, or replaced originals):
  - The plan is versioned (`plan_version` + schema version) and stores, per entry, asset id, content hash, size, mtime, and the chosen rendition (still/motion + trim).
  - Before every render, a fast check compares path + size + mtime; any mismatch triggers a hash check for that asset only.
  - Outcomes shown in one pre-render sheet: Missing (offer Relink folder, Rescan, or Skip with auto-replacement from the same cluster), Changed (offer Use new version, which re-scores that asset, or Skip). Never silently substitute different media.
  - Relink matches by content hash across a user-picked folder, so a reorganized trip folder heals in one step.
  - Renders never read originals that failed the check. Cached proxies are used only for previews, never for final output.

## 9. Keepsake 1 — Trip Video (Hero Film)

- Promise: A 3–5 minute film a family will actually finish watching.
- Input: AI Plan shot list (mix of stills and trimmed clips in story order).
- Render stages:
  - Probe and normalize every source to uniform fps/SAR/audio intermediates: apply rotation metadata, convert VFR to CFR 30 fps, resample audio to 48 kHz stereo, and tone-map HDR (HLG/PQ, iPhone default) to SDR BT.709 via `zscale` + `tonemap` (verified in the G1 build check).
  - Pre-render each segment (Ken Burns still, stabilized clip trim, map segment, title card).
  - Assemble with a stepwise crossfade chain. Never chain 5+ inputs in one filter graph; compute each crossfade offset accounting for overlap.
  - Burn captions after final scale so text stays sharp. Mix audio. Normalize loudness in a final pass.
- Visual rules (MVP):
  - Stills: Ken Burns (slow zoom/pan). Upscale stills first to avoid jitter; vary direction so it never feels mechanical.
  - Clips: trim to the scored highlight window. Stabilization (`vidstab` two-pass, Section 7) only on clips flagged shaky.
  - Transitions: modern MVP 6-pack (all built-in FFmpeg crossfades, 0.5–0.8 s, stepwise-chain safe). One dominant type per chapter; never random per cut.
    - `fade` — default for same-day, same-place cuts.
    - `dissolve` — softer memory blend between related moments.
    - `smoothleft` (smooth push) — forward energy for day/place changes and map-segment entry/exit.
    - `circleopen` (iris) — tender open on kid/family close-ups; `circleclose` reserved for the final shot.
    - `fadeblack` — chapter breaks only. `fadewhite` allowed once as a bright flash (beach/snow) if it earns it.
    - Blur-dissolve signature — dissolve with a brief blur ramp on the outgoing tail for dreamy beats. Crafted recipe, capped at 2–3 uses per film.
    - Excluded from MVP (dated/cheesy or unreliable): pixelize, slice/diagonal/radial wipes, and any custom-expression transition. The bundled static FFmpeg cannot ship custom transitions reliably.
  - Later transition/effects pack (each must be a pre-rendered segment joined by a single crossfade, phone-checked):
    - Mosaic / photo-wall: thumbnail grid animates into the hero frame. Opener or end-recap only, never mid-story.
    - Map fly-over transition: route draws and camera eases into the next place (extends Section 12 full fly-through).
    - Parallax Ken Burns: subject cutout drifts over its background on hero stills only (2–4x still cost, so rationed).
    - Zoom-through via pre-rendered zooms, radial clock wipe (max 1), film-strip (max 1, or never).
  - Color: gentle auto contrast/saturation + unsharp. No LUT pack in MVP.
  - Text: title card (trip name, dates), location/date lower-thirds, chapter cards. One bundled font.
- Audio rules (MVP):
  - Music bed from bundled library or user MP3, ducked under kept clip speech/ambience.
  - Keep clip audio when speech/laughter is detected; otherwise music-forward.
  - Single-pass loudness normalization (social target, true-peak ceiling). Two-pass is later.
- Export preset (MVP default):
  - 1920×1080, 30 fps, H.264 + AAC, fast-start for instant playback. 4K is a later preset.
- Light tweaks exposed: reorder chapters, swap/remove a shot, change music, edit any caption/title, toggle map segment.

## 10. Keepsake 2 — Social Posts (Reels Pack)

- Promise: 3–5 vertical clips the user can post without opening another editor.
- Selection: Highest-scoring distinct moments from the plan (different days/places preferred so the pack feels varied).
- Format per clip:
  - 1080×1920 (9:16), 30 fps, 15–30 seconds, H.264 + AAC.
  - Hook in first 2 seconds (best frame/moment first, not chronological).
  - Burned-in captions (readable without sound), location + date overlay, end card with trip name.
- Live Photos: paired motion clips are preferred Reel hooks (they are already short, alive, and vertical-friendly). Score and trim them like video clips; keep their audio under the music duck.
- Reframing (MVP):
  - Never naive center-crop when a face/subject is off-center.
  - Sample frames, detect face/subject, pick one fixed smart crop per clip (no jitter). Blur-fill background when the subject is too wide for 9:16.
  - Per-frame tracked follow-crop is later.
- Cuts: hard cuts on section/energy boundaries. No beat-perfect sync required in MVP.
- Light tweaks exposed: reorder pack, remove a reel, edit caption text, swap music per reel, re-export single reel.
- Export: one folder with numbered MP4s + a cover frame per reel. Filenames date-prefixed for camera-roll order.
- Share disclosure (Section 14): non-blocking note on the Reels card and completion view. TripTape never posts anywhere itself.

## 11. Keepsake 3 — Photo Book (Read This Before Promising "PDF Upload")

- Reality check (from subagent research, load-bearing):
  - Mixbook, MILK Books, and VistaPrint photo books are all assembled in their online designers from uploaded photos.
  - None of the three accepts a customer print-ready imposed PDF for photo books.
  - Therefore "print-ready PDF optimized for Mixbook/MILK/VistaPrint" cannot mean one-click PDF upload. Promising it would be false.
- What TripTape actually ships in v1 (two deliverables):
  - Deliverable A — Vendor-ready export pack (the real product):
    - Per-page sRGB JPEGs rendered at exact trim size at 300 dpi, in reading order, in a clearly named folder.
    - A screen proof PDF showing the exact book (same layout) for family review before ordering.
    - A one-page guide (PDF/txt) mapping page numbers to the vendor designer steps for the chosen preset.
    - UX copy: "Export for Mixbook / MILK / VistaPrint (JPEGs + guide)." Never "Upload PDF to Mixbook."
    - Intended vendor workflow: user picks the matching product size, uploads the page JPEGs, and drops each onto a full-bleed single-photo page layout in order. The guide lists the exact clicks. This is unverified until the Sprint 0 vendor gate passes.
- Vendor workflow gate (Sprint 0, per vendor × preset, recorded in `docs/vendor-book-guide.md`):
  - Exact product names and trim dimensions the designer offers, and whether a full-bleed single-image page layout exists.
  - Whether a page-sized JPEG is placed without crop/zoom, or how much the designer auto-crops (measure with a calibration grid page).
  - Whether upload order is preserved, and how many manual drag steps a 24-page book takes.
  - Spread behavior: whether a two-page spread can be one image or must be split into left/right halves (TripTape exports whichever the vendor needs).
  - Cover workflow and page-count rules (minimum, maximum, increments of 2/4, whether inside covers count).
  - An actual ordered proof copy from at least one vendor before the hackathon submission claims "orderable."
  - Outcome per vendor: Supported (keep preset + guide), Supported with manual steps (keep, state the steps in UX copy), or Dropped (remove vendor from UI and docs). The headline promise narrows to vendors that pass; the generic press PDF remains regardless.
  - Deliverable B — Generic press PDF:
    - One true bleed PDF (sRGB in v1) for local print shops and generic print-on-demand that do accept PDFs.
    - CMYK / PDF-X conversion is later and must be verified in Typst before it is promised.
- MVP presets (only two):
  - Preset P1 "Classic Square": 12×12 in class (with 11×11 export variant for MILK Large Square), 24–60 pages, layflat-friendly spreads.
  - Preset P2 "Travel Landscape": 11×8.5 in class (with 8.6×6.4 in export variant for MILK Medium Landscape), 24–80 pages, mixed single + spread layouts.
  - Common safe page zone across all three vendors: 24–80 pages. Enforce per-preset caps in preflight.
  - Covers: leave cover assembly to the vendor designer in v1 (spine width depends on final page count and vendor stock). The proof PDF includes a cover-proxy page only.
- Print math (applies to both deliverables):
  - 300 dpi at printed size. sRGB JPEG quality ~90+. Higher dpi only grows files.
  - Bleed 0.125 in (3 mm) on the press PDF where art meets the edge. No baked bleed on vendor JPEGs (designers position art themselves).
  - Safe margin 0.25–0.375 in inside trim for text/faces. Keep critical content at least 0.5 in from the gutter on standard binding.
  - Submit sRGB to all three vendors. They convert internally. Only the generic press variant may go CMYK later.
- Layout engine (MVP templates):
  - Cover-proxy, full-bleed hero, 1-photo + caption, 2–4 photo grid, two-page spread (layflat-flagged), journey-map page (still route map with day markers), chapter divider, closing page.
  - Auto-flow ranked, deduped photos in story order. Captions from the plan, always editable, never below readable size.
- Page counting rules (single source of truth for preflight and the guide):
  - Counted pages = interior pages only. The cover-proxy page appears in the proof PDF but is never counted and never exported as a vendor JPEG.
  - Each two-page spread counts as 2 pages. Spreads always start on an even (left) page; the flow engine inserts a layout change, not a blank, to align them.
  - Intentional blanks (e.g., inside-front facing page) count as pages and are exported as white JPEGs, only when the vendor's rules require them.
  - Final count is rounded up to the vendor's page increment by adding a closing or photo page, never a blank, when content allows.
- Resolution handling (before anything blocks):
  - Auto-flow first places each photo in the largest slot where it reaches ≥300 effective dpi.
  - If a meaningful photo (hero, chapter opener, kept by the user) is below 300 dpi, the engine moves it to a smaller slot in a grid layout.
  - 200–299 dpi at its final slot: warning with page number, export allowed.
  - Below 200 dpi even at the smallest slot: blocking issue with fixes "Use smaller layout," "Replace with similar photo" (same near-dupe/cluster), or "Remove." A labeled override "Print anyway (may look soft)" exists per photo.
- Preflight (blocks export with fix hints):
  - Effective dpi per the resolution handling above, flagged with page number.
  - Text inside safe margin and away from gutter. No critical face within 0.375 in of trim (reuse face boxes).
  - Fonts embedded. sRGB only. Page count inside the chosen preset cap. JPEG pixel dimensions exactly match target trim.

## 12. Maps (Video Animated, Print Still)

- Source: GPS EXIF/ffprobe coordinates clustered into places with day markers.
- Video (MVP): styled static route card (full route polyline, place pins, day labels) with a slow push-in (3 s class). Composite as its own segment near the start and optionally as chapter transitions.
- Video (later): full animated journey (eased camera between stops, drawing route line, pulsing position dot) rendered to frames and composited.
- Print: same route rendered as a high-resolution still on the book map page. Never promise "animated" in print.
- Rendering approach: MapLibre GL (BSD-3) rendering vector tiles from local PMTiles files. Headless frame render to PNG sequence, then FFmpeg composite.
- Tile pipeline (MVP):
  - Bundled basemap: one world PMTiles at low zoom (≈z0–z7, built from OpenStreetMap/Protomaps-style data with a pinned build script in `scripts/`). Size budget ≤60 MB; measured and disclosed in Sprint 0. This is the default and works fully offline for every trip.
  - Route card zoom is clamped to what the bundled tiles support. Trips that span a single town still get a readable card: route line + pins over a coarse basemap, with place labels drawn by TripTape from cluster names rather than from tiles.
  - Optional detail pack (later, opt-in): a higher-zoom regional extract downloaded on request. Off by default because fetching tiles for a bounding box reveals where the family traveled; requires its own consent prompt.
  - Attribution text and its license are bundled with the tiles and drawn by TripTape, not by the tile style, so it cannot be dropped.
- Missing data behavior: no GPS → no map segment or map page (Section 5.4); partial GPS → map shows only GPS-bearing places; a single GPS place → pin card, no route line; GPS outliers (>1,000 km jump within an hour) are dropped from the route.
- Hard rules:
  - Never bulk-scrape public OpenStreetMap tile servers (forbidden by tile usage policy). Use the bundled basemap or opt-in extracts only.
  - Attribution ("© OpenStreetMap contributors") visible on every map frame/page.
  - Privacy: fuzz the first/last ~500 m near home, provide a "hide home" toggle, strip EXIF from exports.

## 13. AI Plan (Meta OSS First, Hybrid, License-Clean)

- Guiding split:
  - Local (on-device, under ~1 GB total download, CPU-viable): everything privacy-sensitive and everything needed for preview and first export.
  - Cloud (per-trip opt-in only): large vision-language polish. Nothing uploads by default.
- Local models (MVP, all license-clean to bundle):
  - Face detection: tiny ONNX face detector (YuNet/BlazeFace class, KB-scale). Detection and counting only by default.
  - Embeddings for dedupe/clustering: DINOv2 small/base class via ONNX (Apache-2.0 line). Cosine-similarity near-dupe grouping.
  - Tagging and best-shot: SigLIP/CLIP-class zero-shot tagging + aesthetic/quality heuristics (sharpness, blur, exposure).
  - Subject masks for reframing: SAM 2.1 small class locally (Apache-2.0 line) when no face is present.
  - Transcription for clip captions/quotes: Whisper turbo class locally, with a smaller fallback for weak machines.
  - Draft captions: small local vision-language model (SmolVLM/Moondream class, Apache-2.0 line). Drafts only, always editable.
- Packaging tiers and budgets (pinned and measured in Sprint 0; numbers below are ceilings, not estimates):
  - Hardware baseline: 8 GB RAM, 4-core CPU from ~2019 (Intel/AMD x64 or Apple M1), no discrete GPU, SSD. Every budget is measured on this class on both OSes.
  - Core tier (bundled in the installer, no download, required for the 3-click flow): face detector, DINOv2-small embedding, SigLIP/CLIP-small tagger. Budget ≤250 MB on disk, ≤1.5 GB peak RAM, full score pass for 1,000 photos ≤10 min.
  - Optional tier (downloaded on demand from the Model settings, one consent click, resumable, checksum-verified, can also be side-loaded from a folder for offline machines): Whisper (small/turbo class), SAM 2.1 small, local VLM caption model. Combined budget ≤1 GB.
  - Fallbacks when an optional model is absent: template captions (Sprint 2), face-or-center-weighted crop instead of SAM masks, no quote captions from speech. The app never blocks on a download, and the offline check (Section 19) runs with the optional tier absent.
  - Sprint 0 records for each pinned model: exact weights + version, runtime (ONNX Runtime CPU EP; CoreML/DirectML EPs optional), disk size, peak RAM, per-asset latency, and fallback quality notes, in `THIRD_PARTY_MODELS.md`.
  - If a core model misses its budget, it is swapped for a smaller class or moved to the optional tier before Sprint 1.
- Cloud opt-in (later, never bundled by default):
  - Llama Vision class models for story arc, chapter titles, and caption polish (Llama Community License — not OSI open source; disclose this in the hackathon story).
  - Larger segmentation/embedding models (SAM 3 / DINOv3 class) only in cloud: custom/gated licenses, do not bundle.
  - Query-based highlight models are research-grade today (GPU, no shippable CPU build). MVP highlights are heuristic, by design.
- MVP highlight score (heuristic, explainable):
  - Face presence + aesthetic + motion + audio energy + speech density + GPS novelty, weighted and saved per asset so the UI can explain "why this moment."
- Music fit (MVP): tempo/energy/daylight/activity prior matched to the bundled library. Labeled "vibe fit" in UI and docs.
- License traps (must stay out of the shipped binary):
  - YOLO Ultralytics line: AGPL-3.0. Use Apache-2.0 alternatives (zero-shot tagging, RT-DETR class) instead.
  - InsightFace buffalo weights: research-only, non-commercial. Do not ship. Use detection + local embeddings or seek permission first.
  - DINOv3 / SAM 3 weights: custom/gated licenses. Cloud opt-in only.
  - Every model shipped needs an entry in a `THIRD_PARTY_MODELS.md` ledger (name, version, license, size, local/cloud).

## 14. Privacy, Consent, and Family Safety

- Default posture: everything local. The app must be fully useful with the network cable pulled.
- First-import consent sheet (plain language, one screen):
  - "TripTape finds faces and places on this computer. Nothing is uploaded unless you turn on Cloud Polish for a trip."
  - Optional: name people to find them faster. Skippable. Names never leave the device.
- Per-trip cloud toggle: "Include photos in Cloud Polish for this trip?" Default off. Faces excluded unless separately consented.
- Sharing disclosure (replaces the v0.3 kid-protection blur gate; no automatic blurring, no extra clicks):
  - Reels are made to be posted, so the Reels card and the Reels completion view show one plain line: "Reels are made for sharing. Watch them first and post only what you're comfortable sharing, especially videos with kids or other people in them."
  - Non-blocking: no checkbox, no confirm, not repeated per reel. TripTape never posts to social media itself; the user does that manually.
  - Trip film and photo book are personal keepsakes and carry no disclosure.
- Controls that must exist in v1:
  - Delete all face data (one tap, in Settings).
  - Hide home on maps (fuzz + toggle).
  - Strip EXIF on export toggle.
  - Opt-in crash reporting (off by default).
- Local diagnostics:
  - Rolling local log (7 days or 20 MB, whichever first) in the app data dir. Logs record job ids, stage, FFmpeg exit codes, stderr tail, and codec/format facts.
  - Redacted at write time: file paths become `<asset:id>` or a hashed folder token, GPS is never logged, person names and captions are never logged.
  - Failed exports show "Copy diagnostic report": a redacted bundle (job row, step, stderr tail, app/OS/encoder versions) the user can inspect before sharing. Nothing is sent automatically.
  - Opt-in crash reports use the same redaction before send.
- Data that never leaves the device without explicit per-trip consent: originals, face embeddings, precise home coordinates, kids' names.
- Live Photos carry audio (voices, background speech) plus still GPS. Pairs stay local by default; EXIF-strip on export also strips the pairing identifier so a shared still cannot be quietly re-linked to its motion file.

## 15. Data Model (SQLite Sketch)

- `projects`: id, name, folder path, created/updated, cloud-consent flag, book preset, music pick.
- `assets`: id, project id, path, content hash, perceptual hash, width/height/duration, taken-at, lat/lon/alt, rotation, score parts, thumb path, proxy path, live-pair fields (pair UUID, motion path, pair confidence, motion duration). One row per Live Photo pair, not two.
- `clusters`: id, project id, kind (day/place/near-dupe), label, center time/GPS.
- `asset_clusters`: asset id, cluster id, is-hero pick.
- `people`: id, project id, display name (nullable), face count. Only populated if face naming ships. Face embeddings stored separately and deletable independently.
- `plans` carries `plan_version` and per-entry source identity (Section 8).
- `plans`: id, project id, plan JSON (shot list, reel selects, chapters, captions, map polyline, music pick), created-at, source (local/cloud).
- `jobs`: id, project id, kind (ingest/score/render-video/render-reels/render-book), state, progress, error, output paths.
- Rule: originals are referenced by path + hash, never copied into the database or cache as originals.

## 16. Repo Layout (Proposed)

- `/` — repo root
  - `PLAN.md` — this file
  - `README.md` — what TripTape is, how to run it, license
  - `LICENSE` — project license (pick in Sprint 0; permissive recommended for hackathon adoption)
  - `THIRD_PARTY_MODELS.md` — model ledger (Section 13)
  - `src/` — frontend (Svelte or React + Vite + Tailwind)
  - `src-tauri/` — Tauri app + Rust workspace
    - `crates/triptape-core/` — scan, hash, EXIF, thumbs, DB, jobs
    - `crates/triptape-media/` — FFmpeg orchestration and renderers
    - `crates/triptape-ai-local/` — local scoring and inference traits
    - `binaries/` — FFmpeg sidecars (fetched by script, not committed if large)
  - `scripts/` — FFmpeg fetch, signing, and release helpers
  - `fixtures/` — tiny test trip (synthetic + consented samples) for CI and demos
  - `docs/` — architecture notes, export presets, vendor book guide

## 17. Sprints (Sequenced, Best-Effort, No Dates)

- How to read this section:
  - Each sprint has a goal, deliverables, and exit criteria. A sprint is done when its exit criteria pass, not when time runs out.
  - Sprints 3–5 (the three keepsakes) can overlap once the shared core (Sprints 1–2) is stable. Priority is equal, per the interview lock.

### Sprint 0 — Foundations and Spikes

- Goal: Prove the riskiest plumbing before building features.
- Deliverables:
  - Tauri v2 app shell that launches on macOS and Windows with the pinned frontend stack.
  - FFmpeg sidecar spike: bundle, launch, run one Ken Burns + crossfade + caption + loudness render from hard-coded inputs on both OSes, using the pinned GPL build (Section 7).
  - Book spike: Typst renders a 4-page proof PDF + per-page JPEGs at exact pixel dimensions.
  - Vendor spike: calibration-grid JPEG pages for P1 and P2 run through each vendor designer (Section 11 vendor gate).
  - Map spike: GPS points → styled route card PNG from the bundled PMTiles basemap, network disabled.
  - Model spike: core-tier models pinned and measured against Section 13 budgets on the hardware baseline, both OSes.
  - Signing spike: signed + notarized macOS build and Authenticode-signed Windows build, one updater hop on a test channel.
  - Repo, license, `THIRD_PARTY_MODELS.md`, CI skeleton (build both OSes), minisign + platform signing checklist started.
- Exit criteria:
  - All spikes produce a file a human can open and judge.
  - Frontend stack, project license, and FFmpeg version + build source pinned in writing.
- Go/no-go gates (each needs a named owner assigned at Sprint 0 kickoff; recorded in `docs/gates.md` with pass/fail evidence):

  | Gate | Pass criteria | Fallback scope if it fails |
  | --- | --- | --- |
  | G1 FFmpeg build | CI build check (Section 7) passes for macOS arm64/x86_64 and Windows x64; `libx264` 1080p30 encode works on a no-GPU baseline machine; output plays in QuickTime, Windows Media Player, and a phone | Switch to another pinned static build source; make hardware encoders the default only if `libx264` is too slow on the baseline |
  | G2 Vendor workflow | Per vendor × preset: designer accepts page JPEGs at the product size with ≤2% visible crop, order is preservable, steps documented | Drop the failing vendor/preset from UI; promise narrows to passing vendors + generic press PDF |
  | G3 Offline maps | Route card renders from bundled tiles with networking off; basemap ≤60 MB | Pins and route on a plain styled background (no basemap, so no tile attribution needed) |
  | G4 Model budgets | Core tier meets Section 13 disk/RAM/throughput ceilings on both OSes | Swap to smaller model class or move to optional tier; heuristic-only scoring as last resort |
  | G5 Signed update | Signed build installs on a clean machine without Gatekeeper/SmartScreen block beyond first-run reputation, and updates one version via the test channel | Ship manual-download updates for the hackathon; keep updater work in Sprint 6 |

  - No keepsake renderer sprint (3–5) starts until G1 passes; Sprint 5 does not start until G2 has a recorded outcome.

### Sprint 1 — Ingest and Library

- Goal: Messy folder in, browsable library out.
- Deliverables:
  - Folder picker, batched walker, content + perceptual hashing, EXIF/ffprobe metadata, SQLite schema + migrations.
  - Live Photo pairing (UUID pass + basename fallback, Section 8), `.AAE` sidecars ignored as assets, HEIC decode via FFmpeg sidecar on Windows.
  - Thumbnail/proxy generation with content-addressed cache and size cap.
  - Virtualized Library grid with day/place grouping placeholders, progress + cancel, browsable-early behavior.
  - Consent sheet (first run) and face-data delete control (wired even if detection lands in Sprint 2).
- Exit criteria:
  - 10k-asset fixture folder becomes browsable in under 10 seconds on a mid-range laptop. No UI freeze. Cancel actually cancels.

### Sprint 2 — Curation Core and Story Review

- Goal: One AI Plan the user can review and lightly tweak.
- Deliverables:
  - Scoring (quality, faces, audio/motion where cheap), near-dupe best-shot collapse, day/place clustering.
  - Local draft captions + chapter titles (template fallback when the local VLM is unavailable or declined).
  - Story screen: chapters, hero/reel selects with "why this moment" reasons, caption editing, place renaming, music picker (bundled + user MP3).
  - Plan JSON persisted per project. Every AI output editable.
- Exit criteria:
  - On the fixture trip, a reviewer agrees the hero selects are "the right photos" at a glance. No uneditable AI text anywhere.

### Sprint 3 — Trip Video Renderer

- Goal: The hero film, end to end.
- Deliverables:
  - Normalize → pre-render → stepwise crossfade assembly → caption burn → audio duck/mix → loudness pass.
  - Modern 6-pack transition assignment by story position (Section 9) + Live Photo motion segments (motion-first, Ken Burns fallback). Stepwise offsets must account for cumulative overlap.
  - Title/location/chapter cards, MVP map segment (route card + push-in), 1080p export preset, EDL saved beside output.
  - Export card in UI with progress, cancel, time estimate, reveal-in-folder.
- Exit criteria:
  - A 3–5 minute film from the fixture trip plays start to finish with no black frames, no mushy captions, and music ducking under speech.

### Sprint 4 — Reels Pack and Maps Polish

- Goal: Post-ready verticals plus the journey map done properly for video.
- Deliverables:
  - Reel selection (varied days/places, Live Photo hooks preferred), smart static crop with blur-fill fallback, burned-in captions, per-reel music, numbered folder export + cover frames.
  - Map segment upgraded within MVP scope (clean route card, day markers, attribution, home fuzzing). Full fly-through only if Sprint 3 finished early.
  - Light tweaks: reorder/remove reels, edit captions, re-export one reel.
- Exit criteria:
  - 3–5 reels from the fixture trip pass a phone check: faces not cropped off, captions readable muted, first 2 seconds hook.

### Sprint 5 — Photo Book Export

- Goal: A book the user can actually order, without false PDF promises.
- Deliverables:
  - Layout engine with MVP templates (Section 11) driven by the shared plan.
  - Presets P1 and P2 with per-page sRGB JPEG export at exact trim pixels + screen proof PDF + vendor guide.
  - Generic press PDF (bleed, sRGB) for local shops.
  - Preflight checks (Section 11) blocking export with page-specific fix hints.
- Exit criteria:
  - Export pack for the fixture trip opens in every vendor designer that passed gate G2, at the right size without manual rescaling, following only the steps in the vendor guide.
  - Preflight, page counting, and resolution handling (Section 11) produce the expected result on the low-resolution and page-count fixtures.

### Sprint 6 — Hardening, Demo, and Hackathon Submission

- Goal: A stranger can run the demo without the team in the room.
- Deliverables:
  - Signed macOS + Windows builds, auto-update dry run on a test channel, cache/settings polish, error states with recovery (disk full, cancel mid-render, missing GPS/time).
  - Demo fixture + 3-minute demo script ("messy folder → magic") + recorded fallback video if live rendering is slow on stage.
  - README with run instructions, architecture summary, model/license ledger, privacy statement, and known limits.
  - Submission checklist (Section 18) completed.
- Exit criteria:
  - Clean-machine test: install, drop fixture folder, export all three keepsakes, no developer tools installed.

## 18. Deliverables Checklist

- Product deliverables:
  - TripTape desktop app (macOS universal + Windows x64) with all three keepsakes.
  - Demo fixture trip (small, consented/synthetic, redistributable).
  - Sample exports from the fixture: hero MP4, reels folder, book proof PDF + JPEG pack.
- Repo deliverables:
  - `PLAN.md` (this file), `README.md`, `LICENSE`, `THIRD_PARTY_MODELS.md`.
  - Build/run docs that work on a clean machine. CI passing on both OSes.
- Hackathon deliverables:
  - Project description naming the "Build for Your World" track and the family problem.
  - Demo video (live flow + fallback capture) under the track time limit.
  - Privacy + licensing statement (local-first faces, Meta OSS usage, third-party model licenses).

## 19. Testing and QA

- Fixtures:
  - Tiny synthetic trip (tens of assets, known GPS/time) for CI. Must be redistributable.
  - Realistic messy trip (hundreds of assets: dupes, blurry shots, vertical + horizontal video, missing GPS, mixed timezones) for manual QA.
  - Live Photo folder: HEIC+MOV pairs, JPG+MOV pairs, split pairs across subfolders, orphan still, orphan short MOV, same-stem long video that must NOT pair, `.AAE` sidecars.
  - Corrupt/edge folder: zero-byte file, unsupported format, no-GPS folder, single-photo folder. App must degrade, not crash.
  - Degradation fixtures (Section 5.4), each with an expected per-card outcome: single photo, 10 photos, short-video-only (clips <5 s), all-low-resolution (≤1 MP), no GPS, partial GPS, single GPS place, no EXIF time.
  - Phone media fixture: iPhone HEVC HDR (HLG/Dolby Vision) clip, rotated portrait video (rotation metadata), variable frame rate clip, 60 fps clip, Android HEIC + MP4, 48 kHz and 44.1 kHz audio mixed in one trip.
  - Source-integrity fixture: plan saved, then one original moved, one edited, one deleted, one replaced with a different file at the same path (Section 8).
  - Privacy fixture: Live Photos with speech audio, GPS near a configured home point.
- Automated checks:
  - Rust unit tests for hash, EXIF/GPS parsing, clustering, plan generation, preflight math (dpi, margins, page caps).
  - Golden-file tests for plan JSON and book pixel dimensions per preset.
  - Render smoke tests: hero + one reel + book pack from the tiny fixture in CI (short timeouts, tiny media).
  - Media correctness: ffprobe assertions on every rendered output (expected duration ±1 frame, constant 30 fps, correct rotation applied, SDR BT.709 tagged after HDR tone-map, AAC stereo 48 kHz). A/V sync check: a clap/flash test clip must stay within ±1 frame after crossfades and the loudness pass.
  - Privacy acceptance: EXIF-strip export contains no GPS, camera serial, or Live pairing UUID (checked with `exiftool`-equivalent parser); home fuzz moves route endpoints ≥500 m; no network calls in local mode (test harness fails on any outbound socket); cloud upload path unreachable unless the per-trip toggle is on; Reels card and completion view show the share disclosure (Section 14).
  - Job recovery: kill the app mid-render and relaunch (job becomes `interrupted`, Resume completes, output verifies); cancel mid-render leaves no file in the destination and no orphan FFmpeg process; disk-full injection fails cleanly.
  - Source integrity: each Section 8 case produces the expected pre-render sheet outcome, never a silent substitution.
  - IPC: malformed, cross-project, and path-traversal ids rejected (Section 7).
- Manual checks (every release candidate):
  - Phone check for reels (muted readability, face framing).
  - Vendor designer check for book JPEG packs (Mixbook + VistaPrint at minimum).
  - Clean-machine install and one-version auto-update of the signed build on macOS arm64, macOS x86_64, and Windows x64, with no dev tools.
  - Playback validation of the hero film and reels on macOS QuickTime, Windows Media Player/Photos, an iPhone, and an Android phone.
  - Sleep/wake during a long render on each OS (laptop lid close ≥5 min), ending in a resumed or recoverable state.
  - Accessibility pass on Import, Story, consent sheet, preflight, and export progress on both OSes: full keyboard operation with visible focus; VoiceOver (macOS) and Narrator/NVDA (Windows) announce controls, progress, and preflight issues; text and controls meet WCAG 2.1 AA contrast; `prefers-reduced-motion` disables UI animation (not exported video).
  - Offline check: full flow with networking disabled (cloud polish must fail gracefully, everything else must work).

## 20. Risks and Mitigations

- Risk: "PDF upload to Mixbook/MILK/VistaPrint" does not exist, so book expectations disappoint.
  - Mitigation: JPEG pack + guide UX (Section 11). Never use "upload PDF" copy.
- Risk: FFmpeg bundle size and signing pain (per-arch sidecars, two-step Windows signing) blocks shipping.
  - Mitigation: Pin one FFmpeg version in Sprint 0, fetch via script with CI cache, test the signed updater before features pile up (gate G5).
- Risk: GPL obligations for the bundled FFmpeg are missed, or a nonfree build sneaks in.
  - Mitigation: GPL text + source link in About/licenses and repo; CI build check rejects `--enable-nonfree` (Section 7).
- Risk: License traps (AGPL detectors, research-only face weights, gated Meta weights) contaminate the open-source release.
  - Mitigation: Section 13 allowlist + `THIRD_PARTY_MODELS.md` ledger reviewed every sprint.
- Risk: 10k-thumbnail grid collapses the webview or IPC.
  - Mitigation: Virtualized grid, `asset://` thumbnails, throttled progress events. Load-test in Sprint 1.
- Risk: Codec/GPS gaps (HEIC on Windows, .MOV GPS, rotation/HDR) produce wrong stories.
  - Mitigation: ffprobe as truth, rotation/timezone normalization, iPhone/Android/DSLR sample corpus in fixtures.
- Risk: Live Photo mis-pairing swallows a real short video or splits a pair.
  - Mitigation: UUID-first pairing, duration ceiling on the fallback, visible pair badge in the Library with a one-click unpair. Never delete or move source files based on pairing.
- Risk: Transition variety turns cheesy or breaks the render chain.
  - Mitigation: Fixed 6-pack with per-chapter discipline and usage caps (Section 9). New effects enter only as pre-rendered segments behind a phone check.
- Risk: Someone posts a reel they later wish they hadn't (kids, other people, location).
  - Mitigation: Share disclosure on Reels (Section 14), EXIF/GPS strip and home fuzz on export, TripTape never posts on the user's behalf.
- Risk: Small or incomplete trips produce broken or padded keepsakes.
  - Mitigation: Eligibility thresholds and degraded variants per keepsake (Section 5.4) with fixtures.
- Risk: Hallucinated captions get printed or posted.
  - Mitigation: Drafts only, always editable, preflight reminds the user to review captions before book export.
- Risk: Map tiles policy violation or missing attribution creates legal exposure.
  - Mitigation: Self-hosted extracts only, attribution on every frame/page, no public-tile scraping.
- Risk: Music licensing / Content ID claims on shared reels.
  - Mitigation: Ship only pre-cleared CC0-class tracks self-hosted in the app. User MP3s stay local. No hotlinked catalogs.
- Risk: Long renders die on sleep, disk-full, or cancel.
  - Mitigation: Job state machine with checkpoints, startup recovery, atomic finalization, and orphan-process cleanup (Section 7); crash/relaunch and sleep/wake tests (Section 19).
- Risk: Signing/notarization outage near the deadline.
  - Mitigation: Keys and test-channel updater dry run in Sprint 0/6, not at the end.

## 21. Open Decisions (TBD, Owner Needed)

- Decided: Frontend stack is Svelte 5 + Vite + Tailwind (`docs/decisions.md`).
- Decided: Project license is MIT (`docs/decisions.md`).
- TBD: Cloud polish host/provider, cost ceiling, and upload-consent plumbing. Needed only when cloud work starts; local MVP must not depend on it.
- TBD: Exact bundled music track list (20–30 CC0-class tracks, self-hosted, ledgered).
- TBD: Typst press-verdict: confirm sRGB proof quality and whether CMYK/PDF-X is achievable or stays out of scope.
- TBD: Bundled PMTiles build source and style (Section 12), and whether the opt-in regional detail pack ships at all.
- TBD: Whether face naming ships in 1.0. The hackathon cut shipped neither detection nor naming (Section 23).

## 22. Success Metrics

- Demo metric: messy fixture folder → all three keepsakes exported with at most 3 intentional clicks (add folder, review, Export all), plus the disclosed one-time consent and share-review confirms (Section 6). Measured on the fixture every release candidate.
- Quality metric: reviewer watching the hero film finishes it and can retell the trip (places in order, people present).
- Reels metric: at least 3 of 5 reels judged "would post as-is" (caption edits allowed) in hallway tests.
- Book metric: JPEG pack accepted at correct dimensions with zero manual rescaling by every vendor that passed gate G2, and at least one physical proof copy ordered and checked before submission.
- Privacy metric: full MVP flow verified offline; face data deletable; no network calls in local mode (verified by firewall/log check).
- Reliability metric: cancel, sleep/wake, and disk-full during render all end in a recoverable state, never a corrupt library.

## 23. Production Roadmap (Audited 2026-11-04)

The hackathon submission is done, including the demo video. The goal is now a release that ordinary families can install and trust. Work is sequenced by user impact, and each phase is tracked as a card on the Obsidian board.

### 23.1 What works today

- Ingest in the app and CLI: folder walk, hashing, EXIF and video metadata, Live Photo pairing, clustering, heuristic scoring, and a saved plan. First-run consent sheet.
- Three keepsakes: Trip Video, Reels Pack, and Photo Book, all from one plan, with offline place names and a route card.
- Story editing: rename a place, edit a caption, leave a photo out and put it back, swap a photo. Edits persist.
- Export screen with progress and reveal-in-folder, and a Reels share disclosure.

### 23.2 Where the code falls short of this plan

- Install: FFmpeg, ffprobe, and Typst are called by bare name from PATH. Nothing is bundled, so the app fails without developer tools. `sips` also makes HEIC decoding best on macOS only.
- Jobs (Section 7): `cancel_job` only changes the database row. FFmpeg keeps running and temp files stay. Outputs are written straight to the destination, the app never runs the free-space precheck (only the CLI does), and interrupted jobs get no Resume or Discard.
- Distribution: no signing, no release workflow, updater off. CI is not confirmed green on Windows.
- Privacy (Section 14): `exif_strip_args` has no caller, so EXIF stripping is not applied to exports. Home fuzz exists only as a CLI flag. There is no rolling log or diagnostic report, and no crash reporting.
- Settings: an About page only. No cache cap or clear, and no controls. The FFmpeg GPL text and source link are absent.
- Export UX: the book preset is fixed to P1, the destination is fixed, eligibility and preflight appear only after a run, and there is no integrity sheet or Relink (Section 8).
- Light tweaks: no reorder, music picker, caption toggle, map toggle, or single-reel re-export. One bundled music track.
- Intelligence: no ML models. Face detection returns nothing, so there is no face scoring or smart crop. Reels crop to center or blurred fill. Audio scoring and stabilization are not implemented.
- Library: thumbnails load lazily, but the grid is not virtualized. No "time guessed" badge or unpair control.
- Gates G1-G5 remain open. Renderers shipped ahead of G1 and G2 for the hackathon, which is now a release risk rather than a schedule rule.

### 23.3 Phases

| Phase | Outcome | Depends on |
| --- | --- | --- |
| R1 Reliable exports | Cancel stops the work, outputs appear only when verified, disk-full fails early, interrupted exports recover | Nothing |
| R2 Self-contained app | Bundled, pinned FFmpeg, ffprobe, and Typst; HEIC works on both OSes; Windows CI green (G1) | Nothing |
| R3 Signed releases | Release workflow, notarized and Authenticode-signed builds, working updater (G5) | R2; owner's signing certificates |
| R4 Privacy and Settings | EXIF strip applied by default, hide home, cache cap and clear, redacted logs and diagnostic report, licenses | Nothing |
| R5 Export choices | Destination picker, eligibility before a run, book preset and preflight in the UI, integrity sheet and Relink | R1 |
| R6 Light tweaks | Music picker and bundled library, reorder, caption and map toggles, single-reel re-export | Music list decision |
| R7 Local intelligence | Pinned models, face detection, smart crop, audio scoring, stabilization (G4) | Budgets measured on baseline hardware |
| R8 Book and map proof | Vendor designer checks and one ordered proof (G2); bundled basemap or the plain-background fallback (G3) | Owner's vendor test order |

Tooling runs alongside these: add fallow, jscpd, and deadwood, plus `cargo-deny` and `cargo-audit`, to CI.

A 1.0 release needs R1 through R5. R6 and R7 improve the product but do not block it, except that docs must not claim features that have not shipped.
