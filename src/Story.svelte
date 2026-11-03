<script lang="ts">
  import { assets } from "./lib/store";

  // Story review: chapters derived from the plan in the full app; this stub
  // shows the first 12 assets as three 4-asset "days" with editable captions.
  let captions: Record<string, string> = {};
  let music = "Sunny Days (bundled)";

  $: chapters = [0, 1, 2].map((c) => ({
    title: `Day ${c + 1}`,
    items: $assets.slice(c * 4, c * 4 + 4)
  })).filter((c) => c.items.length > 0);
</script>

<div class="p-6 max-w-3xl mx-auto">
  <h1 class="text-2xl font-bold mb-1">Story</h1>
  <p class="text-sm text-gray-600 mb-4">Review the auto story. Every caption, title, and pick is editable.</p>

  {#each chapters as ch}
    <section class="mb-6 border rounded p-4 bg-white">
      <h2 class="font-semibold mb-1">{ch.title}</h2>
      <p class="text-xs text-gray-500 mb-3">Why these moments: highest quality, distinct times of day.</p>
      {#each ch.items as a}
        <div class="flex gap-2 items-center mb-2 text-sm">
          <span class="font-mono text-xs w-40 truncate">{a.id}</span>
          <input
            class="border rounded px-2 py-1 flex-1"
            placeholder="Caption draft…"
            bind:value={captions[a.id]}
            aria-label="Caption for {a.id}"
          />
        </div>
      {/each}
    </section>
  {/each}

  <div class="mb-4">
    <label class="text-sm font-medium" for="music">Music pick</label>
    <select id="music" class="border rounded px-2 py-1 ml-2" bind:value={music}>
      <option>Sunny Days (bundled)</option>
      <option>Beach Walk (bundled)</option>
      <option>Custom MP3…</option>
    </select>
  </div>
  <p class="text-xs text-gray-500">Captions are kept for this session only (demo state).</p>
</div>
