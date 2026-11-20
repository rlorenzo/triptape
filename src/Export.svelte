<script lang="ts">
  import ErrorNote from "./lib/ErrorNote.svelte";
  import Thumb from "./lib/Thumb.svelte";
  import {
    assets,
    BOOK_PAGES_MAX,
    BOOK_PAGES_MIN,
    bookPages,
    bookPreset,
    exportJob,
    frameNumbers,
    liveCollages,
    project,
    savedExportFolder,
    saveExportFolder,
    screen,
    validBookPages
  } from "./lib/store";
  import ProgressBar from "./lib/ProgressBar.svelte";
  import { countOf, friendlyStep } from "./lib/trip";
  import { runExport, wasAttempted } from "./lib/exporter";
  import {
    cancelJob,
    checkExportFolder,
    dismissInterruptedExport,
    DEMO_BOOK_PRESETS,
    getBookPresets,
    getExportEligibility,
    getInterruptedExport,
    pickFolder,
    playFilm,
    revealExport,
    REELS_DISCLOSURE,
    type BookPreset,
    type Eligibility,
    type ExportCmd
  } from "./lib/tauri";

  type Keepsake = {
    cmd: Exclude<ExportCmd, "export_all">;
    key: keyof Eligibility;
    label: string;
    /** Its name inside a sentence: "Make the film and the reels". */
    short: string;
    blurb: string;
    button: string;
    note?: string;
  };

  const KEEPSAKES: Keepsake[] = [
    {
      cmd: "export_video",
      key: "video",
      label: "Trip Video",
      short: "film",
      blurb: "A 1080p film of the trip, up to about 5 minutes.",
      button: "Make only the film"
    },
    {
      cmd: "export_reels",
      key: "reels",
      label: "Reels Pack",
      short: "reels",
      blurb: "3–5 vertical clips, ready to post.",
      button: "Make only the reels",
      note: REELS_DISCLOSURE
    },
    {
      cmd: "export_book",
      key: "book",
      label: "Photo Book",
      short: "book",
      blurb: "A print-ready PDF proof and a JPEG of every page.",
      button: "Make only the book"
    }
  ];

  // What the trip can make, from its plan. Unknown (still loading, or no
  // answer) leaves every card on: the run itself still skips with a reason.
  let eligibility: Eligibility | null = null;
  function loadEligibility(projectId: string | undefined) {
    eligibility = null;
    if (!projectId) return;
    getExportEligibility(projectId)
      .then((e) => {
        if ($project?.projectId === projectId) eligibility = e;
      })
      .catch(() => {
        // Only a preview of the run: without it, the cards stay on.
      });
  }
  $: loadEligibility($project?.projectId);

  const canMake = (k: Keepsake, e: Eligibility | null) => e?.[k.key].enabled ?? true;
  $: makeable = KEEPSAKES.filter((k) => canMake(k, eligibility));
  $: cantMake = KEEPSAKES.filter((k) => !canMake(k, eligibility));
  // "Make all three" names what it will really make when some can't be.
  $: someSkipped = cantMake.length > 0 && makeable.length > 0;
  $: makeableNames = makeable.map((k) => k.short).join(" and the ");
  $: allLabel = someSkipped ? `The ${makeableNames}` : "All three keepsakes";

  function labelFor(cmd: ExportCmd): string {
    if (cmd === "export_all") return allLabel;
    return KEEPSAKES.find((k) => k.cmd === cmd)?.label ?? "Keepsake";
  }

  let openError = "";

  // Book shapes and their page limits come from the backend; the preview's
  // copy of them stands in until they arrive.
  let presets: BookPreset[] = DEMO_BOOK_PRESETS;
  getBookPresets()
    .then((p) => (presets = p))
    .catch(() => {});
  $: preset = presets.find((p) => p.id === $bookPreset) ?? presets[0];
  $: pagesMin = preset?.minPages ?? BOOK_PAGES_MIN;
  $: pagesMax = preset?.maxPages ?? BOOK_PAGES_MAX;

  // The field holds what was typed; only a valid count reaches the store.
  let pagesInput: number | null = $bookPages;
  $: pagesInvalid = validBookPages(pagesInput, pagesMin, pagesMax) === null;
  function commitPages() {
    const n = validBookPages(pagesInput, pagesMin, pagesMax);
    if (n !== null) bookPages.set(n);
  }

  // A new shape starts from its own page default.
  function presetChanged() {
    const chosen = presets.find((p) => p.id === $bookPreset);
    if (!chosen) return;
    pagesInput = chosen.defaultPages;
    bookPages.set(chosen.defaultPages);
  }

  // Where the keepsakes go: the folder chosen for this trip, else the usual one.
  let chosenFolder: string | null = null;
  let shownFolder = "";
  let folderError = "";
  // The remembered folder can't be used now (a drive was unplugged, say).
  let folderUnavailable = false;
  function loadFolder(projectId: string | undefined) {
    chosenFolder = projectId ? savedExportFolder(projectId) : null;
    folderError = "";
    folderUnavailable = false;
    const asked = chosenFolder;
    checkExportFolder(asked)
      .then((path) => {
        if (chosenFolder === asked) shownFolder = path;
      })
      .catch(() => {
        if (chosenFolder !== asked) return;
        shownFolder = asked ?? "";
        folderUnavailable = asked !== null;
        folderError = "TripTape can't save to this folder right now. Connect the drive it's on, or choose another folder.";
      });
  }
  $: loadFolder($project?.projectId);

  async function chooseFolder() {
    folderError = "";
    try {
      const picked = await pickFolder("Choose where to save the keepsakes");
      if (!picked || !$project) return;
      const path = await checkExportFolder(picked);
      saveExportFolder($project.projectId, picked);
      chosenFolder = picked;
      shownFolder = path;
      folderUnavailable = false;
    } catch (e) {
      folderError = String(e);
    }
  }

  function useUsualFolder() {
    if (!$project) return;
    saveExportFolder($project.projectId, null);
    loadFolder($project.projectId);
  }

  // An export that stopped when the app closed, waiting on the user's call.
  let interrupted: ExportCmd | null = null;
  function checkInterrupted(projectId: string | undefined) {
    interrupted = null;
    if (!projectId) return;
    getInterruptedExport(projectId)
      .then((cmd) => {
        if ($project?.projectId === projectId) interrupted = cmd;
      })
      .catch(() => {
        // Only a convenience: the keepsakes can still be made from the cards.
      });
  }
  $: checkInterrupted($project?.projectId);

  function dismissInterrupted() {
    interrupted = null;
    if ($project) dismissInterruptedExport($project.projectId).catch(() => {});
  }

  $: job = $exportJob;
  $: busy = job.label !== null;
  // Nothing can start until the chosen folder is usable again.
  $: blocked = busy || folderUnavailable;
  $: percent = Math.round(job.progress * 100);
  $: hasPercent = job.progress > 0;
  $: summary = job.summary;
  $: madeSomething = !!summary && (!!summary.video || summary.reels.length > 0 || summary.bookPages > 0);
  $: skipped = summary?.skipped.filter(wasAttempted) ?? [];

  // A photo from the trip to sit beside the finished film.
  $: poster = $assets.find((a) => a.highlight) ?? $assets[0];

  function start(cmd: ExportCmd) {
    if (!$project || folderUnavailable) return;
    openError = "";
    interrupted = null;
    runExport(cmd, labelFor(cmd), $project.projectId);
  }

  function retry() {
    if (job.cmd) start(job.cmd);
  }

  async function open(action: (path: string) => Promise<void>, path: string) {
    openError = "";
    try {
      await action(path);
    } catch (e) {
      openError = e instanceof Error && e.message.includes("browser preview") ? e.message : "Couldn't open that. The file may have been moved or deleted. Make the keepsake again to get a new copy.";
    }
  }
