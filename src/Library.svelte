<script lang="ts">
  import { assets, project } from "./lib/store";

  import type { AssetView } from "./lib/tauri";

  // "best" passes everything until per-asset scores reach the UI.
  const FILTERS: Record<string, (a: AssetView) => boolean> = {
    all: () => true,
    best: () => true,
    photo: (a) => a.kind === "photo",
    video: (a) => a.kind === "video" || a.isLivePhoto,
    gps: (a) => a.hasGps
  };

  let filter = "all";
  let dayFilter = "";

  $: visible = $assets.filter(FILTERS[filter]);
</script>

<div class="p-6">
  <h1 class="text-2xl font-bold mb-1">Library</h1>
  <p class="text-sm text-gray-600 mb-4">{$project?.projectName} — {$assets.length} assets</p>

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
  {:else}
    <div class="grid grid-cols-2 sm:grid-cols-4 lg:grid-cols-6 gap-2">
      {#each visible as a}
        <div class="aspect-square rounded bg-gray-200 flex flex-col items-center justify-center text-xs text-gray-600" title={a.id}>
          <span class="font-mono">{a.kind}</span>
          {#if a.width}<span>{a.width}×{a.height}</span>{/if}
          {#if a.timeGuessed}<span class="text-amber-700">time guessed</span>{/if}
        </div>
      {/each}
    </div>
  {/if}
</div>
