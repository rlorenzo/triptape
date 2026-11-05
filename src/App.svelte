<script lang="ts">
  import { screen, project, assets, ingestJob, exportJob, storyPicks, IDLE_EXPORT } from "./lib/store";
  import { applyExportProgress, savedExport } from "./lib/exporter";
  import { friendlyStep } from "./lib/trip";
  import { getStory, onProgress, restoreLatestProject, loadAllAssets, isTauri } from "./lib/tauri";
  import Import from "./Import.svelte";
  import Library from "./Library.svelte";
  import Story from "./Story.svelte";
  import Export from "./Export.svelte";
  import Settings from "./Settings.svelte";
  import { onMount, onDestroy, tick } from "svelte";
  import { get } from "svelte/store";

  const tabs = [
    { id: "import", label: "Import" },
    { id: "library", label: "Library" },
    { id: "story", label: "Story" },
    { id: "export", label: "Export" },
    { id: "settings", label: "About" }
  ] as const;

  // Global ingest indicator: App is always mounted, so one subscription
  // covers Library/Story/Export too. A new scan (running) overwrites any
  // earlier state; failures stay visible until that next scan.
  // On mount (Tauri only) the last trip reopens at once: project + assets +
  // Library, with any unfinished hashing resumed by the backend as a fresh
  // ingest job the indicator above picks up automatically.
  onMount(() => {
    const unsub = onProgress((e) => {
      if (e.kind !== "ingest") {
        applyExportProgress(e);
        return;
      }
      ingestJob.set({
        id: e.id,
        progress: e.progress,
        step: e.currentStep ?? "",
        state: e.state,
        error: e.error
      });
      // Background ingest just persisted scores and rebuilt the plan: reload
      // the current project's assets once so Best and Story reflect them
      // without a rescan. Skipped with no project (nothing to reload), in
      // browser demo mode (no real events), and never clobbers a project the
      // user has since switched to.
      if (e.state === "done" && isTauri()) {
        const current = get(project);
        if (current) {
          const pid = current.projectId;
          loadPicks(pid);
          loadAllAssets(pid)
            .then((a) => {
              if (get(project)?.projectId === pid) assets.set(a);
            })
            .catch(() => {});
        }
      }
    });
    (async () => {
      if (!isTauri()) return;
      try {
        if (get(project) !== null) return;
        const summary = await restoreLatestProject();
        if (!summary) return; // empty store: start on Import as today
        // A scan raced the restore: the user's project wins, never clobbered.
        if (get(project) !== null) return;
        project.set(summary);
        assets.set(await loadAllAssets(summary.projectId));
        screen.set("library");
      } catch {
        // Restore is best-effort; a failure starts empty as today.
      }
    })();
    return unsub;
  });

  // The completion state clears itself after a few seconds, in Tauri mode
  // and in the browser-preview simulation alike.
  let clearTimer: ReturnType<typeof setTimeout> | null = null;
  $: if ($ingestJob?.state === "done" && clearTimer === null) {
    clearTimer = setTimeout(() => {
      ingestJob.set(null);
      clearTimer = null;
    }, 3000);
  }
  $: if ($ingestJob && $ingestJob.state !== "done" && clearTimer !== null) {
    clearTimeout(clearTimer);
    clearTimer = null;
  }
  onDestroy(() => {
    if (clearTimer !== null) clearTimeout(clearTimer);
  });

  $: ingestRunning = $ingestJob?.state === "running";
  $: ingestDone = $ingestJob?.state === "done";
  $: ingestFailed = $ingestJob?.state === "failed";
  $: ingestPercent = Math.round(($ingestJob?.progress ?? 0) * 100);

  // Per-trip status starts over when the trip changes.
  let ticketTrip: string | undefined;
  $: if ($project?.projectId !== ticketTrip) {
    ticketTrip = $project?.projectId;
    storyPicks.set(null);
    exportJob.set({ ...IDLE_EXPORT, summary: ticketTrip ? savedExport(ticketTrip) : null });
    if (ticketTrip) loadPicks(ticketTrip);
  }

  // The ticket reports real picks from the moment a trip opens.
  function loadPicks(pid: string) {
    getStory(pid)
      .then((s) => {
        if (get(project)?.projectId === pid) {
          storyPicks.set(s.chapters.reduce((sum, c) => sum + c.entries.length, 0));
        }
      })
      .catch(() => {
        // Not ready yet: the ticket says so, and the ingest "done" event retries.
      });
  }

  // Move focus to the new screen's heading so keyboard and screen-reader users
  // land there. Skips first paint, which keeps default focus on launch.
  let mainEl: HTMLElement;
  let shownScreen = $screen;
  $: if ($screen !== shownScreen) {
    shownScreen = $screen;
    tick().then(() => {
      mainEl?.scrollTo(0, 0);
      mainEl?.querySelector<HTMLElement>("h1")?.focus();
    });
  }

  function formatCount(n: number): string {
    return n.toLocaleString();
  }

  $: made = $exportJob.summary
    ? [
        $exportJob.summary.video && "Film",
        $exportJob.summary.reels.length > 0 && "reels",
        $exportJob.summary.bookPages > 0 && "book"
      ].filter(Boolean)
    : [];
  $: storyStatus =
    $storyPicks !== null && $storyPicks > 0
      ? `${formatCount($storyPicks)} picks`
      : ingestRunning
        ? "Still sorting"
        : $storyPicks === 0
          ? "No picks yet"
          : "Not ready";
  $: keepsakeStatus = $exportJob.label
    ? $exportJob.progress > 0
      ? `Making, ${Math.round($exportJob.progress * 100)}%`
      : "Making"
    : $exportJob.error
      ? "Stopped"
      : made.length > 0
        ? made.length === 3
          ? "All three"
          : made.join(", ")
        : "Not made yet";
