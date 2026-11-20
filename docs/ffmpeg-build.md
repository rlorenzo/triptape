# FFmpeg, ffprobe and Typst sidecars (Gate G1)

PLAN.md Section 7. TripTape bundles FFmpeg and ffprobe (full GPL static
builds, `--enable-gpl --enable-version3`, with `libx264` and `libvidstab`)
and Typst as Tauri `externalBin` sidecars. No `--enable-nonfree` build may
ever ship: it cannot be redistributed. FFmpeg stays a separate executable,
so TripTape's MIT licence is unaffected.

`scripts/fetch-sidecars` (or `scripts/fetch-ffmpeg` for FFmpeg alone)
downloads, verifies and installs them; this file records what it pins.

## Pinned sidecars (pinned 2026-11-20)

FFmpeg **9.0.2** and Typst **0.15.1** on every target.

### macOS (arm64 and x86_64): evermeet.cx

| File | URL | SHA-256 |
| --- | --- | --- |
| `ffmpeg-9.0.2.zip` | <https://evermeet.cx/ffmpeg/ffmpeg-9.0.2.zip> | `4acc0be580f9b2788029eb7bd4d645ff87968911b0a62aeeb3940d42d54558d5` |
| `ffmpeg` (extracted) | | `a45b462cf91ed89148ae218c4577e30896485d7a6792c3673bcf5f823fa01b63` |
| `ffprobe-9.0.2.zip` | <https://evermeet.cx/ffmpeg/ffprobe-9.0.2.zip> | `24a9c968cd4da72d99c7245e914b921815835eb6dff01d99868031aebaf1d439` |
| `ffprobe` (extracted) | | `a29f8c2e03553223a8b480b1a1f04c42f680c138fa952c20086718a075db38cb` |

- Publisher verification, done before the binaries were first run: both
  zips verified against evermeet's detached signatures (`.zip.sig`) with
  GnuPG: `Good signature from "static FFmpeg binaries (signing key)
  <ffmpeg@evermeet.cx>"`, signing subkey
  `70202E8ED17B372A6D04467B4E7BEC998E920EE1` of primary key
  `20F6 EA3E 0CFD 6B4C 5344 7A73 476C 4B61 1A66 0874` (the fingerprint
  published on <https://evermeet.cx/ffmpeg/>, key fetched from
  keys.openpgp.org). Signatures dated 2026-09-18.
- evermeet.cx is the macOS build that ffmpeg.org's download page links.
  The URLs redirect to evermeet's download host (`e.deolaha.ca:4242`); the
  pinned SHA-256 is what makes that safe.
- **Intel only.** evermeet states it will not publish native Apple silicon
  builds. The binaries are `Mach-O 64-bit executable x86_64`, unsigned,
  minimum macOS 10.13, linked only against system libraries. The same
  files ship as `ffmpeg-aarch64-apple-darwin` and run under **Rosetta 2**
  on Apple silicon (see "Apple silicon decision" below).

### Windows x64: gyan.dev "essentials" release build

| File | URL | SHA-256 |
| --- | --- | --- |
| `ffmpeg-9.0.2-essentials_build.zip` | <https://github.com/GyanD/codexffmpeg/releases/download/9.0.2/ffmpeg-9.0.2-essentials_build.zip> | `60f467265b1e312373dbcd92200c2618a74850f98d3d078e94296bb3fa2047ba` |
| `bin/ffmpeg.exe` (extracted) | | `3256173f3f8bffd7df12227c68adf68025edb1832273a9530688a7bb1ed8edec` |
| `bin/ffprobe.exe` (extracted) | | `f0d36ecbbdd3bcfac3efa078c96c7271c2e68b3810595552ac3b7f17e9a65c52` |

