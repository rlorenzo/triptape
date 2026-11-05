<script lang="ts">
  import ErrorNote from "./lib/ErrorNote.svelte";
  import Thumb from "./lib/Thumb.svelte";
  import { assets, exportJob, frameNumbers, project, screen } from "./lib/store";
  import { friendlyStep } from "./lib/trip";
  import { runExport } from "./lib/exporter";
  import { cancelJob, openExport, revealExport, REELS_DISCLOSURE, type ExportCmd } from "./lib/tauri";

  const KEEPSAKES: { cmd: Exclude<ExportCmd, "export_all">; label: string; blurb: string; button: string; note?: string }[] = [
    { cmd: "export_video", label: "Trip Video", blurb: "A 1080p film of the trip, up to about 5 minutes.", button: "Make only the film" },
    {
      cmd: "export_reels",
      label: "Reels Pack",
      blurb: "3–5 vertical clips, ready to post.",
      button: "Make only the reels",
      note: REELS_DISCLOSURE
    },
    { cmd: "export_book", label: "Photo Book", blurb: "A print-ready PDF proof and a JPEG of every page.", button: "Make only the book" }
  ];

  const SKIPPED_LABELS: Record<string, string> = { video: "Trip Video", reels: "Reels Pack", book: "Photo Book" };

  let openError = "";

  $: job = $exportJob;
  $: busy = job.label !== null;
  $: percent = Math.round(job.progress * 100);
  $: hasPercent = job.progress > 0;
  $: summary = job.summary;
  $: madeSomething = !!summary && (!!summary.video || summary.reels.length > 0 || summary.bookPages > 0);
  // A single-keepsake export reports the other two as "not selected"; that isn't news.
  $: skipped = summary?.skipped.filter((s) => !s.reason.startsWith("not selected")) ?? [];

  // A photo from the trip to sit beside the finished film.
  $: poster = $assets.find((a) => a.highlight) ?? $assets[0];

  function retry() {
    const cmd = job.cmd;
    if (!cmd) return;
    start(cmd, cmd === "export_all" ? "All three keepsakes" : (KEEPSAKES.find((k) => k.cmd === cmd)?.label ?? "Keepsake"));
  }

  function start(cmd: ExportCmd, label: string) {
    if (!$project) return;
    openError = "";
    runExport(cmd, label, $project.projectId);
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
    <div class="row">
      <div>
        <h2>{k.label}</h2>
        <p>{k.blurb}</p>
        {#if k.note && !summary?.reels.length}<p class="note">{k.note}</p>{/if}
      </div>
      <button class="btn" disabled={busy} on:click={() => start(k.cmd, k.label)}>{k.button}</button>
    </div>
  {/each}
{/snippet}

<div class="screen">
  <div class="screen-head">
    <div>
      <h1 class="headline" tabindex="-1">{madeSomething ? "Your trip is back" : "Make your keepsakes"}</h1>
      {#if !madeSomething && $project}
        <p class="lede">One pass makes the film, the reels and the book from your story. It can take a while, and you can keep using the app.</p>
      {/if}
    </div>
    {#if $project && !madeSomething}
      <button class="btn btn-primary" disabled={busy} on:click={() => start("export_all", "All three keepsakes")}>Make all three</button>
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
        <div class="progress" role="progressbar" aria-valuemin={0} aria-valuemax={100} aria-valuenow={hasPercent ? percent : undefined} aria-label="Export progress">
          {#if hasPercent}<div style="width: {percent}%"></div>{:else}<div class="indeterminate"></div>{/if}
        </div>
        <div class="running-foot">
          <p role="status" aria-live="polite">{friendlyStep(job.step) || "Working on it. This can take a while."}</p>
          <button class="btn" disabled={!job.jobId} on:click={() => cancelJob(job.jobId)}>Cancel</button>
        </div>
      </section>
    {/if}

    {#if job.error}
      <ErrorNote message="The export stopped before it finished. Try again. If it stops again, check that your computer has free disk space." detail={job.error} />
      {#if job.cmd}<button class="btn retry" on:click={retry}>Try again</button>{/if}
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
              <button class="btn btn-primary" on:click={() => summary?.video && open(openExport, summary.video)}>Play film</button>
              <button class="btn" on:click={() => summary?.video && open(revealExport, summary.video)}>Show in folder</button>
            </div>
          </div>
        {/if}
        {#if summary.reels.length > 0}
          <div class="made">
            <div>
              <h2>Reels Pack</h2>
              <p>{summary.reels.length} {summary.reels.length === 1 ? "clip" : "clips"}. {REELS_DISCLOSURE}</p>
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
              <h2>{SKIPPED_LABELS[s.keepsake] ?? s.keepsake}: not made</h2>
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
      {#if madeSomething}
        <div class="again">
          <h2>Changed the story? Make them again.</h2>
          <button class="btn" disabled={busy} on:click={() => start("export_all", "All three keepsakes")}>Make all three again</button>
        </div>
      {/if}
      {#if madeSomething}
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

  .row .note {
    margin-top: 4px;
    color: var(--ink);
    font-size: 13px;
  }
</style>
