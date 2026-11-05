# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

TripTape is a desktop app for macOS and Windows. Its interface is a web UI in a Tauri v2 shell, with a 1200×800 default window and a 900×600 minimum. There is no mobile app in v1.

## Users

**Primary: a parent or family organizer after a vacation.** They have 500–10,000 photos and clips from phones and cameras. They want something to watch together, something to post, and something to keep on a shelf. They will not learn a timeline editor and will abandon anything that needs 50 decisions. Their jobs:

- "Show me the trip without making me sort 2,000 files."
- "Give me 3 clips I can post today without editing."
- "Give me a book I can actually order without rebuilding it in another tool."
- "Do not upload my kids' faces anywhere without asking me."

**Secondary: a hackathon judge** with 3 minutes and no setup help. When the two audiences conflict, the parent wins. The demo run in `docs/demo-script.md` should still work, but it doesn't drive decisions.

## Product Purpose

TripTape turns a messy folder of family vacation photos and videos into three keepsakes from one curation pass:

- **Trip Video:** a 1080p hero film.
- **Reels Pack:** 3–5 vertical clips.
- **Photo Book:** a print-ready PDF plus per-page JPEGs.

Success means a parent drops in a folder and gets all three with almost no clicks, then finishes watching the film with their family.

## Positioning

"Messy folder to magic" leads: the effortless transformation is the headline. Local processing and privacy are load-bearing but sit underneath it, and support the claim instead of competing with it. The tagline is "Drop in the trip. Get the story back."

TripTape is free and open source (MIT), with no paywall, subscription, or per-export charge.

## Operating Context

The flow is Import → Library → Story → Export, with Settings alongside:

1. **Import:** add one local folder and acknowledge the one-time consent note.
2. **Library:** browse assets. `PLAN.md` specifies day and place groups, which aren't built yet.
3. **Story:** review the automatic cut. Rename places, edit captions, remove or restore picks, and swap a pick for a same-day alternative. Edits persist and reapply on rebuild.
4. **Export:** render all three keepsakes from one screen, with progress and cancel. The results are files on disk, and the user orders the book or posts the reels themselves.

Renders are long-running and scan sizes can be large, so progress and recovery are part of normal use.

## Capabilities and Constraints

- **Input:** local folders only. Apple Live Photos count as one asset. There is no cloud or phone import in v1.
- **Editing model:** the app makes the first cut, then the user makes light tweaks. There is no timeline or keyframe editor in v1.
- **Offline:** the app must be fully useful with no network connection.
- **Thin content:** each keepsake has an eligibility threshold. Below it, the Export card shows a smaller variant or a disabled state with a plain-language reason. Never pad with repeated shots, and never fail silently.
- **Out of scope for v1:** in-app print ordering, posting to social media, accounts, voiceover, generative music, and a theme store.
- **Terminology:** Trip Video (or "film"), Reels Pack, Photo Book, Story, Library. Music matching is "vibe fit", never detected emotion.
- **Reels disclosure:** the Reels card and completion view carry one non-blocking line about reviewing clips before sharing. It adds no checkbox, confirm, or extra click, and the film and book carry no disclosure.

**Show only what works.** The UI promises only what runs today, and planned features stay out of the copy until they ship. These incumbent strings and controls currently conflict with that rule and need resolving in UI work:

- The consent note's Cloud Polish sentence in `src/Import.svelte`.
- The Settings controls that have no backend: clear cache, Cloud Polish, strip EXIF, and delete face data.

Check each against the code at the time of the work, because backend work is in progress on other branches.

**Undecided:** when and whether Cloud Polish ships. `PLAN.md` lists it as post-hackathon backlog.

## Brand Commitments

- **Name:** TripTape.
- **Tagline:** "Drop in the trip. Get the story back."
- **Voice:** plain language for families, with honest limits and no fake precision. Every generated caption, title, and selection stays editable.

## Evidence on Hand

- `scripts/make-fixture.sh` generates a tiny synthetic trip, and `npm run dev` serves a browser preview with demo state and no Tauri backend.
- `docs/demo-script.md` is the 3-minute demo run.
- `README.md` lists the current MVP status and gaps, and `docs/gates.md` tracks open gates.
- `resources/music` holds the bundled music library, and `resources/gazetteer` the place-name data.

No user testimonials, customers, usage metrics, press, or benchmarks exist. Do not invent them.

## Product Principles

1. **Auto first, tweak second.** Every manual control has a good default, and the path with no decisions still gives a finished result.
2. **One curation, three keepsakes.** The user reviews one story, never three separate projects.
3. **Local first, cloud by consent.** Faces and originals stay on the device, and nothing uploads without an explicit per-trip choice.
4. **Editable AI.** Anything the app generates can be changed by the user.
5. **Honest about limits.** The UI shows only what works, gives plain reasons when something can't happen, and never fails silently.

## Accessibility & Inclusion

`PLAN.md` requires this on Import, Story, the consent sheet, preflight, and export progress, on both operating systems:

- Full keyboard operation with visible focus.
- VoiceOver (macOS) and Narrator/NVDA (Windows) announce controls, progress, and preflight issues.
- Text and controls meet WCAG 2.1 AA contrast.
- `prefers-reduced-motion` disables UI animation, but not exported video.