- Publisher verification: the archive's SHA-256 matches both gyan.dev's
  published checksum
  (<https://www.gyan.dev/ffmpeg/builds/packages/ffmpeg-9.0.2-essentials_build.zip.sha256>)
  and GitHub's asset digest on the `9.0.2` release of `GyanD/codexffmpeg`,
  the mirror gyan.dev links for its release builds.
- Why gyan.dev rather than BtbN: gyan.dev publishes an exact `9.0.2`
  release build under a fixed tag, matching macOS. BtbN's builds are
  snapshots of release branches under dated `autobuild-*` tags, and its
  retention policy deletes most of them after a few weeks, so a pinned URL
  would stop working. (The old script also named BtbN's `gpl-shared`
  variant, which needs its DLLs beside the exe; a sidecar wants a static
  build.) PLAN.md Section 7 allows either.
- The build's `README.txt`: "FFmpeg 64-bit static Windows build from
  www.gyan.dev", "License: GPL v3", source commit
  <https://github.com/FFmpeg/FFmpeg/commit/946fcce07b>, `static yes`,
  `shared no`. External libraries include `libx264`, `libx265`,
  `libvidstab`, `libass`, `libfreetype`, `libharfbuzz`, `libfribidi`,
  `libzimg`, `libvmaf`, `libvpx`, `libaom`, `libopus`, `libmp3lame`.
  gyan.dev states its builds never use `--enable-nonfree`.
- Not yet run on Windows from here: `scripts/check-ffmpeg.sh` on this exe
  (including its `-buildconf`) runs in CI. **Pending CI.**

### Typst 0.15.1: official GitHub release (`typst/typst`)

| Target | Archive | Archive SHA-256 | Binary SHA-256 |
| --- | --- | --- | --- |
| aarch64-apple-darwin | `typst-aarch64-apple-darwin.tar.xz` | `48f62ed034aa3a7978309579ac6ca00045e2ef0da73114e8af27cfd8e74dc05a` | `7c4a136b377f3689400afe37b4f0fe3528d50faaa55cbb1106ac8a128f86ba1a` |
| x86_64-apple-darwin | `typst-x86_64-apple-darwin.tar.xz` | `7f9fdd9584866245de9a79e0add8f9236fae6f40a8a45e2c4771ccc14db4e0fa` | `a6fb5786e71f6f95d8323b326353770734c1e4624e7c108783ea578723916b46` |
| x86_64-pc-windows-msvc | `typst-x86_64-pc-windows-msvc.zip` | `19ce3551153c2fe7ee9fa2f95208310c8f4d3209fedb699e0333faf8913f6736` | `081217a463adb006f8894b44227fe4b9c9e91fc85f5463d5948e8370db9bb31e` |

- URLs: `https://github.com/typst/typst/releases/download/v0.15.1/<archive>`.
- Publisher verification: each archive's SHA-256 matches GitHub's asset
  digest on the `v0.15.1` release. Typst publishes no signatures or build
  attestations (`gh attestation verify` finds none).
- Licence: Apache-2.0, with third-party notices in `NOTICE`.

## Configure flags (`ffmpeg -buildconf`, macOS sidecar)

```text
--cc=/usr/bin/clang --prefix=/opt/ffmpeg --extra-version=tessus
--enable-avisynth --enable-fontconfig --enable-gpl --enable-libaom
--enable-libass --enable-libbluray --enable-libdav1d --enable-libfreetype
--enable-libgsm --enable-libharfbuzz --enable-libmodplug --enable-libmp3lame
--enable-libmysofa --enable-libopencore-amrnb --enable-libopencore-amrwb
--enable-libopenh264 --enable-libopenjpeg --enable-libopus
--enable-librubberband --enable-libshine --enable-libsnappy --enable-libsoxr
--enable-libspeex --enable-libtheora --enable-libtwolame --enable-libvidstab
--enable-libvmaf --enable-libvo-amrwbenc --enable-libvorbis --enable-libvpx
--enable-libwebp --enable-libx264 --enable-libx265 --enable-libxavs
--enable-libxml2 --enable-libxvid --enable-libzimg --enable-libzmq
--enable-libzvbi --enable-version3 --pkg-config-flags=--static
--disable-ffplay
```

No `--enable-nonfree`. `scripts/check-ffmpeg.sh` on the macOS sidecar
(2026-11-20, under Rosetta on an arm64 Mac): PASS on every required item
(`libx264`, native `aac`, `xfade`, `perspective`, `loudnorm`) and no gaps:
`zscale`, `drawtext`, `vidstabdetect`, `vidstabtransform`, `subtitles` and
`ass` are all present.

## Apple silicon decision

There is no native arm64 static FFmpeg that meets every rule (official
source, publisher checksum, full GPL with libx264 + libvidstab + libass +
zimg):

| Option | Native arm64 | vidstab | Verification | Notes |
| --- | --- | --- | --- | --- |
| **evermeet.cx x86_64 under Rosetta 2 (chosen)** | No | Yes | GPG signature | Linked from ffmpeg.org. Needs Rosetta 2, which Apple plans to limit after macOS 27. Slower than native. |
| Martin Riedl build server 9.0.2 arm64 | Yes | **No** | SHA-256 file, Developer ID signed (`Martin Riedl (KU3N25YGLU)`) | Has libass, zimg, x264. Fails the vidstab rule. |
| osxexperts.net 9.0.2 arm64 | Yes | Yes | SHA-256 on the page only, unsigned | Anonymous publisher, "for educational purposes only", its source link points at FFmpeg 6.1. Not trusted. |
| Build our own in CI | Yes | Yes | Our own | Most work; we then own GPL source distribution for every library. |

Rosetta 2 is installed with `softwareupdate --install-rosetta
--agree-to-license`. Without it the sidecar cannot start; the app then
reports FFmpeg as unusable rather than silently using something else.
**Owner decision needed** before Apple removes Rosetta: switch arm64 to a
native build (Martin Riedl if vidstab can wait for R7, or our own build).

## How the app finds the tools

`triptape_core::process::resolve_tool`, used by the app and the CLI:

1. An explicit path: the CLI's `--ffmpeg`/`--ffprobe`/`--typst`, then
   `TRIPTAPE_FFMPEG`/`TRIPTAPE_FFPROBE`/`TRIPTAPE_TYPST`.
2. The bundled sidecar next to the running executable (`Contents/MacOS/`
   in the macOS app, the install folder on Windows, `target/<profile>/` in
   a dev build, where tauri-build copies `src-tauri/binaries/`).
3. Debug builds only: `src-tauri/binaries/<name>-<triple>` in the source
   checkout, so `cargo test` uses the sidecar wherever cargo puts test
   binaries.
4. `PATH`, as a development fallback. The app's Export screen and the CLI
   say so, and they list what the FFmpeg in use cannot do
   (`Capabilities::degradations`: no libass captions, no zscale, no vidstab).

`src-tauri/build.rs` drops `externalBin` from a plain `cargo` build when the
sidecars are missing, so clones and CI's Rust steps still build; `tauri
build` reads the config itself and fails without them.

## HEIC on both OSes

macOS decodes HEIC through `sips` (ImageIO) first and FFmpeg second;
Windows has only FFmpeg. With the sidecar and `sips` disabled
(`heic_decodes_without_sips`, ignored test, run on 2026-11-20 against nine
iPhone photos including six Dolby Vision `tmap` gain-map files and an edited
one) every file decoded at full size, upright, matching the ImageIO decode.
See `docs/gates.md` G1 evidence.

## Re-pin procedure

1. Pick the new version and exact, immutable URLs from the sources above.
2. Download into a fresh empty directory. Verify the publisher's signature
   or checksum before running anything: evermeet
   (`gpg --verify ffmpeg-X.zip.sig ffmpeg-X.zip` against the fingerprint
   above), gyan.dev (`.sha256` file and GitHub digest), Typst (GitHub
   digest: `gh release view vX -R typst/typst --json assets`).
3. Record each archive's SHA-256 and each extracted binary's SHA-256 in
   `scripts/fetch-sidecars` and in this file.
4. Run `scripts/fetch-sidecars`: it re-verifies every hash and runs
   `scripts/check-ffmpeg.sh`. Required items must PASS; record any gap here
   and in `docs/gates.md`.
5. Record the new `-buildconf` here and update `resources/licenses/THIRD_PARTY.md`.
6. CI fetches the sidecars on macOS and Windows, runs `check-ffmpeg.sh`,
   the test suite and the fixture render against them.

## Licence obligations

- The bundle ships `licenses/GPL-3.0.txt`, `licenses/THIRD_PARTY.md` (what
  is bundled, from where, and source links) and Typst's `LICENSE` and
  `NOTICE` (`resources/licenses/`, `bundle.resources`). R4 surfaces them in
  Settings.
- FFmpeg source: <https://ffmpeg.org/releases/ffmpeg-9.0.2.tar.xz>; the
  library versions are in `-buildconf` and on the builders' pages. GPLv3
  section 6 makes the distributor responsible for the Corresponding Source
  staying available. Linking upstream is common practice but does not
  guarantee that, so before 1.0 the owner should decide whether to mirror
  the exact source archives (FFmpeg and its GPL libraries) alongside the
  releases.

## History

- 2026-11-03: evermeet 8.0.1 downloaded on the arm64 dev Mac but could not
  run (x86_64, Rosetta not installed then); pins stayed placeholders.
- 2026-11-20: pinned FFmpeg 9.0.2 (evermeet macOS, gyan.dev Windows) and
  Typst 0.15.1, Rosetta decision for Apple silicon.
- Plain Homebrew `ffmpeg` 9.0.2 (dev fallback only) lacks `zscale`,
  `drawtext`, `vidstab` and libass; `/opt/homebrew/opt/ffmpeg-full` has
  them.
