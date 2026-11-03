# Agent Review Summary

## Overview

The review loop hardened the Sprint 1 ingest/library work in four areas: CLI exit semantics, privacy in exported plans, plan retrieval ordering, and crash safety for checkpoint files. The initial review raised 2 high and 4 medium issues. Cycle 1 fixed most of them, and cycle 2 closed the last one: partial scans were still exiting with status 0. The final review found nothing left to fix.

## Improvements Made

- **Partial scans no longer look successful.** Before the review, a scan that hit per-file errors still saved its plan and exited 0, so scripts and CI had no way to tell it was incomplete. Each scan error is now printed to stderr with its path (or `<unknown path>`). Unless the caller passes `--allow-partial true`, the command then exits with status 3. The plan is still saved, so no work is lost.
- **Bad flag values fail before any work starts.** `--allow-partial` must be exactly `true` or `false`. Any other value exits with status 2 before scanning begins, so a typo can't silently fall back to the default and produce a misleading result.
- **Exported plans can now hide the user's home location.** The privacy module already had `fuzz_home_route`, but the `plan` command never called it. A new `--home LAT,LON` flag runs the exported route polyline through the fuzzer, as PLAN.md §14 requires. The coordinates are parsed and range-checked (latitude ±90, longitude ±180), and invalid input is rejected with a clear error.
- **"Latest plan" really returns the newest plan.** The query used to sort by `plan_version` first, so an older plan with a higher version number could win over a newer insert. Plans are now sorted by `created_at`, with `rowid` breaking ties when two plans share the same second. A new regression test covers this exact case: a lower version inserted in the same second.
- **Checkpoint writes are crash-safe.** Checkpoint bytes used to be written straight to `name.partial`. A crash mid-write could leave a truncated file there, and `finalize_checkpoint` would later promote it. Writes now go to a uniquely named temp file (`name.<pid>.<seq>.tmp`), are fsynced, and are then atomically renamed onto `.partial`. If any step fails, the temp file is removed.
- Minor: `rustfmt` reformatted the `base_asset` calls.

## Created Files

None.

## Remaining Items

None. The final review reported no open findings.

## Files Changed

- `src-tauri/crates/triptape-core/src/bin/triptape.rs`
- `src-tauri/crates/triptape-core/src/db.rs`
- `src-tauri/crates/triptape-core/src/jobs.rs`

## Metrics

- Cycles run: 2
- Duration: 7m 35s
- Initial issues: High 2, Medium 4, Low 0
- After cycle 1: High 0, Medium 1, Low 0
- Final issues: High 0, Medium 0, Low 0 (CLEAN)
