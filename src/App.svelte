<script lang="ts">
  import { screen, project } from "./lib/store";
  import Import from "./Import.svelte";
  import Library from "./Library.svelte";
  import Story from "./Story.svelte";
  import Export from "./Export.svelte";
  import Settings from "./Settings.svelte";

  const tabs = ["import", "library", "story", "export", "settings"] as const;
</script>

<nav class="flex gap-1 p-2 border-b bg-white" aria-label="Main">
  {#each tabs as t}
    <button
      class="px-4 py-2 rounded capitalize {$screen === t ? 'bg-gray-900 text-white' : 'hover:bg-gray-100'}"
      aria-current={$screen === t ? "page" : undefined}
      on:click={() => screen.set(t)}>{t}</button>
  {/each}
  {#if $project}
    <span class="ml-auto text-sm text-gray-500 self-center">{$project.projectName}</span>
  {/if}
</nav>

<main>
  {#if $screen === "import"}<Import />
  {:else if $screen === "library"}<Library />
  {:else if $screen === "story"}<Story />
  {:else if $screen === "export"}<Export />
  {:else}<Settings />
  {/if}
</main>