</script>

{#snippet rows()}
  {#each KEEPSAKES as k}
    {@const can = canMake(k, eligibility)}
    {@const why = eligibility?.[k.key].reason ?? ""}
    <div class="row" class:off={!can}>
      <div>
        <h2>{k.label}</h2>
        <p>{k.blurb}</p>
        {#if why}<p class="why" id="why-{k.key}">{can ? `${why}.` : `Can't be made yet. ${why}.`}</p>{/if}
        {#if can && k.note && !summary?.reels.length}<p class="note">{k.note}</p>{/if}
      </div>
      <button class="btn" disabled={blocked || !can} aria-describedby={why ? `why-${k.key}` : undefined} on:click={() => start(k.cmd)}>
        {k.button}
      </button>
    </div>
  {/each}
{/snippet}

<div class="screen">
  <div class="screen-head">
    <div>
      <h1 class="headline" tabindex="-1">{madeSomething ? "Your trip is back" : "Make your keepsakes"}</h1>
      {#if !madeSomething && $project}
        {#if makeable.length === 0}
          <p class="lede">This trip doesn't have enough usable photos for a keepsake yet. Add more to the folder and rescan it from the Library.</p>
        {:else}
          <p class="lede">One pass makes the film, the reels and the book from your story. It can take a while, and you can keep using the app.</p>
          {#if cantMake.length > 0}
            <p class="lede skip-note">
              {cantMake.map((k) => `The ${k.label}`).join(" and the ")} can't be made yet, so {cantMake.length === 1 ? "it" : "they"} will be
              skipped.
            </p>
          {/if}
        {/if}
      {/if}
    </div>
    {#if $project && !madeSomething}
      <button class="btn btn-primary" disabled={blocked || makeable.length === 0} on:click={() => start("export_all")}>
        {someSkipped ? `Make the ${makeableNames}` : "Make all three"}
      </button>
    {/if}
  </div>

  {#if !$project}
    <div class="empty">
      <h2>Nothing to make yet</h2>
      <p>Choose a trip folder first. TripTape makes the keepsakes from its story.</p>
      <button class="btn btn-primary" on:click={() => screen.set("import")}>Choose trip folder</button>
    </div>
  {:else}
    {#if busy}
      <section class="running" aria-label="Export progress">
        <div class="running-head">
          <h2>Making {job.label?.toLowerCase()}</h2>
          {#if hasPercent}<span class="pct">{percent}%</span>{/if}
        </div>
        <ProgressBar value={hasPercent ? job.progress : null} label="Export progress" />
        <div class="running-foot">
          <p role="status" aria-live="polite">{friendlyStep(job.step) || "Working on it. This can take a while."}</p>
          <button class="btn" disabled={!job.jobId} on:click={() => cancelJob(job.jobId)}>Cancel</button>
        </div>
      </section>
    {/if}

    {#if interrupted && !busy}
      <div class="notice stop" role="status">
        <p>{labelFor(interrupted)} stopped when TripTape closed, so it wasn't finished. Your earlier keepsakes are untouched.</p>
        <button class="btn" disabled={blocked} on:click={() => interrupted && start(interrupted)}>Make it again</button>
        <button class="btn" on:click={dismissInterrupted}>Dismiss</button>
      </div>
    {/if}

    {#if job.error}
      <ErrorNote message="The export stopped before it finished. Try again. If it stops again, check that your computer has free disk space." detail={job.error} />
      {#if job.cmd}<button class="btn retry" disabled={blocked} on:click={retry}>Try again</button>{/if}
    {:else if job.cancelled}
      <p class="notice stop" role="status">Export cancelled. Nothing else was made.</p>
    {/if}

    {#if summary}
      <section class="done" aria-label="Finished keepsakes">
        {#if summary.video}
          <div class="made film">
            {#if poster}
              <div class="poster">
                <Thumb projectId={$project.projectId} assetId={poster.id} kind="Photo" alt="A photo from the trip" />
                <span class="frame-no">{$frameNumbers.get(poster.id) ?? ""}</span>
              </div>
            {/if}
            <div class="film-text">
              <h2>Trip Video</h2>
              <p>Your film is ready to watch together.</p>
            </div>
            <div class="made-actions">
              <button class="btn btn-primary" on:click={() => summary?.video && open(playFilm, summary.video)}>Play film</button>
              <button class="btn" on:click={() => summary?.video && open(revealExport, summary.video)}>Show in folder</button>
            </div>
          </div>
        {/if}
        {#if summary.reels.length > 0}
          <div class="made">
            <div>
              <h2>Reels Pack</h2>
              <p>{countOf(summary.reels.length, "clip")}. {REELS_DISCLOSURE}</p>
            </div>
            <div class="made-actions">
              <button class="btn" on:click={() => summary && open(revealExport, summary.reels[0])}>Show in folder</button>
            </div>
          </div>
        {/if}
        {#if summary.bookPages > 0}
          <div class="made">
            <div>
              <h2>Photo Book</h2>
              <p>{summary.bookPages} pages, with a PDF proof and a JPEG of every page. Upload them to your print service when you're ready.</p>
            </div>
            <div class="made-actions">
              <button class="btn" on:click={() => summary && open(revealExport, summary.outDir)}>Show in folder</button>
            </div>
          </div>
        {/if}
        {#each skipped as s}
          <div class="made not-made">
            <div>
              <h2>{labelFor(`export_${s.keepsake}` as ExportCmd)}: not made</h2>
              <p>{s.reason}</p>
            </div>
          </div>
        {/each}
        {#if !madeSomething && skipped.length === 0}
          <div class="made not-made"><div><h2>Nothing was made</h2><p>This trip doesn't have enough usable photos yet. Add more to the folder and rescan it from the Library.</p></div></div>
        {/if}
        <p class="where">Saved in {summary.outDir}</p>
        {#if openError}<ErrorNote message={openError} />{/if}
      </section>
    {/if}

    <section class="list" aria-label="Keepsakes">
      <div class="dest">
        <div class="dest-text">
          <span id="dest-label" class="dest-label">Save keepsakes in</span>
          <p id="dest-path" class="dest-path">{shownFolder}</p>
          <p class="dest-hint">Each trip gets its own folder inside it.</p>
        </div>
        <div class="dest-actions">
          <button class="btn" disabled={busy} aria-describedby="dest-label dest-path" on:click={chooseFolder}>Choose folder…</button>
          {#if chosenFolder}
            <button class="btn" disabled={busy} on:click={useUsualFolder}>Use the usual folder</button>
          {/if}
        </div>
      </div>
      {#if folderError}<ErrorNote message={folderError} />{/if}
      <div class="pages">
        <label for="book-preset">Photo Book shape</label>
        <select id="book-preset" disabled={busy} bind:value={$bookPreset} on:change={presetChanged}>
          {#each presets as p}
            <option value={p.id}>{p.name} in</option>
          {/each}
        </select>
        <label for="book-pages">Most pages in the Photo Book</label>
        <input
          id="book-pages"
          type="number"
          inputmode="numeric"
          min={pagesMin}
          max={pagesMax}
          step="1"
          disabled={busy}
          bind:value={pagesInput}
          on:change={commitPages}
          aria-describedby="book-pages-hint"
          aria-invalid={pagesInvalid}
        />
        <p id="book-pages-hint" class:bad={pagesInvalid}>
          {#if pagesInvalid}
            Choose a whole number from {pagesMin} to {pagesMax}. Using {$bookPages}.
          {:else}
            This shape starts at {preset?.defaultPages ?? $bookPages} pages. More pages fit more photos. Check your print service's limit: Shutterfly
            111, Snapfish 150, Mixbook 400.
          {/if}
        </p>
      </div>
      <div class="live">
        <label for="live-collages">
          <input id="live-collages" type="checkbox" disabled={busy} bind:checked={$liveCollages} aria-describedby="live-collages-hint" />
          Live Photos move in collages
        </label>
        <p id="live-collages-hint">Off shows them as stills.</p>
      </div>
      {#if madeSomething}
        <div class="again">
          <h2>Changed the story? Make them again.</h2>
          <button class="btn" disabled={blocked || makeable.length === 0} on:click={() => start("export_all")}>
            {cantMake.length === 0 ? "Make all three again" : "Make them again"}
          </button>
        </div>
        <details>
          <summary>Make only one</summary>
          {@render rows()}
        </details>
      {:else}
        {@render rows()}
      {/if}
    </section>
  {/if}
</div>

<style>
  h2 {
    font-size: 17px;
    font-weight: 700;
  }

  p {
    max-width: 65ch;
  }

  .running {
    margin-bottom: 24px;
    padding: 16px;
    background: var(--wash);
  }

  .running-head {
    display: flex;
    justify-content: space-between;
    margin-bottom: 8px;
  }

  .pct {
    color: var(--blue);
    font-size: 17px;
    font-weight: 700;
  }

  .running-foot {
    display: flex;
    gap: 16px;
    align-items: center;
    justify-content: space-between;
    margin-top: 10px;
    color: var(--ink-2);
  }

  .stop {
    display: flex;
    gap: 16px;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 24px;
  }

  .done {
    margin-bottom: 36px;
  }

  .made {
    display: flex;
    gap: 24px;
    align-items: center;
    justify-content: space-between;
    padding: 16px;
    background: var(--yellow-soft);
  }

  .made + .made {
    margin-top: 2px;
  }

  .film {
    flex-wrap: wrap;
  }

  .film .made-actions {
    margin-left: auto;
  }

  .film-text {
    flex: 1 1 220px;
    min-width: 0;
  }

  .film h2 {
    font-size: 24px;
    font-weight: 800;
    font-stretch: 125%;
    letter-spacing: -0.01em;
  }

  .poster {
    flex: none;
    width: 180px;
  }

  .retry {
    margin-bottom: 24px;
  }

  .made.not-made {
    background: var(--wash);
  }

  .made p,
  .row p {
    color: var(--ink-2);
  }

  .made-actions {
    display: flex;
    flex: none;
    gap: 8px;
    align-items: center;
  }

  .where {
    margin: 10px 0;
    max-width: none;
    color: var(--ink-2);
    font-size: 13px;
    overflow-wrap: anywhere;
  }

  .again {
    display: flex;
    gap: 16px;
    align-items: center;
    justify-content: space-between;
    padding-bottom: 12px;
  }

  .dest {
    display: flex;
    flex-wrap: wrap;
    gap: 8px 16px;
    align-items: center;
    justify-content: space-between;
    padding: 0 0 16px;
  }

  .dest-text {
    flex: 1 1 280px;
    min-width: 0;
  }

  .dest-label {
    font-weight: 500;
  }

  .dest-path {
    max-width: none;
    overflow-wrap: anywhere;
  }

  .dest-hint {
    color: var(--ink-2);
    font-size: 13px;
  }

  .dest-actions {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
  }

  .pages {
    display: grid;
    grid-template-columns: max-content max-content;
    justify-content: start;
    gap: 4px 16px;
    align-items: center;
    padding: 0 0 16px;
  }

  .pages label {
    font-weight: 500;
  }

  .pages input,
  .pages select {
    font: inherit;
    padding: 4px 8px;
    border: 1px solid var(--line);
  }

  .pages input {
    width: 6em;
  }

  .pages p {
    grid-column: 1 / -1;
    color: var(--ink-2);
    font-size: 13px;
  }

  .pages p.bad {
    color: var(--ink);
  }

  .live {
    padding: 0 0 16px;
  }

  .live label {
    display: flex;
    gap: 8px;
    align-items: center;
    font-weight: 500;
  }

  .live p {
    margin-left: 21px;
    color: var(--ink-2);
    font-size: 13px;
  }

  .row {
    display: flex;
    gap: 24px;
    align-items: center;
    justify-content: space-between;
    padding: 16px 0;
    border-top: 1px solid var(--line);
  }

  summary {
    padding: 4px 0 12px;
    color: var(--ink);
    font-weight: 500;
    cursor: pointer;
  }

  .row.off h2,
  .row.off > div > p:not(.why) {
    color: var(--ink-3);
  }

  .row .why {
    margin-top: 4px;
    color: var(--ink);
    font-size: 13px;
  }

  .skip-note {
    color: var(--ink);
  }

  .row .note {
    margin-top: 4px;
    color: var(--ink);
    font-size: 13px;
  }
</style>
