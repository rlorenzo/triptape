---
version: 1
slug: "src-app-svelte"
primary_target: "src/App.svelte"
related_targets: ["src/Import.svelte","src/Library.svelte","src/Story.svelte","src/Export.svelte","src/Settings.svelte"]
---

# TripTape app shell (whole app)

Scope: Import, Library, Story, Export, About and the shell. Mode: Operate. Audience: the parent in PRODUCT.md, at home on a laptop after the trip. Job: see the whole trip, relive it and fix a little, then get all three keepsakes.

## Direction contract

THESIS: Drop off the mess, come back to a sorted trip. Refuses the gray admin dashboard and the dark pro editor.

OWN-WORLD: One-hour photo counter color, not its objects. Processing-blue sidebar, signal orange primary action, yellow day bands and current state, white work area, cool-gray wash, ink text. Archivo: wide and heavy for headlines, normal width for work. Every photo carries a frame number in deep orange tabular numerals. Photos are never tinted. No envelope, paper, tape, film-strip or cardboard imagery. Never "order", "prints ready" or anything that implies a purchase.

STORY: The parent sees the pile already sorted, moves through it day by day, changes little, makes all three keepsakes, and plays the film.

FIRST VIEWPORT: Left blue sidebar with wordmark, five labeled nav items, and the trip ticket at the bottom showing real status: files found, story picks, keepsakes. Story shows a large headline and the orange "Looks good, go to Export" at top right, day tabs below, a yellow band naming the day and place, an index grid of frame-numbered photos, and a sticky panel on the right for the selected photo.

FORM: Back From the Lab, position 1 of the ordered list, chosen by the user over the assigned direction. Signature move: frame numbers plus the trip ticket, with photos that develop from overexposed to true on first load. seed:0eeed8cf9f1f2b55

FINISH: unreviewed and undocumented is unfinished; this build ends with the finish review, the verdict, DESIGN.md, and every shipping raster carrying its provenance

## Constraints

Thumbnails max out at 320px. Eligibility is only known after an export. Light theme only. Offline: fonts are bundled. Reels disclosure stays on the card and completion view, with no extra click. The UI shows only what works: faces are not detected today, so no copy claims it.
