# Fixtures

All fixtures in this repo are **synthetic and redistributable**: generated
with FFmpeg `lavfi` test sources (`testsrc2`, `smptehdbars`, `sine`) via
[`scripts/make-fixture.sh`](../scripts/make-fixture.sh). No real people,
places, voices, or GPS coordinates — safe to ship, CI, and demo.

## Tiny trip (`tiny-trip/`, generated — not committed)

```sh
./scripts/make-fixture.sh fixtures/tiny-trip
```

- `day1/`, `day2/`: 6 PNG stills (mixed landscape/portrait) + 4 short MP4
  clips (3 s, h264 + AAC) with synthetic spread across two days.
- Used by CI (`spike-render.sh` smoke) and the 3-minute demo script
  ([`docs/demo-script.md`](../docs/demo-script.md)).

## Book spike (`book-spike.typ`)

Hand-written 4-page Typst proof used by `scripts/spike-book.sh`; asserts
3000×2400 px per-page JPEGs.

## Degradation fixtures (Section 5.4, to generate on demand)

Derive from `tiny-trip/` by subsetting:

| Case | Derivation | Expected per-card outcome |
| --- | --- | --- |
| single photo | keep 1 still | short slideshow; reels + book disabled |
| no GPS | strip location | map card/page omitted |
| all low-res | downscale to ≤1 MP | preflight fix hints |
| short-video-only | keep clips <5 s | reels may run under 15 s |
