<script lang="ts">
  import Thumb from "./lib/Thumb.svelte";
  import { assets, ingestJob, project } from "./lib/store";

  import { loadAllAssets, rescanProject } from "./lib/tauri";
  import type { AssetView } from "./lib/tauri";

  // "best" shows only highlight assets (stored scores from background
  // scoring); before scores arrive it is empty, after Done it reflects them.
  const FILTERS: Record<string, (a: AssetView) => boolean> = {
    all: () => true,
    best: (a) => a.highlight === true,
    photo: (a) => a.kind === "photo" || a.isLivePhoto,
    video: (a) => a.kind === "video" || a.isLivePhoto,
    gps: (a) => a.hasGps
  };

  // Backend kind strings are raw (`live-photo`); display the friendly name.
  function kindLabel(a: AssetView): string {
    if (a.isLivePhoto || a.kind === "live-photo" || a.kind === "livephoto") return "Live Photo";
    return a.kind;
  }

  let filter = "all";
  let dayFilter = "";

  $: query = dayFilter.trim().toLowerCase();
  $: filtered = $assets.filter(FILTERS[filter]).filter((a) => {
    if (!query) return true;
    const hay = [a.place ?? "", fmtWhen(a.takenAt), a.kind].join(" ").toLowerCase();
    return hay.includes(query);
  });
  // Oldest-first display guarantee: takenAt ascending, missing last,
  // id tie-break. Sorts a copy so the store array is never mutated.
  $: visible = [...filtered].sort((a, b) => {
    if (a.takenAt == null && b.takenAt == null) return a.id < b.id ? -1 : a.id > b.id ? 1 : 0;
    if (a.takenAt == null) return 1;
    if (b.takenAt == null) return -1;
    return a.takenAt - b.takenAt || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0);
  });
  $: pid = $project?.projectId;

  function fmtWhen(takenAt?: number): string {
    if (!takenAt) return "";
    const d = new Date(takenAt * 1000);
    const date = d.toLocaleDateString(undefined, {
      month: "short",
      day: "numeric",
      year: "numeric"
    });
    const time = d.toLocaleTimeString(undefined, {
      hour: "2-digit",
      minute: "2-digit"
    });
    return `${date}, ${time}`;
  }

  function caption(a: AssetView): string {
    const when = fmtWhen(a.takenAt);
    const parts = [when, a.place].filter((s): s is string => !!s);
    return parts.join(" · ");
  }

  function meta(a: AssetView): string {
    const bits = [kindLabel(a)];
    if (a.width && a.height) bits.push(`${a.width}×${a.height}`);
    if (a.durationSecs) bits.push(`${a.durationSecs.toFixed(1)}s`);
    return bits.join(" · ");
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

<div class="p-6">
  <h1 class="text-2xl font-bold mb-1">Library</h1>
  <div class="flex items-center gap-3 mb-4">
    <p class="text-sm text-gray-600">{$project?.projectName} — {$assets.length} assets</p>
    <button
      class="px-3 py-1 rounded border text-sm bg-white disabled:opacity-50"
      disabled={rescanDisabled}
      on:click={rescan}>{rescanning ? "Rescanning…" : "Rescan folder"}</button>
  </div>
  {#if rescanError}<p class="text-red-600 text-sm mb-4" role="alert">{rescanError}</p>{/if}

  <div class="flex gap-2 mb-4 text-sm" role="toolbar" aria-label="Filters">
    {#each Object.keys(FILTERS) as f}
      <button
        class="px-3 py-1 rounded border {filter === f ? 'bg-gray-900 text-white' : 'bg-white'}"
        on:click={() => (filter = f)}
        aria-pressed={filter === f}>{f === "gps" ? "has place" : f}</button>
    {/each}
    <input class="border rounded px-2 py-1 ml-2" placeholder="day / place" bind:value={dayFilter} aria-label="Filter by day or place" />
  </div>

  {#if $assets.length === 0}
    <p class="text-gray-500">Nothing here yet — add a folder first.</p>
  {:else if visible.length === 0}
    <p class="text-gray-500">No assets match this filter.</p>
  {:else}
    <div class="grid grid-cols-2 sm:grid-cols-4 lg:grid-cols-6 gap-2">
      {#each visible as a (a.id)}
        {@const line = caption(a)}
        <figure class="rounded border bg-white overflow-hidden" title={a.id}>
          <div class="relative">
            <Thumb projectId={pid} assetId={a.id} kind={kindLabel(a)} />
            {#if a.isLivePhoto}
              <span class="absolute top-1 left-1 rounded bg-gray-700 px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-wide text-white">Live</span>
            {/if}
          </div>
          <figcaption class="px-1.5 py-1 text-xs leading-tight">
            {#if line}
              <div class="text-gray-700 truncate">{line}</div>
            {/if}
            <div class="text-gray-500 truncate">{meta(a)}{#if a.timeGuessed} · date approx.{/if}</div>
          </figcaption>
        </figure>
      {/each}
    </div>
  {/if}
</div>
