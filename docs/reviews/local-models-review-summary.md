# Review summary: local models (feat/local-models)

`code-review-loop --branch origin/main`, 2026-11-20: Status CLEAN, cycles 0/5, 22m 36s. The reviewer's iteration-1 verdict was "good to go" (High 0, Medium 0, Low 0). The loop's own summary step failed (usage limit), so this file records the refinement pass's report verbatim.

I applied 8 fixes across 7 files. `cargo fmt`, clippy with `-D warnings`, `npm run lint` and `cargo test --workspace` all pass. Nothing is staged or committed. The diff was over 2,000 lines, so I ran the four-reviewer fan-out on Sonnet, read-only.

**Fixed:**
- **Duplicate model pass (bug):** reopening a project with only DINOv2 installed re-scored every still on every open. It treated missing face output as owed even though the face models weren't installed. The scoring filter (`LocalModels::wants`) and the restore check now share one installed-model check, `ModelPaths::installed()`, which reports embedding and faces separately. It replaces `LocalModels::installed`.
- **Model loading:** the background job now loads the models (about 290 MB per DINOv2 session) only when an asset still needs them. Before, it loaded them on every job.
- **One still pipeline:** `main.rs` and the CLI's `analyze_still` both built the still result, and only the CLI shrank and rotated the original before the models saw it. That logic now lives in one place, `LocalModels::analyze_still` in `local.rs`.
  - One behavior change: when the app has no thumbnail and falls back to the original file, the models now get the shrunk, rotated copy like the CLI does. Before, they got the full-resolution image unrotated.
- **Faster near-duplicate pass (`cluster.rs`):** the cheap time/place check now runs before the embedding comparison, which skips most pair comparisons. Results are unchanged, because a merge already required that check.
- **`cluster.rs` cleanup:** the video exclusion moved into `looks_alike`, and the redundant filtered pHash vector is gone.
- **Face signals as one struct:** `StorySignals` now holds a `FaceSignals` instead of three copied fields, which simplifies `with_faces` and `learn.rs`.
- **`db.rs`:** the face columns are written by one helper, `face_columns`, and read by its inverse. Reads now report column errors instead of silently turning them into defaults.
- **Small cleanup:** `LocalModels::load` uses a table of model files instead of string comparisons.

**Tests added (`local.rs`):** the installed-model check (including the partial-install case), file detection for each capability, and `analyze_still` without models.

**Skipped:**
- Moving the "no face bonus on soft frames" rule out of `signal_bonus`: it would change the order of the reasons shown to the user.
- One shared similarity check across cluster, pick and plan, and fixing `pick_top` ignoring embeddings: both change behavior.
- Storing model output in one JSON column: a schema redesign, too big for this pass.
- CLI decoding each original twice: avoiding it would mean either scoring on the thumbnail or holding full-resolution images in memory.
- Batching FER+ runs, and the per-image copies in `run_f32`: the first relies on a batch size I haven't verified, and the second saves little.
- Removing the per-model timing counters: the CLI uses them for its cost line.
- A shared constant for the `app.triptape.desktop` app ID: the existing `segment_cache` code hardcodes it the same way, so this belongs in a separate change.
- Making the CLI and app agree on worker count: they fall back to different defaults, 2 and 4.
- `fold_signals` pairing smiles with faces by position, and hand-written tensor-filling loops: low value.
