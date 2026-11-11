# Decisions (pinned in Sprint 0, not revisited mid-build)

## Frontend: Svelte 5 + Tailwind (Vite static build)

- Reason: lowest memory on 10k-thumbnail grids (PLAN Section 7); fine-grained
  reactivity fits the virtualized Library + progress-event UI without a VDOM
  tax. React 19 + Vite stays an acceptable fallback only if contributor
  experience demands it — the pin itself does not get revisited mid-build.

## License: MIT

- Reason: permissive license maximizes hackathon adoption and remixing, per
  PLAN Section 17 exit criteria. GPL obligations stay confined to the
  FFmpeg sidecar (separate executable + shipped source link), so the project
  license is unaffected. See `LICENSE` and `THIRD_PARTY_MODELS.md`.

## Typst book pipeline: sRGB-only until the CMYK verdict

- Reason: Typst 0.15.1 emits sRGB; vendor acceptance of sRGB JPEGs at exact
  pixel dims is exactly what gate G2 tests. No CMYK / PDF/X press variant
  until a vendor rejects sRGB output — that work is explicitly Later
  (PLAN Section 5.3). The Sprint 0 book spike proves the sRGB path end to end.

## Faces: detection-only by default, naming is opt-in

- Reason: PLAN Sections 4/14 — faces never leave the device by default;
  MVP core ships count + boxes only. Name labels require explicit opt-in
  consent (first-run sheet), and face data is deletable in Settings.
  No recognition embeddings from research-only weights ever ship
  (`THIRD_PARTY_MODELS.md` license traps).

## Story scoring: tagged-people count, relative quality, squeezed technical score

- Reason: on real trips ~80% of stills score 0.6 to 0.9 technically, so a
  purely technical score ranked by edge detail, and the dullest sharp frame
  beat the group photo. Scoring is now story-first.
- People: the count of people the user's photo library tagged, read from the
  asset's own XMP (embedded or `.xmp` sidecar) in this order:
  `Iptc4xmpExt:PersonInImage`, MWG face regions, then plain `dc:subject`
  keywords. The `dc:subject` route is a heuristic (Apple Photos writes each
  named person as a keyword, other libraries also store topics there): each
  distinct keyword of 1 to 4 words without digits or path/URL punctuation
  counts as a person. Only the count is stored (`assets.people_count`);
  names are personal data (PLAN Sec 14) and are never kept, logged, put in
  plan JSON or exported.
- Bonus: 1 person +0.10, 2 +0.18, 3+ +0.28; own caption +0.10; real video
  clip +0.08. Technical quality is squeezed into 0.30..0.75 so its whole
  spread is smaller than the smallest people bonus; the plan then blends in
  each asset's percentile within its chapter at weight 0.2. Highlight stays
  `total >= 0.65`.
- Video score follows the mean clip-window score of the window that plays,
  plus the same signals, replacing the fixed proxy.
- Hero film: Live Photos play as stills (so they can collage) except the best
  one per chapter; chapters of at most 3 assets merge into the closer
  same-day neighbour; time is dealt by `chapter_size^0.75`; a requested
  length is a minimum (the plan fills up to 3% over it, skipping picks that
  do not fit and trying cheaper ones). Near-identical shots of one moment
  (within 2 min and pHash distance 14, or within 5 s) are not both used in a
  chapter until the rest of the budget is spent.
- pHash is computed for every asset from the thumbnail/poster the scan
  already decodes (HEIC stills, Live Photos, videos), so near-dupe collapse
  works in both the CLI and the app.