</script>

<div class="shell">
  <aside class="side">
    <div class="brand">TripTape</div>
    <nav aria-label="Main">
      {#each tabs as t}
        <button
          class="nav-item"
          class:on={$screen === t.id}
          aria-current={$screen === t.id ? "page" : undefined}
          on:click={() => screen.set(t.id)}>{t.label}</button>
      {/each}
    </nav>

    <section class="ticket" aria-label="Trip status">
      {#if $project}
        <h2 title={$project.projectName}>{$project.projectName}</h2>
        <dl>
          <div><dt>Files found</dt><dd>{formatCount($assets.length || $project.filesSeen)}</dd></div>
          <div><dt>Story</dt><dd>{storyStatus}</dd></div>
          <div><dt>Keepsakes</dt><dd>{keepsakeStatus}</dd></div>
        </dl>
      {:else}
        <h2>No trip yet</h2>
        <p>Add a folder to start.</p>
      {/if}
      {#if ingestRunning || ingestDone || ingestFailed}
        <div class="ingest" role="status" aria-live="polite">
          {#if ingestRunning}
            <div
              class="progress"
              role="progressbar"
              aria-valuemin={0}
              aria-valuemax={100}
              aria-valuenow={ingestPercent}
              aria-label="Checking files"
            >
              <div style="width: {ingestPercent}%"></div>
            </div>
            <p>{ingestPercent}% · {friendlyStep($ingestJob?.step)}</p>
          {:else if ingestDone}
            <p>Library up to date</p>
          {:else}
            <p class="failed" title={$ingestJob?.error}>Couldn't finish checking your files. Rescan the folder from the Library to try again.</p>
          {/if}
        </div>
      {/if}
    </section>
  </aside>

  <main bind:this={mainEl}>
    {#if $screen === "import"}<Import />
    {:else if $screen === "library"}<Library />
    {:else if $screen === "story"}<Story />
    {:else if $screen === "export"}<Export />
    {:else}<Settings />
    {/if}
  </main>
</div>

<style>
  .shell {
    display: grid;
    grid-template-columns: 220px minmax(0, 1fr);
    height: 100%;
  }

  .side {
    display: flex;
    flex-direction: column;
    min-height: 0;
    overflow-y: auto;
    padding: 18px 12px 12px;
    background: var(--blue);
    color: #fff;
  }

  .brand {
    padding: 0 10px 18px;
    font-size: 22px;
    font-weight: 800;
    font-stretch: 125%;
    letter-spacing: -0.01em;
  }

  nav {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .nav-item {
    min-height: 38px;
    padding: 7px 10px;
    border-radius: var(--radius);
    color: #fff;
    font-weight: 500;
    text-align: left;
    cursor: pointer;
    transition: background-color 120ms;
  }

  .nav-item:hover:not(.on) {
    background: var(--blue-deep);
  }

  .nav-item.on {
    background: var(--yellow);
    color: var(--ink);
    font-weight: 700;
  }

  .nav-item:focus-visible {
    outline-color: var(--yellow);
  }

  .ticket {
    margin-top: auto;
    padding: 12px;
    border-top: 6px solid var(--orange);
    background: var(--paper);
    color: var(--ink);
    font-size: 13px;
  }

  .ticket h2 {
    overflow: hidden;
    font-size: 14px;
    font-weight: 700;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .ticket > p {
    color: var(--ink-2);
  }

  dl {
    margin-top: 6px;
  }

  dl > div {
    display: flex;
    flex-wrap: wrap;
    justify-content: space-between;
    gap: 8px;
    padding: 4px 0;
    border-top: 1px solid var(--line);
  }

  dt {
    color: var(--ink-2);
  }

  dd {
    margin-left: auto;
    color: var(--blue);
    font-weight: 800;
    text-align: right;
  }

  .ingest {
    margin-top: 8px;
  }

  .ingest p {
    display: -webkit-box;
    margin-top: 4px;
    overflow: hidden;
    color: var(--ink-2);
    -webkit-box-orient: vertical;
    -webkit-line-clamp: 2;
    line-clamp: 2;
  }

  .ingest .failed {
    color: var(--red);
  }

  main {
    min-width: 0;
    overflow-y: auto;
    background: var(--paper);
  }
</style>
