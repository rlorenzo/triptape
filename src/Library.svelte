<script lang="ts">
  import DayGroup from "./lib/DayGroup.svelte";
  import ErrorNote from "./lib/ErrorNote.svelte";
  import { assets, assetsByTime, dayNumbers, frameNumbers, ingestJob, project, screen } from "./lib/store";
  import { countOf, dayKey, formatDay, formatFull } from "./lib/trip";

  import { loadAllAssets, rescanProject } from "./lib/tauri";
  import type { AssetView } from "./lib/tauri";

  // "best" shows only highlight assets (stored scores from background
  // scoring); before scores arrive it is empty, after Done it reflects them.
  const FILTERS: Record<string, { label: string; test: (a: AssetView) => boolean }> = {
    all: { label: "All", test: () => true },
    best: { label: "Best", test: (a) => a.highlight === true },
    photo: { label: "Photos", test: (a) => a.kind === "photo" || a.isLivePhoto },
    video: { label: "Videos", test: (a) => a.kind === "video" || a.isLivePhoto },
    gps: { label: "Has place", test: (a) => a.hasGps }
  };

  let filter = "all";
  let dayFilter = "";

  $: query = dayFilter.trim().toLowerCase();
  // "Day 1" means that day exactly, not Days 10–19.
  $: askedDay = query.match(/^day\s+(\d+)$/);
  // Filtering keeps the oldest-first order: takenAt ascending, missing last.
  $: visible = $assetsByTime.filter(FILTERS[filter].test).filter((a) => {
    if (!query) return true;
    if (askedDay) return $dayNumbers.get(dayKey(a.takenAt)) === Number(askedDay[1]);
    const hay = [a.place ?? "", formatFull(a.takenAt), a.kind].join(" ").toLowerCase();
    return hay.includes(query);
  });
  $: pid = $project?.projectId;

  function commonPlaces(items: AssetView[]): string[] {
    const counts = new Map<string, number>();
    for (const a of items) if (a.place) counts.set(a.place, (counts.get(a.place) ?? 0) + 1);
    return [...counts.entries()]
      .sort((x, y) => y[1] - x[1])
      .slice(0, 2)
      .map(([place]) => place);
  }

  function groupByDay(items: AssetView[]) {
    const groups = new Map<string, AssetView[]>();
    for (const a of items) {
      const key = dayKey(a.takenAt);
      const group = groups.get(key);
      if (group) group.push(a);
      else groups.set(key, [a]);
    }
    return [...groups.entries()].map(([key, group]) => ({
      key,
      number: $dayNumbers.get(key),
      date: formatDay(group[0].takenAt),
      places: commonPlaces(group),
      items: group
    }));
  }

  $: days = groupByDay(visible);

  function jumpTo(key: string) {
    document.getElementById(`day-${key}`)?.scrollIntoView();
  }

  function clearFilters() {
    filter = "all";
    dayFilter = "";
  }

  // In-place refresh of the current trip's folder (no folder picker).
  // Disabled while a refresh is in flight or a background ingest job is
  // running; failures stay on Library as a plain error line.
  let rescanning = false;
  let rescanError = "";
  $: ingestRunning = $ingestJob?.state === "running";
  $: rescanDisabled = rescanning || ingestRunning || !$project;

  async function rescan() {
    if (!$project || rescanning) return;
    rescanning = true;
    rescanError = "";
    try {
      const summary = await rescanProject($project.projectId);
      project.set(summary);
      assets.set(await loadAllAssets(summary.projectId));
    } catch (e) {
      rescanError = String(e);
    } finally {
      rescanning = false;
    }
  }
</script>

<div class="screen">
  <div class="screen-head">
    <div>
      <h1 class="headline" tabindex="-1">The whole trip</h1>
      {#if $project}
        <p class="lede">{$project.projectName}: {countOf($assets.length, "file")} across {countOf($dayNumbers.size, "day")}.</p>
      {/if}
    </div>
    {#if $assets.length > 0}
      <div class="head-actions">
        <button class="btn" disabled={rescanDisabled} on:click={rescan}>{rescanning ? "Rescanning…" : "Rescan folder"}</button>
        <button class="btn btn-primary" on:click={() => screen.set("story")}>See the story</button>
      </div>
    {/if}
  </div>

  {#if $assets.length === 0}
    <div class="empty">
      <h2>Nothing here yet</h2>
      <p>Choose a trip folder and everything TripTape finds appears here.</p>
      <button class="btn btn-primary" on:click={() => screen.set("import")}>Choose trip folder</button>
    </div>
  {:else}
    <div class="tools">
      <div class="filters" role="group" aria-label="Show">
        {#each Object.entries(FILTERS) as [f, { label }]}
          <button class="filter" class:on={filter === f} on:click={() => (filter = f)} aria-pressed={filter === f}>{label}</button>
        {/each}
      </div>
      <input class="input search" placeholder="Find a day or place" bind:value={dayFilter} aria-label="Find a day or place" />
    </div>
    {#if rescanError}<ErrorNote message="Couldn't rescan the folder. Check that it's still there and the drive is connected, then try again." detail={rescanError} />{/if}

    {#if visible.length === 0}
      <div class="empty">
        {#if filter === "best" && !query && ingestRunning}
          <h2>Still finding the best ones</h2>
          <p>Best picks appear here once TripTape finishes checking your files.</p>
        {:else}
          <h2>Nothing matches</h2>
          <p>Try another day or place, or show everything again.</p>
          <button class="btn" on:click={clearFilters}>Show all files</button>
        {/if}
      </div>
    {:else}
      {#if days.length > 1}
        <nav class="day-index" aria-label="Jump to a day">
          {#each days as day (day.key)}
            <button on:click={() => jumpTo(day.key)}>
              <b>{day.number ? `Day ${day.number}` : "No date"}</b>
              <span>{day.places[0] ?? day.date}</span>
              <span>{countOf(day.items.length, "file")}</span>
            </button>
          {/each}
        </nav>
      {/if}
      {#each days as day (day.key)}
        <DayGroup {day} projectId={pid} frameNumbers={$frameNumbers} />
      {/each}
    {/if}
  {/if}
</div>

<style>
  .screen {
    padding-top: 32px;
  }

  .tools {
    display: flex;
    flex-wrap: wrap;
    gap: 10px;
    align-items: center;
    margin-bottom: 20px;
  }

  .filters {
    display: flex;
    gap: 2px;
  }

  .filter {
    min-height: 36px;
    padding: 6px 12px;
    border-radius: var(--radius);
    color: var(--ink-2);
    font-weight: 500;
    cursor: pointer;
  }

  .filter:hover:not(.on) {
    background: var(--wash);
    color: var(--ink);
  }

  .filter.on {
    background: var(--blue);
    color: #fff;
  }

  .search {
    width: 220px;
  }

  .head-actions {
    display: flex;
    flex: none;
    gap: 8px;
    align-items: center;
  }

  .day-index {
    display: flex;
    gap: 2px;
    margin-bottom: 20px;
    overflow-x: auto;
  }

  .day-index button {
    flex: none;
    display: flex;
    flex-direction: column;
    max-width: 180px;
    padding: 6px 12px;
    background: var(--wash);
    text-align: left;
    cursor: pointer;
  }

  .day-index button:hover {
    background: var(--yellow-soft);
  }

  .day-index button:focus-visible {
    outline-offset: -2px;
  }

  .day-index span {
    overflow: hidden;
    color: var(--ink-2);
    font-size: 13px;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
</style>
