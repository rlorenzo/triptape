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
